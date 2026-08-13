//! Resolving called methods and accessed fields to their metadata tokens.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use lamella_binder::{SignatureCanon, SpecialType, TypeSymbol};
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

/// A signature blob's identity as a string key -- its bytes in hex, so two `TypeSpec` rows are the
/// same row exactly when the runtime would read them as the same type.
fn blob_key(signature: &[u8]) -> String {
    let mut key = String::with_capacity(signature.len() * 2);
    for byte in signature {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// The type a PREDICATE about `ty` should be answered from: an instantiation defers to its generic
/// definition (arity-mangled, ECMA-335 II.10.7.2), everything else is itself.
///
/// **For predicates only, never for identity.** Whether a type is a value type is decided by its
/// definition and no type argument can change it, so `Pair<int>` and `` Pair`1 `` answer alike. That
/// is the opposite of what a TOKEN needs: `Box<int>` and `Box<string>` are different types, and
/// folding them onto their definition's token is the collapse `generics-identity-and-sharing` s2
/// calls a cast hole.
fn definition_of(ty: &TypeSymbol) -> TypeSymbol {
    let TypeSymbol::Instantiation {
        definition,
        arguments,
    } = ty
    else {
        return ty.clone();
    };
    lamella_binder::definition_symbol(definition, arguments.len())
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
    /// Each enum's underlying integral type, by key. An enum-member constant loads at this
    /// width, so a `long`/`ulong`-backed member keeps a value past 32 bits (21.4).
    enum_underlying: BTreeMap<String, SpecialType>,
    /// The struct (value) types declared in this module, by key. Their signatures are
    /// `ValueType` of the type's token, and a value-type local is addressed via
    /// `ldloca` for field access.
    structs: BTreeSet<String>,
    /// The interface types declared in this module, by key. A cast to an interface is not
    /// a plain `castclass` lowering yet (interface dispatch is an interpreter feature), so
    /// emission distinguishes them.
    interfaces: BTreeSet<String>,
    /// The methods declared with a virtual vtable slot (`virtual`/`override`/`abstract`), by
    /// method key. A method group over such a method converts to a delegate through the
    /// object's runtime type (`ldvirtftn`, III.4.18), so an override is honored; a non-virtual
    /// method binds the exact method (`ldftn`).
    virtual_methods: BTreeSet<String>,
    /// The type-parameter NAMES each declared generic type owns, in declaration order, by type key
    /// -- `["TKey", "TValue"]` for `` Pair`2 ``. Empty (absent) for every non-generic type, which is
    /// every C# 1.0 type.
    ///
    /// **THIS IS KEYED BY THE DECLARING TYPE ON PURPOSE, AND THAT IS WHAT MAKES IT SAFE.** The
    /// question a signature writer asks is *"is this name a type parameter HERE"*, and the honest
    /// answer depends on which type's member is being written. A single "currently open scope"
    /// field would answer it for whatever was set last -- and the minting code that writes
    /// MemberRefs for IMPORTED methods runs interleaved with member emission, on signatures whose
    /// `!0` was already decoded back to a NAME (`sigtype_to_symbol`). Under a hidden scope an
    /// imported `T` would encode as `!0` of whichever type happened to be open, which is metadata
    /// that decodes cleanly and means something else. Looked up by declaring type, an external
    /// member simply has no entry.
    type_parameters: BTreeMap<String, Vec<Box<str>>>,
    /// Canonicalizes single-part signature names to their qualified form before keying, so a
    /// parameter/field collected structurally as `StringBuilder` keys (and serializes) the same
    /// as the binder's `System.Text.StringBuilder` -- a forward call resolved against the model's
    /// qualified signature then finds the token the syntax-keyed pre-pass recorded. The default
    /// (empty) canon is the identity, so a table built without one is unchanged.
    canon: SignatureCanon,
    /// The `TypeSpec` row minted for each constructed generic type, by type key -- so every member
    /// named through `Box<int>` shares one parent row instead of minting a fresh one each time.
    ///
    /// A size choice, not a correctness rule: `TypeSpec` is absent from II.24.2.6's sorted-table
    /// list and duplicate rows with equal blobs are interchangeable. It is taken because csc
    /// interns them and a generic-heavy program otherwise pays a row per member reference -- for
    /// `Counter<int>` with one method and one field that is two rows where one does.
    type_specs: BTreeMap<String, Token>,
    /// The `TypeSpec` row naming each type-parameter POSITION (`!0`, `!1`, ...), by index. See
    /// [`Tokens::var_spec`] for why the key is a number.
    var_specs: BTreeMap<u32, Token>,
    /// The `TypeSpec` row naming each METHOD type-parameter position (`!!0`, `!!1`, ...), by index.
    /// A separate map from [`Tokens::var_specs`] because the two are separate numbering spaces and
    /// `!0` is not `!!0` -- one map keyed by number would answer both with whichever was minted.
    mvar_specs: BTreeMap<u32, Token>,
    /// The type parameters in scope for the BODY being emitted right now: the enclosing method's
    /// own (`!!n`), then its declaring type's (`!n`).
    ///
    /// **THIS IS AMBIENT WHERE [`Tokens::type_parameters`] IS KEYED, AND THE DIFFERENCE IS WHICH
    /// QUESTION IS BEING ASKED.** That map answers *"is this name a parameter of the type whose
    /// MEMBER I am writing"*, which depends on the type and so must be looked up by it. This field
    /// answers *"is this name a parameter of the body I am LOWERING"* -- a property of the current
    /// position and of nothing else, which no key can carry, because the same spelling means a
    /// different thing in the next method. It is set by [`Tokens::enter_body_scope`] immediately
    /// around one body's minting walk and restored after, so it is never read outside the walk it
    /// describes.
    body_scope: (Vec<Box<str>>, Vec<Box<str>>),
    /// Whether unmanaged native interop is enabled (the off-by-default `native_interop` knob). When
    /// set, a `[DllImport]` extern method emits an `ImplMap` (P/Invoke); when clear, it is rejected.
    native_interop: bool,
}

impl Tokens {
    /// An empty table (for emitting bodies that reference no members).
    #[must_use]
    pub fn new() -> Tokens {
        Tokens::default()
    }

    /// Installs the signature canon (built from the binder model) that all keys are normalized
    /// through. Set BEFORE any member is recorded, so inserts and look-ups key alike.
    pub fn set_canon(&mut self, canon: SignatureCanon) {
        self.canon = canon;
    }

    /// Enables unmanaged native interop (the `native_interop` knob) for this emit.
    pub fn set_native_interop(&mut self, enabled: bool) {
        self.native_interop = enabled;
    }

    /// Whether unmanaged native interop is enabled (so a `[DllImport]` method emits an `ImplMap`).
    #[must_use]
    pub(crate) fn native_interop(&self) -> bool {
        self.native_interop
    }

    /// `ty` with single-part named types qualified, as every key is normalized. Emission
    /// (`type_sig`, token minting) canonicalizes a syntax-derived type through this before it
    /// branches on the type's shape, so e.g. a single-part `TypedReference` is recognized.
    #[must_use]
    pub(crate) fn canonical(&self, ty: &TypeSymbol) -> TypeSymbol {
        self.canon.canonicalize(ty)
    }

    /// `parameters` each canonicalized, for a method key.
    #[must_use]
    fn canonical_params(&self, parameters: &[TypeSymbol]) -> Vec<TypeSymbol> {
        parameters.iter().map(|ty| self.canonical(ty)).collect()
    }

    /// Records `token` as the `TypeDef` of this type.
    pub fn insert_type(&mut self, ty: &TypeSymbol, token: Token) {
        self.types.insert(type_key(&self.canonical(ty)), token);
    }

    /// The `TypeDef` token for this type, if one was recorded.
    #[must_use]
    pub fn type_token(&self, ty: &TypeSymbol) -> Option<Token> {
        self.types.get(&type_key(&self.canonical(ty))).copied()
    }

    /// Records the type-parameter names a declared generic type owns, in declaration order.
    /// A no-op for an empty list, so a non-generic type never gets an entry.
    pub fn insert_type_parameters(&mut self, ty: &TypeSymbol, names: Vec<Box<str>>) {
        if !names.is_empty() {
            self.type_parameters.insert(type_key(&self.canonical(ty)), names);
        }
    }

    /// The type-parameter names `ty` declares, in declaration order -- so a name's POSITION in this
    /// slice is the `n` of the `!n` a signature encodes it as (II.23.1.16). Empty for a non-generic
    /// type and for any type this module did not declare.
    #[must_use]
    pub(crate) fn type_parameters(&self, ty: &TypeSymbol) -> &[Box<str>] {
        self.type_parameters
            .get(&type_key(&self.canonical(ty)))
            .map_or(&[], Vec::as_slice)
    }

    /// Records that this type is an enum (its signatures lower to the underlying type).
    pub fn insert_enum(&mut self, ty: &TypeSymbol) {
        self.enums.insert(type_key(&self.canonical(ty)));
    }

    /// Whether this type is an enum declared in the module.
    #[must_use]
    /// Deliberately does NOT resolve an instantiation to its definition the way
    /// [`Tokens::is_struct`] does: **C# has no generic enum**, so an instantiation is never one and
    /// the plain lookup is already the right answer. Noted because the asymmetry with its sibling
    /// otherwise reads as an omission somebody would helpfully close.
    pub fn is_enum(&self, ty: &TypeSymbol) -> bool {
        self.enums.contains(&type_key(&self.canonical(ty)))
    }

    /// Records an enum's underlying integral type, so an enum-member constant loads at the
    /// declared width (21.4).
    pub fn insert_enum_underlying(&mut self, ty: &TypeSymbol, underlying: SpecialType) {
        self.enum_underlying
            .insert(type_key(&self.canonical(ty)), underlying);
    }

    /// An enum's underlying integral type, if recorded; `None` for a non-enum or an enum
    /// whose underlying type was not tracked (treated as the default `int`).
    #[must_use]
    pub(crate) fn enum_underlying(&self, ty: &TypeSymbol) -> Option<SpecialType> {
        self.enum_underlying
            .get(&type_key(&self.canonical(ty)))
            .copied()
    }

    /// Records that this type is a struct (a value type with `ValueType` signatures).
    pub fn insert_struct(&mut self, ty: &TypeSymbol) {
        self.structs.insert(type_key(&self.canonical(ty)));
    }

    /// Whether this type is a struct (value type) declared in the module.
    ///
    /// **AN INSTANTIATION IS ASKED ABOUT ITS DEFINITION, AND THAT IS NOT THE SAME AS KEYING IT
    /// BY ONE.** `Pair<int>` is a value type exactly when `` Pair`1 `` is one -- value-ness is a
    /// property of the DEFINITION, which no argument can change. So this question resolves through
    /// it, while [`Tokens::type_token`] deliberately does NOT: `Box<int>` and `Box<string>` are
    /// different types and must never share a token, so only the *predicate* follows the
    /// definition, never the identity.
    ///
    /// Without this an instantiated generic struct answers `false` here (its `Display` key is
    /// never in the set), takes the reference path, and emits `ldelem.ref` on a value type --
    /// unverifiable IL rather than a refusal.
    #[must_use]
    pub fn is_struct(&self, ty: &TypeSymbol) -> bool {
        self.structs.contains(&type_key(&self.canonical(&definition_of(ty))))
    }

    /// Records that this type is an interface declared in the module.
    pub fn insert_interface(&mut self, ty: &TypeSymbol) {
        self.interfaces.insert(type_key(&self.canonical(ty)));
    }

    /// Whether this type is an interface declared in the module.
    #[must_use]
    pub fn is_interface(&self, ty: &TypeSymbol) -> bool {
        self.interfaces.contains(&type_key(&self.canonical(ty)))
    }

    /// Records that the method with this identity occupies a virtual vtable slot, so a
    /// delegate over it dispatches through the receiver's runtime type (III.4.18).
    pub fn insert_virtual_method(
        &mut self,
        declaring: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) {
        self.virtual_methods.insert(method_key(
            &self.canonical(declaring),
            name,
            &self.canonical_params(parameters),
        ));
    }

    /// Whether the method with this identity is virtual (so a delegate over it emits
    /// `ldvirtftn` rather than `ldftn`).
    #[must_use]
    pub(crate) fn is_virtual_method(
        &self,
        declaring: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) -> bool {
        self.virtual_methods.contains(&method_key(
            &self.canonical(declaring),
            name,
            &self.canonical_params(parameters),
        ))
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
            .insert(method_key(&self.canonical(declaring), name, &self.canonical_params(parameters)), token);
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
            .get(&method_key(&self.canonical(declaring), name, &self.canonical_params(parameters)))
            .copied()
    }

    /// The `TypeSpec` row naming type parameter `index` -- the blob `ELEMENT_TYPE_VAR index`, which
    /// `initobj` needs for `default(T)` and a bare `T` has no other token for.
    ///
    /// **ONE ROW PER INDEX SERVES THE WHOLE ASSEMBLY, WHICH IS WHY IT IS KEYED BY NUMBER AND NOT BY
    /// TYPE.** `!0` means "parameter 0 of the generic context this token is used in" and is
    /// resolved at the USE site, so `` Box`1 ``'s `T` and `` Pair`2 ``'s `TKey` are the same row.
    /// Keying by NAME would have been wrong in the other direction: `Pair<A,T>`'s `T` is `!1`.
    #[must_use]
    pub(crate) fn var_spec(&self, index: u32) -> Option<Token> {
        self.var_specs.get(&index).copied()
    }

    /// Records the `TypeSpec` row naming type parameter `index`.
    pub(crate) fn insert_var_spec(&mut self, index: u32, token: Token) {
        self.var_specs.insert(index, token);
    }

    /// The `TypeSpec` row naming METHOD type parameter `index` -- the blob `ELEMENT_TYPE_MVAR
    /// index`. The `!!n` counterpart of [`Tokens::var_spec`], and separate from it for the reason
    /// that function's own doc gives about `!0` against `!!0`: they are two numbering spaces, and a
    /// token minted for one names a different type in the other.
    #[must_use]
    pub(crate) fn mvar_spec(&self, index: u32) -> Option<Token> {
        self.mvar_specs.get(&index).copied()
    }

    /// Records the `TypeSpec` row naming METHOD type parameter `index`.
    pub(crate) fn insert_mvar_spec(&mut self, index: u32, token: Token) {
        self.mvar_specs.insert(index, token);
    }

    /// Opens the generic scope of the body about to be walked, returning the scope it replaced so
    /// the caller can restore it. Pairs with [`Tokens::restore_body_scope`].
    ///
    /// **NESTING IS WHY THIS RETURNS THE OLD VALUE RATHER THAN CLEARING.** A constructor's prologue
    /// arguments and a property's accessor bodies are minted while an enclosing walk is open, and a
    /// scope that reset to empty on the way out would leave the outer walk lowering a `T` as a
    /// class name for the rest of its body.
    pub(crate) fn enter_body_scope(
        &mut self,
        method: &[Box<str>],
        declaring: &[Box<str>],
    ) -> (Vec<Box<str>>, Vec<Box<str>>) {
        core::mem::replace(&mut self.body_scope, (method.to_vec(), declaring.to_vec()))
    }

    /// Restores the generic scope [`Tokens::enter_body_scope`] replaced.
    pub(crate) fn restore_body_scope(&mut self, saved: (Vec<Box<str>>, Vec<Box<str>>)) {
        self.body_scope = saved;
    }

    /// The type parameters in scope for the body being emitted: `(method, declaring)`.
    #[must_use]
    pub(crate) fn body_scope(&self) -> (&[Box<str>], &[Box<str>]) {
        (&self.body_scope.0, &self.body_scope.1)
    }

    /// The POSITION `ty` occupies when it is a bare type parameter of the body being emitted:
    /// `(is_method_parameter, index)`, so `!!n` and `!n` are told apart by the caller rather than
    /// collapsed into a number.
    ///
    /// **THE METHOD'S LIST IS CONSULTED FIRST, AND A PARAMETER SHADOWS A REAL TYPE OF THE SAME
    /// NAME.** Both rules are `open_type_sig`'s, restated at no other site: this is the same
    /// question one layer down, asked where an INSTRUCTION needs a token rather than where a
    /// signature needs an element, and the two answering differently is a program that binds
    /// against one type and emits another.
    ///
    /// **THIS IS THE PREDICATE, AND [`Tokens::type_parameter_spec`] IS THE LOOKUP.** They must
    /// stay separate: a guard written as *"does its spec exist"* answers NO for a parameter whose
    /// row has not been minted yet and mints a `TypeRef` to a class called `T` instead -- the exact
    /// defect the guard is there to stop.
    #[must_use]
    pub(crate) fn body_type_parameter(&self, ty: &TypeSymbol) -> Option<(bool, u32)> {
        let TypeSymbol::Named(parts) = ty else {
            return None;
        };
        let [only] = &parts[..] else {
            return None;
        };
        if let Some(index) = self.body_scope.0.iter().position(|name| name == only) {
            return Some((true, index as u32));
        }
        let index = self.body_scope.1.iter().position(|name| name == only)?;
        Some((false, index as u32))
    }

    /// The `TypeSpec` naming `ty`'s POSITION when `ty` is a bare type parameter of the body being
    /// emitted -- `!!n` for one of the method's own, `!n` for one of the declaring type's. `None`
    /// when it is not a parameter, or when no row has been minted for that position.
    #[must_use]
    pub(crate) fn type_parameter_spec(&self, ty: &TypeSymbol) -> Option<Token> {
        match self.body_type_parameter(ty)? {
            (true, index) => self.mvar_spec(index),
            (false, index) => self.var_spec(index),
        }
    }

    /// The token naming `ty` where an INSTRUCTION operand needs one -- `newarr`, `castclass`,
    /// `box`, `ldtoken` and the rest.
    ///
    /// **A TYPE PARAMETER IS ANSWERED BY ITS POSITION AND IS TESTED FIRST**, because a bare `T` has
    /// no ordinary token by design (minting one invents a `TypeRef` to a class called `T` that no
    /// assembly declares) and because a parameter shadows a real type of the same name. Reversing
    /// the two would silently name a global `class T` from inside a `M<T>()` that shadows it.
    ///
    /// **A CONSTRUCTED GENERIC TYPE IS ANSWERED FROM THE BLOB-KEYED `TypeSpec` MAP, AND THAT IS
    /// WHY THE CASE BELONGS HERE RATHER THAN AT THE INSTRUCTIONS.** Twenty-eight sites ask this
    /// question -- `newarr`, `castclass`, `isinst`, `ldtoken`, `box`, `unbox`, `ldelema`,
    /// `constrained.` and the rest. Let an instantiation reach none of them and each one refuses a
    /// program csc compiles, with a message about its own opcode, so the single
    /// missing case reads as eight unrelated unimplemented features. Adding it at one site would
    /// have fixed one opcode and left the shape of the defect intact
    /// (`a-rule-with-several-implementations-gains-a-new-case-in-none-of-them`).
    #[must_use]
    pub(crate) fn instruction_type_token(&self, ty: &TypeSymbol) -> Option<Token> {
        self.type_parameter_spec(ty)
            .or_else(|| crate::compile::structural_type_spec(self, ty))
            .or_else(|| self.type_token(ty))
    }

    /// The `TypeSpec` row already minted for this signature blob, if any.
    ///
    /// **KEYED BY THE ENCODED SIGNATURE AND NOT BY THE SYMBOL, WHICH IS THE WHOLE POINT.**
    /// `Box<T>` written against a method's `T` and against its declaring type's `T` are ONE symbol
    /// -- the binder substitutes by name and both display as `Box<T>` -- and TWO rows,
    /// `Box<!!0>` and `Box<!0>`. Under a display key the first one minted answered for both, so a
    /// program naming both got one of them twice: metadata that decodes cleanly and means the
    /// other thing.
    #[must_use]
    pub(crate) fn type_spec_for(&self, signature: &[u8]) -> Option<Token> {
        self.type_specs.get(&blob_key(signature)).copied()
    }

    /// Records `token` as the `TypeSpec` row carrying this signature blob.
    pub(crate) fn insert_type_spec_for(&mut self, signature: &[u8], token: Token) {
        self.type_specs.insert(blob_key(signature), token);
    }

    /// Records `token` as the field named by this identity.
    pub fn insert_field(&mut self, declaring: &TypeSymbol, name: &str, token: Token) {
        self.fields.insert(field_key(&self.canonical(declaring), name), token);
    }

    /// The token for the field with this identity, if one was recorded.
    #[must_use]
    pub fn field(&self, declaring: &TypeSymbol, name: &str) -> Option<Token> {
        self.fields.get(&field_key(&self.canonical(declaring), name)).copied()
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
