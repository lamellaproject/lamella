//! What an assembly DEMANDS of whoever consumes it, and whether this runtime can meet it.

use crate::reader::{Assembly, AttrArg, TypeName, decode_custom_attribute};
use crate::signature::{SigType, parse_method};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_token::Token;

/// Which mechanism an assembly used to demand something of its consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandKind {
    /// A required custom modifier (`modreq`) on a signature.
    RequiredModifier,
    /// A `CompilerFeatureRequiredAttribute` on a member.
    CompilerFeature,
}

impl DemandKind {
    /// How to name this mechanism in a refusal a person has to act on.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::RequiredModifier => "required custom modifier",
            Self::CompilerFeature => "compiler feature",
        }
    }
}

/// One thing an assembly demands its consumer understand, and where it demanded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demand {
    /// The mechanism.
    pub kind: DemandKind,
    /// The demanded name: a modifier's full type name, or a feature name.
    pub name: String,
    /// The member carrying it, so a refusal can say WHERE as well as WHAT. A refusal that names
    /// only the feature sends a reader looking through the whole assembly for it.
    pub site: String,
}

/// The required custom modifiers this runtime understands.
///
/// A name here is a claim that this runtime ACTS on the modifier, not merely that it parses it.
/// `IsExternalInit` distinguishes an init-only setter from an ordinary one; `InAttribute` and
/// `IsVolatile` add semantics -- a by-ref that must not be written through, a field that must not be
/// cached -- which this execution model already provides for every member, so honoring them adds no
/// obligation it fails to meet.
pub const UNDERSTOOD_REQUIRED_MODIFIERS: &[&str] = &[
    "System.Runtime.CompilerServices.IsExternalInit",
    "System.Runtime.InteropServices.InAttribute",
    "System.Runtime.CompilerServices.IsVolatile",
];

/// The `CompilerFeatureRequiredAttribute` feature names this runtime implements.
///
/// These are the two names the corlib's own `CompilerFeatureRequiredAttribute` declares as constants.
/// The list is here rather than there because a consumer must not have to load the corlib to find out
/// what it is able to consume.
pub const UNDERSTOOD_COMPILER_FEATURES: &[&str] = &["RefStructs", "RequiredMembers"];

/// The full name of the attribute that carries a compiler-feature demand.
const COMPILER_FEATURE_REQUIRED: &str =
    "System.Runtime.CompilerServices.CompilerFeatureRequiredAttribute";

/// Every demand `assembly` makes of its consumer, in the order found.
///
/// Duplicates are kept: the same feature on twenty members is twenty demands, because a caller
/// reporting a population wants the count and one refusing wants only the first.
#[must_use]
pub fn demands(assembly: &Assembly) -> Vec<Demand> {
    let mut found = Vec::new();

    for (parent, attribute) in assembly.all_custom_attributes() {
        let Some(declaring) = assembly
            .resolve_method(attribute.constructor)
            .and_then(|ctor| ctor.declaring_type)
        else {
            continue;
        };
        if full_name(declaring) != COMPILER_FEATURE_REQUIRED {
            continue;
        }
        let decoded = decode_custom_attribute(attribute.value, &[SigType::String], &|_| 0x08);
        let name = match decoded.as_ref().and_then(|d| d.fixed.first()) {
            Some(AttrArg::Str(feature)) => (*feature).to_string(),
            _ => "<undecodable feature name>".to_string(),
        };
        found.push(Demand {
            kind: DemandKind::CompilerFeature,
            name,
            site: site_of(assembly, parent),
        });
    }

    for type_def in assembly.type_defs() {
        let owner = type_def
            .name()
            .map_or_else(|| "<type>".to_string(), full_name);
        for method in type_def.methods() {
            if let Ok(sig) = parse_method(method.signature_blob()) {
                if !sig.required_modifiers.is_empty() {
                    let site = format!("{}.{}", owner, method.name().unwrap_or("<method>"));
                    push_modifiers(assembly, &sig.required_modifiers, &site, &mut found);
                }
            }
        }
        for field in type_def.fields() {
            if let Some((_, modifiers)) = field.signature_with_modifiers() {
                if !modifiers.is_empty() {
                    let site = format!("{}.{}", owner, field.name().unwrap_or("<field>"));
                    push_modifiers(assembly, &modifiers, &site, &mut found);
                }
            }
        }
    }
    found
}

/// A readable name for whatever an attribute was attached to.
///
/// A token is enough to be precise and not enough to be useful, so the kinds a demand realistically
/// lands on are named and the rest fall back to the token -- which still tells a reader which table
/// row to look at.
fn site_of(assembly: &Assembly, parent: Token) -> String {
    use crate::tables::table;
    match parent.table() {
        table::TYPE_DEF => assembly
            .type_token_name(parent)
            .map_or_else(|| format!("{parent:?}"), full_name),
        table::METHOD_DEF => assembly
            .resolve_method(parent)
            .map_or_else(
                || format!("{parent:?}"),
                |method| {
                    let name = method.name.unwrap_or("<method>");
                    match method.declaring_type {
                        Some(declaring) => format!("{}.{}", full_name(declaring), name),
                        None => name.to_string(),
                    }
                },
            ),
        table::ASSEMBLY => "<assembly>".to_string(),
        table::MODULE => "<module>".to_string(),
        _ => format!("{parent:?}"),
    }
}

/// The first demand this runtime cannot meet, rendered as a refusal that NAMES it.
///
/// `None` means every demand is on one of the two understood lists -- which includes an assembly
/// that demands nothing at all, the common case.
///
/// The message names three things -- the mechanism, the demanded name, and the member carrying it --
/// because a consumer that cannot say why it refused sends its reader to the wrong question. A
/// generic "bad metadata" would describe a corrupt file, which this is not.
#[must_use]
pub fn unmet_demand(assembly: &Assembly) -> Option<String> {
    unmet(&demands(assembly))
}

/// [`unmet_demand`]'s decision, over a demand list a caller already has.
///
/// Split out because it is the POLICY half and the only half with a choice in it: `demands` reports
/// what the metadata says, this decides what to do about it. Keeping them apart is what lets the
/// policy be tested without forging an assembly, and lets a census report a population without
/// taking a view on it.
#[must_use]
pub fn unmet(found: &[Demand]) -> Option<String> {
    found.iter().find_map(|demand| {
        let understood = match demand.kind {
            DemandKind::RequiredModifier => UNDERSTOOD_REQUIRED_MODIFIERS,
            DemandKind::CompilerFeature => UNDERSTOOD_COMPILER_FEATURES,
        };
        if understood.contains(&demand.name.as_str()) {
            return None;
        }
        Some(format!(
            "{} `{}` on `{}` is required by this assembly and is not implemented by this runtime",
            demand.kind.describe(),
            demand.name,
            demand.site
        ))
    })
}

/// Records each required modifier under the name its token resolves to.
fn push_modifiers(assembly: &Assembly, modifiers: &[Token], site: &str, found: &mut Vec<Demand>) {
    for token in modifiers {
        let name = assembly
            .type_token_name(*token)
            .map_or_else(|| format!("<unresolved token {:#010x}>", token.0), full_name);
        found.push(Demand {
            kind: DemandKind::RequiredModifier,
            name,
            site: site.to_string(),
        });
    }
}


/// `Namespace.Name`, or just the name for the global namespace.
fn full_name(name: TypeName<'_>) -> String {
    if name.namespace.is_empty() {
        name.name.to_string()
    } else {
        format!("{}.{}", name.namespace, name.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::{calling, element, parse_method};

    #[test]
    fn a_required_modifier_on_a_parameter_is_collected_not_dropped() {
        let blob = [
            0x00,
            0x01,
            element::VOID,
            element::CMOD_REQD,
            0x09,
            element::I4,
        ];
        let sig = parse_method(&blob).expect("the signature decodes");
        assert_eq!(sig.parameters.len(), 1, "the modifier does not consume the parameter");
        assert_eq!(
            sig.required_modifiers.len(),
            1,
            "a parameter's required modifier reaches the all-positions list"
        );
        assert!(
            sig.return_type_required_modifiers.is_empty(),
            "and NOT the positional return list, which is a different question"
        );
    }

    /// The return position still reaches both lists: the positional one it always did, and the
    /// all-positions one a consumer asks.
    #[test]
    fn a_required_modifier_on_the_return_reaches_both_lists() {
        let blob = [
            0x00,
            0x00,
            element::CMOD_REQD,
            0x09,
            element::VOID,
        ];
        let sig = parse_method(&blob).expect("the signature decodes");
        assert_eq!(sig.return_type_required_modifiers.len(), 1);
        assert_eq!(sig.required_modifiers.len(), 1);
    }

    /// A field signature's modifier is collected too -- `volatile` is spelled this way.
    #[test]
    fn a_fields_required_modifier_is_collected() {
        let blob = [calling::FIELD, element::CMOD_REQD, 0x09, element::I4];
        let (sig, modifiers) =
            crate::signature::parse_field_with_modifiers(&blob).expect("decodes");
        assert_eq!(sig, crate::signature::SigType::I4);
        assert_eq!(modifiers.len(), 1);
    }

    fn demand(kind: DemandKind, name: &str) -> Demand {
        Demand {
            kind,
            name: name.to_string(),
            site: "App.Thing.Member".to_string(),
        }
    }

    /// The policy: an understood name passes, an unknown one is refused, and the refusal NAMES it.
    #[test]
    fn an_unknown_demand_is_refused_and_the_message_names_it() {
        let understood = [
            demand(DemandKind::RequiredModifier, UNDERSTOOD_REQUIRED_MODIFIERS[0]),
            demand(DemandKind::CompilerFeature, UNDERSTOOD_COMPILER_FEATURES[0]),
        ];
        assert_eq!(unmet(&understood), None, "everything understood is not a refusal");

        let unknown = [demand(
            DemandKind::CompilerFeature,
            "RuntimeAsync",
        )];
        let message = unmet(&unknown).expect("an unimplemented feature is refused");
        assert!(
            message.contains("RuntimeAsync"),
            "the refusal names the FEATURE: {message}"
        );
        assert!(
            message.contains("App.Thing.Member"),
            "and the member carrying it, so a reader does not have to search: {message}"
        );

        let mixed = [
            demand(DemandKind::RequiredModifier, UNDERSTOOD_REQUIRED_MODIFIERS[0]),
            demand(DemandKind::RequiredModifier, "Some.Future.Modifier"),
        ];
        assert!(
            unmet(&mixed)
                .expect("the later unknown is still found")
                .contains("Some.Future.Modifier")
        );
    }

    /// A modifier whose token does not resolve is refused rather than skipped.
    #[test]
    fn an_unnameable_modifier_is_refused() {
        let unnameable = [demand(DemandKind::RequiredModifier, "<unresolved token 0x01000099>")];
        assert!(
            unmet(&unnameable).is_some(),
            "a modifier we cannot even name is certainly one we do not understand"
        );
    }
}
