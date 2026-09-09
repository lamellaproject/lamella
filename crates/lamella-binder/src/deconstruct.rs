//! DECONSTRUCTION (C# 7.0): `(a, b) = t`, `(int a, int b) = t`, `var (a, b) = t`, and the
//! `foreach (var (a, b) in ...)` that declares its iteration variables the same way.

use alloc::boxed::Box;
use alloc::vec::Vec;

use lamella_syntax::ast::{
    Argument, AssignmentOperator, DeconstructionTarget, Expr, ExprKind, RefPosition, Stmt, StmtKind,
    TypeRef, TypeRefKind, VariableDeclarator,
};
use lamella_syntax::span::Span;

use crate::bound::{error_expr, Binder, BoundExpr, BoundExprKind};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::statement::{BoundDeclarator, BoundStmt, BoundStmtKind};
use crate::special::SpecialType;
use crate::symbols::ParameterMode;
use crate::types::TypeSymbol;

/// A synthesized local's name. `<` and `>` are not identifier characters, so one of these can
/// never collide with a name the source could write -- the same shape the null-conditional
/// access's `<cond>` holder uses, and named after the same thing it holds.
fn holder_name(span: Span, suffix: &str) -> Box<str> {
    alloc::format!("<deconstruct>{}{suffix}", span.start).into()
}

/// A syntax reference to a local by name, for the rewritten statements to read.
fn name_expr(name: &str, span: Span) -> Expr {
    Expr::new(
        ExprKind::Name {
            name: name.into(),
            verbatim: false,
        },
        span,
    )
}

impl Binder {
    /// Binds a DECONSTRUCTION STATEMENT, or `None` when `expr` is not one.
    ///
    /// **STATEMENT POSITION IS ITS OWN ENTRY POINT, THE WAY A NULL-CONDITIONAL ACCESS'S IS**, and
    /// for a related reason: the declaring forms are not expressions at all (`CS8185`), and the
    /// lowering wants to produce STATEMENTS -- a declaration per target -- rather than a value
    /// that nothing reads.
    pub(crate) fn bind_deconstruction_statement(&mut self, expr: &Expr) -> Option<BoundStmtKind> {
        let ExprKind::Deconstruction { targets, value, .. } = &expr.kind else {
            return None;
        };
        let mut statements = Vec::new();
        self.lower_deconstruction(targets, value, expr.span, &mut statements);
        Some(BoundStmtKind::Block(statements))
    }

    /// Binds a deconstruction in VALUE position -- `M((a, b) = t)`, `var q = ((a, b) = t)`.
    ///
    /// **ONLY THE ASSIGNING FORM HAS A VALUE.** A declaration among the targets is `CS8185`,
    /// reported at the `var` of a designation and at the declaring target otherwise, which is
    /// csc's position for each.
    ///
    /// **THE RESULT TYPE IS THE TARGETS' TYPES, NOT THE VALUE'S**: `long a; int b;
    /// ((a, b) = (40, 2))` is `(long, int)`, measured. So the tuple handed back is built from the
    /// targets after they are assigned rather than from the spill.
    pub(crate) fn bind_deconstruction_value(
        &mut self,
        var_span: Option<Span>,
        targets: &[DeconstructionTarget],
        value: &Expr,
        span: Span,
    ) -> BoundExpr {
        if let Some(at) = first_declaration_span(targets, var_span) {
            self.report(Diagnostic::new(DiagnosticKind::DeclarationNotPermitted, at));
            return error_expr();
        }
        self.report(Diagnostic::new(
            DiagnosticKind::FeatureNotInThisBuild {
                feature: "deconstruction as a value".into(),
                permitted_by: self.language_version(),
            },
            span,
        ));
        let mut statements = Vec::new();
        let Some(assigned) = self.lower_deconstruction(targets, value, span, &mut statements)
        else {
            return error_expr();
        };
        let element_tuple = Expr::new(
            ExprKind::Tuple {
                elements: assigned
                    .elements
                    .into_iter()
                    .map(|value| lamella_syntax::ast::TupleElementExpr { name: None, value })
                    .collect(),
            },
            span,
        );
        let target_ty = crate::bind::value_tuple(&assigned.types);
        let result = self.bind_target_typed(&element_tuple, &target_ty).unwrap_or_else(error_expr);
        let mut spilled = Vec::with_capacity(statements.len());
        for statement in statements {
            match statement.kind {
                BoundStmtKind::Expression(expr) => spilled.push(expr),
                BoundStmtKind::Local { ty, declarators } => {
                    for declarator in declarators {
                        let Some(initializer) = declarator.initializer else {
                            continue;
                        };
                        spilled.push(BoundExpr {
                            ty: ty.clone(),
                            kind: BoundExprKind::Assignment {
                                operator: AssignmentOperator::Assign,
                                target: Box::new(BoundExpr {
                                    ty: ty.clone(),
                                    kind: BoundExprKind::Local(declarator.name),
                                }),
                                value: Box::new(initializer),
                                checked: false,
                            },
                        });
                    }
                }
                BoundStmtKind::Block(_) | _ => {}
            }
        }
        BoundExpr {
            ty: result.ty.clone(),
            kind: BoundExprKind::Sequence {
                spilled,
                value: Box::new(result),
            },
        }
    }

    /// Lowers the whole construct into `statements`; `false` when it could not be bound at all.
    ///
    /// **THE RUNG GATE IS NOT HERE.** It is the parser's, raised after the `=` has committed the
    /// reading -- see [`Parser::try_parse_deconstruction`]. It was here first, and it never fired:
    /// a syntax error BLOCKS BINDING, and below C# 7.0 the tuple literal on the right raises one
    /// before this function is reached, so every rung row reported the literal's gate and not the
    /// target list's.
    fn lower_deconstruction(
        &mut self,
        targets: &[DeconstructionTarget],
        value: &Expr,
        span: Span,
        statements: &mut Vec<BoundStmt>,
    ) -> Option<Assigned> {
        let bound = self.bind_expression(value);
        if bound.ty.is_error() {
            self.recover_targets(targets, 0, statements);
            return None;
        }
        let holder = holder_name(span, "");
        let source_ty = bound.ty.clone();
        self.spill(holder.clone(), source_ty.clone(), bound, span, statements);
        self.spread_targets(targets, &holder, &source_ty, value.span, span, statements)
    }

    /// Declares every target a failed deconstruction would have declared, and says why each
    /// implicitly typed one has no type.
    ///
    /// **A FAILURE THAT DECLARES NOTHING TURNS ONE ERROR INTO ONE PER LATER USE.** `var (a, b) =
    /// (1, 2, 3); return a;` is `CS8132` from csc and nothing else; without this it was `CS8132`
    /// plus a `CS0103` blaming `a` for not existing, in a statement that is not at fault -- and
    /// then, once the name existed, a `CS0165` blaming it for being unassigned. Both are the same
    /// mistake: a variable the source DID declare, reported as though the source had not.
    ///
    /// **`CS8130` IS NARROWER THAN "THE DECONSTRUCTION FAILED", AND THE MEASUREMENT IS THE ONLY
    /// WAY TO KNOW WHICH WAY.** `var (a, b) = (1, 2, 3)` is `CS8132` ALONE -- both variables still
    /// take a type from the element at their position -- while `var (a, b, c) = (1, 2)` adds one
    /// `CS8130` for `c` and none for `a` or `b`. So it is reported for an implicitly typed target
    /// at a position the value could not supply, which is `typed_through` onward, and never for
    /// one that wrote its type: `(int a, int b) = (1, 2, 3)` gets `CS8132` alone too.
    ///
    /// `typed_through` is how many leading targets the value did type. `0` when it could not be
    /// taken apart at all, which is the missing-`Deconstruct` case and reports one per variable.
    fn recover_targets(
        &mut self,
        targets: &[DeconstructionTarget],
        typed_through: usize,
        statements: &mut Vec<BoundStmt>,
    ) {
        for (index, target) in targets.iter().enumerate() {
            match target {
                DeconstructionTarget::Declaration { ty, name, span } => {
                    if ty.is_none() && index >= typed_through {
                        self.report(Diagnostic::new(
                            DiagnosticKind::UninferredDeconstructionVariable {
                                name: name.clone(),
                            },
                            *span,
                        ));
                    }
                    let declared = match ty {
                        Some(written) => self.resolve_type_ref(written),
                        None => TypeSymbol::Error,
                    };
                    statements.push(BoundStmt {
                        kind: BoundStmtKind::Local {
                            ty: declared.clone(),
                            declarators: alloc::vec![BoundDeclarator {
                                name: name.clone(),
                                initializer: Some(error_expr()),
                            }],
                        },
                        span: *span,
                    });
                    self.declare_local(name, declared);
                }
                DeconstructionTarget::Nested { targets, .. } => {
                    self.recover_targets(targets, 0, statements);
                }
                DeconstructionTarget::Expression(expr) => {
                    let target = self.bind_expression(expr);
                    if target.ty.is_error() {
                        return;
                    }
                    statements.push(BoundStmt {
                        kind: BoundStmtKind::Expression(BoundExpr {
                            ty: target.ty.clone(),
                            kind: BoundExprKind::Assignment {
                                operator: AssignmentOperator::Assign,
                                target: Box::new(target),
                                value: Box::new(error_expr()),
                                checked: false,
                            },
                        }),
                        span: expr.span,
                    });
                }
                DeconstructionTarget::Discard(_) => {}
            }
        }
    }

    /// Declares a synthesized local holding `value`, and puts it in scope for the rewritten
    /// statements that read it.
    fn spill(
        &mut self,
        name: Box<str>,
        ty: TypeSymbol,
        value: BoundExpr,
        span: Span,
        statements: &mut Vec<BoundStmt>,
    ) {
        let names = self.tuple_names_of(&value);
        statements.push(BoundStmt {
            kind: BoundStmtKind::Local {
                ty: ty.clone(),
                declarators: alloc::vec![BoundDeclarator {
                    name: name.clone(),
                    initializer: Some(value),
                }],
            },
            span,
        });
        self.declare_local(&name, ty);
        self.record_local_tuple_names(&name, names);
    }

    /// Matches `targets` against the spilled `holder`, appending what each one needs.
    fn spread_targets(
        &mut self,
        targets: &[DeconstructionTarget],
        holder: &str,
        source_ty: &TypeSymbol,
        value_span: Span,
        span: Span,
        statements: &mut Vec<BoundStmt>,
    ) -> Option<Assigned> {
        let elements =
            match self.elements_of(holder, source_ty, targets.len(), value_span, span, statements) {
                Some(elements) => elements,
                None => {
                    let typed_through = crate::bound::tuple_arity(source_ty).unwrap_or(0);
                    self.recover_targets(targets, typed_through, statements);
                    return None;
                }
            };
        let mut assigned = Assigned {
            elements: Vec::with_capacity(targets.len()),
            types: Vec::with_capacity(targets.len()),
        };
        for (target, element) in targets.iter().zip(elements) {
            let ty = self.spread_one(target, element.clone(), value_span, span, statements);
            assigned.elements.push(element);
            assigned.types.push(ty);
        }
        Some(assigned)
    }

    /// One target against the syntax that reads the element it matches.
    fn spread_one(
        &mut self,
        target: &DeconstructionTarget,
        element: Expr,
        value_span: Span,
        span: Span,
        statements: &mut Vec<BoundStmt>,
    ) -> TypeSymbol {
        match target {
            DeconstructionTarget::Discard(at) => {
                let _ = at;
                self.speculative_type(&element)
            }
            DeconstructionTarget::Declaration { ty, name, span: at } => {
                let declared = ty.clone().unwrap_or_else(|| inferred_type_ref(*at));
                let statement = Stmt::new(
                    StmtKind::LocalDeclaration {
                        ty: declared,
                        declarators: alloc::vec![VariableDeclarator {
                            name: name.clone(),
                            initializer: Some(element),
                            span: *at,
                        }],
                        is_const: false,
                    },
                    *at,
                );
                statements.push(self.bind_statement(&statement));
                self.lookup_local_type(name)
            }
            DeconstructionTarget::Expression(expr) => {
                if let ExprKind::Name { name, verbatim } = &expr.kind
                    && &**name == "_"
                    && !verbatim
                    && !self.local_is_visible("_")
                {
                    return self.speculative_type(&element);
                }
                let ty = self.speculative_type(expr);
                let statement = Stmt::new(
                    StmtKind::Expression(Expr::new(
                        ExprKind::Assignment {
                            operator: AssignmentOperator::Assign,
                            target: Box::new(expr.clone()),
                            value: Box::new(element),
                        },
                        expr.span,
                    )),
                    expr.span,
                );
                statements.push(self.bind_statement(&statement));
                ty
            }
            DeconstructionTarget::Nested { targets, span: at } => {
                let bound = self.bind_expression(&element);
                if bound.ty.is_error() {
                    self.recover_targets(targets, 0, statements);
                    return TypeSymbol::Error;
                }
                let holder = holder_name(*at, "");
                let ty = bound.ty.clone();
                self.spill(holder.clone(), ty.clone(), bound, *at, statements);
                self.spread_targets(targets, &holder, &ty, value_span, span, statements);
                ty
            }
        }
    }

    /// Syntax reading each of the `count` elements of the spilled `holder`.
    ///
    /// **THE TUPLE PATH IS TRIED FIRST AND WINS**, measured: a type that is a tuple and also has a
    /// `Deconstruct` deconstructs as a tuple.
    fn elements_of(
        &mut self,
        holder: &str,
        source_ty: &TypeSymbol,
        count: usize,
        value_span: Span,
        span: Span,
        statements: &mut Vec<BoundStmt>,
    ) -> Option<Vec<Expr>> {
        if let Some(arity) = crate::bound::tuple_arity(source_ty) {
            if arity != count {
                self.report(Diagnostic::new(
                    DiagnosticKind::DeconstructWrongCardinality {
                        found: arity,
                        wanted: count,
                    },
                    span,
                ));
                return None;
            }
            return Some(
                (0..count)
                    .map(|index| {
                        Expr::new(
                            ExprKind::MemberAccess {
                                receiver: Box::new(name_expr(holder, span)),
                                name: alloc::format!("Item{}", index + 1).into(),
                            },
                            span,
                        )
                    })
                    .collect(),
            );
        }
        self.deconstruct_call_elements(holder, source_ty, count, value_span, statements)
    }

    /// The elements a user `Deconstruct(out ...)` produces: one synthesized local per target,
    /// filled by one call, then read back by name.
    fn deconstruct_call_elements(
        &mut self,
        holder: &str,
        source_ty: &TypeSymbol,
        count: usize,
        value_span: Span,
        statements: &mut Vec<BoundStmt>,
    ) -> Option<Vec<Expr>> {
        let parameters = match self.deconstruct_parameters(source_ty, count) {
            Some(parameters) => parameters,
            None => {
                let parameters = match self.deconstruct_candidate_parameters(source_ty, count) {
                    Some(parameters) => parameters,
                    None if self.methods_named_in_chain(source_ty, "Deconstruct").is_empty() => {
                        alloc::vec![TypeSymbol::Error; count]
                    }
                    None => alloc::vec![TypeSymbol::Special(SpecialType::Int32); count],
                };
                self.emit_deconstruct_call(holder, &parameters, value_span, &mut Vec::new());
                self.report(Diagnostic::new(
                    DiagnosticKind::MissingDeconstruct {
                        type_name: crate::bound::type_display(source_ty),
                        count,
                    },
                    value_span,
                ));
                return None;
            }
        };
        let mut statements_for_call = Vec::new();
        let elements =
            self.emit_deconstruct_call(holder, &parameters, value_span, &mut statements_for_call);
        statements.extend(statements_for_call);
        Some(elements)
    }

    /// Declares one `out` local per parameter, binds `holder.Deconstruct(out ..., out ...)`, and
    /// reports syntax reading each local back.
    fn emit_deconstruct_call(
        &mut self,
        holder: &str,
        parameters: &[TypeSymbol],
        span: Span,
        statements: &mut Vec<BoundStmt>,
    ) -> Vec<Expr> {
        let count = parameters.len();
        let mut arguments = Vec::with_capacity(count);
        let mut elements = Vec::with_capacity(count);
        for (index, parameter) in parameters.iter().enumerate() {
            let ty = match parameter {
                TypeSymbol::ByRef(inner) => (**inner).clone(),
                other => other.clone(),
            };
            let name = holder_name(span, &alloc::format!("${index}"));
            statements.push(BoundStmt {
                kind: BoundStmtKind::Local {
                    ty: ty.clone(),
                    declarators: alloc::vec![BoundDeclarator {
                        name: name.clone(),
                        initializer: None,
                    }],
                },
                span,
            });
            self.declare_local(&name, ty);
            arguments.push(Argument::positional(Expr::new(
                ExprKind::RefArgument {
                    position: RefPosition::Argument,
                    out: true,
                    operand: Box::new(name_expr(&name, span)),
                },
                span,
            )));
            elements.push(name_expr(&name, span));
        }
        let call = Stmt::new(
            StmtKind::Expression(Expr::new(
                ExprKind::Invocation {
                    receiver: Box::new(Expr::new(
                        ExprKind::MemberAccess {
                            receiver: Box::new(name_expr(holder, span)),
                            name: "Deconstruct".into(),
                        },
                        span,
                    )),
                    type_arguments: Vec::new(),
                    arguments,
                },
                span,
            )),
            span,
        );
        statements.push(self.bind_statement(&call));
        elements
    }

    /// The parameter types of the `Deconstruct` a target count selects, or `None` when the type
    /// has none that fits.
    ///
    /// **CHOSEN BY ARITY, NOT BY OVERLOAD RESOLUTION**, because there are no argument types to
    /// resolve over: every argument is an `out` variable whose type the method itself supplies.
    /// csc admits two arities on one type and picks by the number of targets, measured.
    ///
    /// The three refusals are csc's and each was measured: a `static` one does not qualify
    /// (`CS0176` beside `CS8129`), a non-`void` one does not (`CS8129` alone), and a `ref`
    /// parameter where `out` is wanted does not (`CS1620` beside `CS8129`).
    /// The parameter types of ANY `Deconstruct` of this arity, usable or not -- the candidate a
    /// refusal is about. Used only to give the rejected call real argument types so the ordinary
    /// invocation path can say what is wrong with it.
    fn deconstruct_candidate_parameters(
        &mut self,
        ty: &TypeSymbol,
        count: usize,
    ) -> Option<Vec<TypeSymbol>> {
        self.methods_named_in_chain(ty, "Deconstruct")
            .into_iter()
            .find(|method| method.parameters.len() == count)
            .map(|method| method.parameters)
    }

    fn deconstruct_parameters(&mut self, ty: &TypeSymbol, count: usize) -> Option<Vec<TypeSymbol>> {
        let candidates = self.methods_named_in_chain(ty, "Deconstruct");
        candidates
            .into_iter()
            .find(|method| {
                !method.is_static
                    && method.return_type.is_void()
                    && method.parameters.len() == count
                    && method.parameter_info.len() == count
                    && method
                        .parameter_info
                        .iter()
                        .all(|info| info.mode == ParameterMode::Out)
            })
            .map(|method| method.parameters)
    }
}

impl Binder {
    /// Binds `foreach (var (a, b) in e) body` by lowering it to the ordinary `foreach` it means.
    ///
    /// **THE ITERATION VARIABLE IS SYNTHESIZED AND THE TARGETS COME OFF IT INSIDE THE BODY.** So
    /// the loop below the binder is the one that already exists -- one variable, one type, the
    /// enumerator pattern unchanged -- and the deconstruction is the same statement lowering the
    /// standalone form uses. Nothing about `foreach` had to learn what a deconstruction is, and
    /// nothing about deconstruction had to learn what a loop is.
    ///
    /// **THE TARGETS ARE DECLARED PER ITERATION, INSIDE THE BODY'S SCOPE**, which is what the
    /// source says and what a lambda capturing one of them must see: each pass round the loop
    /// declares its own.
    pub(crate) fn bind_foreach_deconstruction(
        &mut self,
        var_span: Option<Span>,
        targets: &[DeconstructionTarget],
        collection: &Expr,
        body: &Stmt,
        span: Span,
    ) -> BoundStmtKind {
        let _ = var_span;
        let iteration = holder_name(span, "$item");
        let lowered = Stmt::new(
            StmtKind::ForEach {
                ty: inferred_type_ref(Span::empty_at(span.start)),
                name: iteration.clone(),
                collection: collection.clone(),
                body: Box::new(Stmt::new(
                    StmtKind::Block(alloc::vec![
                        Stmt::new(
                            StmtKind::Expression(Expr::new(
                                ExprKind::Deconstruction {
                                    var_span,
                                    targets: targets.to_vec(),
                                    value: Box::new(name_expr(&iteration, span)),
                                },
                                span,
                            )),
                            span,
                        ),
                        body.clone(),
                    ]),
                    body.span,
                )),
            },
            span,
        );
        self.bind_statement(&lowered).kind
    }
}

/// What a lowered deconstruction produced: syntax reading each element, and the type each target
/// took. Both are wanted only by the VALUE form, which builds its result tuple from them.
struct Assigned {
    /// Syntax reading each element -- pure, so the value form may name them a second time.
    elements: Vec<Expr>,
    /// The type each target assigned, which is the result tuple's element type.
    types: Vec<TypeSymbol>,
}

/// The span `CS8185` is reported at, or `None` when no target declares anything.
fn first_declaration_span(
    targets: &[DeconstructionTarget],
    var_span: Option<Span>,
) -> Option<Span> {
    if let Some(at) = var_span {
        return Some(at);
    }
    for target in targets {
        match target {
            DeconstructionTarget::Declaration { span, .. } => return Some(*span),
            DeconstructionTarget::Nested { targets, .. } => {
                if let Some(at) = first_declaration_span(targets, None) {
                    return Some(at);
                }
            }
            DeconstructionTarget::Expression(_) | DeconstructionTarget::Discard(_) => {}
        }
    }
    None
}

/// The `var` a target with no written type declares with.
///
/// A leaf under a `var (...)` designation and an explicit `var a` are the same declaration, and
/// both take their type from the element -- which is exactly what an ordinary implicitly typed
/// local does, so the rewritten statement says `var` and the existing inference answers.
fn inferred_type_ref(span: Span) -> TypeRef {
    TypeRef::new(TypeRefKind::Name(alloc::vec!["var".into()]), span)
}
