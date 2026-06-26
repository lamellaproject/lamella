//! Resolving called methods and accessed fields to their metadata tokens.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use lamella_binder::TypeSymbol;
use lamella_token::Token;

/// A method's identity as a string key: `Declaring::Name(p0,p1,...)`.
fn method_key(declaring: &TypeSymbol, name: &str, parameters: &[TypeSymbol]) -> String {
    let mut key = String::new();
    let _ = write!(key, "{declaring}::{name}(");
    for (index, parameter) in parameters.iter().enumerate() {
        if index > 0 {
            key.push(',');
        }
        let _ = write!(key, "{parameter}");
    }
    key.push(')');
    key
}

/// The name to KEY a method by in the token map. `op_Implicit`/`op_Explicit` overload by RETURN
/// type alone (System.Decimal's `op_Explicit(decimal)` -> int / long / double / ...), so the return
/// type discriminates them here -- without it the overloads collide on `(declaring, name, params)`
/// and a call emits whichever was minted last. The emitted MemberRef keeps the plain `name`; this
/// only affects the lookup key. No other method overloads by return type.
pub(crate) fn conversion_key_name(name: &str, return_type: &TypeSymbol) -> String {
    let mut keyed = String::from(name);
    if matches!(name, "op_Implicit" | "op_Explicit") {
        let _ = write!(keyed, "\u{1}{return_type}");
    }
    keyed
}

/// A field's identity as a string key: `Declaring::Name` (fields do not overload).
fn field_key(declaring: &TypeSymbol, name: &str) -> String {
    let mut key = String::new();
    let _ = write!(key, "{declaring}::{name}");
    key
}

/// A type's identity as a string key: its display name.
fn type_key(ty: &TypeSymbol) -> String {
    let mut key = String::new();
    let _ = write!(key, "{ty}");
    key
}

/// The metadata tokens of the module's members and the strings it loads, keyed by
/// identity (member references are minted into the same table for external calls).
#[derive(Debug, Default)]
pub struct Tokens {
    types: BTreeMap<String, Token>,
    methods: BTreeMap<String, Token>,
    fields: BTreeMap<String, Token>,
    strings: BTreeMap<Vec<u16>, Token>,
    /// The enum types declared in this module, by key. Their signatures lower to the
    /// underlying integer type (v1: `int32`), so they need no `TypeDef` token.
    enums: BTreeSet<String>,
    /// The struct (value) types declared in this module, by key. Their signatures are
    /// `ValueType` of the type's token, and a value-type local is addressed via
    /// `ldloca` for field access.
    structs: BTreeSet<String>,
    /// The interface types declared in this module, by key. A cast to an interface is not
    /// a plain `castclass` lowering yet (interface dispatch is an interpreter feature), so
    /// emission distinguishes them.
    interfaces: BTreeSet<String>,
}

impl Tokens {
    /// An empty table (for emitting bodies that reference no members).
    #[must_use]
    pub fn new() -> Tokens {
        Tokens::default()
    }

    /// Records `token` as the `TypeDef` of this type.
    pub fn insert_type(&mut self, ty: &TypeSymbol, token: Token) {
        self.types.insert(type_key(ty), token);
    }

    /// The `TypeDef` token for this type, if one was recorded.
    #[must_use]
    pub fn type_token(&self, ty: &TypeSymbol) -> Option<Token> {
        self.types.get(&type_key(ty)).copied()
    }

    /// Records that this type is an enum (its signatures lower to the underlying type).
    pub fn insert_enum(&mut self, ty: &TypeSymbol) {
        self.enums.insert(type_key(ty));
    }

    /// Whether this type is an enum declared in the module.
    #[must_use]
    pub fn is_enum(&self, ty: &TypeSymbol) -> bool {
        self.enums.contains(&type_key(ty))
    }

    /// Records that this type is a struct (a value type with `ValueType` signatures).
    pub fn insert_struct(&mut self, ty: &TypeSymbol) {
        self.structs.insert(type_key(ty));
    }

    /// Whether this type is a struct (value type) declared in the module.
    #[must_use]
    pub fn is_struct(&self, ty: &TypeSymbol) -> bool {
        self.structs.contains(&type_key(ty))
    }

    /// Records that this type is an interface declared in the module.
    pub fn insert_interface(&mut self, ty: &TypeSymbol) {
        self.interfaces.insert(type_key(ty));
    }

    /// Whether this type is an interface declared in the module.
    #[must_use]
    pub fn is_interface(&self, ty: &TypeSymbol) -> bool {
        self.interfaces.contains(&type_key(ty))
    }

    /// Records `token` as the method named by this identity.
    pub fn insert_method(
        &mut self,
        declaring: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
        token: Token,
    ) {
        self.methods
            .insert(method_key(declaring, name, parameters), token);
    }

    /// The token for the method with this identity, if one was recorded.
    #[must_use]
    pub fn method(
        &self,
        declaring: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) -> Option<Token> {
        self.methods
            .get(&method_key(declaring, name, parameters))
            .copied()
    }

    /// Records `token` as the field named by this identity.
    pub fn insert_field(&mut self, declaring: &TypeSymbol, name: &str, token: Token) {
        self.fields.insert(field_key(declaring, name), token);
    }

    /// The token for the field with this identity, if one was recorded.
    #[must_use]
    pub fn field(&self, declaring: &TypeSymbol, name: &str) -> Option<Token> {
        self.fields.get(&field_key(declaring, name)).copied()
    }

    /// Records the `ldstr` token for a UTF-16 string literal.
    pub fn insert_string(&mut self, text: &[u16], token: Token) {
        self.strings.insert(text.to_vec(), token);
    }

    /// The `ldstr` token for a string literal, if one was recorded.
    #[must_use]
    pub fn string(&self, text: &[u16]) -> Option<Token> {
        self.strings.get(text).copied()
    }
}
