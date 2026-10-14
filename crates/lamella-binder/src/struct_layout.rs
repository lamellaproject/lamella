//! Whether a struct's layout contains the struct itself (ECMA-334 6th ed 15.4.2).
//!
//! A struct directly depends on a struct it holds an instance field of, and it depends on the
//! transitive closure of that. It is an error for a struct to depend on itself or on a type
//! constructed from it, because its value would have to contain itself.
//!
//! The closure is taken over CONSTRUCTED types, and it can be infinite:
//! `struct First<T> { Second<T> Next; }` and `struct Second<T> { First<Second<T>> Back; }` reach
//! `First<Second<T>>`, then `Second<Second<T>>`, and on without ever repeating a type. The question
//! the rule asks of it has a finite answer all the same, because it is about DEFINITIONS: whether
//! the closure reaches any type constructed from the struct. A type argument changes which
//! definitions an instantiation reaches only where the definition lays a type parameter out inside
//! itself, so each definition is summarized once over its own parameters and the summaries are
//! joined at each instantiation.

use crate::symbols::{Model, TypeInfo, TypeKind, definition_symbol};
use crate::types::TypeSymbol;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// A struct definition, by the namespace and name the model holds it under.
type Definition = (Box<str>, Box<str>);

/// The value types that may hold an instance field of their own type: a core library declares each
/// with one (`System.Int32` holds an `int`), and the runtime lays them out as primitives.
///
/// The exemption belongs to the FIELD's type. A struct holding one of these whose definition holds
/// the struct in turn is not a cycle either; a struct holding any other type that reaches back is.
const PRIMITIVES: [&str; 14] = [
    "Boolean", "Char", "SByte", "Byte", "Int16", "UInt16", "Int32", "UInt32", "Int64", "UInt64",
    "Single", "Double", "IntPtr", "UIntPtr",
];

/// The position in `info.fields` of the first instance field that lays the struct `info` out
/// inside itself, or `None` when none does.
///
/// A field whose type is one of the [`PRIMITIVES`] is never the answer.
pub(crate) fn first_field_containing_itself(model: &Model, info: &TypeInfo) -> Option<usize> {
    let own = definition_of(info);
    let layouts = Layouts::summarize(model, info);
    info.fields.iter().position(|field| {
        !field.is_static
            && !layouts.is_primitive(&field.ty)
            && layouts
                .contents(&field.ty, &info.type_parameters)
                .structs
                .contains(&own)
    })
}

fn definition_of(info: &TypeInfo) -> Definition {
    (info.namespace.clone(), info.name.clone())
}

/// What laying out one type puts inside it, written over the type parameters of the definition
/// the type appears in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Contents {
    /// The struct definitions laid out inside it, whatever the type arguments are.
    structs: BTreeSet<Definition>,
    /// The type parameters laid out inside it, by position: where an argument's own contents join.
    parameters: BTreeSet<usize>,
}

impl Contents {
    fn join(&mut self, other: Contents) {
        self.structs.extend(other.structs);
        self.parameters.extend(other.parameters);
    }
}

/// The contents of every struct definition a layout can reach, each over its own type parameters.
struct Layouts<'m> {
    model: &'m Model,
    /// Each definition's contents, the least that satisfies every definition's fields at once.
    summaries: BTreeMap<Definition, Contents>,
}

impl<'m> Layouts<'m> {
    /// The contents of every struct definition reachable from `root`'s fields.
    ///
    /// The definitions are found first, by the types their fields NAME rather than the types those
    /// fields become, so the search is finite; then every summary grows from empty until no
    /// definition's fields add to it, which ends because a summary only ever grows and there are
    /// finitely many definitions and parameters to add.
    fn summarize(model: &'m Model, root: &TypeInfo) -> Layouts<'m> {
        let mut layouts = Layouts {
            model,
            summaries: BTreeMap::new(),
        };
        let mut found: Vec<Cow<'m, TypeInfo>> = Vec::new();
        for ty in layout_field_types(root) {
            layouts.name_definitions(ty, &mut found);
        }
        let mut reached = 0;
        while let Some(next) = found.get(reached) {
            reached += 1;
            let types: Vec<TypeSymbol> = layout_field_types(next).cloned().collect();
            for ty in &types {
                layouts.name_definitions(ty, &mut found);
            }
        }
        loop {
            let mut grew = false;
            for info in &found {
                let mut contents = Contents::default();
                for ty in layout_field_types(info) {
                    contents.join(layouts.contents(ty, &info.type_parameters));
                }
                let summary = layouts.summaries.entry(definition_of(info)).or_default();
                if *summary != contents {
                    *summary = contents;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        layouts
    }

    /// Adds to `found` every struct definition `ty` names that is not there yet, its type
    /// arguments' included, since a definition may lay an argument out inside itself.
    fn name_definitions(&mut self, ty: &TypeSymbol, found: &mut Vec<Cow<'m, TypeInfo>>) {
        let Some(info) = self.struct_definition(ty) else {
            return;
        };
        if let TypeSymbol::Instantiation { arguments, .. } = ty {
            for argument in arguments {
                self.name_definitions(argument, found);
            }
        }
        let definition = definition_of(&info);
        if !self.summaries.contains_key(&definition) {
            self.summaries.insert(definition, Contents::default());
            found.push(info);
        }
    }

    /// What laying out `ty` puts inside it, where `ty` is written in a definition whose type
    /// parameters are `parameters`.
    fn contents(&self, ty: &TypeSymbol, parameters: &[Box<str>]) -> Contents {
        let mut contents = Contents::default();
        if let TypeSymbol::Named(parts) = ty
            && let [name] = &parts[..]
            && let Some(position) = parameters.iter().position(|parameter| parameter == name)
        {
            contents.parameters.insert(position);
            return contents;
        }
        let Some(info) = self.struct_definition(ty) else {
            return contents;
        };
        let definition = definition_of(&info);
        let summary = self.summaries.get(&definition).cloned().unwrap_or_default();
        contents.structs.insert(definition);
        contents.structs.extend(summary.structs);
        if let TypeSymbol::Instantiation { arguments, .. } = ty {
            for &position in &summary.parameters {
                if let Some(argument) = arguments.get(position) {
                    contents.join(self.contents(argument, parameters));
                }
            }
        }
        contents
    }

    /// The struct definition `ty` is, or is constructed from; `None` for any other kind of type,
    /// which a layout holds by reference or not at all.
    ///
    /// An instantiation whose argument count is not its definition's names nothing, as
    /// [`Model::get_by_symbol`] answers for it.
    fn struct_definition(&self, ty: &TypeSymbol) -> Option<Cow<'m, TypeInfo>> {
        let info = match ty {
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => {
                let info = self
                    .model
                    .get_by_symbol(&definition_symbol(definition, arguments.len()))?;
                (info.type_parameters.len() == arguments.len()).then_some(info)?
            }
            _ => self.model.get_by_symbol(ty)?,
        };
        (info.kind == TypeKind::Struct).then_some(info)
    }

    /// Whether `ty` is one of the [`PRIMITIVES`].
    fn is_primitive(&self, ty: &TypeSymbol) -> bool {
        !matches!(ty, TypeSymbol::Instantiation { .. })
            && self.struct_definition(ty).is_some_and(|info| {
                &*info.namespace == "System" && PRIMITIVES.contains(&&*info.name)
            })
    }
}

/// The types of a struct's instance fields, the ones its import left unnamed included: every
/// type its value is laid out from.
fn layout_field_types(info: &TypeInfo) -> impl Iterator<Item = &TypeSymbol> {
    info.fields
        .iter()
        .filter(|field| !field.is_static)
        .map(|field| &field.ty)
        .chain(&info.hidden_instance_field_types)
}
