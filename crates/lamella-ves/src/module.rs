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
    /// The interfaces this type directly implements, as module [`TypeId`]s (an interface
    /// may be cross-assembly, e.g. a program class implementing `[corlib]System.IComparable`).
    /// `castclass` / `isinst` to an interface succeed when the runtime type -- or any type on
    /// its base chain -- lists the target here. Empty for a type implementing no resolvable
    /// interfaces.
    interfaces: Vec<TypeId>,
    /// This type's virtual / interface-implementing methods (including inherited)
    /// keyed by a signature key (name + parameter types), for interface and
    /// abstract-method dispatch where the static target carries no vtable slot.
    sig_methods: BTreeMap<String, MethodId>,
    /// Whether this type is a value type (a struct / primitive, extending `System.ValueType`
    /// or `System.Enum`). A `callvirt` (or `constrained. callvirt`) that dispatches to a value
    /// type's OWN instance method on a boxed receiver auto-unboxes `this` to a managed pointer
    /// into the box (III.4.2), so the body reads the value through `this` (`ldarg.0; ldind.*`).
    value_type: bool,
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
    /// The reverse of `type_tokens`: a [`TypeId`] mapped to its declaring type's asm-folded
    /// `TypeDef` token (its `Type` handle), so `Object.GetType()` on a reference instance can
    /// hand back the same handle `typeof` / `Type.Name` use.
    type_handles: BTreeMap<TypeId, u32>,
    /// A `callvirt` token mapped to its target's `(signature key, arg count)`, for
    /// dispatching interface / abstract methods whose target has no vtable slot (and
    /// may have no resolvable body).
    call_targets: BTreeMap<u32, (String, u16)>,
    /// Explicit interface implementations (II.22.27 `MethodImpl`): the implementing body
    /// for a `(declaring TypeId, overridden method handle)` pair. The handle is the
    /// asm-folded token of the interface/virtual method named at the `callvirt` site (the
    /// `MethodImpl.MethodDeclaration`). An explicit body (`int IA.Value()`) is private and
    /// named after the interface, so it is reachable only through this map -- a plain
    /// signature match never finds it and cannot tell `IA.Value` from `IB.Value`.
    explicit_overrides: BTreeMap<(TypeId, u32), MethodId>,
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
    /// `newobj` tokens that construct a `System.Text.StringBuilder`, mapped to the
    /// constructor's parameter count -- newobj allocates a builder (seeded from a string arg).
    string_builder_ctors: BTreeMap<u32, u16>,
    /// `newobj` tokens that construct a `System.Collections.ArrayList`, mapped to the
    /// constructor's parameter count -- newobj allocates an empty array-backed list.
    list_ctors: BTreeMap<u32, u16>,
    /// Debug names keyed by method id: the qualified method name and the argument names
    /// in CIL slot order (`this` first for an instance method). Empty unless a loader
    /// records them; the debugger surfaces them on frames and in the arguments view.
    method_debug: BTreeMap<MethodId, MethodDebug>,
    /// Each method's owning assembly id, indexed by [`MethodId`] -- the assembly whose
    /// token space the method's CIL resolves against. Single-assembly loads are all 0.
    method_asm: Vec<u8>,
    /// The [`TypeId`] of `System.String`, if a loaded assembly defined it. It backs virtual
    /// dispatch on a heap string: a `callvirt` on a string receiver dispatches through this
    /// type's vtable (reaching String's Equals / GetHashCode / ToString overrides) since a
    /// heap string is not a field-carrying instance and so has no per-object type id.
    string_type_id: Option<u32>,
    /// The canonical (asm-folded) `TypeDef` token of each primitive value type a corlib
    /// defines (`System.Int32`, `System.Int64`, `System.Single`/`Double`, `System.IntPtr`),
    /// keyed by the evaluation-stack [`Value`] kind that represents it. `System.Array.GetValue`
    /// boxes a value-type element with the matching token so the box carries a real value-type
    /// identity (an `(IComparable)element` cast then resolves the value type's interfaces); a
    /// raw `box` opcode already stamps the operand token, so this is only the untyped
    /// element-accessor path. The widening of `bool`/`char`/`int16`/... onto `Value::Int32`
    /// means an `Int32`-kind element boxes as `System.Int32` regardless of the narrower CTS
    /// element type -- precise enough for the comparison/interface surface and matching the
    /// element type of the `int[]` the accessor is exercised on.
    primitive_int32_token: Option<u32>,
    primitive_int64_token: Option<u32>,
    primitive_native_int_token: Option<u32>,
    #[cfg(feature = "float")]
    primitive_float_token: Option<u32>,
    /// `newobj` tokens whose declaring type is a value type (a struct): such a `newobj`
    /// must construct a struct VALUE in place (passing `this` by managed pointer to the
    /// ctor) and leave that value -- not a heap instance -- on the stack.
    value_type_ctors: BTreeSet<u32>,
    /// The raw little-endian initializer bytes of a field with an RVA data blob, keyed by
    /// the field token, for `RuntimeHelpers.InitializeArray` (a `T[] a = {...}` of a
    /// constant primitive array). Sized to the declaring blob; the interpreter slices it
    /// per the array's element width.
    field_rva_data: BTreeMap<u32, Box<[u8]>>,
    /// Type tokens (in `castclass` / `isinst` / `box` operands) classified by their external
    /// identity -- `System.Object` (a universal catch-all target) or `System.String` -- so a
    /// type test on a boxed value or a heap string is precise rather than unverified.
    object_type_tokens: BTreeSet<u32>,
    string_type_tokens: BTreeSet<u32>,
    /// The simple (unqualified) name of each `ldtoken`'d type, keyed by its asm-folded token,
    /// for `System.Type.get_Name` (a `typeof(T).Name`).
    type_names: BTreeMap<u32, String>,
    /// Each type's vtable slots as `(signature key, implementation)` pairs, in slot order,
    /// keyed by [`TypeId`] -- the loader's view of the vtable it also flattened into the
    /// MethodId-only [`TypeInfo::vtable`]. A type whose base is reached by a cross-assembly
    /// `TypeRef` (e.g. a corlib's own type extends a previously loaded `[mscorlib]System.Object`)
    /// seeds its vtable from its base's slots here, so the base's layout (Object's Equals=0 /
    /// GetHashCode=1 / ToString=2) is inherited and the derived type's own newslot virtuals
    /// append AFTER it -- matching what same-assembly single inheritance already does. Populated
    /// only while loading; the interpreter dispatches through the flattened `vtable`.
    vtable_slot_keys: BTreeMap<TypeId, Vec<(String, MethodId)>>,
    /// The byte size of a type named by a `sizeof` operand token (a value type's computed
    /// layout size, or a primitive's fixed width), keyed by its asm-folded token. The one
    /// fact `sizeof` (III.4.25) pushes; struct sizes come from the shared
    /// `lamella_metadata::value_type_layout`, so the interpreter, AOT, and GC agree.
    type_sizes: BTreeMap<u32, u32>,
}

/// Folds the assembly id into a token key. A token's high byte is its table tag; the largest is
/// `#US` (user strings) = 0x70, which already has bit 30 set -- so only BIT 31 is free across every
/// token kind, and it carries the assembly id for the two-assembly case (corlib = 0, program = 1).
/// The earlier "top two bits" scheme collided `#US` (ldstr) tokens between the corlib and the
/// program (e.g. the corlib's "False" with the program's "42"). Widen this to a u64
/// (`asm << 32 | token`) key if more than two assemblies ever load together.
pub(crate) fn asm_key(asm: u8, token: u32) -> u32 {
    debug_assert!(asm < 2, "asm_key carries one bit; widen to a u64 key for >2 assemblies");
    token | ((asm as u32) << 31)
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

    /// Adds a managed method belonging to assembly `asm` and returns its [`MethodId`].
    pub fn add_method(&mut self, asm: u8, body: MethodBodyImage, arg_count: u16) -> MethodId {
        self.push(asm, Method::Managed { body, arg_count })
    }

    /// Adds a native intrinsic belonging to assembly `asm` and returns its [`MethodId`].
    pub fn add_intrinsic(&mut self, asm: u8, func: IntrinsicFn, arg_count: u16) -> MethodId {
        self.push(asm, Method::Intrinsic { func, arg_count })
    }

    fn push(&mut self, asm: u8, method: Method) -> MethodId {
        let id = self.methods.len() as MethodId;
        self.methods.push(method);
        self.method_asm.push(asm);
        id
    }

    /// The assembly id that owns `method` (the token space its CIL resolves against);
    /// 0 for any method not explicitly recorded.
    #[must_use]
    pub fn method_asm(&self, id: MethodId) -> u8 {
        self.method_asm.get(id as usize).copied().unwrap_or(0)
    }

    /// Binds a `call` token in assembly `asm` to the method it resolves to (standing in
    /// for metadata's `MethodDef`/`MemberRef` resolution).
    pub fn bind_token(&mut self, asm: u8, token: Token, method: MethodId) {
        self.by_token.insert(asm_key(asm, token.0), method);
    }

    /// Binds an `ldstr` token in assembly `asm` to the UTF-16 string it loads (standing
    /// in for the `#US` user-string heap).
    pub fn bind_string(&mut self, asm: u8, token: Token, chars: &[u16]) {
        self.strings.insert(asm_key(asm, token.0), chars.into());
    }

    /// The method a `call` token in assembly `asm` resolves to, if any.
    #[must_use]
    pub fn resolve(&self, asm: u8, token: Token) -> Option<MethodId> {
        self.by_token.get(&asm_key(asm, token.0)).copied()
    }

    /// The string an `ldstr` token in assembly `asm` loads, if any.
    #[must_use]
    pub fn resolve_string(&self, asm: u8, token: Token) -> Option<&[u16]> {
        self.strings.get(&asm_key(asm, token.0)).map(AsRef::as_ref)
    }

    /// The method with the given id, if it exists.
    #[must_use]
    pub fn method(&self, id: MethodId) -> Option<&Method> {
        self.methods.get(id as usize)
    }

    /// The number of declared reference types in the module -- the global [`TypeId`]
    /// offset at which the next assembly's first type will land in a multi-assembly load.
    #[must_use]
    pub fn type_count(&self) -> usize {
        self.types.len()
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
            interfaces: Vec::new(),
            sig_methods: BTreeMap::new(),
            value_type: false,
        });
        id
    }

    /// Records that `method` is declared by `type_id`, so `newobj` can resolve a
    /// constructor to the type it instantiates.
    pub fn set_method_type(&mut self, method: MethodId, type_id: TypeId) {
        self.method_type.insert(method, type_id);
    }

    /// Binds an `ldfld`/`stfld` field token in assembly `asm` to its instance-field slot.
    pub fn bind_field(&mut self, asm: u8, token: Token, slot: u32) {
        self.field_slots.insert(asm_key(asm, token.0), slot);
    }

    /// Records the declaring type of an instance-field token in assembly `asm`.
    pub fn bind_field_type(&mut self, asm: u8, token: Token, type_id: TypeId) {
        self.field_types.insert(asm_key(asm, token.0), type_id);
    }

    /// The declaring type of a field token in assembly `asm`, if recorded.
    #[must_use]
    pub fn field_type(&self, asm: u8, token: Token) -> Option<TypeId> {
        self.field_types.get(&asm_key(asm, token.0)).copied()
    }

    /// The declaring type of `method`, if recorded.
    #[must_use]
    pub fn method_type(&self, method: MethodId) -> Option<TypeId> {
        self.method_type.get(&method).copied()
    }

    /// The instance-field slot a field token in assembly `asm` names, if bound.
    #[must_use]
    pub fn field_slot(&self, asm: u8, token: Token) -> Option<u32> {
        self.field_slots.get(&asm_key(asm, token.0)).copied()
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

    /// Binds a `newarr` element-type token in assembly `asm` to the zero value its
    /// elements take.
    pub fn bind_array_default(&mut self, asm: u8, token: Token, default: Value) {
        self.array_defaults.insert(asm_key(asm, token.0), default);
    }

    /// The zero value of a `newarr` element-type token's elements in assembly `asm`, if bound.
    #[must_use]
    pub fn array_default(&self, asm: u8, token: Token) -> Option<Value> {
        self.array_defaults.get(&asm_key(asm, token.0)).cloned()
    }

    /// Sets `type_id`'s virtual method table (slot -> implementation).
    pub fn set_vtable(&mut self, type_id: TypeId, vtable: Vec<MethodId>) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.vtable = vtable.into_boxed_slice();
        }
    }

    /// Records `type_id`'s vtable as `(signature key, implementation)` slots in slot order, so a
    /// later type whose base is reached by a cross-assembly `TypeRef` can inherit this layout
    /// (the loader seeds the derived vtable from these slots, matching overrides by key).
    pub fn set_vtable_slot_keys(&mut self, type_id: TypeId, slots: Vec<(String, MethodId)>) {
        self.vtable_slot_keys.insert(type_id, slots);
    }

    /// `type_id`'s vtable slots as `(signature key, implementation)` pairs in slot order, if
    /// recorded -- the seed a derived type with a cross-assembly base inherits.
    #[must_use]
    pub fn vtable_slot_keys(&self, type_id: TypeId) -> Option<&[(String, MethodId)]> {
        self.vtable_slot_keys.get(&type_id).map(Vec::as_slice)
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

    /// Registers a static field in assembly `asm` with its zero value, assigning the next
    /// storage slot.
    pub fn bind_static_field(&mut self, asm: u8, token: Token, default: Value) {
        let slot = self.static_defaults.len();
        self.static_defaults.push(default);
        self.static_fields.insert(asm_key(asm, token.0), slot);
    }

    /// Binds a static-field reference token in assembly `asm` to an EXISTING storage slot,
    /// without allocating a new one. This is the cross-assembly case: a program references a
    /// corlib static field by a `MemberRef` token, which the loader resolves by name to the
    /// slot [`Self::bind_static_field`] already assigned under the corlib's own `FieldDef`
    /// token, so the two tokens share one storage slot (the corlib `.cctor` writes it, the
    /// program reads it).
    pub fn bind_static_field_ref(&mut self, asm: u8, token: Token, slot: usize) {
        self.static_fields.insert(asm_key(asm, token.0), slot);
    }

    /// The storage slot of a static field token in assembly `asm`, if registered.
    #[must_use]
    pub fn static_field_slot(&self, asm: u8, token: Token) -> Option<usize> {
        self.static_fields.get(&asm_key(asm, token.0)).copied()
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

    /// Records that the enum type `token` in assembly `asm` has a constant `name` with
    /// underlying `value`.
    pub fn set_enum_constant(&mut self, asm: u8, token: u32, value: i64, name: String) {
        self.enum_constants
            .entry(asm_key(asm, token))
            .or_default()
            .insert(value, name);
    }

    /// The name of the constant with underlying `value` in the enum type `token` of
    /// assembly `asm`, if any.
    #[must_use]
    pub fn enum_value_name(&self, asm: u8, token: u32, value: i64) -> Option<&str> {
        self.enum_constants
            .get(&asm_key(asm, token))
            .and_then(|constants| constants.get(&value))
            .map(String::as_str)
    }

    /// The underlying value of the constant named `name` in the enum type `token` of
    /// assembly `asm`, if any -- the reverse of [`Self::enum_value_name`], for `Enum.Parse`.
    #[must_use]
    pub fn enum_value_by_name(
        &self,
        asm: u8,
        token: u32,
        name: &str,
        ignore_case: bool,
    ) -> Option<i64> {
        self.enum_constants
            .get(&asm_key(asm, token))?
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

    /// Records that the enum type `token` in assembly `asm` has a 64-bit underlying type
    /// (long / ulong).
    pub fn set_enum_wide(&mut self, asm: u8, token: u32) {
        self.enum_wide.insert(asm_key(asm, token));
    }

    /// Whether the enum type `token` in assembly `asm` has a 64-bit underlying type.
    #[must_use]
    pub fn enum_is_wide(&self, asm: u8, token: u32) -> bool {
        self.enum_wide.contains(&asm_key(asm, token))
    }

    /// Records the [`TypeId`] of `System.String`, so a `callvirt` on a heap string can supply
    /// it as the receiver's runtime type and dispatch through String's vtable.
    pub fn set_string_type_id(&mut self, id: u32) {
        self.string_type_id = Some(id);
    }

    /// The [`TypeId`] of `System.String`, if a loaded assembly defined it.
    #[must_use]
    pub fn string_type_id(&self) -> Option<u32> {
        self.string_type_id
    }

    /// Records the canonical `TypeDef` `token` of a primitive value type a corlib defines (in
    /// assembly `asm`), keyed by the [`Value`] kind it loads as, so `System.Array.GetValue` can
    /// box an element with its real value-type identity. The token is asm-folded to match the
    /// key `type_id_by_handle` resolves a box's tag through. `kind` is a zero value of the
    /// representative kind (`Value::Int32(0)` for `System.Int32`, etc.); a non-primitive kind is
    /// ignored.
    pub fn set_primitive_type_token(&mut self, asm: u8, token: Token, kind: &Value) {
        let handle = asm_key(asm, token.0);
        match kind {
            Value::Int32(_) => self.primitive_int32_token = Some(handle),
            Value::Int64(_) => self.primitive_int64_token = Some(handle),
            Value::NativeInt(_) => self.primitive_native_int_token = Some(handle),
            #[cfg(feature = "float")]
            Value::Float(_) => self.primitive_float_token = Some(handle),
            _ => {}
        }
    }

    /// The canonical (asm-folded) `TypeDef` token of the primitive value type the boxed
    /// `value`'s [`Value`] kind represents, if a corlib defined it -- the type tag
    /// `System.Array.GetValue` stamps on the box it returns for a value-type element. `None`
    /// when no corlib registered the kind (then the element boxes with a placeholder tag, as
    /// before).
    #[must_use]
    pub fn primitive_type_token(&self, value: &Value) -> Option<u32> {
        match value {
            Value::Int32(_) => self.primitive_int32_token,
            Value::Int64(_) => self.primitive_int64_token,
            Value::NativeInt(_) => self.primitive_native_int_token,
            #[cfg(feature = "float")]
            Value::Float(_) => self.primitive_float_token,
            _ => None,
        }
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

    /// Marks `type_id` as a value type (a struct / primitive), so a `callvirt` to one of its
    /// own instance methods on a boxed receiver auto-unboxes `this` to a managed pointer.
    pub fn set_type_is_value_type(&mut self, type_id: TypeId, value_type: bool) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.value_type = value_type;
        }
    }

    /// Whether `method`'s declaring type is a value type. A `callvirt` that resolves to such a
    /// method on a boxed receiver must hand `this` as a managed pointer into the box (III.4.2),
    /// not the box reference itself, so the body's `ldarg.0; ldind.*` reads the boxed value.
    #[must_use]
    pub fn method_declares_value_type(&self, method: MethodId) -> bool {
        self.method_type(method)
            .and_then(|type_id| self.types.get(type_id as usize))
            .is_some_and(|info| info.value_type)
    }

    /// Records the interfaces `type_id` directly implements (resolved to module [`TypeId`]s),
    /// so `castclass` / `isinst` to an interface can test the implements relation.
    pub fn set_type_interfaces(&mut self, type_id: TypeId, interfaces: Vec<TypeId>) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.interfaces = interfaces;
        }
    }

    /// Binds a `TypeDef` token in assembly `asm` to its [`TypeId`] (for `castclass` / `isinst`),
    /// and records the reverse handle (the first token bound to a type id) so
    /// `Object.GetType()` can recover a reference instance's `Type` handle.
    pub fn bind_type_token(&mut self, asm: u8, token: Token, type_id: TypeId) {
        let handle = asm_key(asm, token.0);
        self.type_tokens.insert(handle, type_id);
        self.type_handles.entry(type_id).or_insert(handle);
    }

    /// The asm-folded `TypeDef` token (the `Type` handle) of `type_id`, if one was bound --
    /// what `Object.GetType()` hands back for a reference instance so a following `.Name`
    /// resolves through the same path `typeof(T).Name` uses.
    #[must_use]
    pub fn type_handle_of(&self, type_id: TypeId) -> Option<u32> {
        self.type_handles.get(&type_id).copied()
    }

    /// The [`TypeId`] a type token in assembly `asm` names, if it is a same-module type.
    #[must_use]
    pub fn type_id_of(&self, asm: u8, token: Token) -> Option<TypeId> {
        self.type_tokens.get(&asm_key(asm, token.0)).copied()
    }

    /// The [`TypeId`] an already-asm-folded type-token handle names, if it is a declared type.
    /// (A boxed value carries its value type's folded token; this maps it back to a [`TypeId`]
    /// so a cast on the box can consult the value type's implemented interfaces.)
    #[must_use]
    pub fn type_id_by_handle(&self, handle: u32) -> Option<TypeId> {
        self.type_tokens.get(&handle).copied()
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

    /// Whether `type_id` implements `interface_id` -- directly, or by inheriting an
    /// implementation from a base type. Walks `type_id`'s base chain (bounded by the type
    /// count, like [`Self::is_subtype`]); a type matches when it lists `interface_id` among
    /// the interfaces it implements (recorded by [`Self::set_type_interfaces`]).
    ///
    #[must_use]
    pub fn implements_interface(&self, type_id: TypeId, interface_id: TypeId) -> bool {
        let mut current = Some(type_id);
        for _ in 0..=self.types.len() {
            match current {
                Some(id) => {
                    let Some(info) = self.types.get(id as usize) else {
                        break;
                    };
                    if info.interfaces.contains(&interface_id) {
                        return true;
                    }
                    current = info.base;
                }
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

    /// Records a `callvirt` token's target signature key and argument count in assembly
    /// `asm`, for dispatching a method with no vtable slot / no resolvable body.
    pub fn bind_call_target(&mut self, asm: u8, token: Token, sig_key: String, arg_count: u16) {
        self.call_targets
            .insert(asm_key(asm, token.0), (sig_key, arg_count));
    }

    /// A `callvirt` token's target signature key and argument count in assembly `asm`, if
    /// recorded.
    #[must_use]
    pub fn call_target(&self, asm: u8, token: Token) -> Option<(&str, u16)> {
        self.call_targets
            .get(&asm_key(asm, token.0))
            .map(|(key, count)| (key.as_str(), *count))
    }

    /// Records an explicit interface implementation (II.22.27 `MethodImpl`): when `type_id`
    /// is the runtime type, a `callvirt` naming `decl_token` (the overridden interface /
    /// virtual method, in assembly `asm`) dispatches to `body`. The token is folded with its
    /// assembly the same way the `callvirt` operand is, so dispatch can look it up directly.
    pub fn add_explicit_override(
        &mut self,
        asm: u8,
        type_id: TypeId,
        decl_token: Token,
        body: MethodId,
    ) {
        self.explicit_overrides
            .insert((type_id, asm_key(asm, decl_token.0)), body);
    }

    /// The explicit-override body for a `callvirt` naming `decl_token` in assembly `asm` on a
    /// receiver of runtime type `type_id`, if one is recorded.
    #[must_use]
    pub fn explicit_override(&self, asm: u8, type_id: TypeId, decl_token: Token) -> Option<MethodId> {
        self.explicit_overrides
            .get(&(type_id, asm_key(asm, decl_token.0)))
            .copied()
    }

    /// Marks `token` in assembly `asm` as a delegate constructor, so `newobj` on it builds
    /// a delegate.
    pub fn mark_delegate_ctor(&mut self, asm: u8, token: Token) {
        self.delegate_ctors.insert(asm_key(asm, token.0));
    }

    /// Whether `token` in assembly `asm` constructs a delegate.
    #[must_use]
    pub fn is_delegate_ctor(&self, asm: u8, token: Token) -> bool {
        self.delegate_ctors.contains(&asm_key(asm, token.0))
    }

    /// Marks `token` in assembly `asm` as a multi-dimensional array constructor of the given
    /// `rank`, so `newobj` allocates the array from that many length arguments.
    pub fn mark_md_array_ctor(&mut self, asm: u8, token: Token, rank: u16) {
        self.md_array_ctors.insert(asm_key(asm, token.0), rank);
    }

    /// The rank of the multi-dimensional array `token` constructs in assembly `asm`, if it
    /// constructs one.
    #[must_use]
    pub fn md_array_ctor_rank(&self, asm: u8, token: Token) -> Option<u16> {
        self.md_array_ctors.get(&asm_key(asm, token.0)).copied()
    }

    /// Marks `token` in assembly `asm` as a `StringBuilder` constructor taking `params`
    /// parameters, so `newobj` allocates a builder instead of running a managed constructor.
    pub fn mark_string_builder_ctor(&mut self, asm: u8, token: Token, params: u16) {
        self.string_builder_ctors
            .insert(asm_key(asm, token.0), params);
    }

    /// The parameter count of the `StringBuilder` constructor `token` names in assembly
    /// `asm`, if it is one.
    #[must_use]
    pub fn string_builder_ctor_params(&self, asm: u8, token: Token) -> Option<u16> {
        self.string_builder_ctors
            .get(&asm_key(asm, token.0))
            .copied()
    }

    /// Marks `token` in assembly `asm` as an `ArrayList` constructor taking `params`
    /// parameters, so `newobj` allocates an empty list instead of running a managed constructor.
    pub fn mark_list_ctor(&mut self, asm: u8, token: Token, params: u16) {
        self.list_ctors.insert(asm_key(asm, token.0), params);
    }

    /// The parameter count of the `ArrayList` constructor `token` names in assembly `asm`,
    /// if it is one.
    #[must_use]
    pub fn list_ctor_params(&self, asm: u8, token: Token) -> Option<u16> {
        self.list_ctors.get(&asm_key(asm, token.0)).copied()
    }

    /// Marks `token` in assembly `asm` as a delegate `Invoke` taking `param_count` parameters.
    pub fn mark_delegate_invoke(&mut self, asm: u8, token: Token, param_count: u16) {
        self.delegate_invokes
            .insert(asm_key(asm, token.0), param_count);
    }

    /// The parameter count of the delegate `Invoke` named by `token` in assembly `asm`, if
    /// it is one.
    #[must_use]
    pub fn delegate_invoke(&self, asm: u8, token: Token) -> Option<u16> {
        self.delegate_invokes.get(&asm_key(asm, token.0)).copied()
    }

    /// Marks a `newobj` token in assembly `asm` as constructing a value type, so `newobj`
    /// builds a struct value in place rather than a heap instance.
    pub fn mark_value_type_ctor(&mut self, asm: u8, token: Token) {
        self.value_type_ctors.insert(asm_key(asm, token.0));
    }

    /// Whether a `newobj` token in assembly `asm` constructs a value type (a struct).
    #[must_use]
    pub fn is_value_type_ctor(&self, asm: u8, token: Token) -> bool {
        self.value_type_ctors.contains(&asm_key(asm, token.0))
    }

    /// Records the raw little-endian initializer bytes of a field with an RVA data blob in
    /// assembly `asm` (for `RuntimeHelpers.InitializeArray`).
    pub fn bind_field_rva(&mut self, asm: u8, token: Token, data: &[u8]) {
        self.field_rva_data.insert(asm_key(asm, token.0), data.into());
    }

    /// The raw initializer bytes of a field token in assembly `asm`, if it has an RVA blob.
    #[must_use]
    pub fn field_rva(&self, asm: u8, token: Token) -> Option<&[u8]> {
        self.field_rva_data
            .get(&asm_key(asm, token.0))
            .map(AsRef::as_ref)
    }

    /// Records that a type token in assembly `asm` names `System.Object` (a universal
    /// catch-all target for a type test).
    pub fn mark_object_type_token(&mut self, asm: u8, token: Token) {
        self.object_type_tokens.insert(asm_key(asm, token.0));
    }

    /// Whether a type token in assembly `asm` names `System.Object`.
    #[must_use]
    pub fn is_object_type_token(&self, asm: u8, token: Token) -> bool {
        self.object_type_tokens.contains(&asm_key(asm, token.0))
    }

    /// Records that a type token in assembly `asm` names `System.String`.
    pub fn mark_string_type_token(&mut self, asm: u8, token: Token) {
        self.string_type_tokens.insert(asm_key(asm, token.0));
    }

    /// Whether a type token in assembly `asm` names `System.String`.
    #[must_use]
    pub fn is_string_type_token(&self, asm: u8, token: Token) -> bool {
        self.string_type_tokens.contains(&asm_key(asm, token.0))
    }

    /// Records the simple (unqualified) name of a type token in assembly `asm`, for
    /// `System.Type.get_Name`.
    pub fn bind_type_name(&mut self, asm: u8, token: Token, name: String) {
        self.type_names.insert(asm_key(asm, token.0), name);
    }

    /// The simple name of a type token, keyed by its asm-folded value (the handle a
    /// `Type` intrinsic receives), if recorded.
    #[must_use]
    pub fn type_name_by_handle(&self, handle: u32) -> Option<&str> {
        self.type_names.get(&handle).map(String::as_str)
    }

    /// Records the byte `size` of the type a `sizeof` operand token names in assembly `asm`
    /// (a value type's computed layout size, or a primitive's fixed width).
    pub fn set_type_size(&mut self, asm: u8, token: Token, size: u32) {
        self.type_sizes.insert(asm_key(asm, token.0), size);
    }

    /// The byte size of the type a `sizeof` operand token names in assembly `asm`, if
    /// recorded -- what `sizeof` (III.4.25) pushes.
    #[must_use]
    pub fn type_size(&self, asm: u8, token: Token) -> Option<u32> {
        self.type_sizes.get(&asm_key(asm, token.0)).copied()
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
