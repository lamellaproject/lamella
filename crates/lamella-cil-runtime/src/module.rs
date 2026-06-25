//! A minimal, interim module: the methods the interpreter runs and calls between,
//! the intrinsics they reach, and the strings `ldstr` loads.

use crate::interp::Vm;
use crate::trap::Trap;
use crate::value::Value;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_cil::MethodBodyImage;
use lamella_token::Token;

/// An index identifying a [`Method`] within a [`Module`].
pub type MethodId = u32;

/// An index identifying a declared reference type within a [`Module`].
pub type TypeId = u32;

/// A custom-attribute argument value, decoded from the attribute's value blob at load and
/// materialized into a runtime [`Value`] when the attribute is instantiated (`GetCustomAttributes`).
/// The blob is decoded with no heap, so a string keeps its UTF-16 units and a `typeof(X)` argument
/// its already-resolved type handle until the intrinsic (which has the [`Vm`]) turns them into a
/// heap string / a `Type` handle [`Value`].
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    /// A signed integer, widened to `i64` (a `bool`/`char`/`sbyte`/.../`int`/`long`, or an enum's
    /// underlying value). The `wide` flag distinguishes a 64-bit-backed value (boxed as `int64`).
    Int {
        /// The integer value, sign-extended from its source width.
        value: i64,
        /// Whether the source type is 64-bit (`long`/`ulong`), so it materializes as `Int64`.
        wide: bool,
    },
    /// A single-precision float.
    R4(f32),
    /// A double-precision float.
    R8(f64),
    /// A `string`, as UTF-16 code units.
    Str(Box<[u16]>),
    /// A `System.Type` (`typeof(X)`): the resolved asm-folded type handle, or `0` if unresolved.
    Type(u64),
    /// A null reference (a null `string` / `Type` / `object` argument).
    Null,
}

/// One custom attribute applied to a target, decoded and resolved at load: the constructor to
/// run, its positional arguments, and the named field assignments to apply after. The
/// interpreter instantiates it by allocating the attribute type, running `ctor` with `positional`,
/// then storing each `named` value into its field slot ([`crate::intrinsics`]). Only same-module
/// attribute types (with an instantiable [`MethodId`] ctor) are recorded; a named PROPERTY is not
/// modeled (the corpus uses only named fields).
#[derive(Clone, Debug)]
pub struct LoadedAttribute {
    /// The attribute's constructor.
    pub ctor: MethodId,
    /// The [`TypeId`] of the attribute, for allocating its instance.
    pub type_id: TypeId,
    /// The positional (constructor) argument values, in declaration order.
    pub positional: Vec<AttrValue>,
    /// The named field assignments: `(field slot, value)`, applied after the ctor.
    pub named_fields: Vec<(u32, AttrValue)>,
    /// The named PROPERTY assignments: `(setter MethodId, value)`, applied after the named fields by
    /// INVOKING the setter -- so an explicit property's hand-written setter runs (not just an
    /// auto-property's backing field).
    pub named_properties: Vec<(MethodId, AttrValue)>,
}

/// Reflection metadata for a type, keyed by its asm-folded handle (the `System.Type` a `typeof`
/// or `GetType` yields). The `System.Type` introspection members (`Namespace`, `FullName`, the
/// `Is*` kind predicates) read it. Recorded at load for every type in each loaded assembly: the
/// names come from metadata, the kind bits are derived from the type's `flags` and base class.
#[derive(Clone, Default)]
pub struct ReflectType {
    /// The type's namespace (`""` for the global namespace; `Type.Namespace` renders that null).
    pub namespace: String,
    /// The type's full name (`namespace.name`, or the bare `name` in the global namespace).
    pub full_name: String,
    /// `Type.IsEnum`: the type extends `System.Enum`.
    pub is_enum: bool,
    /// `Type.IsValueType`: the type extends `System.ValueType` or `System.Enum`.
    pub is_value_type: bool,
    /// `Type.IsInterface`: the type is an interface.
    pub is_interface: bool,
    /// `Type.IsAbstract`: the type is abstract.
    pub is_abstract: bool,
    /// `Type.IsPublic`: the type has public visibility.
    pub is_public: bool,
    /// `Type.BaseType`: the asm-folded handle of the type's base class, or 0 for none (an interface,
    /// or `System.Object` itself).
    pub base_handle: u64,
}

/// One field of a type, for `Type.GetFields` enumeration: the field's asm-folded handle plus the
/// visibility / static bits `BindingFlags` filters on.
#[derive(Clone, Copy)]
pub struct ReflectField {
    /// The field's asm-folded `Field` token (the `FieldInfo` handle).
    pub handle: u64,
    /// Whether the field is static (`FieldAttributes.Static`).
    pub is_static: bool,
    /// Whether the field is public (`FieldAttributes` access == `Public`).
    pub is_public: bool,
}

/// One method of a type, for `Type.GetMethods` enumeration: the method's asm-folded handle plus the
/// visibility / static bits `BindingFlags` filters on. Constructors (`.ctor`/`.cctor`) are excluded
/// (they are `GetConstructors`), matching .NET.
#[derive(Clone, Copy)]
pub struct ReflectMethod {
    /// The method's asm-folded `MethodDef` token (the `MethodInfo` handle).
    pub handle: u64,
    /// Whether the method is static (`MethodAttributes.Static`).
    pub is_static: bool,
    /// Whether the method is public (`MethodAttributes` access == `Public`).
    pub is_public: bool,
}

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
    /// The type's NON-virtual instance methods (own + inherited), keyed by the same signature key.
    /// Consulted only when a `callvirt`'s static target is absent because its declaring type is in
    /// no loaded assembly -- e.g. a corlib base type our NETMF-surface corlib omits (modern .NET
    /// declares `ManualResetEvent.Set` on `EventWaitHandle`) -- so the call binds to the runtime
    /// type's own method by signature.
    sig_methods_nonvirtual: BTreeMap<String, MethodId>,
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
    by_token: BTreeMap<u64, MethodId>,
    strings: BTreeMap<u64, Box<[u16]>>,
    /// Declared reference types, indexed by [`TypeId`].
    types: Vec<TypeInfo>,
    /// A method's declaring type (so `newobj` can find the type to instantiate).
    method_type: BTreeMap<MethodId, TypeId>,
    /// An `ldfld`/`stfld` field token mapped to its instance-field slot.
    field_slots: BTreeMap<u64, u32>,
    /// An instance-field token mapped to its declaring type, so a `stfld` through a
    /// managed pointer can size the value-type instance it materializes.
    field_types: BTreeMap<u64, TypeId>,
    /// A `newarr` element-type token mapped to its elements' zero value.
    array_defaults: BTreeMap<u64, Value>,
    /// A `newarr` element-type token mapped to the element type's true byte width (`Byte` = 1,
    /// `Int16`/`Char` = 2, `Int32` = 4, `Int64`/`Double` = 8). Only sized primitive element types
    /// appear; a reference / value-type element array is absent (its `Buffer` width is undefined).
    /// Stamped onto each array at `newarr` so `System.Buffer` can size its byte image -- a width a
    /// `byte[]` and an `int[]` (both `Value::Int32` elements) cannot otherwise be told apart by.
    array_element_sizes: BTreeMap<u64, u8>,
    /// A virtual method's vtable slot (only virtual methods appear).
    method_slots: BTreeMap<MethodId, u32>,
    /// A static field token mapped to its storage slot in the [`crate::interp::Vm`].
    static_fields: BTreeMap<u64, usize>,
    /// The zero value of each static field, indexed by storage slot.
    static_defaults: Vec<Value>,
    /// The static constructors (`.cctor`), to run before the entry point.
    static_ctors: Vec<MethodId>,
    /// A `TypeDef` token mapped to its [`TypeId`] (for `castclass` / `isinst`).
    type_tokens: BTreeMap<u64, TypeId>,
    /// The reverse of `type_tokens`: a [`TypeId`] mapped to its declaring type's asm-folded
    /// `TypeDef` token (its `Type` handle), so `Object.GetType()` on a reference instance can
    /// hand back the same handle `typeof` / `Type.Name` use.
    type_handles: BTreeMap<TypeId, u64>,
    /// A `callvirt` token mapped to its target's `(signature key, arg count)`, for
    /// dispatching interface / abstract methods whose target has no vtable slot (and
    /// may have no resolvable body).
    call_targets: BTreeMap<u64, (String, u16)>,
    /// Explicit interface implementations (II.22.27 `MethodImpl`): the implementing body
    /// for a `(declaring TypeId, overridden method handle)` pair. The handle is the
    /// asm-folded token of the interface/virtual method named at the `callvirt` site (the
    /// `MethodImpl.MethodDeclaration`). An explicit body (`int IA.Value()`) is private and
    /// named after the interface, so it is reachable only through this map -- a plain
    /// signature match never finds it and cannot tell `IA.Value` from `IB.Value`.
    explicit_overrides: BTreeMap<(TypeId, u64), MethodId>,
    /// `newobj` tokens that construct a delegate (a delegate type's `.ctor`): instead
    /// of running a constructor, the (target, method) on the stack become a delegate.
    delegate_ctors: BTreeSet<u64>,
    /// A delegate type's `Invoke` token mapped to its parameter count, so `callvirt` on
    /// it calls the delegate's bound method with the bound target.
    delegate_invokes: BTreeMap<u64, u16>,
    /// A value/reference type's `Finalize` method (a destructor), if it declares one --
    /// the finalizer the collector runs when an instance becomes unreachable. Kept
    /// unconditionally (it is tiny); the finalization machinery that consumes it is
    /// behind the `finalizers` feature.
    finalizers: BTreeMap<TypeId, MethodId>,
    /// Each enum type's constants (keyed by its `TypeDef` token): the underlying integer
    /// value mapped to the constant name, so `Enum.ToString` can render the name.
    enum_constants: BTreeMap<u64, BTreeMap<i64, String>>,
    /// Tokens of enum types whose underlying type is 64-bit (long / ulong), so `Enum.Parse`
    /// boxes their values as int64 to match the declared type.
    enum_wide: BTreeSet<u64>,
    /// Tokens of enum types carrying `[System.FlagsAttribute]`, so `Enum.ToString` / `Format`
    /// renders a value as the comma-joined member names it decomposes into (rather than a single
    /// member name or the bare number). Recorded by the loader from the type's custom attributes.
    enum_flags: BTreeSet<u64>,
    /// Each enum type's underlying byte width (`sbyte`/`byte` = 1, `short`/`ushort` = 2,
    /// `int`/`uint` = 4, `long`/`ulong` = 8), keyed by its `TypeDef` handle -- the field width
    /// `Enum.Format`'s "X" zero-pads to (`width * 2` hex digits). Absent for an enum whose width
    /// the loader could not determine; the formatter then defaults to 4 (the `int` default).
    enum_widths: BTreeMap<u64, u8>,
    /// `newobj` tokens that construct a multi-dimensional array (an array TypeSpec's
    /// `.ctor`), mapped to the array's rank -- newobj allocates from that many lengths.
    md_array_ctors: BTreeMap<u64, u16>,
    /// `newobj` tokens that construct a `System.Text.StringBuilder`, mapped to the
    /// constructor's parameter count -- newobj allocates a builder (seeded from a string arg).
    string_builder_ctors: BTreeMap<u64, u16>,
    /// `newobj` tokens that construct a `System.Collections.ArrayList`, mapped to the
    /// constructor's parameter count -- newobj allocates an empty array-backed list.
    list_ctors: BTreeMap<u64, u16>,
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
    primitive_int32_token: Option<u64>,
    primitive_int64_token: Option<u64>,
    primitive_native_int_token: Option<u64>,
    #[cfg(feature = "float")]
    primitive_float_token: Option<u64>,
    /// `newobj` tokens whose declaring type is a value type (a struct): such a `newobj`
    /// must construct a struct VALUE in place (passing `this` by managed pointer to the
    /// ctor) and leave that value -- not a heap instance -- on the stack.
    value_type_ctors: BTreeSet<u64>,
    /// The raw little-endian initializer bytes of a field with an RVA data blob, keyed by
    /// the field token, for `RuntimeHelpers.InitializeArray` (a `T[] a = {...}` of a
    /// constant primitive array). Sized to the declaring blob; the interpreter slices it
    /// per the array's element width.
    field_rva_data: BTreeMap<u64, Box<[u8]>>,
    /// Type tokens (in `castclass` / `isinst` / `box` operands) classified by their external
    /// identity -- `System.Object` (a universal catch-all target) or `System.String` -- so a
    /// type test on a boxed value or a heap string is precise rather than unverified.
    object_type_tokens: BTreeSet<u64>,
    string_type_tokens: BTreeSet<u64>,
    /// The simple (unqualified) name of each `ldtoken`'d type, keyed by its asm-folded token,
    /// for `System.Type.get_Name` (a `typeof(T).Name`).
    type_names: BTreeMap<u64, String>,
    /// Each declared type's FULL name (`namespace.name`, or the bare `name` in the global
    /// namespace), keyed by [`TypeId`]. The simple `type_names` above is keyed by token and is
    /// enough for `Type.Name`; the exception TAG model needs the full name, because the tag is
    /// FNV-1a over `namespace.name` and a bare `Exception` must NOT collide with
    /// `System.Exception`. Populated by the loader alongside the type's other facts; used by
    /// [`Self::exception_tag_of`] / [`Self::exception_base_chain`] so the interpreter's tags equal
    /// the compiler's and AOT's for the same type.
    type_full_names: BTreeMap<TypeId, String>,
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
    type_sizes: BTreeMap<u64, u32>,
    /// The exception TAG of each `catch` clause's declared type, keyed by the clause's
    /// asm-folded catch-type token. The loader computes it from the catch token's metadata
    /// name (`exception_tag_for_name`), so it is available even for a BCL exception type the
    /// program references but no loaded assembly defines (e.g. `System.DivideByZeroException`,
    /// absent from corlib) -- the handler search ([`crate::interp`]) tests this tag for
    /// membership in the thrown exception's base-chain vector, so a `catch` matches ONLY a
    /// subtype of its declared type. A typeless `catch {}` whose token names no type records
    /// nothing and is treated as catch-all.
    catch_type_tags: BTreeMap<u64, u32>,
    /// The custom attributes applied to a target (a type or a member), keyed by the target's
    /// asm-folded metadata token -- the `Type` / `MemberInfo` handle a `GetCustomAttributes`
    /// receiver carries. Each is decoded + resolved at load (its ctor, positional, and named
    /// field values); the interpreter instantiates them on demand. A target with no recorded
    /// (instantiable) attributes is absent.
    custom_attributes: BTreeMap<u64, Vec<LoadedAttribute>>,
    /// A type's member by simple name, keyed by `(type handle, member name)` -> the member's
    /// asm-folded token (a `Field` / `MethodDef` / `Property` token). This is what
    /// `Type.GetField` / `GetMethod` / `GetProperty` resolve a name to: the handle of the
    /// `MemberInfo` whose `GetCustomAttributes` then reads `custom_attributes`. Three separate
    /// maps so a field, a method, and a property of the same name do not collide.
    type_fields_by_name: BTreeMap<(u64, String), u64>,
    type_methods_by_name: BTreeMap<(u64, String), u64>,
    type_properties_by_name: BTreeMap<(u64, String), u64>,
    /// Reflection introspection metadata per type (the `System.Type` `Namespace`/`FullName`/`Is*`
    /// surface), keyed by the type's asm-folded handle. Empty on a Kernel-only build (the loader
    /// records it only under the `NETMFv4_4` reflection tier).
    reflect_types: BTreeMap<u64, ReflectType>,
    /// Each type's fields in declaration order (the `Type.GetFields` enumeration), keyed by the
    /// type's asm-folded handle. Recorded only under the `NETMFv4_4` reflection tier.
    type_fields: BTreeMap<u64, Vec<ReflectField>>,
    /// Each type's parameterless instance `.ctor` (what `Activator.CreateInstance(Type)` runs),
    /// keyed by the type's asm-folded handle. Recorded only under the `NETMFv4_4` reflection tier.
    type_ctors: BTreeMap<u64, MethodId>,
    /// Each type's instance constructors as `(ctor handle, parameter count)`, keyed by the type's
    /// asm-folded handle -- `Type.GetConstructor(Type[])` matches by arity. NETMFv4_4 tier only.
    type_ctors_list: BTreeMap<u64, Vec<(u64, usize)>>,
    /// Each type's methods in declaration order (the `Type.GetMethods` enumeration, constructors
    /// excluded), keyed by the type's asm-folded handle. The `NETMFv4_4` reflection tier only.
    type_methods: BTreeMap<u64, Vec<ReflectMethod>>,
    /// A member's type (a field's `FieldType` or a method's `ReturnType`) as the type's asm-folded
    /// handle, keyed by the member's asm-folded handle. The `NETMFv4_4` reflection tier only.
    member_type_handle: BTreeMap<u64, u64>,
    /// Full type name (`namespace.name`) -> the type's asm-folded handle, for every
    /// reflection-recorded type. Resolves a member's `FieldType` / `ReturnType` (e.g. a primitive
    /// field's `System.Int32`) from its signature. The `NETMFv4_4` reflection tier only.
    name_to_handle: BTreeMap<String, u64>,
    /// A method's `MethodAttributes` flags, keyed by the method's asm-folded handle, for the
    /// `MethodBase.Is*` predicates. The `NETMFv4_4` reflection tier only.
    method_attrs: BTreeMap<u64, u32>,
}

/// Folds the assembly id into a token key: the assembly in the HIGH 32 bits, the metadata token
/// in the low 32. A `u64` key keeps every assembly's token space disjoint without overlapping the
/// token's own table tag (the high byte of the 32-bit token), so up to 256 assemblies (a `u8` asm
/// id) can be resolved simultaneously -- the incremental REPL needs three at once (corlib + the
/// persistent `__Repl` + a running delta). The earlier single-bit `token | (asm << 31)` scheme
/// capped this at two assemblies and would have collided a third's tokens with the first's.
#[must_use]
pub fn asm_key(asm: u8, token: u32) -> u64 {
    ((asm as u64) << 32) | (token as u64)
}

/// Decomposes `value` into the comma-joined member names of a `[Flags]` enum, exactly as
/// .NET's `Enum.FormatFlags` does (verified against the .NET 8 oracle):
///
/// - `value == 0` renders the member named for 0 if one exists, else `"0"`.
/// - Otherwise the members are walked from HIGHEST value to lowest; a member whose bits are all
///   still set in the remaining value is consumed (its bits cleared) and its name recorded. Walking
///   high-first makes a composite member (e.g. `RW = Read | Write`) win over its parts, and recording
///   front-to-back yields the names in INCREASING value order (`RW, Exec`).
/// - A member of value 0 is never used as a flag part (only as the zero name above).
/// - If any bit is left over with no covering member -- or no member matched at all -- the whole
///   value renders as its underlying DECIMAL number, matching .NET.
///
/// `constants` is the enum's `value -> name` map (ascending by value, a `BTreeMap`).
fn format_flag_names(constants: &BTreeMap<i64, String>, value: i64) -> String {
    if value == 0 {
        return match constants.get(&0) {
            Some(name) => name.clone(),
            None => String::from("0"),
        };
    }
    let mut remaining = value;
    let mut names: Vec<&str> = Vec::new();
    for (member_value, name) in constants.iter().rev() {
        let bits = *member_value;
        if bits != 0 && (remaining & bits) == bits {
            names.push(name.as_str());
            remaining &= !bits;
            if remaining == 0 {
                break;
            }
        }
    }
    if remaining != 0 || names.is_empty() {
        return value.to_string();
    }
    names.reverse();
    names.join(", ")
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

    /// The method whose asm-folded handle is `handle` (the `MethodInfo` handle reflection carries
    /// -- a `MethodDef` token folded with its assembly), if bound.
    #[must_use]
    pub fn resolve_by_handle(&self, handle: u64) -> Option<MethodId> {
        self.by_token.get(&handle).copied()
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
            sig_methods_nonvirtual: BTreeMap::new(),
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

    /// The instance-field slot of the field whose asm-folded handle is `handle` (a `FieldInfo`
    /// handle that reflection carries), if it names an instance field.
    #[must_use]
    pub fn field_slot_by_handle(&self, handle: u64) -> Option<u32> {
        self.field_slots.get(&handle).copied()
    }

    /// The static storage slot of the field whose asm-folded handle is `handle` (a `FieldInfo`
    /// handle), if it names a static field.
    #[must_use]
    pub fn static_field_slot_by_handle(&self, handle: u64) -> Option<usize> {
        self.static_fields.get(&handle).copied()
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

    /// Appends one instance field of zero value `default` to an ALREADY-LOADED `type_id`,
    /// returning the new field's slot index (its position in the grown layout). This is the
    /// incremental-REPL seam: a submission delta naming a `__Repl` field the persistent type
    /// does not yet have grows the type by this one slot, and the caller grows the single live
    /// instance to match ([`crate::Heap::grow_instance`]). Appending keeps every prior slot
    /// stable, so a field's slot (and the values already stored there) never shifts. Returns
    /// `None` if `type_id` is not a declared type.
    pub fn add_type_field(&mut self, type_id: TypeId, default: Value) -> Option<u32> {
        let info = self.types.get_mut(type_id as usize)?;
        let slot = info.field_defaults.len() as u32;
        let mut grown = Vec::from(core::mem::take(&mut info.field_defaults));
        grown.push(default);
        info.field_defaults = grown.into_boxed_slice();
        Some(slot)
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

    /// Binds a `newarr` element-type token in assembly `asm` to the element type's byte width
    /// (`Byte` = 1, `Int16`/`Char` = 2, `Int32` = 4, `Int64`/`Double` = 8) -- the size
    /// `System.Buffer` measures the array's byte image in. Only sized primitive element types are
    /// bound; a reference / value-type element array is left unbound.
    pub fn bind_array_element_size(&mut self, asm: u8, token: Token, size: u8) {
        self.array_element_sizes.insert(asm_key(asm, token.0), size);
    }

    /// The element type's byte width of a `newarr` element-type token in assembly `asm`, or `0`
    /// when none was bound (a reference / value-type element array). `newarr` stamps this onto the
    /// array so `System.Buffer.BlockCopy` / `ByteLength` can size it.
    #[must_use]
    pub fn array_element_size(&self, asm: u8, token: Token) -> u8 {
        self.array_element_sizes
            .get(&asm_key(asm, token.0))
            .copied()
            .unwrap_or(0)
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
        self.enum_value_name_by_handle(asm_key(asm, token), value)
    }

    /// The name of the constant with underlying `value` in the enum type identified by an
    /// already-asm-folded `handle` (the box tag / `RuntimeTypeHandle` an intrinsic holds, so it
    /// keys the map directly rather than re-folding a raw token).
    #[must_use]
    pub fn enum_value_name_by_handle(&self, handle: u64, value: i64) -> Option<&str> {
        self.enum_constants
            .get(&handle)
            .and_then(|constants| constants.get(&value))
            .map(String::as_str)
    }

    /// The name of the constant with underlying `value` in the enum named by `handle`, resolving
    /// ACROSS assemblies: first the direct handle (the same-assembly case, where the box tag /
    /// constraint token equals the enum's own folded `TypeDef` token), then -- on a miss -- the
    /// enum's CANONICAL handle reached through the shared [`TypeId`]. A program boxing or calling
    /// `.ToString()` on a CORLIB enum (e.g. `DayOfWeek`) carries that enum's `TypeRef` in the
    /// program's token space, which keys nothing in `enum_constants` (the constants were recorded
    /// under the corlib's `TypeDef`); mapping `handle -> TypeId -> canonical handle` lands on the
    /// corlib entry, so the member name resolves like .NET rather than falling back to the number.
    #[must_use]
    pub fn enum_value_name_resolved(&self, handle: u64, value: i64) -> Option<&str> {
        if let Some(name) = self.enum_value_name_by_handle(handle, value) {
            return Some(name);
        }
        let type_id = self.type_id_by_handle(handle)?;
        let canonical = self.type_handle_of(type_id)?;
        self.enum_value_name_by_handle(canonical, value)
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
        self.enum_value_by_name_handle(asm_key(asm, token), name, ignore_case)
    }

    /// The underlying value of the constant named `name` in the enum type identified by an
    /// already-asm-folded `handle` -- the by-handle form for an intrinsic holding a folded tag.
    #[must_use]
    pub fn enum_value_by_name_handle(
        &self,
        handle: u64,
        name: &str,
        ignore_case: bool,
    ) -> Option<i64> {
        self.enum_constants
            .get(&handle)?
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

    /// Whether the type named by `handle` is an enum -- it has at least one recorded constant --
    /// resolving ACROSS assemblies like [`Self::enum_value_name_resolved`] (the program's
    /// `TypeRef` to a corlib enum maps through the shared [`TypeId`] to the corlib's entry). The
    /// constrained-`ToString` path needs this to tell an enum (whose `ToString` renders the
    /// underlying NUMBER when no member matches) from a plain value type (whose inherited
    /// `ValueType.ToString` renders the type NAME).
    #[must_use]
    pub fn is_enum_by_handle(&self, handle: u64) -> bool {
        if self.enum_constants.contains_key(&handle) {
            return true;
        }
        self.type_id_by_handle(handle)
            .and_then(|type_id| self.type_handle_of(type_id))
            .is_some_and(|canonical| self.enum_constants.contains_key(&canonical))
    }

    /// The constant map of the enum named by `handle`, resolving ACROSS assemblies the same way
    /// [`Self::enum_value_name_resolved`] does: the direct handle (same-assembly), then -- on a
    /// miss -- the enum's CANONICAL handle via the shared [`TypeId`] (a program naming a corlib
    /// enum by `TypeRef`). The map is `value -> name`, ordered ascending by value (a `BTreeMap`),
    /// which is the order `Enum.GetNames` / `GetValues` report and the flags formatter walks.
    fn enum_map_resolved(&self, handle: u64) -> Option<&BTreeMap<i64, String>> {
        if let Some(constants) = self.enum_constants.get(&handle) {
            return Some(constants);
        }
        let canonical = self.type_handle_of(self.type_id_by_handle(handle)?)?;
        self.enum_constants.get(&canonical)
    }

    /// Records that the enum type `token` in assembly `asm` carries `[FlagsAttribute]`.
    pub fn set_enum_flags(&mut self, asm: u8, token: u32) {
        self.enum_flags.insert(asm_key(asm, token));
    }

    /// Whether the enum named by `handle` carries `[FlagsAttribute]`, resolving across assemblies
    /// (direct handle, then the canonical handle via the shared [`TypeId`]) like the name lookups.
    #[must_use]
    pub fn enum_is_flags_by_handle(&self, handle: u64) -> bool {
        if self.enum_flags.contains(&handle) {
            return true;
        }
        self.type_id_by_handle(handle)
            .and_then(|type_id| self.type_handle_of(type_id))
            .is_some_and(|canonical| self.enum_flags.contains(&canonical))
    }

    /// Records the underlying byte `width` (1/2/4/8) of the enum type `token` in assembly `asm`.
    pub fn set_enum_width(&mut self, asm: u8, token: u32, width: u8) {
        self.enum_widths.insert(asm_key(asm, token), width);
    }

    /// The underlying byte width of the enum named by `handle` (resolving across assemblies),
    /// defaulting to 4 (`int`) when unknown -- the width `Enum.Format`'s "X" zero-pads to.
    #[must_use]
    pub fn enum_width_by_handle(&self, handle: u64) -> u8 {
        if let Some(&width) = self.enum_widths.get(&handle) {
            return width;
        }
        self.type_id_by_handle(handle)
            .and_then(|type_id| self.type_handle_of(type_id))
            .and_then(|canonical| self.enum_widths.get(&canonical).copied())
            .unwrap_or(4)
    }

    /// The enum's members as `(value, name)` pairs ordered ascending by value -- the order
    /// `Enum.GetNames` and `Enum.GetValues` report. Resolves across assemblies; `None` if the
    /// handle names no known enum.
    #[must_use]
    pub fn enum_members_by_handle(&self, handle: u64) -> Option<Vec<(i64, String)>> {
        self.enum_map_resolved(handle).map(|constants| {
            constants
                .iter()
                .map(|(value, name)| (*value, name.clone()))
                .collect()
        })
    }

    /// Renders `value` as `Enum.ToString` / the "G" format does: for a `[Flags]` enum, the
    /// comma-joined member names the value decomposes into (`Enum.FormatFlags`); otherwise the
    /// single member name. `None` when no name applies (the caller renders the underlying number,
    /// matching .NET's fallback). Resolves across assemblies. `flags` forces the flags algorithm
    /// regardless of the attribute (the "F" format).
    #[must_use]
    pub fn enum_name_or_flags(&self, handle: u64, value: i64, flags: bool) -> Option<String> {
        let constants = self.enum_map_resolved(handle)?;
        if flags || self.enum_is_flags_by_handle(handle) {
            Some(format_flag_names(constants, value))
        } else {
            constants.get(&value).cloned()
        }
    }

    /// Records that the enum type `token` in assembly `asm` has a 64-bit underlying type
    /// (long / ulong).
    pub fn set_enum_wide(&mut self, asm: u8, token: u32) {
        self.enum_wide.insert(asm_key(asm, token));
    }

    /// Whether the enum type identified by an already-asm-folded `handle` has a 64-bit underlying
    /// type (long / ulong) -- the by-handle form for an intrinsic holding a folded tag.
    #[must_use]
    pub fn enum_is_wide_by_handle(&self, handle: u64) -> bool {
        self.enum_wide.contains(&handle)
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
    pub fn primitive_type_token(&self, value: &Value) -> Option<u64> {
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
    pub fn type_handle_of(&self, type_id: TypeId) -> Option<u64> {
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
    pub fn type_id_by_handle(&self, handle: u64) -> Option<TypeId> {
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

    /// Whether `type_id` implements `interface_id` -- directly, by inheriting an
    /// implementation from a base type, or because an implemented interface itself extends
    /// `interface_id`. Walks `type_id`'s base chain (a type matches when it lists
    /// `interface_id` among the interfaces it implements, recorded by
    /// [`Self::set_type_interfaces`]) AND the interface-extends-interface graph reachable
    /// from each implemented interface.
    ///
    #[must_use]
    pub fn implements_interface(&self, type_id: TypeId, interface_id: TypeId) -> bool {
        let mut visited = Vec::new();
        let mut pending = Vec::new();

        let mut current = Some(type_id);
        for _ in 0..=self.types.len() {
            match current {
                Some(id) => {
                    let Some(info) = self.types.get(id as usize) else {
                        break;
                    };
                    for &iface in &info.interfaces {
                        if !pending.contains(&iface) {
                            pending.push(iface);
                        }
                    }
                    current = info.base;
                }
                None => break,
            }
        }

        while let Some(iface) = pending.pop() {
            if iface == interface_id {
                return true;
            }
            if visited.contains(&iface) {
                continue;
            }
            visited.push(iface);
            if let Some(info) = self.types.get(iface as usize) {
                for &base_iface in &info.interfaces {
                    if !visited.contains(&base_iface) {
                        pending.push(base_iface);
                    }
                }
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

    /// Records `type_id`'s non-virtual instance methods keyed by signature (the map should include
    /// inherited methods), for [`Module::sig_dispatch_nonvirtual`].
    pub fn set_sig_methods_nonvirtual(
        &mut self,
        type_id: TypeId,
        methods: BTreeMap<String, MethodId>,
    ) {
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.sig_methods_nonvirtual = methods;
        }
    }

    /// The non-virtual instance method of `type_id` matching `sig_key` -- the last-resort
    /// `callvirt` target when the static target is absent (its declaring type is in no loaded
    /// assembly), so dispatch falls to the runtime type's own method by signature.
    #[must_use]
    pub fn sig_dispatch_nonvirtual(&self, type_id: TypeId, sig_key: &str) -> Option<MethodId> {
        self.types
            .get(type_id as usize)?
            .sig_methods_nonvirtual
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

    /// The raw initializer bytes of a field identified by an already-asm-folded `handle` (the
    /// `RuntimeFieldHandle` an `ldtoken <field>` pushed), if it has an RVA blob -- keyed by the
    /// handle directly, as the intrinsic holds the folded form.
    #[must_use]
    pub fn field_rva_by_handle(&self, handle: u64) -> Option<&[u8]> {
        self.field_rva_data.get(&handle).map(AsRef::as_ref)
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

    /// Records the exception TAG of a `catch` clause's declared type, keyed by the catch-type
    /// `token` in assembly `asm`. The loader supplies the tag computed from the token's
    /// metadata name, so the handler search can match a catch by type without the catch type
    /// having to resolve to a loaded [`TypeId`] (a referenced-but-undefined BCL exception type
    /// still gets a tag from its name).
    #[cfg(feature = "exceptions")]
    pub fn bind_catch_type_tag(&mut self, asm: u8, token: Token, tag: u32) {
        self.catch_type_tags.insert(asm_key(asm, token.0), tag);
    }

    /// The exception TAG recorded for a `catch` clause's declared type token in assembly `asm`,
    /// if one was bound. `None` for a typeless `catch {}` (whose token names no type), which the
    /// handler search treats as a catch-all.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub fn catch_type_tag(&self, asm: u8, token: Token) -> Option<u32> {
        self.catch_type_tags.get(&asm_key(asm, token.0)).copied()
    }

    /// The simple name of a type token, keyed by its asm-folded value (the handle a
    /// `Type` intrinsic receives), if recorded.
    #[must_use]
    pub fn type_name_by_handle(&self, handle: u64) -> Option<&str> {
        self.type_names.get(&handle).map(String::as_str)
    }

    /// Records one custom attribute (decoded + resolved at load) applied to the target whose
    /// asm-folded token is `target_handle` -- the `Type` / `MemberInfo` handle a
    /// `GetCustomAttributes` receiver carries. Attributes accumulate in application order.
    pub fn add_custom_attribute(&mut self, target_handle: u64, attribute: LoadedAttribute) {
        self.custom_attributes
            .entry(target_handle)
            .or_default()
            .push(attribute);
    }

    /// The custom attributes recorded for the target whose asm-folded token is `target_handle`
    /// (a type or member), in application order. Empty if none.
    #[must_use]
    pub fn custom_attributes_of(&self, target_handle: u64) -> &[LoadedAttribute] {
        self.custom_attributes
            .get(&target_handle)
            .map_or(&[], Vec::as_slice)
    }

    /// Records that the type whose handle is `type_handle` has a field named `name` whose
    /// asm-folded `Field` token is `field_handle` -- what `Type.GetField(name)` returns.
    pub fn bind_type_field_name(&mut self, type_handle: u64, name: &str, field_handle: u64) {
        self.type_fields_by_name
            .insert((type_handle, name.to_string()), field_handle);
    }

    /// Records that the type whose handle is `type_handle` has a method named `name` whose
    /// asm-folded `MethodDef` token is `method_handle` -- what `Type.GetMethod(name)` returns.
    pub fn bind_type_method_name(&mut self, type_handle: u64, name: &str, method_handle: u64) {
        self.type_methods_by_name
            .insert((type_handle, name.to_string()), method_handle);
    }

    /// Records that the type whose handle is `type_handle` has a property named `name` whose
    /// asm-folded `Property` token is `property_handle` -- what `Type.GetProperty(name)` returns.
    pub fn bind_type_property_name(&mut self, type_handle: u64, name: &str, property_handle: u64) {
        self.type_properties_by_name
            .insert((type_handle, name.to_string()), property_handle);
    }

    /// The asm-folded `Field` token of the field named `name` on the type whose handle is
    /// `type_handle` (the `FieldInfo` handle `Type.GetField` returns), or `None`.
    #[must_use]
    pub fn type_field_handle(&self, type_handle: u64, name: &str) -> Option<u64> {
        self.type_fields_by_name
            .get(&(type_handle, name.to_string()))
            .copied()
    }

    /// The asm-folded `MethodDef` token of the method named `name` on the type whose handle is
    /// `type_handle` (the `MethodInfo` handle `Type.GetMethod` returns), or `None`.
    #[must_use]
    pub fn type_method_handle(&self, type_handle: u64, name: &str) -> Option<u64> {
        self.type_methods_by_name
            .get(&(type_handle, name.to_string()))
            .copied()
    }

    /// The asm-folded `Property` token of the property named `name` on the type whose handle is
    /// `type_handle` (the `PropertyInfo` handle `Type.GetProperty` returns), or `None`.
    #[must_use]
    pub fn type_property_handle(&self, type_handle: u64, name: &str) -> Option<u64> {
        self.type_properties_by_name
            .get(&(type_handle, name.to_string()))
            .copied()
    }

    /// Records the reflection introspection metadata for the type whose asm-folded handle is
    /// `handle` -- the `System.Type` `Namespace`/`FullName`/`Is*` surface.
    pub fn bind_reflect_type(&mut self, handle: u64, info: ReflectType) {
        if !info.full_name.is_empty() {
            self.name_to_handle
                .entry(info.full_name.clone())
                .or_insert(handle);
        }
        self.reflect_types.insert(handle, info);
    }

    /// The asm-folded handle of the type whose full name is `full_name` (`namespace.name`), if a
    /// type with that name was reflection-recorded -- how `FieldInfo.FieldType` / `MethodInfo
    /// .ReturnType` resolve a member's type (e.g. a primitive's `System.Int32`) from its signature.
    #[must_use]
    pub fn type_handle_by_name(&self, full_name: &str) -> Option<u64> {
        self.name_to_handle.get(full_name).copied()
    }

    /// The reflection introspection metadata recorded for the type whose asm-folded handle is
    /// `handle`, or `None` if the handle names no recorded type.
    #[must_use]
    pub fn reflect_type(&self, handle: u64) -> Option<&ReflectType> {
        self.reflect_types.get(&handle)
    }

    /// Records the field list (for `Type.GetFields`) of the type whose asm-folded handle is
    /// `type_handle`, in declaration order.
    pub fn bind_type_fields(&mut self, type_handle: u64, fields: Vec<ReflectField>) {
        self.type_fields.insert(type_handle, fields);
    }

    /// The fields (declaration order) of the type whose asm-folded handle is `type_handle`, for
    /// `Type.GetFields` enumeration; empty if none recorded.
    #[must_use]
    pub fn type_fields(&self, type_handle: u64) -> &[ReflectField] {
        self.type_fields.get(&type_handle).map_or(&[], Vec::as_slice)
    }

    /// Records the parameterless instance constructor of the type whose asm-folded handle is
    /// `type_handle` (what `Activator.CreateInstance(Type)` runs).
    pub fn bind_type_ctor(&mut self, type_handle: u64, ctor: MethodId) {
        self.type_ctors.insert(type_handle, ctor);
    }

    /// The parameterless instance constructor recorded for the type whose asm-folded handle is
    /// `type_handle`, or `None`.
    #[must_use]
    pub fn type_ctor(&self, type_handle: u64) -> Option<MethodId> {
        self.type_ctors.get(&type_handle).copied()
    }

    /// Records one instance constructor (its handle + parameter count) of the type whose asm-folded
    /// handle is `type_handle`, for `Type.GetConstructor`.
    pub fn bind_type_ctor_overload(&mut self, type_handle: u64, ctor_handle: u64, param_count: usize) {
        self.type_ctors_list
            .entry(type_handle)
            .or_default()
            .push((ctor_handle, param_count));
    }

    /// The instance constructors `(handle, parameter count)` of the type whose asm-folded handle is
    /// `type_handle`, for `Type.GetConstructor` arity matching; empty if none recorded.
    #[must_use]
    pub fn type_ctors_list(&self, type_handle: u64) -> &[(u64, usize)] {
        self.type_ctors_list
            .get(&type_handle)
            .map_or(&[], Vec::as_slice)
    }

    /// Records the method list (for `Type.GetMethods`, constructors excluded) of the type whose
    /// asm-folded handle is `type_handle`, in declaration order.
    pub fn bind_type_methods(&mut self, type_handle: u64, methods: Vec<ReflectMethod>) {
        self.type_methods.insert(type_handle, methods);
    }

    /// The methods (declaration order, constructors excluded) of the type whose asm-folded handle
    /// is `type_handle`, for `Type.GetMethods` enumeration; empty if none recorded.
    #[must_use]
    pub fn type_methods(&self, type_handle: u64) -> &[ReflectMethod] {
        self.type_methods
            .get(&type_handle)
            .map_or(&[], Vec::as_slice)
    }

    /// Records the type of the member whose asm-folded handle is `handle` -- a field's `FieldType`
    /// or a method's `ReturnType` (the type's own asm-folded handle).
    pub fn bind_member_type(&mut self, handle: u64, type_handle: u64) {
        self.member_type_handle.insert(handle, type_handle);
    }

    /// The type handle recorded for the member whose asm-folded handle is `handle`
    /// (`FieldInfo.FieldType` / `MethodInfo.ReturnType`), or `None` if its type was not resolvable.
    #[must_use]
    pub fn member_type(&self, handle: u64) -> Option<u64> {
        self.member_type_handle.get(&handle).copied()
    }

    /// Records a method's `MethodAttributes` flags (for `MethodBase.Is*`), keyed by its handle.
    pub fn bind_method_attrs(&mut self, handle: u64, attrs: u32) {
        self.method_attrs.insert(handle, attrs);
    }

    /// The `MethodAttributes` flags of the method whose asm-folded handle is `handle`, or `None`.
    #[must_use]
    pub fn method_attrs(&self, handle: u64) -> Option<u32> {
        self.method_attrs.get(&handle).copied()
    }

    /// Records `type_id`'s FULL name (`namespace.name`, or the bare `name` in the global
    /// namespace), the form the exception TAG model hashes. The loader supplies this from
    /// metadata so the interpreter's per-type tag equals the compiler's and the AOT image's.
    pub fn bind_type_full_name(&mut self, type_id: TypeId, full_name: String) {
        self.type_full_names.insert(type_id, full_name);
    }

    /// The full name (`namespace.name`) recorded for `type_id`, if any.
    #[must_use]
    pub fn type_full_name(&self, type_id: TypeId) -> Option<&str> {
        self.type_full_names.get(&type_id).map(String::as_str)
    }

    /// The exception TAG of `type_id`: [`crate::exception::exception_tag`] of its full name. `None`
    /// if no full name was recorded (so `0`, the no-exception sentinel, is never produced from a
    /// missing name). Identical to `lamella_metadata::Assembly::exception_tag` for the same type,
    /// so a thrown exception is identified the same way in the interpreter, the AOT image, and the
    /// emitter -- the basis for crossing the AOT <-> interpreter boundary in mixed mode.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub fn exception_tag_of(&self, type_id: TypeId) -> Option<u32> {
        self.type_full_name(type_id)
            .map(crate::exception::exception_tag)
    }

    /// `type_id`'s base-chain tag VECTOR -- `[tag(type_id), tag(base), ..., tag(System.Exception)]`,
    /// leaf-first up the `extends` chain -- the membership vector a catch is tested against in mixed
    /// mode. Walks the same base chain [`Self::is_subtype`] does (bounded by the type count, so
    /// malformed cyclic metadata cannot loop forever); a type with no recorded full name contributes
    /// no entry but the walk continues to its base. This vector and a live `is_subtype` walk give the
    /// same catch verdict (`exception::tag_is_subtype(catch_tag, &chain)` == `is_subtype(thrown, catch)`),
    /// the equivalence the AOT contract relies on.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub fn exception_base_chain(&self, type_id: TypeId) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut current = Some(type_id);
        for _ in 0..=self.types.len() {
            match current {
                Some(id) => {
                    if let Some(tag) = self.exception_tag_of(id) {
                        chain.push(tag);
                    }
                    current = self.types.get(id as usize).and_then(|info| info.base);
                }
                None => break,
            }
        }
        chain
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

    #[test]
    fn implements_interface_walks_the_interface_extends_chain() {
        let mut module = Module::new();
        let ienumerable = module.add_type(Vec::new());
        let icollection = module.add_type(Vec::new());
        let ilist = module.add_type(Vec::new());
        let idictionary = module.add_type(Vec::new());
        let icomparer = module.add_type(Vec::new());
        let array_list = module.add_type(Vec::new());
        let hashtable = module.add_type(Vec::new());

        module.set_type_interfaces(icollection, Vec::from([ienumerable]));
        module.set_type_interfaces(ilist, Vec::from([icollection]));
        module.set_type_interfaces(idictionary, Vec::from([icollection]));
        module.set_type_interfaces(array_list, Vec::from([ilist]));
        module.set_type_interfaces(hashtable, Vec::from([idictionary]));

        assert!(module.implements_interface(array_list, ilist));
        assert!(module.implements_interface(array_list, icollection));
        assert!(module.implements_interface(array_list, ienumerable));
        assert!(!module.implements_interface(array_list, icomparer));
        assert!(!module.implements_interface(array_list, idictionary));

        assert!(module.implements_interface(hashtable, idictionary));
        assert!(module.implements_interface(hashtable, icollection));
        assert!(module.implements_interface(hashtable, ienumerable));
        assert!(!module.implements_interface(hashtable, ilist));

        let derived = module.add_type(Vec::new());
        module.set_type_base(derived, Some(array_list));
        assert!(module.implements_interface(derived, ilist));
        assert!(module.implements_interface(derived, ienumerable));
        assert!(!module.implements_interface(derived, icomparer));
    }
}
