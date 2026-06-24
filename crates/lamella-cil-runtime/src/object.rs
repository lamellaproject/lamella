//! The managed heap and the reference-type object model.

#[cfg(feature = "gc")]
use crate::value::Location;
use crate::value::Value;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// A reference to a heap object: an index into the [`Heap`] arena. The null
/// reference is [`crate::value::Value::Null`], not an `ObjectRef`, so every
/// `ObjectRef` names a live object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef(u32);

/// The in-heap representation of a `System.String`'s code units, chosen by the string
/// storage encoding. UTF-16 by default (O(1) indexing, lone surrogates free); the
/// `string-utf8` feature switches to UTF-8 (~half the size for ASCII text, at O(n) UTF-16
/// indexing). Either way the [`Heap::as_string`] seam presents .NET UTF-16 semantics. See
/// `docs/bcl-profiles-and-strings.md` (the surrogate-preserving WTF-8 tier is future).
#[cfg(not(feature = "string-utf8"))]
type StrStore = Box<[u16]>;
#[cfg(feature = "string-utf8")]
type StrStore = Box<[u8]>;

/// Encodes UTF-16 code units into the backing [`StrStore`] (UTF-16: the units verbatim).
#[cfg(not(feature = "string-utf8"))]
fn encode_string(units: &[u16]) -> StrStore {
    units.into()
}

/// Encodes UTF-16 code units into UTF-8 bytes; a lone surrogate (unrepresentable in
/// well-formed UTF-8) re-encodes to U+FFFD -- the well-formed tier's one parity gap.
#[cfg(feature = "string-utf8")]
fn encode_string(units: &[u16]) -> StrStore {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4];
    for scalar in core::char::decode_utf16(units.iter().copied()) {
        let scalar = scalar.unwrap_or(core::char::REPLACEMENT_CHARACTER);
        bytes.extend_from_slice(scalar.encode_utf8(&mut buf).as_bytes());
    }
    bytes.into_boxed_slice()
}

/// Decodes the backing [`StrStore`] to UTF-16 code units, .NET `String` semantics
/// (UTF-16: borrowed verbatim).
#[cfg(not(feature = "string-utf8"))]
pub(crate) fn decode_string(store: &StrStore) -> Cow<'_, [u16]> {
    Cow::Borrowed(store)
}

/// Decodes UTF-8 bytes to UTF-16 code units (a supplementary scalar becomes a surrogate
/// pair, so `String.Length` stays the UTF-16 unit count).
#[cfg(feature = "string-utf8")]
pub(crate) fn decode_string(store: &StrStore) -> Cow<'_, [u16]> {
    let mut units = Vec::new();
    let mut buf = [0u16; 2];
    for scalar in core::str::from_utf8(store).unwrap_or("").chars() {
        units.extend_from_slice(scalar.encode_utf16(&mut buf));
    }
    Cow::Owned(units)
}

/// A heap-allocated object: a `System.String` or an instance of a declared
/// reference type.
///
/// Strings are UTF-16 code units, matching `ldstr`'s `#US` heap and the lexer, so a
/// lone surrogate is representable. An instance carries its type id and its instance
/// fields. (`Eq` is not derived: a field may be a `Float`.)
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    /// A `System.String`, as its encoded code units ([`StrStore`]).
    Str(StrStore),
    /// An instance of a declared reference type: its type id (an index into the
    /// module's type table) and its instance fields in declaration-order slots.
    Instance {
        /// The declaring type, as a module type id.
        type_id: u32,
        /// The instance fields, one per slot (instance fields only, in declaration
        /// order), each initialized to its zero value at allocation.
        fields: Vec<Value>,
    },
    /// A single-dimensional, zero-based array (a vector): its elements, each
    /// initialized to the element type's zero value at allocation.
    Array {
        /// The elements, indexed 0..len.
        elements: Vec<Value>,
        /// The element type's true byte width (`Byte` = 1, `Int16`/`Char` = 2, `Int32` = 4,
        /// `Int64`/`Double` = 8, ...), captured from the `newarr` element type at allocation.
        /// This is the .NET element-type size `System.Buffer` measures byte ranges in -- distinct
        /// from the element's *storage* width on the stack (a `byte[]` and an `int[]` both store
        /// `Value::Int32`, but their `Buffer` widths are 1 and 4). `0` means "unknown": an array
        /// allocated without `newarr` element-type info (e.g. `String.Split`'s result, an
        /// `ArrayList` backing store), for which the storage width is the only available size.
        element_size: u8,
    },
    /// A multi-dimensional (rectangular) array (II.14.2): its per-dimension lengths and
    /// its elements in row-major order. Accessed via `Get`/`Set`/`.ctor` calls on the
    /// array type, not the `ldelem`/`stelem` opcodes.
    MdArray {
        /// The length of each dimension.
        dims: Box<[i32]>,
        /// The elements, row-major; one per product-of-dims slot.
        elements: Box<[Value]>,
    },
    /// A boxed value type (III.4.1 `box`): a heap copy of a value-type value, tagged
    /// with the value type's token so `unbox` / `unbox.any` and casts can recover it.
    Boxed {
        /// The boxed value type's asm-folded token (the `asm_key` of the `box` instruction's
        /// operand) -- the assembly in the high 32 bits, the metadata token in the low 32.
        type_token: u64,
        /// The boxed value (a copy of the value-type value).
        value: Value,
    },
    /// A delegate (II.14.6): an invocation list of bound methods -- each a `(target,
    /// method)` where the target is a `Value::Object` for an instance method or
    /// `Value::Null` for a static one. A single-cast delegate has one entry; `Combine`
    /// concatenates lists for multicast. `Invoke` calls them in order.
    Delegate {
        /// The bound methods, called in order; the last one's result is the delegate's.
        invocations: Vec<(Value, u32)>,
    },
    /// A `System.Text.StringBuilder`: a growable buffer of UTF-16 code units. It holds no
    /// object references, so the collector treats it like a string (nothing to trace).
    /// `capacity` tracks .NET's observable `Capacity` (>= the live length, default 16,
    /// doubling when an append outgrows it) independently of the backing `Vec`'s own
    /// allocation, so `get_Capacity` reports the .NET value rather than Rust's growth.
    StringBuilder {
        /// The accumulated UTF-16 code units (the live content; `buf.len()` is `Length`).
        buf: Vec<u16>,
        /// The observable `Capacity`: always `>= buf.len()`, 16 by default, doubled when an
        /// append outgrows it (.NET's growth rule), tracked apart from the `Vec`'s own.
        capacity: usize,
    },
}

/// The initial collection threshold (object count) before the live set is known; it
/// adapts upward to ~2x the survivors after each collection, so the working set settles.
#[cfg(feature = "gc")]
const INITIAL_GC_THRESHOLD: usize = 64;

/// The managed heap: an arena of [`Object`]s. With the `gc` feature, the collector
/// compacts it; without it, an append-only arena that leaks (the no-GC profile).
#[derive(Debug)]
#[cfg_attr(not(feature = "gc"), derive(Default))]
pub struct Heap {
    objects: Vec<Object>,
    /// The object count at which the next collection triggers (see
    /// [`Heap::should_collect`]); adapts after each collection.
    #[cfg(feature = "gc")]
    gc_threshold: usize,
    /// Parallel to `objects`: whether each object is registered for finalization (its
    /// type has a `Finalize` and it has not been finalized or suppressed). The collector
    /// promotes an unreachable registered object to the f-reachable set instead of
    /// reclaiming it.
    #[cfg(feature = "finalizers")]
    finalize_registered: Vec<bool>,
}

#[cfg(feature = "gc")]
impl Default for Heap {
    fn default() -> Heap {
        Heap {
            objects: Vec::new(),
            gc_threshold: INITIAL_GC_THRESHOLD,
            #[cfg(feature = "finalizers")]
            finalize_registered: Vec::new(),
        }
    }
}

impl Heap {
    /// Creates an empty heap.
    #[must_use]
    pub fn new() -> Heap {
        Heap::default()
    }

    /// Allocates `object` and returns a reference to it. With the `gc` feature an
    /// object unreachable from the roots is later reclaimed by [`Heap::collect`];
    /// without it the arena only grows.
    pub fn alloc(&mut self, object: Object) -> ObjectRef {
        let index = self.objects.len() as u32;
        self.objects.push(object);
        #[cfg(feature = "finalizers")]
        self.finalize_registered.push(false);
        ObjectRef(index)
    }

    /// Interns a UTF-16 string as a `System.String` and returns a reference. The units are
    /// encoded into the heap's [`StrStore`] (the seam where UTF-8 / WTF-8 storage drops in).
    pub fn alloc_string(&mut self, chars: &[u16]) -> ObjectRef {
        self.alloc(Object::Str(encode_string(chars)))
    }

    /// The object `reference` names, if it is live.
    #[must_use]
    pub fn get(&self, reference: ObjectRef) -> Option<&Object> {
        self.objects.get(reference.0 as usize)
    }

    /// The UTF-16 code units of the string at `reference`, if it is a string. Borrowed
    /// under UTF-16 storage; an owned decode under a UTF-8 / WTF-8 store.
    #[must_use]
    pub fn as_string(&self, reference: ObjectRef) -> Option<Cow<'_, [u16]>> {
        match self.get(reference)? {
            Object::Str(store) => Some(decode_string(store)),
            Object::Instance { .. }
            | Object::Array { .. }
            | Object::MdArray { .. }
            | Object::Boxed { .. }
            | Object::Delegate { .. }
            | Object::StringBuilder { .. } => None,
        }
    }

    /// Allocates an instance of `type_id` with initial `fields` and returns a
    /// reference. The caller supplies the zero-initialized field slots.
    pub fn alloc_instance(&mut self, type_id: u32, fields: Vec<Value>) -> ObjectRef {
        self.alloc(Object::Instance { type_id, fields })
    }

    /// The value of instance field `slot` at `reference`, if it is an instance with
    /// that slot.
    #[must_use]
    pub fn instance_field(&self, reference: ObjectRef, slot: u32) -> Option<Value> {
        match self.get(reference)? {
            Object::Instance { fields, .. } => fields.get(slot as usize).cloned(),
            Object::Str(_)
            | Object::Array { .. }
            | Object::MdArray { .. }
            | Object::Boxed { .. }
            | Object::Delegate { .. }
            | Object::StringBuilder { .. } => None,
        }
    }

    /// Stores `value` into instance field `slot` at `reference`. Returns `false` if
    /// `reference` is not an instance or `slot` is out of range.
    pub fn set_instance_field(&mut self, reference: ObjectRef, slot: u32, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Instance { fields, .. }) => match fields.get_mut(slot as usize) {
                Some(target) => {
                    *target = value;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// Grows the instance at `reference` by appending `additional` zero-default field slots,
    /// keeping every existing field value (including an `ObjectRef`, which stays valid because
    /// the object is grown in place on the SAME heap -- no relocation). Returns the new field
    /// count, or `None` if `reference` is not an instance.
    ///
    /// This is the runtime half of incremental-REPL instance growth: when a submission delta
    /// adds field(s) to the persistent `__Repl` type ([`crate::Module::add_type_field`]), the
    /// single live `__Repl` instance is grown to match by this call, so a later submission can
    /// read/write the new slot. Growing in place (rather than re-allocating a larger instance
    /// and copying) means a reference-typed prior field's `ObjectRef` is never disturbed -- the
    /// exact property that lets reference-typed REPL state survive across submissions.
    pub fn grow_instance(&mut self, reference: ObjectRef, additional: &[Value]) -> Option<usize> {
        match self.objects.get_mut(reference.0 as usize)? {
            Object::Instance { fields, .. } => {
                fields.extend_from_slice(additional);
                Some(fields.len())
            }
            _ => None,
        }
    }

    /// The type id of the instance at `reference`, if it is an instance (the basis
    /// for virtual dispatch once that lands).
    #[must_use]
    pub fn type_of(&self, reference: ObjectRef) -> Option<u32> {
        match self.get(reference)? {
            Object::Instance { type_id, .. } => Some(*type_id),
            Object::Str(_)
            | Object::Array { .. }
            | Object::MdArray { .. }
            | Object::Boxed { .. }
            | Object::Delegate { .. }
            | Object::StringBuilder { .. } => None,
        }
    }

    /// Whether the object at `reference` is a heap string -- the basis for supplying
    /// `System.String` as its runtime type so a `callvirt` reaches String's overrides.
    #[must_use]
    pub fn is_string(&self, reference: ObjectRef) -> bool {
        matches!(self.get(reference), Some(Object::Str(_)))
    }

    /// Allocates an array with the given `elements` (already filled with the element
    /// type's zero value) and returns a reference. The element type's byte width is left
    /// "unknown" (`0`) -- for an array minted without `newarr` element-type info (e.g. a
    /// `String.Split` result or an `ArrayList` backing store), where the element storage width
    /// is the only size available. A `newarr`'d primitive array instead uses
    /// [`Heap::alloc_array_sized`] so `System.Buffer` can measure its true byte length.
    pub fn alloc_array(&mut self, elements: Vec<Value>) -> ObjectRef {
        self.alloc(Object::Array {
            elements,
            element_size: 0,
        })
    }

    /// Allocates an array tagged with its element type's true byte width (`element_size`), the
    /// `newarr` path: a `byte[]` is 1, a `short[]`/`char[]` 2, an `int[]` 4, a `long[]`/`double[]`
    /// 8. `System.Buffer.BlockCopy` / `ByteLength` measure byte ranges in this width -- which a
    /// `byte[]` and an `int[]` (both storing `Value::Int32`) cannot share. `element_size == 0`
    /// means the element type is not a sized primitive (a reference / value-type array), recorded
    /// as such so `Buffer` rejects it.
    pub fn alloc_array_sized(&mut self, elements: Vec<Value>, element_size: u8) -> ObjectRef {
        self.alloc(Object::Array {
            elements,
            element_size,
        })
    }

    /// Shallow-clones an array (single- or multi-dimensional) and returns a reference to
    /// the new array (`System.Array.Clone`). The copy is a fresh array of the same shape
    /// holding the SAME element values: a reference element is shared (the two arrays point
    /// at one object), a value-type element is copied bitwise -- exactly .NET's shallow
    /// `Array.Clone`. Returns `None` if `reference` is not an array.
    pub fn clone_array(&mut self, reference: ObjectRef) -> Option<ObjectRef> {
        let copy = match self.get(reference)? {
            Object::Array {
                elements,
                element_size,
            } => Object::Array {
                elements: elements.clone(),
                element_size: *element_size,
            },
            Object::MdArray { dims, elements } => Object::MdArray {
                dims: dims.clone(),
                elements: elements.clone(),
            },
            _ => return None,
        };
        Some(self.alloc(copy))
    }

    /// Allocates a boxed value type tagged with the asm-folded `type_token` and returns a
    /// reference.
    pub fn alloc_boxed(&mut self, type_token: u64, value: Value) -> ObjectRef {
        self.alloc(Object::Boxed { type_token, value })
    }

    /// The value inside the box at `reference`, if it is a box.
    #[must_use]
    pub fn boxed_value(&self, reference: ObjectRef) -> Option<Value> {
        match self.get(reference)? {
            Object::Boxed { value, .. } => Some(value.clone()),
            _ => None,
        }
    }

    /// The value type's token a box is tagged with (the asm-folded token from the `box`
    /// site), if `reference` is a box -- the basis for a precise type test on a boxed value.
    #[must_use]
    pub fn boxed_type_token(&self, reference: ObjectRef) -> Option<u64> {
        match self.get(reference)? {
            Object::Boxed { type_token, .. } => Some(*type_token),
            _ => None,
        }
    }

    /// Stores `value` into the box at `reference` (for `unbox` + `stobj`/`stind`); returns
    /// `false` if `reference` is not a box.
    pub fn set_boxed_value(&mut self, reference: ObjectRef, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Boxed { value: slot, .. }) => {
                *slot = value;
                true
            }
            _ => false,
        }
    }

    /// Allocates a delegate binding `target` to `method` and returns a reference.
    pub fn alloc_delegate(&mut self, target: Value, method: u32) -> ObjectRef {
        self.alloc(Object::Delegate {
            invocations: alloc::vec![(target, method)],
        })
    }

    /// Allocates a (multicast) delegate with the given invocation list.
    pub fn alloc_multicast(&mut self, invocations: Vec<(Value, u32)>) -> ObjectRef {
        self.alloc(Object::Delegate { invocations })
    }

    /// Allocates a `System.Text.StringBuilder` seeded with `initial` code units and an
    /// observable `Capacity` of at least `capacity` (raised to the seed length so the
    /// invariant `capacity >= length` always holds, e.g. a long seed string).
    pub fn alloc_string_builder(&mut self, initial: Vec<u16>, capacity: usize) -> ObjectRef {
        let capacity = capacity.max(initial.len());
        self.alloc(Object::StringBuilder {
            buf: initial,
            capacity,
        })
    }

    /// The code units accumulated in the string builder at `reference`, if it is one.
    #[must_use]
    pub fn string_builder_buf(&self, reference: ObjectRef) -> Option<&[u16]> {
        match self.get(reference)? {
            Object::StringBuilder { buf, .. } => Some(buf),
            _ => None,
        }
    }

    /// Mutable access to the builder buffer at `reference`, if it is one (for `Append`).
    pub fn string_builder_buf_mut(&mut self, reference: ObjectRef) -> Option<&mut Vec<u16>> {
        match self.objects.get_mut(reference.0 as usize)? {
            Object::StringBuilder { buf, .. } => Some(buf),
            _ => None,
        }
    }

    /// The observable `Capacity` of the builder at `reference`, if it is one.
    #[must_use]
    pub fn string_builder_capacity(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::StringBuilder { capacity, .. } => Some(*capacity),
            _ => None,
        }
    }

    /// Raises the builder's tracked `Capacity` to cover `length`, by .NET's growth rule:
    /// when the new length outgrows the current capacity, the capacity becomes the larger
    /// of the needed length and twice the old capacity (so repeated appends double it).
    /// A length within the current capacity leaves it unchanged. Call after any op that
    /// extends the buffer (append / insert) so `get_Capacity` matches .NET.
    pub fn string_builder_grow_capacity(&mut self, reference: ObjectRef, length: usize) {
        if let Some(Object::StringBuilder { capacity, .. }) =
            self.objects.get_mut(reference.0 as usize)
        {
            if length > *capacity {
                *capacity = length.max(capacity.saturating_mul(2));
            }
        }
    }

    /// The invocation list of the delegate at `reference`, if it is a delegate.
    #[must_use]
    pub fn delegate_invocations(&self, reference: ObjectRef) -> Option<&[(Value, u32)]> {
        match self.get(reference)? {
            Object::Delegate { invocations } => Some(invocations),
            _ => None,
        }
    }

    /// The length of the array at `reference`, if it is an array.
    #[must_use]
    pub fn array_len(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::Array { elements, .. } => Some(elements.len()),
            Object::MdArray { elements, .. } => Some(elements.len()),
            _ => None,
        }
    }

    /// The length of dimension `dim` of the array at `reference` (a multi-dimensional
    /// array's per-dimension length, or a single-dimension array's length at `dim == 0`).
    #[must_use]
    pub fn array_dimension(&self, reference: ObjectRef, dim: i32) -> Option<i32> {
        match self.get(reference)? {
            Object::MdArray { dims, .. } => dims.get(usize::try_from(dim).ok()?).copied(),
            Object::Array { elements, .. } if dim == 0 => i32::try_from(elements.len()).ok(),
            _ => None,
        }
    }

    /// The width in bytes of an element of the array at `reference`, read off its first
    /// element (every element of an array has the same width). Used to translate a
    /// pinned-array pointer's raw byte offset back into an element index (a `fixed` pointer
    /// walks in bytes; the heap stores whole `Value` elements). `None` if `reference` is not
    /// an array, or an empty one (no element to size, and a `fixed` pointer into it cannot be
    /// dereferenced anyway).
    #[must_use]
    pub fn array_element_width(&self, reference: ObjectRef) -> Option<usize> {
        let first = match self.get(reference)? {
            Object::Array { elements, .. } => elements.first(),
            Object::MdArray { elements, .. } => elements.first(),
            _ => return None,
        };
        first.map(value_storage_width)
    }

    /// The element type's true byte width of the array at `reference` -- the .NET element size
    /// `System.Buffer` measures byte offsets and lengths in: a `byte[]` is 1, a `short[]`/`char[]`
    /// 2, an `int[]` 4, a `long[]`/`double[]` 8. Recorded from the `newarr` element type at
    /// allocation (a value the element's stack storage cannot recover, since a `byte[]` and an
    /// `int[]` both store `Value::Int32`). `None` if `reference` is not an array, or is one whose
    /// element type is not a sized primitive (a reference / value-type array, `element_size == 0`)
    /// -- the case `System.Buffer.BlockCopy` rejects with `ArgumentException`.
    #[must_use]
    pub fn array_element_byte_size(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::Array { element_size, .. } if *element_size != 0 => {
                Some(usize::from(*element_size))
            }
            _ => None,
        }
    }

    /// The element at `index` of the array at `reference`, if it is an array with
    /// that index in bounds. For a multi-dimensional array `index` is the row-major flat
    /// index (the form a `Location::Element` carries for a rectangular-array element pointer).
    #[must_use]
    pub fn array_get(&self, reference: ObjectRef, index: usize) -> Option<Value> {
        match self.get(reference)? {
            Object::Array { elements, .. } => elements.get(index).cloned(),
            Object::MdArray { elements, .. } => elements.get(index).cloned(),
            _ => None,
        }
    }

    /// Stores `value` at `index` of the array at `reference`. Returns `false` if
    /// `reference` is not an array or `index` is out of range. For a multi-dimensional array
    /// `index` is the row-major flat index (matching `array_get`).
    pub fn array_set(&mut self, reference: ObjectRef, index: usize, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { elements, .. }) => match elements.get_mut(index) {
                Some(slot) => {
                    *slot = value;
                    true
                }
                None => false,
            },
            Some(Object::MdArray { elements, .. }) => match elements.get_mut(index) {
                Some(slot) => {
                    *slot = value;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// Appends `value` to the array-backed list at `reference` (`ArrayList.Add`), returning
    /// the new element's index, or `None` if `reference` is not array-backed.
    pub fn array_push(&mut self, reference: ObjectRef, value: Value) -> Option<usize> {
        match self.objects.get_mut(reference.0 as usize)? {
            Object::Array { elements, .. } => {
                elements.push(value);
                Some(elements.len() - 1)
            }
            _ => None,
        }
    }

    /// Removes every element of the list at `reference` (`ArrayList.Clear`). Returns `false`
    /// if `reference` is not array-backed.
    pub fn array_clear(&mut self, reference: ObjectRef) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { elements, .. }) => {
                elements.clear();
                true
            }
            _ => false,
        }
    }

    /// Removes the element at `index` of the list at `reference` (`ArrayList.RemoveAt`).
    /// Returns `false` if not array-backed or `index` is out of range.
    pub fn array_remove_at(&mut self, reference: ObjectRef, index: usize) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { elements, .. }) if index < elements.len() => {
                elements.remove(index);
                true
            }
            _ => false,
        }
    }

    /// Inserts `value` before `index` of the list at `reference` (`ArrayList.Insert`).
    /// Returns `false` if not array-backed or `index` is past the end.
    pub fn array_insert(&mut self, reference: ObjectRef, index: usize, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { elements, .. }) if index <= elements.len() => {
                elements.insert(index, value);
                true
            }
            _ => false,
        }
    }

    /// Allocates a multi-dimensional array with the given per-dimension lengths (elements
    /// zero-initialized to int32 zero) and returns a reference.
    pub fn alloc_md_array(&mut self, dims: Vec<i32>) -> ObjectRef {
        let total: usize = dims.iter().map(|&d| d.max(0) as usize).product();
        let elements = alloc::vec![Value::Int32(0); total].into_boxed_slice();
        self.alloc(Object::MdArray {
            dims: dims.into_boxed_slice(),
            elements,
        })
    }

    /// The element at `indices` (row-major) of the multi-dimensional array at `reference`,
    /// if it is such an array with every index in bounds.
    #[must_use]
    pub fn md_array_get(&self, reference: ObjectRef, indices: &[i32]) -> Option<Value> {
        let Object::MdArray { dims, elements } = self.get(reference)? else {
            return None;
        };
        elements.get(md_flat_index(dims, indices)?).cloned()
    }

    /// Stores `value` at `indices` of the multi-dimensional array at `reference`; returns
    /// `false` if `reference` is not such an array or an index is out of range.
    pub fn md_array_set(&mut self, reference: ObjectRef, indices: &[i32], value: Value) -> bool {
        let Some(Object::MdArray { dims, elements }) = self.objects.get_mut(reference.0 as usize)
        else {
            return false;
        };
        let Some(flat) = md_flat_index(dims, indices) else {
            return false;
        };
        match elements.get_mut(flat) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        }
    }

    /// The row-major flat index of `indices` into the multi-dimensional array at `reference`
    /// (the index a `Location::Element` carries to address a rectangular-array element in
    /// place, for `Address`/`ref a[i,j]`), or `None` if `reference` is not such an array or
    /// an index is out of range. The same indices-to-flat computation `md_array_get` uses, so
    /// a read through the element pointer and a `Get` cannot disagree.
    #[must_use]
    pub fn md_array_flat_index(&self, reference: ObjectRef, indices: &[i32]) -> Option<usize> {
        let Object::MdArray { dims, .. } = self.get(reference)? else {
            return None;
        };
        md_flat_index(dims, indices)
    }

    /// The total byte length of the primitive array at `reference` (`System.Buffer.ByteLength`):
    /// element count times the element type's byte width. `None` if `reference` is not a
    /// single-dimensional primitive array (a reference / value-type array, the case the managed
    /// `Buffer` rejects with `ArgumentException`).
    #[must_use]
    pub fn buffer_byte_length(&self, reference: ObjectRef) -> Option<usize> {
        let width = self.array_element_byte_size(reference)?;
        let len = match self.get(reference)? {
            Object::Array { elements, .. } => elements.len(),
            _ => return None,
        };
        len.checked_mul(width)
    }

    /// `System.Buffer.BlockCopy`: copies `count` bytes from the `src` primitive array's flat
    /// little-endian byte image (starting at byte `src_offset`) into the `dst` primitive array's
    /// flat little-endian byte image (starting at byte `dst_offset`). Each array is viewed as a
    /// contiguous byte buffer in which element `i` occupies bytes `i*width .. i*width + width`,
    /// the element value encoded little-endian -- exactly .NET's layout on a little-endian host.
    /// Copying spans and splits elements as the byte ranges require (e.g. one `int` source element
    /// becomes four consecutive `byte` destination elements, least-significant byte first).
    ///
    /// Returns `false` -- the managed wrapper's `ArgumentException` / `ArgumentNullException` /
    /// `ArgumentOutOfRangeException` cases -- if either reference is not a sized primitive array,
    /// an offset/count is negative-derived (the caller passes already-validated `usize`s, so this
    /// is the bounds half), or a byte range runs past its array's byte length. On success the
    /// destination elements are re-decoded from their modified bytes and `true` is returned.
    pub fn buffer_block_copy(
        &mut self,
        src: ObjectRef,
        src_offset: usize,
        dst: ObjectRef,
        dst_offset: usize,
        count: usize,
    ) -> bool {
        let (Some(src_len), Some(dst_len)) = (
            self.buffer_byte_length(src),
            self.buffer_byte_length(dst),
        ) else {
            return false;
        };
        let (Some(src_end), Some(dst_end)) =
            (src_offset.checked_add(count), dst_offset.checked_add(count))
        else {
            return false;
        };
        if src_end > src_len || dst_end > dst_len {
            return false;
        }
        if count == 0 {
            return true;
        }
        let src_bytes = self.array_to_le_bytes(src);
        let mut dst_bytes = self.array_to_le_bytes(dst);
        dst_bytes[dst_offset..dst_end].copy_from_slice(&src_bytes[src_offset..src_end]);
        self.array_from_le_bytes(dst, &dst_bytes)
    }

    /// The flat little-endian byte image of the primitive array at `reference`: each element
    /// encoded at the element type's byte width, concatenated. The inverse of
    /// [`Heap::array_from_le_bytes`]. Assumes `reference` is a sized primitive array (the caller
    /// checked via [`Heap::buffer_byte_length`]); a non-array yields an empty image.
    fn array_to_le_bytes(&self, reference: ObjectRef) -> Vec<u8> {
        let Some(width) = self.array_element_byte_size(reference) else {
            return Vec::new();
        };
        let Some(Object::Array { elements, .. }) = self.get(reference) else {
            return Vec::new();
        };
        let mut bytes = Vec::with_capacity(elements.len() * width);
        for element in elements {
            encode_element_le(element, width, &mut bytes);
        }
        bytes
    }

    /// Rewrites the primitive array at `reference` from a flat little-endian byte image (the form
    /// [`Heap::array_to_le_bytes`] produces): each `width`-byte group decodes back to an element
    /// value of the array's element kind. Returns `false` if `reference` is not a sized primitive
    /// array, or `bytes` is not exactly the array's byte length.
    fn array_from_le_bytes(&mut self, reference: ObjectRef, bytes: &[u8]) -> bool {
        let Some(width) = self.array_element_byte_size(reference) else {
            return false;
        };
        let Some(Object::Array { elements, .. }) = self.objects.get_mut(reference.0 as usize) else {
            return false;
        };
        if bytes.len() != elements.len() * width {
            return false;
        }
        for (index, element) in elements.iter_mut().enumerate() {
            let chunk = &bytes[index * width..index * width + width];
            *element = decode_element_le(element, chunk);
        }
        true
    }
}

/// Appends the little-endian byte encoding of one primitive array element (`value`) at the
/// element type's byte `width` to `out`. The integer kinds emit their low `width` bytes; a
/// `width == 4` float emits the IEEE-754 single bits, a `width == 8` float the double bits --
/// matching how `System.Buffer` views a `float[]` (4-byte elements) versus a `double[]` (8).
fn encode_element_le(value: &Value, width: usize, out: &mut Vec<u8>) {
    match value {
        Value::Int32(n) => out.extend_from_slice(&(*n as u32).to_le_bytes()[..width.min(4)]),
        Value::Int64(n) | Value::NativeInt(n) => {
            out.extend_from_slice(&(*n as u64).to_le_bytes()[..width.min(8)]);
        }
        #[cfg(feature = "float")]
        Value::Single(f) => out.extend_from_slice(&f.to_le_bytes()[..width.min(4)]),
        #[cfg(feature = "float")]
        Value::Float(f) if width == 4 => out.extend_from_slice(&(*f as f32).to_le_bytes()),
        #[cfg(feature = "float")]
        Value::Float(f) => out.extend_from_slice(&f.to_le_bytes()[..width.min(8)]),
        _ => {}
    }
}

/// Decodes one element from its little-endian bytes back to the kind of the existing `current`
/// element (so the array's element kind is preserved). Integer kinds sign-extend from the byte
/// width -- matching the RVA array initializer (`read_le_int`), so the canonicalizing `ldelem.i1`
/// /`u1` read yields the same value either way; float kinds reinterpret the IEEE bits (4-byte
/// single widened to the stored `f64`, 8-byte double directly).
fn decode_element_le(current: &Value, bytes: &[u8]) -> Value {
    match current {
        Value::Int32(_) => Value::Int32(read_signed_le(bytes) as i32),
        Value::Int64(_) => Value::Int64(read_signed_le(bytes)),
        Value::NativeInt(_) => Value::NativeInt(read_signed_le(bytes)),
        #[cfg(feature = "float")]
        Value::Single(_) => {
            let mut buf = [0u8; 4];
            buf[..bytes.len().min(4)].copy_from_slice(&bytes[..bytes.len().min(4)]);
            Value::Single(f32::from_le_bytes(buf))
        }
        #[cfg(feature = "float")]
        Value::Float(_) if bytes.len() == 4 => {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(bytes);
            Value::Float(f64::from(f32::from_le_bytes(buf)))
        }
        #[cfg(feature = "float")]
        Value::Float(_) => {
            let mut buf = [0u8; 8];
            buf[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
            Value::Float(f64::from_le_bytes(buf))
        }
        other => other.clone(),
    }
}

/// A little-endian integer of `bytes.len()` (1, 2, 4, or 8) bytes, sign-extended to `i64` -- the
/// same convention the RVA initializer uses, so a negative sub-`int32` encoding round-trips.
fn read_signed_le(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    let take = bytes.len().min(8);
    buf[..take].copy_from_slice(&bytes[..take]);
    let unsigned = u64::from_le_bytes(buf);
    let bits = take * 8;
    if bits < 64 && unsigned & (1 << (bits - 1)) != 0 {
        (unsigned | (!0u64 << bits)) as i64
    } else {
        unsigned as i64
    }
}

/// The row-major flat index for `indices` in an array of shape `dims`, or `None` if the
/// ranks differ or any index is out of range.
fn md_flat_index(dims: &[i32], indices: &[i32]) -> Option<usize> {
    if indices.len() != dims.len() {
        return None;
    }
    let mut flat: usize = 0;
    for (&dim, &index) in dims.iter().zip(indices) {
        if index < 0 || index >= dim {
            return None;
        }
        flat = flat * (dim as usize) + (index as usize);
    }
    Some(flat)
}

/// The mark-compact garbage collector (the `gc` feature -- `surface.gc = collected`).
///
/// A precise, moving, stop-the-world collector over the index-based arena: it marks the
/// objects reachable from the roots, compacts the survivors (renumbering -- an
/// `ObjectRef` is an arena index), and rewrites every reference. `enumerate_roots` visits
/// each root value mutably; the caller exposes the interpreter frames, statics, and any
/// other root state. It is called twice -- once to mark, once to relocate.
#[cfg(feature = "gc")]
impl Heap {
    /// The number of objects currently in the arena.
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Whether the live arena has reached the collection threshold -- the trigger the
    /// interpreter checks at each instruction boundary to decide whether to collect.
    #[must_use]
    pub fn should_collect(&self) -> bool {
        self.objects.len() >= self.gc_threshold
    }

    /// Registers `reference` for finalization -- its type declares a `Finalize`, so the
    /// collector runs that finalizer when the object becomes unreachable. Also serves
    /// `ReRegisterForFinalize` (re-arming a suppressed or already-finalized object).
    #[cfg(feature = "finalizers")]
    pub fn register_finalizer(&mut self, reference: ObjectRef) {
        if let Some(slot) = self.finalize_registered.get_mut(reference.0 as usize) {
            *slot = true;
        }
    }

    /// Cancels finalization for `reference` (`GC.SuppressFinalize`).
    #[cfg(feature = "finalizers")]
    pub fn suppress_finalizer(&mut self, reference: ObjectRef) {
        if let Some(slot) = self.finalize_registered.get_mut(reference.0 as usize) {
            *slot = false;
        }
    }

    /// Reclaims every object unreachable from the roots and compacts the survivors,
    /// returning the objects promoted for finalization (the f-reachable set) at their new
    /// positions -- always empty without the `finalizers` feature.
    pub fn collect<R>(&mut self, mut enumerate_roots: R) -> Vec<ObjectRef>
    where
        R: FnMut(&mut dyn FnMut(&mut Value)),
    {
        let count = self.objects.len();
        let mut live = alloc::vec![false; count];
        let mut work: Vec<usize> = Vec::new();
        enumerate_roots(&mut |value| {
            collect_refs(value, &mut |reference| {
                let index = reference.0 as usize;
                if index < count && !live[index] {
                    live[index] = true;
                    work.push(index);
                }
            });
        });
        trace(&self.objects, &mut live, &mut work);

        #[cfg(feature = "finalizers")]
        let finalizable: Vec<usize> = {
            let mut promoted = Vec::new();
            #[allow(clippy::needless_range_loop)]
            for index in 0..count {
                if !live[index] && self.finalize_registered[index] {
                    live[index] = true;
                    self.finalize_registered[index] = false;
                    work.push(index);
                    promoted.push(index);
                }
            }
            trace(&self.objects, &mut live, &mut work);
            promoted
        };

        let mut remap: Vec<Option<u32>> = alloc::vec![None; count];
        let mut next = 0u32;
        for (slot, &alive) in remap.iter_mut().zip(live.iter()) {
            if alive {
                *slot = Some(next);
                next += 1;
            }
        }
        let old = core::mem::take(&mut self.objects);
        #[cfg(feature = "finalizers")]
        let old_registered = core::mem::take(&mut self.finalize_registered);
        for (index, mut object) in old.into_iter().enumerate() {
            if live[index] {
                remap_object(&mut object, &remap);
                self.objects.push(object);
                #[cfg(feature = "finalizers")]
                self.finalize_registered.push(old_registered[index]);
            }
        }
        enumerate_roots(&mut |value| remap_value(value, &remap));
        self.gc_threshold = self
            .objects
            .len()
            .saturating_mul(2)
            .max(INITIAL_GC_THRESHOLD);

        #[cfg(feature = "finalizers")]
        let result = finalizable
            .iter()
            .filter_map(|&index| remap[index].map(ObjectRef))
            .collect();
        #[cfg(not(feature = "finalizers"))]
        let result = Vec::new();
        result
    }
}

/// Drains the mark worklist: traces every reference out of each marked object, marking
/// and enqueuing newly reached objects. Shared by the root mark and the finalizer-promotion
/// re-trace.
#[cfg(feature = "gc")]
fn trace(objects: &[Object], live: &mut [bool], work: &mut Vec<usize>) {
    while let Some(index) = work.pop() {
        let mut children: Vec<usize> = Vec::new();
        object_refs(&objects[index], &mut |reference| {
            children.push(reference.0 as usize);
        });
        for child in children {
            if child < live.len() && !live[child] {
                live[child] = true;
                work.push(child);
            }
        }
    }
}

/// Visits each heap reference inside a managed-pointer location, recursing through a
/// nested value-type field address.
#[cfg(feature = "gc")]
fn location_refs<F: FnMut(ObjectRef)>(location: &Location, visit: &mut F) {
    match location {
        Location::Field { object, .. } | Location::Boxed { object } => visit(*object),
        Location::Element { array, .. } => visit(*array),
        Location::Nested { base, .. } => location_refs(base, visit),
        Location::Local { .. }
        | Location::Arg { .. }
        | Location::Static { .. }
        | Location::Stack { .. } => {}
    }
}

/// Visits each heap reference inside a value -- an object reference, a heap-pointing
/// byref, or (recursively) a value-type's fields.
#[cfg(feature = "gc")]
fn collect_refs<F: FnMut(ObjectRef)>(value: &Value, visit: &mut F) {
    match value {
        Value::Object(reference) => visit(*reference),
        Value::ByRef(location) => location_refs(location, visit),
        #[cfg(feature = "typed-references")]
        Value::TypedRef { location, .. } => location_refs(location, visit),
        Value::Struct(fields) => fields.iter().for_each(|field| collect_refs(field, visit)),
        _ => {}
    }
}

/// Visits each heap reference inside an object.
#[cfg(feature = "gc")]
fn object_refs<F: FnMut(ObjectRef)>(object: &Object, visit: &mut F) {
    match object {
        Object::Instance { fields, .. } => fields.iter().for_each(|f| collect_refs(f, visit)),
        Object::Array { elements, .. } => elements.iter().for_each(|e| collect_refs(e, visit)),
        Object::MdArray { elements, .. } => elements.iter().for_each(|e| collect_refs(e, visit)),
        Object::Boxed { value, .. } => collect_refs(value, visit),
        Object::Delegate { invocations } => invocations
            .iter()
            .for_each(|(target, _)| collect_refs(target, visit)),
        Object::Str(_) | Object::StringBuilder { .. } => {}
    }
}

/// Rewrites each heap reference inside a managed-pointer location, recursing through a
/// nested value-type field address.
#[cfg(feature = "gc")]
fn remap_location(location: &mut Location, remap: &[Option<u32>]) {
    match location {
        Location::Field { object, .. } | Location::Boxed { object } => remap_ref(object, remap),
        Location::Element { array, .. } => remap_ref(array, remap),
        Location::Nested { base, .. } => remap_location(base, remap),
        Location::Local { .. }
        | Location::Arg { .. }
        | Location::Static { .. }
        | Location::Stack { .. } => {}
    }
}

/// Rewrites each heap reference inside a value to its compacted position.
#[cfg(feature = "gc")]
fn remap_value(value: &mut Value, remap: &[Option<u32>]) {
    match value {
        Value::Object(reference) => remap_ref(reference, remap),
        Value::ByRef(location) => remap_location(location, remap),
        #[cfg(feature = "typed-references")]
        Value::TypedRef { location, .. } => remap_location(location, remap),
        Value::Struct(fields) => fields.iter_mut().for_each(|f| remap_value(f, remap)),
        _ => {}
    }
}

/// Rewrites each heap reference inside an object to its compacted position.
#[cfg(feature = "gc")]
fn remap_object(object: &mut Object, remap: &[Option<u32>]) {
    match object {
        Object::Instance { fields, .. } => fields.iter_mut().for_each(|f| remap_value(f, remap)),
        Object::Array { elements, .. } => elements.iter_mut().for_each(|e| remap_value(e, remap)),
        Object::MdArray { elements, .. } => elements.iter_mut().for_each(|e| remap_value(e, remap)),
        Object::Boxed { value, .. } => remap_value(value, remap),
        Object::Delegate { invocations } => invocations
            .iter_mut()
            .for_each(|(target, _)| remap_value(target, remap)),
        Object::Str(_) | Object::StringBuilder { .. } => {}
    }
}

/// Rewrites one reference via the compaction map (left unchanged if its target is gone).
#[cfg(feature = "gc")]
fn remap_ref(reference: &mut ObjectRef, remap: &[Option<u32>]) {
    if let Some(Some(new)) = remap.get(reference.0 as usize) {
        reference.0 = *new;
    }
}

/// The storage width in bytes of an array element holding this `Value` -- the element size
/// a pinned-array (`fixed`) pointer steps by. Element slots hold a reduced stack value, so
/// the width follows the value kind: a 32-bit element (the common `int[]` / `bool[]` /
/// `char[]` case, all widened to `Int32` on the stack) is 4 bytes, a 64-bit element 8, a
/// reference one host word. Sub-`int32` typed arrays are not laid out distinctly yet, so a
/// 32-bit slot is the right stride for the integer arrays csc pins here.
#[must_use]
fn value_storage_width(value: &Value) -> usize {
    match value {
        Value::Int32(_) => 4,
        Value::Int64(_) => 8,
        Value::NativeInt(_) => 8,
        #[cfg(feature = "float")]
        Value::Float(_) => 8,
        #[cfg(feature = "float")]
        Value::Single(_) => 4,
        _ => core::mem::size_of::<usize>(),
    }
}

#[cfg(all(test, feature = "gc"))]
mod gc_tests {
    use super::*;

    #[test]
    fn collect_reclaims_unreachable_and_relocates_live() {
        let mut heap = Heap::new();
        let kept = heap.alloc_string(&[b'a' as u16]);
        let _garbage = heap.alloc_string(&[b'x' as u16]);
        let root = heap.alloc_instance(7, alloc::vec![Value::Object(kept)]);
        assert_eq!(heap.object_count(), 3);

        let mut roots = alloc::vec![Value::Object(root)];
        heap.collect(|visit| roots.iter_mut().for_each(visit));

        assert_eq!(heap.object_count(), 2);
        let root = match &roots[0] {
            Value::Object(reference) => *reference,
            other => panic!("root not an object: {other:?}"),
        };
        let kept = match heap.instance_field(root, 0).unwrap() {
            Value::Object(reference) => reference,
            other => panic!("field not an object: {other:?}"),
        };
        assert_eq!(heap.as_string(kept).as_deref(), Some(&[b'a' as u16][..]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_objects_are_distinct_and_retrievable() {
        let mut heap = Heap::new();
        let a = heap.alloc_string(&[b'h' as u16, b'i' as u16]);
        let b = heap.alloc_string(&[b'y' as u16, b'o' as u16]);
        assert_ne!(a, b);
        assert_eq!(
            heap.as_string(a).as_deref(),
            Some(&[b'h' as u16, b'i' as u16][..])
        );
        assert_eq!(
            heap.as_string(b).as_deref(),
            Some(&[b'y' as u16, b'o' as u16][..])
        );
    }

    #[test]
    fn instances_carry_a_type_and_mutable_fields() {
        let mut heap = Heap::new();
        let object = heap.alloc_instance(7, alloc::vec![Value::Int32(0), Value::Null]);
        assert_eq!(heap.type_of(object), Some(7));
        assert_eq!(heap.instance_field(object, 0), Some(Value::Int32(0)));
        assert_eq!(heap.instance_field(object, 1), Some(Value::Null));
        assert!(heap.set_instance_field(object, 0, Value::Int32(42)));
        assert_eq!(heap.instance_field(object, 0), Some(Value::Int32(42)));
        assert!(!heap.set_instance_field(object, 9, Value::Int32(1)));
        let text = heap.alloc_string(&[b'x' as u16]);
        assert_eq!(heap.type_of(text), None);
        assert!(!heap.set_instance_field(text, 0, Value::Int32(1)));
    }
}
