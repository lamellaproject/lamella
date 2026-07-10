//! A minimal, interim module: the methods the interpreter runs and calls between,
//! the intrinsics they reach, and the strings `ldstr` loads.

use crate::interp::Vm;
use crate::object::PrimKind;
use crate::trap::Trap;
use crate::value::Value;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
#[cfg(all(not(feature = "code-preload"), not(feature = "code-in-place")))]
use core::cell::OnceCell;
use lamella_cil::MethodBodyImage;
#[cfg(feature = "code-in-place")]
use lamella_cil::{EhClause, read_body_layout, write_method_body};
#[cfg(not(feature = "code-in-place"))]
use lamella_cil::read_method_body;
use lamella_token::Token;

#[cfg(all(feature = "code-preload", feature = "code-in-place"))]
compile_error!(
    "code_loading picks ONE representation: enable at most one of `code-preload` and `code-in-place`"
);

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
    /// A single-dimension array argument (`new int[]{...}`): the decoded elements in order.
    /// A NULL array decodes as [`AttrValue::Null`], never as an empty `Array`.
    Array(Box<[AttrValue]>),
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

/// One parameter of a method, for `System.Reflection.ParameterInfo` (a `MethodBase.GetParameters`
/// entry): the parameter's asm-folded type handle (its `ParameterType`) and its declared name.
#[derive(Clone, Debug)]
pub struct MethodParam {
    /// The parameter's type as an asm-folded type handle (the `Type` `ParameterType` returns).
    pub type_handle: u64,
    /// The parameter's declared name (empty if the metadata records none).
    pub name: String,
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
    /// keyed by the interned signature id of "name + parameter types", for interface and
    /// abstract-method dispatch where the static target carries no vtable slot.
    sig_methods: BTreeMap<u32, MethodId>,
    /// The type's NON-virtual instance methods (own + inherited), keyed by the same interned id.
    /// Consulted only when a `callvirt`'s static target is absent because its declaring type is in
    /// no loaded assembly -- e.g. a corlib base type our NETMF-surface corlib omits (modern .NET
    /// declares `ManualResetEvent.Set` on `EventWaitHandle`) -- so the call binds to the runtime
    /// type's own method by signature.
    sig_methods_nonvirtual: BTreeMap<u32, MethodId>,
    /// Whether this type is a value type (a struct / primitive, extending `System.ValueType`
    /// or `System.Enum`). A `callvirt` (or `constrained. callvirt`) that dispatches to a value
    /// type's OWN instance method on a boxed receiver auto-unboxes `this` to a managed pointer
    /// into the box (III.4.2), so the body reads the value through `this` (`ldarg.0; ldind.*`).
    value_type: bool,
}

/// One method's callable form, by value -- identical for a live module (the `methods` Vec)
/// and a baked one (its 24-byte record decoded from the image, the intrinsic from the
/// boot-resolved column), so the interpreter's call sites need no representation knowledge.
#[derive(Clone, Copy)]
pub enum MethodKind {
    /// Managed CIL: push a frame and run it.
    Managed,
    /// A native intrinsic: invoke directly.
    Intrinsic(IntrinsicFn),
}

/// The baked views a booted module reads in place of the `methods` and `types` Vecs: every
/// record stays in the image; the only resident piece is the per-method registry-index column
/// (2 bytes each -- the fn pointers live in the static registry table).
#[cfg(feature = "code-in-place")]
#[derive(Clone)]
struct BakedMethods {
    /// Arena-relative offset of the 24-byte method records.
    table: usize,
    count: usize,
    /// The borrowed image's arena bytes -- the `'static` source the code slices come from.
    arena_static: &'static [u8],
    /// The boot-resolved REGISTRY INDEX per method (`u16::MAX` = managed) -- allocated ONLY
    /// when the image's registry fingerprint differs from this build's. On the fingerprinted
    /// (same-build) flow it stays empty: each intrinsic record's index HINT is read straight
    /// from flash.
    intrinsic_index: Box<[u16]>,
    /// Whether the in-record index hints are valid for this build (fingerprint matched).
    hints_valid: bool,
    /// Arena-relative offset of the per-method assembly-id byte column.
    method_asm: usize,
    /// Arena-relative offset of the 16-byte per-type records (base, flags, defaults ref).
    type_table: usize,
    type_count: usize,
    /// Arena-relative offset of the static-seed value trees, decoded at first static access.
    statics_offset: usize,
    static_count: usize,
}

/// A native (runtime-implemented) method: the intrinsic ABI.
///
/// This is the seam a BCL method crosses to reach Rust -- `System.Console.Write`
/// and friends are intrinsics of this shape. The function receives the runtime
/// context [`Vm`] (heap, console, ...) and the call arguments in declaration
/// order, and returns the method's result (`None` for `void`) or a [`Trap`]. It is
/// documented as a shared seam in `docs/COORDINATION.md`.
pub type IntrinsicFn = fn(&mut Vm, &Module, &[Value]) -> Result<Option<Value>, Trap>;

/// The raw-CIL input to the loader for one managed method. Owned (`Box<[u8]>`, a copy of the PE
/// bytes) by default; under `flash-image` (XIP) a `&'static [u8]` borrow of the flash-resident PE, so
/// the CIL is never copied into RAM. The lazy `on_use` body keeps it as-is for a first-use decode;
/// `code-preload` decodes it at load and keeps only the image.
#[cfg(not(feature = "flash-image"))]
pub type RawCil = Box<[u8]>;
/// The `flash-image` (XIP) form: a `&'static` borrow of the flash-resident PE, so the raw CIL costs
/// no RAM. See the owned default above for the full rationale.
#[cfg(feature = "flash-image")]
pub type RawCil = &'static [u8];

/// A managed method's body storage -- the one place the `code_loading` knob changes representation,
/// chosen at build time so the per-instruction fetch carries no runtime branch and every
/// `Method::Managed` match site stays uniform:
///
/// - default (`on_use`): the raw CIL plus a lazy decode cache -- an uncalled method never decodes,
///   at the cost of a per-call `OnceCell` check.
/// - `code-preload` feature (`preload`): the decoded body held resident -- [`ManagedBody::image`] is
///   a direct field read; all bodies decode once at load.
/// - `code-in-place` feature (`in_place`, XIP): NOTHING decoded is ever resident -- the body is a
///   `&'static` slice of the flash-resident code bytes plus its (byte-offset) exception clauses,
///   and the interpreter decodes one instruction per step straight from flash
///   ([`lamella_cil::decode_at`]), keeping its instruction pointer as a CIL byte offset.
#[derive(Clone)]
pub struct ManagedBody {
    /// The decoded body held resident (the `code-preload` opt-in): a direct per-instruction read.
    #[cfg(feature = "code-preload")]
    image: MethodBodyImage,
    /// The raw CIL, decoded into `decoded` on first use (the `on_use` default). Owned by default; a
    /// `&'static` borrow of the flash PE under `flash-image` (see [`RawCil`]).
    #[cfg(all(not(feature = "code-preload"), not(feature = "code-in-place")))]
    raw_il: RawCil,
    #[cfg(all(not(feature = "code-preload"), not(feature = "code-in-place")))]
    decoded: OnceCell<MethodBodyImage>,
    /// The method's instruction bytes in the flash-resident PE (the `code-in-place` XIP form):
    /// header stripped, decoded per step, never resident.
    #[cfg(feature = "code-in-place")]
    code: &'static [u8],
    /// The method's exception clauses with BYTE-OFFSET regions (the form the file encodes), the
    /// only resident remnant of the body -- empty for the typical unprotected method.
    #[cfg(feature = "code-in-place")]
    handlers: Box<[EhClause]>,
}

impl ManagedBody {
    /// Builds a body from raw CIL bytes (the loader's path): eager decodes now (and drops the raw
    /// bytes); lazy keeps the bytes to decode on first use; in-place keeps only the flash code
    /// slice and the byte-form exception clauses.
    fn from_raw(raw_il: RawCil) -> ManagedBody {
        #[cfg(feature = "code-preload")]
        {
            ManagedBody {
                image: read_method_body(&*raw_il).unwrap_or_else(|_| empty_body()),
            }
        }
        #[cfg(all(not(feature = "code-preload"), not(feature = "code-in-place")))]
        {
            ManagedBody {
                raw_il,
                decoded: OnceCell::new(),
            }
        }
        #[cfg(feature = "code-in-place")]
        {
            match read_body_layout(raw_il) {
                Ok(layout) => ManagedBody {
                    code: raw_il
                        .get(layout.code_offset..layout.code_offset + layout.code_len)
                        .unwrap_or(&[]),
                    handlers: layout.handlers.into_boxed_slice(),
                },
                Err(_) => ManagedBody {
                    code: &[],
                    handlers: Box::new([]),
                },
            }
        }
    }

    /// Builds a body from an already-decoded image (the test / builder path).
    fn from_image(image: MethodBodyImage) -> ManagedBody {
        #[cfg(feature = "code-preload")]
        {
            ManagedBody { image }
        }
        #[cfg(all(not(feature = "code-preload"), not(feature = "code-in-place")))]
        {
            let decoded = OnceCell::new();
            let _ = decoded.set(image);
            #[cfg(not(feature = "flash-image"))]
            let raw_il: RawCil = Vec::new().into_boxed_slice();
            #[cfg(feature = "flash-image")]
            let raw_il: RawCil = &[];
            ManagedBody { raw_il, decoded }
        }
        #[cfg(feature = "code-in-place")]
        {
            match write_method_body(&image) {
                Ok(bytes) => ManagedBody::from_raw(Box::leak(bytes.into_boxed_slice())),
                Err(_) => ManagedBody {
                    code: &[],
                    handlers: Box::new([]),
                },
            }
        }
    }

    /// The decoded body. On the default `on_use` it lazily decodes on first call and caches; under
    /// `code-preload` it is a direct field read (no per-call check). Absent under `code-in-place`,
    /// which never materializes a decoded body.
    #[cfg(not(feature = "code-in-place"))]
    fn image(&self) -> &MethodBodyImage {
        #[cfg(feature = "code-preload")]
        {
            &self.image
        }
        #[cfg(not(feature = "code-preload"))]
        {
            self.decoded
                .get_or_init(|| read_method_body(&*self.raw_il).unwrap_or_else(|_| empty_body()))
        }
    }

    /// The flash-resident instruction bytes the interpreter decodes per step (byte-offset ip).
    #[cfg(feature = "code-in-place")]
    fn code(&self) -> &'static [u8] {
        self.code
    }

    /// The exception clauses, regions in BYTE offsets -- directly comparable to the byte ip.
    #[cfg(feature = "code-in-place")]
    fn eh(&self) -> &[EhClause] {
        &self.handlers
    }

    /// Resident-RAM accounting for [`Module::heap_report`]: `(raw CIL bytes kept, decoded bytes
    /// resident)`. Eager keeps no raw copy and its body is always resident; lazy keeps the raw bytes
    /// and counts the decoded body only once it has decoded; in-place keeps neither -- its only
    /// resident remnant is the byte-form exception-clause table, reported in the first slot.
    fn footprint(&self) -> (usize, usize) {
        #[cfg(feature = "code-preload")]
        {
            (0, decoded_body_size(&self.image))
        }
        #[cfg(all(not(feature = "code-preload"), not(feature = "code-in-place")))]
        {
            #[cfg(not(feature = "flash-image"))]
            let raw_resident = self.raw_il.len();
            #[cfg(feature = "flash-image")]
            let raw_resident = 0;
            (raw_resident, self.decoded.get().map_or(0, decoded_body_size))
        }
        #[cfg(feature = "code-in-place")]
        {
            (self.handlers.len() * core::mem::size_of::<EhClause>(), 0)
        }
    }
}

/// The resident byte size of a decoded body -- its instruction and handler slices (approximate:
/// excludes the occasional `Operand::Switch` jump table).
#[cfg(not(feature = "code-in-place"))]
fn decoded_body_size(body: &MethodBodyImage) -> usize {
    core::mem::size_of_val(body.code.as_ref()) + core::mem::size_of_val(body.handlers.as_ref())
}

/// A callable method: either managed CIL the interpreter executes, or a native
/// intrinsic it invokes directly.
#[derive(Clone)]
pub enum Method {
    /// Managed CIL: the method's [`ManagedBody`] (its representation the `code_loading` level's --
    /// lazy-decoded, preloaded, or in-place from flash) and its argument count.
    Managed {
        /// The method's body storage; see [`ManagedBody`].
        body: ManagedBody,
        /// How many arguments the method takes (a resolved signature replaces this
        /// once `lamella-metadata` is wired in).
        arg_count: u16,
    },
    /// A native intrinsic implemented in Rust.
    Intrinsic {
        /// The Rust implementation.
        func: IntrinsicFn,
        /// The intrinsic's stable registry id (FNV-1a-32 of the fn's own name), stamped at bind time
        /// so the bake records it WITHOUT a fn-pointer comparison (unreliable on wasm32). See
        /// [`crate::intrinsic_registry::intrinsic_id`].
        id: u32,
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

/// An empty method body -- the lazy-decode fallback for the (in practice unreachable) case where a body
/// the loader already decoded once fails to re-decode. Running it traps at once (`FellThroughEnd`)
/// rather than producing wrong results.
#[cfg(not(feature = "code-in-place"))]
fn empty_body() -> MethodBodyImage {
    MethodBodyImage {
        max_stack: 0,
        init_locals: false,
        local_var_sig: None,
        code: Vec::new().into_boxed_slice(),
        handlers: Vec::new().into_boxed_slice(),
    }
}

/// The variable-argument info a vararg `call` site carries beyond the fixed parameters (II.23.2.1):
/// what the interpreter threads to the callee so `arglist` / `System.ArgIterator` can walk the extra
/// arguments. The fixed args are passed like any call; these describe the variadic tail.
#[derive(Clone, Debug)]
pub struct VarargSite {
    /// The total stack arguments the call passes (`this`? + fixed + variable) -- the count to take
    /// off the caller's evaluation stack (vs the callee's defined fixed-parameter count).
    pub total_args: u16,
    /// The callee-frame argument index at which the variable arguments begin (`this`? + fixed count).
    pub vararg_start: u16,
    /// The asm-folded type handle of each variable argument, in order -- so `arglist` builds a
    /// correctly-typed `TypedReference` for each, the same handle `ldtoken` / `refanytype` carry.
    pub var_types: Vec<u64>,
}

/// The marshaling kind of one P/Invoke parameter, derived from the managed signature at
/// load: a SCALAR passes its integer value through unchanged (every integer width and a
/// char/bool ride the one 64-bit argument slot of the host call ABI); an ANSI STRING
/// marshals the managed string to NUL-terminated bytes the callee reads by pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PInvokeParam {
    /// An integer/pointer-width value, passed through as its 64-bit slot.
    Scalar,
    /// A managed `string` marshaled to a NUL-terminated ANSI/UTF-8 byte buffer.
    AnsiString,
}

/// What a P/Invoke returns, mapped back onto the evaluation stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PInvokeReturn {
    /// `void`: nothing is pushed.
    Void,
    /// A 32-bit integer (the host call's 64-bit return truncated).
    Int32,
}

/// A `[DllImport]` target recorded at load from a bodyless PinvokeImpl method's `ImplMap`
/// row (II.22.22): the native library + entry names and the marshaling shape of the
/// signature. A `call` of such a method dispatches through the [`crate::Vm`]'s host seam
/// (a host runner installs one; a device build leaves it unset and the call traps).
#[derive(Clone, Debug)]
pub struct PInvokeTarget {
    /// The native library (`DllImport("kernel32.dll")`).
    pub module: String,
    /// The entry-point symbol within it.
    pub entry: String,
    /// Each parameter's marshaling kind, in order.
    pub params: Vec<PInvokeParam>,
    /// The return mapping.
    pub returns: PInvokeReturn,
}

/// The runtime primitive kind a boxed value must display as. The stack [`Value`](crate::Value)
/// collapses `bool` and `char` into [`Value::Int32`](crate::Value::Int32) (III.1.1.1), so a box
/// alone cannot tell `box bool` `1` (`"True"`) from `box int` `1` (`"1"`), nor `box char` `66`
/// (`"B"`) from the integer `66`. The loader records this from the `box` operand's metadata type
/// name (`System.Boolean` / `System.Char`) -- a name that resolves from the delta's own metadata
/// even when corlib is absent from the runtime, which matters because the incremental REPL never
/// loads corlib into the VM (so a display-time `TypeRef`-to-type resolution is unavailable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxedPrimitive {
    /// `System.Boolean`: displays `"True"` when the underlying value is non-zero, else `"False"`.
    Boolean,
    /// `System.Char`: displays the underlying UTF-16 code unit as its character.
    Char,
}

/// A `const` field's Constant-row value (II.22.9), decoded at load for
/// `FieldInfo.GetRawConstantValue`: the KIND is kept (a `bool` constant materializes as a
/// Boolean-boxed value, not a bare int) so the raw constant round-trips a cast exactly as
/// .NET's does. Lives in the module's LIVE reflection maps (like [`LoadedAttribute`]);
/// the baked tier trims reflection metadata by design, so it is not serialized.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldConstant {
    /// A `bool` constant.
    Bool(bool),
    /// A `char` constant (a UTF-16 code unit).
    Char(u16),
    /// An integer constant, sign-extended; `wide` marks a 64-bit source (`long`/`ulong`).
    Int {
        /// The value, sign-extended from its source width.
        value: i64,
        /// Whether the source type is 64-bit, so it materializes as `Int64`.
        wide: bool,
    },
    /// A `float` constant.
    #[cfg(feature = "float")]
    R4(f32),
    /// A `double` constant.
    #[cfg(feature = "float")]
    R8(f64),
    /// A `string` constant, as UTF-16 code units.
    Str(Box<[u16]>),
    /// A `null` constant (a reference-typed const).
    Null,
}

/// A collection of methods, the call tokens that name them, and the strings
/// `ldstr` loads.
#[derive(Clone, Default)]
pub struct Module {
    methods: Vec<Method>,
    /// Call-token bindings before [`Module::freeze`] drains them into the arena, and any bound
    /// AFTER the freeze (an incremental REPL delta) -- the loader's scratch and the small mutable
    /// overlay in one. Runtime lookups try the frozen tables first, then here.
    by_token: BTreeMap<u64, MethodId>,
    /// The frozen flat tables (little-endian, explicit widths) every profile reads at runtime --
    /// what the bake step later writes to flash. Grows table by table as the flatten lands.
    arena: TableArena,
    /// The frozen table views over `arena` -- one per flattened builder map.
    frozen: FrozenTables,
    /// The baked method table, when this module BOOTED from an image ([`Module::from_baked`]):
    /// method records read from flash, only the intrinsic column resident. `None` on a live
    /// (loader-built) module, which uses `methods`.
    #[cfg(feature = "code-in-place")]
    baked: Option<BakedMethods>,
    /// The signature interner: every distinct "name + parameter types" dispatch key maps to one
    /// stable 1-based `u32` id (0 reserved), so dispatch compares integers and the tables store
    /// ids. `sig_pool` owns the text (indexed by `id - 1`) for the load-time consumers that still
    /// speak strings (cross-assembly vtable seeding); the runtime never reads the text.
    sig_lookup: BTreeMap<String, u32>,
    sig_pool: Vec<String>,
    /// `ldstr` strings as little-endian UTF-16 BYTES -- the same shape the frozen pool
    /// stores, so builder and arena share one representation.
    strings: BTreeMap<u64, Box<[u8]>>,
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
    /// A `newarr` element-type token mapped to the element type's primitive kind
    /// ([`PrimKind`]: byte width + read/write encoding). Only sized primitive element types
    /// appear; a reference / value-type element array is absent. `newarr` packs an array of a
    /// mapped kind ([`crate::object::ArrayStorage::Packed`]) -- element storage at the true
    /// byte width -- which is also what tells a `byte[]` from an `int[]` for `System.Buffer`.
    array_prim_kinds: BTreeMap<u64, PrimKind>,
    /// A virtual method's vtable slot (only virtual methods appear).
    method_slots: BTreeMap<MethodId, u32>,
    /// A static field token mapped to its storage slot in the [`crate::interp::Vm`].
    static_fields: BTreeMap<u64, usize>,
    /// The zero value of each static field, indexed by storage slot.
    static_defaults: Vec<Value>,
    /// The static constructors (`.cctor`), to run before the entry point.
    static_ctors: Vec<MethodId>,
    /// A type's `.cctor`, for the LAZY trigger (II.10.5.3): the interpreter runs it on the
    /// type's first static-field access / method entry, marking it run in the `Vm`. Runtime-
    /// side only (not baked): a baked image boots its cctors eagerly.
    cctor_types: BTreeMap<TypeId, MethodId>,
    /// A bodyless PinvokeImpl method's `[DllImport]` target, keyed by the method token's
    /// asm-folded handle -- consulted where an unresolvable `call` would otherwise trap.
    /// Runtime-side only (a baked device image has no host FFI).
    pinvoke_targets: BTreeMap<u64, PInvokeTarget>,
    /// The [`TypeId`] owning each contiguous run of static storage slots
    /// (`start, end, type`), so a static-field access can find the type whose cctor it
    /// triggers. Loader-recorded, runtime-side only.
    static_slot_types: Vec<(u32, u32, TypeId)>,
    /// A `TypeDef` token mapped to its [`TypeId`] (for `castclass` / `isinst`).
    type_tokens: BTreeMap<u64, TypeId>,
    /// The reverse of `type_tokens`: a [`TypeId`] mapped to its declaring type's asm-folded
    /// `TypeDef` token (its `Type` handle), so `Object.GetType()` on a reference instance can
    /// hand back the same handle `typeof` / `Type.Name` use.
    type_handles: BTreeMap<TypeId, u64>,
    /// A `callvirt` token mapped to its target's `(signature key, arg count)`, for
    /// dispatching interface / abstract methods whose target has no vtable slot (and
    /// may have no resolvable body).
    call_targets: BTreeMap<u64, (u32, u16)>,
    /// A vararg `call` token mapped to its [`VarargSite`]: the variable-argument info the call
    /// passes beyond the fixed parameters, so the interpreter takes the full stack-argument count
    /// and threads the variable arguments to the callee for `arglist` / `ArgIterator`.
    vararg_sites: BTreeMap<u64, VarargSite>,
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
    vtable_slot_keys: BTreeMap<TypeId, Vec<(u32, MethodId)>>,
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
    /// The [`BoxedPrimitive`] display kind of a `box` operand type token, keyed by the operand's
    /// asm-folded token -- the same handle a resulting [`Object::Boxed`](crate::Object::Boxed)
    /// carries. The loader records it from the operand's metadata name for `System.Boolean` /
    /// `System.Char` only: the two primitives the stack `Value` cannot tell from a plain `Int32`,
    /// so a boxed one would otherwise display as its raw integer. Consulted by the boxed-value
    /// display path (`Object.ToString` / `WriteLine(object)`, [`crate::intrinsics`]) via the box's
    /// own token, so it needs no corlib loaded in the runtime (the incremental REPL loads none).
    box_primitives: BTreeMap<u64, BoxedPrimitive>,
    /// The custom attributes applied to a target (a type or a member), keyed by the target's
    /// asm-folded metadata token -- the `Type` / `MemberInfo` handle a `GetCustomAttributes`
    /// receiver carries. Each is decoded + resolved at load (its ctor, positional, and named
    /// field values); the interpreter instantiates them on demand. A target with no recorded
    /// (instantiable) attributes is absent.
    custom_attributes: BTreeMap<u64, Vec<LoadedAttribute>>,
    /// Per-field reflection flags, keyed by the field's asm-folded token: `(is_literal,
    /// is_static)` -- `FieldInfo.IsLiteral` / `IsStatic`. Live-tier only (see [`FieldConstant`]).
    field_meta: BTreeMap<u64, (bool, bool)>,
    /// A `literal` field's decoded Constant-row value, keyed by the field's asm-folded token --
    /// what `FieldInfo.GetRawConstantValue` materializes. Live-tier only.
    field_constants: BTreeMap<u64, FieldConstant>,
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
    /// Each method's parameters (`MethodBase.GetParameters` -> `ParameterInfo[]`), keyed by the
    /// method's asm-folded handle. The `NETMFv4_4` reflection tier only.
    method_params: BTreeMap<u64, Vec<MethodParam>>,
    /// Each loaded assembly's display name (`Assembly.FullName`), keyed by its assembly id. The
    /// `NETMFv4_4` reflection tier only.
    assembly_names: BTreeMap<u8, String>,
    /// Each loaded assembly's declared type handles (`Assembly.GetTypes`, `<Module>` excluded), keyed
    /// by its assembly id. The `NETMFv4_4` reflection tier only.
    assembly_types: BTreeMap<u8, Vec<u64>>,
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

/// The synthetic attribute-store key for one of a method's `Param` rows (II.22.33): the
/// method's own asm-folded handle tagged in bits `asm_key` never sets -- bit 63 marks
/// "parameter attributes", bits 40..56 carry the row's 1-based sequence (0 = the return
/// value's row). Custom attributes on a `Param` row are recorded under this key at load,
/// so `ParameterInfo.GetCustomAttributes` reads the same store as every other target.
#[must_use]
pub fn param_attr_key(method_handle: u64, sequence: u16) -> u64 {
    method_handle | (1 << 63) | ((sequence as u64) << 40)
}

/// The frozen, flat form of the module's binding tables: one growable little-endian byte arena
/// the table views index into. Every record uses explicitly-sized fields (`u32`/`u64`, never a
/// pointer or `usize`) at fixed offsets, so the SAME bytes mean the same tables on any target --
/// owned in RAM on a host today, and byte-for-byte what the bake step later writes to flash for
/// a device to borrow.
#[derive(Clone, Default)]
struct TableArena {
    /// Owned while the loader builds and freezes; borrowed (`'static`, the baked image in flash)
    /// once a module boots [`Module::from_baked`] -- the tables then cost no RAM at all.
    bytes: alloc::borrow::Cow<'static, [u8]>,
}

impl TableArena {
    /// The resident (owned) byte count -- 0 once the arena borrows a baked image.
    fn resident_len(&self) -> usize {
        match &self.bytes {
            alloc::borrow::Cow::Owned(bytes) => bytes.len(),
            alloc::borrow::Cow::Borrowed(_) => 0,
        }
    }

    fn read_u32(&self, at: usize) -> Option<u32> {
        self.bytes
            .get(at..at.checked_add(4)?)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
    }

    fn read_u64(&self, at: usize) -> Option<u64> {
        self.bytes
            .get(at..at.checked_add(8)?)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
    }

    fn push_u32(&mut self, value: u32) {
        self.bytes.to_mut().extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(&mut self, value: u64) {
        self.bytes.to_mut().extend_from_slice(&value.to_le_bytes());
    }

    fn write_u32(&mut self, at: usize, value: u32) {
        if let Some(slot) = at
            .checked_add(4)
            .and_then(|end| self.bytes.to_mut().get_mut(at..end))
        {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// One assembly's dense token-row column in the arena for one metadata table kind (`MethodDef`
/// call bindings, `MemberRef` cross-assembly bindings, ...): entry `row - 1` holds the bound
/// value plus one, with `0` meaning the row is unbound in the frozen image. Rows are contiguous
/// from 1, so a token lookup is a direct index -- no search.
#[derive(Clone, Copy, Default)]
struct DenseTokenColumn {
    asm: u8,
    table: u32,
    rows: u32,
    offset: usize,
}

/// A dense `u32` column in the arena indexed by a contiguous id (a [`MethodId`] or [`TypeId`]):
/// entry `id` holds the bound value plus one, `0` meaning unbound. Lookup is a bounds check and
/// one 4-byte read; an id past `entries` (added after the freeze) falls back to the mutable map.
#[derive(Clone, Copy, Default)]
struct DenseIdColumn {
    offset: usize,
    entries: usize,
}

impl DenseIdColumn {
    fn get(&self, arena: &TableArena, id: u32) -> Option<u32> {
        if (id as usize) < self.entries {
            let bound = arena.read_u32(self.offset + id as usize * 4).unwrap_or(0);
            if bound != 0 {
                return Some(bound - 1);
            }
        }
        None
    }
}

/// A sorted `(u64 key -> u32 value)` table in the arena: 12-byte records binary-searched by
/// key. The frozen home of sparse token bindings (`MemberRef` call sites and any key outside a
/// dense column's rows).
#[derive(Clone, Copy, Default)]
struct SortedTokenTable {
    offset: usize,
    entries: usize,
}

impl SortedTokenTable {
    const RECORD: usize = 12;

    fn get(&self, arena: &TableArena, key: u64) -> Option<u32> {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = self.offset + middle * SortedTokenTable::RECORD;
            let probe = arena.read_u64(at)?;
            match probe.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return arena.read_u32(at + 8),
            }
        }
        None
    }

    /// The table's records read back out (a re-freeze carries them into the rebuilt table).
    fn entries_of(&self, arena: &TableArena) -> Vec<(u64, u32)> {
        (0..self.entries)
            .filter_map(|entry| {
                let at = self.offset + entry * SortedTokenTable::RECORD;
                Some((arena.read_u64(at)?, arena.read_u32(at + 8)?))
            })
            .collect()
    }

    /// Sorts `entries` by key and writes them as a fresh table at the arena's end.
    fn write(arena: &mut TableArena, mut entries: Vec<(u64, u32)>) -> SortedTokenTable {
        entries.sort_unstable_by_key(|(key, _)| *key);
        let offset = arena.bytes.len();
        for (key, value) in &entries {
            arena.push_u64(*key);
            arena.push_u32(*value);
        }
        SortedTokenTable {
            offset,
            entries: entries.len(),
        }
    }
}

/// A sorted `u64` presence set in the arena: 8-byte records binary-searched by key -- the frozen
/// form of a `BTreeSet<u64>` (token classifications such as "is a delegate ctor").
#[derive(Clone, Copy, Default)]
struct SortedTokenSet {
    offset: usize,
    entries: usize,
}

impl SortedTokenSet {
    fn contains(&self, arena: &TableArena, key: u64) -> bool {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let Some(probe) = arena.read_u64(self.offset + middle * 8) else {
                return false;
            };
            match probe.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    fn entries_of(&self, arena: &TableArena) -> Vec<u64> {
        (0..self.entries)
            .filter_map(|entry| arena.read_u64(self.offset + entry * 8))
            .collect()
    }

    fn write(arena: &mut TableArena, mut keys: Vec<u64>) -> SortedTokenSet {
        keys.sort_unstable();
        let offset = arena.bytes.len();
        for key in &keys {
            arena.push_u64(*key);
        }
        SortedTokenSet {
            offset,
            entries: keys.len(),
        }
    }
}

/// A sorted `(u64 key -> byte blob)` table: 16-byte records (`key`, payload offset, payload
/// length) binary-searched by key, the payload bytes living elsewhere in the arena -- the frozen
/// form of the RVA-initializer map (`RuntimeHelpers.InitializeArray` blobs).
#[derive(Clone, Copy, Default)]
struct SortedBlobTable {
    offset: usize,
    entries: usize,
}

impl SortedBlobTable {
    const RECORD: usize = 16;

    fn get<'m>(&self, arena: &'m TableArena, key: u64) -> Option<&'m [u8]> {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = self.offset + middle * SortedBlobTable::RECORD;
            let probe = arena.read_u64(at)?;
            match probe.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => {
                    let payload = arena.read_u32(at + 8)? as usize;
                    let length = arena.read_u32(at + 12)? as usize;
                    return arena.bytes.get(payload..payload.checked_add(length)?);
                }
            }
        }
        None
    }

    /// The records read back (payload offsets stay valid -- the arena only ever appends).
    fn entries_of(&self, arena: &TableArena) -> Vec<(u64, u32, u32)> {
        (0..self.entries)
            .filter_map(|entry| {
                let at = self.offset + entry * SortedBlobTable::RECORD;
                Some((
                    arena.read_u64(at)?,
                    arena.read_u32(at + 8)?,
                    arena.read_u32(at + 12)?,
                ))
            })
            .collect()
    }

    fn write(arena: &mut TableArena, mut records: Vec<(u64, u32, u32)>) -> SortedBlobTable {
        records.sort_unstable_by_key(|(key, _, _)| *key);
        let offset = arena.bytes.len();
        for (key, payload, length) in &records {
            arena.push_u64(*key);
            arena.push_u32(*payload);
            arena.push_u32(*length);
        }
        SortedBlobTable {
            offset,
            entries: records.len(),
        }
    }
}

/// A dense `u64` column indexed by a contiguous id: entry `id` holds the value, `0` meaning
/// absent (an asm-folded token is never 0). The frozen form of `type_handles` (`TypeId` -> its
/// `Type` handle).
#[derive(Clone, Copy, Default)]
struct DenseU64Column {
    offset: usize,
    entries: usize,
}

impl DenseU64Column {
    fn get(&self, arena: &TableArena, id: u32) -> Option<u64> {
        if (id as usize) < self.entries {
            let value = arena.read_u64(self.offset + id as usize * 8).unwrap_or(0);
            if value != 0 {
                return Some(value);
            }
        }
        None
    }
}

/// One assembly's dense token-row column of `u64` entries for one metadata table kind (the
/// `call_targets` shape: `(SigId << 16) | arg count`, 0 = unbound).
#[derive(Clone, Copy, Default)]
struct DenseWideColumn {
    asm: u8,
    table: u32,
    rows: u32,
    offset: usize,
}

/// Packs an explicit-override key (`(TypeId, overridden method handle)`) into one orderable
/// `u64` table key: the handle occupies its full 40 bits (`asm << 32 | token`), the type id the
/// 24 bits above. `None` for an id too large to pack (it then stays in the mutable map).
fn packed_override_key(type_id: TypeId, handle: u64) -> Option<u64> {
    if type_id < (1 << 24) && handle < (1 << 40) {
        Some(((type_id as u64) << 40) | handle)
    } else {
        None
    }
}

/// The frozen views over [`Module`]'s arena, one per flattened table -- together with the arena
/// bytes they index, the exact content the bake step writes to flash.
#[derive(Clone, Default)]
struct FrozenTables {
    /// Per-assembly dense `MethodDef` columns of the frozen `by_token` (direct index by row).
    token_columns: Vec<DenseTokenColumn>,
    /// The sparse remainder of the frozen `by_token` (`MemberRef` bindings), binary-searched.
    token_spill: SortedTokenTable,
    /// The frozen `method_type` (declaring type per method), a dense [`MethodId`] column.
    method_type: DenseIdColumn,
    /// The frozen `method_slots` (vtable slot per virtual method), a dense [`MethodId`] column.
    method_slot: DenseIdColumn,
    /// The frozen `type_tokens` (`TypeDef` token -> [`TypeId`], for `castclass` / `isinst`).
    type_tokens: SortedTokenTable,
    /// The frozen `type_handles` (a [`TypeId`]'s `Type` handle), a dense column.
    type_handles: DenseU64Column,
    /// The frozen `field_slots` (field token -> instance slot; the `ldfld`/`stfld` path).
    field_slots: SortedTokenTable,
    /// The frozen `field_types` (field token -> declaring [`TypeId`]).
    field_types: SortedTokenTable,
    /// The frozen `static_fields` (static field token -> [`crate::interp::Vm`] storage slot).
    static_fields: SortedTokenTable,
    /// The frozen `array_prim_kinds` (`newarr` element token -> [`PrimKind`] code).
    array_prim_kinds: SortedTokenTable,
    /// The frozen `enum_widths` (enum handle -> underlying byte width).
    enum_widths: SortedTokenTable,
    /// The frozen `delegate_invokes` (delegate `Invoke` token -> parameter count).
    delegate_invokes: SortedTokenTable,
    /// The frozen `md_array_ctors` (multi-dimensional array ctor token -> rank).
    md_array_ctors: SortedTokenTable,
    /// The frozen `string_builder_ctors` (`StringBuilder` ctor token -> parameter count).
    string_builder_ctors: SortedTokenTable,
    /// The frozen `list_ctors` (`ArrayList` ctor token -> parameter count).
    list_ctors: SortedTokenTable,
    /// The frozen `finalizers` ([`TypeId`] -> its `Finalize` method).
    finalizers: SortedTokenTable,
    /// The frozen `type_sizes` (value-type token -> byte size, the `sizeof` path).
    type_sizes: SortedTokenTable,
    /// The frozen `catch_type_tags` (catch-clause type token -> exception tag).
    catch_type_tags: SortedTokenTable,
    /// The frozen `explicit_overrides`, keyed by [`packed_override_key`].
    explicit_overrides: SortedTokenTable,
    /// The frozen `delegate_ctors` (`newobj` tokens constructing a delegate).
    delegate_ctors: SortedTokenSet,
    /// The frozen `enum_wide` (enum handles with a 64-bit underlying type).
    enum_wide: SortedTokenSet,
    /// The frozen `enum_flags` (enum handles carrying `[Flags]`).
    enum_flags: SortedTokenSet,
    /// The frozen `object_type_tokens` (type-test tokens naming `System.Object`).
    object_type_tokens: SortedTokenSet,
    /// The frozen `string_type_tokens` (type-test tokens naming `System.String`).
    string_type_tokens: SortedTokenSet,
    /// The frozen `value_type_ctors` (`newobj` tokens whose declaring type is a struct).
    value_type_ctors: SortedTokenSet,
    /// The frozen `field_rva_data` (field token -> RVA initializer blob).
    field_rva_data: SortedBlobTable,
    /// The frozen `ldstr` pool (token -> little-endian UTF-16 bytes).
    strings: SortedBlobTable,
    /// The frozen reflection NAME POOL: every distinct member/type name once, UTF-8, sorted --
    /// a name's id IS its sorted rank, so a runtime string finds its id by binary search over
    /// pool slices, and every name table stores ids. Rebuilt whole on a (rare) re-freeze.
    name_pool: NamePool,
    /// The frozen `type_names` (asm-folded token -> name id).
    type_names: SortedTokenTable,
    /// The frozen `type_full_names` ([`TypeId`] -> full-name id), a dense column.
    type_full_names: DenseIdColumn,
    /// The frozen `Type.GetField` index: `(handle << 24 | name id)` -> field handle (split u32s).
    fields_by_name: SortedWideTable,
    /// The frozen `Type.GetMethod` index, same key shape.
    methods_by_name: SortedWideTable,
    /// The frozen `Type.GetProperty` index, same key shape.
    properties_by_name: SortedWideTable,
    /// The frozen `name_to_handle` (full-name id -> type handle, split u32s).
    name_to_handle: SortedWideTable,
    /// The frozen `type_ctors` (type handle -> parameterless ctor).
    type_ctors: SortedTokenTable,
    /// The frozen `method_attrs` (method handle -> flags).
    method_attrs: SortedTokenTable,
    /// The frozen `member_type_handle` (member handle -> its type's handle, split u32s).
    member_type_handle: SortedWideTable,
    /// The frozen `reflect_types` (type handle -> namespace/full-name ids, `Is*` flag bits,
    /// base handle), 28-byte records.
    reflect_types: SortedReflectTable,
    /// The frozen `type_fields`: handle -> (record offset, count) into 16-byte field records.
    type_fields: SortedWideTable,
    /// The frozen `type_methods`, same shape as `type_fields`.
    type_methods: SortedWideTable,
    /// The frozen `method_params`: handle -> (offset, count) into 16-byte param records
    /// (type handle split, name id).
    method_params: SortedWideTable,
    /// The frozen `type_ctors_list`: handle -> (offset, count) into 16-byte ctor records.
    type_ctors_list: SortedWideTable,
    /// The frozen `box_primitives` (value-type token -> display kind).
    box_primitives: SortedTokenTable,
    /// The frozen `assembly_names` (assembly id -> name id).
    assembly_names: SortedTokenTable,
    /// The frozen `assembly_types`: assembly id -> (offset, count) into u64 handle records.
    assembly_types: SortedWideTable,
    /// The frozen `enum_constants`: enum handle -> (offset, count) into 16-byte records
    /// (underlying value as u64 bits, name-pool id) ordered ascending by value.
    enum_constants: SortedWideTable,
    /// The frozen virtual/interface signature dispatch: per [`TypeId`], a packed run of
    /// `(SigId, method)` records scanned linearly -- the frozen shape of the old per-type map,
    /// keeping dispatch's locality (a handful of contiguous records per type).
    sig_virtual_runs: DenseU64Column,
    /// The frozen non-virtual signature dispatch (the absent-static-target fallback), same shape.
    sig_nonvirtual_runs: DenseU64Column,
    /// Dense per-`(assembly, table)` columns of the frozen `call_targets`: entry `row - 1` packs
    /// `(SigId << 16) | arg count` (a `SigId` is nonzero, so 0 means unbound). `callvirt` tokens
    /// are `MemberRef`/`MethodDef` rows -- contiguous -- so the hot lookup is a direct index.
    call_target_columns: Vec<DenseWideColumn>,
    /// The sparse remainder of the frozen `call_targets`.
    call_targets: SortedWideTable,
    /// The frozen `vtable_slot_keys`: per [`TypeId`], a packed `(run offset + 1) << 32 | count`
    /// into slot-run records (interned sig id, method) laid down in slot order.
    vtable_slot_runs: DenseU64Column,
    /// The frozen vtables: per [`TypeId`], a packed run of [`MethodId`]s in slot order --
    /// `vtable_entry` is a direct index into the run.
    vtable_runs: DenseU64Column,
    /// The frozen direct-interface lists: per [`TypeId`], a packed run of interface [`TypeId`]s.
    interface_runs: DenseU64Column,
    /// The frozen `array_defaults` (`newarr` element token -> its elements' scalar zero value).
    array_defaults: SortedValueTable,
}

/// A sorted `(u64 key -> scalar Value)` table: 24-byte records (`key`, payload bits, tag + pad)
/// binary-searched by key -- the frozen form of the `newarr` element-default map. Only scalar
/// values encode ([`encode_scalar_value`]); anything else stays in the builder map.
#[derive(Clone, Copy, Default)]
struct SortedValueTable {
    offset: usize,
    entries: usize,
}

impl SortedValueTable {
    const RECORD: usize = 24;

    fn get(&self, arena: &TableArena, key: u64) -> Option<Value> {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = self.offset + middle * SortedValueTable::RECORD;
            let probe = arena.read_u64(at)?;
            match probe.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => {
                    return decode_scalar_value(arena.read_u32(at + 16)?, arena.read_u64(at + 8)?);
                }
            }
        }
        None
    }

    fn entries_of(&self, arena: &TableArena) -> Vec<(u64, u64, u32)> {
        (0..self.entries)
            .filter_map(|entry| {
                let at = self.offset + entry * SortedValueTable::RECORD;
                Some((
                    arena.read_u64(at)?,
                    arena.read_u64(at + 8)?,
                    arena.read_u32(at + 16)?,
                ))
            })
            .collect()
    }

    fn write(arena: &mut TableArena, mut records: Vec<(u64, u64, u32)>) -> SortedValueTable {
        records.sort_unstable_by_key(|(key, _, _)| *key);
        let offset = arena.bytes.len();
        for (key, bits, tag) in &records {
            arena.push_u64(*key);
            arena.push_u64(*bits);
            arena.push_u32(*tag);
            arena.push_u32(0);
        }
        SortedValueTable {
            offset,
            entries: records.len(),
        }
    }
}

/// The frozen name pool: `entries` `(offset, length)` records ordered by the names' byte order
/// (UTF-8 `str` ordering IS byte ordering), so a name's id is its rank and a runtime string
/// resolves to an id by binary-searching the pool slices.
#[derive(Clone, Copy, Default)]
struct NamePool {
    offsets: usize,
    entries: usize,
}

impl NamePool {
    fn str_at<'m>(&self, arena: &'m TableArena, id: u32) -> Option<&'m str> {
        if id as usize >= self.entries {
            return None;
        }
        let at = self.offsets + id as usize * 8;
        let offset = arena.read_u32(at)? as usize;
        let length = arena.read_u32(at + 4)? as usize;
        core::str::from_utf8(arena.bytes.get(offset..offset.checked_add(length)?)?).ok()
    }

    fn id_of(&self, arena: &TableArena, name: &str) -> Option<u32> {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let probe = self.str_at(arena, middle as u32)?;
            match probe.cmp(name) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Some(middle as u32),
            }
        }
        None
    }

    /// Writes `names` (pre-sorted, deduplicated) as the pool: the UTF-8 bytes, then the rank-
    /// ordered offset records.
    fn write(arena: &mut TableArena, names: &[String]) -> NamePool {
        let mut records: Vec<(u32, u32)> = Vec::with_capacity(names.len());
        for name in names {
            records.push((arena.bytes.len() as u32, name.len() as u32));
            arena.bytes.to_mut().extend_from_slice(name.as_bytes());
        }
        let offsets = arena.bytes.len();
        for (offset, length) in &records {
            arena.push_u32(*offset);
            arena.push_u32(*length);
        }
        NamePool {
            offsets,
            entries: names.len(),
        }
    }
}

/// A sorted `(u64 key -> ReflectType record)` table: 28-byte records (`key`, namespace id,
/// full-name id, `Is*` flag bits, base handle split) binary-searched by key.
#[derive(Clone, Copy, Default)]
struct SortedReflectTable {
    offset: usize,
    entries: usize,
}

impl SortedReflectTable {
    const RECORD: usize = 28;

    fn get(&self, arena: &TableArena, key: u64) -> Option<(u32, u32, u32, u64)> {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = self.offset + middle * SortedReflectTable::RECORD;
            let probe = arena.read_u64(at)?;
            match probe.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => {
                    let base = (u64::from(arena.read_u32(at + 20)?) << 32)
                        | u64::from(arena.read_u32(at + 24)?);
                    return Some((
                        arena.read_u32(at + 8)?,
                        arena.read_u32(at + 12)?,
                        arena.read_u32(at + 16)?,
                        base,
                    ));
                }
            }
        }
        None
    }

    fn entries_of(&self, arena: &TableArena) -> Vec<(u64, u32, u32, u32, u64)> {
        (0..self.entries)
            .filter_map(|entry| {
                let at = self.offset + entry * SortedReflectTable::RECORD;
                let base = (u64::from(arena.read_u32(at + 20)?) << 32)
                    | u64::from(arena.read_u32(at + 24)?);
                Some((
                    arena.read_u64(at)?,
                    arena.read_u32(at + 8)?,
                    arena.read_u32(at + 12)?,
                    arena.read_u32(at + 16)?,
                    base,
                ))
            })
            .collect()
    }

    fn write(arena: &mut TableArena, mut records: Vec<(u64, u32, u32, u32, u64)>) -> SortedReflectTable {
        records.sort_unstable_by_key(|(key, ..)| *key);
        let offset = arena.bytes.len();
        for (key, namespace, full, flags, base) in &records {
            arena.push_u64(*key);
            arena.push_u32(*namespace);
            arena.push_u32(*full);
            arena.push_u32(*flags);
            arena.push_u32((*base >> 32) as u32);
            arena.push_u32(*base as u32);
        }
        SortedReflectTable {
            offset,
            entries: records.len(),
        }
    }
}

/// The `Is*` bits of a frozen [`ReflectType`] record.
const REFLECT_ENUM: u32 = 1;
const REFLECT_VALUE_TYPE: u32 = 2;
const REFLECT_INTERFACE: u32 = 4;
const REFLECT_ABSTRACT: u32 = 8;
const REFLECT_PUBLIC: u32 = 16;

/// One way a program exceeds the baking build's capability profile -- found at BAKE time by
/// [`Module::validate_profile`], so a deployment fails on the workstation with a nameable
/// method instead of trapping on the device.
#[cfg(feature = "code-in-place")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileViolation {
    /// The method uses an opcode this build's feature set traps on (a float constant on the
    /// no-float tier, a throw on the no-exceptions tier, ...).
    UnsupportedOpcode {
        /// The offending method.
        method: MethodId,
        /// Its recorded display name, when the build carries debug names.
        name: Option<String>,
        /// The opcode's mnemonic.
        opcode: &'static str,
    },
    /// A call site in reachable code resolves to nothing under this profile -- typically a BCL
    /// method whose intrinsic the feature set compiles out.
    UnresolvedCall {
        /// The calling method.
        method: MethodId,
        /// Its recorded display name, when the build carries debug names.
        name: Option<String>,
        /// The unresolved token.
        token: u32,
    },
}

#[cfg(feature = "code-in-place")]
impl core::fmt::Display for ProfileViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let describe = |f: &mut core::fmt::Formatter<'_>, method: &MethodId, name: &Option<String>| {
            match name {
                Some(name) => write!(f, "method {method} ({name})"),
                None => write!(f, "method {method}"),
            }
        };
        match self {
            ProfileViolation::UnsupportedOpcode {
                method,
                name,
                opcode,
            } => {
                describe(f, method, name)?;
                write!(f, " uses `{opcode}`, which this profile does not support")
            }
            ProfileViolation::UnresolvedCall {
                method,
                name,
                token,
            } => {
                describe(f, method, name)?;
                write!(
                    f,
                    " calls token 0x{token:08X}, which resolves to nothing under this profile"
                )
            }
        }
    }
}

/// Why a module could not bake to (or boot from) an image.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BakeError {
    /// A bound intrinsic has no registry id -- add it to `intrinsic_registry`.
    UnregisteredIntrinsic(MethodId),
    /// The image bytes are not a Lamella image (bad magic / truncated).
    NotAnImage,
    /// The image's format version is not this build's.
    WrongVersion(u32),
    /// The image records an intrinsic id this build's registry lacks (a profile mismatch).
    MissingIntrinsic(u32),
    /// The image's recorded content checksum does not match its bytes (corruption / truncation /
    /// a partially-written flash image).
    ChecksumMismatch {
        /// The checksum recorded in the image's header (word 18).
        stored: u64,
        /// The checksum recomputed from the image's directory + arena as read.
        computed: u64,
    },
}

impl core::fmt::Display for BakeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BakeError::UnregisteredIntrinsic(id) => {
                write!(f, "method {id} is bound to an unregistered intrinsic")
            }
            BakeError::NotAnImage => f.write_str("not a Lamella baked image"),
            BakeError::WrongVersion(version) => {
                write!(f, "baked image format version {version} is not supported")
            }
            BakeError::MissingIntrinsic(id) => {
                write!(f, "image needs intrinsic 0x{id:08X}, absent from this build")
            }
            BakeError::ChecksumMismatch { stored, computed } => {
                write!(f, "baked image checksum mismatch: header {stored:#018x}, computed {computed:#018x}")
            }
        }
    }
}

/// Appends one image-directory word.
#[cfg(feature = "code-in-place")]
fn image_push(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Reads the `index`th image-directory word.
#[cfg(feature = "code-in-place")]
fn image_word(bytes: &[u8], index: usize) -> Option<u64> {
    let at = index * 8;
    bytes
        .get(at..at + 8)
        .and_then(|word| word.try_into().ok())
        .map(u64::from_le_bytes)
}

/// CRC-64/ECMA-182 (poly 0x42F0E1EBA9EA3693, init 0, no reflection) over a byte run, updating `crc`.
/// A baked image's content checksum uses this: strong error detection for flash integrity, and a
/// 64-bit content id for deploy-skip -- a false collision would skip a NEEDED redeploy, so the 32-bit
/// wire CRC is too few bits here.
#[cfg(feature = "code-in-place")]
fn crc64_update(mut crc: u64, bytes: &[u8]) -> u64 {
    const POLY: u64 = 0x42F0_E1EB_A9EA_3693;
    for &byte in bytes {
        crc ^= u64::from(byte) << 56;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000_0000_0000 != 0 { (crc << 1) ^ POLY } else { crc << 1 };
        }
    }
    crc
}

/// A baked image's content checksum: CRC-64 over the directory then the arena (the actual baked
/// data). `write_baked` records it in the header; `from_baked` recomputes + compares at boot.
#[cfg(feature = "code-in-place")]
fn image_checksum(directory: &[u8], arena: &[u8]) -> u64 {
    crc64_update(crc64_update(0, directory), arena)
}

/// The content checksum recorded in a baked image's header (word 18), or `None` if the bytes are not
/// a current-version Lamella image. Reads ONLY the fixed header -- for a deploy-skip check (does a
/// device already hold this exact image?) without parsing or hashing the whole image. Not gated on
/// `code-in-place`: a deploy HOST that never boots an image still needs it to content-address one.
pub fn baked_image_checksum(image: &[u8]) -> Option<u64> {
    if image.get(..4) != Some(&b"LMLI"[..]) {
        return None;
    }
    let version = u32::from_le_bytes(image.get(4..8)?.try_into().ok()?);
    if version != BAKED_IMAGE_VERSION {
        return None;
    }
    let at = 8 + 18 * 8;
    Some(u64::from_le_bytes(image.get(at..at + 8)?.try_into().ok()?))
}

/// The baked-image format version. v3 repurposed the `newarr` element table's payload from a
/// byte width to a [`PrimKind`] code (packed primitive arrays) -- a v2 image's widths would
/// misdecode as kinds, so `from_baked` rejects it (rebake). v2 added the content checksum
/// (header word 18); v1 had no checksum and a shorter header.
const BAKED_IMAGE_VERSION: u32 = 3;

/// Packs a by-name index key: the member/type handle in the high 40 bits, the name id in the low
/// 24. `None` if either exceeds its field (the entry then stays in the builder map).
fn packed_name_key(handle: u64, name_id: u32) -> Option<u64> {
    if handle < (1 << 40) && name_id < (1 << 24) {
        Some((handle << 24) | u64::from(name_id))
    } else {
        None
    }
}

/// Encodes a scalar [`Value`] as a `(tag, bits)` record for the arena, `None` for a value shape
/// (struct, object) that has no flat scalar form -- it then stays in its builder map.
fn encode_scalar_value(value: &Value) -> Option<(u32, u64)> {
    match value {
        Value::Null => Some((0, 0)),
        Value::Int32(bits) => Some((1, *bits as u64)),
        Value::Int64(bits) => Some((2, *bits as u64)),
        Value::NativeInt(bits) => Some((3, *bits as u64)),
        #[cfg(feature = "float")]
        Value::Float(bits) => Some((4, bits.to_bits())),
        #[cfg(feature = "float")]
        Value::Single(bits) => Some((5, u64::from(bits.to_bits()))),
        _ => None,
    }
}

/// Appends `value` as a 12-byte tag+bits record stream (pre-order for a struct: a tag-6 node
/// carries its field count, the fields follow). The zero-value shapes the loader builds
/// (field/static defaults) are exactly scalars + nested struct shapes.
#[cfg(feature = "code-in-place")]
fn encode_value_tree(out: &mut TableArena, value: &Value) -> bool {
    if let Some((tag, bits)) = encode_scalar_value(value) {
        out.push_u32(tag);
        out.push_u64(bits);
        return true;
    }
    if let Value::Struct(fields) = value {
        out.push_u32(6);
        out.push_u64(fields.len() as u64);
        return fields.iter().all(|field| encode_value_tree(out, field));
    }
    false
}

/// Reads one [`encode_value_tree`] node at `at`, returning the value and the next offset.
/// Depth-guarded (a malformed image cannot recurse unboundedly).
#[cfg(feature = "code-in-place")]
fn decode_value_tree(arena: &TableArena, at: usize, depth: u32) -> Option<(Value, usize)> {
    if depth > 16 {
        return None;
    }
    let tag = arena.read_u32(at)?;
    let bits = arena.read_u64(at + 4)?;
    let mut next = at + 12;
    if tag == 6 {
        let mut fields = Vec::with_capacity(bits as usize);
        for _ in 0..bits {
            let (field, after) = decode_value_tree(arena, next, depth + 1)?;
            fields.push(field);
            next = after;
        }
        return Some((Value::Struct(fields.into_boxed_slice()), next));
    }
    Some((decode_scalar_value(tag, bits)?, next))
}

/// Decodes an [`encode_scalar_value`] record. An unknown tag (a float value in a no-float
/// build's image) is `None`.
fn decode_scalar_value(tag: u32, bits: u64) -> Option<Value> {
    match tag {
        0 => Some(Value::Null),
        1 => Some(Value::Int32(bits as i32)),
        2 => Some(Value::Int64(bits as i64)),
        3 => Some(Value::NativeInt(bits as i64)),
        #[cfg(feature = "float")]
        4 => Some(Value::Float(f64::from_bits(bits))),
        #[cfg(feature = "float")]
        5 => Some(Value::Single(f32::from_bits(bits as u32))),
        _ => None,
    }
}

impl FrozenTables {
    /// Serializes every view (offset, count) -- the image directory -- as fixed-order words.
    #[cfg(feature = "code-in-place")]
    fn write_directory(&self, out: &mut Vec<u8>) {
        let pair = |out: &mut Vec<u8>, offset: usize, entries: usize| {
            image_push(out, offset as u64);
            image_push(out, entries as u64);
        };
        image_push(out, self.token_columns.len() as u64);
        for column in &self.token_columns {
            image_push(out, u64::from(column.asm));
            image_push(out, u64::from(column.table));
            image_push(out, u64::from(column.rows));
            image_push(out, column.offset as u64);
        }
        image_push(out, self.call_target_columns.len() as u64);
        for column in &self.call_target_columns {
            image_push(out, u64::from(column.asm));
            image_push(out, u64::from(column.table));
            image_push(out, u64::from(column.rows));
            image_push(out, column.offset as u64);
        }
        pair(out, self.token_spill.offset, self.token_spill.entries);
        pair(out, self.method_type.offset, self.method_type.entries);
        pair(out, self.method_slot.offset, self.method_slot.entries);
        pair(out, self.type_tokens.offset, self.type_tokens.entries);
        pair(out, self.type_handles.offset, self.type_handles.entries);
        pair(out, self.field_slots.offset, self.field_slots.entries);
        pair(out, self.field_types.offset, self.field_types.entries);
        pair(out, self.static_fields.offset, self.static_fields.entries);
        pair(out, self.array_prim_kinds.offset, self.array_prim_kinds.entries);
        pair(out, self.enum_widths.offset, self.enum_widths.entries);
        pair(out, self.delegate_invokes.offset, self.delegate_invokes.entries);
        pair(out, self.md_array_ctors.offset, self.md_array_ctors.entries);
        pair(out, self.string_builder_ctors.offset, self.string_builder_ctors.entries);
        pair(out, self.list_ctors.offset, self.list_ctors.entries);
        pair(out, self.finalizers.offset, self.finalizers.entries);
        pair(out, self.type_sizes.offset, self.type_sizes.entries);
        pair(out, self.catch_type_tags.offset, self.catch_type_tags.entries);
        pair(out, self.explicit_overrides.offset, self.explicit_overrides.entries);
        pair(out, self.delegate_ctors.offset, self.delegate_ctors.entries);
        pair(out, self.enum_wide.offset, self.enum_wide.entries);
        pair(out, self.enum_flags.offset, self.enum_flags.entries);
        pair(out, self.object_type_tokens.offset, self.object_type_tokens.entries);
        pair(out, self.string_type_tokens.offset, self.string_type_tokens.entries);
        pair(out, self.value_type_ctors.offset, self.value_type_ctors.entries);
        pair(out, self.field_rva_data.offset, self.field_rva_data.entries);
        pair(out, self.sig_virtual_runs.offset, self.sig_virtual_runs.entries);
        pair(out, self.sig_nonvirtual_runs.offset, self.sig_nonvirtual_runs.entries);
        pair(out, self.call_targets.offset, self.call_targets.entries);
        pair(out, self.vtable_slot_runs.offset, self.vtable_slot_runs.entries);
        pair(out, self.vtable_runs.offset, self.vtable_runs.entries);
        pair(out, self.interface_runs.offset, self.interface_runs.entries);
        pair(out, self.array_defaults.offset, self.array_defaults.entries);
        pair(out, self.strings.offset, self.strings.entries);
        pair(out, self.name_pool.offsets, self.name_pool.entries);
        pair(out, self.type_names.offset, self.type_names.entries);
        pair(out, self.type_full_names.offset, self.type_full_names.entries);
        pair(out, self.fields_by_name.offset, self.fields_by_name.entries);
        pair(out, self.methods_by_name.offset, self.methods_by_name.entries);
        pair(out, self.properties_by_name.offset, self.properties_by_name.entries);
        pair(out, self.name_to_handle.offset, self.name_to_handle.entries);
        pair(out, self.type_ctors.offset, self.type_ctors.entries);
        pair(out, self.method_attrs.offset, self.method_attrs.entries);
        pair(out, self.member_type_handle.offset, self.member_type_handle.entries);
        pair(out, self.reflect_types.offset, self.reflect_types.entries);
        pair(out, self.type_fields.offset, self.type_fields.entries);
        pair(out, self.type_methods.offset, self.type_methods.entries);
        pair(out, self.method_params.offset, self.method_params.entries);
        pair(out, self.type_ctors_list.offset, self.type_ctors_list.entries);
        pair(out, self.box_primitives.offset, self.box_primitives.entries);
        pair(out, self.assembly_names.offset, self.assembly_names.entries);
        pair(out, self.assembly_types.offset, self.assembly_types.entries);
        pair(out, self.enum_constants.offset, self.enum_constants.entries);
    }

    /// Reads a [`FrozenTables::write_directory`] image back; returns the views and the word
    /// count consumed.
    #[cfg(feature = "code-in-place")]
    fn read_directory(bytes: &[u8]) -> Option<(FrozenTables, usize)> {
        let mut frozen = FrozenTables::default();
        let mut cursor = 0usize;
        let mut take = |bytes: &[u8]| -> Option<u64> {
            let value = image_word(bytes, cursor)?;
            cursor += 1;
            Some(value)
        };
        let columns = take(bytes)? as usize;
        for _ in 0..columns {
            frozen.token_columns.push(DenseTokenColumn {
                asm: take(bytes)? as u8,
                table: take(bytes)? as u32,
                rows: take(bytes)? as u32,
                offset: take(bytes)? as usize,
            });
        }
        let wide_columns = take(bytes)? as usize;
        for _ in 0..wide_columns {
            frozen.call_target_columns.push(DenseWideColumn {
                asm: take(bytes)? as u8,
                table: take(bytes)? as u32,
                rows: take(bytes)? as u32,
                offset: take(bytes)? as usize,
            });
        }
        let mut pair = |bytes: &[u8]| -> Option<(usize, usize)> {
            Some((take(bytes)? as usize, take(bytes)? as usize))
        };
        macro_rules! sorted {
            ($field:ident, $kind:ident) => {
                let (offset, entries) = pair(bytes)?;
                frozen.$field = $kind { offset, entries };
            };
        }
        sorted!(token_spill, SortedTokenTable);
        sorted!(method_type, DenseIdColumn);
        sorted!(method_slot, DenseIdColumn);
        sorted!(type_tokens, SortedTokenTable);
        sorted!(type_handles, DenseU64Column);
        sorted!(field_slots, SortedTokenTable);
        sorted!(field_types, SortedTokenTable);
        sorted!(static_fields, SortedTokenTable);
        sorted!(array_prim_kinds, SortedTokenTable);
        sorted!(enum_widths, SortedTokenTable);
        sorted!(delegate_invokes, SortedTokenTable);
        sorted!(md_array_ctors, SortedTokenTable);
        sorted!(string_builder_ctors, SortedTokenTable);
        sorted!(list_ctors, SortedTokenTable);
        sorted!(finalizers, SortedTokenTable);
        sorted!(type_sizes, SortedTokenTable);
        sorted!(catch_type_tags, SortedTokenTable);
        sorted!(explicit_overrides, SortedTokenTable);
        sorted!(delegate_ctors, SortedTokenSet);
        sorted!(enum_wide, SortedTokenSet);
        sorted!(enum_flags, SortedTokenSet);
        sorted!(object_type_tokens, SortedTokenSet);
        sorted!(string_type_tokens, SortedTokenSet);
        sorted!(value_type_ctors, SortedTokenSet);
        sorted!(field_rva_data, SortedBlobTable);
        sorted!(sig_virtual_runs, DenseU64Column);
        sorted!(sig_nonvirtual_runs, DenseU64Column);
        sorted!(call_targets, SortedWideTable);
        sorted!(vtable_slot_runs, DenseU64Column);
        sorted!(vtable_runs, DenseU64Column);
        sorted!(interface_runs, DenseU64Column);
        sorted!(array_defaults, SortedValueTable);
        sorted!(strings, SortedBlobTable);
        {
            let (offsets, entries) = pair(bytes)?;
            frozen.name_pool = NamePool { offsets, entries };
        }
        sorted!(type_names, SortedTokenTable);
        sorted!(type_full_names, DenseIdColumn);
        sorted!(fields_by_name, SortedWideTable);
        sorted!(methods_by_name, SortedWideTable);
        sorted!(properties_by_name, SortedWideTable);
        sorted!(name_to_handle, SortedWideTable);
        sorted!(type_ctors, SortedTokenTable);
        sorted!(method_attrs, SortedTokenTable);
        sorted!(member_type_handle, SortedWideTable);
        sorted!(reflect_types, SortedReflectTable);
        sorted!(type_fields, SortedWideTable);
        sorted!(type_methods, SortedWideTable);
        sorted!(method_params, SortedWideTable);
        sorted!(type_ctors_list, SortedWideTable);
        sorted!(box_primitives, SortedTokenTable);
        sorted!(assembly_names, SortedTokenTable);
        sorted!(assembly_types, SortedWideTable);
        sorted!(enum_constants, SortedWideTable);
        Some((frozen, cursor))
    }
}

/// A sorted `(u64 key -> (u32, u32))` table in the arena: 16-byte records binary-searched by
/// key -- the frozen form of a map with a two-part value (`call_targets`' sig id + arg count).
#[derive(Clone, Copy, Default)]
struct SortedWideTable {
    offset: usize,
    entries: usize,
}

impl SortedWideTable {
    const RECORD: usize = 16;

    fn get(&self, arena: &TableArena, key: u64) -> Option<(u32, u32)> {
        let (mut low, mut high) = (0usize, self.entries);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = self.offset + middle * SortedWideTable::RECORD;
            let probe = arena.read_u64(at)?;
            match probe.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => {
                    return Some((arena.read_u32(at + 8)?, arena.read_u32(at + 12)?));
                }
            }
        }
        None
    }

    fn entries_of(&self, arena: &TableArena) -> Vec<(u64, u32, u32)> {
        (0..self.entries)
            .filter_map(|entry| {
                let at = self.offset + entry * SortedWideTable::RECORD;
                Some((
                    arena.read_u64(at)?,
                    arena.read_u32(at + 8)?,
                    arena.read_u32(at + 12)?,
                ))
            })
            .collect()
    }

    fn write(arena: &mut TableArena, mut records: Vec<(u64, u32, u32)>) -> SortedWideTable {
        records.sort_unstable_by_key(|(key, _, _)| *key);
        let offset = arena.bytes.len();
        for (key, first, second) in &records {
            arena.push_u64(*key);
            arena.push_u32(*first);
            arena.push_u32(*second);
        }
        SortedWideTable {
            offset,
            entries: records.len(),
        }
    }
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
fn format_flag_names(constants: &[(i64, String)], value: i64) -> String {
    if value == 0 {
        return match constants
            .iter()
            .find(|(member, _)| *member == 0)
            .map(|(_, name)| name)
        {
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

    /// Adds a managed method belonging to assembly `asm` (storing its RAW CIL bytes for lazy decoding)
    /// and returns its [`MethodId`]. The bytes are owned ([`RawCil`] = `Box<[u8]>`) by default, or a
    /// `&'static` borrow of the flash PE under `flash-image`.
    pub fn add_method(&mut self, asm: u8, raw_il: RawCil, arg_count: u16) -> MethodId {
        self.push(
            asm,
            Method::Managed {
                body: ManagedBody::from_raw(raw_il),
                arg_count,
            },
        )
    }

    /// Adds a managed method from an already-DECODED body, pre-populating the lazy cache -- a
    /// convenience for callers (and tests) that build a [`MethodBodyImage`] directly rather than
    /// holding the method's raw bytes. The body runs as-is, with no encode/decode round-trip.
    pub fn add_method_image(&mut self, asm: u8, body: MethodBodyImage, arg_count: u16) -> MethodId {
        self.push(
            asm,
            Method::Managed {
                body: ManagedBody::from_image(body),
                arg_count,
            },
        )
    }

    /// Adds a native intrinsic belonging to assembly `asm` and returns its [`MethodId`]. `id` is the
    /// intrinsic's stable registry id (`intrinsic_registry::intrinsic_id(fn_name)`), stamped by the
    /// binder so the bake never compares fn pointers.
    pub fn add_intrinsic(&mut self, asm: u8, func: IntrinsicFn, id: u32, arg_count: u16) -> MethodId {
        self.push(asm, Method::Intrinsic { func, id, arg_count })
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
        #[cfg(feature = "code-in-place")]
        if let Some(baked) = &self.baked {
            return baked
                .arena_static
                .get(baked.method_asm + id as usize)
                .copied()
                .unwrap_or(0);
        }
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
        let mut bytes = Vec::with_capacity(chars.len() * 2);
        for unit in chars {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        self.strings.insert(asm_key(asm, token.0), bytes.into_boxed_slice());
    }

    /// The method a `call` token in assembly `asm` resolves to, if any.
    #[must_use]
    pub fn resolve(&self, asm: u8, token: Token) -> Option<MethodId> {
        self.resolve_by_handle(asm_key(asm, token.0))
    }

    /// The method whose asm-folded handle is `handle` (the `MethodInfo` handle reflection carries
    /// -- a `MethodDef` token folded with its assembly), if bound.
    #[must_use]
    pub fn resolve_by_handle(&self, handle: u64) -> Option<MethodId> {
        self.frozen_token(handle)
            .or_else(|| self.by_token.get(&handle).copied())
    }

    /// Looks `handle` up in the frozen token tables: the dense per-assembly `MethodDef` column
    /// when the token kind and row fit one (a direct index), else the sorted spill. `None` falls
    /// back to the mutable map (pre-freeze loads and post-freeze REPL deltas).
    fn frozen_token(&self, handle: u64) -> Option<MethodId> {
        let token = handle as u32;
        let table = token >> 24;
        let asm = (handle >> 32) as u8;
        let row = token & Token::MAX_ROW;
        if row != 0 {
            for column in &self.frozen.token_columns {
                if column.asm == asm && column.table == table && row <= column.rows {
                    let bound = self
                        .arena
                        .read_u32(column.offset + (row as usize - 1) * 4)
                        .unwrap_or(0);
                    if bound != 0 {
                        return Some(bound - 1);
                    }
                    break;
                }
            }
        }
        self.frozen.token_spill.get(&self.arena, handle)
    }

    /// Freezes the binding tables built so far into the flat arena (the runtime read form and,
    /// under the bake, the flash image), draining the mutable builders. Each load entry point
    /// calls this once after its assemblies are bound; a later incremental delta binds into the
    /// (now empty) builders, which lookups consult after the frozen tables.
    pub fn freeze(&mut self) {
        self.freeze_by_token();
        let entries = self.methods.len();
        self.frozen.method_type = Module::freeze_dense_id_column(
            &mut self.arena,
            self.frozen.method_type,
            &mut self.method_type,
            entries,
        );
        self.frozen.method_slot = Module::freeze_dense_id_column(
            &mut self.arena,
            self.frozen.method_slot,
            &mut self.method_slots,
            entries,
        );
        self.freeze_token_tables();
        self.freeze_token_sets();
        self.freeze_type_handles();
        self.freeze_rva_blobs();
        self.freeze_sig_tables();
        self.freeze_type_shape();
        self.freeze_strings();
        self.freeze_names();
        self.freeze_reflect_records();
    }

    /// Drains every name-keyed reflection builder through one sorted UTF-8 pool. Name ids are
    /// pool ranks, so the pool and all six name tables rebuild TOGETHER whenever any builder has
    /// new entries (old frozen entries thaw back to strings through the old pool first).
    fn freeze_names(&mut self) {
        if self.type_names.is_empty()
            && self.type_full_names.is_empty()
            && self.type_fields_by_name.is_empty()
            && self.type_methods_by_name.is_empty()
            && self.type_properties_by_name.is_empty()
            && self.name_to_handle.is_empty()
            && self.reflect_types.is_empty()
            && self.method_params.is_empty()
            && self.assembly_names.is_empty()
            && self.enum_constants.is_empty()
        {
            return;
        }
        let old_pool = self.frozen.name_pool;
        let thaw = |arena: &TableArena, id: u32| -> String {
            old_pool
                .str_at(arena, id)
                .map(String::from)
                .unwrap_or_default()
        };

        let mut type_names: Vec<(u64, String)> = self
            .frozen
            .type_names
            .entries_of(&self.arena)
            .into_iter()
            .map(|(handle, id)| (handle, thaw(&self.arena, id)))
            .collect();
        type_names.extend(core::mem::take(&mut self.type_names));

        let mut full_names: Vec<(TypeId, String)> = (0..self.frozen.type_full_names.entries)
            .filter_map(|index| {
                let id = self.frozen.type_full_names.get(&self.arena, index as u32)?;
                Some((index as TypeId, thaw(&self.arena, id)))
            })
            .collect();
        full_names.extend(core::mem::take(&mut self.type_full_names));

        let thaw_by_name = |arena: &TableArena, table: SortedWideTable| -> Vec<((u64, String), u64)> {
            table
                .entries_of(arena)
                .into_iter()
                .map(|(key, high, low)| {
                    let handle = key >> 24;
                    let name = thaw(arena, (key & 0x00FF_FFFF) as u32);
                    ((handle, name), (u64::from(high) << 32) | u64::from(low))
                })
                .collect()
        };
        let mut fields = thaw_by_name(&self.arena, self.frozen.fields_by_name);
        fields.extend(core::mem::take(&mut self.type_fields_by_name));
        let mut methods = thaw_by_name(&self.arena, self.frozen.methods_by_name);
        methods.extend(core::mem::take(&mut self.type_methods_by_name));
        let mut properties = thaw_by_name(&self.arena, self.frozen.properties_by_name);
        properties.extend(core::mem::take(&mut self.type_properties_by_name));

        let mut reflect_rows: Vec<(u64, ReflectType)> = self
            .frozen
            .reflect_types
            .entries_of(&self.arena)
            .into_iter()
            .map(|(key, namespace, full, flags, base)| {
                (
                    key,
                    ReflectType {
                        namespace: thaw(&self.arena, namespace),
                        full_name: thaw(&self.arena, full),
                        is_enum: flags & REFLECT_ENUM != 0,
                        is_value_type: flags & REFLECT_VALUE_TYPE != 0,
                        is_interface: flags & REFLECT_INTERFACE != 0,
                        is_abstract: flags & REFLECT_ABSTRACT != 0,
                        is_public: flags & REFLECT_PUBLIC != 0,
                        base_handle: base,
                    },
                )
            })
            .collect();
        reflect_rows.extend(core::mem::take(&mut self.reflect_types));

        let thaw_params = |arena: &TableArena, pool: NamePool, table: SortedWideTable| {
            table
                .entries_of(arena)
                .into_iter()
                .map(|(key, offset, count)| {
                    let params: Vec<MethodParam> = (0..count as usize)
                        .filter_map(|index| {
                            let at = offset as usize + index * 16;
                            let type_handle = (u64::from(arena.read_u32(at)?) << 32)
                                | u64::from(arena.read_u32(at + 4)?);
                            let name = pool
                                .str_at(arena, arena.read_u32(at + 8)?)
                                .map(String::from)
                                .unwrap_or_default();
                            Some(MethodParam { type_handle, name })
                        })
                        .collect();
                    (key, params)
                })
                .collect::<Vec<_>>()
        };
        let mut param_rows = thaw_params(&self.arena, old_pool, self.frozen.method_params);
        param_rows.extend(core::mem::take(&mut self.method_params));

        let mut enum_rows: Vec<(u64, Vec<(i64, String)>)> = self
            .frozen
            .enum_constants
            .entries_of(&self.arena)
            .into_iter()
            .map(|(key, offset, count)| {
                let members: Vec<(i64, String)> = (0..count as usize)
                    .filter_map(|index| {
                        let at = offset as usize + index * 16;
                        Some((
                            self.arena.read_u64(at)? as i64,
                            thaw(&self.arena, self.arena.read_u32(at + 8)?),
                        ))
                    })
                    .collect();
                (key, members)
            })
            .collect();
        enum_rows.extend(
            core::mem::take(&mut self.enum_constants)
                .into_iter()
                .map(|(key, members)| (key, members.into_iter().collect::<Vec<_>>())),
        );

        let mut assembly_name_rows: Vec<(u8, String)> = self
            .frozen
            .assembly_names
            .entries_of(&self.arena)
            .into_iter()
            .map(|(asm, id)| (asm as u8, thaw(&self.arena, id)))
            .collect();
        assembly_name_rows.extend(core::mem::take(&mut self.assembly_names));

        let mut handles_by_name: Vec<(String, u64)> = self
            .frozen
            .name_to_handle
            .entries_of(&self.arena)
            .into_iter()
            .map(|(id, high, low)| {
                (thaw(&self.arena, id as u32), (u64::from(high) << 32) | u64::from(low))
            })
            .collect();
        handles_by_name.extend(core::mem::take(&mut self.name_to_handle));

        let mut names: Vec<&String> = type_names
            .iter()
            .map(|(_, name)| name)
            .chain(full_names.iter().map(|(_, name)| name))
            .chain(fields.iter().map(|((_, name), _)| name))
            .chain(methods.iter().map(|((_, name), _)| name))
            .chain(properties.iter().map(|((_, name), _)| name))
            .chain(handles_by_name.iter().map(|(name, _)| name))
            .chain(
                reflect_rows
                    .iter()
                    .flat_map(|(_, info)| [&info.namespace, &info.full_name]),
            )
            .chain(
                param_rows
                    .iter()
                    .flat_map(|(_, params)| params.iter().map(|param| &param.name)),
            )
            .chain(assembly_name_rows.iter().map(|(_, name)| name))
            .chain(
                enum_rows
                    .iter()
                    .flat_map(|(_, members)| members.iter().map(|(_, name)| name)),
            )
            .collect();
        names.sort_unstable();
        names.dedup();
        let pool_names: Vec<String> = names.into_iter().cloned().collect();
        let pool = NamePool::write(&mut self.arena, &pool_names);
        let id_of = |name: &str| -> u32 {
            pool_names
                .binary_search_by(|probe| probe.as_str().cmp(name))
                .map(|rank| rank as u32)
                .unwrap_or(u32::MAX)
        };

        self.frozen.type_names = SortedTokenTable::write(
            &mut self.arena,
            type_names
                .into_iter()
                .map(|(handle, name)| (handle, id_of(&name)))
                .collect(),
        );
        let full_name_ids: Vec<(u32, u32)> =
            full_names.into_iter().map(|(id, name)| (id, id_of(&name))).collect();
        let entries = self.types.len();
        {
            let mut values = alloc::vec![0u32; entries];
            for (type_id, name_id) in &full_name_ids {
                if let Some(slot) = values.get_mut(*type_id as usize) {
                    *slot = name_id + 1;
                }
            }
            let offset = self.arena.bytes.len();
            for value in &values {
                self.arena.push_u32(*value);
            }
            self.frozen.type_full_names = DenseIdColumn { offset, entries };
        }
        let mut write_by_name = |rows: Vec<((u64, String), u64)>| -> (SortedWideTable, Vec<((u64, String), u64)>) {
            let mut records = Vec::with_capacity(rows.len());
            let mut kept = Vec::new();
            for ((handle, name), value) in rows {
                match packed_name_key(handle, id_of(&name)) {
                    Some(key) => records.push((key, (value >> 32) as u32, value as u32)),
                    None => kept.push(((handle, name), value)),
                }
            }
            (SortedWideTable::write(&mut self.arena, records), kept)
        };
        let (table, kept) = write_by_name(fields);
        self.frozen.fields_by_name = table;
        self.type_fields_by_name.extend(kept);
        let (table, kept) = write_by_name(methods);
        self.frozen.methods_by_name = table;
        self.type_methods_by_name.extend(kept);
        let (table, kept) = write_by_name(properties);
        self.frozen.properties_by_name = table;
        self.type_properties_by_name.extend(kept);
        self.frozen.name_to_handle = SortedWideTable::write(
            &mut self.arena,
            handles_by_name
                .into_iter()
                .map(|(name, handle)| {
                    (u64::from(id_of(&name)), (handle >> 32) as u32, handle as u32)
                })
                .collect(),
        );
        self.frozen.reflect_types = SortedReflectTable::write(
            &mut self.arena,
            reflect_rows
                .into_iter()
                .map(|(key, info)| {
                    let mut flags = 0;
                    if info.is_enum {
                        flags |= REFLECT_ENUM;
                    }
                    if info.is_value_type {
                        flags |= REFLECT_VALUE_TYPE;
                    }
                    if info.is_interface {
                        flags |= REFLECT_INTERFACE;
                    }
                    if info.is_abstract {
                        flags |= REFLECT_ABSTRACT;
                    }
                    if info.is_public {
                        flags |= REFLECT_PUBLIC;
                    }
                    (
                        key,
                        id_of(&info.namespace),
                        id_of(&info.full_name),
                        flags,
                        info.base_handle,
                    )
                })
                .collect(),
        );

        let mut param_refs: Vec<(u64, u32, u32)> = Vec::with_capacity(param_rows.len());
        for (key, params) in param_rows {
            let offset = self.arena.bytes.len() as u32;
            for param in &params {
                self.arena.push_u32((param.type_handle >> 32) as u32);
                self.arena.push_u32(param.type_handle as u32);
                self.arena.push_u32(id_of(&param.name));
                self.arena.push_u32(0);
            }
            param_refs.push((key, offset, params.len() as u32));
        }
        self.frozen.method_params = SortedWideTable::write(&mut self.arena, param_refs);

        let mut enum_refs: Vec<(u64, u32, u32)> = Vec::with_capacity(enum_rows.len());
        for (key, members) in enum_rows {
            let offset = self.arena.bytes.len() as u32;
            for (value, name) in &members {
                self.arena.push_u64(*value as u64);
                self.arena.push_u32(id_of(name));
                self.arena.push_u32(0);
            }
            enum_refs.push((key, offset, members.len() as u32));
        }
        self.frozen.enum_constants = SortedWideTable::write(&mut self.arena, enum_refs);

        self.frozen.assembly_names = SortedTokenTable::write(
            &mut self.arena,
            assembly_name_rows
                .into_iter()
                .map(|(asm, name)| (u64::from(asm), id_of(&name)))
                .collect(),
        );

        self.frozen.name_pool = pool;
    }

    /// Drains the string-free reflection record builders: the field/method lists, the ctor
    /// overload lists, the assembly type lists, and the boxed-primitive display kinds.
    fn freeze_reflect_records(&mut self) {
        fn freeze_flagged(
            arena: &mut TableArena,
            table: SortedWideTable,
            rows: Vec<(u64, Vec<(u64, u32)>)>,
        ) -> SortedWideTable {
            if rows.is_empty() {
                return table;
            }
            let mut refs = table.entries_of(arena);
            for (key, records) in rows {
                let offset = arena.bytes.len() as u32;
                let count = records.len() as u32;
                for (handle, flags) in records {
                    arena.push_u64(handle);
                    arena.push_u32(flags);
                    arena.push_u32(0);
                }
                refs.push((key, offset, count));
            }
            SortedWideTable::write(arena, refs)
        }
        let field_rows: Vec<(u64, Vec<(u64, u32)>)> = core::mem::take(&mut self.type_fields)
            .into_iter()
            .map(|(key, fields)| {
                (
                    key,
                    fields
                        .into_iter()
                        .map(|field| {
                            (
                                field.handle,
                                u32::from(field.is_static) | (u32::from(field.is_public) << 1),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        self.frozen.type_fields = freeze_flagged(&mut self.arena, self.frozen.type_fields, field_rows);
        let method_rows: Vec<(u64, Vec<(u64, u32)>)> = core::mem::take(&mut self.type_methods)
            .into_iter()
            .map(|(key, methods)| {
                (
                    key,
                    methods
                        .into_iter()
                        .map(|method| {
                            (
                                method.handle,
                                u32::from(method.is_static) | (u32::from(method.is_public) << 1),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        self.frozen.type_methods =
            freeze_flagged(&mut self.arena, self.frozen.type_methods, method_rows);
        let ctor_rows: Vec<(u64, Vec<(u64, u32)>)> = core::mem::take(&mut self.type_ctors_list)
            .into_iter()
            .map(|(key, ctors)| {
                (
                    key,
                    ctors
                        .into_iter()
                        .map(|(handle, argc)| (handle, argc as u32))
                        .collect(),
                )
            })
            .collect();
        self.frozen.type_ctors_list =
            freeze_flagged(&mut self.arena, self.frozen.type_ctors_list, ctor_rows);

        if !self.box_primitives.is_empty() {
            let mut entries = self.frozen.box_primitives.entries_of(&self.arena);
            entries.extend(core::mem::take(&mut self.box_primitives).into_iter().map(
                |(key, kind)| {
                    (
                        key,
                        match kind {
                            BoxedPrimitive::Boolean => 0,
                            BoxedPrimitive::Char => 1,
                        },
                    )
                },
            ));
            self.frozen.box_primitives = SortedTokenTable::write(&mut self.arena, entries);
        }
        if !self.assembly_types.is_empty() {
            let mut refs = self.frozen.assembly_types.entries_of(&self.arena);
            for (asm, handles) in core::mem::take(&mut self.assembly_types) {
                let offset = self.arena.bytes.len() as u32;
                for handle in &handles {
                    self.arena.push_u64(*handle);
                }
                refs.push((u64::from(asm), offset, handles.len() as u32));
            }
            self.frozen.assembly_types = SortedWideTable::write(&mut self.arena, refs);
        }
    }

    /// Drains the `ldstr` strings into the arena pool (payload bytes, then blob records).
    fn freeze_strings(&mut self) {
        if self.strings.is_empty() {
            return;
        }
        let mut records = self.frozen.strings.entries_of(&self.arena);
        for (key, text) in core::mem::take(&mut self.strings) {
            let payload = self.arena.bytes.len() as u32;
            self.arena.bytes.to_mut().extend_from_slice(&text);
            records.push((key, payload, text.len() as u32));
        }
        self.frozen.strings = SortedBlobTable::write(&mut self.arena, records);
    }

    /// Drains each type's vtable and interface list into packed `u32` runs, and the scalar
    /// `newarr` defaults into their value table. `field_defaults` stays a resident builder: an
    /// incremental REPL delta GROWS it (`add_type_field`), so it is mutable by design and tiny.
    fn freeze_type_shape(&mut self) {
        let entries = self.types.len();
        let mut vtables: Vec<(TypeId, Vec<u32>)> = Vec::new();
        let mut interfaces: Vec<(TypeId, Vec<u32>)> = Vec::new();
        for (type_id, info) in self.types.iter_mut().enumerate() {
            if !info.vtable.is_empty() {
                vtables.push((type_id as TypeId, core::mem::take(&mut info.vtable).into_vec()));
            }
            if !info.interfaces.is_empty() {
                interfaces.push((type_id as TypeId, core::mem::take(&mut info.interfaces)));
            }
        }
        self.frozen.vtable_runs =
            Module::freeze_u32_runs(&mut self.arena, self.frozen.vtable_runs, vtables, entries);
        self.frozen.interface_runs = Module::freeze_u32_runs(
            &mut self.arena,
            self.frozen.interface_runs,
            interfaces,
            entries,
        );
        if !self.array_defaults.is_empty() {
            let mut records = self.frozen.array_defaults.entries_of(&self.arena);
            let drained = core::mem::take(&mut self.array_defaults);
            for (key, value) in drained {
                match encode_scalar_value(&value) {
                    Some((tag, bits)) => records.push((key, bits, tag)),
                    None => {
                        self.array_defaults.insert(key, value);
                    }
                }
            }
            self.frozen.array_defaults = SortedValueTable::write(&mut self.arena, records);
        }
    }

    /// Writes per-type `u32` record runs (vtable slots, interface lists) into the arena and
    /// returns the dense run column, carrying an earlier column's runs forward.
    fn freeze_u32_runs(
        arena: &mut TableArena,
        old: DenseU64Column,
        runs: Vec<(TypeId, Vec<u32>)>,
        entries: usize,
    ) -> DenseU64Column {
        if runs.is_empty() && old.entries >= entries {
            return old;
        }
        let mut packed = alloc::vec![0u64; entries];
        for index in 0..old.entries.min(entries) {
            packed[index] = arena.read_u64(old.offset + index * 8).unwrap_or(0);
        }
        for (type_id, records) in runs {
            let run_offset = arena.bytes.len() as u64;
            for record in &records {
                arena.push_u32(*record);
            }
            if let Some(slot) = packed.get_mut(type_id as usize) {
                *slot = ((run_offset + 1) << 32) | records.len() as u64;
            }
        }
        let offset = arena.bytes.len();
        for value in &packed {
            arena.push_u64(*value);
        }
        DenseU64Column { offset, entries }
    }

    /// Drains the signature-dispatch builders: each type's id-keyed sig maps into the two global
    /// `(TypeId, SigId) -> method` tables, `call_targets` into its wide table, and the vtable
    /// slot-key vectors into slot-run records addressed by a dense per-type column.
    fn freeze_sig_tables(&mut self) {
        let entries = self.types.len();
        let mut virtual_runs: Vec<(TypeId, Vec<(u32, u32)>)> = Vec::new();
        let mut nonvirtual_runs: Vec<(TypeId, Vec<(u32, u32)>)> = Vec::new();
        for (type_id, info) in self.types.iter_mut().enumerate() {
            if !info.sig_methods.is_empty() {
                virtual_runs.push((
                    type_id as TypeId,
                    core::mem::take(&mut info.sig_methods).into_iter().collect(),
                ));
            }
            if !info.sig_methods_nonvirtual.is_empty() {
                nonvirtual_runs.push((
                    type_id as TypeId,
                    core::mem::take(&mut info.sig_methods_nonvirtual).into_iter().collect(),
                ));
            }
        }
        self.frozen.sig_virtual_runs =
            Module::freeze_runs(&mut self.arena, self.frozen.sig_virtual_runs, virtual_runs, entries);
        self.frozen.sig_nonvirtual_runs = Module::freeze_runs(
            &mut self.arena,
            self.frozen.sig_nonvirtual_runs,
            nonvirtual_runs,
            entries,
        );
        if !self.call_targets.is_empty() {
            let map = core::mem::take(&mut self.call_targets);
            let mut row_extent: BTreeMap<(u8, u32), (u32, u32)> = BTreeMap::new();
            for handle in map.keys() {
                let token = *handle as u32;
                let row = token & Token::MAX_ROW;
                if row != 0 {
                    let group = ((*handle >> 32) as u8, token >> 24);
                    let (max_row, bound_count) = row_extent.entry(group).or_insert((0, 0));
                    *max_row = (*max_row).max(row);
                    *bound_count += 1;
                }
            }
            for ((asm, table), (max_row, bound_count)) in &row_extent {
                if *max_row as usize <= (*bound_count as usize) * 4 + 64
                    && !self
                        .frozen
                        .call_target_columns
                        .iter()
                        .any(|column| column.asm == *asm && column.table == *table)
                {
                    let offset = self.arena.bytes.len();
                    self.arena.bytes.to_mut().resize(offset + *max_row as usize * 8, 0);
                    self.frozen.call_target_columns.push(DenseWideColumn {
                        asm: *asm,
                        table: *table,
                        rows: *max_row,
                        offset,
                    });
                }
            }
            let mut spill = self.frozen.call_targets.entries_of(&self.arena);
            for (handle, (sig, count)) in map {
                let token = handle as u32;
                let asm = (handle >> 32) as u8;
                let table = token >> 24;
                let row = token & Token::MAX_ROW;
                if let Some(column) = self.frozen.call_target_columns.iter().find(|column| {
                    column.asm == asm && column.table == table && row >= 1 && row <= column.rows
                }) {
                    let at = column.offset + (row as usize - 1) * 8;
                    if let Some(slot) = at
                        .checked_add(8)
                        .and_then(|end| self.arena.bytes.to_mut().get_mut(at..end))
                    {
                        slot.copy_from_slice(&(((sig as u64) << 16) | count as u64).to_le_bytes());
                        continue;
                    }
                }
                spill.push((handle, sig, count as u32));
            }
            self.frozen.call_targets = SortedWideTable::write(&mut self.arena, spill);
        }
        let entries = self.types.len();
        if !(self.vtable_slot_keys.is_empty() && self.frozen.vtable_slot_runs.entries >= entries) {
            let mut packed = alloc::vec![0u64; entries];
            for index in 0..self.frozen.vtable_slot_runs.entries.min(entries) {
                packed[index] = self
                    .arena
                    .read_u64(self.frozen.vtable_slot_runs.offset + index * 8)
                    .unwrap_or(0);
            }
            for (type_id, slots) in core::mem::take(&mut self.vtable_slot_keys) {
                if type_id as usize >= entries {
                    self.vtable_slot_keys.insert(type_id, slots);
                    continue;
                }
                let run_offset = self.arena.bytes.len() as u64;
                for (sig, method) in &slots {
                    self.arena.push_u32(*sig);
                    self.arena.push_u32(*method);
                }
                if let Some(slot) = packed.get_mut(type_id as usize) {
                    *slot = ((run_offset + 1) << 32) | slots.len() as u64;
                }
            }
            let offset = self.arena.bytes.len();
            for value in &packed {
                self.arena.push_u64(*value);
            }
            self.frozen.vtable_slot_runs = DenseU64Column { offset, entries };
        }
    }

    /// Drains every integer-valued token map into its sorted arena table (narrow values widen to
    /// the record's `u32`; the accessors narrow them back).
    fn freeze_token_tables(&mut self) {
        fn drain<V: Copy>(
            arena: &mut TableArena,
            table: SortedTokenTable,
            map: &mut BTreeMap<u64, V>,
            widen: fn(V) -> u32,
        ) -> SortedTokenTable {
            if map.is_empty() {
                return table;
            }
            let mut entries = table.entries_of(arena);
            entries.extend(core::mem::take(map).into_iter().map(|(key, value)| (key, widen(value))));
            SortedTokenTable::write(arena, entries)
        }
        let arena = &mut self.arena;
        let frozen = &mut self.frozen;
        frozen.type_tokens = drain(arena, frozen.type_tokens, &mut self.type_tokens, |id| id);
        frozen.field_slots = drain(arena, frozen.field_slots, &mut self.field_slots, |slot| slot);
        frozen.field_types = drain(arena, frozen.field_types, &mut self.field_types, |id| id);
        frozen.static_fields = drain(arena, frozen.static_fields, &mut self.static_fields, |slot| {
            slot as u32
        });
        frozen.array_prim_kinds = drain(
            arena,
            frozen.array_prim_kinds,
            &mut self.array_prim_kinds,
            |kind| u32::from(kind.code()),
        );
        frozen.enum_widths = drain(arena, frozen.enum_widths, &mut self.enum_widths, u32::from);
        frozen.delegate_invokes = drain(
            arena,
            frozen.delegate_invokes,
            &mut self.delegate_invokes,
            u32::from,
        );
        frozen.md_array_ctors = drain(arena, frozen.md_array_ctors, &mut self.md_array_ctors, u32::from);
        frozen.string_builder_ctors = drain(
            arena,
            frozen.string_builder_ctors,
            &mut self.string_builder_ctors,
            u32::from,
        );
        frozen.list_ctors = drain(arena, frozen.list_ctors, &mut self.list_ctors, u32::from);
        frozen.type_sizes = drain(arena, frozen.type_sizes, &mut self.type_sizes, |size| size);
        frozen.type_ctors = drain(arena, frozen.type_ctors, &mut self.type_ctors, |ctor| ctor);
        frozen.method_attrs = drain(arena, frozen.method_attrs, &mut self.method_attrs, |attrs| {
            attrs
        });
        if !self.member_type_handle.is_empty() {
            let mut records = frozen.member_type_handle.entries_of(arena);
            records.extend(
                core::mem::take(&mut self.member_type_handle)
                    .into_iter()
                    .map(|(key, handle)| (key, (handle >> 32) as u32, handle as u32)),
            );
            frozen.member_type_handle = SortedWideTable::write(arena, records);
        }
        frozen.catch_type_tags = drain(arena, frozen.catch_type_tags, &mut self.catch_type_tags, |tag| {
            tag
        });
        if !self.finalizers.is_empty() {
            let mut entries = frozen.finalizers.entries_of(arena);
            entries.extend(
                core::mem::take(&mut self.finalizers)
                    .into_iter()
                    .map(|(type_id, method)| (type_id as u64, method)),
            );
            frozen.finalizers = SortedTokenTable::write(arena, entries);
        }
        if !self.explicit_overrides.is_empty() {
            let mut entries = frozen.explicit_overrides.entries_of(arena);
            let unpacked = core::mem::take(&mut self.explicit_overrides);
            for ((type_id, handle), method) in unpacked {
                match packed_override_key(type_id, handle) {
                    Some(key) => entries.push((key, method)),
                    None => {
                        self.explicit_overrides.insert((type_id, handle), method);
                    }
                }
            }
            frozen.explicit_overrides = SortedTokenTable::write(arena, entries);
        }
    }

    /// Drains every token classification set into its sorted arena presence table.
    fn freeze_token_sets(&mut self) {
        fn drain(
            arena: &mut TableArena,
            set: SortedTokenSet,
            builder: &mut BTreeSet<u64>,
        ) -> SortedTokenSet {
            if builder.is_empty() {
                return set;
            }
            let mut keys = set.entries_of(arena);
            keys.extend(core::mem::take(builder));
            SortedTokenSet::write(arena, keys)
        }
        let arena = &mut self.arena;
        let frozen = &mut self.frozen;
        frozen.delegate_ctors = drain(arena, frozen.delegate_ctors, &mut self.delegate_ctors);
        frozen.enum_wide = drain(arena, frozen.enum_wide, &mut self.enum_wide);
        frozen.enum_flags = drain(arena, frozen.enum_flags, &mut self.enum_flags);
        frozen.object_type_tokens = drain(arena, frozen.object_type_tokens, &mut self.object_type_tokens);
        frozen.string_type_tokens = drain(arena, frozen.string_type_tokens, &mut self.string_type_tokens);
        frozen.value_type_ctors = drain(arena, frozen.value_type_ctors, &mut self.value_type_ctors);
    }

    /// Drains `type_handles` into its dense-by-[`TypeId`] `u64` column.
    fn freeze_type_handles(&mut self) {
        let entries = self.types.len();
        if self.type_handles.is_empty() && self.frozen.type_handles.entries >= entries {
            return;
        }
        let mut values = alloc::vec![0u64; entries];
        for index in 0..self.frozen.type_handles.entries.min(entries) {
            values[index] = self
                .arena
                .read_u64(self.frozen.type_handles.offset + index * 8)
                .unwrap_or(0);
        }
        for (type_id, handle) in core::mem::take(&mut self.type_handles) {
            match values.get_mut(type_id as usize) {
                Some(slot) => *slot = handle,
                None => {
                    self.type_handles.insert(type_id, handle);
                }
            }
        }
        let offset = self.arena.bytes.len();
        for value in &values {
            self.arena.push_u64(*value);
        }
        self.frozen.type_handles = DenseU64Column { offset, entries };
    }

    /// Drains `field_rva_data` into the blob table (payload bytes first, then the records).
    fn freeze_rva_blobs(&mut self) {
        if self.field_rva_data.is_empty() {
            return;
        }
        let mut records = self.frozen.field_rva_data.entries_of(&self.arena);
        for (key, blob) in core::mem::take(&mut self.field_rva_data) {
            let payload = self.arena.bytes.len() as u32;
            self.arena.bytes.to_mut().extend_from_slice(&blob);
            records.push((key, payload, blob.len() as u32));
        }
        self.frozen.field_rva_data = SortedBlobTable::write(&mut self.arena, records);
    }

    /// Writes per-type `(u32, u32)` record runs into the arena and returns the dense run column
    /// (packed `(offset + 1) << 32 | count` per type), carrying an earlier column's runs forward.
    fn freeze_runs(
        arena: &mut TableArena,
        old: DenseU64Column,
        runs: Vec<(TypeId, Vec<(u32, u32)>)>,
        entries: usize,
    ) -> DenseU64Column {
        if runs.is_empty() && old.entries >= entries {
            return old;
        }
        let mut packed = alloc::vec![0u64; entries];
        for index in 0..old.entries.min(entries) {
            packed[index] = arena.read_u64(old.offset + index * 8).unwrap_or(0);
        }
        for (type_id, records) in runs {
            let run_offset = arena.bytes.len() as u64;
            for (first, second) in &records {
                arena.push_u32(*first);
                arena.push_u32(*second);
            }
            if let Some(slot) = packed.get_mut(type_id as usize) {
                *slot = ((run_offset + 1) << 32) | records.len() as u64;
            }
        }
        let offset = arena.bytes.len();
        for value in &packed {
            arena.push_u64(*value);
        }
        DenseU64Column { offset, entries }
    }

    /// Drains a dense-id-keyed builder map into a fresh arena column of `entries` values,
    /// carrying an earlier column's values forward (a re-freeze after a delta batch). An id past
    /// `entries` stays in the map (it belongs to a method the next freeze will cover).
    fn freeze_dense_id_column(
        arena: &mut TableArena,
        old: DenseIdColumn,
        map: &mut BTreeMap<u32, u32>,
        entries: usize,
    ) -> DenseIdColumn {
        if map.is_empty() && old.entries >= entries {
            return old;
        }
        let mut values = alloc::vec![0u32; entries];
        for index in 0..old.entries.min(entries) {
            values[index] = arena.read_u32(old.offset + index * 4).unwrap_or(0);
        }
        for (id, value) in core::mem::take(map) {
            match values.get_mut(id as usize) {
                Some(slot) => *slot = value + 1,
                None => {
                    map.insert(id, value);
                }
            }
        }
        let offset = arena.bytes.len();
        for value in &values {
            arena.push_u32(*value);
        }
        DenseIdColumn { offset, entries }
    }

    /// Splits `by_token` into the arena's frozen form: `MethodDef` keys pack into one dense
    /// direct-index column per assembly (their rows are contiguous from 1 -- the census confirms
    /// a column entry per method); everything else (`MemberRef` bindings) sorts into the spill
    /// table. An earlier spill's entries are carried into the rebuilt table, so a second freeze
    /// after a delta batch stays correct.
    fn freeze_by_token(&mut self) {
        if self.by_token.is_empty() {
            return;
        }
        let map = core::mem::take(&mut self.by_token);

        let mut row_extent: BTreeMap<(u8, u32), (u32, u32)> = BTreeMap::new();
        for handle in map.keys() {
            let token = *handle as u32;
            let row = token & Token::MAX_ROW;
            if row != 0 {
                let group = ((*handle >> 32) as u8, token >> 24);
                let (max_row, bound_count) = row_extent.entry(group).or_insert((0, 0));
                *max_row = (*max_row).max(row);
                *bound_count += 1;
            }
        }
        for ((asm, table), (max_row, bound_count)) in &row_extent {
            if *max_row as usize <= (*bound_count as usize) * 4 + 64
                && !self
                    .frozen
                    .token_columns
                    .iter()
                    .any(|column| column.asm == *asm && column.table == *table)
            {
                let offset = self.arena.bytes.len();
                self.arena.bytes.to_mut().resize(offset + *max_row as usize * 4, 0);
                self.frozen.token_columns.push(DenseTokenColumn {
                    asm: *asm,
                    table: *table,
                    rows: *max_row,
                    offset,
                });
            }
        }

        let mut spill: Vec<(u64, u32)> = self.frozen.token_spill.entries_of(&self.arena);
        for (handle, id) in map {
            let token = handle as u32;
            let asm = (handle >> 32) as u8;
            let table = token >> 24;
            let row = token & Token::MAX_ROW;
            if let Some(column) = self.frozen.token_columns.iter().find(|column| {
                column.asm == asm && column.table == table && row >= 1 && row <= column.rows
            }) {
                self.arena
                    .write_u32(column.offset + (row as usize - 1) * 4, id + 1);
                continue;
            }
            spill.push((handle, id));
        }
        self.frozen.token_spill = SortedTokenTable::write(&mut self.arena, spill);
    }

    /// The string an `ldstr` token in assembly `asm` loads, as little-endian UTF-16 bytes
    /// (a flash-pool slice once frozen; [`crate::Heap::alloc_string_le`] consumes it).
    #[must_use]
    pub fn resolve_string_le(&self, asm: u8, token: Token) -> Option<&[u8]> {
        let key = asm_key(asm, token.0);
        self.frozen
            .strings
            .get(&self.arena, key)
            .or_else(|| self.strings.get(&key).map(AsRef::as_ref))
    }

    /// The method with the given id, if it exists.
    #[must_use]
    pub fn method(&self, id: MethodId) -> Option<&Method> {
        self.methods.get(id as usize)
    }

    /// One 16-byte baked type record's words `(base + 1, flags, defaults offset, count)`.
    #[cfg(feature = "code-in-place")]
    fn baked_type_record(&self, type_id: TypeId) -> Option<(u32, u32, u32, u32)> {
        let baked = self.baked.as_ref()?;
        if type_id as usize >= baked.type_count {
            return None;
        }
        let at = baked.type_table + type_id as usize * 16;
        Some((
            self.arena.read_u32(at)?,
            self.arena.read_u32(at + 4)?,
            self.arena.read_u32(at + 8)?,
            self.arena.read_u32(at + 12)?,
        ))
    }

    /// One 24-byte baked method record's six words, if this module is baked and has one.
    #[cfg(feature = "code-in-place")]
    fn baked_record(&self, id: MethodId) -> Option<(u32, u32, u32, u32, u32, u32)> {
        let baked = self.baked.as_ref()?;
        if id as usize >= baked.count {
            return None;
        }
        let at = baked.table + id as usize * 24;
        Some((
            self.arena.read_u32(at)?,
            self.arena.read_u32(at + 4)?,
            self.arena.read_u32(at + 8)?,
            self.arena.read_u32(at + 12)?,
            self.arena.read_u32(at + 16)?,
            self.arena.read_u32(at + 20)?,
        ))
    }

    /// The callable form of `id` -- see [`MethodKind`]. The one method-table accessor the
    /// interpreter's call sites need.
    #[must_use]
    pub fn method_kind(&self, id: MethodId) -> Option<MethodKind> {
        #[cfg(feature = "code-in-place")]
        if let Some(baked) = &self.baked {
            let (kind, ..) = self.baked_record(id)?;
            return if kind == 0 {
                Some(MethodKind::Managed)
            } else if baked.hints_valid {
                let (_, _, _, hint, ..) = self.baked_record(id)?;
                crate::intrinsic_registry::intrinsic_at(hint as u16).map(MethodKind::Intrinsic)
            } else {
                baked
                    .intrinsic_index
                    .get(id as usize)
                    .and_then(|index| crate::intrinsic_registry::intrinsic_at(*index))
                    .map(MethodKind::Intrinsic)
            };
        }
        match self.methods.get(id as usize)? {
            Method::Managed { .. } => Some(MethodKind::Managed),
            Method::Intrinsic { func, .. } => Some(MethodKind::Intrinsic(*func)),
        }
    }

    /// How many stack arguments a call to `id` takes.
    #[must_use]
    pub fn method_arg_count(&self, id: MethodId) -> Option<u16> {
        #[cfg(feature = "code-in-place")]
        if self.baked.is_some() {
            let (_, arg_count, ..) = self.baked_record(id)?;
            return Some(arg_count as u16);
        }
        self.methods.get(id as usize).map(Method::arg_count)
    }

    /// The decoded body of a managed method (see [`ManagedBody`] for the `code_loading` levels), or
    /// `None` for an intrinsic or unknown method. On the default `on_use` build it decodes lazily on
    /// the first call and caches; under `code-preload` it is a direct read of the resident body.
    /// Absent under `code-in-place`, which never materializes a decoded body -- an in-place consumer
    /// reads [`Module::method_code_bytes`] and [`Module::method_eh`] instead.
    #[must_use]
    #[cfg(not(feature = "code-in-place"))]
    pub fn method_body(&self, id: MethodId) -> Option<&MethodBodyImage> {
        match self.method(id)? {
            Method::Managed { body, .. } => Some(body.image()),
            Method::Intrinsic { .. } => None,
        }
    }

    /// The flash-resident instruction bytes of a managed method (the `code-in-place` fetch source:
    /// decoded one instruction per step, ip in byte offsets), or `None` for an intrinsic or unknown
    /// method.
    #[must_use]
    #[cfg(feature = "code-in-place")]
    pub fn method_code_bytes(&self, id: MethodId) -> Option<&'static [u8]> {
        if let Some(baked) = &self.baked {
            let (kind, _, code_offset, code_len, ..) = self.baked_record(id)?;
            if kind != 0 {
                return None;
            }
            return baked
                .arena_static
                .get(code_offset as usize..code_offset as usize + code_len as usize);
        }
        match self.method(id)? {
            Method::Managed { body, .. } => Some(body.code()),
            Method::Intrinsic { .. } => None,
        }
    }

    /// A managed method's exception clauses with BYTE-OFFSET regions (directly comparable to the
    /// `code-in-place` byte instruction pointer), or `None` for an intrinsic or unknown method.
    #[must_use]
    #[cfg(feature = "code-in-place")]
    pub fn method_eh(&self, id: MethodId) -> Option<Vec<EhClause>> {
        if self.baked.is_some() {
            let (kind, _, _, _, eh_offset, eh_count) = self.baked_record(id)?;
            if kind != 0 {
                return None;
            }
            return Some(self.decode_baked_eh(eh_offset as usize, eh_count as usize));
        }
        match self.method(id)? {
            Method::Managed { body, .. } => Some(body.eh().to_vec()),
            Method::Intrinsic { .. } => None,
        }
    }

    /// Materializes a baked method's 24-byte exception records (throw-path only, so the copy is
    /// off the hot path).
    #[cfg(feature = "code-in-place")]
    fn decode_baked_eh(&self, eh_offset: usize, eh_count: usize) -> Vec<EhClause> {
        (0..eh_count)
            .map(|clause| {
                let at = eh_offset + clause * 24;
                let read = |delta: usize| self.arena.read_u32(at + delta).unwrap_or(0);
                EhClause {
                    try_range: lamella_cil::InstructionRange {
                        start: read(0),
                        end: read(4),
                    },
                    handler_range: lamella_cil::InstructionRange {
                        start: read(8),
                        end: read(12),
                    },
                    kind: match read(16) {
                        0 => lamella_cil::EhKind::Catch(Token(read(20))),
                        1 => lamella_cil::EhKind::Filter {
                            filter_start: read(20),
                        },
                        3 => lamella_cil::EhKind::Fault,
                        _ => lamella_cil::EhKind::Finally,
                    },
                }
            })
            .collect()
    }

    /// Eagerly decode every managed method's body, populating the lazy cache up front -- the
    /// **`preload`** level of the `code_loading` knob. Trades RAM (all bodies resident) for speed: no
    /// per-method decode the first time it runs (no runtime decode jitter), suited to a RAM-rich host
    /// or a latency-sensitive target. The default is `on_use` (lazy, see [`Module::method_body`]);
    /// pick `preload` only where the memory is available. Returns how many bodies were decoded.
    pub fn preload_bodies(&self) -> usize {
        #[cfg(not(feature = "code-in-place"))]
        {
            (0..self.methods.len())
                .filter(|&index| self.method_body(index as MethodId).is_some())
                .count()
        }
        #[cfg(feature = "code-in-place")]
        {
            0
        }
    }

    /// Bakes this module into one self-contained little-endian image: header, directory, and
    /// the arena -- with the method table (managed code bytes + byte-form exception clauses,
    /// intrinsics as registry ids), the per-type residue (field-default value trees, base,
    /// flags), the statics, and the scalars appended into the arena first. The image is what a
    /// device flashes; [`Module::from_baked`] boots it with the arena BORROWED in place.
    ///
    /// # Errors
    /// [`BakeError::UnregisteredIntrinsic`] if a bound intrinsic has no registry id.
    #[cfg(feature = "code-in-place")]
    pub fn write_baked(&mut self, entry: Option<MethodId>) -> Result<Vec<u8>, BakeError> {
        self.freeze();

        let method_count = self.methods.len();
        let mut method_records: Vec<(u32, u32, u32, u32, u32, u32)> =
            Vec::with_capacity(method_count);
        for (id, method) in self.methods.iter().enumerate() {
            match method {
                Method::Managed { body, arg_count } => {
                    let code = body.code();
                    let code_offset = self.arena.bytes.len() as u32;
                    self.arena.bytes.to_mut().extend_from_slice(code);
                    let eh = body.eh();
                    let eh_offset = self.arena.bytes.len() as u32;
                    for clause in eh {
                        self.arena.push_u32(clause.try_range.start);
                        self.arena.push_u32(clause.try_range.end);
                        self.arena.push_u32(clause.handler_range.start);
                        self.arena.push_u32(clause.handler_range.end);
                        let (kind, payload) = match &clause.kind {
                            lamella_cil::EhKind::Catch(token) => (0, token.0),
                            lamella_cil::EhKind::Filter { filter_start } => (1, *filter_start),
                            lamella_cil::EhKind::Finally => (2, 0),
                            lamella_cil::EhKind::Fault => (3, 0),
                        };
                        self.arena.push_u32(kind);
                        self.arena.push_u32(payload);
                    }
                    method_records.push((
                        0,
                        u32::from(*arg_count),
                        code_offset,
                        code.len() as u32,
                        eh_offset,
                        eh.len() as u32,
                    ));
                }
                Method::Intrinsic { id: intrinsic, arg_count, .. } => {
                    let hint = crate::intrinsic_registry::intrinsic_index_of(*intrinsic)
                        .ok_or(BakeError::UnregisteredIntrinsic(id as MethodId))?;
                    method_records.push((
                        1,
                        u32::from(*arg_count),
                        *intrinsic,
                        u32::from(hint),
                        0,
                        0,
                    ));
                }
            }
        }
        let method_table = self.arena.bytes.len();
        for (kind, argc, a, b, c, d) in &method_records {
            self.arena.push_u32(*kind);
            self.arena.push_u32(*argc);
            self.arena.push_u32(*a);
            self.arena.push_u32(*b);
            self.arena.push_u32(*c);
            self.arena.push_u32(*d);
        }
        let method_asm = self.arena.bytes.len();
        self.arena.bytes.to_mut().extend_from_slice(&self.method_asm);

        let type_count = self.types.len();
        let mut type_records: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(type_count);
        for info in &self.types {
            let defaults_offset = self.arena.bytes.len() as u32;
            for value in info.field_defaults.iter() {
                encode_value_tree(&mut self.arena, value);
            }
            type_records.push((
                info.base.map_or(0, |base| base + 1),
                u32::from(info.value_type),
                defaults_offset,
                info.field_defaults.len() as u32,
            ));
        }
        let type_table = self.arena.bytes.len();
        for (base, flags, defaults, count) in &type_records {
            self.arena.push_u32(*base);
            self.arena.push_u32(*flags);
            self.arena.push_u32(*defaults);
            self.arena.push_u32(*count);
        }

        let statics = self.arena.bytes.len();
        for value in &self.static_defaults {
            encode_value_tree(&mut self.arena, value);
        }
        let static_ctors = self.arena.bytes.len();
        for ctor in &self.static_ctors {
            self.arena.push_u32(*ctor);
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"LMLI");
        out.extend_from_slice(&BAKED_IMAGE_VERSION.to_le_bytes());
        image_push(&mut out, entry.map_or(0, |method| u64::from(method) + 1));
        image_push(&mut out, method_count as u64);
        image_push(&mut out, method_table as u64);
        image_push(&mut out, method_asm as u64);
        image_push(&mut out, type_count as u64);
        image_push(&mut out, type_table as u64);
        image_push(&mut out, self.static_defaults.len() as u64);
        image_push(&mut out, statics as u64);
        image_push(&mut out, self.static_ctors.len() as u64);
        image_push(&mut out, static_ctors as u64);
        image_push(&mut out, self.string_type_id.map_or(0, |id| u64::from(id) + 1));
        image_push(&mut out, self.primitive_int32_token.unwrap_or(0));
        image_push(&mut out, self.primitive_int64_token.unwrap_or(0));
        image_push(&mut out, self.primitive_native_int_token.unwrap_or(0));
        #[cfg(feature = "float")]
        image_push(&mut out, self.primitive_float_token.unwrap_or(0));
        #[cfg(not(feature = "float"))]
        image_push(&mut out, 0);
        image_push(&mut out, crate::intrinsic_registry::registry_fingerprint());
        let mut directory = Vec::new();
        self.frozen.write_directory(&mut directory);
        image_push(&mut out, directory.len() as u64);
        image_push(&mut out, self.arena.bytes.len() as u64);
        image_push(&mut out, image_checksum(&directory, &self.arena.bytes));
        out.extend_from_slice(&directory);
        out.extend_from_slice(&self.arena.bytes);
        Ok(out)
    }

    /// Boots a module from a baked image, the arena BORROWED from the (flash-resident) bytes:
    /// no loader, no metadata parse, no table construction -- the binding layer costs no RAM.
    /// Returns the module and the image's entry method, if it recorded one.
    ///
    /// # Errors
    /// [`BakeError`] for a non-image, a version mismatch, or an intrinsic id this build lacks.
    #[cfg(feature = "code-in-place")]
    pub fn from_baked(image: &'static [u8]) -> Result<(Module, Option<MethodId>), BakeError> {
        if image.get(..4) != Some(b"LMLI") {
            return Err(BakeError::NotAnImage);
        }
        let version = image
            .get(4..8)
            .and_then(|word| word.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or(BakeError::NotAnImage)?;
        if version != BAKED_IMAGE_VERSION {
            return Err(BakeError::WrongVersion(version));
        }
        let words = &image[8..];
        let word = |index: usize| image_word(words, index).ok_or(BakeError::NotAnImage);
        let entry = word(0)?;
        let method_count = word(1)? as usize;
        let method_table = word(2)? as usize;
        let method_asm = word(3)? as usize;
        let type_count = word(4)? as usize;
        let type_table = word(5)? as usize;
        let static_count = word(6)? as usize;
        let statics = word(7)? as usize;
        let static_ctor_count = word(8)? as usize;
        let static_ctors_at = word(9)? as usize;
        let string_type_id = word(10)?;
        let primitive_int32 = word(11)?;
        let primitive_int64 = word(12)?;
        let primitive_native = word(13)?;
        let primitive_float = word(14)?;
        let fingerprint = word(15)?;
        let directory_len = word(16)? as usize;
        let arena_len = word(17)? as usize;
        let stored_checksum = word(18)?;
        let directory_start = 8 + 19 * 8;
        let arena_start = directory_start + directory_len;
        let directory = image
            .get(directory_start..arena_start)
            .ok_or(BakeError::NotAnImage)?;
        let arena_bytes = image
            .get(arena_start..arena_start + arena_len)
            .ok_or(BakeError::NotAnImage)?;
        let computed = image_checksum(directory, arena_bytes);
        if computed != stored_checksum {
            return Err(BakeError::ChecksumMismatch { stored: stored_checksum, computed });
        }
        let (frozen, _) =
            FrozenTables::read_directory(directory).ok_or(BakeError::NotAnImage)?;
        let arena = TableArena {
            bytes: alloc::borrow::Cow::Borrowed(arena_bytes),
        };

        let hints_valid =
            fingerprint == crate::intrinsic_registry::registry_fingerprint();
        let mut intrinsic_index: Vec<u16> = Vec::new();
        if !hints_valid {
            intrinsic_index.reserve_exact(method_count);
        }
        for index in 0..method_count {
            let at = method_table + index * 24;
            let kind = arena.read_u32(at).ok_or(BakeError::NotAnImage)?;
            if kind == 0 {
                if !hints_valid {
                    intrinsic_index.push(u16::MAX);
                }
            } else if hints_valid {
                let hint = arena.read_u32(at + 12).ok_or(BakeError::NotAnImage)?;
                let id = arena.read_u32(at + 8).ok_or(BakeError::NotAnImage)?;
                if crate::intrinsic_registry::intrinsic_at(hint as u16).is_none() {
                    return Err(BakeError::MissingIntrinsic(id));
                }
            } else {
                let id = arena.read_u32(at + 8).ok_or(BakeError::NotAnImage)?;
                let position = crate::intrinsic_registry::intrinsic_index_of(id)
                    .ok_or(BakeError::MissingIntrinsic(id))?;
                intrinsic_index.push(position);
            }
        }
        let baked = BakedMethods {
            table: method_table,
            count: method_count,
            arena_static: arena_bytes,
            intrinsic_index: intrinsic_index.into_boxed_slice(),
            hints_valid,
            method_asm,
            type_table,
            type_count,
            statics_offset: statics,
            static_count,
        };

        let static_ctor_list: Vec<MethodId> = (0..static_ctor_count)
            .filter_map(|index| arena.read_u32(static_ctors_at + index * 4))
            .collect();
        let module = Module {
            methods: Vec::new(),
            baked: Some(baked),
            arena,
            frozen,
            static_ctors: static_ctor_list,
            string_type_id: string_type_id.checked_sub(1).map(|id| id as u32),
            primitive_int32_token: (primitive_int32 != 0).then_some(primitive_int32),
            primitive_int64_token: (primitive_int64 != 0).then_some(primitive_int64),
            primitive_native_int_token: (primitive_native != 0).then_some(primitive_native),
            #[cfg(feature = "float")]
            primitive_float_token: (primitive_float != 0).then_some(primitive_float),
            ..Module::default()
        };
        #[cfg(not(feature = "float"))]
        let _ = primitive_float;
        Ok((module, entry.checked_sub(1).map(|method| method as MethodId)))
    }

    /// Marks the methods and types the program can actually reach, for the bake-time trim:
    /// a worklist from `entry` and every static constructor, decoding each reached body and
    /// following its tokens. Reachable TYPES pull their vtable targets, signature-dispatch
    /// targets, and finalizer (runtime dispatch is type-driven); a type named by `ldtoken`
    /// (C# `typeof`) is a REFLECTION ROOT keeping every member, so `Type.GetMethod` +
    /// `Invoke` over it still work after the trim. Runs on a LIVE (pre-freeze) module, where
    /// every binding is still in its builder.
    ///
    /// The returned sets are `(methods, types, ldstr string keys)` to keep.
    #[cfg(feature = "code-in-place")]
    #[must_use]
    pub fn reachable_set(&self, entry: Option<MethodId>) -> (Vec<bool>, Vec<bool>, BTreeSet<u64>) {
        let mut keep_method = alloc::vec![false; self.methods.len()];
        let mut keep_type = alloc::vec![false; self.types.len()];
        let mut reflected = alloc::vec![false; self.types.len()];
        let mut strings: BTreeSet<u64> = BTreeSet::new();
        let mut queue: Vec<MethodId> = Vec::new();

        let push = |method: MethodId, keep: &mut Vec<bool>, queue: &mut Vec<MethodId>| {
            if let Some(slot) = keep.get_mut(method as usize) {
                if !*slot {
                    *slot = true;
                    queue.push(method);
                }
            }
        };
        if let Some(entry) = entry {
            push(entry, &mut keep_method, &mut queue);
        }
        for &ctor in &self.static_ctors {
            push(ctor, &mut keep_method, &mut queue);
        }
        for attributes in self.custom_attributes.values() {
            for attribute in attributes {
                push(attribute.ctor, &mut keep_method, &mut queue);
            }
        }

        let mut type_queue: Vec<(TypeId, bool)> = self
            .custom_attributes
            .values()
            .flatten()
            .map(|attribute| (attribute.type_id, false))
            .collect();

        while !queue.is_empty() || !type_queue.is_empty() {
            while let Some((type_id, as_reflection_root)) = type_queue.pop() {
                let index = type_id as usize;
                let first_visit = matches!(keep_type.get(index), Some(false));
                if let Some(slot) = keep_type.get_mut(index) {
                    *slot = true;
                }
                let promote =
                    as_reflection_root && matches!(reflected.get(index), Some(false));
                if !(first_visit || promote) {
                    continue;
                }
                if promote {
                    if let Some(slot) = reflected.get_mut(index) {
                        *slot = true;
                    }
                }
                if let Some(info) = self.types.get(index) {
                    if first_visit {
                        for &target in info.vtable.iter() {
                            push(target, &mut keep_method, &mut queue);
                        }
                        for target in info
                            .sig_methods
                            .values()
                            .chain(info.sig_methods_nonvirtual.values())
                        {
                            push(*target, &mut keep_method, &mut queue);
                        }
                        if let Some(base) = info.base {
                            type_queue.push((base, as_reflection_root));
                        }
                        for &interface in &info.interfaces {
                            type_queue.push((interface, false));
                        }
                        if let Some(finalizer) = self.finalizers.get(&type_id).copied() {
                            push(finalizer, &mut keep_method, &mut queue);
                        }
                    }
                }
                if promote || (first_visit && as_reflection_root) {
                    if let Some(handle) = self.type_handles.get(&type_id).copied() {
                        for list in [self.type_methods.get(&handle)] {
                            for method in list.into_iter().flatten() {
                                if let Some(id) = self.resolve_by_handle(method.handle) {
                                    push(id, &mut keep_method, &mut queue);
                                }
                            }
                        }
                        for ctors in [self.type_ctors_list.get(&handle)] {
                            for (ctor_handle, _) in ctors.into_iter().flatten() {
                                if let Some(id) = self.resolve_by_handle(*ctor_handle) {
                                    push(id, &mut keep_method, &mut queue);
                                }
                            }
                        }
                        if let Some(ctor) = self.type_ctors.get(&handle).copied() {
                            push(ctor, &mut keep_method, &mut queue);
                        }
                    }
                }
            }

            let Some(method) = queue.pop() else { continue };
            if let Some(type_id) = self.method_type.get(&method).copied() {
                type_queue.push((type_id, false));
            }
            let asm = self.method_asm(method);
            let Some(Method::Managed { body, .. }) = self.methods.get(method as usize) else {
                continue;
            };
            let Ok(code) = lamella_cil::decode(body.code()) else {
                continue;
            };
            for clause in body.eh() {
                if let lamella_cil::EhKind::Catch(token) = clause.kind {
                    if let Some(type_id) = self.type_id_by_handle(asm_key(asm, token.0)) {
                        type_queue.push((type_id, false));
                    }
                }
            }
            for instruction in &code {
                let lamella_cil::Operand::Token(token) = instruction.operand else {
                    continue;
                };
                use lamella_cil::Opcode;
                match instruction.opcode {
                    Opcode::Call
                    | Opcode::Callvirt
                    | Opcode::Newobj
                    | Opcode::Jmp
                    | Opcode::Ldftn
                    | Opcode::Ldvirtftn => {
                        if let Some(target) = self.resolve(asm, token) {
                            push(target, &mut keep_method, &mut queue);
                        }
                    }
                    Opcode::Ldstr => {
                        strings.insert(asm_key(asm, token.0));
                    }
                    Opcode::Ldtoken => {
                        if let Some(type_id) = self.type_id_of(asm, token) {
                            type_queue.push((type_id, true));
                        }
                    }
                    Opcode::Ldfld
                    | Opcode::Stfld
                    | Opcode::Ldflda
                    | Opcode::Ldsfld
                    | Opcode::Stsfld
                    | Opcode::Ldsflda => {
                        if let Some(type_id) = self.field_type(asm, token) {
                            type_queue.push((type_id, false));
                        }
                    }
                    Opcode::Box
                    | Opcode::Unbox
                    | Opcode::UnboxAny
                    | Opcode::Castclass
                    | Opcode::Isinst
                    | Opcode::Constrained
                    | Opcode::Initobj
                    | Opcode::Newarr
                    | Opcode::Cpobj
                    | Opcode::Ldobj
                    | Opcode::Stobj => {
                        if let Some(type_id) = self.type_id_of(asm, token) {
                            type_queue.push((type_id, false));
                        }
                    }
                    _ => {}
                }
            }
        }
        (keep_method, keep_type, strings)
    }

    /// Checks every kept managed body against THIS build's capability profile (the baking
    /// build is compiled with the target's features, so its `cfg` set IS the profile): an
    /// opcode a feature gate traps on, or a reachable call token that resolves to nothing
    /// (an intrinsic the profile compiles out), is reported as a [`ProfileViolation`] with
    /// the method named where debug names are on. Empty means the image will not hit a
    /// profile trap the workstation could have caught -- the "runs on interpreter-P means
    /// runs on device-P" guarantee, enforced at bake.
    #[cfg(feature = "code-in-place")]
    #[must_use]
    pub fn validate_profile(&self, keep: Option<&[bool]>) -> Vec<ProfileViolation> {
        let mut violations = Vec::new();
        for (id, method) in self.methods.iter().enumerate() {
            if keep.is_some_and(|keep| !keep.get(id).copied().unwrap_or(false)) {
                continue;
            }
            let Method::Managed { body, .. } = method else {
                continue;
            };
            let method = id as MethodId;
            let name = || self.method_name(method).map(String::from);
            let asm = self.method_asm(method);
            if !cfg!(feature = "exceptions") && !body.eh().is_empty() {
                violations.push(ProfileViolation::UnsupportedOpcode {
                    method,
                    name: name(),
                    opcode: "try/catch/finally",
                });
            }
            let Ok(code) = lamella_cil::decode(body.code()) else {
                continue;
            };
            for instruction in &code {
                use lamella_cil::Opcode;
                let unsupported = match instruction.opcode {
                    Opcode::LdcR4
                    | Opcode::LdcR8
                    | Opcode::ConvR4
                    | Opcode::ConvR8
                    | Opcode::ConvRUn
                    | Opcode::Ckfinite => !cfg!(feature = "float"),
                    Opcode::Throw
                    | Opcode::Rethrow
                    | Opcode::Leave
                    | Opcode::LeaveS
                    | Opcode::Endfinally
                    | Opcode::Endfilter => !cfg!(feature = "exceptions"),
                    Opcode::Mkrefany | Opcode::Refanyval | Opcode::Refanytype => {
                        !cfg!(feature = "typed-references")
                    }
                    Opcode::Arglist => !cfg!(feature = "varargs"),
                    _ => false,
                };
                if unsupported {
                    violations.push(ProfileViolation::UnsupportedOpcode {
                        method,
                        name: name(),
                        opcode: instruction.opcode.mnemonic(),
                    });
                    continue;
                }
                let lamella_cil::Operand::Token(token) = instruction.operand else {
                    continue;
                };
                let unresolved = match instruction.opcode {
                    Opcode::Call | Opcode::Jmp | Opcode::Ldftn => {
                        self.resolve(asm, token).is_none()
                    }
                    Opcode::Newobj => {
                        self.resolve(asm, token).is_none()
                            && !self.is_delegate_ctor(asm, token)
                            && self.md_array_ctor_rank(asm, token).is_none()
                            && self.string_builder_ctor_params(asm, token).is_none()
                            && self.list_ctor_params(asm, token).is_none()
                    }
                    Opcode::Callvirt | Opcode::Ldvirtftn => {
                        self.resolve(asm, token).is_none()
                            && self.call_target(asm, token).is_none()
                            && self.delegate_invoke(asm, token).is_none()
                    }
                    _ => false,
                };
                if unresolved {
                    violations.push(ProfileViolation::UnresolvedCall {
                        method,
                        name: name(),
                        token: token.0,
                    });
                }
            }
        }
        violations
    }

    /// Discards everything [`Module::reachable_set`] did not keep, BEFORE the freeze, so every
    /// frozen table and pool shrinks to the kept surface: an unreachable managed body bakes
    /// EMPTY (calling it traps `FellThroughEnd` -- it cannot be called from kept code), an
    /// unreachable INTRINSIC demotes to that same empty managed body (so the baked image
    /// demands only the intrinsics kept code can reach -- the boot-time registry check then
    /// gates on the app's real surface, not the whole corlib's), an unreachable type loses its
    /// shape and reflection records, and the ldstr pool keeps only the strings kept code loads.
    #[cfg(feature = "code-in-place")]
    pub fn retain_reachable(
        &mut self,
        keep_method: &[bool],
        keep_type: &[bool],
        strings: &BTreeSet<u64>,
    ) {
        let kept_method =
            |handle_map: &BTreeMap<u64, MethodId>, handle: u64| handle_map.contains_key(&handle);
        let empty_raw: RawCil = &[];
        for (id, method) in self.methods.iter_mut().enumerate() {
            if keep_method.get(id).copied().unwrap_or(false) {
                continue;
            }
            match method {
                Method::Managed { body, .. } => *body = ManagedBody::from_raw(empty_raw),
                Method::Intrinsic { arg_count, .. } => {
                    *method = Method::Managed {
                        body: ManagedBody::from_raw(empty_raw),
                        arg_count: *arg_count,
                    };
                }
            }
        }
        let method_keep =
            |id: &MethodId| keep_method.get(*id as usize).copied().unwrap_or(false);
        self.by_token.retain(|_, id| method_keep(id));
        self.method_slots.retain(|id, _| method_keep(id));
        self.method_debug.retain(|id, _| method_keep(id));
        for (type_id, info) in self.types.iter_mut().enumerate() {
            if !keep_type.get(type_id).copied().unwrap_or(false) {
                info.vtable = Box::new([]);
                info.interfaces = Vec::new();
                info.sig_methods = BTreeMap::new();
                info.sig_methods_nonvirtual = BTreeMap::new();
                info.field_defaults = Box::new([]);
            } else {
                info.sig_methods.retain(|_, id| method_keep(id));
                info.sig_methods_nonvirtual.retain(|_, id| method_keep(id));
            }
        }
        let type_kept = |type_id: &TypeId| keep_type.get(*type_id as usize).copied().unwrap_or(false);
        let handle_of_kept_type: BTreeSet<u64> = self
            .type_handles
            .iter()
            .filter(|(type_id, _)| type_kept(type_id))
            .map(|(_, handle)| *handle)
            .collect();
        self.type_tokens.retain(|_, id| type_kept(id));
        self.type_full_names.retain(|id, _| type_kept(id));
        self.vtable_slot_keys.retain(|id, _| type_kept(id));
        self.finalizers.retain(|id, _| type_kept(id));
        let by_token = core::mem::take(&mut self.by_token);
        self.type_fields
            .retain(|handle, _| handle_of_kept_type.contains(handle));
        self.type_methods.retain(|handle, fields| {
            if !handle_of_kept_type.contains(handle) {
                return false;
            }
            fields.retain(|method| kept_method(&by_token, method.handle));
            true
        });
        self.type_ctors_list.retain(|handle, ctors| {
            if !handle_of_kept_type.contains(handle) {
                return false;
            }
            ctors.retain(|(ctor, _)| kept_method(&by_token, *ctor));
            true
        });
        self.type_ctors
            .retain(|handle, id| handle_of_kept_type.contains(handle) && method_keep(id));
        self.type_fields_by_name
            .retain(|(handle, _), _| handle_of_kept_type.contains(handle));
        self.type_methods_by_name.retain(|(handle, _), member| {
            handle_of_kept_type.contains(handle) && kept_method(&by_token, *member)
        });
        self.type_properties_by_name
            .retain(|(handle, _), _| handle_of_kept_type.contains(handle));
        self.reflect_types
            .retain(|handle, _| handle_of_kept_type.contains(handle));
        self.method_params
            .retain(|handle, _| kept_method(&by_token, *handle));
        self.method_attrs
            .retain(|handle, _| kept_method(&by_token, *handle));
        self.custom_attributes.retain(|handle, _| {
            handle_of_kept_type.contains(handle) || kept_method(&by_token, *handle)
        });
        self.by_token = by_token;
        self.strings.retain(|key, _| strings.contains(key));
        self.enum_constants
            .retain(|handle, _| handle_of_kept_type.contains(handle));
        let by_token_after = core::mem::take(&mut self.by_token);
        self.type_names.retain(|handle, _| {
            handle_of_kept_type.contains(handle) || kept_method(&by_token_after, *handle)
        });
        self.by_token = by_token_after;
        self.type_handles.retain(|id, _| type_kept(id));
    }

    /// A rough per-structure breakdown of the module's resident heap (content + per-entry header
    /// bytes), for LOCALIZING the load footprint -- the Phase-2 metadata-skeleton work. Approximate;
    /// the relative sizes are what matter (which structure to lean first).
    #[must_use]
    pub fn heap_report(&self) -> Vec<(&'static str, usize)> {
        let str_entry = |s: &str| s.len() + 40;
        #[cfg(not(feature = "flash-image"))]
        let raw_header = 24;
        #[cfg(feature = "flash-image")]
        let raw_header = 0;
        let methods = self
            .methods
            .iter()
            .map(|method| match method {
                Method::Managed { body, .. } => body.footprint().0 + raw_header,
                Method::Intrinsic { .. } => 8,
            })
            .sum();
        let decoded_bodies: usize = self
            .methods
            .iter()
            .map(|method| match method {
                Method::Managed { body, .. } => body.footprint().1,
                Method::Intrinsic { .. } => 0,
            })
            .sum();
        let sig_methods = self
            .types
            .iter()
            .map(|info| (info.sig_methods.len() + info.sig_methods_nonvirtual.len()) * 12)
            .sum::<usize>()
            + self.sig_lookup.keys().map(|key| str_entry(key)).sum::<usize>()
            + self.sig_pool.iter().map(|key| str_entry(key)).sum::<usize>();
        let vtables = self
            .types
            .iter()
            .map(|info| {
                info.vtable.len() * 4
                    + info.field_defaults.len() * core::mem::size_of::<Value>()
                    + info.interfaces.len() * 4
            })
            .sum();
        let vtable_slot_keys = self
            .vtable_slot_keys
            .values()
            .map(|slots| slots.len() * 8 + 24)
            .sum();
        let method_debug = self
            .method_debug
            .values()
            .map(|debug| debug.name.len() + 24 + debug.args.iter().map(|arg| arg.len() + 24).sum::<usize>())
            .sum();
        let by_name = self
            .type_methods_by_name
            .keys()
            .chain(self.type_fields_by_name.keys())
            .chain(self.type_properties_by_name.keys())
            .map(|(_, name)| str_entry(name))
            .sum();
        let call_targets = self.call_targets.len() * (16 + 24);
        let strings = self.strings.values().map(|text| text.len() + 24).sum();
        let custom_attrs = self.custom_attributes.values().map(|attrs| attrs.len() * 32 + 16).sum();
        let type_names = self
            .type_names
            .values()
            .chain(self.type_full_names.values())
            .map(|name| name.len() + 40)
            .sum();
        let method_structs = self.methods.len() * core::mem::size_of::<Method>();
        let token_maps = (self.by_token.len()
            + self.method_type.len()
            + self.field_slots.len()
            + self.field_types.len()
            + self.array_defaults.len()
            + self.array_prim_kinds.len()
            + self.method_slots.len()
            + self.static_fields.len()
            + self.type_tokens.len()
            + self.type_handles.len()
            + self.vararg_sites.len()
            + self.explicit_overrides.len()
            + self.delegate_invokes.len()
            + self.finalizers.len()
            + self.enum_widths.len()
            + self.md_array_ctors.len()
            + self.string_builder_ctors.len()
            + self.list_ctors.len()
            + self.type_sizes.len()
            + self.catch_type_tags.len()
            + self.value_type_ctors.len()
            + self.delegate_ctors.len()
            + self.enum_wide.len()
            + self.enum_flags.len()
            + self.object_type_tokens.len()
            + self.string_type_tokens.len())
            * 48;
        let data = self.method_asm.len()
            + self.static_defaults.len() * core::mem::size_of::<Value>()
            + self.field_rva_data.values().map(|bytes| bytes.len() + 24).sum::<usize>()
            + self.enum_constants.values().flat_map(|map| map.values()).map(|name| name.len() + 40).sum::<usize>();
        Vec::from([
            ("methods (raw IL)", methods),
            ("decoded bodies (OnceCell, once run)", decoded_bodies),
            ("method structs (inline)", method_structs),
            ("token->id maps/sets (~26 BTreeMaps)", token_maps),
            ("sig interner (Strings x2) + builder id maps", sig_methods),
            ("types.vtable+field_defaults", vtables),
            ("vtable_slot_keys", vtable_slot_keys),
            ("method_debug (debugger names)", method_debug),
            ("type_*_by_name indexes", by_name),
            ("call_targets", call_targets),
            ("strings (ldstr UTF-16)", strings),
            ("custom_attributes", custom_attrs),
            ("type_names+full_names", type_names),
            ("data (asm/static/rva/enum)", data),
            ("arena (frozen flat tables, owned)", self.arena.resident_len()),
            #[cfg(feature = "code-in-place")]
            (
                "baked intrinsic index column (u16/method)",
                self.baked
                    .as_ref()
                    .map_or(0, |baked| baked.intrinsic_index.len() * 2),
            ),
        ])
    }

    /// Per-table ENTRY COUNTS behind [`Module::heap_report`]'s byte totals -- the flatten/bake
    /// design input: which tables are dense-keyed (a direct-index array in the flat image), which
    /// are sparse (a sorted binary-search table), and how large each string/blob pool must be.
    #[must_use]
    pub fn table_census(&self) -> Vec<(&'static str, usize)> {
        Vec::from([
            ("methods", self.methods.len()),
            ("types", self.types.len()),
            ("arena bytes (frozen, resident)", self.arena.resident_len()),
            ("arena bytes (total incl. borrowed image)", self.arena.bytes.len()),
            ("token columns (dense, frozen)", self.frozen.token_columns.len()),
            ("token spill entries (frozen)", self.frozen.token_spill.entries),
            ("method_type column entries (frozen)", self.frozen.method_type.entries),
            ("method_slot column entries (frozen)", self.frozen.method_slot.entries),
            ("by_token", self.by_token.len()),
            ("method_type", self.method_type.len()),
            ("field_slots", self.field_slots.len()),
            ("field_types", self.field_types.len()),
            ("array_defaults", self.array_defaults.len()),
            ("array_prim_kinds", self.array_prim_kinds.len()),
            ("method_slots", self.method_slots.len()),
            ("static_fields", self.static_fields.len()),
            ("type_tokens", self.type_tokens.len()),
            ("type_handles", self.type_handles.len()),
            ("vararg_sites", self.vararg_sites.len()),
            ("explicit_overrides", self.explicit_overrides.len()),
            ("delegate_invokes", self.delegate_invokes.len()),
            ("finalizers", self.finalizers.len()),
            ("enum_widths", self.enum_widths.len()),
            ("md_array_ctors", self.md_array_ctors.len()),
            ("string_builder_ctors", self.string_builder_ctors.len()),
            ("list_ctors", self.list_ctors.len()),
            ("type_sizes", self.type_sizes.len()),
            ("catch_type_tags", self.catch_type_tags.len()),
            ("value_type_ctors", self.value_type_ctors.len()),
            ("delegate_ctors", self.delegate_ctors.len()),
            ("enum_wide", self.enum_wide.len()),
            ("enum_flags", self.enum_flags.len()),
            ("object_type_tokens", self.object_type_tokens.len()),
            ("string_type_tokens", self.string_type_tokens.len()),
            ("call_targets", self.call_targets.len()),
            ("strings (ldstr)", self.strings.len()),
            ("custom_attribute targets", self.custom_attributes.len()),
            ("type_names", self.type_names.len()),
            ("type_full_names", self.type_full_names.len()),
            ("name_to_handle", self.name_to_handle.len()),
            ("type_fields_by_name", self.type_fields_by_name.len()),
            ("type_methods_by_name", self.type_methods_by_name.len()),
            ("type_properties_by_name", self.type_properties_by_name.len()),
            ("method_debug", self.method_debug.len()),
            ("sig pool (unique sigs interned)", self.sig_pool.len()),
            (
                "sig_methods entries (all types)",
                self.types.iter().map(|info| info.sig_methods.len()).sum(),
            ),
            (
                "sig_methods_nonvirtual entries",
                self.types.iter().map(|info| info.sig_methods_nonvirtual.len()).sum(),
            ),
            (
                "vtable slots (all types)",
                self.types.iter().map(|info| info.vtable.len()).sum(),
            ),
            (
                "vtable_slot_keys entries",
                self.vtable_slot_keys.values().map(Vec::len).sum(),
            ),
            (
                "interface impls (all types)",
                self.types.iter().map(|info| info.interfaces.len()).sum(),
            ),
            (
                "field_defaults slots (all types)",
                self.types.iter().map(|info| info.field_defaults.len()).sum(),
            ),
            ("static_defaults", self.static_defaults.len()),
            ("field_rva_data", self.field_rva_data.len()),
            (
                "enum_constants (all enums)",
                self.enum_constants.values().map(BTreeMap::len).sum(),
            ),
        ])
    }

    /// The number of declared reference types in the module -- the global [`TypeId`]
    /// offset at which the next assembly's first type will land in a multi-assembly load.
    #[must_use]
    pub fn type_count(&self) -> usize {
        #[cfg(feature = "code-in-place")]
        if let Some(baked) = &self.baked {
            return baked.type_count;
        }
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.field_types.get(&self.arena, key).or_else(|| self.field_types.get(&key).copied())
    }
    }

    /// The declaring type of `method`, if recorded.
    #[must_use]
    pub fn method_type(&self, method: MethodId) -> Option<TypeId> {
        self.frozen.method_type
            .get(&self.arena, method)
            .or_else(|| self.method_type.get(&method).copied())
    }

    /// The instance-field slot a field token in assembly `asm` names, if bound.
    #[must_use]
    pub fn field_slot(&self, asm: u8, token: Token) -> Option<u32> {
        self.field_slot_by_handle(asm_key(asm, token.0))
    }

    /// The instance-field slot of the field whose asm-folded handle is `handle` (a `FieldInfo`
    /// handle that reflection carries), if it names an instance field.
    #[must_use]
    pub fn field_slot_by_handle(&self, handle: u64) -> Option<u32> {
        self.frozen.field_slots.get(&self.arena, handle).or_else(|| self.field_slots.get(&handle).copied())
    }

    /// The static storage slot of the field whose asm-folded handle is `handle` (a `FieldInfo`
    /// handle), if it names a static field.
    #[must_use]
    pub fn static_field_slot_by_handle(&self, handle: u64) -> Option<usize> {
        self.frozen.static_fields.get(&self.arena, handle).map(|slot| slot as usize).or_else(|| self.static_fields.get(&handle).copied())
    }

    /// The zero values of `type_id`'s instance fields, for allocating an instance.
    #[must_use]
    pub fn type_field_defaults(&self, type_id: TypeId) -> Option<Vec<Value>> {
        #[cfg(feature = "code-in-place")]
        if self.baked.is_some() {
            let (_, _, defaults_offset, count) = self.baked_type_record(type_id)?;
            let mut at = defaults_offset as usize;
            let mut fields = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let (value, next) = decode_value_tree(&self.arena, at, 0)?;
                fields.push(value);
                at = next;
            }
            return Some(fields);
        }
        self.types
            .get(type_id as usize)
            .map(|info| info.field_defaults.to_vec())
    }

    /// The base type of `type_id`, if it has a resolvable one -- the subtype walks' step.
    fn type_base(&self, type_id: TypeId) -> Option<TypeId> {
        #[cfg(feature = "code-in-place")]
        if self.baked.is_some() {
            return self.baked_type_record(type_id)?.0.checked_sub(1);
        }
        self.types.get(type_id as usize)?.base
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
        let key = asm_key(asm, token.0);
        self.frozen
            .array_defaults
            .get(&self.arena, key)
            .or_else(|| self.array_defaults.get(&key).cloned())
    }

    /// Binds a `newarr` element-type token in assembly `asm` to the element type's primitive
    /// kind ([`PrimKind`]: `Byte`/`Boolean` = `U1`, `SByte` = `I1`, `Int16` = `I2`,
    /// `UInt16`/`Char` = `U2`, ...). Only sized primitive element types are bound; a
    /// reference / value-type element array is left unbound (its elements stay boxed).
    pub fn bind_array_prim_kind(&mut self, asm: u8, token: Token, kind: PrimKind) {
        self.array_prim_kinds.insert(asm_key(asm, token.0), kind);
    }

    /// The element type's primitive kind of a `newarr` element-type token in assembly `asm`,
    /// or `None` when none was bound (a reference / value-type element array). `newarr` packs
    /// the array's element storage at this kind, which also sizes it for `System.Buffer`.
    #[must_use]
    pub fn array_prim_kind(&self, asm: u8, token: Token) -> Option<PrimKind> {
        let key = asm_key(asm, token.0);
        self.frozen
            .array_prim_kinds
            .get(&self.arena, key)
            .and_then(|code| u8::try_from(code).ok())
            .or_else(|| self.array_prim_kinds.get(&key).map(|kind| kind.code()))
            .and_then(PrimKind::from_code)
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
        let interned: Vec<(u32, MethodId)> = slots
            .into_iter()
            .map(|(key, method)| (self.intern_sig(&key), method))
            .collect();
        self.vtable_slot_keys.insert(type_id, interned);
    }

    /// `type_id`'s vtable slots as `(signature key, implementation)` pairs in slot order, if
    /// recorded -- the seed a derived type with a cross-assembly base inherits.
    #[must_use]
    pub fn vtable_slot_keys(&self, type_id: TypeId) -> Option<Vec<(String, MethodId)>> {
        let slots = self
            .frozen_slot_run(type_id)
            .or_else(|| self.vtable_slot_keys.get(&type_id).cloned())?;
        Some(
            slots
                .into_iter()
                .map(|(sig, method)| (self.sig_text(sig), method))
                .collect(),
        )
    }

    /// One type's frozen vtable slot run, if the freeze recorded one.
    fn frozen_slot_run(&self, type_id: TypeId) -> Option<Vec<(u32, MethodId)>> {
        let packed = self.frozen.vtable_slot_runs.get(&self.arena, type_id)?;
        let offset = ((packed >> 32) as usize).checked_sub(1)?;
        let count = (packed & 0xFFFF_FFFF) as usize;
        Some(
            (0..count)
                .filter_map(|index| {
                    Some((
                        self.arena.read_u32(offset + index * 8)?,
                        self.arena.read_u32(offset + index * 8 + 4)?,
                    ))
                })
                .collect(),
        )
    }

    /// Interns a signature dispatch key, returning its stable 1-based id.
    pub fn intern_sig(&mut self, key: &str) -> u32 {
        if let Some(id) = self.sig_lookup.get(key) {
            return *id;
        }
        let id = (self.sig_pool.len() + 1) as u32;
        self.sig_pool.push(String::from(key));
        self.sig_lookup.insert(String::from(key), id);
        id
    }

    /// The interned text of `sig` (empty for an unknown id) -- load-time consumers only.
    fn sig_text(&self, sig: u32) -> String {
        sig.checked_sub(1)
            .and_then(|index| self.sig_pool.get(index as usize))
            .cloned()
            .unwrap_or_default()
    }

    /// Records a virtual method's vtable slot (for `callvirt` dispatch).
    pub fn bind_method_slot(&mut self, method: MethodId, slot: u32) {
        self.method_slots.insert(method, slot);
    }

    /// The vtable slot of `method`, if it is virtual.
    #[must_use]
    pub fn method_slot(&self, method: MethodId) -> Option<u32> {
        self.frozen.method_slot
            .get(&self.arena, method)
            .or_else(|| self.method_slots.get(&method).copied())
    }

    /// The implementation at `slot` in `type_id`'s vtable, if present -- the
    /// `callvirt` target for a `this` of that runtime type.
    #[must_use]
    pub fn vtable_entry(&self, type_id: TypeId, slot: u32) -> Option<MethodId> {
        if let Some(packed) = self.frozen.vtable_runs.get(&self.arena, type_id) {
            let offset = ((packed >> 32) as usize).checked_sub(1)?;
            let count = packed & 0xFFFF_FFFF;
            if u64::from(slot) < count {
                return self.arena.read_u32(offset + slot as usize * 4);
            }
            return None;
        }
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.static_fields.get(&self.arena, key).map(|slot| slot as usize).or_else(|| self.static_fields.get(&key).copied())
    }
    }

    /// How many static-field slots the module declares -- the cheap per-access guard the
    /// interpreter compares against the [`crate::interp::Vm`]'s live statics before deciding to
    /// seed them.
    #[must_use]
    pub fn static_field_count(&self) -> usize {
        #[cfg(feature = "code-in-place")]
        if let Some(baked) = &self.baked {
            return baked.static_count;
        }
        self.static_defaults.len()
    }

    /// The static fields' seed values, decoded/cloned ON THE SEEDING PATH ONLY (the per-access
    /// guard is [`Module::static_field_count`]): from the image's value trees on a baked module,
    /// from the loader's Vec on a live one.
    #[must_use]
    pub fn decode_static_defaults(&self) -> Vec<Value> {
        #[cfg(feature = "code-in-place")]
        if let Some(baked) = &self.baked {
            let mut at = baked.statics_offset;
            let mut seeds = Vec::with_capacity(baked.static_count);
            for _ in 0..baked.static_count {
                let Some((value, next)) = decode_value_tree(&self.arena, at, 0) else {
                    break;
                };
                seeds.push(value);
                at = next;
            }
            return seeds;
        }
        self.static_defaults.clone()
    }

    /// The zero values of all static fields, indexed by slot (to initialize storage).
    #[must_use]
    pub fn static_field_defaults(&self) -> &[Value] {
        &self.static_defaults
    }

    /// Records a static constructor (`.cctor`) to run before the entry point. The declaring
    /// type (already bound via [`Module::set_method_type`]) keys the lazy-trigger map.
    pub fn add_static_ctor(&mut self, method: MethodId) {
        if let Some(type_id) = self.method_type(method) {
            self.cctor_types.insert(type_id, method);
        }
        self.static_ctors.push(method);
    }

    /// The `.cctor` of `type_id`, if it declares one -- the lazy-initialization trigger map.
    #[must_use]
    pub fn cctor_of_type(&self, type_id: TypeId) -> Option<MethodId> {
        self.cctor_types.get(&type_id).copied()
    }

    /// Records the `[DllImport]` target of the bodyless PinvokeImpl method `token`.
    pub fn bind_pinvoke_target(&mut self, asm: u8, token: u32, target: PInvokeTarget) {
        self.pinvoke_targets.insert(asm_key(asm, token), target);
    }

    /// The `[DllImport]` target of the method `token` in assembly `asm`, if it is one.
    #[must_use]
    pub fn pinvoke_target(&self, asm: u8, token: u32) -> Option<&PInvokeTarget> {
        self.pinvoke_targets.get(&asm_key(asm, token))
    }

    /// Records that static storage slots `start..end` belong to `type_id`, so a static
    /// access can trigger that type's lazy `.cctor`.
    pub fn bind_static_slot_range(&mut self, start: u32, end: u32, type_id: TypeId) {
        if end > start {
            self.static_slot_types.push((start, end, type_id));
        }
    }

    /// The type owning static storage slot `slot`, if recorded.
    #[must_use]
    pub fn type_of_static_slot(&self, slot: usize) -> Option<TypeId> {
        let slot = slot as u32;
        self.static_slot_types
            .iter()
            .find(|&&(start, end, _)| slot >= start && slot < end)
            .map(|&(_, _, type_id)| type_id)
    }

    /// Records `type_id`'s `Finalize` method (its destructor), so allocating an instance
    /// registers it for finalization and the collector can run it on reclamation.
    pub fn set_finalizer(&mut self, type_id: TypeId, method: MethodId) {
        self.finalizers.insert(type_id, method);
    }

    /// The `Finalize` method `type_id` declares, if any.
    #[must_use]
    pub fn finalizer_of(&self, type_id: TypeId) -> Option<MethodId> {
        self.frozen.finalizers.get(&self.arena, type_id as u64).or_else(|| self.finalizers.get(&type_id).copied())
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
        if let Some((offset, count)) = self.frozen.enum_constants.get(&self.arena, handle) {
            for index in 0..count as usize {
                let at = offset as usize + index * 16;
                if self.arena.read_u64(at).map(|bits| bits as i64) == Some(value) {
                    let id = self.arena.read_u32(at + 8)?;
                    return self.frozen.name_pool.str_at(&self.arena, id);
                }
            }
            return None;
        }
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
        for (value, constant) in self.enum_members_resolved_direct(handle)? {
            let matched = if ignore_case {
                constant.eq_ignore_ascii_case(name)
            } else {
                constant == name
            };
            if matched {
                return Some(value);
            }
        }
        None
    }

    /// The members of exactly the enum `handle` names (no cross-assembly resolution), ordered
    /// ascending by value: the frozen run materialized, else the builder map.
    fn enum_members_resolved_direct(&self, handle: u64) -> Option<Vec<(i64, String)>> {
        if let Some((offset, count)) = self.frozen.enum_constants.get(&self.arena, handle) {
            return Some(
                (0..count as usize)
                    .filter_map(|index| {
                        let at = offset as usize + index * 16;
                        let value = self.arena.read_u64(at)? as i64;
                        let name = self
                            .frozen
                            .name_pool
                            .str_at(&self.arena, self.arena.read_u32(at + 8)?)?;
                        Some((value, String::from(name)))
                    })
                    .collect(),
            );
        }
        self.enum_constants.get(&handle).map(|constants| {
            constants
                .iter()
                .map(|(value, name)| (*value, name.clone()))
                .collect()
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
        let known = |handle: u64| {
            self.frozen.enum_constants.get(&self.arena, handle).is_some()
                || self.enum_constants.contains_key(&handle)
        };
        if known(handle) {
            return true;
        }
        self.type_id_by_handle(handle)
            .and_then(|type_id| self.type_handle_of(type_id))
            .is_some_and(known)
    }

    /// The constant map of the enum named by `handle`, resolving ACROSS assemblies the same way
    /// [`Self::enum_value_name_resolved`] does: the direct handle (same-assembly), then -- on a
    /// miss -- the enum's CANONICAL handle via the shared [`TypeId`] (a program naming a corlib
    /// enum by `TypeRef`). The map is `value -> name`, ordered ascending by value (a `BTreeMap`),
    /// which is the order `Enum.GetNames` / `GetValues` report and the flags formatter walks.
    fn enum_members_resolved(&self, handle: u64) -> Option<Vec<(i64, String)>> {
        if let Some(members) = self.enum_members_resolved_direct(handle) {
            return Some(members);
        }
        let canonical = self.type_handle_of(self.type_id_by_handle(handle)?)?;
        self.enum_members_resolved_direct(canonical)
    }

    /// Records that the enum type `token` in assembly `asm` carries `[FlagsAttribute]`.
    pub fn set_enum_flags(&mut self, asm: u8, token: u32) {
        self.enum_flags.insert(asm_key(asm, token));
    }

    /// Whether the enum named by `handle` carries `[FlagsAttribute]`, resolving across assemblies
    /// (direct handle, then the canonical handle via the shared [`TypeId`]) like the name lookups.
    #[must_use]
    pub fn enum_is_flags_by_handle(&self, handle: u64) -> bool {
        if self.enum_flags_contains(handle) {
            return true;
        }
        self.type_id_by_handle(handle)
            .and_then(|type_id| self.type_handle_of(type_id))
            .is_some_and(|canonical| self.enum_flags_contains(canonical))
    }

    /// Records the underlying byte `width` (1/2/4/8) of the enum type `token` in assembly `asm`.
    pub fn set_enum_width(&mut self, asm: u8, token: u32, width: u8) {
        self.enum_widths.insert(asm_key(asm, token), width);
    }

    /// The underlying byte width of the enum named by `handle` (resolving across assemblies),
    /// defaulting to 4 (`int`) when unknown -- the width `Enum.Format`'s "X" zero-pads to.
    #[must_use]
    pub fn enum_width_by_handle(&self, handle: u64) -> u8 {
        if let Some(width) = self.enum_width_of(handle) {
            return width;
        }
        self.type_id_by_handle(handle)
            .and_then(|type_id| self.type_handle_of(type_id))
            .and_then(|canonical| self.enum_width_of(canonical))
            .unwrap_or(4)
    }

    /// One enum handle's recorded byte width, frozen table first.
    fn enum_width_of(&self, handle: u64) -> Option<u8> {
        self.frozen
            .enum_widths
            .get(&self.arena, handle)
            .map(|width| width as u8)
            .or_else(|| self.enum_widths.get(&handle).copied())
    }

    /// Whether the enum named by `handle` carries `[Flags]`, frozen set first.
    fn enum_flags_contains(&self, handle: u64) -> bool {
        self.frozen.enum_flags.contains(&self.arena, handle) || self.enum_flags.contains(&handle)
    }

    /// Whether the enum named by `handle` has a 64-bit underlying type, frozen set first.
    fn enum_wide_contains(&self, handle: u64) -> bool {
        self.frozen.enum_wide.contains(&self.arena, handle) || self.enum_wide.contains(&handle)
    }

    /// The enum's members as `(value, name)` pairs ordered ascending by value -- the order
    /// `Enum.GetNames` and `Enum.GetValues` report. Resolves across assemblies; `None` if the
    /// handle names no known enum.
    #[must_use]
    pub fn enum_members_by_handle(&self, handle: u64) -> Option<Vec<(i64, String)>> {
        self.enum_members_resolved(handle)
    }

    /// Renders `value` as `Enum.ToString` / the "G" format does: for a `[Flags]` enum, the
    /// comma-joined member names the value decomposes into (`Enum.FormatFlags`); otherwise the
    /// single member name. `None` when no name applies (the caller renders the underlying number,
    /// matching .NET's fallback). Resolves across assemblies. `flags` forces the flags algorithm
    /// regardless of the attribute (the "F" format).
    #[must_use]
    pub fn enum_name_or_flags(&self, handle: u64, value: i64, flags: bool) -> Option<String> {
        let constants = self.enum_members_resolved(handle)?;
        if flags || self.enum_is_flags_by_handle(handle) {
            Some(format_flag_names(&constants, value))
        } else {
            constants
                .iter()
                .find(|(member, _)| *member == value)
                .map(|(_, name)| name.clone())
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
        self.enum_wide_contains(handle)
    }

    /// Records the [`BoxedPrimitive`] display kind of a `box` operand type token in assembly `asm`
    /// (`System.Boolean` / `System.Char`), so a boxed value of that type displays as `"True"` /
    /// `"False"` or its character rather than its raw underlying `Int32`. The loader derives the
    /// kind from the operand's metadata type name at load; only these two primitives are recorded.
    pub fn bind_box_primitive(&mut self, asm: u8, token: Token, kind: BoxedPrimitive) {
        self.box_primitives.insert(asm_key(asm, token.0), kind);
    }

    /// The [`BoxedPrimitive`] display kind of the box whose value type's asm-folded token is
    /// `handle` -- the token an [`Object::Boxed`](crate::Object::Boxed) carries -- if the loader
    /// recorded one. `None` for any other boxed value type (a plain integer, an enum, a struct),
    /// which renders through its normal display path.
    #[must_use]
    pub fn box_primitive_by_handle(&self, handle: u64) -> Option<BoxedPrimitive> {
        if let Some(kind) = self.frozen.box_primitives.get(&self.arena, handle) {
            return match kind {
                0 => Some(BoxedPrimitive::Boolean),
                1 => Some(BoxedPrimitive::Char),
                _ => None,
            };
        }
        self.box_primitives.get(&handle).copied()
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
        let Some(type_id) = self.method_type(method) else {
            return false;
        };
        #[cfg(feature = "code-in-place")]
        if self.baked.is_some() {
            return self
                .baked_type_record(type_id)
                .is_some_and(|(_, flags, _, _)| flags & 1 != 0);
        }
        self.types
            .get(type_id as usize)
            .is_some_and(|info| info.value_type)
    }

    /// The `index`th direct interface of `type_id` (frozen run first, builder during load),
    /// `None` past the end -- the allocation-free iteration the subtype walks use.
    fn type_interface_at(&self, type_id: TypeId, index: usize) -> Option<TypeId> {
        if let Some(packed) = self.frozen.interface_runs.get(&self.arena, type_id) {
            let offset = ((packed >> 32) as usize).checked_sub(1)?;
            let count = (packed & 0xFFFF_FFFF) as usize;
            if index < count {
                return self.arena.read_u32(offset + index * 4);
            }
            return None;
        }
        self.types
            .get(type_id as usize)?
            .interfaces
            .get(index)
            .copied()
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
        self.frozen.type_handles.get(&self.arena, type_id).or_else(|| self.type_handles.get(&type_id).copied())
    }

    /// The [`TypeId`] a type token in assembly `asm` names, if it is a same-module type.
    #[must_use]
    pub fn type_id_of(&self, asm: u8, token: Token) -> Option<TypeId> {
        self.type_id_by_handle(asm_key(asm, token.0))
    }

    /// The [`TypeId`] an already-asm-folded type-token handle names, if it is a declared type.
    /// (A boxed value carries its value type's folded token; this maps it back to a [`TypeId`]
    /// so a cast on the box can consult the value type's implemented interfaces.)
    #[must_use]
    pub fn type_id_by_handle(&self, handle: u64) -> Option<TypeId> {
        self.frozen.type_tokens.get(&self.arena, handle).or_else(|| self.type_tokens.get(&handle).copied())
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
                    for index in 0.. {
                        let Some(iface) = self.type_interface_at(id, index) else {
                            break;
                        };
                        if !pending.contains(&iface) {
                            pending.push(iface);
                        }
                    }
                    current = self.type_base(id);
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
            for index in 0.. {
                let Some(base_iface) = self.type_interface_at(iface, index) else {
                    break;
                };
                if !visited.contains(&base_iface) {
                    pending.push(base_iface);
                }
            }
        }
        false
    }

    /// Records `type_id`'s methods keyed by signature, for interface / abstract
    /// dispatch (the map should include inherited methods).
    pub fn set_sig_methods(&mut self, type_id: TypeId, methods: BTreeMap<String, MethodId>) {
        let interned: BTreeMap<u32, MethodId> = methods
            .into_iter()
            .map(|(key, method)| (self.intern_sig(&key), method))
            .collect();
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.sig_methods = interned;
        }
    }

    /// The method of `type_id` matching `sig_key` -- the `callvirt` target for an
    /// interface or abstract method on a `this` of that runtime type.
    #[must_use]
    pub fn sig_dispatch(&self, type_id: TypeId, sig: u32) -> Option<MethodId> {
        self.scan_sig_run(self.frozen.sig_virtual_runs, type_id, sig)
            .or_else(|| self.types.get(type_id as usize)?.sig_methods.get(&sig).copied())
    }

    /// Scans one type's frozen `(SigId, method)` run for `sig` -- a short contiguous walk.
    fn scan_sig_run(&self, runs: DenseU64Column, type_id: TypeId, sig: u32) -> Option<MethodId> {
        let packed = runs.get(&self.arena, type_id)?;
        let offset = ((packed >> 32) as usize).checked_sub(1)?;
        let count = (packed & 0xFFFF_FFFF) as usize;
        for index in 0..count {
            if self.arena.read_u32(offset + index * 8)? == sig {
                return self.arena.read_u32(offset + index * 8 + 4);
            }
        }
        None
    }

    /// Records `type_id`'s non-virtual instance methods keyed by signature (the map should include
    /// inherited methods), for [`Module::sig_dispatch_nonvirtual`].
    pub fn set_sig_methods_nonvirtual(
        &mut self,
        type_id: TypeId,
        methods: BTreeMap<String, MethodId>,
    ) {
        let interned: BTreeMap<u32, MethodId> = methods
            .into_iter()
            .map(|(key, method)| (self.intern_sig(&key), method))
            .collect();
        if let Some(info) = self.types.get_mut(type_id as usize) {
            info.sig_methods_nonvirtual = interned;
        }
    }

    /// The non-virtual instance method of `type_id` matching `sig_key` -- the last-resort
    /// `callvirt` target when the static target is absent (its declaring type is in no loaded
    /// assembly), so dispatch falls to the runtime type's own method by signature.
    #[must_use]
    pub fn sig_dispatch_nonvirtual(&self, type_id: TypeId, sig: u32) -> Option<MethodId> {
        self.scan_sig_run(self.frozen.sig_nonvirtual_runs, type_id, sig)
            .or_else(|| {
                self.types
                    .get(type_id as usize)?
                    .sig_methods_nonvirtual
                    .get(&sig)
                    .copied()
            })
    }

    /// Records a `callvirt` token's target signature key and argument count in assembly
    /// `asm`, for dispatching a method with no vtable slot / no resolvable body.
    pub fn bind_call_target(&mut self, asm: u8, token: Token, sig_key: String, arg_count: u16) {
        let sig = self.intern_sig(&sig_key);
        self.call_targets
            .insert(asm_key(asm, token.0), (sig, arg_count));
    }

    /// A `callvirt` token's target signature key and argument count in assembly `asm`, if
    /// recorded.
    #[must_use]
    pub fn call_target(&self, asm: u8, token: Token) -> Option<(u32, u16)> {
        let table = token.0 >> 24;
        let row = token.0 & Token::MAX_ROW;
        if row != 0 {
            for column in &self.frozen.call_target_columns {
                if column.asm == asm && column.table == table && row <= column.rows {
                    let packed = self
                        .arena
                        .read_u64(column.offset + (row as usize - 1) * 8)
                        .unwrap_or(0);
                    if packed != 0 {
                        return Some(((packed >> 16) as u32, packed as u16));
                    }
                    break;
                }
            }
        }
        let key = asm_key(asm, token.0);
        self.frozen
            .call_targets
            .get(&self.arena, key)
            .map(|(sig, count)| (sig, count as u16))
            .or_else(|| self.call_targets.get(&key).copied())
    }

    /// Records a vararg `call` token's [`VarargSite`] in assembly `asm`.
    pub fn bind_vararg_site(&mut self, asm: u8, token: Token, site: VarargSite) {
        self.vararg_sites.insert(asm_key(asm, token.0), site);
    }

    /// A `call` token's [`VarargSite`] in assembly `asm`, if it is a vararg call site.
    #[must_use]
    pub fn vararg_site(&self, asm: u8, token: Token) -> Option<&VarargSite> {
        self.vararg_sites.get(&asm_key(asm, token.0))
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
        let handle = asm_key(asm, decl_token.0);
        packed_override_key(type_id, handle)
            .and_then(|key| self.frozen.explicit_overrides.get(&self.arena, key))
            .or_else(|| self.explicit_overrides.get(&(type_id, handle)).copied())
    }

    /// Marks `token` in assembly `asm` as a delegate constructor, so `newobj` on it builds
    /// a delegate.
    pub fn mark_delegate_ctor(&mut self, asm: u8, token: Token) {
        self.delegate_ctors.insert(asm_key(asm, token.0));
    }

    /// Whether `token` in assembly `asm` constructs a delegate.
    #[must_use]
    pub fn is_delegate_ctor(&self, asm: u8, token: Token) -> bool {
        {
        let key = asm_key(asm, token.0);
        self.frozen.delegate_ctors.contains(&self.arena, key) || self.delegate_ctors.contains(&key)
    }
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.md_array_ctors.get(&self.arena, key).map(|count| count as u16).or_else(|| self.md_array_ctors.get(&key).copied())
    }
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
        let key = asm_key(asm, token.0);
        self.frozen
            .string_builder_ctors
            .get(&self.arena, key)
            .map(|count| count as u16)
            .or_else(|| self.string_builder_ctors.get(&key).copied())
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.list_ctors.get(&self.arena, key).map(|count| count as u16).or_else(|| self.list_ctors.get(&key).copied())
    }
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.delegate_invokes.get(&self.arena, key).map(|count| count as u16).or_else(|| self.delegate_invokes.get(&key).copied())
    }
    }

    /// Marks a `newobj` token in assembly `asm` as constructing a value type, so `newobj`
    /// builds a struct value in place rather than a heap instance.
    pub fn mark_value_type_ctor(&mut self, asm: u8, token: Token) {
        self.value_type_ctors.insert(asm_key(asm, token.0));
    }

    /// Whether a `newobj` token in assembly `asm` constructs a value type (a struct).
    #[must_use]
    pub fn is_value_type_ctor(&self, asm: u8, token: Token) -> bool {
        {
        let key = asm_key(asm, token.0);
        self.frozen.value_type_ctors.contains(&self.arena, key) || self.value_type_ctors.contains(&key)
    }
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
        self.frozen.field_rva_data.get(&self.arena, handle).or_else(|| self.field_rva_data.get(&handle).map(AsRef::as_ref))
    }

    /// Records that a type token in assembly `asm` names `System.Object` (a universal
    /// catch-all target for a type test).
    pub fn mark_object_type_token(&mut self, asm: u8, token: Token) {
        self.object_type_tokens.insert(asm_key(asm, token.0));
    }

    /// Whether a type token in assembly `asm` names `System.Object`.
    #[must_use]
    pub fn is_object_type_token(&self, asm: u8, token: Token) -> bool {
        {
        let key = asm_key(asm, token.0);
        self.frozen.object_type_tokens.contains(&self.arena, key) || self.object_type_tokens.contains(&key)
    }
    }

    /// Records that a type token in assembly `asm` names `System.String`.
    pub fn mark_string_type_token(&mut self, asm: u8, token: Token) {
        self.string_type_tokens.insert(asm_key(asm, token.0));
    }

    /// Whether a type token in assembly `asm` names `System.String`.
    #[must_use]
    pub fn is_string_type_token(&self, asm: u8, token: Token) -> bool {
        {
        let key = asm_key(asm, token.0);
        self.frozen.string_type_tokens.contains(&self.arena, key) || self.string_type_tokens.contains(&key)
    }
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.catch_type_tags.get(&self.arena, key).or_else(|| self.catch_type_tags.get(&key).copied())
    }
    }

    /// The simple name of a type token, keyed by its asm-folded value (the handle a
    /// `Type` intrinsic receives), if recorded.
    #[must_use]
    pub fn type_name_by_handle(&self, handle: u64) -> Option<&str> {
        self.frozen
            .type_names
            .get(&self.arena, handle)
            .and_then(|id| self.frozen.name_pool.str_at(&self.arena, id))
            .or_else(|| self.type_names.get(&handle).map(String::as_str))
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

    /// Records a field's reflection flags: `is_literal` (`FieldAttributes.Literal` -- a
    /// `const`) and `is_static`, keyed by the field's asm-folded token.
    pub fn bind_field_meta(&mut self, field_handle: u64, is_literal: bool, is_static: bool) {
        self.field_meta.insert(field_handle, (is_literal, is_static));
    }

    /// The `(is_literal, is_static)` flags recorded for a field handle, if any.
    #[must_use]
    pub fn field_meta(&self, field_handle: u64) -> Option<(bool, bool)> {
        self.field_meta.get(&field_handle).copied()
    }

    /// Records a `literal` field's decoded Constant-row value (see [`FieldConstant`]).
    pub fn bind_field_constant(&mut self, field_handle: u64, constant: FieldConstant) {
        self.field_constants.insert(field_handle, constant);
    }

    /// The Constant-row value recorded for a `literal` field handle, if any.
    #[must_use]
    pub fn field_constant(&self, field_handle: u64) -> Option<&FieldConstant> {
        self.field_constants.get(&field_handle)
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
        self.frozen_name_lookup(self.frozen.fields_by_name, type_handle, name)
            .or_else(|| {
                self.type_fields_by_name
                    .get(&(type_handle, name.to_string()))
                    .copied()
            })
    }

    /// One by-name index lookup against the frozen tables: resolve the runtime string to its
    /// pool id, pack the key, binary-search.
    fn frozen_name_lookup(&self, table: SortedWideTable, handle: u64, name: &str) -> Option<u64> {
        let id = self.frozen.name_pool.id_of(&self.arena, name)?;
        let (high, low) = table.get(&self.arena, packed_name_key(handle, id)?)?;
        Some((u64::from(high) << 32) | u64::from(low))
    }

    /// The asm-folded `MethodDef` token of the method named `name` on the type whose handle is
    /// `type_handle` (the `MethodInfo` handle `Type.GetMethod` returns), or `None`.
    #[must_use]
    pub fn type_method_handle(&self, type_handle: u64, name: &str) -> Option<u64> {
        self.frozen_name_lookup(self.frozen.methods_by_name, type_handle, name)
            .or_else(|| {
                self.type_methods_by_name
                    .get(&(type_handle, name.to_string()))
                    .copied()
            })
    }

    /// The asm-folded `Property` token of the property named `name` on the type whose handle is
    /// `type_handle` (the `PropertyInfo` handle `Type.GetProperty` returns), or `None`.
    #[must_use]
    pub fn type_property_handle(&self, type_handle: u64, name: &str) -> Option<u64> {
        self.frozen_name_lookup(self.frozen.properties_by_name, type_handle, name)
            .or_else(|| {
                self.type_properties_by_name
                    .get(&(type_handle, name.to_string()))
                    .copied()
            })
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
        self.frozen
            .name_pool
            .id_of(&self.arena, full_name)
            .and_then(|id| self.frozen.name_to_handle.get(&self.arena, u64::from(id)))
            .map(|(high, low)| (u64::from(high) << 32) | u64::from(low))
            .or_else(|| self.name_to_handle.get(full_name).copied())
    }

    /// The reflection introspection metadata recorded for the type whose asm-folded handle is
    /// `handle`, or `None` if the handle names no recorded type.
    #[must_use]
    pub fn reflect_type(&self, handle: u64) -> Option<ReflectType> {
        if let Some(direct) = self.reflect_type_direct(handle) {
            return Some(direct);
        }
        let canonical = self.type_handle_of(self.type_id_by_handle(handle)?)?;
        if canonical == handle {
            return None;
        }
        self.reflect_type_direct(canonical)
    }

    /// The reflection record keyed DIRECTLY by `handle` (no cross-assembly folding): the frozen
    /// table first, then the mutable one. [`Self::reflect_type`] adds the shared-`TypeId` fallback.
    fn reflect_type_direct(&self, handle: u64) -> Option<ReflectType> {
        if let Some((namespace, full, flags, base)) = self.frozen.reflect_types.get(&self.arena, handle) {
            let pool_str = |id: u32| {
                self.frozen
                    .name_pool
                    .str_at(&self.arena, id)
                    .map(String::from)
                    .unwrap_or_default()
            };
            return Some(ReflectType {
                namespace: pool_str(namespace),
                full_name: pool_str(full),
                is_enum: flags & REFLECT_ENUM != 0,
                is_value_type: flags & REFLECT_VALUE_TYPE != 0,
                is_interface: flags & REFLECT_INTERFACE != 0,
                is_abstract: flags & REFLECT_ABSTRACT != 0,
                is_public: flags & REFLECT_PUBLIC != 0,
                base_handle: base,
            });
        }
        self.reflect_types.get(&handle).cloned()
    }

    /// Records the field list (for `Type.GetFields`) of the type whose asm-folded handle is
    /// `type_handle`, in declaration order.
    pub fn bind_type_fields(&mut self, type_handle: u64, fields: Vec<ReflectField>) {
        self.type_fields.insert(type_handle, fields);
    }

    /// The fields (declaration order) of the type whose asm-folded handle is `type_handle`, for
    /// `Type.GetFields` enumeration; empty if none recorded.
    #[must_use]
    pub fn type_fields(&self, type_handle: u64) -> Vec<ReflectField> {
        if let Some((offset, count)) = self.frozen.type_fields.get(&self.arena, type_handle) {
            return (0..count as usize)
                .filter_map(|index| {
                    let at = offset as usize + index * 16;
                    let flags = self.arena.read_u32(at + 8)?;
                    Some(ReflectField {
                        handle: self.arena.read_u64(at)?,
                        is_static: flags & 1 != 0,
                        is_public: flags & 2 != 0,
                    })
                })
                .collect();
        }
        self.type_fields.get(&type_handle).cloned().unwrap_or_default()
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
        self.frozen
            .type_ctors
            .get(&self.arena, type_handle)
            .or_else(|| self.type_ctors.get(&type_handle).copied())
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
    pub fn type_ctors_list(&self, type_handle: u64) -> Vec<(u64, usize)> {
        if let Some((offset, count)) = self.frozen.type_ctors_list.get(&self.arena, type_handle) {
            return (0..count as usize)
                .filter_map(|index| {
                    let at = offset as usize + index * 16;
                    Some((
                        self.arena.read_u64(at)?,
                        self.arena.read_u32(at + 8)? as usize,
                    ))
                })
                .collect();
        }
        self.type_ctors_list
            .get(&type_handle)
            .cloned()
            .unwrap_or_default()
    }

    /// Records the method list (for `Type.GetMethods`, constructors excluded) of the type whose
    /// asm-folded handle is `type_handle`, in declaration order.
    pub fn bind_type_methods(&mut self, type_handle: u64, methods: Vec<ReflectMethod>) {
        self.type_methods.insert(type_handle, methods);
    }

    /// The methods (declaration order, constructors excluded) of the type whose asm-folded handle
    /// is `type_handle`, for `Type.GetMethods` enumeration; empty if none recorded.
    #[must_use]
    pub fn type_methods(&self, type_handle: u64) -> Vec<ReflectMethod> {
        if let Some((offset, count)) = self.frozen.type_methods.get(&self.arena, type_handle) {
            return (0..count as usize)
                .filter_map(|index| {
                    let at = offset as usize + index * 16;
                    let flags = self.arena.read_u32(at + 8)?;
                    Some(ReflectMethod {
                        handle: self.arena.read_u64(at)?,
                        is_static: flags & 1 != 0,
                        is_public: flags & 2 != 0,
                    })
                })
                .collect();
        }
        self.type_methods.get(&type_handle).cloned().unwrap_or_default()
    }

    /// Records a method's parameters (for `MethodBase.GetParameters`), keyed by its asm-folded handle.
    pub fn bind_method_params(&mut self, handle: u64, params: Vec<MethodParam>) {
        self.method_params.insert(handle, params);
    }

    /// The parameters of the method whose asm-folded handle is `handle` (empty if none recorded).
    #[must_use]
    pub fn method_params(&self, handle: u64) -> Vec<MethodParam> {
        if let Some((offset, count)) = self.frozen.method_params.get(&self.arena, handle) {
            return (0..count as usize)
                .filter_map(|index| {
                    let at = offset as usize + index * 16;
                    let type_handle = (u64::from(self.arena.read_u32(at)?) << 32)
                        | u64::from(self.arena.read_u32(at + 4)?);
                    let name = self
                        .frozen
                        .name_pool
                        .str_at(&self.arena, self.arena.read_u32(at + 8)?)
                        .map(String::from)
                        .unwrap_or_default();
                    Some(MethodParam { type_handle, name })
                })
                .collect();
        }
        self.method_params.get(&handle).cloned().unwrap_or_default()
    }

    /// Records assembly `asm`'s display name (for `Assembly.FullName`).
    pub fn bind_assembly_name(&mut self, asm: u8, name: String) {
        self.assembly_names.insert(asm, name);
    }

    /// Assembly `asm`'s display name, if recorded.
    #[must_use]
    pub fn assembly_name(&self, asm: u8) -> Option<&str> {
        self.frozen
            .assembly_names
            .get(&self.arena, u64::from(asm))
            .and_then(|id| self.frozen.name_pool.str_at(&self.arena, id))
            .or_else(|| self.assembly_names.get(&asm).map(String::as_str))
    }

    /// Appends a declared type handle to assembly `asm`'s type list (for `Assembly.GetTypes`).
    pub fn add_assembly_type(&mut self, asm: u8, handle: u64) {
        self.assembly_types.entry(asm).or_default().push(handle);
    }

    /// Assembly `asm`'s declared type handles in declaration order (`Assembly.GetTypes`).
    #[must_use]
    pub fn assembly_types(&self, asm: u8) -> Vec<u64> {
        if let Some((offset, count)) = self.frozen.assembly_types.get(&self.arena, u64::from(asm)) {
            return (0..count as usize)
                .filter_map(|index| self.arena.read_u64(offset as usize + index * 8))
                .collect();
        }
        self.assembly_types.get(&asm).cloned().unwrap_or_default()
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
        self.frozen
            .member_type_handle
            .get(&self.arena, handle)
            .map(|(high, low)| (u64::from(high) << 32) | u64::from(low))
            .or_else(|| self.member_type_handle.get(&handle).copied())
    }

    /// Records a method's `MethodAttributes` flags (for `MethodBase.Is*`), keyed by its handle.
    pub fn bind_method_attrs(&mut self, handle: u64, attrs: u32) {
        self.method_attrs.insert(handle, attrs);
    }

    /// The `MethodAttributes` flags of the method whose asm-folded handle is `handle`, or `None`.
    #[must_use]
    pub fn method_attrs(&self, handle: u64) -> Option<u32> {
        self.frozen
            .method_attrs
            .get(&self.arena, handle)
            .or_else(|| self.method_attrs.get(&handle).copied())
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
        self.frozen
            .type_full_names
            .get(&self.arena, type_id)
            .and_then(|id| self.frozen.name_pool.str_at(&self.arena, id))
            .or_else(|| self.type_full_names.get(&type_id).map(String::as_str))
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
                    current = self.type_base(id);
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
        {
        let key = asm_key(asm, token.0);
        self.frozen.type_sizes.get(&self.arena, key).or_else(|| self.type_sizes.get(&key).copied())
    }
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

    /// The raw-body bytes of a one-`ret` method, in the shape [`Module::add_method`] takes under
    /// the active feature set: owned by default, leaked to `'static` under `flash-image` (a test's
    /// stand-in for a flash slice).
    fn one_ret_raw() -> RawCil {
        use lamella_cil::{Instruction, Opcode, write_method_body};
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: Vec::from([Instruction::simple(Opcode::Ret)]).into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let bytes = write_method_body(&body).expect("encode").into_boxed_slice();
        #[cfg(not(feature = "flash-image"))]
        {
            bytes
        }
        #[cfg(feature = "flash-image")]
        {
            Box::leak(bytes)
        }
    }

    #[test]
    fn array_prim_kinds_survive_the_freeze() {
        let mut module = Module::new();
        let byte_elem = Token(0x0100_0009);
        let long_elem = Token(0x0100_000A);
        module.bind_array_prim_kind(0, byte_elem, PrimKind::U1);
        module.bind_array_prim_kind(0, long_elem, PrimKind::I8);
        assert_eq!(module.array_prim_kind(0, byte_elem), Some(PrimKind::U1));

        module.freeze();

        let census: BTreeMap<&str, usize> = module.table_census().into_iter().collect();
        assert_eq!(census["array_prim_kinds"], 0, "freeze drains the builder map");
        assert_eq!(module.array_prim_kind(0, byte_elem), Some(PrimKind::U1));
        assert_eq!(module.array_prim_kind(0, long_elem), Some(PrimKind::I8));
        assert_eq!(module.array_prim_kind(0, Token(0x0100_0001)), None);
        assert_eq!(module.array_prim_kind(1, byte_elem), None, "unknown assembly");
    }

    #[test]
    fn frozen_token_lookups_use_the_dense_column_and_spill() {
        let mut module = Module::new();
        let first = module.add_method(0, one_ret_raw(), 0);
        let second = module.add_method(0, one_ret_raw(), 0);
        let external = module.add_method(1, one_ret_raw(), 0);
        let outlier = module.add_method(1, one_ret_raw(), 0);
        module.bind_token(0, Token::new(0x06, 1), first);
        module.bind_token(0, Token::new(0x06, 5), second);
        module.bind_token(0, Token::new(0x0A, 3), external);
        module.bind_token(0, Token::new(0x0B, 1_000_000), outlier);

        module.freeze();

        let census: BTreeMap<&str, usize> = module.table_census().into_iter().collect();
        assert_eq!(census["by_token"], 0, "freeze drains the builder map");
        assert_eq!(census["token spill entries (frozen)"], 1, "the sparse outlier spills");
        assert_eq!(module.resolve(0, Token::new(0x06, 1)), Some(first));
        assert_eq!(module.resolve(0, Token::new(0x06, 5)), Some(second));
        assert_eq!(
            module.resolve(0, Token::new(0x0A, 3)),
            Some(external),
            "a MemberRef row resolves via its own dense column"
        );
        assert_eq!(module.resolve(0, Token::new(0x0B, 1_000_000)), Some(outlier));
        assert_eq!(module.resolve(0, Token::new(0x06, 3)), None, "unbound row in range");
        assert_eq!(module.resolve(0, Token::new(0x06, 9)), None, "row past the column");
        assert_eq!(module.resolve(2, Token::new(0x06, 1)), None, "unknown assembly");

        let late = module.add_method(0, one_ret_raw(), 0);
        module.bind_token(0, Token::new(0x06, 3), late);
        assert_eq!(
            module.resolve(0, Token::new(0x06, 3)),
            Some(late),
            "a post-freeze bind (a REPL delta) resolves through the overlay map"
        );

        module.freeze();
        assert_eq!(
            module.resolve(0, Token::new(0x06, 3)),
            Some(late),
            "a second freeze folds the overlay into the frozen tables"
        );
        assert_eq!(module.resolve(0, Token::new(0x0A, 3)), Some(external), "spill carried over");
    }

    #[cfg(not(feature = "code-in-place"))]
    #[test]
    fn raw_bodies_decode_lazily_and_under_preload() {
        use lamella_cil::Opcode;
        let mut module = Module::new();
        let id = module.add_method(0, one_ret_raw(), 0);

        assert_eq!(module.preload_bodies(), 1, "one managed body decoded");
        let decoded = module.method_body(id).expect("a managed body");
        assert_eq!(decoded.code.len(), 1);
        assert_eq!(decoded.code[0].opcode, Opcode::Ret);
    }

    #[cfg(feature = "code-in-place")]
    #[test]
    fn a_baked_image_boots_and_resolves_like_the_module_it_came_from() {
        use lamella_cil::{Instruction, Opcode, Operand, write_method_body};
        let mut module = Module::new();
        let add_body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: Vec::from([
                Instruction::new(Opcode::Ldarg0, Operand::None),
                Instruction::new(Opcode::Ldarg1, Operand::None),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ])
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let raw: RawCil = Box::leak(write_method_body(&add_body).expect("encode").into_boxed_slice());
        let add = module.add_method(0, raw, 2);
        module.bind_token(0, Token::new(0x06, 1), add);
        let write_line = module.add_intrinsic(
            0,
            crate::intrinsics::console_write_line_empty,
            crate::intrinsic_registry::intrinsic_id("console_write_line_empty"),
            0,
        );
        module.bind_token(0, Token::new(0x0A, 1), write_line);
        module.bind_string(0, Token::new(0x70, 1), &[104, 105]);

        let image = module.write_baked(Some(add)).expect("bake");
        let leaked: &'static [u8] = Box::leak(image.into_boxed_slice());
        let (booted, entry) = Module::from_baked(leaked).expect("boot");

        assert_eq!(entry, Some(add));
        assert_eq!(booted.resolve(0, Token::new(0x06, 1)), Some(add));
        assert_eq!(booted.resolve(0, Token::new(0x0A, 1)), Some(write_line));
        assert_eq!(
            booted.resolve_string_le(0, Token::new(0x70, 1)),
            Some(&[104u8, 0, 105, 0][..]),
            "the ldstr pool survives the round trip"
        );
        assert_eq!(booted.arena.resident_len(), 0, "the arena is borrowed, zero RAM");
        let mut vm = Vm::new();
        let result = crate::interp::run(
            &booted,
            &mut vm,
            add,
            Vec::from([Value::Int32(40), Value::Int32(2)]),
        )
        .expect("run from the baked image");
        assert_eq!(result, Some(Value::Int32(42)));
    }

    #[cfg(feature = "code-in-place")]
    #[test]
    fn a_corrupt_baked_image_fails_the_checksum() {
        let mut module = Module::new();
        let id = module.add_method(0, one_ret_raw(), 0);
        let image = module.write_baked(Some(id)).expect("bake");

        assert!(baked_image_checksum(&image).is_some(), "a v2 image records a checksum");

        let clean: &'static [u8] = Box::leak(image.clone().into_boxed_slice());
        assert!(Module::from_baked(clean).is_ok(), "the clean image boots");

        let mut bytes = image;
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let corrupt: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        assert!(
            matches!(Module::from_baked(corrupt), Err(BakeError::ChecksumMismatch { .. })),
            "a corrupted image is rejected by the checksum, not run"
        );
    }

    #[cfg(feature = "code-in-place")]
    #[test]
    fn trim_demotes_an_unreachable_intrinsic_to_an_empty_body() {
        let mut module = Module::new();
        let reached = module.add_intrinsic(
            0,
            crate::intrinsics::console_write_line_empty,
            crate::intrinsic_registry::intrinsic_id("console_write_line_empty"),
            0,
        );
        let unreached = module.add_intrinsic(
            0,
            crate::intrinsics::array_clone,
            crate::intrinsic_registry::intrinsic_id("array_clone"),
            1,
        );

        let (methods, types, strings) = module.reachable_set(Some(reached));
        assert!(methods[reached as usize], "the entry intrinsic is reached");
        assert!(!methods[unreached as usize], "nothing reaches the other intrinsic");
        module.retain_reachable(&methods, &types, &strings);

        assert!(
            matches!(module.methods[reached as usize], Method::Intrinsic { .. }),
            "a reached intrinsic keeps its binding"
        );
        assert!(
            matches!(module.methods[unreached as usize], Method::Managed { .. }),
            "an unreached intrinsic demotes to an empty managed body (no baked demand)"
        );

        let image = module.write_baked(Some(reached)).expect("bake");
        let leaked: &'static [u8] = Box::leak(image.into_boxed_slice());
        let (_booted, entry) = Module::from_baked(leaked).expect("boot the trimmed bake");
        assert_eq!(entry, Some(reached));
    }

    #[cfg(feature = "code-in-place")]
    #[test]
    fn in_place_bodies_stay_raw_in_flash() {
        use lamella_cil::{Opcode, decode_at};
        let mut module = Module::new();
        let id = module.add_method(0, one_ret_raw(), 0);

        assert_eq!(module.preload_bodies(), 0, "nothing decodes under in_place");
        let code = module.method_code_bytes(id).expect("flash code bytes");
        let (instruction, next) = decode_at(code, 0).expect("decode the first instruction");
        assert_eq!(instruction.opcode, Opcode::Ret);
        assert_eq!(next, code.len());
        assert!(module.method_eh(id).expect("clauses").is_empty());
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
