//! A minimal, interim module: the methods the interpreter runs and calls between,
//! the intrinsics they reach, and the strings `ldstr` loads.

use crate::interp::Vm;
use crate::trap::Trap;
use crate::value::Value;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use lamella_cil::MethodBodyImage;
use lamella_token::Token;

/// An index identifying a [`Method`] within a [`Module`].
pub type MethodId = u32;

/// An index identifying a declared reference type within a [`Module`].
pub type TypeId = u32;

/// A declared reference type's runtime shape: the zero value of each instance field
/// (one per declaration-order slot, copied when allocating an instance) and its
/// virtual method table.
#[derive(Clone)]
struct TypeInfo {
    field_defaults: Box<[Value]>,
    /// The virtual method table: slot -> the most-derived implementation for this
    /// type. Empty for a type with no virtual methods.
    vtable: Box<[MethodId]>,
    /// The base type, for subtype checks (`None` for an Object/external base).
    base: Option<TypeId>,
    /// This type's virtual / interface-implementing methods (including inherited)
    /// keyed by a signature key (name + parameter types), for interface and
    /// abstract-method dispatch where the static target carries no vtable slot.
    sig_methods: BTreeMap<String, MethodId>,
}

/// A native (runtime-implemented) method: the intrinsic ABI.
///
/// This is the seam a BCL method crosses to reach Rust -- `System.Console.Write`
/// and friends are intrinsics of this shape. The function receives the runtime
/// context [`Vm`] (heap, console, ...) and the call arguments in declaration
/// order, and returns the method's result (`None` for `void`) or a [`Trap`]. It is
/// documented as a shared seam in `docs/COORDINATION.md`.
pub type IntrinsicFn = fn(&mut Vm, &Module, &[Value]) -> Result<Option<Value>, Trap>;

/// A callable method: either managed CIL the interpreter executes, or a native
/// intrinsic it invokes directly.
#[derive(Clone)]
pub enum Method {
    /// Managed CIL: the decoded body and its argument count.
    Managed {
        /// The method's decoded CIL body.
        body: MethodBodyImage,
        /// How many arguments the method takes (a resolved signature replaces this
        /// once `lamella-metadata` is wired in).
        arg_count: u16,
    },
    /// A native intrinsic implemented in Rust.
    Intrinsic {
        /// The Rust implementation.
        func: IntrinsicFn,
        /// How many arguments it takes from the caller's stack.
        arg_count: u16,
    },
}

impl Method {
    /// How many arguments this method takes from the caller's evaluation stack.
    #[must_use]
    pub fn arg_count(&self) -> u16 {
        match self {
            Method::Managed { arg_count, .. } | Method::Intrinsic { arg_count, .. } => *arg_count,
        }
    }
}

/// A collection of methods, the call tokens that name them, and the strings
/// `ldstr` loads.
#[derive(Clone, Default)]
pub struct Module {
    methods: Vec<Method>,
    by_token: BTreeMap<u32, MethodId>,
    strings: BTreeMap<u32, Box<[u16]>>,
    /// Declared reference types, indexed by [`TypeId`].
    types: Vec<TypeInfo>,
    /// A method's declaring type (so `newobj` can find the type to instantiate).
    method_type: BTreeMap<MethodId, TypeId>,
    /// An `ldfld`/`stfld` field token mapped to its instance-field slot.
    field_slots: BTreeMap<u32, u32>,
    /// An instance-field token mapped to its declaring type, so a `stfld` through a
    /// managed pointer can size the value-type instance it materializes.
    field_types: BTreeMap<u32, TypeId>,
    /// A `newarr` element-type token mapped to its elements' zero value.
    array_defaults: BTreeMap<u32, Value>,
    /// A virtual method's vtable slot (only virtual methods appear).
    method_slots: BTreeMap<MethodId, u32>,
    /// A static field token mapped to its storage slot in the [`crate::interp::Vm`].
    static_fields: BTreeMap<u32, usize>,
    /// The zero value of each static field, indexed by storage slot.
    static_defaults: Vec<Value>,
    /// The static constructors (`.cctor`), to run before the entry point.
    static_ctors: Vec<MethodId>,
    /// A `TypeDef` token mapped to its [`TypeId`] (for `castclass` / `isinst`).
    type_tokens: BTreeMap<u32, TypeId>,
    /// A `callvirt` token mapped to its target's `(signature key, arg count)`, for
    /// dispatching interface / abstract methods whose target has no vtable slot (and
    /// may have no resolvable body).
    call_targets: BTreeMap<u32, (String, u16)>,
    /// `newobj` tokens that construct a delegate (a delegate type's `.ctor`): instead
    /// of running a constructor, the (target, method) on the stack become a delegate.
    delegate_ctors: BTreeSet<u32>,
    /// A delegate type's `Invoke` token mapped to its parameter count, so `callvirt` on
    /// it calls the delegate's bound method with the bound target.
    delegate_invokes: BTreeMap<u32, u16>,
    /// A value/reference type's `Finalize` method (a destructor), if it declares one --
    /// the finalizer the collector runs when an instance becomes unreachable. Kept
    /// unconditionally (it is tiny); the finalization machinery that consumes it is
    /// behind the `finalizers` feature.
    finalizers: BTreeMap<TypeId, MethodId>,
    /// Each enum type's constants (keyed by its `TypeDef` token): the underlying integer
    /// value mapped to the constant name, so `Enum.ToString` can render the name.
    enum_constants: BTreeMap<u32, BTreeMap<i64, String>>,
    /// Tokens of enum types whose underlying type is 64-bit (long / ulong), so `Enum.Parse`
    /// boxes their values as int64 to match the declared type.
    enum_wide: BTreeSet<u32>,
    /// `newobj` tokens that construct a multi-dimensional array (an array TypeSpec's
    /// `.ctor`), mapped to the array's rank -- newobj allocates from that many lengths.
    md_array_ctors: BTreeMap<u32, u16>,
    /// Debug names keyed by method id: the qualified method name and the argument names
    /// in CIL slot order (`this` first for an instance method). Empty unless a loader
    /// records them; the debugger surfaces them on frames and in the arguments view.
    method_debug: BTreeMap<MethodId, MethodDebug>,
}

/// Debug display names for one method: its qualified name and its argument names in CIL
/// slot order (`this` first for an instance method), recorded by a loader for the debugger.
#[derive(Clone)]
struct MethodDebug {
    name: String,
    args: Vec<String>,
}

impl Module {
    /// Creates an empty module.
    #[must_use]
    pub fn new() -> Module {
        Module::default()
    }

    /// Adds a managed method and returns its [`MethodId`].
    pub fn add_method(&mut self, body: MethodBodyImage, arg_count: u16) -> MethodId {
        self.push(Method::Managed { body, arg_count })
    }

    /// Adds a native intrinsic and returns its [`MethodId`].
    pub fn add_intrinsic(&mut self, func: IntrinsicFn, arg_count: u16) -> MethodId {
        self.push(Method::Intrinsic { func, arg_count })
    }

    fn push(&mut self, method: Method) -> MethodId {
        let id = self.methods.len() as MethodId;
        self.methods.push(method);
        id
    }

    /// Binds a `call` token to the method it resolves to (standing in for
    /// metadata's `MethodDef`/`MemberRef` resolution).
    pub fn bind_token(&mut self, token: Token, method: MethodId) {
        self.by_token.insert(token.0, method);
    }

    /// Binds an `ldstr` token to the UTF-16 string it loads (standing in for the
    /// `#US` user-string heap).
    pub fn bind_string(&mut self, token: Token, chars: &[u16]) {
        self.strings.insert(token.0, chars.into());
    }

    /// The method a `call` token resolves to, if any.
    #[must_use]
    pub fn resolve(&self, token: Token) -> Option<MethodId> {
        self.by_token.get(&token.0).copied()
    }

    /// The string an `ldstr` token loads, if any.
    #[must_use]
    pub fn resolve_string(&self, token: Token) -> Option<&[u16]> {
        self.strings.get(&token.0).map(AsRef::as_ref)
    }

    /// The method with the given id, if it exists.
    #[must_use]
    pub fn method(&self, id: MethodId) -> Option<&Method> {
        self.methods.get(id as usize)
    }

    /// Records debug names for method `id`: its qualified name (e.g. `Program.Fact`) and
    /// its argument names in CIL slot order (`this` first for an instance method). The
    /// debugger surfaces these on stack frames and in the arguments view.
    pub fn set_method_debug(&mut self, id: MethodId, name: String, args: Vec<String>) {
        self.method_debug.insert(id, MethodDebug { name, args });
    }

    /// The qualified display name recorded for method `id`, if any.
    #[must_use]
    pub fn method_name(&self, id: MethodId) -> Option<&str> {
        self.method_debug.get(&id).map(|debug| debug.name.as_str())
    }

    /// The name recorded for argument slot `index` of method `id`, if present and
    /// non-empty (`this` is slot 0 of an instance method).
    #[must_use]
    pub fn arg_name(&self, id: MethodId, index: usize) -> Option<&str> {
        self.method_debug
            .get(&id)
            .and_then(|debug| debug.args.get(index))
            .map(String::as_str)
            .filter(|name| !name.is_empty())
    }

    /// Adds a declared reference type with the zero values of its instance fields
    /// (one per slot, declaration order) and returns its [`TypeId`].
    pub fn add_type(&mut self, field_defaults: Vec<Value>) -> TypeId {
        let id = self.types.len() as TypeId;
        self.types.push(TypeInfo {
            field_defaults: field_defaults.into_boxed_slice(),
            vtable: Box::default(),
            base: None,
            sig_methods: BTreeMap::new(),
        });
        id
    }

    /// Records that `method` is declared by `type_id`, so `newobj` can resolve a
    /// constructor to the type it instantiates.
    pub fn set_method_type(&mut self, method: MethodId, type_id: TypeId) {
        self.method_type.insert(method, type_id);
    }

    /// Binds an `ldfld`/`stfld` field token to its instance-field slot.
    pub fn bind_field(&mut self, token: Token, slot: u32) {
        self.field_slots.insert(token.0, slot);
    }

    /// Records the declaring type of an instance-field token.
    pub fn bind_field_type(&mut self, token: Token, type_id: TypeId) {
        self.field_types.insert(token.0, type_id);
    }

    /// The declaring type of a field token, if recorded.
    #[must_use]
    pub fn field_type(&self, token: Token) -> Option<TypeId> {
        self.field_types.get(&token.0).copied()
    }

    /// The declaring type of `method`, if recorded.
    #[must_use]
    pub fn method_type(&self, method: MethodId) -> Option<TypeId> {
        self.method_type.get(&method).copied()
    }

    /// The instance-field slot a field token names, if bound.
    #[must_use]
    pub fn field_slot(&self, token: Token) -> Option<u32> {
        self.field_slots.get(&token.0).copied()
    }

    /// The zero values of `type_id`'s instance fields, for allocating an instance.
    #[must_use]
    pub fn type_field_defaults(&self, type_id: TypeId) -> Option<&[Value]> {
        self.types
            .get(type_id as usize)
            .map(|info| &info.field_defaults[..])
    }

    /// Replaces `type_id`'s full instance-field layout (base fields first, then own),
    /// computed once every base type is known.
    pub fn set_type_field_defaults(&mut self, type_id: TypeId, field_defaults: Vec<Value>) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.field_defaults = field_defaults.into_boxed_slice();
        }
    }

    /// Binds a `newarr` element-type token to the zero value its elements take.
    pub fn bind_array_default(&mut self, token: Token, default: Value) {
        self.array_defaults.insert(token.0, default);
    }

    /// The zero value of a `newarr` element-type token's elements, if bound.
    #[must_use]
    pub fn array_default(&self, token: Token) -> Option<Value> {
        self.array_defaults.get(&token.0).cloned()
    }

    /// Sets `type_id`'s virtual method table (slot -> implementation).
    pub fn set_vtable(&mut self, type_id: TypeId, vtable: Vec<MethodId>) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.vtable = vtable.into_boxed_slice();
        }
    }

    /// Records a virtual method's vtable slot (for `callvirt` dispatch).
    pub fn bind_method_slot(&mut self, method: MethodId, slot: u32) {
        self.method_slots.insert(method, slot);
    }

    /// The vtable slot of `method`, if it is virtual.
    #[must_use]
    pub fn method_slot(&self, method: MethodId) -> Option<u32> {
        self.method_slots.get(&method).copied()
    }

    /// The implementation at `slot` in `type_id`'s vtable, if present -- the
    /// `callvirt` target for a `this` of that runtime type.
    #[must_use]
    pub fn vtable_entry(&self, type_id: TypeId, slot: u32) -> Option<MethodId> {
        self.types
            .get(type_id as usize)?
            .vtable
            .get(slot as usize)
            .copied()
    }

    /// Registers a static field with its zero value, assigning the next storage slot.
    pub fn bind_static_field(&mut self, token: Token, default: Value) {
        let slot = self.static_defaults.len();
        self.static_defaults.push(default);
        self.static_fields.insert(token.0, slot);
    }

    /// The storage slot of a static field token, if registered.
    #[must_use]
    pub fn static_field_slot(&self, token: Token) -> Option<usize> {
        self.static_fields.get(&token.0).copied()
    }

    /// The zero values of all static fields, indexed by slot (to initialize storage).
    #[must_use]
    pub fn static_field_defaults(&self) -> &[Value] {
        &self.static_defaults
    }

    /// Records a static constructor (`.cctor`) to run before the entry point.
    pub fn add_static_ctor(&mut self, method: MethodId) {
        self.static_ctors.push(method);
    }

    /// Records `type_id`'s `Finalize` method (its destructor), so allocating an instance
    /// registers it for finalization and the collector can run it on reclamation.
    pub fn set_finalizer(&mut self, type_id: TypeId, method: MethodId) {
        self.finalizers.insert(type_id, method);
    }

    /// The `Finalize` method `type_id` declares, if any.
    #[must_use]
    pub fn finalizer_of(&self, type_id: TypeId) -> Option<MethodId> {
        self.finalizers.get(&type_id).copied()
    }

    /// Records that the enum type `token` has a constant `name` with underlying `value`.
    pub fn set_enum_constant(&mut self, token: u32, value: i64, name: String) {
        self.enum_constants
            .entry(token)
            .or_default()
            .insert(value, name);
    }

    /// The name of the constant with underlying `value` in the enum type `token`, if any.
    #[must_use]
    pub fn enum_value_name(&self, token: u32, value: i64) -> Option<&str> {
        self.enum_constants
            .get(&token)
            .and_then(|constants| constants.get(&value))
            .map(String::as_str)
    }

    /// The underlying value of the constant named `name` in the enum type `token`, if any
    /// -- the reverse of [`Self::enum_value_name`], for `Enum.Parse`.
    #[must_use]
    pub fn enum_value_by_name(&self, token: u32, name: &str, ignore_case: bool) -> Option<i64> {
        self.enum_constants
            .get(&token)?
            .iter()
            .find_map(|(value, constant)| {
                let matched = if ignore_case {
                    constant.eq_ignore_ascii_case(name)
                } else {
                    constant == name
                };
                matched.then_some(*value)
            })
    }

    /// Records that the enum type `token` has a 64-bit underlying type (long / ulong).
    pub fn set_enum_wide(&mut self, token: u32) {
        self.enum_wide.insert(token);
    }

    /// Whether the enum type `token` has a 64-bit underlying type.
    #[must_use]
    pub fn enum_is_wide(&self, token: u32) -> bool {
        self.enum_wide.contains(&token)
    }

    /// The static constructors, in the order to run them.
    #[must_use]
    pub fn static_ctors(&self) -> &[MethodId] {
        &self.static_ctors
    }

    /// Records `type_id`'s base type, for subtype checks.
    pub fn set_type_base(&mut self, type_id: TypeId, base: Option<TypeId>) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.base = base;
        }
    }

    /// Binds a `TypeDef` token to its [`TypeId`] (for `castclass` / `isinst`).
    pub fn bind_type_token(&mut self, token: Token, type_id: TypeId) {
        self.type_tokens.insert(token.0, type_id);
    }

    /// The [`TypeId`] a type token names, if it is a same-module type.
    #[must_use]
    pub fn type_id_of(&self, token: Token) -> Option<TypeId> {
        self.type_tokens.get(&token.0).copied()
    }

    /// Whether `sub` is `ancestor` or a type derived from it, walking the base chain.
    /// Bounded by the type count so malformed cyclic metadata cannot loop forever.
    #[must_use]
    pub fn is_subtype(&self, sub: TypeId, ancestor: TypeId) -> bool {
        let mut current = Some(sub);
        for _ in 0..=self.types.len() {
            match current {
                Some(type_id) if type_id == ancestor => return true,
                Some(type_id) => current = self.types.get(type_id as usize).and_then(|i| i.base),
                None => break,
            }
        }
        false
    }

    /// Records `type_id`'s methods keyed by signature, for interface / abstract
    /// dispatch (the map should include inherited methods).
    pub fn set_sig_methods(&mut self, type_id: TypeId, methods: BTreeMap<String, MethodId>) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.sig_methods = methods;
        }
    }

    /// The method of `type_id` matching `sig_key` -- the `callvirt` target for an
    /// interface or abstract method on a `this` of that runtime type.
    #[must_use]
    pub fn sig_dispatch(&self, type_id: TypeId, sig_key: &str) -> Option<MethodId> {
        self.types
            .get(type_id as usize)?
            .sig_methods
            .get(sig_key)
            .copied()
    }

    /// Records a `callvirt` token's target signature key and argument count, for
    /// dispatching a method with no vtable slot / no resolvable body.
    pub fn bind_call_target(&mut self, token: Token, sig_key: String, arg_count: u16) {
        self.call_targets.insert(token.0, (sig_key, arg_count));
    }

    /// A `callvirt` token's target signature key and argument count, if recorded.
    #[must_use]
    pub fn call_target(&self, token: Token) -> Option<(&str, u16)> {
        self.call_targets
            .get(&token.0)
            .map(|(key, count)| (key.as_str(), *count))
    }

    /// Marks `token` as a delegate constructor, so `newobj` on it builds a delegate.
    pub fn mark_delegate_ctor(&mut self, token: Token) {
        self.delegate_ctors.insert(token.0);
    }

    /// Whether `token` constructs a delegate.
    #[must_use]
    pub fn is_delegate_ctor(&self, token: Token) -> bool {
        self.delegate_ctors.contains(&token.0)
    }

    /// Marks `token` as a multi-dimensional array constructor of the given `rank`, so
    /// `newobj` allocates the array from that many length arguments.
    pub fn mark_md_array_ctor(&mut self, token: Token, rank: u16) {
        self.md_array_ctors.insert(token.0, rank);
    }

    /// The rank of the multi-dimensional array `token` constructs, if it constructs one.
    #[must_use]
    pub fn md_array_ctor_rank(&self, token: Token) -> Option<u16> {
        self.md_array_ctors.get(&token.0).copied()
    }

    /// Marks `token` as a delegate `Invoke` taking `param_count` parameters.
    pub fn mark_delegate_invoke(&mut self, token: Token, param_count: u16) {
        self.delegate_invokes.insert(token.0, param_count);
    }

    /// The parameter count of the delegate `Invoke` named by `token`, if it is one.
    #[must_use]
    pub fn delegate_invoke(&self, token: Token) -> Option<u16> {
        self.delegate_invokes.get(&token.0).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_debug_names_round_trip() {
        let mut module = Module::new();
        module.set_method_debug(
            0,
            String::from("Program.Twice"),
            Vec::from([String::from("this"), String::new(), String::from("count")]),
        );
        assert_eq!(module.method_name(0), Some("Program.Twice"));
        assert_eq!(module.arg_name(0, 0), Some("this"));
        assert_eq!(module.arg_name(0, 1), None);
        assert_eq!(module.arg_name(0, 2), Some("count"));
        assert_eq!(module.arg_name(0, 3), None);
        assert_eq!(module.method_name(1), None);
        assert_eq!(module.arg_name(1, 0), None);
    }
}
