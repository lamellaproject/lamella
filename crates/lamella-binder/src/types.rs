//! The binder's type representation (ECMA-334 1st ed, clause 11).

use crate::special::SpecialType;
use alloc::boxed::Box;
use core::fmt;

/// A type, as the binder understands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeSymbol {
    /// A predefined type (4.1.4).
    Special(SpecialType),
    /// A named type given by its dotted name, not yet bound to a declaration.
    Named(Box<[Box<str>]>),
    /// A constructed generic type -- one instantiation of a generic definition, `List<int>`
    /// (ECMA-334 4th ed 25.5; ECMA-335 4th ed II.23.2.12 `GENERICINST`).
    ///
    /// `arguments` is never empty, and the arity is part of the identity (25.5.1): `Box` and
    /// `Box<T>` are different types, which is why the metadata spells the definition with a
    /// backtick arity. An empty argument list would make them one.
    Instantiation {
        /// The generic definition's dotted name, as [`TypeSymbol::Named`] carries one.
        definition: Box<[Box<str>]>,
        /// The type arguments, in declaration order; at least one.
        arguments: Box<[TypeSymbol]>,
    },
    /// An array type: `rank` dimensions of `element` (`int[]` is rank 1,
    /// `int[,]` rank 2).
    Array {
        /// The element type.
        element: Box<TypeSymbol>,
        /// The number of dimensions (at least 1).
        rank: u8,
    },
    /// An unsafe pointer type `T*` (III.1.1.5): a raw managed pointer to `element`.
    Pointer(Box<TypeSymbol>),
    /// A managed-pointer (`ref`/`out`) parameter type `T&` (III.1.1.4.2): the referent is
    /// `element`. It arises only on a method signature's parameter -- declared in source or
    /// preserved from a referenced assembly's metadata -- so the signature encodes the byref
    /// and overloading distinguishes the passing mode (10.6). The parameter's BODY use reads
    /// and writes the referent through the address it holds.
    ByRef(Box<TypeSymbol>),
    /// A type that could not be resolved; emitted with a diagnostic so binding
    /// continues.
    Error,
}

impl TypeSymbol {
    /// A predefined-type symbol.
    #[must_use]
    pub fn special(special: SpecialType) -> TypeSymbol {
        TypeSymbol::Special(special)
    }

    /// An array of `self`.
    #[must_use]
    pub fn into_array(self, rank: u8) -> TypeSymbol {
        TypeSymbol::Array {
            element: Box::new(self),
            rank,
        }
    }

    /// This symbol with a written-out `System` built-in folded to its special form:
    /// `System.Int32` written in source and the `int` keyword are ONE type (4.1.4),
    /// whether the built-in comes from a reference or from a corlib-style compilation's
    /// own `System` declarations. Every constructor of a named symbol folds through
    /// this, so the two spellings never diverge in comparisons.
    #[must_use]
    pub fn fold_builtin(self) -> TypeSymbol {
        if let TypeSymbol::Named(parts) = &self {
            if let [namespace, name] = &parts[..] {
                if let Some(special) = crate::reference::special_for_named(namespace, name) {
                    return TypeSymbol::Special(special);
                }
            }
        }
        self
    }

    /// Whether this is `void`.
    #[must_use]
    pub fn is_void(&self) -> bool {
        matches!(self, TypeSymbol::Special(SpecialType::Void))
    }

    /// Whether this is the error type (resolution failed).
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, TypeSymbol::Error)
    }
}

/// Whether a definition and its arguments are `System.Nullable<T>` -- the type `T?` denotes
/// (11.4). One argument and the two-part `System.Nullable` name; the last part is unmangled here,
/// as [`TypeSymbol::Instantiation`] holds it everywhere.
#[must_use]
pub(crate) fn is_nullable(definition: &[Box<str>], arguments: &[TypeSymbol]) -> bool {
    arguments.len() == 1
        && matches!(definition, [namespace, name]
            if &**namespace == "System" && &**name == "Nullable")
}

/// Renders a type as C# SOURCE would spell it, for a diagnostic.
///
/// **THIS IS NOT THE CANONICAL INSTANTIATION SPELLING AND MUST NEVER BECOME IT.** An
/// instantiation has two strings and they are deliberately different
/// (the generics identity rule): the IDENTITY, which is
/// `` System.Collections.Generic.List`1[System.Int32] `` and exists to be unique, deterministic and
/// frozen; and the DISPLAY name, `List<int>`, which exists to match what the programmer wrote.
/// One string serving both forces a choice between a correctness key nobody may prettify and a
/// device carrying bracketed identities in its name table. `Display` is the second of the two.
///
/// The identity's rule is `lamella_aot::generics::TypeArg::spell`, and when the binder needs to
/// produce one it takes it from there rather than growing a second implementation here.
impl fmt::Display for TypeSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeSymbol::Special(special) => f.write_str(special.keyword()),
            TypeSymbol::Named(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(part)?;
                }
                Ok(())
            }
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } if is_nullable(definition, arguments) => {
                write!(f, "{}?", arguments[0])
            }
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => {
                let mut taken = 0usize;
                for (index, part) in definition.iter().enumerate() {
                    if index > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(&crate::symbols::unmangled_type_name(part))?;
                    let own = if index + 1 == definition.len() {
                        arguments.len().saturating_sub(taken)
                    } else {
                        crate::symbols::mangled_arity(part)
                    };
                    let end = taken.saturating_add(own).min(arguments.len());
                    if taken < end {
                        f.write_str("<")?;
                        for (position, argument) in arguments[taken..end].iter().enumerate() {
                            if position > 0 {
                                f.write_str(", ")?;
                            }
                            write!(f, "{argument}")?;
                        }
                        f.write_str(">")?;
                    }
                    taken = end;
                }
                Ok(())
            }
            TypeSymbol::Array { element, rank } => {
                write!(f, "{element}[")?;
                for _ in 1..*rank {
                    f.write_str(",")?;
                }
                f.write_str("]")
            }
            TypeSymbol::Pointer(element) => write!(f, "{element}*"),
            TypeSymbol::ByRef(element) => write!(f, "ref {element}"),
            TypeSymbol::Error => f.write_str("<error>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;

    fn named(parts: &[&str]) -> TypeSymbol {
        TypeSymbol::Named(parts.iter().map(|&p| p.into()).collect())
    }

    #[test]
    fn display_uses_keywords_and_dotted_names() {
        assert_eq!(TypeSymbol::special(SpecialType::Int32).to_string(), "int");
        assert_eq!(
            TypeSymbol::special(SpecialType::String).to_string(),
            "string"
        );
        assert_eq!(
            named(&["System", "IO", "Stream"]).to_string(),
            "System.IO.Stream"
        );
    }

    #[test]
    fn arrays_render_with_rank_commas() {
        let int_array = TypeSymbol::special(SpecialType::Int32).into_array(1);
        assert_eq!(int_array.to_string(), "int[]");
        let rank2 = TypeSymbol::special(SpecialType::Int32).into_array(2);
        assert_eq!(rank2.to_string(), "int[,]");
        let jagged = TypeSymbol::special(SpecialType::Int32)
            .into_array(1)
            .into_array(1);
        assert_eq!(format!("{jagged}"), "int[][]");
    }

    #[test]
    fn void_and_error_are_recognized() {
        assert!(TypeSymbol::special(SpecialType::Void).is_void());
        assert!(!TypeSymbol::special(SpecialType::Int32).is_void());
        assert!(TypeSymbol::Error.is_error());
    }
}

impl TypeSymbol {
    /// The type's name WITHOUT its namespace, as csc renders it in a handful of messages.
    ///
    /// **csc DOES NOT USE ONE RENDERING FOR ALL DIAGNOSTICS, AND THIS IS NOT A STYLE CHOICE WE
    /// GET TO MAKE.** Measured on the same type in the same file: `CS0029` says
    /// *'System.Func<int, int>'* and `CS1593` says *'Func<int, int>'*. Both are csc, one compilation
    /// apart. The messages are search keys, so each one is copied as it is, and this exists for the
    /// second group rather than as an opinion about which is better.
    ///
    /// Structural rather than string surgery: dropping everything before the last `.` would also
    /// cut into a type ARGUMENT (`Func<System.Guid, int>`), which is a different type's name.
    #[must_use]
    pub fn short_name(&self) -> alloc::string::String {
        use alloc::string::ToString;
        match self {
            TypeSymbol::Named(parts) => parts
                .last()
                .map_or_else(|| self.to_string(), |name| (**name).to_string()),
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => {
                let mut out = definition
                    .last()
                    .map_or_else(alloc::string::String::new, |name| (**name).to_string());
                out.push('<');
                for (index, argument) in arguments.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&argument.to_string());
                }
                out.push('>');
                out
            }
            _ => self.to_string(),
        }
    }
}
