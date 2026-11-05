//! Identity and reference conversions for delegate-value signatures (edition 4, 14.5.10.3).

use super::{Binder, LanguageVersion, TypeKind, TypeSymbol};

impl Binder {
    /// Edition 2, 22.1, requires identical signatures. Edition 4, 14.5.10.3, also permits
    /// implicit reference conversions; numeric, boxing and user-defined conversions do not qualify.
    pub(super) fn delegate_signature_conversion(&self, from: &TypeSymbol, to: &TypeSymbol) -> bool {
        from == to
            || (self
                .language_version
                .delegate_value_allows_reference_variance()
                && self.delegate_signature_reference_conversion(from, to))
    }

    /// Array element conversions depend on the constraints in the current scope. A model-only
    /// assignment conversion cannot distinguish a reference conversion from boxing for `T`.
    fn delegate_signature_reference_conversion(&self, from: &TypeSymbol, to: &TypeSymbol) -> bool {
        if !self.satisfies_reference_type_constraint(from) {
            return false;
        }
        // A reference-type parameter may convert to a parameter it depends on even when the
        // destination is not itself known to be a reference type (edition 4, 13.1.4).
        if self.is_bare_type_parameter(to)
            && self.type_parameter_converts_to(from, to, self.type_parameters_in_scope.len())
        {
            return true;
        }
        if let TypeSymbol::Array {
            element: from_element,
            rank: from_rank,
        } = from
        {
            if let TypeSymbol::Array {
                element: to_element,
                rank: to_rank,
            } = to
            {
                return from_rank == to_rank
                    && self.delegate_signature_reference_conversion(from_element, to_element);
            }
            if let TypeSymbol::Instantiation {
                definition,
                arguments,
            } = to
                && let [system, collections, generic, name] = &definition[..]
                && &**system == "System"
                && &**collections == "Collections"
                && &**generic == "Generic"
                && let [to_element] = &arguments[..]
            {
                // Edition 4, 13.1.4, permits IList<T> and its bases. Edition 5, 11.2.7,
                // adds IReadOnlyList<T> and its bases; the earlier conversion list is closed.
                let array_interface = match &**name {
                    "IList" | "ICollection" | "IEnumerable" => true,
                    "IReadOnlyList" | "IReadOnlyCollection" => {
                        self.language_version >= LanguageVersion::CSharp5
                    }
                    _ => false,
                };
                // A source declaration can shadow a framework interface's full name without
                // becoming one of the interfaces implemented by an array.
                let framework_interface = self
                    .type_info_of(to)
                    .is_some_and(|info| info.is_external && info.kind == TypeKind::Interface);
                return array_interface
                    && framework_interface
                    && *from_rank == 1
                    && (from_element.as_ref() == to_element
                        || self.delegate_signature_reference_conversion(from_element, to_element));
            }
        }
        self.satisfies_reference_type_constraint(to) && self.satisfies_named_constraint(from, to)
    }
}
