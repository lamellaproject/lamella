//! **Evaluating a program straight out of a precompiled artifact.** This is the whole evaluator.

use lamella_js_bytecode::{Artifact, Node, Tag};

use super::{new_scope, new_with_scope, Binding, Completion, Interpreter, Mutability, Scope};
use crate::abstract_ops as ops;
use crate::bytecode::decode;
use crate::value::JsValue;
use crate::{String, ToString, Vec};
use alloc::rc::Rc;

/// A destructuring target whose REFERENCE half has already been evaluated.
///
/// The nodes are `Copy` and hold offsets, so carrying one across the iterator step costs nothing
/// -- which is what lets the reference be resolved at the position the standard resolves it at
/// rather than at the position that would be convenient.
enum TargetNode<'a> {
    /// `o.x` with `o` and `"x"` already evaluated, plus the default that applies if the value that
    /// arrives is `undefined`.
    ///
    /// THE RECEIVER IS SEPARATE FROM THE BASE BECAUSE `super.x` IS A TARGET TOO. For an ordinary
    /// member the two are the same object; for `super.x` the base is the HOME OBJECT'S PROTOTYPE and
    /// the receiver is `this`, so the write lands on the instance. Carrying only the base wrote to
    /// the CLASS, and walking `super` as an ordinary expression -- which is what this did -- reported
    /// "`super` is only legal as `super(...)` or `super.x`" for a program that IS `super.x`.
    ///
    /// THE KEY IS **UNCOERCED**. This IS a Reference Record, and a Reference Record holds a value
    /// rather than a property key until `PutValue` runs -- which for a destructuring target is after
    /// the source has been read. Holding a [`crate::object::PropertyKey`] here spelled the coercion
    /// into the resolver and put `k.toString` a step early in every destructuring form.
    Member {
        base: JsValue,
        receiver: JsValue,
        key: TargetKey,
        default: Option<Node<'a>>,
    },
    /// Anything with no reference to resolve early: every binding leaf, and every nested pattern.
    Pattern(Node<'a>),
}

/// A member access's key, which may not have been through `ToPropertyKey` yet.
///
/// # A REFERENCE RECORD DOES NOT HOLD A PROPERTY KEY, IT HOLDS A VALUE
///
/// `[[ReferencedName]]` "may be an ECMAScript language value other than a String or a Symbol until
/// `ToPropertyKey` is performed", and the standard attaches a NOTE to the step saying where that
/// shows: **`a[b] = c` does not coerce `b` until after `c` has run.** Every other use coerces at
/// once, which is exactly why an engine coerces at once everywhere and is right nearly always.
///
/// Two variants rather than one `JsValue`, because a STATIC name never had a coercion to defer and
/// must not acquire one: `o.x = v` has no user code in its key, and a `Pending` holding the string
/// `"x"` would be indistinguishable from a computed key that produced it.
enum TargetKey {
    /// `o.x`: a name, already a key.
    Ready(crate::object::PropertyKey),
    /// `o[k]`: the key expression's VALUE, not yet coerced.
    Pending(JsValue),
}

/// What one link of a member/call chain produced.
///
/// **IT IS A TYPE RATHER THAN A SENTINEL VALUE OR A FLAG ON THE INTERPRETER, AND THAT IS THE
/// POINT.** "This chain short-circuited" has to travel exactly as far as the chain and no further;
/// a marker value could leak into a program as a wrong answer, and a mutable flag would have to be
/// cleared at every site that evaluates a subexpression -- which is a rule with fifty spellings.
/// Here the only way to observe a short-circuit is to be handed one by the link below you, and the
/// only way to consume it is to be a link yourself.
enum Chain {
    Value(JsValue),
    /// A `?.` met `null` or `undefined`. The whole chain is `undefined` and nothing further in it
    /// runs -- not a computed key, not an argument, not a call.
    ShortCircuit,
}

/// What a call's callee resolved to: the function, and the `this` its reference supplies.
enum Callee {
    Reference(JsValue, JsValue),
    /// A `?.` inside the callee short-circuited, so the call itself does not happen.
    ShortCircuit,
}

/// How much of a run the evaluator handled, and what it could not.
///
/// **DEVELOPMENT INSTRUMENTATION WITH A KNOWN EXPIRY, NOT PART OF THE ENGINE'S CONTRACT.** It
/// exists to answer "is every node kind implemented" with a count instead of an opinion, over real
/// programs rather than over a list someone wrote. `refused` is expected to be zero, and a test
/// asserts it is -- both over the pinned corpus and over one program per tag the format defines.
///
/// A tag that is never REACHED is invisible to this, which is why the per-tag table is checked
/// against programs chosen to contain each tag rather than against whatever a corpus happens to
/// hold. Absence of evidence reads exactly like evidence of absence in a census.
#[derive(Debug, Clone)]
pub struct Coverage {
    /// Nodes the evaluator handled.
    pub walked: u32,
    /// Nodes whose tag has no arm. Expected to be zero.
    pub refused: u32,
    /// Which tags were refused, indexed by tag byte, so a gap names itself.
    pub by_tag: [u32; 256],
}

impl Default for Coverage {
    fn default() -> Self {
        Self { walked: 0, refused: 0, by_tag: [0; 256] }
    }
}

impl Coverage {
    /// The tags that fell back, most frequent first -- the work list, in order.
    #[must_use]
    pub fn worst_tags(&self) -> Vec<(Tag, u32)> {
        let mut rows: Vec<(Tag, u32)> = (0u16..256)
            .filter_map(|byte| {
                let byte = byte as u8;
                let count = self.by_tag[byte as usize];
                if count == 0 {
                    return None;
                }
                Tag::from_u8(byte).map(|tag| (tag, count))
            })
            .collect();
        rows.sort_by_key(|(_, count)| core::cmp::Reverse(*count));
        rows
    }

    /// Walked as a share of everything, times a thousand.
    #[must_use]
    pub fn milli_coverage(&self) -> u32 {
        let total = self.walked + self.refused;
        if total == 0 {
            return 1000;
        }
        (u64::from(self.walked) * 1000 / u64::from(total)) as u32
    }
}

impl Interpreter {
    /// Runs a precompiled program.
    ///
    /// The artifact's own `min_engine` is NOT checked here -- that is
    /// [`lamella_js_bytecode::Artifact::check_engine`], and it is the CALLER's, because only an
    /// embedder knows whether it is willing to run a program compiled by a newer toolchain. Doing
    /// it here would make every test that builds an artifact in-process pay for a policy it has no
    /// stake in.
    /// Takes anything that can BE a program: an `Rc<[u8]>` the encoder just produced, or a
    /// `&'static [u8]` borrowed straight out of flash. The second is the XIP case and it copies
    /// nothing -- see [`super::ProgramBytes`].
    ///
    /// **THIS IS THE DEVICE'S ENTRY POINT.** A part with no parser links the reader, this
    /// method, and nothing else of the compiler.
    pub fn run_artifact(&mut self, program: impl Into<super::ProgramBytes>) -> Completion {
        self.run_artifact_measured(program, &mut Coverage::default())
    }

    /// [`Interpreter::run_artifact`], reporting which node kinds it met.
    ///
    /// Development instrumentation. It is separate so that [`Coverage`] -- which exists to prove
    /// the evaluator has an arm for every tag and has no meaning to an embedder -- is not a
    /// parameter of the method a device calls.
    #[doc(hidden)]
    pub fn run_artifact_measured(
        &mut self,
        program: impl Into<super::ProgramBytes>,
        coverage: &mut Coverage,
    ) -> Completion {
        let program = program.into();
        self.programs.push(program.clone());
        let index = (self.programs.len() - 1) as u32;
        let outer_program = core::mem::replace(&mut self.current_program, index);
        let artifact = match Artifact::open(&program) {
            Ok(artifact) => artifact,
            Err(error) => return self.host_error(&crate::format!("unreadable artifact: {error}")),
        };
        let artifact = &artifact;
        let Ok(root) = artifact.root() else {
            return self.host_error("the artifact's root node is unreadable");
        };
        if root.expect(Tag::Program, "Program").is_err() {
            return self.host_error("the artifact's root is not a Program");
        }
        let mut fields = root.fields();
        let (Ok(_span), Ok(strict)) = (fields.span(), fields.bool()) else {
            return self.host_error("the artifact's Program header is unreadable");
        };
        self.strict = strict;

        let Ok(body) = collect_body(&mut fields) else {
            return self.host_error("the artifact's Program body is unreadable");
        };
        let scope = Rc::clone(&self.global);
        let (lexical, vars, functions) = top_level_declared_names(artifact, &body);
        if let Err(abrupt) = self.global_declaration_refusals(&lexical, &vars, &functions) {
            self.current_program = outer_program;
            return abrupt;
        }
        self.hoist_vars_nodes(artifact, &body, &scope, true, &scope);
        self.hoist_nodes(artifact, &body, &scope);
        let completion = self.statement_nodes(artifact, &body, &scope, coverage);
        coverage.walked += self.nested_walked;
        coverage.refused += self.nested_refused;
        for (index, count) in self.nested_by_tag.iter().enumerate() {
            coverage.by_tag[index] += count;
        }
        self.current_program = outer_program;
        self.drain_jobs();
        completion
    }

    /// Evaluates a run of statements, threading completion values the way `statements` does.
    fn statement_nodes(
        &mut self,
        artifact: &Artifact<'_>,
        body: &[Node<'_>],
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        let mut last = JsValue::Undefined;
        for node in body {
            match self.statement_node(artifact, node, scope, coverage) {
                Completion::Normal(value) => {
                    if !completes_empty_node(node) {
                        last = value;
                    }
                }
                abrupt => return abrupt,
            }
        }
        Completion::Normal(last)
    }

    fn statement_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        match node.tag() {
            Tag::StmtEmpty | Tag::StmtDebugger | Tag::StmtFunction => {
                coverage.walked += 1;
                Completion::Normal(JsValue::Undefined)
            }
            Tag::StmtExpression => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
                    return self.host_error("unreadable expression statement");
                };
                self.expression_node(artifact, &child, scope, coverage)
            }
            Tag::StmtBlock => {
                coverage.walked += 1;
                let mut f = node.fields();
                if f.span().is_err() {
                    return self.host_error("unreadable block");
                }
                let Ok(body) = collect_body(&mut f) else {
                    return self.host_error("unreadable block body");
                };
                let inner = new_scope(Some(Rc::clone(scope)));
                self.hoist_nodes(artifact, &body, &inner);
                self.statement_nodes(artifact, &body, &inner, coverage)
            }
            Tag::StmtDeclaration => {
                coverage.walked += 1;
                self.declaration_node(artifact, node, scope, coverage)
            }
            Tag::StmtIf => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(test), Ok(consequent)) = (f.span(), f.child(), f.child()) else {
                    return self.host_error("unreadable if");
                };
                let condition = normal!(self.expression_node(artifact, &test, scope, coverage));
                if ops::to_boolean(&condition) {
                    return self.statement_node(artifact, &consequent, scope, coverage);
                }
                match f.option_child() {
                    Ok(Some(alternate)) => {
                        self.statement_node(artifact, &alternate, scope, coverage)
                    }
                    Ok(None) => Completion::Normal(JsValue::Undefined),
                    Err(_) => self.host_error("unreadable else branch"),
                }
            }
            Tag::StmtWith => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(object), Ok(body)) = (f.span(), f.child(), f.child()) else {
                    return self.host_error("unreadable with statement");
                };
                let value = normal!(self.expression_node(artifact, &object, scope, coverage));
                let bound = match self.to_object(&value) {
                    Ok(id) => id,
                    Err(abrupt) => return abrupt,
                };
                let inner = new_with_scope(bound, Rc::clone(scope));
                self.statement_node(artifact, &body, &inner, coverage)
            }
            Tag::StmtThrow => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
                    return self.host_error("unreadable throw");
                };
                let value = normal!(self.expression_node(artifact, &child, scope, coverage));
                Completion::Throw(value)
            }
            Tag::StmtTry => {
                coverage.walked += 1;
                let mut f = node.fields();
                if f.span().is_err() {
                    return self.host_error("unreadable try");
                }
                let Ok(block) = collect_body(&mut f) else {
                    return self.host_error("unreadable try block");
                };
                let inner = new_scope(Some(Rc::clone(scope)));
                self.hoist_nodes(artifact, &block, &inner);
                let mut completion = self.statement_nodes(artifact, &block, &inner, coverage);

                let handler = match f.option_child() {
                    Ok(handler) => handler,
                    Err(_) => return self.host_error("unreadable catch clause"),
                };
                if let (Completion::Throw(thrown), Some(handler)) = (&completion, &handler) {
                    let thrown = thrown.clone();
                    let mut h = handler.fields();
                    if h.span().is_err() {
                        return self.host_error("unreadable catch clause");
                    }
                    let catch_scope = new_scope(Some(Rc::clone(scope)));
                    match h.option_child() {
                        Ok(Some(param)) => {
                            if let Err(abrupt) = self
                                .declare_pattern_node(artifact, &param, thrown, &catch_scope, coverage)
                            {
                                return abrupt;
                            }
                        }
                        Ok(None) => {}
                        Err(_) => return self.host_error("unreadable catch parameter"),
                    }
                    let Ok(body) = collect_body(&mut h) else {
                        return self.host_error("unreadable catch body");
                    };
                    let block_scope = new_scope(Some(Rc::clone(&catch_scope)));
                    self.hoist_nodes(artifact, &body, &block_scope);
                    completion = self.statement_nodes(artifact, &body, &block_scope, coverage);
                }

                match f.bool() {
                    Ok(true) => {
                        let Ok(finalizer) = collect_body(&mut f) else {
                            return self.host_error("unreadable finally block");
                        };
                        let finally_scope = new_scope(Some(Rc::clone(scope)));
                        self.hoist_nodes(artifact, &finalizer, &finally_scope);
                        let finally_completion =
                            self.statement_nodes(artifact, &finalizer, &finally_scope, coverage);
                        if finally_completion.is_abrupt() {
                            return finally_completion;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => return self.host_error("unreadable finally flag"),
                }
                completion
            }
            Tag::StmtWhile | Tag::StmtDoWhile | Tag::StmtFor | Tag::StmtForIn | Tag::StmtForOf => {
                self.loop_statement_node(artifact, node, None, scope, coverage)
            }
            Tag::StmtLabeled => {
                coverage.walked += 1;
                let mut f = node.fields();
                if f.span().is_err() {
                    return self.host_error("unreadable labelled statement");
                }
                let (Ok(id), Ok(body)) = (f.str_id(), f.child()) else {
                    return self.host_error("unreadable label");
                };
                let Ok(label) = artifact.str_utf8(id) else {
                    return self.host_error("unreadable label");
                };
                let label = String::from(label);
                let completion = if is_loop_tag(body.tag()) {
                    self.loop_statement_node(artifact, &body, Some(label.clone()), scope, coverage)
                } else {
                    self.statement_node(artifact, &body, scope, coverage)
                };
                match completion {
                    Completion::Break(Some(target)) if target == label => {
                        Completion::Normal(JsValue::Undefined)
                    }
                    other => other,
                }
            }
            Tag::StmtReturn => {
                coverage.walked += 1;
                let mut f = node.fields();
                if f.span().is_err() {
                    return self.host_error("unreadable return");
                }
                let value = match f.option_child() {
                    Ok(Some(argument)) => {
                        normal!(self.expression_node(artifact, &argument, scope, coverage))
                    }
                    Ok(None) => JsValue::Undefined,
                    Err(_) => return self.host_error("unreadable return argument"),
                };
                Completion::Return(value)
            }
            Tag::StmtBreak | Tag::StmtContinue => {
                coverage.walked += 1;
                let mut f = node.fields();
                if f.span().is_err() {
                    return self.host_error("unreadable break or continue");
                }
                let label = match read_optional_str(artifact, &mut f) {
                    Ok(label) => label,
                    Err(_) => return self.host_error("unreadable label"),
                };
                if node.tag() == Tag::StmtBreak {
                    Completion::Break(label)
                } else {
                    Completion::Continue(label)
                }
            }
            Tag::StmtClass => {
                coverage.walked += 1;
                let mut f = node.fields();
                let Ok(class) = f.child() else {
                    return self.host_error("unreadable class declaration");
                };
                let value = normal!(self.class_value_node(artifact, &class, scope, coverage));
                let mut c = class.fields();
                let (Ok(_span), Ok(name)) = (c.span(), read_optional_str(artifact, &mut c)) else {
                    return self.host_error("unreadable class name");
                };
                if let Some(name) = name {
                    self.initialize_existing(scope, &name, value);
                }
                Completion::Normal(JsValue::Undefined)
            }
            Tag::StmtSwitch => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(discriminant)) = (f.span(), f.child()) else {
                    return self.host_error("unreadable switch");
                };
                let value = normal!(self.expression_node(artifact, &discriminant, scope, coverage));
                let Ok(cases) = collect_body(&mut f) else {
                    return self.host_error("unreadable switch cases");
                };
                let inner = new_scope(Some(Rc::clone(scope)));
                let mut bodies = Vec::with_capacity(cases.len());
                for case in &cases {
                    let mut c = case.fields();
                    let (Ok(_span), Ok(test)) = (c.span(), c.option_child()) else {
                        return self.host_error("unreadable switch case");
                    };
                    let Ok(body) = collect_body(&mut c) else {
                        return self.host_error("unreadable switch case body");
                    };
                    self.hoist_nodes(artifact, &body, &inner);
                    bodies.push((test, body));
                }
                let mut matched = None;
                for (index, (test, _)) in bodies.iter().enumerate() {
                    if let Some(test) = test {
                        let candidate =
                            normal!(self.expression_node(artifact, test, &inner, coverage));
                        if ops::strict_equals(&value, &candidate) {
                            matched = Some(index);
                            break;
                        }
                    }
                }
                let start = match matched {
                    Some(index) => index,
                    None => match bodies.iter().position(|(test, _)| test.is_none()) {
                        Some(index) => index,
                        None => return Completion::Normal(JsValue::Undefined),
                    },
                };
                for (_, body) in &bodies[start..] {
                    match self.statement_nodes(artifact, body, &inner, coverage) {
                        Completion::Break(None) => return Completion::Normal(JsValue::Undefined),
                        Completion::Normal(_) => {}
                        abrupt => return abrupt,
                    }
                }
                Completion::Normal(JsValue::Undefined)
            }
            _ => self.refuse_statement(node, coverage),
        }
    }

    /// Every loop in the language, carrying the label it was written with -- or `None`.
    ///
    /// **THE LABEL IS PASSED INTO THE LOOP RATHER THAN WRAPPED AROUND IT**, and that is the
    /// whole reason this function exists. `break outer` can be absorbed from outside, because it
    /// leaves the loop either way -- but `continue outer` must RESUME that loop, which only the
    /// loop itself can do. An implementation that only wraps gets `break` right and `continue`
    /// wrong, and the half that works is the half people write.
    ///
    /// For `for-of` a lost label is worse than a wrong jump: leaving by an unrecognised label
    /// skips `IteratorClose`, so the iterator is never told its consumer stopped.
    fn loop_statement_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        label: Option<String>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        match node.tag() {
            Tag::StmtWhile => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(test), Ok(body)) = (f.span(), f.child(), f.child()) else {
                    return self.host_error("unreadable while");
                };
                self.loop_body(label, scope, &[], |me, scope| {
                    let condition = normal!(me.expression_node(artifact, &test, scope, coverage));
                    if !ops::to_boolean(&condition) {
                        return Completion::Break(None);
                    }
                    me.statement_node(artifact, &body, scope, coverage)
                })
            }
            Tag::StmtDoWhile => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(body), Ok(test)) = (f.span(), f.child(), f.child()) else {
                    return self.host_error("unreadable do-while");
                };
                let mut first = true;
                self.loop_body(label, scope, &[], |me, scope| {
                    if !first {
                        let condition =
                            normal!(me.expression_node(artifact, &test, scope, coverage));
                        if !ops::to_boolean(&condition) {
                            return Completion::Break(None);
                        }
                    }
                    first = false;
                    me.statement_node(artifact, &body, scope, coverage)
                })
            }
            Tag::StmtFor => {
                coverage.walked += 1;
                let mut f = node.fields();
                if f.span().is_err() {
                    return self.host_error("unreadable for");
                }
                let outer = new_scope(Some(Rc::clone(scope)));
                let mut per_iteration: Vec<String> = Vec::new();
                match f.option_child() {
                    Ok(Some(init)) => match init.tag() {
                        Tag::ForInitDeclaration => {
                            self.hoist_declaration_node(artifact, &init, &outer);
                            per_iteration = per_iteration_names(artifact, &init);
                            let completion =
                                self.declaration_node(artifact, &init, &outer, coverage);
                            if completion.is_abrupt() {
                                return completion;
                            }
                        }
                        Tag::ForInitExpression => {
                            let Ok(inner) = init.fields().child() else {
                                return self.host_error("unreadable for-init expression");
                            };
                            let completion =
                                self.expression_node(artifact, &inner, &outer, coverage);
                            if completion.is_abrupt() {
                                return completion;
                            }
                        }
                        _ => {}
                    },
                    Ok(None) => {}
                    Err(_) => return self.host_error("unreadable for-init"),
                }
                let (test, update) = match (f.option_child(), f.option_child()) {
                    (Ok(test), Ok(update)) => (test, update),
                    _ => return self.host_error("unreadable for header"),
                };
                let Ok(body) = f.child() else {
                    return self.host_error("unreadable for body");
                };
                let mut first = true;
                self.loop_body(label, &outer, &per_iteration, |me, scope| {
                    if !first {
                        if let Some(update) = &update {
                            let completion =
                                me.expression_node(artifact, update, scope, coverage);
                            if completion.is_abrupt() {
                                return completion;
                            }
                        }
                    }
                    first = false;
                    if let Some(test) = &test {
                        let condition = normal!(me.expression_node(artifact, test, scope, coverage));
                        if !ops::to_boolean(&condition) {
                            return Completion::Break(None);
                        }
                    }
                    me.statement_node(artifact, &body, scope, coverage)
                })
            }
            Tag::StmtForIn => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(left), Ok(right), Ok(body)) =
                    (f.span(), f.child(), f.child(), f.child())
                else {
                    return self.host_error("unreadable for-in");
                };
                let head = self.iteration_head_dead_zone(artifact, &left, scope);
                let subject = normal!(self.expression_node(artifact, &right, &head, coverage));
                if matches!(subject, JsValue::Undefined | JsValue::Null) {
                    return Completion::Normal(JsValue::Undefined);
                }
                let id = match self.to_object(&subject) {
                    Ok(id) => id,
                    Err(abrupt) => return abrupt,
                };
                let mut keys: Vec<(crate::value::ObjectId, crate::object::PropertyKey)> =
                    Vec::new();
                let mut seen: Vec<crate::object::PropertyKey> = Vec::new();
                let mut current = Some(id);
                while let Some(object_id) = current {
                    let step = match self.own_string_keys_of(object_id) {
                        Ok(keys) => keys,
                        Err(abrupt) => return abrupt,
                    };
                    for key in step {
                        if seen.contains(&key) {
                            continue;
                        }
                        let enumerable = match self.own_property(object_id, &key) {
                            Ok(property) => property.is_some_and(|property| property.enumerable),
                            Err(abrupt) => return abrupt,
                        };
                        seen.push(key.clone());
                        if enumerable {
                            keys.push((object_id, key));
                        }
                    }
                    current = match self.get_prototype_of(object_id) {
                        Ok(next) => next,
                        Err(abrupt) => return abrupt,
                    };
                }
                let mut remaining = keys.into_iter();
                self.loop_body(label, scope, &[], |me, inner| {
                    let key = loop {
                        let Some((holder, key)) = remaining.next() else {
                            return Completion::Break(None);
                        };
                        let still_there = match me.own_property(holder, &key) {
                            Ok(property) => property.is_some(),
                            Err(abrupt) => return abrupt,
                        };
                        if still_there {
                            break key;
                        }
                    };
                    let value = JsValue::String(crate::string_value::JsString::from(
                        key.to_display().as_str(),
                    ));
                    if let Some(abrupt) =
                        me.bind_iteration_head_node(artifact, &left, value, inner, coverage)
                    {
                        return abrupt;
                    }
                    me.statement_node(artifact, &body, inner, coverage)
                })
            }
            Tag::StmtForOf => {
                coverage.walked += 1;
                let mut f = node.fields();
                let (Ok(_span), Ok(left), Ok(right), Ok(body)) =
                    (f.span(), f.child(), f.child(), f.child())
                else {
                    return self.host_error("unreadable for-of");
                };
                let head = self.iteration_head_dead_zone(artifact, &left, scope);
                let subject = normal!(self.expression_node(artifact, &right, &head, coverage));
                let mut record = match crate::iterator::get_iterator(self, &subject) {
                    Ok(record) => record,
                    Err(abrupt) => return abrupt,
                };
                let outcome = {
                    let record = &mut record;
                    self.loop_body(label, scope, &[], |me, inner| {
                        let result = match crate::iterator::iterator_step(me, record) {
                            Ok(Some(result)) => result,
                            Ok(None) => return Completion::Break(None),
                            Err(abrupt) => return abrupt,
                        };
                        let value = match crate::iterator::iterator_value(me, result) {
                            Ok(value) => value,
                            Err(abrupt) => {
                                record.done = true;
                                return abrupt;
                            }
                        };
                        if let Some(abrupt) =
                            me.bind_iteration_head_node(artifact, &left, value, inner, coverage)
                        {
                            return abrupt;
                        }
                        me.statement_node(artifact, &body, inner, coverage)
                    })
                };
                crate::iterator::iterator_close(self, &record, outcome)
            }
            _ => self.internal_defect("a loop tag reached the loop driver that is not a loop"),
        }
    }

    fn expression_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        let mut f = node.fields();
        match node.tag() {
            Tag::ExprNumberSmall => {
                coverage.walked += 1;
                let (Ok(_span), Ok(int)) = (f.span(), f.i64()) else {
                    return self.host_error("unreadable small integer");
                };
                #[allow(clippy::cast_precision_loss)]
                Completion::Normal(JsValue::Number(int as f64))
            }
            Tag::ExprNumber => {
                coverage.walked += 1;
                let (Ok(_span), Ok(bits)) = (f.span(), f.f64_bits()) else {
                    return self.host_error("unreadable number");
                };
                Completion::Normal(JsValue::Number(f64::from_bits(bits)))
            }
            Tag::ExprBoolean => {
                coverage.walked += 1;
                let (Ok(_span), Ok(value)) = (f.span(), f.bool()) else {
                    return self.host_error("unreadable boolean");
                };
                Completion::Normal(JsValue::Boolean(value))
            }
            Tag::ExprNull => {
                coverage.walked += 1;
                Completion::Normal(JsValue::Null)
            }
            Tag::ExprString => {
                coverage.walked += 1;
                let (Ok(_span), Ok(id)) = (f.span(), f.str_id()) else {
                    return self.host_error("unreadable string");
                };
                let Ok(units) = artifact.str_utf16(id) else {
                    return self.host_error("unreadable string blob");
                };
                let units: Vec<u16> = units.collect();
                Completion::Normal(JsValue::String(crate::JsString::from_units(&units)))
            }
            Tag::ExprParenthesized => {
                coverage.walked += 1;
                let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
                    return self.host_error("unreadable parenthesized expression");
                };
                self.expression_node(artifact, &child, scope, coverage)
            }
            Tag::ExprIdentifier => {
                coverage.walked += 1;
                let (Ok(_span), Ok(id)) = (f.span(), f.str_id()) else {
                    return self.host_error("unreadable identifier");
                };
                let Ok(name) = artifact.str_utf8(id) else {
                    return self.host_error("unreadable identifier name");
                };
                match self.lookup(scope, name) {
                    Ok(value) => Completion::Normal(value),
                    Err(abrupt) => abrupt,
                }
            }
            Tag::ExprSequence => {
                coverage.walked += 1;
                if f.span().is_err() {
                    return self.host_error("unreadable sequence");
                }
                let Ok(parts) = collect_body(&mut f) else {
                    return self.host_error("unreadable sequence body");
                };
                let mut last = JsValue::Undefined;
                for part in &parts {
                    last = normal!(self.expression_node(artifact, part, scope, coverage));
                }
                Completion::Normal(last)
            }
            Tag::ExprConditional => {
                coverage.walked += 1;
                let (Ok(_span), Ok(test), Ok(consequent), Ok(alternate)) =
                    (f.span(), f.child(), f.child(), f.child())
                else {
                    return self.host_error("unreadable conditional");
                };
                let decision = normal!(self.expression_node(artifact, &test, scope, coverage));
                let taken = if ops::to_boolean(&decision) { consequent } else { alternate };
                self.expression_node(artifact, &taken, scope, coverage)
            }
            Tag::ExprBinary => {
                coverage.walked += 1;
                let (Ok(_span), Ok(byte), Ok(left), Ok(right)) =
                    (f.span(), f.byte(), f.child(), f.child())
                else {
                    return self.host_error("unreadable binary expression");
                };
                let Ok(operator) = decode::binary_operator(byte, node.offset()) else {
                    return self.host_error("unknown binary operator");
                };
                let left_value = normal!(self.expression_node(artifact, &left, scope, coverage));
                let right_value = normal!(self.expression_node(artifact, &right, scope, coverage));
                self.binary(operator, &left_value, &right_value)
            }
            Tag::ExprThis => {
                coverage.walked += 1;
                if !self.this_is_initialized() {
                    return self.this_before_super();
                }
                Completion::Normal(self.current_this())
            }
            Tag::ExprMember | Tag::ExprCall => {
                match self.chain_node(artifact, node, scope, coverage) {
                    Ok(Chain::Value(value)) => Completion::Normal(value),
                    Ok(Chain::ShortCircuit) => Completion::Normal(JsValue::Undefined),
                    Err(abrupt) => abrupt,
                }
            }
            Tag::ExprArray => {
                coverage.walked += 1;
                if f.span().is_err() {
                    return self.host_error("unreadable array literal");
                }
                let Ok(count) = f.count() else {
                    return self.host_error("unreadable array length");
                };
                let mut object = crate::object::Object::new(Some(self.intrinsics.array_prototype));
                object.is_array = true;
                let id = self.allocate(object);
                let mut index = 0u32;
                for _ in 0..count {
                    let Ok(element) = f.child() else {
                        return self.host_error("unreadable array element");
                    };
                    let mut e = element.fields();
                    match element.tag() {
                        Tag::ElemHole => index += 1,
                        Tag::ElemExpression => {
                            let Ok(inner) = e.child() else {
                                return self.host_error("unreadable array element");
                            };
                            let value =
                                normal!(self.expression_node(artifact, &inner, scope, coverage));
                            let _ = self.create_data_property(
                                id,
                                crate::object::PropertyKey::from_str(&index.to_string()),
                                value,
                            );
                            index += 1;
                        }
                        Tag::ElemSpread => {
                            let (Ok(_span), Ok(inner)) = (e.span(), e.child()) else {
                                return self.host_error("unreadable array spread");
                            };
                            let source =
                                normal!(self.expression_node(artifact, &inner, scope, coverage));
                            let values = match crate::iterator::iterate_to_list(self, &source) {
                                Ok(values) => values,
                                Err(abrupt) => return abrupt,
                            };
                            for value in values {
                                let _ = self.create_data_property(
                                    id,
                                    crate::object::PropertyKey::from_str(&index.to_string()),
                                    value,
                                );
                                index += 1;
                            }
                        }
                        _ => return self.host_error("not an array element"),
                    }
                }
                self.object_mut(id).set_own(
                    crate::object::PropertyKey::from_str("length"),
                    crate::object::Property {
                        kind: crate::object::PropertyKind::Data {
                            value: JsValue::Number(f64::from(index)),
                            writable: true,
                        },
                        enumerable: false,
                        configurable: false,
                    },
                );
                Completion::Normal(JsValue::Object(id))
            }
            Tag::ExprNew => {
                coverage.walked += 1;
                let (Ok(_span), Ok(callee)) = (f.span(), f.child()) else {
                    return self.host_error("unreadable new expression");
                };
                let callee_value =
                    normal!(self.expression_node(artifact, &callee, scope, coverage));
                let values = match self.argument_list_node(artifact, &mut f, scope, coverage) {
                    Ok(values) => values,
                    Err(abrupt) => return abrupt,
                };
                let JsValue::Object(id) = callee_value else {
                    return self.type_error("not a constructor");
                };
                if self.object(id).callable.is_none() {
                    return self.type_error("not a constructor");
                }
                self.construct(id, values)
            }
            Tag::ExprLogical => {
                coverage.walked += 1;
                let (Ok(_span), Ok(byte), Ok(left), Ok(right)) =
                    (f.span(), f.byte(), f.child(), f.child())
                else {
                    return self.host_error("unreadable logical expression");
                };
                let Ok(operator) = decode::logical_operator(byte, node.offset()) else {
                    return self.host_error("unknown logical operator");
                };
                let left_value = normal!(self.expression_node(artifact, &left, scope, coverage));
                use crate::ast::LogicalOperator as Logical;
                let take_right = match operator {
                    Logical::And => ops::to_boolean(&left_value),
                    Logical::Or => !ops::to_boolean(&left_value),
                    Logical::NullishCoalescing => {
                        matches!(left_value, JsValue::Null | JsValue::Undefined)
                    }
                };
                if take_right {
                    self.expression_node(artifact, &right, scope, coverage)
                } else {
                    Completion::Normal(left_value)
                }
            }
            Tag::ExprAssignment => {
                let (Ok(_span), Ok(byte), Ok(target), Ok(value)) =
                    (f.span(), f.byte(), f.child(), f.child())
                else {
                    return self.host_error("unreadable assignment");
                };
                let Ok(operator) = decode::assignment_operator(byte, node.offset()) else {
                    return self.host_error("unknown assignment operator");
                };
                if target.tag() != Tag::TargetPattern {
                    coverage.walked += 1;
                    return self.throw("SyntaxError", "invalid assignment target");
                }
                let mut t = target.fields();
                let Ok(pattern) = t.child() else {
                    return self.host_error("unreadable assignment target");
                };
                let parenthesized = matches!(t.bool(), Ok(true));
                use crate::ast::AssignmentOperator as Assign;

                match pattern.tag() {
                    Tag::PatMember => {
                        let mut m = pattern.fields();
                        let (Ok(_span), Ok(object), Ok(property)) = (m.span(), m.child(), m.child())
                        else {
                            return self.host_error("unreadable member target");
                        };
                        let through_super = object.tag() == Tag::ExprSuper;
                        if !through_super && !matches!(m.bool(), Ok(false)) {
                            return self.refuse_expression(node, coverage);
                        }
                        coverage.walked += 1;
                        let (base, receiver, pending) = if through_super {
                            match self.super_target(artifact, &property, scope, coverage) {
                                Ok(parts) => parts,
                                Err(abrupt) => return abrupt,
                            }
                        } else {
                            let base =
                                normal!(self.expression_node(artifact, &object, scope, coverage));
                            let pending = match self
                                .member_key_value(artifact, &property, scope, coverage)
                            {
                                Ok(key) => key,
                                Err(abrupt) => return abrupt,
                            };
                            (base.clone(), base, pending)
                        };
                        let (key, assigned) = if operator == Assign::Assign {
                            let assigned =
                                normal!(self.expression_node(artifact, &value, scope, coverage));
                            (pending, assigned)
                        } else {
                            let key = match self.settle_key_after_base(&base, pending) {
                                Ok(key) => key,
                                Err(abrupt) => return abrupt,
                            };
                            if let Some(logical) = super::logical_assignment(operator) {
                                let current =
                                    normal!(self.read_with_receiver(&base, &receiver, &key));
                                if !logical.takes_the_right_side(&current) {
                                    return Completion::Normal(current);
                                }
                                let right = normal!(
                                    self.expression_node(artifact, &value, scope, coverage)
                                );
                                (TargetKey::Ready(key), right)
                            } else {
                                let current =
                                    normal!(self.read_with_receiver(&base, &receiver, &key));
                                let right = normal!(
                                    self.expression_node(artifact, &value, scope, coverage)
                                );
                                let Some(binary) = super::compound_operator(operator) else {
                                    return self
                                        .internal_defect("an assignment operator with no rule");
                                };
                                (TargetKey::Ready(key), normal!(self.binary(binary, &current, &right)))
                            }
                        };
                        let written = self.put_member(&base, receiver, key, assigned.clone());
                        if written.is_abrupt() {
                            return written;
                        }
                        Completion::Normal(assigned)
                    }
                    Tag::PatArray | Tag::PatObject => {
                        coverage.walked += 1;
                        if operator != Assign::Assign {
                            return self
                                .throw("SyntaxError", "a destructuring assignment must use `=`");
                        }
                        let assigned =
                            normal!(self.expression_node(artifact, &value, scope, coverage));
                        if let Err(abrupt) = self.destructure_node(
                            artifact,
                            &pattern,
                            assigned.clone(),
                            scope,
                            super::Binds::Assign,
                            coverage,
                        ) {
                            return abrupt;
                        }
                        Completion::Normal(assigned)
                    }
                    Tag::PatIdentifier => {
                        coverage.walked += 1;
                        let mut i = pattern.fields();
                        let (Ok(_span), Ok(id)) = (i.span(), i.str_id()) else {
                            return self.host_error("unreadable assignment name");
                        };
                        let Ok(name) = artifact.str_utf8(id) else {
                            return self.host_error("unreadable assignment name");
                        };
                        let name = String::from(name);
                        let base = ok_or_abrupt!(self.resolve_binding(scope, &name));
                        let unresolvable_target = self.strict && base.is_unresolvable();
                        let assigned = if operator == Assign::Assign {
                            let assigned =
                                normal!(self.expression_node(artifact, &value, scope, coverage));
                            if !parenthesized {
                                self.name_if_anonymous(artifact, &value, &assigned, &name);
                            }
                            assigned
                        } else if let Some(logical) = super::logical_assignment(operator) {
                            let current = ok_or_abrupt!(self.binding_value_of(&base, &name));
                            if !logical.takes_the_right_side(&current) {
                                return Completion::Normal(current);
                            }
                            let assigned =
                                normal!(self.expression_node(artifact, &value, scope, coverage));
                            if !parenthesized {
                                self.name_if_anonymous(artifact, &value, &assigned, &name);
                            }
                            assigned
                        } else {
                            let current = ok_or_abrupt!(self.binding_value_of(&base, &name));
                            let right =
                                normal!(self.expression_node(artifact, &value, scope, coverage));
                            let Some(binary) = super::compound_operator(operator) else {
                                return self.internal_defect("an assignment operator with no rule");
                            };
                            normal!(self.binary(binary, &current, &right))
                        };
                        if unresolvable_target {
                            return self.throw(
                                "ReferenceError",
                                &crate::format!("assignment to the undeclared `{name}`"),
                            );
                        }
                        if let Err(abrupt) = self.put_value(&base, &name, assigned.clone()) {
                            return abrupt;
                        }
                        Completion::Normal(assigned)
                    }
                    _ => {
                        coverage.walked += 1;
                        self.throw("SyntaxError", "invalid assignment target")
                    }
                }
            }
            Tag::ExprUnary => {
                let (Ok(_span), Ok(byte)) = (f.span(), f.byte()) else {
                    return self.host_error("unreadable unary expression");
                };
                let Ok(operator) = decode::unary_operator(byte, node.offset()) else {
                    return self.host_error("unknown unary operator");
                };
                use crate::ast::UnaryOperator as Unary;
                coverage.walked += 1;
                let Ok(argument) = f.child() else {
                    return self.host_error("unreadable unary operand");
                };
                if operator == Unary::Delete {
                    return self.delete_node(artifact, &argument, scope, coverage);
                }
                let bare = strip_parenthesized(&argument, coverage);
                if operator == Unary::TypeOf && bare.tag() == Tag::ExprIdentifier {
                    let mut i = bare.fields();
                    let (Ok(_span), Ok(id)) = (i.span(), i.str_id()) else {
                        return self.host_error("unreadable typeof operand");
                    };
                    let Ok(name) = artifact.str_utf8(id) else {
                        return self.host_error("unreadable typeof operand");
                    };
                    if !ok_or_abrupt!(self.resolves(scope, name)) {
                        return Completion::Normal(JsValue::string("undefined"));
                    }
                }
                let value = normal!(self.expression_node(artifact, &argument, scope, coverage));
                self.unary(operator, &value)
            }
            Tag::ExprRegExp => {
                coverage.walked += 1;
                let (Ok(_span), Ok(body_id), Ok(flags_id)) = (f.span(), f.str_id(), f.str_id())
                else {
                    return self.host_error("unreadable regular expression literal");
                };
                let (Ok(body), Ok(flags)) =
                    (artifact.str_utf8(body_id), artifact.str_utf8(flags_id))
                else {
                    return self.host_error("unreadable regular expression blob");
                };
                let (body, flags) = (crate::JsString::from(body), crate::JsString::from(flags));
                match crate::regexp::create(self, &body, &flags) {
                    Ok(id) => Completion::Normal(JsValue::Object(id)),
                    Err(abrupt) => abrupt,
                }
            }
            Tag::ExprTemplate => {
                coverage.walked += 1;
                let (Ok(_span), Ok(quasis)) = (f.span(), collect_body(&mut f)) else {
                    return self.host_error("unreadable template literal");
                };
                let Ok(expressions) = collect_body(&mut f) else {
                    return self.host_error("unreadable template interpolations");
                };
                let mut out = crate::string_value::JsString::new();
                for (index, quasi) in quasis.iter().enumerate() {
                    match read_template_cooked(artifact, quasi) {
                        Ok(Some(cooked)) => out.extend_from(&cooked),
                        Ok(None) => {
                            return self
                                .throw("SyntaxError", "an invalid escape has no cooked value")
                        }
                        Err(()) => return self.host_error("unreadable template element"),
                    }
                    if let Some(expression) = expressions.get(index) {
                        let value =
                            normal!(self.expression_node(artifact, expression, scope, coverage));
                        match self.to_string_value(&value) {
                            Ok(text) => out.extend_from(&text),
                            Err(abrupt) => return abrupt,
                        }
                    }
                }
                Completion::Normal(JsValue::String(out))
            }
            Tag::ExprTagged => {
                coverage.walked += 1;
                let (Ok(span), Ok(tag), Ok(quasi)) = (f.span(), f.child(), f.child()) else {
                    return self.host_error("unreadable tagged template");
                };
                let bare_tag = strip_parenthesized(&tag, coverage);
                let (function, receiver) = if bare_tag.tag() == Tag::ExprMember {
                    let mut m = bare_tag.fields();
                    let (Ok(_span), Ok(object), Ok(property)) = (m.span(), m.child(), m.child())
                    else {
                        return self.host_error("unreadable tag callee");
                    };
                    let base = normal!(self.expression_node(artifact, &object, scope, coverage));
                    let key = match self
                        .member_key_after_base(artifact, &property, &base, scope, coverage)
                    {
                        Ok(key) => key,
                        Err(abrupt) => return abrupt,
                    };
                    let function = normal!(self.get_member(&base, &key));
                    (function, base)
                } else {
                    (
                        normal!(self.expression_node(artifact, &tag, scope, coverage)),
                        JsValue::Undefined,
                    )
                };
                let span = crate::source::Span::new(span.start as usize, span.end as usize);
                let strings = match self.template_objects.get(&(span.start, span.end)) {
                    Some(cached) => *cached,
                    None => {
                        let Ok(elements) = decode::template_elements(artifact, &quasi) else {
                            return self.host_error("undecodable template elements");
                        };
                        self.template_object(span, &elements)
                    }
                };
                let mut values = crate::vec![JsValue::Object(strings)];
                let mut q = quasi.fields();
                let (Ok(_span), Ok(_quasis)) = (q.span(), collect_body(&mut q)) else {
                    return self.host_error("unreadable tagged template literal");
                };
                let Ok(expressions) = collect_body(&mut q) else {
                    return self.host_error("unreadable tagged template interpolations");
                };
                for expression in &expressions {
                    values.push(normal!(self.expression_node(
                        artifact,
                        expression,
                        scope,
                        coverage
                    )));
                }
                self.call_value(&function, receiver, values)
            }
            Tag::ExprClass => {
                coverage.walked += 1;
                let Ok(class) = f.child() else {
                    return self.host_error("unreadable class expression");
                };
                self.class_value_node(artifact, &class, scope, coverage)
            }
            Tag::ExprFunction => {
                coverage.walked += 1;
                let Ok(function) = f.child() else {
                    return self.host_error("unreadable function expression");
                };
                let mut g = function.fields();
                if g.span().is_err() {
                    return self.host_error("unreadable function header");
                }
                let Ok(name) = read_optional_str(artifact, &mut g) else {
                    return self.host_error("unreadable function name");
                };
                let Some(name) = name else {
                    return match self.make_closure_node(artifact, &function, scope) {
                        Ok(value) => Completion::Normal(value),
                        Err(abrupt) => abrupt,
                    };
                };
                let inner = new_scope(Some(Rc::clone(scope)));
                let value = match self.make_closure_node(artifact, &function, &inner) {
                    Ok(value) => value,
                    Err(abrupt) => return abrupt,
                };
                inner.borrow_mut().bindings.insert(
                    name,
                    Binding {
                        value: value.clone(),
                        mutability: Mutability::ImmutableSelfName,
                        initialized: true,
                        lexical: false,
                    },
                );
                Completion::Normal(value)
            }
            Tag::ExprObject => {
                coverage.walked += 1;
                if f.span().is_err() {
                    return self.host_error("unreadable object literal");
                }
                let Ok(count) = f.count() else {
                    return self.host_error("unreadable property count");
                };
                let id = self.allocate(crate::object::Object::new(Some(
                    self.intrinsics.object_prototype,
                )));
                for _ in 0..count {
                    let Ok(property) = f.child() else {
                        return self.host_error("unreadable property");
                    };
                    let mut o = property.fields();
                    match property.tag() {
                        Tag::ObjProperty => {
                            let (Ok(_span), Ok(key_node), Ok(value_node)) =
                                (o.span(), o.child(), o.child())
                            else {
                                return self.host_error("unreadable property");
                            };
                            let (Ok(computed), Ok(shorthand)) = (o.bool(), o.bool()) else {
                                return self.host_error("unreadable property flags");
                            };
                            let key =
                                match self.property_key_node(artifact, &key_node, scope, coverage) {
                                    Ok(key) => key,
                                    Err(abrupt) => return abrupt,
                                };
                            let is_proto_setter = !computed
                                && !shorthand
                                && matches!(&key, crate::object::PropertyKey::String(text)
                                    if text.to_lossy_string() == "__proto__");
                            let value =
                                normal!(self.expression_node(artifact, &value_node, scope, coverage));
                            if is_proto_setter {
                                match value {
                                    JsValue::Object(prototype) => {
                                        self.object_mut(id).prototype = Some(prototype);
                                    }
                                    JsValue::Null => self.object_mut(id).prototype = None,
                                    _ => {}
                                }
                                continue;
                            }
                            let name = match &key {
                                crate::object::PropertyKey::String(text) => {
                                    text.to_lossy_string()
                                }
                                crate::object::PropertyKey::Symbol(symbol) => {
                                    match self.symbol_description(*symbol) {
                                        Some(description) => alloc::format!(
                                            "[{}]",
                                            description.to_lossy_string()
                                        ),
                                        None => alloc::string::String::new(),
                                    }
                                }
                            };
                            self.name_if_anonymous(artifact, &value_node, &value, &name);
                            let _ = self.create_data_property(id, key, value);
                        }
                        Tag::ObjMethod => {
                            let (Ok(_span), Ok(key_node), Ok(function)) =
                                (o.span(), o.child(), o.child())
                            else {
                                return self.host_error("unreadable method");
                            };
                            let Ok(kind_byte) = o.byte() else {
                                return self.host_error("unreadable method kind");
                            };
                            let Ok(kind) = decode::method_kind(kind_byte, property.offset()) else {
                                return self.host_error("unknown method kind");
                            };
                            let key =
                                match self.property_key_node(artifact, &key_node, scope, coverage) {
                                    Ok(key) => key,
                                    Err(abrupt) => return abrupt,
                                };
                            let value =
                                match self.make_method_closure_node(artifact, &function, scope) {
                                    Ok(value) => value,
                                    Err(abrupt) => return abrupt,
                                };
                            let JsValue::Object(function_id) = value else {
                                return self.internal_defect("a method must be a function object");
                            };
                            self.make_method(function_id, id, &key, kind);
                            use crate::ast::MethodKind;
                            match kind {
                                MethodKind::Get | MethodKind::Set => {
                                    let existing = self.object(id).own(&key).cloned();
                                    let (mut get, mut set) = match existing.map(|p| p.kind) {
                                        Some(crate::object::PropertyKind::Accessor { get, set }) => {
                                            (get, set)
                                        }
                                        _ => (None, None),
                                    };
                                    if kind == MethodKind::Get {
                                        get = Some(function_id);
                                    } else {
                                        set = Some(function_id);
                                    }
                                    self.object_mut(id).set_own(
                                        key,
                                        crate::object::Property {
                                            kind: crate::object::PropertyKind::Accessor {
                                                get,
                                                set,
                                            },
                                            enumerable: true,
                                            configurable: true,
                                        },
                                    );
                                }
                                MethodKind::Normal => {
                                    let _ = self.create_data_property(id, key, value);
                                }
                            }
                        }
                        Tag::ObjCoverInitializedName => {
                            return self
                                .throw("SyntaxError", "`{ a = 1 }` is only legal as a pattern");
                        }
                        Tag::ObjSpread => {
                            let (Ok(_span), Ok(inner)) = (o.span(), o.child()) else {
                                return self.host_error("unreadable object spread");
                            };
                            let source =
                                normal!(self.expression_node(artifact, &inner, scope, coverage));
                            if let Err(abrupt) = self.copy_data_properties(id, &source, &[]) {
                                return abrupt;
                            }
                        }
                        _ => return self.host_error("not an object property"),
                    }
                }
                Completion::Normal(JsValue::Object(id))
            }
            Tag::ExprUpdate => {
                let (Ok(_span), Ok(byte), Ok(prefix), Ok(argument)) =
                    (f.span(), f.byte(), f.bool(), f.child())
                else {
                    return self.host_error("unreadable update expression");
                };
                let Ok(operator) = decode::update_operator(byte, node.offset()) else {
                    return self.host_error("unknown update operator");
                };
                let argument = &strip_parenthesized(&argument, coverage);
                use crate::ast::UpdateOperator as Update;
                let step = |old: f64| match operator {
                    Update::Increment => old + 1.0,
                    Update::Decrement => old - 1.0,
                };

                if argument.tag() == Tag::ExprMember {
                    let mut m = argument.fields();
                    let (Ok(_span), Ok(object), Ok(property)) = (m.span(), m.child(), m.child())
                    else {
                        return self.host_error("unreadable update target");
                    };
                    let through_super = object.tag() == Tag::ExprSuper;
                    if !through_super && !matches!(m.bool(), Ok(false)) {
                        return self.refuse_expression(node, coverage);
                    }
                    coverage.walked += 1;
                    let (base, receiver, key) = if through_super {
                        match self.super_reference(artifact, &property, scope, coverage) {
                            Ok(parts) => parts,
                            Err(abrupt) => return abrupt,
                        }
                    } else {
                        let base =
                            normal!(self.expression_node(artifact, &object, scope, coverage));
                        let key = match self
                            .member_key_after_base(artifact, &property, &base, scope, coverage)
                        {
                            Ok(key) => key,
                            Err(abrupt) => return abrupt,
                        };
                        (base.clone(), base, key)
                    };
                    let current = normal!(self.read_with_receiver(&base, &receiver, &key));
                    let old = match self.to_number_value(&current) {
                        Ok(old) => old,
                        Err(abrupt) => return abrupt,
                    };
                    let new = step(old);
                    let written =
                        self.write_with_receiver(&base, receiver, key, JsValue::Number(new));
                    if written.is_abrupt() {
                        return written;
                    }
                    return Completion::Normal(JsValue::Number(if prefix { new } else { old }));
                }

                if argument.tag() != Tag::ExprIdentifier {
                    coverage.walked += 1;
                    return self.internal_defect("an update target the parser did not refine");
                }
                coverage.walked += 1;
                let mut i = argument.fields();
                let (Ok(_span), Ok(id)) = (i.span(), i.str_id()) else {
                    return self.host_error("unreadable update name");
                };
                let Ok(name) = artifact.str_utf8(id) else {
                    return self.host_error("unreadable update name");
                };
                let name = String::from(name);
                let base = ok_or_abrupt!(self.resolve_binding(scope, &name));
                let current = ok_or_abrupt!(self.binding_value_of(&base, &name));
                let old = match self.to_number_value(&current) {
                    Ok(old) => old,
                    Err(abrupt) => return abrupt,
                };
                let new = step(old);
                if let Err(abrupt) = self.put_value(&base, &name, JsValue::Number(new)) {
                    return abrupt;
                }
                Completion::Normal(JsValue::Number(if prefix { new } else { old }))
            }
            Tag::ExprArrow => {
                coverage.walked += 1;
                let Ok(arrow) = f.child() else {
                    return self.host_error("unreadable arrow");
                };
                let Ok((length, flags)) = function_header_node(&arrow) else {
                    return self.host_error("unreadable arrow header");
                };
                Completion::Normal(self.push_artifact_closure(
                    length,
                    "",
                    arrow.offset() as u32,
                    flags & lamella_js_bytecode::format::FN_STRICT != 0,
                    true,
                    false,
                    false,
                    false,
                    scope,
                ))
            }
            Tag::ExprSuper => {
                coverage.walked += 1;
                self.throw("SyntaxError", "`super` is only legal as `super(...)` or `super.x`")
            }
            Tag::ExprNewTarget => {
                coverage.walked += 1;
                Completion::Normal(self.current_new_target())
            }
            _ => self.refuse_expression(node, coverage),
        }
    }

    /// `ClassDefinitionEvaluation`, with the strictness the class brings with it.
    ///
    /// **ALL PARTS OF A CLASS ARE STRICT CODE**, and "all parts" reaches further than the method
    /// bodies -- which get it from their own parsed header and were right. The HERITAGE expression
    /// and every COMPUTED PROPERTY NAME are ordinary expressions written in the class, and they ran
    /// in whatever mode surrounded the class. So this, inside a sloppy function, did nothing at all
    /// instead of throwing:
    ///
    /// ```text
    /// class B { [Object.preventExtensions({}).prop = 4]() {} }
    /// ```
    ///
    /// THE HERITAGE IS WHERE THIS LOOKS CORRECT WITHOUT BEING CORRECT. The same expression in
    /// `extends` position produces a TypeError with or without the strictness: a sloppy write is a
    /// silent no-op, so the clause evaluates to `4`, and `4` is not a constructor. Right answer,
    /// wrong step -- a differential probe agrees on that line while the two engines are doing
    /// different things.
    ///
    /// Restoring the flag is not optional even though a class body is the last thing evaluated in
    /// most programs: a class EXPRESSION sits inside a larger one, and `(class {}, x = 1)` must
    /// still be sloppy after the comma.
    fn class_value_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        let surrounding = self.strict;
        self.strict = true;
        let completion = self.class_definition_node(artifact, node, scope, coverage);
        self.strict = surrounding;
        completion
    }

    /// Builds a class from its artifact node: a constructor, its prototype, and the members on each.
    ///
    /// **A CLASS IS NOT SUGAR FOR A FUNCTION PLUS SOME ASSIGNMENTS, AND THE DIFFERENCES ARE ALL
    /// INVISIBLE IN THE SYNTAX.** Its methods are NON-ENUMERABLE where an object literal's are
    /// enumerable, so `for (var k in new C())` lists nothing; its body is always strict; and
    /// `constructor` is not a method but the class's own callable. An implementation that desugars
    /// to `function C() {}; C.prototype.m = ...` gets the first of those wrong for every class ever
    /// written, and the symptom is extra keys in ordinary loops far from the class.
    ///
    /// **THE IMPLICIT CONSTRUCTOR IS THE ONE THING HERE THAT HAS NO ARTIFACT OFFSET.** Every
    /// other body in the port is reached by arithmetic from a `Function` node, but a class with no
    /// declared `constructor` has no such node to point at -- the tree does not contain one either,
    /// which is exactly why the round trip stays lossless. So its `code_at` is `None`, and that is a
    /// THIRD state rather than a missing one: not "a body somewhere else", but "no body at all".
    fn class_definition_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        let mut f = node.fields();
        if f.span().is_err() {
            return self.host_error("unreadable class");
        }
        let Ok(name) = read_optional_str(artifact, &mut f) else {
            return self.host_error("unreadable class name");
        };
        let heritage = match f.option_child() {
            Ok(heritage) => heritage,
            Err(_) => return self.host_error("unreadable class heritage"),
        };
        let Ok(members) = collect_body(&mut f) else {
            return self.host_error("unreadable class members");
        };

        let inner = new_scope(Some(Rc::clone(scope)));
        if let Some(name) = &name {
            inner.borrow_mut().bindings.insert(
                name.clone(),
                Binding {
                    value: JsValue::Undefined,
                    mutability: Mutability::Immutable,
                    initialized: false,
                    lexical: false,
                },
            );
        }
        let (parent, derived) = match heritage {
            None => (None, false),
            Some(expression) => {
                match normal!(self.expression_node(artifact, &expression, &inner, coverage)) {
                    JsValue::Object(parent) if self.is_constructor(parent) => (Some(parent), true),
                    JsValue::Null => (None, true),
                    _ => return self.type_error("a class heritage must be a constructor or null"),
                }
            }
        };

        let mut headers = Vec::with_capacity(members.len());
        for member in &members {
            match class_member_header(artifact, member) {
                Ok(header) => headers.push(header),
                Err(()) => return self.host_error("unreadable class member"),
            }
        }
        let constructor_index = headers.iter().position(|header| {
            !header.is_static
                && !header.computed
                && header.kind == crate::ast::MethodKind::Normal
                && !header.is_generator
                && class_member_key_is(artifact, &header.key, "constructor")
        });

        let parent_prototype = match (parent, derived) {
            (None, false) => Some(self.intrinsics.object_prototype),
            (None, true) => None,
            (Some(parent), _) => {
                match self.get_property(parent, &crate::object::PropertyKey::from_str("prototype")) {
                    Completion::Normal(JsValue::Object(prototype)) => Some(prototype),
                    Completion::Normal(JsValue::Null) => None,
                    Completion::Normal(_) => {
                        return self.type_error("the parent's `prototype` is not an object")
                    }
                    abrupt => return abrupt,
                }
            }
        };
        let prototype = self.allocate(crate::object::Object::new(parent_prototype));

        let (length, needs_arguments, code_at) = match constructor_index {
            Some(index) => {
                let function = &headers[index].function;
                match function_header_node(function) {
                    Ok((length, flags)) => (
                        length,
                        flags & lamella_js_bytecode::format::FN_NO_ARGUMENTS == 0,
                        Some(super::CodeRef {
                            program: self.current_program,
                            at: function.offset() as u32,
                        }),
                    ),
                    Err(()) => return self.host_error("unreadable constructor parameters"),
                }
            }
            None => (0, false, None),
        };
        self.closures.push(super::Closure {
            code_at,
            needs_arguments,
            scope: Rc::clone(&inner),
            is_arrow: false,
            captured_this_cell: None,
            captured_this: None,
            strict: true,
            is_method: false,
            is_generator: false,
            home_object: Some(prototype),
            implicit_derived: derived && constructor_index.is_none(),
            class_object: None,
            class_kind: if derived {
                super::ClassKind::Derived
            } else {
                super::ClassKind::Base
            },
            captured_new_target: None,
        });
        let closure_index = (self.closures.len() - 1) as u32;
        let mut object = crate::object::Object::new(Some(match parent {
            Some(parent) => parent,
            None => self.intrinsics.function_prototype,
        }));
        object.callable = Some(crate::object::Callable::Closure(closure_index));
        let constructor = self.allocate(object);
        self.closures[closure_index as usize].class_object = Some(constructor);
        self.set_function_metadata(constructor, name.as_deref().unwrap_or(""), length);
        self.object_mut(constructor).set_own(
            crate::object::PropertyKey::from_str("prototype"),
            crate::object::Property {
                kind: crate::object::PropertyKind::Data {
                    value: JsValue::Object(prototype),
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            },
        );
        self.object_mut(prototype).set_own(
            crate::object::PropertyKey::from_str("constructor"),
            crate::object::Property::builtin(JsValue::Object(constructor)),
        );

        if let Some(name) = &name {
            inner.borrow_mut().bindings.insert(
                name.clone(),
                Binding {
                    value: JsValue::Object(constructor),
                    mutability: Mutability::Immutable,
                    initialized: true,
                    lexical: false,
                },
            );
        }

        for (index, header) in headers.iter().enumerate() {
            if constructor_index == Some(index) {
                continue;
            }
            let target = if header.is_static { constructor } else { prototype };
            let key = match self.property_key_node(artifact, &header.key, &inner, coverage) {
                Ok(key) => key,
                Err(abrupt) => return abrupt,
            };
            let value = match self.make_method_closure_node(artifact, &header.function, &inner) {
                Ok(value) => value,
                Err(abrupt) => return abrupt,
            };
            let JsValue::Object(function_id) = value else {
                return self.internal_defect("a class member must be a function object");
            };
            self.make_method(function_id, target, &key, header.kind);
            let property = match header.kind {
                crate::ast::MethodKind::Get | crate::ast::MethodKind::Set => {
                    let existing = self.object(target).own(&key).cloned();
                    let (mut get, mut set) = match existing.map(|property| property.kind) {
                        Some(crate::object::PropertyKind::Accessor { get, set }) => (get, set),
                        _ => (None, None),
                    };
                    if header.kind == crate::ast::MethodKind::Get {
                        get = Some(function_id);
                    } else {
                        set = Some(function_id);
                    }
                    crate::object::Property::accessor(get, set)
                }
                crate::ast::MethodKind::Normal => crate::object::Property::builtin(value),
            };
            if let Err(abrupt) = crate::builtins::define_complete_or_throw(self, target, key, &property)
            {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Object(constructor))
    }


    /// Runs a declaration: evaluate each initializer, bind each target.
    ///
    /// Takes `StmtDeclaration` **or** `ForInitDeclaration`, which carry the identical payload --
    /// span, kind byte, declarator run. One function because they are one operation; the tags differ
    /// only in where the declaration was written.
    ///
    /// The initializer is evaluated BEFORE the target is bound, and both are walked. The target
    /// used to be decoded on the reasoning that a binding pattern is a small subtree; a pattern
    /// DEFAULT is an arbitrary expression, so it was not bounded at all.
    fn declaration_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        let mut f = node.fields();
        let (Ok(_span), Ok(kind), Ok(count)) = (f.span(), f.byte(), f.count()) else {
            return self.host_error("unreadable declaration");
        };
        let Ok(kind) = decode::declaration_kind(kind, node.offset()) else {
            return self.host_error("unknown declaration kind");
        };
        for _ in 0..count {
            let Ok(declarator) = f.child() else {
                return self.host_error("unreadable declarator");
            };
            let mut d = declarator.fields();
            let (Ok(_span), Ok(target)) = (d.span(), d.child()) else {
                return self.host_error("unreadable declarator target");
            };
            if kind == crate::ast::DeclarationKind::Var {
                let mut probe = d;
                if let (Some(name), Ok(Some(init))) =
                    (read_identifier_name(artifact, &target), probe.option_child())
                {
                    let reference = ok_or_abrupt!(self.resolve_binding(scope, name));
                    let value = normal!(self.expression_node(artifact, &init, scope, coverage));
                    self.name_if_anonymous(artifact, &init, &value, name);
                    if let Err(abrupt) = self.put_value(&reference, name, value) {
                        return abrupt;
                    }
                    continue;
                }
            }
            let value = match d.option_child() {
                Ok(Some(init)) => {
                    let value = normal!(self.expression_node(artifact, &init, scope, coverage));
                    if let Some(name) = read_identifier_name(artifact, &target) {
                        self.name_if_anonymous(artifact, &init, &value, name);
                    }
                    value
                }
                Ok(None) if kind == crate::ast::DeclarationKind::Var => continue,
                Ok(None) => JsValue::Undefined,
                Err(_) => return self.host_error("unreadable initializer"),
            };
            let binds = if kind == crate::ast::DeclarationKind::Var && self.is_global_scope(scope) {
                super::Binds::GlobalVar
            } else {
                super::Binds::Var
            };
            if let Err(abrupt) =
                self.destructure_node(artifact, &target, value, scope, binds, coverage)
            {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Undefined)
    }

    /// Creates the temporal-dead-zone bindings a `let`/`const` declaration introduces.
    ///
    /// **ONLY THE TARGETS ARE READ, WHICH IS THE WHOLE POINT.** Hoisting needs the NAMES a
    /// declaration binds and nothing else, so decoding the statement -- initializers included --
    /// to answer a question about its targets rebuilds most of the subtree on every block entry.
    /// That is the RAM this tier exists to stop spending, being spent by the bookkeeping rather
    /// than by the program.
    fn hoist_declaration_node(&mut self, artifact: &Artifact<'_>, node: &Node<'_>, scope: &Scope) {
        let mut f = node.fields();
        let (Ok(_span), Ok(kind), Ok(count)) = (f.span(), f.byte(), f.count()) else {
            return;
        };
        let Ok(kind) = decode::declaration_kind(kind, node.offset()) else {
            return;
        };
        if kind == crate::ast::DeclarationKind::Var {
            return;
        }
        let mutability = if kind == crate::ast::DeclarationKind::Const {
            Mutability::Immutable
        } else {
            Mutability::Mutable
        };
        for _ in 0..count {
            let Ok(declarator) = f.child() else { return };
            let mut d = declarator.fields();
            let (Ok(_span), Ok(target)) = (d.span(), d.child()) else { return };
            let mut names = Vec::new();
            pattern_names_node(artifact, &target, &mut names);
            for name in names {
                scope.borrow_mut().bindings.insert(
                    name,
                    Binding {
                        value: JsValue::Undefined,
                        mutability,
                        initialized: false,
                        lexical: true,
                    },
                );
            }
        }
    }

    /// `ForIn/OfHeadEvaluation` step 2: the environment the SUBJECT of a `for-in`/`for-of` is
    /// evaluated in.
    ///
    /// # THE HEAD'S OWN NAMES ARE IN SCOPE WHILE THE THING IT ITERATES IS EVALUATED
    ///
    /// `for (let x of [x])` does not read an outer `x`; it throws, because `BoundNames of
    /// ForDeclaration` are created -- uninitialized -- in an environment of their own before the
    /// expression runs. Without it the subject is evaluated one link too far out, and every one of
    /// these answers a plausible value instead of raising:
    ///
    /// ```text
    /// let x = 1; for (let x in { x }) ;           // ReferenceError, not a walk over {x: 1}
    /// for (let x of (f = () => typeof x, [])) ;   // f() throws, and `typeof` does not excuse it
    /// ```
    ///
    /// `typeof` SUPPRESSES THE ERROR FOR AN *UNRESOLVABLE* NAME, NOT FOR A DEAD ZONE -- which is
    /// what makes the omission visible at all, and why the corpus probes it with `typeof`.
    ///
    /// THE LOOP BODY DOES NOT RUN IN THIS ENVIRONMENT. It is discarded once the subject has been
    /// evaluated, and each iteration gets a fresh one whose parent is the scope around the loop.
    ///
    /// A `var` HEAD AND AN ASSIGNMENT HEAD GET NOTHING, which is the spec's empty-`TDZnames` case
    /// rather than a shortcut: `for (var x in { x })` reads the hoisted `x`, and must.
    fn iteration_head_dead_zone(
        &mut self,
        artifact: &Artifact<'_>,
        left: &Node<'_>,
        scope: &Scope,
    ) -> Scope {
        if left.tag() != Tag::ForInitDeclaration {
            return Rc::clone(scope);
        }
        let dead_zone = new_scope(Some(Rc::clone(scope)));
        self.hoist_declaration_node(artifact, left, &dead_zone);
        let empty = dead_zone.borrow().bindings.is_empty();
        if empty {
            Rc::clone(scope)
        } else {
            dead_zone
        }
    }

    /// Binds one iteration's value to a `for-in`/`for-of` head. `Some(..)` is an abrupt completion.
    ///
    /// **ONE FUNCTION BECAUSE THE RULE IS ONE RULE.** The AST path spells this out twice, once in
    /// `for_in` and once in `for_of`, and the two copies are identical -- which is the shape that
    /// let a labelled `for-of` diverge from a labelled `while` in this same file once before. **A
    /// rule implemented at one of two entrances is implemented at neither.**
    ///
    /// The head is re-read from the artifact on every iteration and that is the point: reading it
    /// is a handful of varints and no allocation, where decoding it once per loop built a `ForInit`
    /// that stayed resident for the loop's whole life.
    fn bind_iteration_head_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        left: &Node<'a>,
        value: JsValue,
        inner: &Scope,
        coverage: &mut Coverage,
    ) -> Option<Completion> {
        coverage.walked += 1;
        let mut f = left.fields();
        match left.tag() {
            Tag::ForInitDeclaration => {
                let (Ok(_span), Ok(kind), Ok(count)) = (f.span(), f.byte(), f.count()) else {
                    return Some(self.host_error("unreadable iterating head"));
                };
                let Ok(kind) = decode::declaration_kind(kind, left.offset()) else {
                    return Some(self.host_error("unknown declaration kind"));
                };
                if count == 0 {
                    return Some(self.internal_defect("an iterating head with no binding"));
                }
                let Ok(declarator) = f.child() else {
                    return Some(self.host_error("unreadable iterating head"));
                };
                let mut d = declarator.fields();
                let (Ok(_span), Ok(target)) = (d.span(), d.child()) else {
                    return Some(self.host_error("unreadable iterating head target"));
                };
                if kind != crate::ast::DeclarationKind::Var {
                    self.hoist_declaration_node(artifact, left, inner);
                }
                self.destructure_node(artifact, &target, value, inner, super::Binds::Var, coverage)
                    .err()
            }
            Tag::ForInitPattern => {
                let Ok(pattern) = f.child() else {
                    return Some(self.host_error("unreadable iterating head"));
                };
                self.destructure_node(
                    artifact,
                    &pattern,
                    value,
                    inner,
                    super::Binds::Assign,
                    coverage,
                )
                .err()
            }
            Tag::ForInitExpression => {
                Some(self.internal_defect("an iterating head the parser did not refine"))
            }
            _ => Some(self.host_error("not an iterating head")),
        }
    }

    /// `NamedEvaluation`: give an anonymous function the name of the binding it is being given to.
    ///
    /// # THE TEST IS SYNTACTIC, NOT "THE NAME HAPPENS TO BE EMPTY"
    ///
    /// `var f = function g() {}` leaves `f.name` as `"g"`, and `var f = (function () {})` -- the
    /// COVER form, parenthesized -- still names it `"f"`. So the question is what was WRITTEN, which
    /// only the node can answer; inspecting the produced function instead gets the first case wrong
    /// the moment anyone writes a named function expression.
    ///
    /// It is also why this takes the source NODE and not just the value.
    fn name_if_anonymous(
        &mut self,
        artifact: &Artifact<'_>,
        source: &Node<'_>,
        value: &JsValue,
        name: &str,
    ) {
        if !is_anonymous_function_definition(artifact, source) {
            return;
        }
        let JsValue::Object(id) = value else { return };
        if self.object(*id).callable.is_none() {
            return;
        }
        let body_named_it = self
            .object(*id)
            .own(&crate::object::PropertyKey::from_str("name"))
            .is_some_and(|property| {
                !matches!(property.data_value(), Some(JsValue::String(text)) if text.is_empty())
            });
        if body_named_it {
            return;
        }
        self.object_mut(*id).set_own(
            crate::object::PropertyKey::from_str("name"),
            crate::object::Property {
                kind: crate::object::PropertyKind::Data {
                    value: JsValue::string(name),
                    writable: false,
                },
                enumerable: false,
                configurable: true,
            },
        );
    }


    /// `var [a] = x` and a `for (var [a] of ...)` head: bind into the hoisted `var`.
    /// A `let`/`const`, a parameter, a `catch` binding: create the names in THIS scope.
    ///
    /// **EVERY LEAF DECLARES, NOT JUST A TOP-LEVEL IDENTIFIER.** A destructured parameter
    /// (`function f([a]) {}`) or `catch` binding must create its names here; assigning into a
    /// hoisted `var` instead would silently write to an outer binding of the same name, or create a
    /// global.
    pub(crate) fn declare_pattern_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        pattern: &Node<'a>,
        value: JsValue,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<(), Completion> {
        self.destructure_node(artifact, pattern, value, scope, super::Binds::Declare, coverage)
    }

    /// Applies a destructuring pattern to a value, reading the pattern out of the artifact.
    ///
    /// # ONE FUNCTION FOR THREE SYNTAXES, BECAUSE THEY ARE ONE ALGORITHM
    ///
    /// `var [a] = x`, `let [a] = x` and `[a] = x` differ ONLY in what happens at a leaf: bind into
    /// the hoisted `var`, create a fresh binding here, or assign to whatever the target already
    /// names. Everything above the leaf -- the iterator, the elisions, the defaults, the rest
    /// element, when the iterator is closed -- is identical, and writing it three times is three
    /// places for those answers to drift apart.
    ///
    /// # WHY THIS EXISTS AT ALL, WHEN A DECODED PATTERN WAS ALREADY CORRECT
    ///
    /// A pattern was decoded and handed to the AST evaluator, on the reasoning that a pattern is a
    /// small subtree. **It is not bounded, because a pattern DEFAULT is an arbitrary expression** --
    /// it may be a function, an arrow or a class, so the decoded path had to be able to BUILD A
    /// CLOSURE, which kept the entire AST expression evaluator alive to serve it. Walking the
    /// pattern is what lets that evaluator go.
    ///
    /// [`super::Binds::Assign`] is also the only mode in which a leaf may be a MEMBER expression:
    /// `[o.x] = y` is legal and `var [o.x] = y` is a SyntaxError, which the parser already enforces.
    fn destructure_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        pattern: &Node<'a>,
        value: JsValue,
        scope: &Scope,
        binds: super::Binds,
        coverage: &mut Coverage,
    ) -> Result<(), Completion> {
        coverage.walked += 1;
        let mut f = pattern.fields();
        match pattern.tag() {
            Tag::PatIdentifier => {
                let (Ok(_span), Ok(id)) = (f.span(), f.str_id()) else {
                    return Err(self.host_error("unreadable binding name"));
                };
                let Ok(name) = artifact.str_utf8(id) else {
                    return Err(self.host_error("unreadable binding name"));
                };
                self.bind_identifier(name, value, scope, binds)
            }
            Tag::PatMember => {
                if binds != super::Binds::Assign {
                    return Err(self.internal_defect("a member target in a binding position"));
                }
                let (Ok(_span), Ok(object), Ok(property)) = (f.span(), f.child(), f.child()) else {
                    return Err(self.host_error("unreadable member target"));
                };
                if object.tag() == Tag::ExprSuper {
                    let (base, receiver, key) =
                        self.super_target(artifact, &property, scope, coverage)?;
                    let written = self.put_member(&base, receiver, key, value);
                    return match written {
                        Completion::Normal(_) => Ok(()),
                        abrupt => Err(abrupt),
                    };
                }
                let base = match self.expression_node(artifact, &object, scope, coverage) {
                    Completion::Normal(base) => base,
                    abrupt => return Err(abrupt),
                };
                let key = self.member_key_value(artifact, &property, scope, coverage)?;
                let written = self.put_member(&base, base.clone(), key, value);
                if written.is_abrupt() {
                    return Err(written);
                }
                Ok(())
            }
            Tag::PatDefault => {
                let (Ok(_span), Ok(target), Ok(default)) = (f.span(), f.child(), f.child()) else {
                    return Err(self.host_error("unreadable pattern default"));
                };
                let value = if matches!(value, JsValue::Undefined) {
                    let produced = match self.expression_node(artifact, &default, scope, coverage) {
                        Completion::Normal(value) => value,
                        abrupt => return Err(abrupt),
                    };
                    if let Some(name) = read_identifier_name(artifact, &target) {
                        self.name_if_anonymous(artifact, &default, &produced, name);
                    }
                    produced
                } else {
                    value
                };
                self.destructure_node(artifact, &target, value, scope, binds, coverage)
            }
            Tag::PatRest => {
                let (Ok(_span), Ok(argument)) = (f.span(), f.child()) else {
                    return Err(self.host_error("unreadable rest element"));
                };
                self.destructure_node(artifact, &argument, value, scope, binds, coverage)
            }
            Tag::PatArray => {
                if f.span().is_err() {
                    return Err(self.host_error("unreadable array pattern"));
                }
                self.destructure_array_node(artifact, f, value, scope, binds, coverage)
            }
            Tag::PatObject => {
                if f.span().is_err() {
                    return Err(self.host_error("unreadable object pattern"));
                }
                self.destructure_object_node(artifact, f, &value, scope, binds, coverage)
            }
            _ => {
                coverage.walked -= 1;
                coverage.refused += 1;
                coverage.by_tag[pattern.tag() as u8 as usize] += 1;
                Err(self
                    .host_error(&crate::format!("no arm for pattern tag {}", pattern.tag() as u8)))
            }
        }
    }

    /// `IteratorBindingInitialization` / `ArrayAssignmentPattern`.
    ///
    /// **AN ARRAY PATTERN IS THE ITERATOR PROTOCOL, NOT INDEXING.** `var [a, b] = x` asks `x` for
    /// an iterator and takes two steps; it never reads `x[0]`. The difference is invisible for a
    /// dense array and total for everything else -- and it is why this and `for-of` had to be one
    /// piece of work rather than two.
    ///
    /// `fields` arrives positioned at the element count, so the caller has already consumed the
    /// span. `Fields` is `Copy` and holds two offsets, so passing it by value copies nothing that
    /// matters and keeps the cursor's position part of the signature.
    fn destructure_array_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        fields: lamella_js_bytecode::Fields<'a>,
        value: JsValue,
        scope: &Scope,
        binds: super::Binds,
        coverage: &mut Coverage,
    ) -> Result<(), Completion> {
        let mut record = crate::iterator::get_iterator(self, &value)?;
        let outcome =
            self.destructure_array_elements_node(artifact, fields, scope, binds, &mut record, coverage);
        match outcome {
            Ok(()) => {
                let closed = crate::iterator::iterator_close(
                    self,
                    &record,
                    Completion::Normal(JsValue::Undefined),
                );
                if closed.is_abrupt() {
                    return Err(closed);
                }
                Ok(())
            }
            Err(abrupt) => Err(crate::iterator::iterator_close(self, &record, abrupt)),
        }
    }

    fn destructure_array_elements_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        mut fields: lamella_js_bytecode::Fields<'a>,
        scope: &Scope,
        binds: super::Binds,
        record: &mut crate::iterator::IteratorRecord,
        coverage: &mut Coverage,
    ) -> Result<(), Completion> {
        let Ok(count) = fields.count() else {
            return Err(self.host_error("unreadable array pattern"));
        };
        for _ in 0..count {
            let element = match fields.option_child() {
                Ok(element) => element,
                Err(_) => return Err(self.host_error("unreadable array pattern element")),
            };
            let target = match element {
                Some(element) => {
                    Some(self.resolve_target_node(artifact, &element, scope, binds, coverage)?)
                }
                None => None,
            };
            let item = if record.done {
                JsValue::Undefined
            } else {
                match crate::iterator::iterator_step(self, record)? {
                    None => JsValue::Undefined,
                    Some(result) => match crate::iterator::iterator_value(self, result) {
                        Ok(item) => item,
                        Err(abrupt) => {
                            record.done = true;
                            return Err(abrupt);
                        }
                    },
                }
            };
            if let Some(target) = target {
                self.assign_resolved_node(artifact, target, item, scope, binds, coverage)?;
            }
        }
        let rest = match fields.option_child() {
            Ok(rest) => rest,
            Err(_) => return Err(self.host_error("unreadable rest element")),
        };
        if let Some(rest) = rest {
            let target = self.resolve_target_node(artifact, &rest, scope, binds, coverage)?;
            let mut values = Vec::new();
            while !record.done {
                match crate::iterator::iterator_step(self, record)? {
                    None => break,
                    Some(result) => match crate::iterator::iterator_value(self, result) {
                        Ok(item) => values.push(item),
                        Err(abrupt) => {
                            record.done = true;
                            return Err(abrupt);
                        }
                    },
                }
            }
            let array = self.new_array(values);
            self.assign_resolved_node(
                artifact,
                target,
                JsValue::Object(array),
                scope,
                binds,
                coverage,
            )?;
        }
        Ok(())
    }

    /// Evaluates the REFERENCE half of a destructuring target, ahead of the value that will be
    /// written to it.
    ///
    /// # A REFERENCE IS TWO HALVES AND THE STANDARD SPLITS THEM ACROSS THE ITERATOR STEP
    ///
    /// `[o.x] = it` evaluates `o` and the key `"x"` **first**, then steps the iterator, then writes.
    /// Doing the whole target after the step reads correctly and is a different program: a throw
    /// from `o` happens after a value has been taken, and for a REST element it happens after the
    /// iterator has been drained -- which does not terminate when the iterator never reports `done`.
    ///
    /// Only an ASSIGNMENT has a reference to evaluate. A binding pattern's leaf is a name being
    /// created, so there is nothing to resolve early and this hands the node back unchanged.
    fn resolve_target_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        pattern: &Node<'a>,
        scope: &Scope,
        binds: super::Binds,
        coverage: &mut Coverage,
    ) -> Result<TargetNode<'a>, Completion> {
        if binds != super::Binds::Assign {
            return Ok(TargetNode::Pattern(*pattern));
        }
        let (inner, default) = match pattern.tag() {
            Tag::PatDefault => {
                let mut f = pattern.fields();
                let (Ok(_span), Ok(target), Ok(value)) = (f.span(), f.child(), f.child()) else {
                    return Err(self.host_error("unreadable pattern default"));
                };
                (target, Some(value))
            }
            Tag::PatRest => {
                let mut f = pattern.fields();
                let (Ok(_span), Ok(argument)) = (f.span(), f.child()) else {
                    return Err(self.host_error("unreadable rest element"));
                };
                (argument, None)
            }
            _ => (*pattern, None),
        };
        if inner.tag() != Tag::PatMember {
            return Ok(TargetNode::Pattern(*pattern));
        }
        let mut m = inner.fields();
        let (Ok(_span), Ok(object), Ok(property)) = (m.span(), m.child(), m.child()) else {
            return Err(self.host_error("unreadable member target"));
        };
        if object.tag() == Tag::ExprSuper {
            let (base, receiver, key) = self.super_target(artifact, &property, scope, coverage)?;
            return Ok(TargetNode::Member { base, receiver, key, default });
        }
        let base = match self.expression_node(artifact, &object, scope, coverage) {
            Completion::Normal(base) => base,
            abrupt => return Err(abrupt),
        };
        let key = self.member_key_value(artifact, &property, scope, coverage)?;
        Ok(TargetNode::Member { base: base.clone(), receiver: base, key, default })
    }

    fn assign_resolved_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        target: TargetNode<'a>,
        value: JsValue,
        scope: &Scope,
        binds: super::Binds,
        coverage: &mut Coverage,
    ) -> Result<(), Completion> {
        match target {
            TargetNode::Pattern(pattern) => {
                self.destructure_node(artifact, &pattern, value, scope, binds, coverage)
            }
            TargetNode::Member { base, receiver, key, default } => {
                let value = match default {
                    Some(default) if matches!(value, JsValue::Undefined) => {
                        match self.expression_node(artifact, &default, scope, coverage) {
                            Completion::Normal(value) => value,
                            abrupt => return Err(abrupt),
                        }
                    }
                    _ => value,
                };
                let written = self.put_member(&base, receiver, key, value);
                if written.is_abrupt() {
                    return Err(written);
                }
                Ok(())
            }
        }
    }

    /// `ObjectBindingInitialization`.
    ///
    /// **NOT the iterator protocol at all** -- an object pattern is property reads, in written
    /// order, and a computed key runs its expression at its own position among them.
    ///
    /// `fields` arrives positioned at the property count, as in [`Self::destructure_array_node`].
    fn destructure_object_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        mut fields: lamella_js_bytecode::Fields<'a>,
        value: &JsValue,
        scope: &Scope,
        binds: super::Binds,
        coverage: &mut Coverage,
    ) -> Result<(), Completion> {
        if matches!(value, JsValue::Undefined | JsValue::Null) {
            return Err(self.type_error("cannot destructure null or undefined"));
        }
        let Ok(count) = fields.count() else {
            return Err(self.host_error("unreadable object pattern"));
        };
        let mut taken: Vec<crate::object::PropertyKey> = Vec::new();
        for _ in 0..count {
            let Ok(property) = fields.child() else {
                return Err(self.host_error("unreadable object pattern property"));
            };
            if property.tag() != Tag::ObjectPatternProperty {
                return Err(self.host_error("not an object pattern property"));
            }
            coverage.walked += 1;
            let mut p = property.fields();
            let (Ok(_span), Ok(key_node), Ok(target)) = (p.span(), p.child(), p.child()) else {
                return Err(self.host_error("unreadable object pattern property"));
            };
            let key = self.property_key_node(artifact, &key_node, scope, coverage)?;
            let target = self.resolve_target_node(artifact, &target, scope, binds, coverage)?;
            let item = match self.get_member(value, &key) {
                Completion::Normal(item) => item,
                abrupt => return Err(abrupt),
            };
            taken.push(key);
            self.assign_resolved_node(artifact, target, item, scope, binds, coverage)?;
        }
        let rest = match fields.option_child() {
            Ok(rest) => rest,
            Err(_) => return Err(self.host_error("unreadable object rest")),
        };
        if let Some(rest) = rest {
            let target = self.resolve_target_node(artifact, &rest, scope, binds, coverage)?;
            let copy = self.allocate(crate::object::Object::new(Some(
                self.intrinsics.object_prototype,
            )));
            self.copy_data_properties(copy, value, &taken)?;
            self.assign_resolved_node(
                artifact,
                target,
                JsValue::Object(copy),
                scope,
                binds,
                coverage,
            )?;
        }
        Ok(())
    }

    /// Binds a function's declared parameters from its artifact node. `Some(..)` is abrupt.
    ///
    /// **A REST PARAMETER IS *NOT* THE ITERATOR PROTOCOL.** `function f(...xs)` collects the
    /// remaining ARGUMENTS -- an internal list no user code can hook -- where `[...xs]` drives an
    /// iterator. Routing a rest parameter through `iterate_to_list` would make `f.apply(null, x)`
    /// ask `x` for an iterator, which no engine does.
    ///
    /// `fields` arrives positioned at the parameter count and is left just past the run, because
    /// the caller reads the BODY out of the same cursor immediately afterwards.
    fn bind_params_node<'a>(
        &mut self,
        artifact: &Artifact<'a>,
        fields: &mut lamella_js_bytecode::Fields<'a>,
        arguments: &[JsValue],
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<bool, Completion> {
        let Ok(count) = fields.count() else {
            return Err(self.host_error("unreadable parameter list"));
        };
        let mut scan = *fields;
        let mut names: Option<crate::Vec<crate::String>> = Some(crate::Vec::new());
        for _ in 0..count {
            let Ok(param) = scan.child() else {
                names = None;
                break;
            };
            if param.tag() != Tag::PatIdentifier {
                names = None;
                break;
            }
            let mut p = param.fields();
            let (Ok(_span), Ok(id)) = (p.span(), p.str_id()) else {
                names = None;
                break;
            };
            let Ok(name) = artifact.str_utf8(id) else {
                names = None;
                break;
            };
            if let Some(names) = names.as_mut() {
                names.push(crate::String::from(name));
            }
        }
        self.adopt_parameter_map(names.as_deref());
        let simple = names.is_some();
        {
            let mut declare = *fields;
            let mut declared = crate::Vec::new();
            for _ in 0..count {
                let Ok(param) = declare.child() else { break };
                pattern_names_node(artifact, &param, &mut declared);
            }
            let mut borrowed = scope.borrow_mut();
            for name in declared {
                borrowed.bindings.insert(
                    name,
                    Binding {
                        value: JsValue::Undefined,
                        mutability: Mutability::Mutable,
                        initialized: false,
                        lexical: false,
                    },
                );
            }
        }
        let mut bound = 0usize;
        let mut rest_taken = false;
        for _ in 0..count {
            let Ok(param) = fields.child() else {
                return Err(self.host_error("unreadable parameter"));
            };
            if rest_taken {
                continue;
            }
            if param.tag() == Tag::PatRest {
                let mut p = param.fields();
                let (Ok(_span), Ok(argument)) = (p.span(), p.child()) else {
                    return Err(self.host_error("unreadable rest parameter"));
                };
                let rest = arguments.get(bound..).unwrap_or_default().to_vec();
                let collected = JsValue::Object(self.new_array(rest));
                if let Err(abrupt) =
                    self.declare_pattern_node(artifact, &argument, collected, scope, coverage)
                {
                    return Err(abrupt);
                }
                rest_taken = true;
                continue;
            }
            let value = arguments.get(bound).cloned().unwrap_or(JsValue::Undefined);
            if let Err(abrupt) = self.declare_pattern_node(artifact, &param, value, scope, coverage)
            {
                return Err(abrupt);
            }
            bound += 1;
        }
        Ok(simple)
    }

    /// `delete`, reading the operand's SHAPE to decide which kind of reference it is.
    ///
    /// **DELETING A NON-REFERENCE IS `true`, NOT AN ERROR.** `delete 1` and `delete f()` are both
    /// legal and both report success without anything being removed -- so `true` is right for every
    /// operand that is not a member expression, and was wrong as a blanket answer.
    ///
    /// **BUT `true` IS STEP 3, AND IT IS ONLY REACHED BECAUSE STEP 1 ALREADY RAN.** 13.5.1.2
    /// opens *"Let ref be ? Evaluation of UnaryExpression"* and only then *"If ref is not a
    /// Reference Record, return true"*. Answering from the shape alone gets the value right and
    /// skips the program: `delete f()` would never call `f`, `delete (x = 1)` would never assign,
    /// and `delete (0, undeclared)` would report success where the comma expression's own
    /// `GetValue` raises a ReferenceError.
    fn delete_node(
        &mut self,
        artifact: &Artifact<'_>,
        argument: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Completion {
        let argument = &strip_parenthesized(argument, coverage);
        if argument.tag() == Tag::ExprIdentifier {
            let mut f = argument.fields();
            let (Ok(_span), Ok(id)) = (f.span(), f.str_id()) else {
                return self.host_error("unreadable delete target");
            };
            let Ok(name) = artifact.str_utf8(id) else {
                return self.host_error("unreadable delete target");
            };
            let name = String::from(name);
            coverage.walked += 1;
            let deleted = ok_or_abrupt!(self.delete_binding(scope, &name));
            return Completion::Normal(JsValue::Boolean(deleted));
        }
        if argument.tag() != Tag::ExprMember {
            normal!(self.expression_node(artifact, argument, scope, coverage));
            return Completion::Normal(JsValue::Boolean(true));
        }
        let mut f = argument.fields();
        let (Ok(_span), Ok(object), Ok(property)) = (f.span(), f.child(), f.child()) else {
            return self.host_error("unreadable member expression");
        };
        let Ok(optional) = f.bool() else {
            return self.host_error("unreadable optional flag");
        };
        if object.tag() == Tag::ExprSuper {
            if let Err(abrupt) = self.super_target(artifact, &property, scope, coverage) {
                return abrupt;
            }
            return self.throw("ReferenceError", "a super reference cannot be deleted");
        }
        let base = match self.chain_node(artifact, &object, scope, coverage) {
            Ok(Chain::Value(value)) => value,
            Ok(Chain::ShortCircuit) => return Completion::Normal(JsValue::Boolean(true)),
            Err(abrupt) => return abrupt,
        };
        if optional && matches!(base, JsValue::Undefined | JsValue::Null) {
            return Completion::Normal(JsValue::Boolean(true));
        }
        let key = match self.member_key_after_base(artifact, &property, &base, scope, coverage) {
            Ok(key) => key,
            Err(abrupt) => return abrupt,
        };
        let JsValue::Object(id) = base else {
            if matches!(base, JsValue::Undefined | JsValue::Null) {
                return self.type_error("cannot delete a property of null or undefined");
            }
            return Completion::Normal(JsValue::Boolean(true));
        };
        let removed = match self.delete_own_property(id, &key) {
            Ok(removed) => removed,
            Err(abrupt) => return abrupt,
        };
        if !removed && self.strict {
            let message =
                crate::format!("cannot delete `{}` because it is not configurable", key.to_display());
            return self.type_error(&message);
        }
        Completion::Normal(JsValue::Boolean(removed))
    }

    /// The property key a member expression names.
    fn member_key_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<crate::object::PropertyKey, Completion> {
        let mut f = node.fields();
        match node.tag() {
            Tag::MemberIdentifier | Tag::MemberPrivate => {
                let (Ok(_span), Ok(id)) = (f.span(), f.str_id()) else {
                    return Err(self.host_error("unreadable member name"));
                };
                let Ok(name) = artifact.str_utf8(id) else {
                    return Err(self.host_error("unreadable member name"));
                };
                Ok(if node.tag() == Tag::MemberPrivate {
                    crate::object::PropertyKey::from_str(&crate::format!("#{name}"))
                } else {
                    crate::object::PropertyKey::from_str(name)
                })
            }
            Tag::MemberComputed => {
                let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
                    return Err(self.host_error("unreadable computed key"));
                };
                match self.expression_node(artifact, &child, scope, coverage) {
                    Completion::Normal(value) => self.to_property_key(&value),
                    abrupt => Err(abrupt),
                }
            }
            _ => Err(self.host_error("not a member property")),
        }
    }

    /// One link of a member/call chain, evaluated where a short-circuit can still travel outward.
    ///
    /// # THE SHORT-CIRCUIT BELONGS TO THE CHAIN, NOT TO THE LINK THAT TRIGGERED IT
    ///
    /// `a?.b.c(++x).d` with a nullish `a` is `undefined`, and **`++x` never runs**. Returning
    /// `undefined` from the `a?.b` link alone -- which is what this engine did, under a comment
    /// claiming it yielded undefined "for the WHOLE chain rather than for this link" -- leaves the
    /// enclosing `.c` reading a property of `undefined`, so the program gets a TypeError where the
    /// standard gives a value, and every side effect further along the chain runs on the way there.
    /// **A comment asserting the behaviour the code does not have is the one kind of comment
    /// worse than none.**
    ///
    /// THE CHAIN IS THE SPINE OF OBJECTS AND CALLEES AND NOTHING ELSE. A computed key, an
    /// argument and a parenthesized expression all go through [`Self::expression_node`], which is
    /// where a chain becomes a value -- so `(a?.b).c` throws, `o[a?.b]` reads `o[undefined]`, and a
    /// short-circuit can never escape into an expression that merely contains one.
    fn chain_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<Chain, Completion> {
        match node.tag() {
            Tag::ExprMember => self.member_node(artifact, node, scope, coverage),
            Tag::ExprCall => self.call_node(artifact, node, scope, coverage),
            _ => match self.expression_node(artifact, node, scope, coverage) {
                Completion::Normal(value) => Ok(Chain::Value(value)),
                abrupt => Err(abrupt),
            },
        }
    }

    fn member_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<Chain, Completion> {
        let mut f = node.fields();
        let (Ok(_span), Ok(object), Ok(property)) = (f.span(), f.child(), f.child()) else {
            return Err(self.host_error("unreadable member expression"));
        };
        coverage.walked += 1;
        let Ok(optional) = f.bool() else {
            return Err(self.host_error("unreadable optional flag"));
        };
        if object.tag() == Tag::ExprSuper {
            let (base, receiver, key) =
                self.super_reference(artifact, &property, scope, coverage)?;
            return match self.read_with_receiver(&base, &receiver, &key) {
                Completion::Normal(value) => Ok(Chain::Value(value)),
                abrupt => Err(abrupt),
            };
        }
        let base = match self.chain_node(artifact, &object, scope, coverage)? {
            Chain::Value(value) => value,
            Chain::ShortCircuit => return Ok(Chain::ShortCircuit),
        };
        if optional && matches!(base, JsValue::Undefined | JsValue::Null) {
            return Ok(Chain::ShortCircuit);
        }
        let key = self.member_key_after_base(artifact, &property, &base, scope, coverage)?;
        match self.get_member(&base, &key) {
            Completion::Normal(value) => Ok(Chain::Value(value)),
            abrupt => Err(abrupt),
        }
    }

    fn call_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<Chain, Completion> {
        let mut f = node.fields();
        let (Ok(_span), Ok(callee)) = (f.span(), f.child()) else {
            return Err(self.host_error("unreadable call"));
        };
        coverage.walked += 1;
        let Ok(optional) = f.bool() else {
            return Err(self.host_error("unreadable optional-call flag"));
        };
        if callee.tag() == Tag::ExprSuper {
            if !self.frame_is_derived() {
                return Err(
                    self.throw("SyntaxError", "`super()` is only legal in a derived constructor")
                );
            }
            let Some(Some(class_object)) = self.class_stack.last().copied() else {
                return Err(
                    self.throw("SyntaxError", "`super()` is only legal in a derived constructor")
                );
            };
            let parent = self.super_constructor(class_object);
            let values = self.argument_list_node(artifact, &mut f, scope, coverage)?;
            let completion = self.construct_parent(parent, values);
            let Completion::Normal(result) = completion else { return Err(completion) };
            self.bind_this_value(result.clone())?;
            return Ok(Chain::Value(result));
        }

        let (callee_value, receiver) =
            match self.callee_node(artifact, &callee, scope, coverage)? {
                Callee::Reference(function, receiver) => (function, receiver),
                Callee::ShortCircuit => return Ok(Chain::ShortCircuit),
            };
        if optional && matches!(callee_value, JsValue::Undefined | JsValue::Null) {
            return Ok(Chain::ShortCircuit);
        }
        let values = self.argument_list_node(artifact, &mut f, scope, coverage)?;
        if !self.is_callable(&callee_value) {
            let named = match decode::expression(artifact, &callee) {
                Ok(expression) => super::callee_name(&expression),
                Err(_) => String::from("an expression"),
            };
            return Err(self.type_error(&crate::format!("{named} is not a function")));
        }
        match self.call_value(&callee_value, receiver, values) {
            Completion::Normal(value) => Ok(Chain::Value(value)),
            abrupt => Err(abrupt),
        }
    }

    /// The function a call will run, and the `this` its REFERENCE supplies.
    ///
    /// # A REFERENCE SURVIVES PARENTHESES AND DIES IN A COMMA
    ///
    /// `(o.m)()` calls with `this === o` and `(0, o.m)()` does not, because a parenthesized
    /// expression evaluates to the same Reference Record while a comma evaluates to its VALUE.
    /// This engine keyed off the callee node's own tag, so every parenthesized method call lost its
    /// receiver -- `this` became the global object in sloppy code and the method read fields off
    /// the wrong thing. The parentheses are stripped for the RECEIVER question and honoured for the
    /// CHAIN question, which are two different facts about the same node.
    fn callee_node(
        &mut self,
        artifact: &Artifact<'_>,
        callee: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<Callee, Completion> {
        let bare = strip_parenthesized(callee, coverage);
        let parenthesized = bare.offset() != callee.offset();
        if bare.tag() == Tag::ExprMember {
            let mut m = bare.fields();
            let (Ok(_span), Ok(object), Ok(property)) = (m.span(), m.child(), m.child()) else {
                return Err(self.host_error("unreadable method callee"));
            };
            let Ok(optional) = m.bool() else {
                return Err(self.host_error("unreadable optional flag"));
            };
            coverage.walked += 1;
            if object.tag() == Tag::ExprSuper {
                let (base, receiver, key) =
                    self.super_reference(artifact, &property, scope, coverage)?;
                return match self.read_with_receiver(&base, &receiver, &key) {
                    Completion::Normal(function) => Ok(Callee::Reference(function, receiver)),
                    abrupt => Err(abrupt),
                };
            }
            let base = match self.chain_node(artifact, &object, scope, coverage)? {
                Chain::Value(value) => value,
                Chain::ShortCircuit if parenthesized => {
                    return Ok(Callee::Reference(JsValue::Undefined, JsValue::Undefined));
                }
                Chain::ShortCircuit => return Ok(Callee::ShortCircuit),
            };
            if optional && matches!(base, JsValue::Undefined | JsValue::Null) {
                if parenthesized {
                    return Ok(Callee::Reference(JsValue::Undefined, JsValue::Undefined));
                }
                return Ok(Callee::ShortCircuit);
            }
            let key = self.member_key_after_base(artifact, &property, &base, scope, coverage)?;
            return match self.get_member(&base, &key) {
                Completion::Normal(function) => Ok(Callee::Reference(function, base)),
                abrupt => Err(abrupt),
            };
        }
        if bare.tag() == Tag::ExprIdentifier {
            let mut i = bare.fields();
            let (Ok(_span), Ok(id)) = (i.span(), i.str_id()) else {
                return Err(self.host_error("unreadable call target"));
            };
            let Ok(name) = artifact.str_utf8(id) else {
                return Err(self.host_error("unreadable call target"));
            };
            coverage.walked += 1;
            return match self.lookup_reference(scope, name) {
                Ok((value, base)) => Ok(Callee::Reference(
                    value,
                    base.with_base_object().map_or(JsValue::Undefined, JsValue::Object),
                )),
                Err(abrupt) => Err(abrupt),
            };
        }
        if bare.tag() == Tag::ExprCall {
            return match self.chain_node(artifact, &bare, scope, coverage)? {
                Chain::Value(value) => Ok(Callee::Reference(value, JsValue::Undefined)),
                Chain::ShortCircuit if parenthesized => {
                    Ok(Callee::Reference(JsValue::Undefined, JsValue::Undefined))
                }
                Chain::ShortCircuit => Ok(Callee::ShortCircuit),
            };
        }
        match self.expression_node(artifact, callee, scope, coverage) {
            Completion::Normal(value) => Ok(Callee::Reference(value, JsValue::Undefined)),
            abrupt => Err(abrupt),
        }
    }

    /// The three parts of a `super` property reference, produced in the standard's order.
    ///
    /// # THE KEY EXPRESSION RUNS BETWEEN THE `this` CHECK AND THE BASE READ
    ///
    /// 13.3.7.1 is: `GetThisBinding()`, then **evaluate the key expression**, then
    /// `MakeSuperPropertyReference` -- which is where `GetSuperBase` happens -- and `ToPropertyKey`
    /// later still, inside `GetValue`. Three separate points, and `superPropOrdering.js` pins the
    /// gaps between all of them: `super[ruin()]` must let `ruin` change the prototype BEFORE the
    /// base is read, while a key whose `toString` re-parents the object must NOT, because by then
    /// the base is fixed.
    ///
    /// Bundling the `this` check with the base read gets the first gap wrong; coercing the key
    /// with the base gets the second wrong. They are one function here so the order is written
    /// once rather than at each of the three call sites.
    fn super_reference(
        &mut self,
        artifact: &Artifact<'_>,
        property: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<(JsValue, JsValue, crate::object::PropertyKey), Completion> {
        let (base, receiver, key) = self.super_target(artifact, property, scope, coverage)?;
        let key = self.settle_key(key)?;
        Ok((base, receiver, key))
    }

    /// `MakeSuperPropertyReference`, with the key left uncoerced -- the `super` spelling of
    /// [`Self::member_key_value`], and the same reason.
    ///
    /// THE ORDER OF THE THREE PARTS IS FIXED AND IS NOT THE WRITTEN ORDER: `this` first (so a
    /// derived constructor before `super()` refuses ahead of everything), then the key EXPRESSION,
    /// then `GetSuperBase`. The coercion is a fourth step the caller places.
    fn super_target(
        &mut self,
        artifact: &Artifact<'_>,
        property: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<(JsValue, JsValue, TargetKey), Completion> {
        let receiver = self.super_this()?;
        if property.tag() != Tag::MemberComputed {
            let base = self.super_prototype()?;
            let key = self.member_key_node(artifact, property, scope, coverage)?;
            return Ok((base, receiver, TargetKey::Ready(key)));
        }
        let mut f = property.fields();
        let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
            return Err(self.host_error("unreadable computed key"));
        };
        let value = match self.expression_node(artifact, &child, scope, coverage) {
            Completion::Normal(value) => value,
            abrupt => return Err(abrupt),
        };
        let base = self.super_prototype()?;
        Ok((base, receiver, TargetKey::Pending(value)))
    }

    /// The key EXPRESSION of a member access, evaluated and deliberately NOT coerced.
    ///
    /// # `ToPropertyKey` IS NOT PART OF EVALUATING THE REFERENCE
    ///
    /// A Reference Record's `[[ReferencedName]]` "may be an ECMAScript language value other than a
    /// String or a Symbol until `ToPropertyKey` is performed", and the standard has a NOTE saying
    /// where that matters: **`a[b] = c` does not coerce `b` until after `c` has been evaluated.**
    /// Every other use coerces immediately, which is why doing it here looks right.
    ///
    /// So the expression runs now -- its position in the order is fixed -- and the coercion is a
    /// separate step the caller places. [`Self::member_key_after_base`] puts it where a READ wants
    /// it; the assignment walker puts it after the value.
    fn member_key_value(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<TargetKey, Completion> {
        if node.tag() != Tag::MemberComputed {
            return Ok(TargetKey::Ready(self.member_key_node(artifact, node, scope, coverage)?));
        }
        let mut f = node.fields();
        let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
            return Err(self.host_error("unreadable computed key"));
        };
        match self.expression_node(artifact, &child, scope, coverage) {
            Completion::Normal(value) => Ok(TargetKey::Pending(value)),
            abrupt => Err(abrupt),
        }
    }

    /// `ToPropertyKey`, run at the moment the caller decides.
    fn settle_key(&mut self, key: TargetKey) -> Result<crate::object::PropertyKey, Completion> {
        match key {
            TargetKey::Ready(key) => Ok(key),
            TargetKey::Pending(value) => self.to_property_key(&value),
        }
    }

    /// `ToObject(V.[[Base]])` and then `ToPropertyKey(V.[[ReferencedName]])` -- the order `GetValue`
    /// and `PutValue` share, and it is the order that decides which of two user callbacks runs.
    fn settle_key_after_base(
        &mut self,
        base: &JsValue,
        key: TargetKey,
    ) -> Result<crate::object::PropertyKey, Completion> {
        if matches!(key, TargetKey::Pending(_))
            && matches!(base, JsValue::Undefined | JsValue::Null)
        {
            let what = if matches!(base, JsValue::Null) { "null" } else { "undefined" };
            return Err(self.type_error(&crate::format!("cannot read a property of {what}")));
        }
        self.settle_key(key)
    }

    /// `PutValue` steps 5.a-5.e for a property reference: `ToObject` the base, then `ToPropertyKey`
    /// the name, then `[[Set]]` through the reference's own this-value.
    ///
    /// # ONE OPERATION, AND THREE POSITIONS PERFORM IT
    ///
    /// A member reference is written to by an ASSIGNMENT (`o[k] = v`), by a DESTRUCTURING target
    /// (`[o[k]] = it`, `({p: o[k]} = src)`), and by the pattern walker's own member leaf. All three
    /// must coerce the key HERE rather than when the reference was built, because the standard
    /// defers `ToPropertyKey` into `PutValue` and there is program text in between -- the right-hand
    /// side for an assignment, and **the source read** for a destructuring target. A position that
    /// calls [`Self::member_key_node`] and coerces at once runs `k.toString` before `src.p`'s
    /// getter instead of after it, which is why the rule lives in one function and not in each arm.
    ///
    /// THE ORDER OF THE TWO STEPS IS ITSELF OBSERVABLE. `ToObject` is 5.a and `ToPropertyKey` is
    /// 5.c, so `[null[k]] = it` is a TypeError from the BASE and never runs `k`'s `toString`.
    /// [`Self::write_with_receiver`] refuses a nullish base too, but it refuses it AFTER the key --
    /// the right exception reached through the wrong step, which a throwing `toString` is exactly
    /// the program that tells apart.
    fn put_member(
        &mut self,
        base: &JsValue,
        receiver: JsValue,
        key: TargetKey,
        value: JsValue,
    ) -> Completion {
        if matches!(base, JsValue::Undefined | JsValue::Null) {
            return self.type_error("cannot set a property of null or undefined");
        }
        let key = match self.settle_key(key) {
            Ok(key) => key,
            Err(abrupt) => return abrupt,
        };
        self.write_with_receiver(base, receiver, key, value)
    }

    /// The key a READ wants -- the expression, then the base check, then the coercion -- which is
    /// the standard's THREE-STEP ORDER intact.
    ///
    /// # `ToObject(base)` COMES BEFORE `ToPropertyKey(name)`, AND BOTH CAN RUN A PROGRAM
    ///
    /// `GetValue` on a property reference is `ToObject(base)` and then `ToPropertyKey(name)`, in
    /// that order -- so `null[{ toString() { throw } }]` is a TypeError about the null base and the
    /// `toString` is never called. Coercing the key first, which this engine did, runs user code the
    /// standard says is unreachable and reports whatever that code threw. 44 conformance runs in
    /// `compound-assignment` alone turn on it, each with a `toString` that throws a marker.
    ///
    /// **The key EXPRESSION is still evaluated first**, and that is the other half rather than a
    /// contradiction: `null[f()]` where `f` throws reports `f`'s error, because evaluating the
    /// expression belongs to building the reference and coercing its value belongs to using it.
    /// Both directions are tested, in the same file, one assertion apart.
    fn member_key_after_base(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        base: &JsValue,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<crate::object::PropertyKey, Completion> {
        let key = self.member_key_value(artifact, node, scope, coverage)?;
        self.settle_key_after_base(base, key)
    }

    /// The key an object-literal property names.
    ///
    /// A COMPUTED key is walked, because its expression is ordinary program text that can call
    /// anything. The other forms are decoded -- they are single small nodes, and `property_key`
    /// already carries the rule that a NUMERIC key becomes its STRING form, so `{1: "a"}` and
    /// `{"1": "a"}` are the same property. Reimplementing that here would be a second place for it
    /// to be true.
    fn property_key_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<crate::object::PropertyKey, Completion> {
        if node.tag() == Tag::KeyComputed {
            let mut f = node.fields();
            let (Ok(_span), Ok(child)) = (f.span(), f.child()) else {
                return Err(self.host_error("unreadable computed key"));
            };
            return match self.expression_node(artifact, &child, scope, coverage) {
                Completion::Normal(value) => self.to_property_key(&value),
                abrupt => Err(abrupt),
            };
        }
        let Ok(key) = decode::property_key(artifact, node) else {
            return Err(self.host_error("undecodable property key"));
        };
        self.property_key(&key)
    }

    /// The argument values of a call, in order.
    ///
    /// A SPREAD ITERATES, and iterating is observable -- it calls `Symbol.iterator`, and it can
    /// throw or have effects. It goes through the same `iterate_to_list` the AST path uses rather
    /// than a second implementation of the protocol.
    fn argument_list_node(
        &mut self,
        artifact: &Artifact<'_>,
        fields: &mut lamella_js_bytecode::Fields<'_>,
        scope: &Scope,
        coverage: &mut Coverage,
    ) -> Result<Vec<JsValue>, Completion> {
        let Ok(count) = fields.count() else {
            return Err(self.host_error("unreadable argument count"));
        };
        let mut values = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let Ok(argument) = fields.child() else {
                return Err(self.host_error("unreadable argument"));
            };
            let mut a = argument.fields();
            match argument.tag() {
                Tag::ArgExpression => {
                    let Ok(inner) = a.child() else {
                        return Err(self.host_error("unreadable argument expression"));
                    };
                    match self.expression_node(artifact, &inner, scope, coverage) {
                        Completion::Normal(value) => values.push(value),
                        abrupt => return Err(abrupt),
                    }
                }
                Tag::ArgSpread => {
                    let (Ok(_span), Ok(inner)) = (a.span(), a.child()) else {
                        return Err(self.host_error("unreadable spread argument"));
                    };
                    let source = match self.expression_node(artifact, &inner, scope, coverage) {
                        Completion::Normal(source) => source,
                        abrupt => return Err(abrupt),
                    };
                    values.extend(crate::iterator::iterate_to_list(self, &source)?);
                }
                _ => return Err(self.host_error("not an argument")),
            }
        }
        Ok(values)
    }

    /// Runs a function body that lives in the artifact, from the offset the closure recorded.
    ///
    /// The program is reached by cloning the `Rc` -- a refcount bump -- so the bytes are held by
    /// a local while `&mut self` walks them. Opening from `self.programs` directly would borrow the
    /// interpreter immutably for the whole walk.
    ///
    /// IT IS THE CLOSURE'S OWN PROGRAM, NOT THE CURRENT ONE. A function called from a later
    /// program must walk the bytes it was compiled from, and `current_program` is saved across the
    /// call so a closure BUILT inside this body records that program rather than the caller's.
    ///
    /// THE PARAMETERS ARE BOUND HERE RATHER THAN BY THE CALLER, because they live in the same
    /// node the body does and the artifact is open exactly once. Binding them in `call_body` would
    /// mean opening the program twice per call to read two adjacent field runs.
    pub(crate) fn call_body_from_artifact(
        &mut self,
        code: super::CodeRef,
        arguments: &[JsValue],
        scope: &Scope,
        phase: super::BodyPhase,
    ) -> Completion {
        let Some(program) = self.programs.get(code.program as usize).cloned() else {
            return self.host_error("a closure names a program this interpreter has not loaded");
        };
        let outer_program = core::mem::replace(&mut self.current_program, code.program);
        let artifact = match Artifact::open(&program) {
            Ok(artifact) => artifact,
            Err(error) => {
                self.current_program = outer_program;
                return self.host_error(&crate::format!("unreadable artifact: {error}"));
            }
        };
        let Ok(node) = artifact.node_at(code.at as usize) else {
            self.current_program = outer_program;
            return self.host_error("a closure's recorded offset is not a node");
        };
        let mut coverage = Coverage::default();
        let mut f = node.fields();
        let completion = 'body: {
            match node.tag() {
                Tag::Function => {
                    if f.span().is_err() || skip_optional_str(&mut f).is_err() {
                        break 'body self.host_error("unreadable function header");
                    }
                    let simple = match phase {
                        super::BodyPhase::BodyOnly { simple } => {
                            if skip_run(&mut f).is_err() {
                                break 'body self.host_error("unreadable parameter list");
                            }
                            simple
                        }
                        super::BodyPhase::Whole | super::BodyPhase::ParametersOnly => {
                            match self
                                .bind_params_node(&artifact, &mut f, arguments, scope, &mut coverage)
                            {
                                Ok(simple) => simple,
                                Err(abrupt) => break 'body abrupt,
                            }
                        }
                    };
                    if phase == super::BodyPhase::ParametersOnly {
                        self.pending_params_simple = Some(simple);
                        break 'body Completion::Normal(JsValue::Undefined);
                    }
                    let Ok(body) = collect_body(&mut f) else {
                        break 'body self.host_error("unreadable function body");
                    };
                    let body_scope = match self.pending_generator_body_scope.clone() {
                        Some(existing) => existing,
                        None => body_environment(scope, simple),
                    };
                    self.hoist_vars_nodes(&artifact, &body, &body_scope, simple, scope);
                    self.hoist_nodes(&artifact, &body, &body_scope);
                    let completion =
                        self.statement_nodes(&artifact, &body, &body_scope, &mut coverage);
                    finish_body(completion)
                }
                Tag::Arrow => {
                    if f.span().is_err() {
                        break 'body self.host_error("unreadable arrow header");
                    }
                    let simple = match self
                        .bind_params_node(&artifact, &mut f, arguments, scope, &mut coverage)
                    {
                        Ok(simple) => simple,
                        Err(abrupt) => break 'body abrupt,
                    };
                    let Ok(body) = f.child() else {
                        break 'body self.host_error("unreadable arrow body");
                    };
                    let mut b = body.fields();
                    match body.tag() {
                        Tag::ArrowBodyExpression => {
                            let Ok(inner) = b.child() else {
                                break 'body self.host_error("unreadable arrow expression body");
                            };
                            self.expression_node(&artifact, &inner, scope, &mut coverage)
                        }
                        Tag::ArrowBodyBlock => {
                            let Ok(statements) = collect_body(&mut b) else {
                                break 'body self.host_error("unreadable arrow block body");
                            };
                            let body_scope = body_environment(scope, simple);
                            self.hoist_vars_nodes(
                                &artifact,
                                &statements,
                                &body_scope,
                                simple,
                                scope,
                            );
                            self.hoist_nodes(&artifact, &statements, &body_scope);
                            let completion = self.statement_nodes(
                                &artifact,
                                &statements,
                                &body_scope,
                                &mut coverage,
                            );
                            finish_body(completion)
                        }
                        _ => self.host_error("not an arrow body"),
                    }
                }
                _ => self.host_error("a closure's offset is not a function node"),
            }
        };
        self.current_program = outer_program;
        self.nested_walked += coverage.walked;
        self.nested_refused += coverage.refused;
        for (index, count) in coverage.by_tag.iter().enumerate() {
            self.nested_by_tag[index] += count;
        }
        completion
    }

    /// Builds a closure whose PARAMETERS AND BODY both stay in the artifact.
    ///
    /// **THE CLOSURE OWNS NEITHER HALF OF THE FUNCTION.** It records the offset of its own
    /// function node and nothing else; the parameter list is re-read at every call from the same
    /// node the body is. Owning a decoded parameter list was the last per-closure allocation, and
    /// it was also what forced the AST evaluator to stay alive -- a parameter default is an
    /// arbitrary expression, so a decoded parameter had to be bound by the decoded path.
    ///
    /// Only `length` is computed here, because it is asked for by `f.length` rather than by a
    /// call and would otherwise need the node re-opened on a property read.
    pub(crate) fn make_closure_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
    ) -> Result<JsValue, Completion> {
        self.make_closure_node_as(artifact, node, scope, false)
    }

    /// The same, for a **method definition**: `{ m() {} }`, a class member, a getter, a setter.
    ///
    /// THE ROLE HAS TO BE KNOWN BEFORE THE OBJECT EXISTS. A non-constructor never gets a
    /// `prototype` property, and the one an ordinary function gets is NON-CONFIGURABLE -- so
    /// building the function first and removing the property afterwards is refused, silently, and
    /// the method still reports `'prototype' in m === true`.
    pub(crate) fn make_method_closure_node(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
    ) -> Result<JsValue, Completion> {
        self.make_closure_node_as(artifact, node, scope, true)
    }

    fn make_closure_node_as(
        &mut self,
        artifact: &Artifact<'_>,
        node: &Node<'_>,
        scope: &Scope,
        is_method: bool,
    ) -> Result<JsValue, Completion> {
        let mut f = node.fields();
        if f.span().is_err() {
            return Err(self.host_error("unreadable function header"));
        }
        let Ok(name) = read_optional_str(artifact, &mut f) else {
            return Err(self.host_error("unreadable function name"));
        };
        let Ok((length, flags)) = function_header_node(node) else {
            return Err(self.host_error("unreadable function header"));
        };
        Ok(self.push_artifact_closure(
            length,
            name.as_deref().unwrap_or(""),
            node.offset() as u32,
            flags & lamella_js_bytecode::format::FN_STRICT != 0,
            false,
            is_method,
            flags & lamella_js_bytecode::format::FN_GENERATOR != 0,
            flags & lamella_js_bytecode::format::FN_NO_ARGUMENTS == 0,
            scope,
        ))
    }


    /// A tag with no arm. **It is a refusal rather than a delegation.**
    ///
    /// A delegating fallback -- decode the unhandled tag and hand it to a tree evaluator, correct
    /// by construction because the round-trip suite proves a decoded subtree EQUALS the one the
    /// parser built -- is what makes a large port safe to do in pieces. Every tag has an arm, so
    /// nothing reaches here, and keeping the delegation would keep a second evaluator alive to
    /// serve a path that never runs.
    ///
    /// The COUNTER stays, and that is deliberate: `refused`/`by_tag` are what the completeness
    /// test reads, and the signal is unchanged. A tag with no arm still increments them; it simply
    /// produces a host error instead of a quiet correct answer. **Measuring the walk by refusal is
    /// the stricter of the two.**
    fn refuse_statement(&mut self, node: &Node<'_>, coverage: &mut Coverage) -> Completion {
        coverage.refused += 1;
        coverage.by_tag[node.tag() as u8 as usize] += 1;
        self.host_error(&crate::format!("no arm for statement tag {}", node.tag() as u8))
    }

    fn refuse_expression(&mut self, node: &Node<'_>, coverage: &mut Coverage) -> Completion {
        coverage.refused += 1;
        coverage.by_tag[node.tag() as u8 as usize] += 1;
        self.host_error(&crate::format!("no arm for expression tag {}", node.tag() as u8))
    }


    /// Creates the lexical bindings a body introduces, before any of it runs.
    ///
    /// This walk is SHALLOW -- `hoist` only ever looks at a body's own statements -- so decoding
    /// the handful it cares about costs a bounded amount and no recursion. The deep walk is
    /// `hoist_vars_nodes`, and that one is done over nodes for exactly that reason.
    fn hoist_nodes(&mut self, artifact: &Artifact<'_>, body: &[Node<'_>], scope: &Scope) {
        for node in body {
            let node = &peel_labels(node);
            match node.tag() {
                Tag::StmtFunction => {
                    let mut f = node.fields();
                    let Ok(function) = f.child() else { continue };
                    let mut g = function.fields();
                    if g.span().is_err() {
                        continue;
                    }
                    let Ok(name) = read_optional_str(artifact, &mut g) else { continue };
                    let Ok(value) = self.make_closure_node(artifact, &function, scope) else {
                        continue;
                    };
                    if let Some(name) = name {
                        scope.borrow_mut().bindings.insert(
                            name.clone(),
                            Binding::var(value.clone()),
                        );
                        if self.is_global_scope(scope) {
                            self.create_global_function_binding(&name, value);
                        }
                    }
                }
                Tag::StmtDeclaration => self.hoist_declaration_node(artifact, node, scope),
                Tag::StmtClass => {
                    let Ok(class) = node.fields().child() else { continue };
                    let mut c = class.fields();
                    let (Ok(_span), Ok(Some(name))) = (c.span(), read_optional_str(artifact, &mut c))
                    else {
                        continue;
                    };
                    scope.borrow_mut().bindings.insert(
                        name,
                        Binding {
                            value: JsValue::Undefined,
                            mutability: Mutability::Mutable,
                            initialized: false,
                            lexical: true,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// Creates every `var` binding a function body introduces, wherever it is nested.
    ///
    /// A `var` CROSSES BLOCKS AND STOPS AT FUNCTIONS -- `{ var y = 1; } y;` reads 1. Done over
    /// NODES rather than by decoding, because this walk is the deep one: decoding for it would
    /// materialise the whole tree and hand back exactly the RAM the tier exists to delete.
    fn hoist_vars_nodes(
        &mut self,
        artifact: &Artifact<'_>,
        body: &[Node<'_>],
        scope: &Scope,
        simple: bool,
        parameters: &Scope,
    ) {
        let mut names = Vec::new();
        for node in body {
            collect_var_names_node(artifact, node, &mut names);
        }
        for name in names {
            let seed = if simple {
                JsValue::Undefined
            } else {
                let borrowed = parameters.borrow();
                borrowed
                    .bindings
                    .get(&name)
                    .map(|binding| binding.value.clone())
                    .unwrap_or(JsValue::Undefined)
            };
            scope.borrow_mut().bindings.entry(name.clone()).or_insert(Binding::var(seed));
            if self.is_global_scope(scope) {
                self.create_global_var_binding(&name);
            }
        }
    }
}

/// The environment a function BODY declares into, which is the parameter scope only when the
/// parameter list is simple.
///
/// # A CLOSURE MADE IN A PARAMETER DEFAULT MUST NOT SEE THE BODY'S `var`s
///
/// `(function(_ = probe = function () { return x; }) { var x = 'inside'; }())` with an outer
/// `var x = 'outside'` -- `probe()` is **`'outside'`**. The default runs before the body declares
/// anything, and the standard keeps them apart with a second environment rather than by ordering:
/// step 27 of FunctionDeclarationInstantiation makes one exactly when the parameter list contains
/// an expression. One environment for both gives the closure the body's binding, which is a scope
/// the program never wrote.
///
/// THE CONDITION HERE IS "NOT SIMPLE" WHERE THE STANDARD'S IS "CONTAINS AN EXPRESSION", and the
/// two differ only for a list that is non-simple with nothing evaluable in it -- `(a, ...rest)`,
/// `({x})`. Those have no closures in the parameter list to see anything, and a `var` sharing a
/// parameter's name is seeded from it either way, so the extra environment is not observable. It
/// is stated rather than left to be rediscovered.
pub(crate) fn body_environment(parameters: &Scope, simple: bool) -> Scope {
    if simple {
        Rc::clone(parameters)
    } else {
        new_scope(Some(Rc::clone(parameters)))
    }
}

/// The node-walking twin of `completes_empty`: statements that DECLARE or do nothing, rather than
/// ones that evaluate to nothing.
fn completes_empty_node(node: &Node<'_>) -> bool {
    matches!(
        node.tag(),
        Tag::StmtDeclaration | Tag::StmtFunction | Tag::StmtEmpty | Tag::StmtDebugger
    )
}

/// One class member's header, read without decoding its function.
///
/// The FUNCTION stays a `Node`. That is the point of reading a header separately: the member list
/// is scanned twice -- once to find `constructor`, once to install everything else -- and decoding
/// every method's body on the first pass to answer a question about its NAME would rebuild most of
/// the tree this port exists to stop building.
struct ClassMemberHeader<'a> {
    key: Node<'a>,
    kind: crate::ast::MethodKind,
    is_static: bool,
    computed: bool,
    function: Node<'a>,
    /// Read from the member's own function node, because `MethodKind` does not carry it: a
    /// generator method is `Normal` in kind and differs only in the flag on its function.
    is_generator: bool,
}

fn class_member_header<'a>(
    _artifact: &Artifact<'a>,
    member: &Node<'a>,
) -> Result<ClassMemberHeader<'a>, ()> {
    if member.tag() != Tag::ClassMember {
        return Err(());
    }
    let mut f = member.fields();
    f.span().map_err(|_| ())?;
    let key = f.child().map_err(|_| ())?;
    let kind = decode::method_kind(f.byte().map_err(|_| ())?, member.offset()).map_err(|_| ())?;
    let is_static = f.bool().map_err(|_| ())?;
    let computed = f.bool().map_err(|_| ())?;
    let function = f.child().map_err(|_| ())?;
    let is_generator = function_header_node(&function)
        .map(|(_, flags)| flags & lamella_js_bytecode::format::FN_GENERATOR != 0)
        .unwrap_or(false);
    Ok(ClassMemberHeader { key, kind, is_static, computed, function, is_generator })
}

/// Whether a class member's `PropName` is `name`, over the two shapes it can be WRITTEN in.
/// A COMPUTED key is not one of them.
///
/// A STRING-LITERAL KEY HAS A `PropName` LIKE ANY OTHER. Accepting only the identifier form makes
/// `class A { "constructor"() { return {}; } }` a class whose `constructor` is an ordinary
/// prototype method and whose instances come from the DEFAULT constructor: `new A()` answers an
/// instance of `A` where the standard answers the `{}` the body returns.
///
/// A COMPUTED key's `PropName` is `empty` even when it evaluates to the same text, so
/// `class A { ["constructor"]() {} }` is an ordinary method.
///
/// **THIS IS THE ARTIFACT-SIDE TWIN OF `early_errors::class_member_key_is`**, which reads the same
/// rule off the AST, and the two must agree. The early-error side is the one that decides
/// `class A { "constructor"(){} constructor(){} }` has two constructors, so a disagreement REFUSES
/// a program for having two of something the evaluator can only find one of.
/// `name` MUST BE ASCII, which every name this answers about is: it is compared against a
/// string key's UTF-16 units one at a time.
fn class_member_key_is(artifact: &Artifact<'_>, key: &Node<'_>, name: &str) -> bool {
    let mut f = key.fields();
    if f.span().is_err() {
        return false;
    }
    let Ok(id) = f.str_id() else { return false };
    match key.tag() {
        Tag::KeyIdentifier => artifact.str_utf8(id).is_ok_and(|written| written == name),
        Tag::KeyString => artifact
            .str_utf16(id)
            .is_ok_and(|units| units.eq(name.chars().map(|character| character as u16))),
        _ => false,
    }
}

/// A function's declared `length` and its flags byte, read without decoding anything.
///
/// **`length` IS NOT THE PARAMETER COUNT.** `function f(a, b = 1, c) {}` has length 1, not 3, and
/// Test262 checks the length of essentially every function it is given. Counting all the parameters
/// is the obvious implementation and is wrong for every function that has a default.
///
/// Takes a `Function` **or** an `Arrow`, which differ in two fields: a function may carry a name,
/// and its body is a RUN of statements where an arrow's is a single node. Everything else is
/// identical, so the rules live here once rather than at each call site.
fn function_header_node(function: &Node<'_>) -> Result<(u32, u8), ()> {
    let mut f = function.fields();
    f.span().map_err(|_| ())?;
    let is_function = function.tag() == Tag::Function;
    match function.tag() {
        Tag::Function => skip_optional_str(&mut f).map_err(|_| ())?,
        Tag::Arrow => {}
        _ => return Err(()),
    }
    let count = f.count().map_err(|_| ())?;
    let mut length = 0u32;
    let mut counting = true;
    for _ in 0..count {
        let param = f.child().map_err(|_| ())?;
        if counting && matches!(param.tag(), Tag::PatDefault | Tag::PatRest) {
            counting = false;
        }
        if counting {
            length += 1;
        }
    }
    if is_function {
        skip_run(&mut f).map_err(|_| ())?;
    } else {
        f.skip_child().map_err(|_| ())?;
    }
    let flags = f.byte().map_err(|_| ())?;
    Ok((length, flags))
}

/// Reads an optional string field: a presence byte, then an id.
fn read_optional_str(
    artifact: &Artifact<'_>,
    fields: &mut lamella_js_bytecode::Fields<'_>,
) -> Result<Option<String>, lamella_js_bytecode::FormatError> {
    if !fields.bool()? {
        return Ok(None);
    }
    let id = fields.str_id()?;
    Ok(Some(String::from(artifact.str_utf8(id)?)))
}

/// A function BLOCK body's completion: falling off the end yields `undefined`, and only `return`
/// produces a value.
fn finish_body(completion: Completion) -> Completion {
    match completion {
        Completion::Normal(_) => Completion::Normal(JsValue::Undefined),
        Completion::Return(value) => Completion::Normal(value),
        abrupt => abrupt,
    }
}

/// Steps over an optional string field, which the format writes as a presence byte then an id.
fn skip_optional_str(
    fields: &mut lamella_js_bytecode::Fields<'_>,
) -> Result<(), lamella_js_bytecode::FormatError> {
    if fields.bool()? {
        fields.str_id()?;
    }
    Ok(())
}

/// Steps over a `count`-prefixed run of children without looking inside them. **O(1) per child.**
fn skip_run(fields: &mut lamella_js_bytecode::Fields<'_>) -> Result<(), lamella_js_bytecode::FormatError> {
    let count = fields.count()?;
    for _ in 0..count {
        fields.skip_child()?;
    }
    Ok(())
}

/// The name a bare-identifier binding target introduces, or `None` for anything composite.
///
/// It is deliberately narrow: only a plain identifier is a NAMING context. `var [f] = [fn]`
/// binds `f` too, but the function was not written in the naming position and must stay anonymous.
fn read_identifier_name<'a>(artifact: &Artifact<'a>, target: &Node<'a>) -> Option<&'a str> {
    if target.tag() != Tag::PatIdentifier {
        return None;
    }
    let mut f = target.fields();
    f.span().ok()?;
    artifact.str_utf8(f.str_id().ok()?).ok()
}

/// `IsAnonymousFunctionDefinition`: was this expression WRITTEN as a function with no name?
///
/// A PARENTHESIZED form still counts -- `(function () {})` and `(() => {})` are function
/// definitions through the cover grammar, and the conformance tests exercise that case by name.
/// Stopping at the parenthesis would silently drop a quarter of them.
fn is_anonymous_function_definition(artifact: &Artifact<'_>, node: &Node<'_>) -> bool {
    match node.tag() {
        Tag::ExprArrow => true,
        Tag::ExprParenthesized => {
            let mut f = node.fields();
            match (f.span(), f.child()) {
                (Ok(_), Ok(inner)) => is_anonymous_function_definition(artifact, &inner),
                _ => false,
            }
        }
        Tag::ExprFunction | Tag::ExprClass => {
            let mut f = node.fields();
            let Ok(inner) = f.child() else { return false };
            let mut i = inner.fields();
            if i.span().is_err() {
                return false;
            }
            matches!(i.bool(), Ok(false))
        }
        _ => false,
    }
}

/// A template piece's COOKED text, or `None` where an invalid escape left it without one.
///
/// The RAW text is deliberately not read here. Only a TAGGED template can see it, and that path
/// goes through `template_object`, which caches per call site -- so an untagged template never pays
/// to read a string no program can observe.
fn read_template_cooked(
    artifact: &Artifact<'_>,
    node: &Node<'_>,
) -> Result<Option<crate::string_value::JsString>, ()> {
    if node.tag() != Tag::TemplateElement {
        return Err(());
    }
    let mut f = node.fields();
    f.span().map_err(|_| ())?;
    if !f.bool().map_err(|_| ())? {
        return Ok(None);
    }
    let id = f.str_id().map_err(|_| ())?;
    let units: Vec<u16> = artifact.str_utf16(id).map_err(|_| ())?.collect();
    Ok(Some(crate::string_value::JsString::from_units(&units)))
}

/// Peels `( ... )` off a node until something else is underneath.
///
/// IT LOOPS RATHER THAN UNWRAPPING ONCE: `((o.m))()` is two layers and still a method call, and a
/// single unwrap is defeated by the second pair of parentheses -- the same shape as
/// `is_labelled_function` looking through any number of labels.
///
/// Unreadable nodes stop the walk rather than being reported: the caller is about to evaluate the
/// node it gets back, and that evaluation reports the damage with the position it happened at.
fn strip_parenthesized<'a>(node: &Node<'a>, coverage: &mut Coverage) -> Node<'a> {
    let mut current = *node;
    while current.tag() == Tag::ExprParenthesized {
        let mut f = current.fields();
        let (Ok(_span), Ok(inner)) = (f.span(), f.child()) else { return current };
        coverage.walked += 1;
        current = inner;
    }
    current
}

/// The five tags that carry a label into themselves rather than having one wrapped around them.
///
/// Written beside `loop_statement_node`'s match and NOT derived from it, because the two can
/// disagree in only one direction that matters: a tag listed here but missing there reaches the
/// driver's catch-all, which is a defect that names itself. A tag missing HERE would silently take
/// the wrapped path and lose `continue`, which is the quiet half of the bug the AST evaluator had.
fn is_loop_tag(tag: Tag) -> bool {
    matches!(
        tag,
        Tag::StmtWhile | Tag::StmtDoWhile | Tag::StmtFor | Tag::StmtForIn | Tag::StmtForOf
    )
}

fn collect_body<'a>(
    fields: &mut lamella_js_bytecode::Fields<'a>,
) -> Result<Vec<Node<'a>>, lamella_js_bytecode::FormatError> {
    let count = fields.count()? as usize;
    let mut body = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        body.push(fields.child()?);
    }
    Ok(body)
}

/// A statement with every enclosing label stripped off it.
///
/// # EVERY DECLARATION-COLLECTING RULE IN THE TEXT LOOKS THROUGH A LABEL, AND THIS ONE DID NOT
///
/// `LabelledItem : FunctionDeclaration` is in the sloppy grammar, and `IsLabelledFunction` looks
/// through ANY number of labels precisely because `a: b: c: function f() {}` is one declaration
/// wearing three. The shallow hoist matched on the tag it saw, so a labelled function declaration
/// hoisted nothing and its own name was a ReferenceError on the next line -- the parser accepted the
/// program and the hoister could not see it.
///
/// It is deliberately not recursive on anything but a label: peeling further would reach into
/// blocks and loop bodies, which is the DEEP walk's job and would hoist a nested declaration into
/// the wrong scope.
fn peel_labels<'a>(node: &Node<'a>) -> Node<'a> {
    let mut current = node.clone();
    while current.tag() == Tag::StmtLabeled {
        let mut f = current.fields();
        let (Ok(_span), Ok(_label), Ok(body)) = (f.span(), f.str_id(), f.child()) else {
            return current;
        };
        current = body;
    }
    current
}

/// `LexicallyDeclaredNames`, `VarDeclaredNames` and the top-level function names of a Script, which
/// is exactly what `GlobalDeclarationInstantiation` asks about before it creates anything.
///
/// THE THREE LISTS ARE THE STANDARD'S AND THEIR DIFFERENCES MATTER. A `var` is collected from
/// ANY DEPTH short of a nested function, because that is where a `var` hoists to; a `let`, a
/// `const` and a `class` are collected only from the TOP LEVEL, because one inside a block belongs
/// to that block and cannot collide with a global. A top-level function is BOTH -- it is a var-scoped
/// declaration and it gets the stricter `CanDeclareGlobalFunction` test -- which is why it is
/// returned separately rather than folded into either list.
///
/// Through any number of labels, exactly as `hoist_nodes` is: `lbl: function f() {}` declares `f`.
fn top_level_declared_names(
    artifact: &Artifact<'_>,
    body: &[Node<'_>],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut lexical = Vec::new();
    let mut vars = Vec::new();
    let mut functions = Vec::new();
    for node in body {
        collect_var_names_node(artifact, node, &mut vars);
        let node = peel_labels(node);
        match node.tag() {
            Tag::StmtFunction => {
                let mut f = node.fields();
                let Ok(function) = f.child() else { continue };
                let mut g = function.fields();
                if g.span().is_err() {
                    continue;
                }
                if let Ok(Some(name)) = read_optional_str(artifact, &mut g) {
                    functions.push(name);
                }
            }
            Tag::StmtClass => {
                let Ok(class) = node.fields().child() else { continue };
                let mut c = class.fields();
                if c.span().is_err() {
                    continue;
                }
                if let Ok(Some(name)) = read_optional_str(artifact, &mut c) {
                    lexical.push(name);
                }
            }
            Tag::StmtDeclaration => {
                let mut f = node.fields();
                let (Ok(_span), Ok(kind), Ok(count)) = (f.span(), f.byte(), f.count()) else {
                    continue;
                };
                if kind == 0 {
                    continue;
                }
                for _ in 0..count {
                    let Ok(declarator) = f.child() else { break };
                    let mut d = declarator.fields();
                    let (Ok(_span), Ok(target)) = (d.span(), d.child()) else { break };
                    pattern_names_node(artifact, &target, &mut lexical);
                }
            }
            _ => {}
        }
    }
    (lexical, vars, functions)
}

/// The node-walking twin of `collect_var_names`, arm for arm.
///
/// A nested FUNCTION is a boundary: its `var`s are its own, so there is deliberately no arm for
/// one. That silence is the rule, not an omission -- the AST version says the same thing with the
/// same `_ => {}`.
fn collect_var_names_node(artifact: &Artifact<'_>, node: &Node<'_>, out: &mut Vec<String>) {
    let mut f = node.fields();
    match node.tag() {
        Tag::StmtDeclaration => {
            let (Ok(_span), Ok(kind)) = (f.span(), f.byte()) else { return };
            if kind != 0 {
                return;
            }
            declarator_names(artifact, &mut f, out);
        }
        Tag::StmtBlock => {
            if f.span().is_err() {
                return;
            }
            for_each_child(&mut f, |child| collect_var_names_node(artifact, child, out));
        }
        Tag::StmtIf => {
            let (Ok(_span), Ok(_test), Ok(consequent)) = (f.span(), f.child(), f.child()) else {
                return;
            };
            collect_var_names_node(artifact, &consequent, out);
            if let Ok(Some(alternate)) = f.option_child() {
                collect_var_names_node(artifact, &alternate, out);
            }
        }
        Tag::StmtWhile => {
            let (Ok(_span), Ok(_test), Ok(body)) = (f.span(), f.child(), f.child()) else { return };
            collect_var_names_node(artifact, &body, out);
        }
        Tag::StmtDoWhile => {
            let (Ok(_span), Ok(body)) = (f.span(), f.child()) else { return };
            collect_var_names_node(artifact, &body, out);
        }
        Tag::StmtLabeled => {
            let (Ok(_span), Ok(_label), Ok(body)) = (f.span(), f.str_id(), f.child()) else {
                return;
            };
            collect_var_names_node(artifact, &body, out);
        }
        Tag::StmtWith => {
            let (Ok(_span), Ok(_object), Ok(body)) = (f.span(), f.child(), f.child()) else {
                return;
            };
            collect_var_names_node(artifact, &body, out);
        }
        Tag::StmtFor => {
            if f.span().is_err() {
                return;
            }
            if let Ok(Some(init)) = f.option_child() {
                for_init_var_names(artifact, &init, out);
            }
            let (Ok(_test), Ok(_update)) = (f.option_child(), f.option_child()) else { return };
            if let Ok(body) = f.child() {
                collect_var_names_node(artifact, &body, out);
            }
        }
        Tag::StmtForIn | Tag::StmtForOf => {
            let (Ok(_span), Ok(left), Ok(_right), Ok(body)) =
                (f.span(), f.child(), f.child(), f.child())
            else {
                return;
            };
            for_init_var_names(artifact, &left, out);
            collect_var_names_node(artifact, &body, out);
        }
        Tag::StmtTry => {
            if f.span().is_err() {
                return;
            }
            for_each_child(&mut f, |child| collect_var_names_node(artifact, child, out));
            if let Ok(Some(handler)) = f.option_child() {
                let mut h = handler.fields();
                if h.span().is_ok() {
                    let _ = h.option_child();
                    for_each_child(&mut h, |child| collect_var_names_node(artifact, child, out));
                }
            }
            if let Ok(true) = f.bool() {
                for_each_child(&mut f, |child| collect_var_names_node(artifact, child, out));
            }
        }
        Tag::StmtSwitch => {
            let (Ok(_span), Ok(_discriminant)) = (f.span(), f.child()) else { return };
            for_each_child(&mut f, |case| {
                let mut c = case.fields();
                if c.span().is_err() {
                    return;
                }
                let _ = c.option_child();
                for_each_child(&mut c, |child| collect_var_names_node(artifact, child, out));
            });
        }
        _ => {}
    }
}

/// The `var` names a `for` header's init clause introduces, if it is one.
fn for_init_var_names(artifact: &Artifact<'_>, node: &Node<'_>, out: &mut Vec<String>) {
    if node.tag() != Tag::ForInitDeclaration {
        return;
    }
    let mut f = node.fields();
    let (Ok(_span), Ok(kind)) = (f.span(), f.byte()) else { return };
    if kind != 0 {
        return;
    }
    declarator_names(artifact, &mut f, out);
}

fn declarator_names(
    artifact: &Artifact<'_>,
    fields: &mut lamella_js_bytecode::Fields<'_>,
    out: &mut Vec<String>,
) {
    for_each_child(fields, |declarator| {
        let mut d = declarator.fields();
        if d.span().is_err() {
            return;
        }
        if let Ok(target) = d.child() {
            pattern_names_node(artifact, &target, out);
        }
    });
}

/// The names a `for` head declares that each iteration needs its own copy of: a `let` head's
/// bound names, and nothing else.
///
/// A `var` head has ONE binding for the whole loop by definition, and a `const` head cannot be
/// updated -- so both give an empty list and the loop keeps a single environment.
fn per_iteration_names(artifact: &Artifact<'_>, init: &Node<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut f = init.fields();
    let (Ok(_span), Ok(kind), Ok(count)) = (f.span(), f.byte(), f.count()) else { return out };
    if kind != 1 {
        return out;
    }
    for _ in 0..count {
        let Ok(declarator) = f.child() else { return out };
        let mut d = declarator.fields();
        let (Ok(_span), Ok(target)) = (d.span(), d.child()) else { return out };
        pattern_names_node(artifact, &target, &mut out);
    }
    out
}

/// Every name a binding pattern introduces.
///
/// **A MEMBER TARGET INTRODUCES NOTHING**, which is not an oversight: `[o.x] = y` writes through
/// an existing reference and declares no name, so hoisting must not create one. Treating it like a
/// leaf would put a binding called nothing-in-particular into the scope of every destructuring
/// assignment.
///
/// A pure read -- no scope, no `&mut self` -- because hoisting must not be able to run a program.
/// A computed key inside a pattern (`var { [f()]: a } = o`) is skipped here for that reason; `f`
/// runs when the pattern is EVALUATED, not when its names are collected.
fn pattern_names_node(artifact: &Artifact<'_>, node: &Node<'_>, out: &mut Vec<String>) {
    let mut f = node.fields();
    match node.tag() {
        Tag::PatIdentifier => {
            if let (Ok(_span), Ok(id)) = (f.span(), f.str_id()) {
                if let Ok(name) = artifact.str_utf8(id) {
                    out.push(String::from(name));
                }
            }
        }
        Tag::PatDefault | Tag::PatRest => {
            if f.span().is_err() {
                return;
            }
            if let Ok(target) = f.child() {
                pattern_names_node(artifact, &target, out);
            }
        }
        Tag::PatArray => {
            if f.span().is_err() {
                return;
            }
            let Ok(count) = f.count() else { return };
            for _ in 0..count {
                match f.option_child() {
                    Ok(None) => {}
                    Ok(Some(element)) => pattern_names_node(artifact, &element, out),
                    Err(_) => return,
                }
            }
            if let Ok(Some(rest)) = f.option_child() {
                pattern_names_node(artifact, &rest, out);
            }
        }
        Tag::PatObject => {
            if f.span().is_err() {
                return;
            }
            let Ok(count) = f.count() else { return };
            for _ in 0..count {
                let Ok(property) = f.child() else { return };
                let mut p = property.fields();
                let (Ok(_span), Ok(_key)) = (p.span(), p.child()) else { return };
                let Ok(target) = p.child() else { return };
                pattern_names_node(artifact, &target, out);
            }
            if let Ok(Some(rest)) = f.option_child() {
                pattern_names_node(artifact, &rest, out);
            }
        }
        Tag::PatMember => {}
        _ => {}
    }
}

/// Runs `visit` over a `count`-prefixed run of children.
fn for_each_child<'a>(
    fields: &mut lamella_js_bytecode::Fields<'a>,
    mut visit: impl FnMut(&Node<'a>),
) {
    let Ok(count) = fields.count() else { return };
    for _ in 0..count {
        let Ok(child) = fields.child() else { return };
        visit(&child);
    }
}

