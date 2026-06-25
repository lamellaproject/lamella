//! The predefined types of C# 1.0 (ECMA-334 1st ed, 4.1.4).

use lamella_syntax::ast::PredefinedType;

/// A predefined (built-in) type of C# 1.0, named by its `System` identity rather
/// than its keyword (4.1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialType {
    /// `bool` -- `System.Boolean`.
    Boolean,
    /// `byte` -- `System.Byte`.
    Byte,
    /// `sbyte` -- `System.SByte`.
    SByte,
    /// `short` -- `System.Int16`.
    Int16,
    /// `ushort` -- `System.UInt16`.
    UInt16,
    /// `int` -- `System.Int32`.
    Int32,
    /// `uint` -- `System.UInt32`.
    UInt32,
    /// `long` -- `System.Int64`.
    Int64,
    /// `ulong` -- `System.UInt64`.
    UInt64,
    /// `char` -- `System.Char`.
    Char,
    /// `float` -- `System.Single`.
    Single,
    /// `double` -- `System.Double`.
    Double,
    /// `decimal` -- `System.Decimal`.
    Decimal,
    /// `string` -- `System.String`.
    String,
    /// `object` -- `System.Object`.
    Object,
    /// `void` -- `System.Void`.
    Void,
    /// The null type (11.2.7) -- the type of the `null` literal, and its only value. It has
    /// no keyword and no `System` identity; it converts implicitly to every reference type
    /// (13.1.6) and is never a value type, so it is never boxed.
    Null,
}

impl SpecialType {
    /// The special type a predefined-type keyword denotes (4.1.4).
    #[must_use]
    pub fn from_predefined(predefined: PredefinedType) -> SpecialType {
        match predefined {
            PredefinedType::Bool => SpecialType::Boolean,
            PredefinedType::Byte => SpecialType::Byte,
            PredefinedType::Sbyte => SpecialType::SByte,
            PredefinedType::Short => SpecialType::Int16,
            PredefinedType::Ushort => SpecialType::UInt16,
            PredefinedType::Int => SpecialType::Int32,
            PredefinedType::Uint => SpecialType::UInt32,
            PredefinedType::Long => SpecialType::Int64,
            PredefinedType::Ulong => SpecialType::UInt64,
            PredefinedType::Char => SpecialType::Char,
            PredefinedType::Float => SpecialType::Single,
            PredefinedType::Double => SpecialType::Double,
            PredefinedType::Decimal => SpecialType::Decimal,
            PredefinedType::String => SpecialType::String,
            PredefinedType::Object => SpecialType::Object,
            PredefinedType::Void => SpecialType::Void,
        }
    }

    /// The C# keyword spelling of the type (`int`, `string`, ...), as it appears
    /// in diagnostics (4.1.4).
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            SpecialType::Boolean => "bool",
            SpecialType::Byte => "byte",
            SpecialType::SByte => "sbyte",
            SpecialType::Int16 => "short",
            SpecialType::UInt16 => "ushort",
            SpecialType::Int32 => "int",
            SpecialType::UInt32 => "uint",
            SpecialType::Int64 => "long",
            SpecialType::UInt64 => "ulong",
            SpecialType::Char => "char",
            SpecialType::Single => "float",
            SpecialType::Double => "double",
            SpecialType::Decimal => "decimal",
            SpecialType::String => "string",
            SpecialType::Object => "object",
            SpecialType::Void => "void",
            SpecialType::Null => "<null>",
        }
    }

    /// The type's namespace and name in the `System` namespace (4.1.4).
    #[must_use]
    pub fn full_name(self) -> (&'static str, &'static str) {
        let name = match self {
            SpecialType::Boolean => "Boolean",
            SpecialType::Byte => "Byte",
            SpecialType::SByte => "SByte",
            SpecialType::Int16 => "Int16",
            SpecialType::UInt16 => "UInt16",
            SpecialType::Int32 => "Int32",
            SpecialType::UInt32 => "UInt32",
            SpecialType::Int64 => "Int64",
            SpecialType::UInt64 => "UInt64",
            SpecialType::Char => "Char",
            SpecialType::Single => "Single",
            SpecialType::Double => "Double",
            SpecialType::Decimal => "Decimal",
            SpecialType::String => "String",
            SpecialType::Object => "Object",
            SpecialType::Void => "Void",
            SpecialType::Null => "Object",
        };
        ("System", name)
    }

    /// Whether the type is one of the C# numeric types (4.1.4): the integral and
    /// floating-point types and `decimal`.
    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            SpecialType::Byte
                | SpecialType::SByte
                | SpecialType::Int16
                | SpecialType::UInt16
                | SpecialType::Int32
                | SpecialType::UInt32
                | SpecialType::Int64
                | SpecialType::UInt64
                | SpecialType::Char
                | SpecialType::Single
                | SpecialType::Double
                | SpecialType::Decimal
        )
    }

    /// Whether the type is an integral type (4.1.5): the numeric types other than the
    /// floating-point ones and `decimal` (so `sbyte`..`ulong` and `char`). The offset of
    /// pointer arithmetic must be integral.
    #[must_use]
    pub fn is_integral(self) -> bool {
        self.is_numeric()
            && !matches!(
                self,
                SpecialType::Single | SpecialType::Double | SpecialType::Decimal
            )
    }

    /// Whether the type is an unsigned integral type (4.1.5): `byte`, `ushort`,
    /// `uint`, `ulong`, and `char` (16-bit unsigned). These select the `.un` CIL
    /// forms for division, remainder, right shift, and the relational operators.
    #[must_use]
    pub fn is_unsigned(self) -> bool {
        matches!(
            self,
            SpecialType::Byte
                | SpecialType::UInt16
                | SpecialType::UInt32
                | SpecialType::UInt64
                | SpecialType::Char
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_map_to_their_system_identities() {
        assert_eq!(
            SpecialType::from_predefined(PredefinedType::Int),
            SpecialType::Int32
        );
        assert_eq!(
            SpecialType::from_predefined(PredefinedType::Float),
            SpecialType::Single
        );
        assert_eq!(
            SpecialType::from_predefined(PredefinedType::Bool),
            SpecialType::Boolean
        );
        assert_eq!(
            SpecialType::from_predefined(PredefinedType::Ulong),
            SpecialType::UInt64
        );
    }

    #[test]
    fn full_names_are_in_the_system_namespace() {
        assert_eq!(SpecialType::Int32.full_name(), ("System", "Int32"));
        assert_eq!(SpecialType::Single.full_name(), ("System", "Single"));
        assert_eq!(SpecialType::String.full_name(), ("System", "String"));
        assert_eq!(SpecialType::Void.full_name(), ("System", "Void"));
    }

    #[test]
    fn numeric_classification_matches_the_spec() {
        assert!(SpecialType::Int32.is_numeric());
        assert!(SpecialType::Char.is_numeric());
        assert!(SpecialType::Decimal.is_numeric());
        assert!(!SpecialType::Boolean.is_numeric());
        assert!(!SpecialType::String.is_numeric());
        assert!(!SpecialType::Object.is_numeric());
    }
}
