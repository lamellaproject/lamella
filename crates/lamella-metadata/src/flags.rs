//! Type, field, and method attribute flags (ECMA-335 1st ed, II.23.1).

/// `TypeAttributes` bits (II.23.1.15).
pub mod type_attr {
    /// The visibility sub-field.
    pub const VISIBILITY_MASK: u32 = 0x0000_0007;
    /// Public top-level visibility.
    pub const PUBLIC: u32 = 0x0000_0001;
    /// The class-semantics sub-field (class vs. interface).
    pub const CLASS_SEMANTICS_MASK: u32 = 0x0000_0020;
    /// The type is an interface.
    pub const INTERFACE: u32 = 0x0000_0020;
    /// The type is abstract.
    pub const ABSTRACT: u32 = 0x0000_0080;
    /// The type is sealed.
    pub const SEALED: u32 = 0x0000_0100;
}

/// `FieldAttributes` bits (II.23.1.5).
pub mod field_attr {
    /// The accessibility sub-field.
    pub const ACCESS_MASK: u32 = 0x0007;
    /// Public accessibility.
    pub const PUBLIC: u32 = 0x0006;
    /// The field is static.
    pub const STATIC: u32 = 0x0010;
    /// The field is initialize-only (`readonly`).
    pub const INIT_ONLY: u32 = 0x0020;
    /// The field is a compile-time constant (`const`).
    pub const LITERAL: u32 = 0x0040;
}

/// `MethodAttributes` bits (II.23.1.10).
pub mod method_attr {
    /// The accessibility sub-field.
    pub const ACCESS_MASK: u32 = 0x0007;
    /// Public accessibility.
    pub const PUBLIC: u32 = 0x0006;
    /// The method is static.
    pub const STATIC: u32 = 0x0010;
    /// The method is final (cannot be overridden).
    pub const FINAL: u32 = 0x0020;
    /// The method is virtual.
    pub const VIRTUAL: u32 = 0x0040;
    /// The method is abstract (no body).
    pub const ABSTRACT: u32 = 0x0400;
}

/// Whether a type's flags mark it public.
#[must_use]
pub fn type_is_public(flags: u32) -> bool {
    flags & type_attr::VISIBILITY_MASK == type_attr::PUBLIC
}

/// Whether a type's flags mark it an interface.
#[must_use]
pub fn type_is_interface(flags: u32) -> bool {
    flags & type_attr::CLASS_SEMANTICS_MASK == type_attr::INTERFACE
}

/// Whether a type's flags mark it abstract.
#[must_use]
pub fn type_is_abstract(flags: u32) -> bool {
    flags & type_attr::ABSTRACT != 0
}

/// Whether a type's flags mark it sealed.
#[must_use]
pub fn type_is_sealed(flags: u32) -> bool {
    flags & type_attr::SEALED != 0
}

/// Whether a field's flags mark it static.
#[must_use]
pub fn field_is_static(flags: u32) -> bool {
    flags & field_attr::STATIC != 0
}

/// Whether a field's flags mark it a literal (`const`).
#[must_use]
pub fn field_is_literal(flags: u32) -> bool {
    flags & field_attr::LITERAL != 0
}

/// Whether a method's flags mark it static.
#[must_use]
pub fn method_is_static(flags: u32) -> bool {
    flags & method_attr::STATIC != 0
}

/// Whether a method's flags mark it virtual.
#[must_use]
pub fn method_is_virtual(flags: u32) -> bool {
    flags & method_attr::VIRTUAL != 0
}

/// Whether a method's flags mark it abstract.
#[must_use]
pub fn method_is_abstract(flags: u32) -> bool {
    flags & method_attr::ABSTRACT != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_predicates() {
        let flags = type_attr::PUBLIC | type_attr::SEALED;
        assert!(type_is_public(flags));
        assert!(type_is_sealed(flags));
        assert!(!type_is_interface(flags));
        assert!(!type_is_abstract(flags));
        let interface = type_attr::PUBLIC | type_attr::INTERFACE | type_attr::ABSTRACT;
        assert!(type_is_interface(interface));
        assert!(type_is_abstract(interface));
    }

    #[test]
    fn member_predicates() {
        let field = field_attr::PUBLIC | field_attr::STATIC | field_attr::LITERAL;
        assert!(field_is_static(field));
        assert!(field_is_literal(field));
        let method = method_attr::PUBLIC | method_attr::VIRTUAL;
        assert!(method_is_virtual(method));
        assert!(!method_is_static(method));
        assert!(!method_is_abstract(method));
    }
}
