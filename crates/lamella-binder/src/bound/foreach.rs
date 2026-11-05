//! Foreach member lookup on interfaces and constrained parameters (edition 4, 14.3 and 25.7.2).

use super::*;

enum PatternMember {
    Method(MethodSymbol),
    Property(PropertySymbol),
    Other,
}

struct PatternDeclaration {
    declaring: TypeSymbol,
    interface: bool,
    member: PatternMember,
}

impl PatternDeclaration {
    fn is_method(&self) -> bool {
        matches!(self.member, PatternMember::Method(_))
    }

    fn is_class_member(&self) -> bool {
        !self.interface && !is_system_object(&self.declaring)
    }
}

impl Binder {
    /// Combines a parameter's effective base class with its effective interface set. Interfaces
    /// reached only through a class constraint are not in that set (edition 4, 25.7). An ordinary
    /// interface supplies one root, followed by the same inherited-member lookup.
    fn foreach_parameter_roots(&self, ty: &TypeSymbol) -> Vec<TypeSymbol> {
        let mut roots = alloc::vec![self.lookup_type_of(ty)];
        let mut pending = alloc::vec![ty.clone()];
        let mut visited = Vec::new();
        while let Some(parameter) = pending.pop() {
            if visited.contains(&parameter) {
                continue;
            }
            let Some(constraints) = self.constraints_of_type_parameter(&parameter) else {
                continue;
            };
            visited.push(parameter);
            for constraint in &constraints.types {
                let constraint = self.resolve_type(constraint);
                if self.is_bare_type_parameter(&constraint) {
                    pending.push(constraint);
                } else if self
                    .type_info_of(&constraint)
                    .is_some_and(|info| info.kind == TypeKind::Interface)
                    && !roots.contains(&constraint)
                {
                    roots.push(constraint);
                }
            }
        }
        roots
    }

    fn foreach_member_base(&self, derived: &TypeSymbol, base: &TypeSymbol) -> bool {
        derived != base
            && self
                .member_lookup_walk(derived, false)
                .skip(1)
                .any(|(ty, _)| ty == *base)
    }

    /// Keeps declaration identity across interface diamonds and unrelated interfaces. Equal
    /// signatures in unrelated interfaces are distinct candidates, not duplicate declarations.
    fn foreach_parameter_lookup(&self, ty: &TypeSymbol, name: &str) -> Vec<PatternDeclaration> {
        let mut declarations = Vec::new();
        let mut visited = Vec::new();
        for root in self.foreach_parameter_roots(ty) {
            for (declaring, info) in self.member_lookup_walk(&root, true) {
                if visited.contains(&declaring) {
                    continue;
                }
                visited.push(declaring.clone());
                let interface = info.kind == TypeKind::Interface;
                let mut push = |member| {
                    declarations.push(PatternDeclaration {
                        declaring: declaring.clone(),
                        interface,
                        member,
                    });
                };
                for method in info.methods_named(name) {
                    if !method.is_override
                        && method.explicit_interface.is_none()
                        && self.is_accessible(&declaring, method.accessibility)
                    {
                        push(PatternMember::Method(method.clone()));
                    }
                }
                for property in info
                    .properties
                    .iter()
                    .filter(|property| &*property.name == name)
                {
                    if !property.is_override
                        && property.explicit_interface.is_none()
                        && self.is_accessible(&declaring, property.accessibility)
                    {
                        push(PatternMember::Property(property.clone()));
                    }
                }
                if info.fields.iter().any(|field| {
                    &*field.name == name && self.is_accessible(&declaring, field.accessibility)
                }) || info.events.iter().any(|event| {
                    &*event.name == name && self.is_accessible(&declaring, event.accessibility)
                }) {
                    push(PatternMember::Other);
                }
                let definition = type_symbol_in(&info.namespace, &info.name).to_string();
                if self
                    .model
                    .get(&definition, name)
                    .is_some_and(|nested| self.is_accessible(&declaring, nested.accessibility))
                {
                    push(PatternMember::Other);
                }
            }
        }
        // Hiding precedes applicability. In member lookup a method hides only non-methods, a
        // non-method hides all base members, and class declarations hide interface members
        // (14.3). Base methods, those with the same signature included, are removed after
        // applicability, below (14.5.5.1).
        let hidden: Vec<_> = declarations
            .iter()
            .map(|candidate| {
                declarations.iter().any(|other| {
                    (!other.is_method() || !candidate.is_method())
                        && (self.foreach_member_base(&other.declaring, &candidate.declaring)
                            || (other.is_class_member() && candidate.interface))
                })
            })
            .collect();
        declarations
            .into_iter()
            .zip(hidden)
            .filter_map(|(declaration, hidden)| (!hidden).then_some(declaration))
            .collect()
    }

    /// The Current and MoveNext of an enumerator that is a type parameter or, at every version,
    /// an interface, found by the combined lookup, including split constraints and inherited
    /// members. The foreach disposal rule explicitly includes type parameters, despite the older
    /// class/struct/interface wording (edition 4, 15.8.4).
    pub(super) fn foreach_parameter_members(
        &mut self,
        ty: &TypeSymbol,
        span: Span,
    ) -> Option<(MethodReference, MethodReference)> {
        let properties = self.foreach_parameter_lookup(ty, "Current");
        let [current] = &properties[..] else {
            return None;
        };
        let PatternMember::Property(property) = &current.member else {
            return None;
        };
        if property.accessibility != Accessibility::Public
            || property.is_static
            || !property.has_getter
            || !property
                .getter_accessibility
                .is_some_and(|accessibility| self.is_accessible(&current.declaring, accessibility))
        {
            return None;
        }
        let get_current = self.resolve_property_getter(&current.declaring, "Current", span)?;
        let members = self.foreach_parameter_lookup(ty, "MoveNext");
        if members.iter().any(|member| !member.is_method()) {
            return None;
        }
        let applicable: Vec<_> = members
            .iter()
            .filter_map(|declaration| match &declaration.member {
                // An empty argument list cannot infer any method type argument.
                PatternMember::Method(method)
                    if method.type_parameters.is_empty()
                        && is_applicable(&self.model, method, &[], &[]) =>
                {
                    Some((declaration, method))
                }
                _ => None,
            })
            .collect();
        // Only an applicable class method removes interface methods (25.7.2).
        // Applicable declarations in derived types remove base methods (14.5.5.1).
        let preferred: Vec<_> = applicable
            .iter()
            .filter(|(candidate, _)| {
                !applicable.iter().any(|(other, _)| {
                    self.foreach_member_base(&other.declaring, &candidate.declaring)
                        || (other.is_class_member() && candidate.interface)
                })
            })
            .collect();
        let methods: Vec<_> = preferred.iter().map(|(_, method)| *method).collect();
        let chosen = best_candidate(&self.model, &methods, &[], &[])?;
        if chosen.is_static
            || chosen.accessibility != Accessibility::Public
            || chosen.return_type != TypeSymbol::Special(SpecialType::Boolean)
            || (self
                .language_version
                .foreach_incomplete_pattern_uses_interface()
                && !chosen.parameters.is_empty())
        {
            return None;
        }
        let (declaration, _) = preferred
            .iter()
            .find(|(_, method)| core::ptr::eq(*method, chosen))?;
        let move_next = MethodReference {
            declaring_type: declaration.declaring.clone(),
            declaring_instantiation: self.declaring_instantiation_of(
                &declaration.declaring,
                &chosen.name,
                &chosen.parameters,
            ),
            name: chosen.name.clone(),
            parameters: chosen.parameters.clone(),
            return_type: chosen.return_type.clone(),
            is_static: chosen.is_static,
            is_vararg: chosen.is_vararg,
            instantiation: None,
        };
        Some((move_next, get_current))
    }

    /// Interface calls on a generic enumerator must address its original storage (25.7.3).
    /// Recognizing the pattern does not authorize lowering those calls through boxed copies.
    pub(crate) fn foreach_needs_generic_interface_calls(
        &self,
        enumerator: &TypeSymbol,
        move_next: &MethodReference,
        current: &MethodReference,
    ) -> bool {
        self.is_bare_type_parameter(enumerator)
            && [move_next, current].iter().any(|method| {
                self.type_info_of(&method.declaring_type)
                    .is_some_and(|info| info.kind == TypeKind::Interface)
            })
    }
}
