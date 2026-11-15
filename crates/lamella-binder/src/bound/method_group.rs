//! Method-group applicability, target selection and delegate signature validation.

use super::{
    Better, Binder, BoundExpr, BoundExprKind, CandidateSet, Diagnostic, DiagnosticKind, Judged,
    LanguageVersion, MethodSymbol, Span, TypeKind, TypeSymbol, argument_display, argument_type,
    best_candidate, better_conversion, map_arguments, qualified_method_with_modes,
};
use alloc::{format, string::ToString, vec::Vec};

impl Binder {
    fn delegate_invoke_signature(&self, ty: &TypeSymbol) -> Option<MethodSymbol> {
        self.type_info_of(ty)
            .filter(|info| info.kind == TypeKind::Delegate)
            .and_then(|info| {
                info.methods
                    .iter()
                    .find(|method| &*method.name == "Invoke")
                    .cloned()
            })
    }

    fn no_delegate_target(argument: &BoundExpr, delegate: &TypeSymbol) -> DiagnosticKind {
        let method = match &argument.kind {
            BoundExprKind::MethodGroup { name, .. } => name.clone(),
            _ => "method group".into(),
        };
        DiagnosticKind::NoOverloadMatchesDelegate {
            method,
            delegate: delegate.to_string().into(),
        }
    }

    /// Delegate parameters act as variable arguments in normal form. Optional parameters
    /// cannot be omitted and a parameter array cannot be expanded. The seventh edition
    /// also excludes incompatible return types before selecting the best method.
    fn select_delegate_target(
        &self,
        argument: &BoundExpr,
        delegate: &TypeSymbol,
        filter_return: bool,
    ) -> Result<MethodSymbol, DiagnosticKind> {
        let no_match = || Self::no_delegate_target(argument, delegate);
        let Some(invoke) = self.delegate_invoke_signature(delegate) else {
            return Err(no_match());
        };
        let BoundExprKind::MethodGroup { receiver, name } = &argument.kind else {
            return Err(no_match());
        };
        for tier in self.methods_in_chain_tiered(&receiver.ty, name) {
            let applicable: Vec<&MethodSymbol> = tier
                .iter()
                .filter(|candidate| {
                    candidate.parameters.len() == invoke.parameters.len()
                        && candidate
                            .parameters
                            .iter()
                            .zip(&invoke.parameters)
                            .enumerate()
                            .all(|(index, (parameter, argument))| {
                                candidate.parameter_mode(index) == invoke.parameter_mode(index)
                                    && if self.language_version == LanguageVersion::CSharp1
                                        || matches!(parameter, TypeSymbol::ByRef(_))
                                        || matches!(argument, TypeSymbol::ByRef(_))
                                    {
                                        parameter == argument
                                    } else {
                                        self.converts(argument, parameter)
                                    }
                            })
                        && (!filter_return
                            || self.delegate_signature_conversion(
                                &candidate.return_type,
                                &invoke.return_type,
                            ))
                })
                .collect();
            if applicable.is_empty() {
                continue;
            }
            return best_candidate(
                &self.model,
                &applicable,
                &invoke.parameters,
                &[],
                self.language_version,
            )
            .cloned()
            .ok_or_else(|| DiagnosticKind::AmbiguousCall {
                method: name.clone(),
            });
        }
        Err(no_match())
    }

    /// The selected method and its delegate must agree by identity or reference conversion.
    /// A failed return filter is retried only to describe the error, never to admit a method.
    pub(super) fn method_group_target(
        &self,
        argument: &BoundExpr,
        delegate: &TypeSymbol,
    ) -> Result<MethodSymbol, DiagnosticKind> {
        let target = match self.select_delegate_target(
            argument,
            delegate,
            self.language_version >= LanguageVersion::CSharp7,
        ) {
            Ok(target) => target,
            Err(DiagnosticKind::NoOverloadMatchesDelegate { .. })
                if self.language_version >= LanguageVersion::CSharp7 =>
            {
                self.select_delegate_target(argument, delegate, false)?
            }
            Err(error) => return Err(error),
        };
        let invoke = self
            .delegate_invoke_signature(delegate)
            .ok_or_else(|| Self::no_delegate_target(argument, delegate))?;
        if !self.delegate_signature_conversion(&target.return_type, &invoke.return_type) {
            let BoundExprKind::MethodGroup { receiver, name } = &argument.kind else {
                return Err(Self::no_delegate_target(argument, delegate));
            };
            return Err(DiagnosticKind::WrongDelegateReturnType {
                method: format!(
                    "{} {}",
                    target.return_type,
                    qualified_method_with_modes(&receiver.ty, name, &target)
                )
                .into(),
            });
        }
        if !invoke
            .parameters
            .iter()
            .zip(&target.parameters)
            .all(|(from, to)| self.delegate_signature_conversion(from, to))
        {
            return Err(Self::no_delegate_target(argument, delegate));
        }
        Ok(target)
    }

    /// Before the seventh edition, an applicable target can still fail compatibility
    /// when the conversion is applied. Its existence is what call applicability asks.
    pub(super) fn method_group_matches_delegate(
        &self,
        argument: &BoundExpr,
        parameter: &TypeSymbol,
    ) -> bool {
        if self.language_version >= LanguageVersion::CSharp7 {
            self.method_group_target(argument, parameter).is_ok()
        } else {
            match self.select_delegate_target(argument, parameter, false) {
                Ok(_) => true,
                Err(DiagnosticKind::AmbiguousCall { .. }) => {
                    self.language_version == LanguageVersion::CSharp2
                }
                Err(_) => false,
            }
        }
    }

    /// Selects the first declaring tier with an applicable closed candidate. A method group's
    /// error-type placeholder cannot make an incompatible derived parameter hide a base member.
    pub(super) fn candidates_for_method_group_call(
        &mut self,
        receiver: &TypeSymbol,
        name: &str,
        type_arguments: &[TypeSymbol],
        argument_types: &[TypeSymbol],
        arguments: &[BoundExpr],
        names: &[Option<(&str, Span)>],
        inferred: &[usize],
        span: Span,
    ) -> CandidateSet {
        let tiers = self.methods_in_chain_tiered(receiver, name);
        for tier in &tiers {
            let before = self.diagnostics.len();
            let set = self.candidates_for_type_arguments(
                tier,
                type_arguments,
                argument_types,
                names,
                receiver,
                span,
            );
            if set.candidates.iter().any(|method| {
                self.judge_method_group_call(method, arguments, names, inferred)
                    .is_some()
            }) {
                return set;
            }
            // A discarded tier contributes neither a candidate nor its type-argument diagnostics.
            self.diagnostics.truncate(before);
        }
        self.candidates_for_type_arguments(
            &tiers.concat(),
            type_arguments,
            argument_types,
            names,
            receiver,
            span,
        )
    }

    /// Tries the normal form first, then the expanded form, using delegate applicability for
    /// method groups and the ordinary expression conversion relation for other arguments.
    pub(super) fn judge_method_group_call<'a>(
        &self,
        method: &'a MethodSymbol,
        arguments: &[BoundExpr],
        names: &[Option<(&str, Span)>],
        inferred: &[usize],
    ) -> Option<Judged<'a>> {
        if !self.accepts_argument_count_at_version(method, arguments.len()) {
            return None;
        }
        let filled = map_arguments(method, names)?;
        for normal in [true, false] {
            if normal {
                if filled.len() > method.parameters.len()
                    || (method.is_params && filled.last().is_some_and(Option::is_none))
                {
                    continue;
                }
            } else if !method.is_params {
                continue;
            }
            let judged = Judged::mapped(method, &filled, arguments.len(), normal);
            if judged.types.len() == arguments.len()
                && arguments.iter().zip(&judged.types).enumerate().all(
                    |(index, (argument, parameter))| {
                        if inferred.contains(&index) {
                            matches!(parameter, TypeSymbol::ByRef(_))
                        } else if matches!(argument.kind, BoundExprKind::Ref { .. })
                            || matches!(parameter, TypeSymbol::ByRef(_))
                        {
                            argument_type(argument) == *parameter
                        } else {
                            self.assignable(argument, parameter)
                        }
                    },
                )
            {
                return Some(judged);
            }
        }
        None
    }

    /// Compare conversions in source argument order, including the sixth edition's
    /// preference for a delegate compatible with the selected method-group target.
    pub(super) fn better_method_group_call(
        &self,
        first: &Judged<'_>,
        second: &Judged<'_>,
        arguments: &[BoundExpr],
    ) -> bool {
        first.is_better_by(
            second,
            arguments.len(),
            self.language_version,
            |index, p, q| {
                let argument = &arguments[index];
                let mut relation = better_conversion(
                    &self.model,
                    &argument_type(argument),
                    p,
                    q,
                    self.language_version,
                );
                if matches!(argument.kind, BoundExprKind::MethodGroup { .. })
                    && self.language_version >= LanguageVersion::CSharp6
                {
                    relation = match (
                        self.method_group_target(argument, p).is_ok(),
                        self.method_group_target(argument, q).is_ok(),
                    ) {
                        (true, false) => Better::First,
                        (false, true) => Better::Second,
                        _ => relation,
                    };
                }
                relation
            },
        )
    }

    /// Failed calls first explain an incompatible supplied argument. A metadata default
    /// unavailable in the selected edition cannot hide an earlier type error.
    pub(super) fn report_method_group_call_failure(
        &mut self,
        name: &str,
        declaring: &TypeSymbol,
        candidates: &[MethodSymbol],
        arguments: &[BoundExpr],
        names: &[Option<(&str, Span)>],
        inferred: &[usize],
        span: Span,
    ) {
        for candidate in candidates
            .iter()
            .filter(|c| c.accepts_argument_count(arguments.len()))
        {
            let Some(filled) = map_arguments(candidate, names) else {
                continue;
            };
            for (index, argument) in arguments.iter().enumerate() {
                let Some(parameter_index) = filled.iter().position(|slot| *slot == Some(index))
                else {
                    continue;
                };
                let Some(parameter) = candidate.parameters.get(parameter_index) else {
                    continue;
                };
                if inferred.contains(&index) && matches!(parameter, TypeSymbol::ByRef(_)) {
                    continue;
                }
                let kind = if matches!(argument.kind, BoundExprKind::MethodGroup { .. }) {
                    match self.method_group_target(argument, parameter) {
                        Ok(_) => continue,
                        Err(error @ DiagnosticKind::WrongDelegateReturnType { .. })
                        | Err(error @ DiagnosticKind::AmbiguousCall { .. }) => error,
                        Err(_) => DiagnosticKind::ArgumentConversion {
                            index: index as u32 + 1,
                            from: "method group".into(),
                            to: parameter.to_string().into(),
                        },
                    }
                } else if self.assignable(argument, parameter) {
                    continue;
                } else {
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: argument_display(&argument.ty),
                        to: parameter.to_string().into(),
                    }
                };
                self.report(Diagnostic::new(kind, span));
                return;
            }
            if !self.accepts_argument_count_at_version(candidate, arguments.len()) {
                if let Some(parameter) = filled.iter().position(Option::is_none) {
                    self.report(Diagnostic::new(
                        DiagnosticKind::MissingArgumentForParameter {
                            parameter: candidate
                                .parameter_name(parameter)
                                .unwrap_or("value")
                                .into(),
                            method: qualified_method_with_modes(declaring, name, candidate),
                        },
                        span,
                    ));
                    return;
                }
            }
        }
        if let [only] = candidates {
            if names.iter().any(Option::is_some)
                && self.check_named_arguments(name, only, names, names.len())
            {
                return;
            }
            if arguments.len() < only.parameters.len() {
                self.report(Diagnostic::new(
                    DiagnosticKind::MissingArgumentForParameter {
                        parameter: only
                            .parameter_name(arguments.len())
                            .unwrap_or("value")
                            .into(),
                        method: qualified_method_with_modes(declaring, name, only),
                    },
                    span,
                ));
                return;
            }
        }
        self.report(Diagnostic::new(
            DiagnosticKind::NoOverloadForArgumentCount {
                method: name.into(),
                count: arguments.len() as u32,
            },
            span,
        ));
    }
}
