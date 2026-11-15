use super::*;

/// Distinct parameter identities for one interface-mapping walk. Model signatures keep
/// parameters as simple names, so a closed base's global type can have the same spelling
/// as a derived parameter. Instantiate the walk's root with private names before following
/// its bases; normal substitution then preserves where each argument came from.
///
/// These names are used only for interface identity. Member signatures and diagnostics
/// recover their original spelling, and the stored model is never changed.
#[derive(Default)]
pub(super) struct InterfaceTypeParameters {
    originals: BTreeMap<Box<str>, TypeSymbol>,
}

impl InterfaceTypeParameters {
    pub(super) fn for_type(model: &Model, ty: &TypeSymbol) -> (Self, TypeSymbol) {
        let mut parameters = Self::default();
        let TypeSymbol::Named(parts) = ty else {
            return (parameters, ty.clone());
        };
        let Some(info) = model
            .get_by_symbol(ty)
            .filter(|info| !info.type_parameters.is_empty())
        else {
            return (parameters, ty.clone());
        };
        let arguments = info
            .type_parameters
            .iter()
            .enumerate()
            .map(|(index, name)| {
                // The delimiters cannot occur in a source identifier.
                let key: Box<str> = alloc::format!("<interface-parameter:{index}>").into();
                parameters.originals.insert(
                    key.clone(),
                    TypeSymbol::Named(alloc::vec![name.clone()].into()),
                );
                TypeSymbol::Named(alloc::vec![key].into())
            })
            .collect();
        let mut definition = parts.to_vec();
        if let Some(name) = definition.last_mut() {
            *name = unmangled_type_name(name);
        }
        (
            parameters,
            TypeSymbol::Instantiation {
                definition: definition.into(),
                arguments,
            },
        )
    }

    pub(super) fn original(&self, ty: &TypeSymbol) -> TypeSymbol {
        let bindings = self
            .originals
            .iter()
            .map(|(name, ty)| (&**name, ty))
            .collect();
        crate::symbols::substitute(ty, &bindings)
    }

    pub(super) fn same_parameters(&self, candidate: &[TypeSymbol], wanted: &[TypeSymbol]) -> bool {
        candidate.len() == wanted.len()
            && candidate
                .iter()
                .zip(wanted)
                .all(|(left, right)| self.original(left) == *right)
    }
}
