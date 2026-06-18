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
    /// An array type: `rank` dimensions of `element` (`int[]` is rank 1,
    /// `int[,]` rank 2).
    Array {
        /// The element type.
        element: Box<TypeSymbol>,
        /// The number of dimensions (at least 1).
        rank: u8,
    },
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
            TypeSymbol::Array { element, rank } => {
                write!(f, "{element}[")?;
                for _ in 1..*rank {
                    f.write_str(",")?;
                }
                f.write_str("]")
            }
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
