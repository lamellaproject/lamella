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

/// `MethodImplAttributes` bits (II.23.1.11): how a method's body is provided.
///
/// The 2-bit `CodeTypeMask` selects the body's form. ECMA-335 requires it to be
/// exactly one of `IL`, `Native`, or `Runtime` in a conforming image (II.15.4.3 / the
/// MethodDef validity rules). `Runtime` -- "the implementation is provided by the
/// runtime" -- is the standard seam for a method the runtime supplies (delegates,
/// array `Get`/`Set`, and our BCL intrinsics). Note that `InternalCall` (0x1000) is a
/// *separate* bit that II.23.1.11 reserves as "shall be zero in conforming
/// implementations"; it is deliberately not modeled here.
pub mod method_impl {
    /// The code-type sub-field.
    pub const CODE_TYPE_MASK: u32 = 0x0003;
    /// The body is CIL.
    pub const IL: u32 = 0x0000;
    /// The body is native code addressed by the method RVA.
    pub const NATIVE: u32 = 0x0001;
    /// The body is provided by the runtime.
    pub const RUNTIME: u32 = 0x0003;
}

/// Whether a type's flags mark it public.
#[must_use]
pub fn type_is_public(flags: u32) -> bool {
    flags & type_attr::VISIBILITY_MASK == type_attr::PUBLIC
}

/// Whether a type's flags mark it nested (any `Nested*` visibility, II.23.1.15): such a
/// type has no namespace of its own and is named through its enclosing type.
#[must_use]
pub fn type_is_nested(flags: u32) -> bool {
    flags & type_attr::VISIBILITY_MASK > type_attr::PUBLIC
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

/// The code-type a method's implementation flags select (II.23.1.11): one of
/// [`method_impl::IL`], [`method_impl::NATIVE`], or [`method_impl::RUNTIME`].
#[must_use]
pub fn method_impl_code_type(impl_flags: u32) -> u32 {
    impl_flags & method_impl::CODE_TYPE_MASK
}

/// Whether a method's body is provided by the runtime (II.23.1.11 `Runtime`) -- the
/// conforming seam a managed BCL method crosses to a native runtime implementation.
#[must_use]
pub fn method_impl_is_runtime(impl_flags: u32) -> bool {
    method_impl_code_type(impl_flags) == method_impl::RUNTIME
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

    #[test]
    fn method_impl_predicates() {
        assert!(method_impl_is_runtime(method_impl::RUNTIME));
        assert_eq!(
            method_impl_code_type(method_impl::RUNTIME),
            method_impl::RUNTIME
        );
        assert!(!method_impl_is_runtime(method_impl::IL));
        assert!(!method_impl_is_runtime(0x1000));
        assert_eq!(method_impl_code_type(0x1000), method_impl::IL);
    }
}
