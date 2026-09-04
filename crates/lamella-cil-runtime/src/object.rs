//! The managed heap and the reference-type object model.

#[cfg(feature = "gc")]
use crate::value::Location;
use crate::value::Value;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// A reference to a heap object: an index into the [`Heap`] arena. The null
/// reference is [`crate::value::Value::Null`], not an `ObjectRef`, so every
/// `ObjectRef` names a live object. The index is `pub(crate)` so the interpreter's
/// side tables keyed by heap position -- the exception-message map and the `Monitor`
/// lock table -- can read it to remap their keys when the compactor relocates objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef(pub(crate) u32);

impl ObjectRef {
    /// The heap slot index, as a debugger's display/correlation id (the Lamella Link
    /// `DBG_VARS` Object handle): two equal indices name the same object. Meaningful
    /// only while execution is halted -- a compacting collection may renumber slots --
    /// and never a pointer; on-device drill-down goes through the stateless
    /// slot/field-path selector, not this id.
    #[must_use]
    pub fn index(self) -> u32 {
        self.0
    }
}

/// The in-heap representation of a `System.String`'s code units, chosen by the string
/// storage encoding. UTF-16 by default (O(1) indexing, lone surrogates free); the
/// `string-utf8` feature switches to UTF-8 (~half the size for ASCII text, at O(n) UTF-16
/// indexing) and `string-utf8-wtf8` to WTF-8 (the same size, and a lone surrogate survives
/// too). Either way the [`Heap::as_string`] seam presents .NET UTF-16 semantics.
///
/// # Why three tiers and not two
///
/// The two byte tiers are not a refinement of each other, they trade opposite guarantees.
/// `string-utf8` guarantees the backing bytes are ALWAYS valid UTF-8, so a UTF-8-native
/// consumer (a socket write, an `LPUTF8Str` marshal) needs no validation pass -- and pays
/// for it by being unable to hold a lone surrogate. `string-utf8-wtf8` holds every String
/// the UTF-16 tier can and is byte-identical to UTF-8 for well-formed text, so egress is a
/// copy in the normal case -- but its bytes are not guaranteed valid UTF-8, so the consumer
/// needs the check the other tier makes unnecessary. Neither dominates; they are picked per
/// build, and `string-utf8-wtf8` wins when both are set (matching `lamella-aot`, so the
/// interpreter previews the tier an AOT build of the same profile would use).
#[cfg(not(any(feature = "string-utf8", feature = "string-utf8-wtf8")))]
type StrStore = Box<[u16]>;
#[cfg(any(feature = "string-utf8", feature = "string-utf8-wtf8"))]
type StrStore = Box<[u8]>;

/// A code unit the storage tier cannot hold, and where it sat: the encoder's refusal.
///
/// Only the well-formed UTF-8 tier ever produces one, and only for a lone surrogate. The two
/// fields are named for the `System.Text.EncoderFallbackException` properties they become --
/// `CharUnknown` and `Index` -- and `index` counts UTF-16 CODE UNITS, not scalars, which is
/// what .NET reports (a lone surrogate after one supplementary character is at index 2, not 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnencodableChar {
    /// The code unit that could not be encoded (a lone surrogate).
    pub char_unknown: u16,
    /// Its position in the input, counted in UTF-16 code units.
    pub index: u32,
}

/// Encodes UTF-16 code units into the backing [`StrStore`] (UTF-16: the units verbatim).
///
/// Infallible on this tier: `System.String`'s value space IS the input's, so there is nothing
/// to reject. The `Result` is the seam's shape, not this tier's -- see the `string-utf8` twin.
#[cfg(not(any(feature = "string-utf8", feature = "string-utf8-wtf8")))]
fn encode_string(units: &[u16]) -> Result<StrStore, UnencodableChar> {
    Ok(units.into())
}

/// Decodes little-endian UTF-16 bytes (the `ldstr` pool's on-disk shape) into code units.
fn decode_le_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Encodes UTF-16 code units into UTF-8 bytes, REFUSING a lone surrogate rather than losing it.
///
/// This tier's `System.String` has a strictly smaller value space than .NET's: a lone surrogate
/// has no well-formed UTF-8 form, so it cannot be stored. The refusal becomes
/// `System.Text.EncoderFallbackException` -- .NET's established spelling for "this character
/// could not be encoded into the target encoding", and an `ArgumentException` subclass, so a
/// `catch (ArgumentException)` written against desktop .NET still fires.
///
/// It throws rather than substituting U+FFFD even though `Encoding.UTF8`'s DEFAULT fallback
/// does substitute, because the operation here is not `GetBytes` -- it is String CONSTRUCTION,
/// and desktop .NET never loses data doing that. A silent U+FFFD would make the two byte tiers
/// differ by a wrong answer instead of by a raised error. `string-utf8-wtf8` is the tier that
/// does not have to choose.
#[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
fn encode_string(units: &[u16]) -> Result<StrStore, UnencodableChar> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4];
    let mut index = 0u32;
    for scalar in core::char::decode_utf16(units.iter().copied()) {
        match scalar {
            Ok(scalar) => {
                bytes.extend_from_slice(scalar.encode_utf8(&mut buf).as_bytes());
                index += scalar.len_utf16() as u32;
            }
            Err(unpaired) => {
                return Err(UnencodableChar {
                    char_unknown: unpaired.unpaired_surrogate(),
                    index,
                });
            }
        }
    }
    Ok(bytes.into_boxed_slice())
}

/// Decodes the backing [`StrStore`] to UTF-16 code units, .NET `String` semantics
/// (UTF-16: borrowed verbatim).
#[cfg(not(any(feature = "string-utf8", feature = "string-utf8-wtf8")))]
pub(crate) fn decode_string(store: &StrStore) -> Cow<'_, [u16]> {
    Cow::Borrowed(store)
}

/// Decodes UTF-8 bytes to UTF-16 code units (a supplementary scalar becomes a surrogate
/// pair, so `String.Length` stays the UTF-16 unit count).
#[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
pub(crate) fn decode_string(store: &StrStore) -> Cow<'_, [u16]> {
    let mut units = Vec::new();
    let mut buf = [0u16; 2];
    for scalar in core::str::from_utf8(store).unwrap_or("").chars() {
        units.extend_from_slice(scalar.encode_utf16(&mut buf));
    }
    Cow::Owned(units)
}

#[cfg(feature = "string-utf8-wtf8")]
use lamella_wtf8::{next_code_point, push_code_point};

/// Encodes UTF-16 code units into WTF-8 bytes.
///
/// A well-formed surrogate PAIR combines to its supplementary code point and takes FOUR bytes
/// -- that is what makes this WTF-8 and not CESU-8, and it is why the output is byte-identical
/// to UTF-8 for every well-formed string. Anything else, including a LONE surrogate, encodes as
/// its own code point: `U+D800` becomes `ED A0 80`, which is ill-formed UTF-8 on purpose and is
/// exactly the parity `string-utf8` cannot offer.
///
/// Infallible: this tier holds every String the UTF-16 tier can, so it never refuses one.
#[cfg(feature = "string-utf8-wtf8")]
fn encode_string(units: &[u16]) -> Result<StrStore, UnencodableChar> {
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let unit = u32::from(units[i]);
        let code = if (0xD800..=0xDBFF).contains(&unit)
            && i + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&u32::from(units[i + 1]))
        {
            let low = u32::from(units[i + 1]);
            i += 2;
            0x1_0000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
        } else {
            i += 1;
            unit
        };
        push_code_point(code, &mut bytes);
    }
    Ok(bytes.into_boxed_slice())
}

/// Decodes WTF-8 bytes to UTF-16 code units, the inverse of [`encode_string`].
///
/// `core::str::from_utf8` CANNOT be used here: it rejects the encoded-surrogate form this tier
/// exists to store, so routing through `&str` would turn every lone surrogate into an error and
/// lose precisely the data the tier preserves. A code point below `0x1_0000` -- surrogate range
/// included -- comes back as one unit; a supplementary one splits back into its pair, so
/// `String.Length` stays the UTF-16 unit count.
///
/// The store is produced by [`encode_string`], so its shape is an invariant rather than input:
/// a byte sequence that could not have come from there yields `U+FFFD` and resynchronizes,
/// which keeps a corrupted heap from being read as an unbounded loop or a panic.
#[cfg(feature = "string-utf8-wtf8")]
pub(crate) fn decode_string(store: &StrStore) -> Cow<'_, [u16]> {
    let mut units = Vec::new();
    let mut i = 0;
    while let Some((code, width)) = next_code_point(store, i) {
        i += width;
        if code < 0x1_0000 {
            units.push(code as u16);
        } else {
            let supplementary = code - 0x1_0000;
            units.push(0xD800 + (supplementary >> 10) as u16);
            units.push(0xDC00 + (supplementary & 0x3FF) as u16);
        }
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
    /// A single-dimensional, zero-based array (a vector): its element storage, each
    /// element initialized to the element type's zero value at allocation.
    Array {
        /// The element storage: packed bytes for a `newarr`'d primitive element type
        /// (one element per [`PrimKind`] byte width -- a `byte[512]` costs 512 bytes),
        /// boxed [`Value`] slots for everything else (references, value types, arrays
        /// minted without `newarr` element info).
        store: ArrayStorage,
        /// The element type's asm-folded token, captured from `newarr` -- what a covariant
        /// `stelem.ref` (I.8.7.1) checks the stored value against. `0` means "untracked" (an
        /// array minted without `newarr` info): such an array never raises
        /// `ArrayTypeMismatchException`, the pre-tracking behavior.
        element_type: u64,
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
    /// A weak cell: a single `target` reference the collector treats WEAKLY -- it is NOT traced
    /// (so the cell alone never keeps the target alive) and is cleared to `Null` when the target is
    /// reclaimed, or forwarded when the target survives. The managed `System.WeakReference` holds
    /// this cell by a STRONG reference, so the cell lives exactly as long as the `WeakReference`
    /// does, then is reclaimed normally -- no weak-handle leak. `target` is `Object` or `Null`.
    #[cfg(feature = "gc")]
    Weak {
        /// The weakly-held target (an `Object` reference or `Null`); the collector clears it to
        /// `Null` when the target is reclaimed, or forwards it when the target survives.
        target: Value,
    },
}

/// The primitive element kind of a packed array: which .NET element type family the packed
/// bytes encode, fixing the byte width, the integer sign-extension a whole-value read applies
/// (III.1.1.1 widens sub-`int32` elements to the stack `int32` by the ELEMENT type's
/// signedness), and the float reinterpretation. One kind covers every element type it is
/// observationally identical to: `U1` is `byte` and `bool`, `U2` is `ushort` and `char`,
/// `I4` is `int` and `uint` (a 32-bit element loads as its raw bits either way; signedness
/// above 16 bits is an operator property, not a load property), `I8` is `long` and `ulong`.
///
/// The discriminants are the frozen-image codes ([`crate::module::Module`] bakes the
/// `newarr`-token-to-kind table), so they are stable format values, not incidental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PrimKind {
    /// `sbyte`: 1 byte, sign-extended on read.
    I1 = 1,
    /// `byte` / `bool`: 1 byte, zero-extended on read.
    U1 = 2,
    /// `short`: 2 bytes little-endian, sign-extended on read.
    I2 = 3,
    /// `ushort` / `char`: 2 bytes little-endian, zero-extended on read.
    U2 = 4,
    /// `int` / `uint`: 4 bytes little-endian, read as the raw 32 bits.
    I4 = 5,
    /// `long` / `ulong`: 8 bytes little-endian, read as the raw 64 bits.
    I8 = 6,
    /// `float` (`System.Single`): 4 IEEE-754 bytes little-endian.
    F4 = 7,
    /// `double` (`System.Double`): 8 IEEE-754 bytes little-endian.
    F8 = 8,
}

impl PrimKind {
    /// The element's byte width in the packed image -- also the .NET element size
    /// `System.Buffer` measures byte ranges in, and the stride a pinned-array
    /// (`fixed`) pointer steps by.
    #[must_use]
    pub fn byte_width(self) -> usize {
        match self {
            PrimKind::I1 | PrimKind::U1 => 1,
            PrimKind::I2 | PrimKind::U2 => 2,
            PrimKind::I4 | PrimKind::F4 => 4,
            PrimKind::I8 | PrimKind::F8 => 8,
        }
    }

    /// The stable frozen-image code (the enum discriminant).
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The kind for a frozen-image code, `None` for an unknown code (a future image on an
    /// older runtime -- the array falls back to boxed storage rather than misdecoding).
    #[must_use]
    pub fn from_code(code: u8) -> Option<PrimKind> {
        Some(match code {
            1 => PrimKind::I1,
            2 => PrimKind::U1,
            3 => PrimKind::I2,
            4 => PrimKind::U2,
            5 => PrimKind::I4,
            6 => PrimKind::I8,
            7 => PrimKind::F4,
            8 => PrimKind::F8,
            _ => return None,
        })
    }

    /// Whether this build can pack elements of this kind: the float kinds need the `float`
    /// feature's [`Value::Single`]/[`Value::Float`] to read an element back, so a no-float
    /// build stores such arrays as boxed values instead (where they degrade exactly as
    /// floats otherwise do on that tier).
    #[must_use]
    pub fn packable(self) -> bool {
        match self {
            PrimKind::I1 | PrimKind::U1 | PrimKind::I2 | PrimKind::U2 | PrimKind::I4 | PrimKind::I8 => true,
            PrimKind::F4 | PrimKind::F8 => cfg!(feature = "float"),
        }
    }
}

/// How a single-dimensional array stores its elements.
///
/// `Values` is the general form: one boxed [`Value`] (16 bytes) per element, required for
/// reference and value-type elements. `Packed` is the primitive form `newarr` picks when the
/// element type is a sized primitive: the elements as their little-endian bytes at the
/// element type's true width -- so a `byte[512]` costs 512 bytes, not 8 KiB. Every element
/// access funnels through [`Heap::array_get`]/[`Heap::array_set`] (and the byte-view
/// accessors), which decode/encode per [`PrimKind`]; a packed array holds no object
/// references, so the collector neither traces nor rewrites its bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum ArrayStorage {
    /// One boxed [`Value`] slot per element.
    Values(Vec<Value>),
    /// The elements as little-endian bytes, `kind.byte_width()` bytes each.
    Packed {
        /// The element bytes, `len * kind.byte_width()` of them.
        bytes: Vec<u8>,
        /// The element kind: byte width + read/write encoding.
        kind: PrimKind,
    },
}

impl ArrayStorage {
    /// The element count.
    fn len(&self) -> usize {
        match self {
            ArrayStorage::Values(elements) => elements.len(),
            ArrayStorage::Packed { bytes, kind } => bytes.len() / kind.byte_width(),
        }
    }

    /// The element at `index` as its stack [`Value`] (III.1.1.1 widening for packed
    /// sub-`int32` kinds), or `None` out of bounds -- or for a packed float element in a
    /// no-float build, which has no stack value to widen to (such arrays are not packed
    /// in the first place; see [`PrimKind::packable`]).
    fn get(&self, index: usize) -> Option<Value> {
        match self {
            ArrayStorage::Values(elements) => elements.get(index).cloned(),
            ArrayStorage::Packed { bytes, kind } => {
                let width = kind.byte_width();
                let chunk = bytes.get(index.checked_mul(width)?..)?.get(..width)?;
                decode_packed(*kind, chunk)
            }
        }
    }

    /// Stores `value` at `index`. `false` out of bounds, or for a value a packed store
    /// cannot encode (a reference into a primitive image -- unreachable from verified IL).
    fn set(&mut self, index: usize, value: Value) -> bool {
        match self {
            ArrayStorage::Values(elements) => match elements.get_mut(index) {
                Some(slot) => {
                    *slot = value;
                    true
                }
                None => false,
            },
            ArrayStorage::Packed { bytes, kind } => {
                let width = kind.byte_width();
                let Some(start) = index.checked_mul(width) else {
                    return false;
                };
                let Some(chunk) = bytes.get_mut(start..).and_then(|tail| tail.get_mut(..width))
                else {
                    return false;
                };
                encode_packed(*kind, &value, chunk)
            }
        }
    }
}

/// The type-DEFAULT value with the same shape as `value` -- the zero a boxed array slot takes on
/// `Array.Clear`: `0` for a number, `null` for any reference, and a struct whose every field is
/// recursively zeroed for a value type. A managed pointer / typed reference has no default (it
/// cannot be an array element in verified IL), so it is left unchanged.
fn zeroed_value(value: &Value) -> Value {
    match value {
        Value::Int32(_) => Value::Int32(0),
        Value::Int64(_) => Value::Int64(0),
        Value::NativeInt(_) => Value::NativeInt(0),
        #[cfg(feature = "float")]
        Value::Float(_) => Value::Float(0.0),
        #[cfg(feature = "float")]
        Value::Single(_) => Value::Single(0.0),
        Value::Object(_) | Value::Null => Value::Null,
        Value::Struct(fields) => Value::Struct(fields.iter().map(zeroed_value).collect()),
        other => other.clone(),
    }
}

/// Decodes one packed element (`chunk` is exactly the kind's width) to its stack value:
/// sub-`int32` integers widen by the ELEMENT type's signedness (III.1.1.1), 32/64-bit
/// integers load their raw bits, floats reinterpret their IEEE bytes. The float kinds
/// yield `None` without the `float` feature (no stack value exists to decode to).
fn decode_packed(kind: PrimKind, chunk: &[u8]) -> Option<Value> {
    Some(match kind {
        PrimKind::I1 => Value::Int32(i32::from(chunk[0] as i8)),
        PrimKind::U1 => Value::Int32(i32::from(chunk[0])),
        PrimKind::I2 => Value::Int32(i32::from(i16::from_le_bytes([chunk[0], chunk[1]]))),
        PrimKind::U2 => Value::Int32(i32::from(u16::from_le_bytes([chunk[0], chunk[1]]))),
        PrimKind::I4 => Value::Int32(i32::from_le_bytes(chunk.try_into().ok()?)),
        PrimKind::I8 => Value::Int64(i64::from_le_bytes(chunk.try_into().ok()?)),
        #[cfg(feature = "float")]
        PrimKind::F4 => Value::Single(f32::from_le_bytes(chunk.try_into().ok()?)),
        #[cfg(feature = "float")]
        PrimKind::F8 => Value::Float(f64::from_le_bytes(chunk.try_into().ok()?)),
        #[cfg(not(feature = "float"))]
        PrimKind::F4 | PrimKind::F8 => return None,
    })
}

/// Encodes `value` into one packed element slot (`chunk` is exactly the kind's width):
/// integers truncate to the element width (the `stelem.i1`/`.i2` narrowing -- C# already
/// pre-`conv`ed, so this is the low bytes), floats round/widen between `Single` and
/// `Float` exactly as `stelem.r4`/`.r8` do. `false` for a value the kind cannot encode.
fn encode_packed(kind: PrimKind, value: &Value, chunk: &mut [u8]) -> bool {
    let int_bits = |value: &Value| -> Option<i64> {
        match value {
            Value::Int32(n) => Some(i64::from(*n)),
            Value::Int64(n) | Value::NativeInt(n) => Some(*n),
            _ => None,
        }
    };
    match kind {
        PrimKind::I1 | PrimKind::U1 | PrimKind::I2 | PrimKind::U2 | PrimKind::I4 | PrimKind::I8 => {
            let Some(bits) = int_bits(value) else {
                return false;
            };
            chunk.copy_from_slice(&bits.to_le_bytes()[..chunk.len()]);
            true
        }
        #[cfg(feature = "float")]
        PrimKind::F4 => {
            let single = match value {
                Value::Single(f) => *f,
                Value::Float(f) => *f as f32,
                _ => return false,
            };
            chunk.copy_from_slice(&single.to_le_bytes());
            true
        }
        #[cfg(feature = "float")]
        PrimKind::F8 => {
            let double = match value {
                Value::Float(f) => *f,
                Value::Single(f) => f64::from(*f),
                _ => return false,
            };
            chunk.copy_from_slice(&double.to_le_bytes());
            true
        }
        #[cfg(not(feature = "float"))]
        PrimKind::F4 | PrimKind::F8 => false,
    }
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
    /// The string intern table (III.4.16): character content -> the canonical
    /// `System.String`, so every `ldstr` of the same sequence yields the same object.
    /// Entries are strong roots for the collector and relocate with the survivors.
    intern: BTreeMap<Vec<u16>, ObjectRef>,
    /// The object count at which the next collection triggers (see
    /// [`Heap::should_collect`]); adapts after each collection.
    #[cfg(feature = "gc")]
    gc_threshold: usize,
    /// The identity hash of each object that has been ASKED for one, as `(object index, hash)`
    /// sorted by index -- the storage behind `Object.GetHashCode`.
    ///
    /// # Why it cannot be derived from the reference
    ///
    /// `Object.GetHashCode` answers a value that is stable for the object's LIFETIME and equal for
    /// two references to one object. An `ObjectRef` satisfies neither on its own: [`Heap::collect`]
    /// compacts, so every surviving object's index changes whenever a collection runs. The hash is
    /// therefore ASSIGNED once, on first request, and carried across collections with its object.
    ///
    /// # Lazy, and WEAK
    ///
    /// Entries appear only for objects somebody hashed, so a program that never hashes pays
    /// nothing. And a hashed object is NOT kept alive by having been hashed: the collector drops
    /// entries whose object did not survive, in the same pass that rewrites the survivors' indices.
    /// Treating these as strong roots -- the way the exception-message table deliberately does --
    /// would mean every object ever used as a dictionary key lived forever.
    ///
    identity_hashes: Vec<(u32, i32)>,
    /// The counter the next identity hash is mixed from. Not the hash itself: consecutive counter
    /// values would put every object in adjacent buckets, which is the distribution failure this
    /// whole change exists to remove.
    next_identity_hash: u32,
    /// A HARD ceiling on the live object count (`None` = unbounded, the host default). An
    /// embedder on a fixed heap sets it BELOW the real capacity (leaving headroom to allocate
    /// the OutOfMemoryException itself); when a collection cannot bring the count under it, the
    /// VES raises a catchable `Trap::OutOfMemory` rather than letting the allocator fail hard.
    object_budget: Option<usize>,
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
            intern: BTreeMap::new(),
            identity_hashes: Vec::new(),
            next_identity_hash: 0,
            gc_threshold: INITIAL_GC_THRESHOLD,
            object_budget: None,
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

    /// The identity hash of `reference`, assigning one on first request.
    ///
    /// Stable for the object's lifetime, equal for two references to one object, and unrelated to
    /// the object's current index -- see [`Heap::identity_hashes`] for why the index cannot be it.
    /// Never zero, so a stored hash is distinguishable from an empty slot by eye in a dump.
    pub fn identity_hash(&mut self, reference: ObjectRef) -> i32 {
        let index = reference.0;
        match self.identity_hashes.binary_search_by_key(&index, |&(key, _)| key) {
            Ok(at) => self.identity_hashes[at].1,
            Err(at) => {
                self.next_identity_hash = self.next_identity_hash.wrapping_add(1);
                let mut mixed = self.next_identity_hash;
                mixed ^= mixed >> 16;
                mixed = mixed.wrapping_mul(0x7feb_352d);
                mixed ^= mixed >> 15;
                mixed = mixed.wrapping_mul(0x846c_a68b);
                mixed ^= mixed >> 16;
                let hash = if mixed == 0 { 1 } else { mixed as i32 };
                self.identity_hashes.insert(at, (index, hash));
                hash
            }
        }
    }

    /// How many objects currently carry an assigned identity hash -- for tests that need to see the
    /// table shrink when hashed objects die, which is the property that keeps it from being a leak.
    #[must_use]
    pub fn identity_hash_count(&self) -> usize {
        self.identity_hashes.len()
    }

    /// Sets the HARD live-object budget (`None` = unbounded). See [`Heap::object_budget`].
    pub fn set_object_budget(&mut self, budget: Option<usize>) {
        self.object_budget = budget;
    }

    /// Whether the live object count has reached the configured budget (always false when
    /// unbounded). The VES tests this AFTER a collection to decide a catchable out-of-memory:
    /// a mark-compact collection shrinks `objects` to the survivors, so a post-GC reading is
    /// the live count.
    #[must_use]
    pub fn at_budget(&self) -> bool {
        self.object_budget
            .is_some_and(|budget| self.objects.len() >= budget)
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

    /// Allocates a FRESH `System.String` (a distinct object every call -- the shape of a
    /// computed string: concat, ToString, StringBuilder). The units are encoded into the
    /// heap's [`StrStore`] (the seam where UTF-8 / WTF-8 storage drops in). Literals go
    /// through [`Heap::intern_string`] instead.
    ///
    /// # Errors
    ///
    /// [`UnencodableChar`] when the tier cannot hold one of the units -- only the well-formed
    /// UTF-8 tier refuses, and only a lone surrogate. The caller turns it into the managed
    /// `System.Text.EncoderFallbackException`. Text of Rust provenance cannot hit this and
    /// should use [`Heap::alloc_text`], whose argument type rules it out.
    pub fn alloc_string(&mut self, chars: &[u16]) -> Result<ObjectRef, UnencodableChar> {
        Ok(self.alloc(Object::Str(encode_string(chars)?)))
    }

    /// Allocates a `System.String` from Rust text.
    ///
    /// Infallible BY THE ARGUMENT TYPE, on every tier: a `&str` is well-formed UTF-8, so its
    /// UTF-16 form contains no lone surrogate and no tier's encoder can refuse it. This is the
    /// path for text the RUNTIME produced -- a fault message, a type name, a formatted number
    /// -- as opposed to units of program provenance, which go through [`Heap::alloc_string`]
    /// and can be refused. It also skips the round trip through UTF-16 on the byte tiers.
    pub fn alloc_text(&mut self, text: &str) -> ObjectRef {
        #[cfg(not(any(feature = "string-utf8", feature = "string-utf8-wtf8")))]
        let store: StrStore = text.encode_utf16().collect::<Vec<u16>>().into_boxed_slice();
        #[cfg(any(feature = "string-utf8", feature = "string-utf8-wtf8"))]
        let store: StrStore = text.as_bytes().into();
        self.alloc(Object::Str(store))
    }

    /// Allocates a fresh string given as little-endian UTF-16 BYTES (the frozen `ldstr`
    /// pool's shape -- a flash slice needs no alignment to be read this way).
    ///
    /// # Errors
    ///
    /// As [`Heap::alloc_string`].
    pub fn alloc_string_le(&mut self, bytes: &[u8]) -> Result<ObjectRef, UnencodableChar> {
        let units = decode_le_units(bytes);
        self.alloc_string(&units)
    }

    /// The canonical (interned) `System.String` for `chars`, allocated on first sight:
    /// III.4.16 -- every `ldstr` of the same character sequence yields the SAME object,
    /// across modules. Interned strings stay live across collections (the table is a
    /// strong root) and relocate with the survivors.
    /// # Errors
    ///
    /// As [`Heap::alloc_string`] -- a literal the tier cannot hold is refused at its FIRST
    /// `ldstr` and never enters the pool, so the refusal does not depend on interning order.
    pub fn intern_string(&mut self, chars: &[u16]) -> Result<ObjectRef, UnencodableChar> {
        if let Some(&existing) = self.intern.get(chars) {
            return Ok(existing);
        }
        let reference = self.alloc_string(chars)?;
        self.intern.insert(chars.to_vec(), reference);
        Ok(reference)
    }

    /// [`Heap::intern_string`] over little-endian UTF-16 bytes (the `ldstr` pool's shape).
    ///
    /// # Errors
    ///
    /// As [`Heap::intern_string`].
    pub fn intern_string_le(&mut self, bytes: &[u8]) -> Result<ObjectRef, UnencodableChar> {
        let units = decode_le_units(bytes);
        self.intern_string(&units)
    }

    /// The canonical interned `System.String` for `chars` IF one already exists, without
    /// allocating -- `String.IsInterned`'s "is this content in the pool" query (vs
    /// [`Heap::intern_string`], which adds it).
    #[must_use]
    pub fn interned(&self, chars: &[u16]) -> Option<ObjectRef> {
        self.intern.get(chars).copied()
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
            #[cfg(feature = "gc")]
            Object::Weak { .. } => None,
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

    /// Allocates a weak cell holding `target` (an object reference or `Null`) and returns a
    /// reference to it. The collector treats the cell's target weakly (see [`Object::Weak`]); the
    /// managed `WeakReference` keeps this cell alive by a strong reference.
    #[cfg(feature = "gc")]
    pub fn alloc_weak(&mut self, target: Value) -> ObjectRef {
        self.alloc(Object::Weak { target })
    }

    /// The target of the weak cell at `reference` (`Null` once the target has been reclaimed), or
    /// `None` if `reference` is not a weak cell.
    #[cfg(feature = "gc")]
    #[must_use]
    pub fn weak_cell_target(&self, reference: ObjectRef) -> Option<Value> {
        match self.get(reference)? {
            Object::Weak { target } => Some(target.clone()),
            _ => None,
        }
    }

    /// Re-points the weak cell at `reference` to `target`. Returns `false` if it is not a weak cell.
    #[cfg(feature = "gc")]
    pub fn set_weak_cell_target(&mut self, reference: ObjectRef, target: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Weak { target: slot }) => {
                *slot = target;
                true
            }
            _ => false,
        }
    }

    /// The value of instance field `slot` at `reference`, if it is an instance with
    /// that slot.
    #[must_use]
    pub fn instance_field(&self, reference: ObjectRef, slot: u32) -> Option<Value> {
        match self.get(reference)? {
            Object::Instance { fields, .. } => fields.get(slot as usize).cloned(),
            #[cfg(feature = "gc")]
            Object::Weak { .. } => None,
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

    /// The type id of the instance at `reference`, if it is an instance -- the basis a virtual
    /// dispatch resolves through.
    #[must_use]
    pub fn type_of(&self, reference: ObjectRef) -> Option<u32> {
        match self.get(reference)? {
            Object::Instance { type_id, .. } => Some(*type_id),
            #[cfg(feature = "gc")]
            Object::Weak { .. } => None,
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

    /// Allocates a boxed-value array with the given `elements` (already filled with the
    /// element type's zero value) and returns a reference -- for an array minted without
    /// `newarr` element-type info (e.g. a `String.Split` result or an `ArrayList` backing
    /// store). A `newarr` of a sized primitive element instead packs
    /// ([`Heap::alloc_packed_array`]); of any other element, [`Heap::alloc_typed_array`]
    /// carries the covariance token.
    pub fn alloc_array(&mut self, elements: Vec<Value>) -> ObjectRef {
        self.alloc(Object::Array {
            store: ArrayStorage::Values(elements),
            element_type: 0,
        })
    }

    /// Allocates a boxed-value array tagged with its `newarr` element-type token (what a
    /// covariant `stelem.ref` checks against) -- the `newarr` path for a reference or
    /// value-type element.
    pub fn alloc_typed_array(&mut self, elements: Vec<Value>, element_type: u64) -> ObjectRef {
        self.alloc(Object::Array {
            store: ArrayStorage::Values(elements),
            element_type,
        })
    }

    /// Allocates a PACKED zero-initialized array of `length` elements of primitive `kind`,
    /// tagged with its `newarr` element-type token -- the `newarr` path for a sized primitive
    /// element. The whole point: the array costs `length * kind.byte_width()` bytes (a
    /// `byte[512]` is 512 bytes), and its zero fill IS the allocation.
    pub fn alloc_packed_array(&mut self, kind: PrimKind, length: usize, element_type: u64) -> ObjectRef {
        self.alloc(Object::Array {
            store: ArrayStorage::Packed {
                bytes: alloc::vec![0u8; length * kind.byte_width()],
                kind,
            },
            element_type,
        })
    }

    /// Allocates a packed array directly from its little-endian element `bytes` (which must
    /// be a whole number of `kind`-width elements) -- for an intrinsic producing a primitive
    /// array it already holds as raw bytes.
    pub fn alloc_packed_from_bytes(&mut self, kind: PrimKind, bytes: Vec<u8>, element_type: u64) -> ObjectRef {
        debug_assert_eq!(bytes.len() % kind.byte_width(), 0);
        self.alloc(Object::Array {
            store: ArrayStorage::Packed { bytes, kind },
            element_type,
        })
    }

    /// The asm-folded element-type token captured at `newarr` for the array at `reference`
    /// (`0` = untracked), or `None` if it is not a single-dimensional array.
    #[must_use]
    pub fn array_element_type(&self, reference: ObjectRef) -> Option<u64> {
        match self.get(reference)? {
            Object::Array { element_type, .. } => Some(*element_type),
            _ => None,
        }
    }

    /// Shallow-clones an array (single- or multi-dimensional) and returns a reference to
    /// the new array (`System.Array.Clone`). The copy is a fresh array of the same shape
    /// holding the SAME element values: a reference element is shared (the two arrays point
    /// at one object), a value-type element is copied bitwise -- exactly .NET's shallow
    /// `Array.Clone`. Returns `None` if `reference` is not an array.
    pub fn clone_array(&mut self, reference: ObjectRef) -> Option<ObjectRef> {
        let copy = match self.get(reference)? {
            Object::Array {
                store,
                element_type,
            } => Object::Array {
                store: store.clone(),
                element_type: *element_type,
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
            Object::Array { store, .. } => Some(store.len()),
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
            Object::Array { store, .. } if dim == 0 => i32::try_from(store.len()).ok(),
            _ => None,
        }
    }

    /// The rank (dimension count) of the array at `reference`: a single-dimension array is
    /// rank 1; a multi-dimensional array reports its dimension count.
    #[must_use]
    pub fn array_rank(&self, reference: ObjectRef) -> Option<i32> {
        match self.get(reference)? {
            Object::Array { .. } => Some(1),
            Object::MdArray { dims, .. } => i32::try_from(dims.len()).ok(),
            _ => None,
        }
    }

    /// The width in bytes of an element of the array at `reference` -- the stride a
    /// pinned-array (`fixed`) pointer's raw byte offset steps by. A packed array knows its
    /// true element width; a boxed-value array is read off its first element's storage
    /// width (every element of an array has the same width). `None` if `reference` is not
    /// an array, or an empty boxed one (no element to size, and a `fixed` pointer into it
    /// cannot be dereferenced anyway).
    #[must_use]
    pub fn array_element_width(&self, reference: ObjectRef) -> Option<usize> {
        let first = match self.get(reference)? {
            Object::Array {
                store: ArrayStorage::Packed { kind, .. },
                ..
            } => return Some(kind.byte_width()),
            Object::Array {
                store: ArrayStorage::Values(elements),
                ..
            } => elements.first(),
            Object::MdArray { elements, .. } => elements.first(),
            _ => return None,
        };
        first.map(value_storage_width)
    }

    /// The element type's true byte width of the array at `reference` -- the .NET element size
    /// `System.Buffer` measures byte offsets and lengths in: a `byte[]` is 1, a `short[]`/`char[]`
    /// 2, an `int[]` 4, a `long[]`/`double[]` 8. Exactly the packed arrays carry this (`newarr`
    /// packs every sized-primitive element type). `None` if `reference` is not an array, or is
    /// one whose element type is not a sized primitive (a reference / value-type array)
    /// -- the case `System.Buffer.BlockCopy` rejects with `ArgumentException`.
    #[must_use]
    pub fn array_element_byte_size(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::Array {
                store: ArrayStorage::Packed { kind, .. },
                ..
            } => Some(kind.byte_width()),
            _ => None,
        }
    }

    /// The element at `index` of the array at `reference`, if it is an array with
    /// that index in bounds. For a multi-dimensional array `index` is the row-major flat
    /// index (the form a `Location::Element` carries for a rectangular-array element pointer).
    #[must_use]
    pub fn array_get(&self, reference: ObjectRef, index: usize) -> Option<Value> {
        match self.get(reference)? {
            Object::Array { store, .. } => store.get(index),
            Object::MdArray { elements, .. } => elements.get(index).cloned(),
            _ => None,
        }
    }

    /// Stores `value` at `index` of the array at `reference`. Returns `false` if
    /// `reference` is not an array or `index` is out of range. For a multi-dimensional array
    /// `index` is the row-major flat index (matching `array_get`).
    pub fn array_set(&mut self, reference: ObjectRef, index: usize, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { store, .. }) => store.set(index, value),
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

    /// Zeros the `[index, index + length)` element range of the array at `reference` to each
    /// element's type DEFAULT (`Array.Clear`): a packed primitive array zeros its bytes; a boxed
    /// array sets each slot to the zero of its current value's shape (`0` for a number, `null` for
    /// a reference, a field-zeroed struct for a value type -- recursively). Handles reference AND
    /// value-type element arrays uniformly, unlike a `SetValue(null)` loop. `false` if `reference`
    /// is not a vector or the range is out of bounds (the managed wrapper bounds-checks first).
    pub fn clear_range(&mut self, reference: ObjectRef, index: usize, length: usize) -> bool {
        let end = match index.checked_add(length) {
            Some(end) => end,
            None => return false,
        };
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { store, .. }) => match store {
                ArrayStorage::Packed { bytes, kind } => {
                    let width = kind.byte_width();
                    match (index.checked_mul(width), end.checked_mul(width)) {
                        (Some(start), Some(stop)) => match bytes.get_mut(start..stop) {
                            Some(slice) => {
                                slice.fill(0);
                                true
                            }
                            None => false,
                        },
                        _ => false,
                    }
                }
                ArrayStorage::Values(elements) => {
                    if end > elements.len() {
                        return false;
                    }
                    for slot in &mut elements[index..end] {
                        *slot = zeroed_value(slot);
                    }
                    true
                }
            },
            _ => false,
        }
    }

    /// Moves `length` elements from `source[source_index]` to `destination[destination_index]`
    /// in their STORED representation (`System.Array.Copy`): a packed primitive range moves as one
    /// contiguous byte range, a boxed-slot range slot by slot. Overlapping ranges within a single
    /// array move as if through a temporary, so a self-copy in either direction is correct.
    ///
    /// Returns `false` having copied NOTHING when the pair cannot be moved verbatim: two different
    /// element representations (a widening copy, or a packed array against a boxed-slot one), a
    /// covariant store that needs a per-element type check, a non-array operand, or a range outside
    /// either array. `Array.Copy` answers a `false` by running its untyped element seam, which
    /// handles those cases one element at a time -- so declining is always safe, never lossy.
    ///
    /// Two primitive arrays must agree on their element kind EXACTLY. The same-width signed/unsigned
    /// interchange the CLR permits (`int[]` to `uint[]`) is bit-identical and would be sound to move,
    /// but distinguishing it from the same-width pairs that are NOT interchangeable (`int[]` against
    /// `float[]`, which must not be reinterpreted) needs the element-compatibility relation the
    /// interpreter holds, not the storage kind. Those pairs keep the untyped seam: correct, just not
    /// accelerated.
    pub fn copy_range(
        &mut self,
        source: ObjectRef,
        source_index: usize,
        destination: ObjectRef,
        destination_index: usize,
        length: usize,
    ) -> bool {
        let (Some(source_end), Some(destination_end)) = (
            source_index.checked_add(length),
            destination_index.checked_add(length),
        ) else {
            return false;
        };
        match (self.get(source), self.get(destination)) {
            (
                Some(Object::Array { store: from, .. }),
                Some(Object::Array { store: into, .. }),
            ) if source_end <= from.len() && destination_end <= into.len() => {}
            _ => return false,
        }
        if length == 0 {
            return true;
        }
        if source == destination {
            let Some(Object::Array { store, .. }) = self.objects.get_mut(source.0 as usize) else {
                return false;
            };
            match store {
                ArrayStorage::Packed { bytes, kind } => {
                    let width = kind.byte_width();
                    bytes.copy_within(
                        source_index * width..source_end * width,
                        destination_index * width,
                    );
                }
                ArrayStorage::Values(elements) => {
                    if destination_index > source_index {
                        for offset in (0..length).rev() {
                            let value = elements[source_index + offset].clone();
                            elements[destination_index + offset] = value;
                        }
                    } else {
                        for offset in 0..length {
                            let value = elements[source_index + offset].clone();
                            elements[destination_index + offset] = value;
                        }
                    }
                }
            }
            return true;
        }
        let source_slot = source.0 as usize;
        let destination_slot = destination.0 as usize;
        let (source_object, destination_object) = if source_slot < destination_slot {
            let (before, from_destination) = self.objects.split_at_mut(destination_slot);
            (&mut before[source_slot], &mut from_destination[0])
        } else {
            let (before, from_source) = self.objects.split_at_mut(source_slot);
            (&mut from_source[0], &mut before[destination_slot])
        };
        let (
            Object::Array {
                store: source_store,
                element_type: source_element,
            },
            Object::Array {
                store: destination_store,
                element_type: destination_element,
            },
        ) = (source_object, destination_object)
        else {
            return false;
        };
        match (source_store, destination_store) {
            (
                ArrayStorage::Packed {
                    bytes: source_bytes,
                    kind: source_kind,
                },
                ArrayStorage::Packed {
                    bytes: destination_bytes,
                    kind: destination_kind,
                },
            ) => {
                if source_kind != destination_kind {
                    return false;
                }
                let width = source_kind.byte_width();
                destination_bytes[destination_index * width..destination_end * width]
                    .copy_from_slice(&source_bytes[source_index * width..source_end * width]);
                true
            }
            (
                ArrayStorage::Values(source_elements),
                ArrayStorage::Values(destination_elements),
            ) => {
                if *destination_element != 0 && *destination_element != *source_element {
                    return false;
                }
                destination_elements[destination_index..destination_end]
                    .clone_from_slice(&source_elements[source_index..source_end]);
                true
            }
            _ => false,
        }
    }

    /// Appends `value` to the array-backed list at `reference` (`ArrayList.Add`), returning
    /// the new element's index, or `None` if `reference` is not array-backed.
    pub fn array_push(&mut self, reference: ObjectRef, value: Value) -> Option<usize> {
        match self.objects.get_mut(reference.0 as usize)? {
            Object::Array {
                store: ArrayStorage::Values(elements),
                ..
            } => {
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
            Some(Object::Array {
                store: ArrayStorage::Values(elements),
                ..
            }) => {
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
            Some(Object::Array {
                store: ArrayStorage::Values(elements),
                ..
            }) if index < elements.len() => {
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
            Some(Object::Array {
                store: ArrayStorage::Values(elements),
                ..
            }) if index <= elements.len() => {
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
    /// its packed byte image's length. `None` if `reference` is not a packed primitive array
    /// (a reference / value-type array, the case the managed `Buffer` rejects with
    /// `ArgumentException`).
    #[must_use]
    pub fn buffer_byte_length(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::Array {
                store: ArrayStorage::Packed { bytes, .. },
                ..
            } => Some(bytes.len()),
            _ => None,
        }
    }

    /// `System.Buffer.BlockCopy`: copies `count` bytes from byte `src_offset` of the `src`
    /// primitive array's packed little-endian image into byte `dst_offset` of the `dst`
    /// array's -- a bounds-checked memcpy, since a packed array IS its flat byte image
    /// (element `i` occupies bytes `i*width .. i*width + width`, little-endian, exactly
    /// .NET's layout on a little-endian host). Copying spans and splits elements as the byte
    /// ranges require (one `int` source element becomes four consecutive `byte` destination
    /// elements, least-significant byte first).
    ///
    /// Returns `false` -- the managed wrapper's `ArgumentException` / `ArgumentNullException` /
    /// `ArgumentOutOfRangeException` cases -- if either reference is not a packed primitive
    /// array or a byte range runs past its array's byte length.
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
        if src == dst {
            let Some(Object::Array {
                store: ArrayStorage::Packed { bytes, .. },
                ..
            }) = self.objects.get_mut(src.0 as usize)
            else {
                return false;
            };
            bytes.copy_within(src_offset..src_end, dst_offset);
            return true;
        }
        let staged: Vec<u8> = match self.get(src) {
            Some(Object::Array {
                store: ArrayStorage::Packed { bytes, .. },
                ..
            }) => bytes[src_offset..src_end].to_vec(),
            _ => return false,
        };
        let Some(Object::Array {
            store: ArrayStorage::Packed { bytes, .. },
            ..
        }) = self.objects.get_mut(dst.0 as usize)
        else {
            return false;
        };
        bytes[dst_offset..dst_end].copy_from_slice(&staged);
        true
    }

    /// A borrowed view of `count` elements of the packed BYTE array at `reference` starting
    /// at element `offset` -- the zero-copy read side of a bulk intrinsic (a socket send
    /// crosses its seam straight from the managed array). `None` if `reference` is not a
    /// packed 1-byte-element array or the range is out of bounds; the caller falls back to
    /// the element-at-a-time funnel (a boxed-value array).
    #[must_use]
    pub fn array_u8_slice(&self, reference: ObjectRef, offset: usize, count: usize) -> Option<&[u8]> {
        match self.get(reference)? {
            Object::Array {
                store: ArrayStorage::Packed { bytes, kind },
                ..
            } if kind.byte_width() == 1 => bytes.get(offset..offset.checked_add(count)?),
            _ => None,
        }
    }

    /// The mutable twin of [`Heap::array_u8_slice`] -- the zero-copy write side (a socket
    /// receive lands in the managed array with no staging buffer).
    pub fn array_u8_slice_mut(
        &mut self,
        reference: ObjectRef,
        offset: usize,
        count: usize,
    ) -> Option<&mut [u8]> {
        match self.objects.get_mut(reference.0 as usize)? {
            Object::Array {
                store: ArrayStorage::Packed { bytes, kind },
                ..
            } if kind.byte_width() == 1 => bytes.get_mut(offset..offset.checked_add(count)?),
            _ => None,
        }
    }

    /// Fills the packed array at `reference` from a raw little-endian element blob (the
    /// `RuntimeHelpers.InitializeArray` RVA initializer): a straight prefix memcpy, since the
    /// packed image and the blob share the layout. A short blob fills a prefix (matching the
    /// element-at-a-time funnel's behavior); a long one is truncated to the array. `false` if
    /// `reference` is not a packed array -- the caller then takes the per-element path.
    pub fn packed_init_from_blob(&mut self, reference: ObjectRef, data: &[u8]) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array {
                store: ArrayStorage::Packed { bytes, .. },
                ..
            }) => {
                let take = data.len().min(bytes.len());
                bytes[..take].copy_from_slice(&data[..take]);
                true
            }
            _ => false,
        }
    }

    /// The elements of the array at `reference` in `start..start + len` as UTF-16 code units
    /// (the `String(char[])` constructors' read): each element's low 16 bits. Handles both
    /// storages natively -- a packed `char[]`'s units are its byte pairs. `None` if
    /// `reference` is not a single-dimensional array or the range is out of bounds.
    #[must_use]
    pub fn array_utf16_units(
        &self,
        reference: ObjectRef,
        start: usize,
        len: usize,
    ) -> Option<Vec<u16>> {
        let Object::Array { store, .. } = self.get(reference)? else {
            return None;
        };
        let end = start.checked_add(len).filter(|&end| end <= store.len())?;
        Some(match store {
            ArrayStorage::Values(elements) => elements[start..end]
                .iter()
                .map(|unit| match unit {
                    Value::Int32(code) => *code as u16,
                    _ => 0,
                })
                .collect(),
            ArrayStorage::Packed { bytes, kind } => {
                let width = kind.byte_width();
                bytes[start * width..end * width]
                    .chunks_exact(width)
                    .map(|chunk| match width {
                        1 => u16::from(chunk[0]),
                        2 => u16::from_le_bytes([chunk[0], chunk[1]]),
                        _ => u16::from_le_bytes([chunk[0], chunk[1]]),
                    })
                    .collect()
            }
        })
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
        for reference in self.intern.values() {
            let index = reference.0 as usize;
            if index < count && !live[index] {
                live[index] = true;
                work.push(index);
            }
        }
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
        for reference in self.intern.values_mut() {
            if let Some(new_index) = remap[reference.0 as usize] {
                *reference = ObjectRef(new_index);
            }
        }
        self.identity_hashes.retain_mut(|(index, _)| match remap[*index as usize] {
            Some(moved) => {
                *index = moved;
                true
            }
            None => false,
        });
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
        Location::StringChar { string, .. } => visit(*string),
        Location::Nested { base, .. } => location_refs(base, visit),
        Location::Local { .. }
        | Location::Arg { .. }
        | Location::LocalBytes { .. }
        | Location::ArgBytes { .. }
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
        Object::Array {
            store: ArrayStorage::Packed { .. },
            ..
        } => {}
        Object::Array {
            store: ArrayStorage::Values(elements),
            ..
        } => elements.iter().for_each(|e| collect_refs(e, visit)),
        Object::MdArray { elements, .. } => elements.iter().for_each(|e| collect_refs(e, visit)),
        Object::Boxed { value, .. } => collect_refs(value, visit),
        Object::Delegate { invocations } => invocations
            .iter()
            .for_each(|(target, _)| collect_refs(target, visit)),
        Object::Weak { .. } => {}
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
        Location::StringChar { string, .. } => remap_ref(string, remap),
        Location::Nested { base, .. } => remap_location(base, remap),
        Location::Local { .. }
        | Location::Arg { .. }
        | Location::LocalBytes { .. }
        | Location::ArgBytes { .. }
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
        Object::Array {
            store: ArrayStorage::Packed { .. },
            ..
        } => {}
        Object::Array {
            store: ArrayStorage::Values(elements),
            ..
        } => elements.iter_mut().for_each(|e| remap_value(e, remap)),
        Object::MdArray { elements, .. } => elements.iter_mut().for_each(|e| remap_value(e, remap)),
        Object::Boxed { value, .. } => remap_value(value, remap),
        Object::Delegate { invocations } => invocations
            .iter_mut()
            .for_each(|(target, _)| remap_value(target, remap)),
        Object::Weak { target } => {
            let cleared = if let Value::Object(reference) = target {
                match remap.get(reference.0 as usize).copied().flatten() {
                    Some(new) => {
                        reference.0 = new;
                        false
                    }
                    None => true,
                }
            } else {
                false
            };
            if cleared {
                *target = Value::Null;
            }
        }
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
        let kept = heap.alloc_text("a");
        let _garbage = heap.alloc_text("x");
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

    #[test]
    fn interned_strings_survive_collection_and_relocate() {
        let mut heap = Heap::new();
        let _garbage = heap.alloc_text("x");
        let interned = heap.intern_string(&[b'i' as u16]).expect("ASCII");
        assert_eq!(heap.intern_string(&[b'i' as u16]), Ok(interned));
        assert_ne!(heap.alloc_text("i"), interned);

        heap.collect(|_visit| {});

        assert_eq!(heap.object_count(), 1);
        let relocated = heap.intern_string(&[b'i' as u16]).expect("ASCII");
        assert_eq!(heap.object_count(), 1);
        assert_eq!(heap.as_string(relocated).as_deref(), Some(&[b'i' as u16][..]));
    }

    #[test]
    fn weak_cell_does_not_keep_its_target_alive_and_is_cleared() {
        let mut heap = Heap::new();
        let target = heap.alloc_text("w");
        let cell = heap.alloc_weak(Value::Object(target));
        let mut roots = alloc::vec![Value::Object(cell)];
        heap.collect(|visit| roots.iter_mut().for_each(visit));

        assert_eq!(heap.object_count(), 1);
        let cell = match &roots[0] {
            Value::Object(reference) => *reference,
            other => panic!("root not an object: {other:?}"),
        };
        assert_eq!(heap.weak_cell_target(cell), Some(Value::Null));
    }

    #[test]
    fn weak_cell_forwards_a_target_that_survives_via_a_strong_root() {
        let mut heap = Heap::new();
        let _garbage = heap.alloc_text("x");
        let target = heap.alloc_text("k");
        let cell = heap.alloc_weak(Value::Object(target));
        let keeper = heap.alloc_instance(3, alloc::vec![Value::Object(target)]);
        let mut roots = alloc::vec![Value::Object(cell), Value::Object(keeper)];
        heap.collect(|visit| roots.iter_mut().for_each(visit));

        let cell = match &roots[0] {
            Value::Object(reference) => *reference,
            other => panic!("root not an object: {other:?}"),
        };
        let keeper = match &roots[1] {
            Value::Object(reference) => *reference,
            other => panic!("root not an object: {other:?}"),
        };
        let weak_target = match heap.weak_cell_target(cell).unwrap() {
            Value::Object(reference) => reference,
            other => panic!("weak target not forwarded to an object: {other:?}"),
        };
        let strong_target = match heap.instance_field(keeper, 0).unwrap() {
            Value::Object(reference) => reference,
            other => panic!("strong field not an object: {other:?}"),
        };
        assert_eq!(weak_target, strong_target);
        assert_eq!(heap.as_string(weak_target).as_deref(), Some(&[b'k' as u16][..]));
    }

    /// `fixed (char* p = s)` yields a [`Location::StringChar`], the OTHER managed pointer a
    /// `fixed` statement can produce, and it must be rooted and forwarded exactly as an element
    /// pointer is. Rooted through the pointer ALONE, so the test proves both halves: the string
    /// stays alive because the pointer names it, and the pointer reaches it afterwards.
    ///
    /// This is why the interpreter needs no `pinned` support: a managed pointer here carries its
    /// base object, so the compactor moves the string and the pointer follows. A tier whose
    /// `fixed` pointer is a RAW ADDRESS has no such option and must pin instead.
    #[test]
    fn pinned_string_pointer_roots_its_string_and_is_forwarded() {
        let mut heap = Heap::new();
        let _garbage = heap.alloc_text("x");
        let text = heap.alloc_text("hi");
        let mut roots = alloc::vec![Value::ByRef(Location::StringChar {
            string: text,
            byte_offset: 2,
        })];
        heap.collect(|visit| roots.iter_mut().for_each(visit));

        assert_eq!(heap.object_count(), 1);
        let Value::ByRef(Location::StringChar { string, byte_offset }) = roots[0] else {
            panic!("root not a string-char pointer: {:?}", roots[0]);
        };
        assert_eq!(byte_offset, 2, "the displacement is relative, so it does not move");
        assert_eq!(heap.as_string(string).as_deref(), Some(&[b'h' as u16, b'i' as u16][..]));
    }

    #[test]
    fn packed_array_relocates_with_bytes_intact_and_element_pointers_forwarded() {
        let mut heap = Heap::new();
        let _garbage = heap.alloc_text("x");
        let array = heap.alloc_packed_array(PrimKind::U1, 4, 0);
        assert!(heap.array_set(array, 2, Value::Int32(0xAB)));
        let mut roots = alloc::vec![Value::ByRef(Location::Element {
            array,
            index: 2,
            byte_offset: 0,
        })];
        heap.collect(|visit| roots.iter_mut().for_each(visit));

        assert_eq!(heap.object_count(), 1);
        let Value::ByRef(Location::Element { array, index, .. }) = roots[0] else {
            panic!("root not an element pointer: {:?}", roots[0]);
        };
        assert_eq!(heap.array_get(array, index), Some(Value::Int32(0xAB)));
        assert_eq!(heap.array_get(array, 0), Some(Value::Int32(0)));
        assert_eq!(heap.buffer_byte_length(array), Some(4));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WTF-8 byte forms, taken from the encoding rules rather than from the encoder under test --
    /// an expectation produced by the thing under test proves only that it is self-consistent.
    /// `U+D800` is the case the whole tier exists for: `1101 1000 0000 0000` laid into the
    /// three-byte frame `1110xxxx 10xxxxxx 10xxxxxx` gives `ED A0 80`, which is ill-formed
    /// UTF-8 and well-formed WTF-8.
    #[cfg(feature = "string-utf8-wtf8")]
    const WTF8_VECTORS: &[(&[u16], &[u8])] = &[
        (&[0x0041], &[0x41]),
        (&[0x00E9], &[0xC3, 0xA9]),
        (&[0x20AC], &[0xE2, 0x82, 0xAC]),
        (&[0xD83D, 0xDE00], &[0xF0, 0x9F, 0x98, 0x80]),
        (&[0xD800], &[0xED, 0xA0, 0x80]),
        (&[0xDFFF], &[0xED, 0xBF, 0xBF]),
    ];

    /// Encoding is byte-exact against the hand-derived vectors, in both directions.
    ///
    /// The supplementary row is the one that separates WTF-8 from CESU-8: a well-formed pair
    /// must combine into FOUR bytes, not encode as two three-byte surrogates. A CESU-8 encoder
    /// passes every other row here and fails that one.
    #[cfg(feature = "string-utf8-wtf8")]
    #[test]
    fn wtf8_encodes_and_decodes_the_spec_vectors() {
        for (units, bytes) in WTF8_VECTORS {
            let encoded = encode_string(units).expect("WTF-8 refuses nothing");
            assert_eq!(&encoded[..], *bytes, "encoding {units:04X?}");
            assert_eq!(&decode_string(&encoded)[..], *units, "decoding {bytes:02X?}");
        }
    }

    /// The property the tier is FOR: a lone surrogate survives storage. Under `string-utf8` this
    /// same construction is REFUSED, so this test is what distinguishes the two byte tiers rather
    /// than merely exercising one -- `well_formed_utf8_refuses_a_lone_surrogate` is its inverse,
    /// and the two together are why neither tier is a refinement of the other.
    #[cfg(feature = "string-utf8-wtf8")]
    #[test]
    fn wtf8_round_trips_a_lone_surrogate_through_the_heap() {
        let mut heap = Heap::new();
        let mixed = [0x0048u16, 0xD800, 0x0069];
        let reference = heap.alloc_string(&mixed).expect("WTF-8 refuses nothing");
        assert_eq!(heap.as_string(reference).as_deref(), Some(&mixed[..]));
    }

    /// A supplementary character occupies TWO UTF-16 units after the round trip, so
    /// `String.Length` keeps .NET's semantics on a tier that stores one code point.
    #[cfg(feature = "string-utf8-wtf8")]
    #[test]
    fn wtf8_keeps_utf16_length_semantics() {
        let mut heap = Heap::new();
        let reference = heap
            .alloc_string(&[0xD83D, 0xDE00])
            .expect("a well-formed pair is representable");
        assert_eq!(heap.as_string(reference).unwrap().len(), 2);
        assert_eq!(encode_string(&[0xD83D, 0xDE00]).unwrap().len(), 4);
    }

    /// A byte sequence [`encode_string`] could not have produced must not hang or panic the
    /// decoder -- a corrupted heap should read as replacement characters, not as a crash.
    #[cfg(feature = "string-utf8-wtf8")]
    #[test]
    fn wtf8_decoder_resynchronizes_on_bytes_it_could_not_have_written() {
        let store: StrStore = vec![0x80, 0x41, 0xE2, 0x82].into_boxed_slice();
        let units = decode_string(&store);
        assert_eq!(&units[..], &[0xFFFD, 0x0041, 0xFFFD]);
    }

    /// The well-formed tier's parity gap, PINNED rather than described in a comment: a lone
    /// surrogate cannot be stored here, so the construction is REFUSED rather than coming back as
    /// U+FFFD, which is what this test asserted; the replacement was silent data loss at String
    /// construction, and this is the assertion that changed when it stopped.
    ///
    /// The refusal reports the unit and its position, because those become the managed
    /// `EncoderFallbackException`'s `CharUnknown` and `Index`.
    #[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
    #[test]
    fn well_formed_utf8_refuses_a_lone_surrogate() {
        let mut heap = Heap::new();
        assert_eq!(
            heap.alloc_string(&[0x0048, 0xD800, 0x0069]),
            Err(UnencodableChar {
                char_unknown: 0xD800,
                index: 1,
            })
        );
        assert_eq!(heap.object_count(), 0);
    }

    /// The refusal's index counts UTF-16 CODE UNITS, not scalars: the lone surrogate here sits at
    /// unit 2 and scalar 1, because the supplementary character before it occupies two units.
    /// .NET reports 2 (measured against `Encoding.GetBytes` under `EncoderExceptionFallback`), and
    /// the two numbers only diverge once a supplementary character precedes the offending one --
    /// which is why the simpler case above cannot catch this being wrong.
    #[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
    #[test]
    fn the_refusal_index_counts_utf16_units_not_scalars() {
        let mut heap = Heap::new();
        assert_eq!(
            heap.alloc_string(&[0xD83D, 0xDE00, 0xD800]),
            Err(UnencodableChar {
                char_unknown: 0xD800,
                index: 2,
            })
        );
    }

    /// A lone LOW surrogate is refused too, and reports itself rather than the high half: the
    /// encoder has no pair to report and `CharUnknown` is the unit it actually met.
    #[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
    #[test]
    fn well_formed_utf8_refuses_a_lone_low_surrogate() {
        let mut heap = Heap::new();
        assert_eq!(
            heap.alloc_string(&[0xDFFF]),
            Err(UnencodableChar {
                char_unknown: 0xDFFF,
                index: 0,
            })
        );
    }

    /// The refusal is about the STORAGE, not about surrogates as such: a well-formed pair encodes
    /// to its four-byte code point and is accepted. Without this, "refuses a lone surrogate" would
    /// pass equally well for an encoder that refused every surrogate unit it saw.
    #[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
    #[test]
    fn well_formed_utf8_accepts_a_surrogate_pair() {
        let mut heap = Heap::new();
        let reference = heap
            .alloc_string(&[0x0048, 0xD83D, 0xDE00, 0x0069])
            .expect("a well-formed pair is representable");
        assert_eq!(
            heap.as_string(reference).as_deref(),
            Some(&[0x0048u16, 0xD83D, 0xDE00, 0x0069][..])
        );
    }

    #[test]
    fn allocated_objects_are_distinct_and_retrievable() {
        let mut heap = Heap::new();
        let a = heap.alloc_text("hi");
        let b = heap.alloc_text("yo");
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
    fn object_budget_reports_at_and_over_the_ceiling() {
        let mut heap = Heap::new();
        assert!(!heap.at_budget());
        heap.alloc_text("a");
        assert!(!heap.at_budget());
        heap.set_object_budget(Some(2));
        assert!(!heap.at_budget());
        heap.alloc_text("b");
        assert!(heap.at_budget());
        heap.alloc_text("c");
        assert!(heap.at_budget());
        heap.set_object_budget(None);
        assert!(!heap.at_budget());
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
        let text = heap.alloc_text("x");
        assert_eq!(heap.type_of(text), None);
        assert!(!heap.set_instance_field(text, 0, Value::Int32(1)));
    }

    #[test]
    fn packed_array_is_byte_sized_and_zero_initialized() {
        let mut heap = Heap::new();
        let array = heap.alloc_packed_array(PrimKind::U1, 512, 7);
        assert_eq!(heap.array_len(array), Some(512));
        assert_eq!(heap.buffer_byte_length(array), Some(512));
        assert_eq!(heap.array_element_byte_size(array), Some(1));
        assert_eq!(heap.array_element_width(array), Some(1));
        assert_eq!(heap.array_element_type(array), Some(7));
        assert_eq!(heap.array_get(array, 0), Some(Value::Int32(0)));
        assert_eq!(heap.array_get(array, 511), Some(Value::Int32(0)));
        assert_eq!(heap.array_get(array, 512), None);
        assert_eq!(heap.array_rank(array), Some(1));
        assert_eq!(heap.array_dimension(array, 0), Some(512));
    }

    #[test]
    fn copy_range_moves_a_packed_range_between_two_arrays() {
        let mut heap = Heap::new();
        let source = heap.alloc_packed_array(PrimKind::I4, 5, 7);
        let destination = heap.alloc_packed_array(PrimKind::I4, 5, 7);
        for index in 0..5 {
            assert!(heap.array_set(source, index, Value::Int32(10 * (index as i32 + 1))));
        }
        assert!(heap.copy_range(source, 1, destination, 0, 3));
        assert_eq!(heap.array_get(destination, 0), Some(Value::Int32(20)));
        assert_eq!(heap.array_get(destination, 1), Some(Value::Int32(30)));
        assert_eq!(heap.array_get(destination, 2), Some(Value::Int32(40)));
        assert_eq!(heap.array_get(destination, 3), Some(Value::Int32(0)));
        assert_eq!(heap.array_get(source, 1), Some(Value::Int32(20)));
    }

    #[test]
    fn copy_range_moves_an_overlapping_range_as_if_through_a_temporary() {
        let mut heap = Heap::new();
        let right = heap.alloc_packed_array(PrimKind::I4, 5, 7);
        for index in 0..5 {
            assert!(heap.array_set(right, index, Value::Int32(index as i32 + 1)));
        }
        assert!(heap.copy_range(right, 0, right, 1, 4));
        for (index, expected) in [1, 1, 2, 3, 4].iter().enumerate() {
            assert_eq!(heap.array_get(right, index), Some(Value::Int32(*expected)));
        }
        let left = heap.alloc_packed_array(PrimKind::I4, 5, 7);
        for index in 0..5 {
            assert!(heap.array_set(left, index, Value::Int32(index as i32 + 1)));
        }
        assert!(heap.copy_range(left, 1, left, 0, 4));
        for (index, expected) in [2, 3, 4, 5, 5].iter().enumerate() {
            assert_eq!(heap.array_get(left, index), Some(Value::Int32(*expected)));
        }
    }

    #[test]
    fn copy_range_declines_what_it_cannot_move_verbatim() {
        let mut heap = Heap::new();
        let ints = heap.alloc_packed_array(PrimKind::I4, 4, 7);
        let bytes = heap.alloc_packed_array(PrimKind::U1, 4, 8);
        let slots = heap.alloc_array(alloc::vec![
            Value::Int32(1),
            Value::Int32(2),
            Value::Int32(3),
            Value::Int32(4)
        ]);
        assert!(heap.array_set(ints, 0, Value::Int32(9)));
        assert!(!heap.copy_range(ints, 0, bytes, 0, 4));
        assert!(!heap.copy_range(bytes, 0, ints, 0, 4));
        assert!(!heap.copy_range(ints, 0, slots, 0, 4));
        assert!(!heap.copy_range(slots, 0, ints, 0, 4));
        assert_eq!(heap.array_get(bytes, 0), Some(Value::Int32(0)));
        assert_eq!(heap.array_get(slots, 0), Some(Value::Int32(1)));
    }

    #[test]
    fn copy_range_declines_a_range_past_either_end_without_copying() {
        let mut heap = Heap::new();
        let source = heap.alloc_packed_array(PrimKind::I4, 4, 7);
        let destination = heap.alloc_packed_array(PrimKind::I4, 4, 7);
        for index in 0..4 {
            assert!(heap.array_set(source, index, Value::Int32(index as i32 + 1)));
        }
        assert!(!heap.copy_range(source, 2, destination, 0, 3));
        assert!(!heap.copy_range(source, 0, destination, 2, 3));
        assert!(!heap.copy_range(source, 1, destination, 0, usize::MAX));
        assert_eq!(heap.array_get(destination, 0), Some(Value::Int32(0)));
        assert!(heap.copy_range(source, 0, destination, 0, 0));
        assert_eq!(heap.array_get(destination, 0), Some(Value::Int32(0)));
    }

    #[test]
    fn copy_range_moves_reference_slots_but_declines_a_covariant_destination() {
        let mut heap = Heap::new();
        let source = heap.alloc_typed_array(alloc::vec![Value::Int32(1), Value::Int32(2)], 42);
        let same_type = heap.alloc_typed_array(alloc::vec![Value::Null, Value::Null], 42);
        let other_type = heap.alloc_typed_array(alloc::vec![Value::Null, Value::Null], 99);
        let untracked = heap.alloc_array(alloc::vec![Value::Null, Value::Null]);
        assert!(heap.copy_range(source, 0, same_type, 0, 2));
        assert_eq!(heap.array_get(same_type, 1), Some(Value::Int32(2)));
        assert!(!heap.copy_range(source, 0, other_type, 0, 2));
        assert_eq!(heap.array_get(other_type, 0), Some(Value::Null));
        assert!(heap.copy_range(source, 0, untracked, 0, 2));
        assert_eq!(heap.array_get(untracked, 0), Some(Value::Int32(1)));
    }

    #[test]
    fn packed_reads_canonicalize_by_the_element_kind() {
        let mut heap = Heap::new();
        let bytes = heap.alloc_packed_array(PrimKind::U1, 2, 0);
        let sbytes = heap.alloc_packed_array(PrimKind::I1, 2, 0);
        assert!(heap.array_set(bytes, 0, Value::Int32(200)));
        assert!(heap.array_set(sbytes, 0, Value::Int32(200)));
        assert_eq!(heap.array_get(bytes, 0), Some(Value::Int32(200)));
        assert_eq!(heap.array_get(sbytes, 0), Some(Value::Int32(-56)));
        assert!(heap.array_set(bytes, 1, Value::Int32(0x1FF)));
        assert_eq!(heap.array_get(bytes, 1), Some(Value::Int32(0xFF)));

        let shorts = heap.alloc_packed_array(PrimKind::I2, 1, 0);
        let chars = heap.alloc_packed_array(PrimKind::U2, 1, 0);
        assert!(heap.array_set(shorts, 0, Value::Int32(0xFFFF)));
        assert!(heap.array_set(chars, 0, Value::Int32(0xFFFF)));
        assert_eq!(heap.array_get(shorts, 0), Some(Value::Int32(-1)));
        assert_eq!(heap.array_get(chars, 0), Some(Value::Int32(0xFFFF)));

        let longs = heap.alloc_packed_array(PrimKind::I8, 1, 0);
        assert!(heap.array_set(longs, 0, Value::Int64(-5_000_000_000)));
        assert_eq!(heap.array_get(longs, 0), Some(Value::Int64(-5_000_000_000)));

        assert!(!heap.array_set(bytes, 0, Value::Null));
    }

    #[cfg(feature = "float")]
    #[test]
    fn packed_float_elements_round_and_roundtrip() {
        let mut heap = Heap::new();
        let singles = heap.alloc_packed_array(PrimKind::F4, 1, 0);
        let doubles = heap.alloc_packed_array(PrimKind::F8, 1, 0);
        assert!(heap.array_set(singles, 0, Value::Float(1.5)));
        assert_eq!(heap.array_get(singles, 0), Some(Value::Single(1.5)));
        assert!(heap.array_set(doubles, 0, Value::Float(core::f64::consts::PI)));
        assert_eq!(
            heap.array_get(doubles, 0),
            Some(Value::Float(core::f64::consts::PI))
        );
    }

    #[test]
    fn buffer_block_copy_splices_packed_byte_images() {
        let mut heap = Heap::new();
        let ints = heap.alloc_packed_array(PrimKind::I4, 2, 0);
        assert!(heap.array_set(ints, 0, Value::Int32(0x0403_0201)));
        assert!(heap.array_set(ints, 1, Value::Int32(0x0807_0605)));
        let bytes = heap.alloc_packed_array(PrimKind::U1, 8, 0);
        assert!(heap.buffer_block_copy(ints, 0, bytes, 0, 8));
        for i in 0..8 {
            assert_eq!(heap.array_get(bytes, i), Some(Value::Int32(i as i32 + 1)));
        }
        assert!(heap.buffer_block_copy(bytes, 0, bytes, 2, 6));
        let spliced: Vec<i32> = (0..8)
            .map(|i| match heap.array_get(bytes, i) {
                Some(Value::Int32(byte)) => byte,
                other => panic!("byte {i} read {other:?}"),
            })
            .collect();
        assert_eq!(spliced, alloc::vec![1, 2, 1, 2, 3, 4, 5, 6]);
        assert!(!heap.buffer_block_copy(ints, 4, bytes, 0, 8));
        let plain = heap.alloc_array(alloc::vec![Value::Null]);
        assert!(!heap.buffer_block_copy(plain, 0, bytes, 0, 1));
    }

    #[test]
    fn u8_slice_views_open_only_on_packed_byte_arrays() {
        let mut heap = Heap::new();
        let bytes = heap.alloc_packed_array(PrimKind::U1, 4, 0);
        assert!(heap.array_set(bytes, 1, Value::Int32(0xAB)));
        assert_eq!(heap.array_u8_slice(bytes, 0, 4), Some(&[0, 0xAB, 0, 0][..]));
        assert_eq!(heap.array_u8_slice(bytes, 1, 2), Some(&[0xAB, 0][..]));
        assert_eq!(heap.array_u8_slice(bytes, 2, 3), None);
        if let Some(view) = heap.array_u8_slice_mut(bytes, 2, 2) {
            view.copy_from_slice(&[0xCD, 0xEF]);
        } else {
            panic!("mutable view refused");
        }
        assert_eq!(heap.array_get(bytes, 3), Some(Value::Int32(0xEF)));
        let ints = heap.alloc_packed_array(PrimKind::I4, 4, 0);
        assert_eq!(heap.array_u8_slice(ints, 0, 4), None);
        let plain = heap.alloc_array(alloc::vec![Value::Int32(1)]);
        assert_eq!(heap.array_u8_slice(plain, 0, 1), None);
    }

    #[test]
    fn packed_init_from_blob_fills_a_prefix() {
        let mut heap = Heap::new();
        let bytes = heap.alloc_packed_array(PrimKind::U1, 4, 0);
        assert!(heap.packed_init_from_blob(bytes, &[0xE4, 2]));
        assert_eq!(heap.array_get(bytes, 0), Some(Value::Int32(228)));
        assert_eq!(heap.array_get(bytes, 1), Some(Value::Int32(2)));
        assert_eq!(heap.array_get(bytes, 2), Some(Value::Int32(0)));
        let plain = heap.alloc_array(alloc::vec![Value::Int32(0)]);
        assert!(!heap.packed_init_from_blob(plain, &[1]));
    }

    #[test]
    fn utf16_units_read_from_either_storage() {
        let mut heap = Heap::new();
        let packed = heap.alloc_packed_array(PrimKind::U2, 3, 0);
        for (i, unit) in [b'h' as i32, b'i' as i32, b'!' as i32].into_iter().enumerate() {
            assert!(heap.array_set(packed, i, Value::Int32(unit)));
        }
        assert_eq!(
            heap.array_utf16_units(packed, 0, 3).as_deref(),
            Some(&[b'h' as u16, b'i' as u16, b'!' as u16][..])
        );
        assert_eq!(
            heap.array_utf16_units(packed, 1, 2).as_deref(),
            Some(&[b'i' as u16, b'!' as u16][..])
        );
        assert_eq!(heap.array_utf16_units(packed, 1, 3), None);
        let boxed = heap.alloc_array(alloc::vec![Value::Int32(b'x' as i32)]);
        assert_eq!(
            heap.array_utf16_units(boxed, 0, 1).as_deref(),
            Some(&[b'x' as u16][..])
        );
    }

    #[test]
    fn list_ops_reject_a_packed_array() {
        let mut heap = Heap::new();
        let packed = heap.alloc_packed_array(PrimKind::U1, 2, 0);
        assert_eq!(heap.array_push(packed, Value::Int32(1)), None);
        assert!(!heap.array_clear(packed));
        assert!(!heap.array_remove_at(packed, 0));
        assert!(!heap.array_insert(packed, 0, Value::Int32(1)));
        assert_eq!(heap.array_len(packed), Some(2));
    }
}

#[cfg(all(test, feature = "gc"))]
mod identity_hash_tests {
    use super::*;

    /// The contract's first clause: one object, one hash, however often it is asked.
    #[test]
    fn one_object_answers_one_hash() {
        let mut heap = Heap::new();
        let object = heap.alloc_instance(1, alloc::vec![Value::Int32(7)]);
        let first = heap.identity_hash(object);
        assert_eq!(heap.identity_hash(object), first, "asking twice must not reassign");
        assert_ne!(first, 0, "an assigned hash is never zero");
    }

    /// The clause the old constant broke: distinct objects get distinct hashes, so a table keyed by
    /// them does not collapse into one bucket.
    #[test]
    fn distinct_objects_get_distinct_hashes() {
        let mut heap = Heap::new();
        let hashes: Vec<i32> = (0..64)
            .map(|n| {
                let object = heap.alloc_instance(1, alloc::vec![Value::Int32(n)]);
                heap.identity_hash(object)
            })
            .collect();
        let mut unique = hashes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), hashes.len(), "64 objects produced a collision: {hashes:?}");
    }

    /// THE CASE THE REFERENCE ITSELF CANNOT SATISFY. A collection compacts, so a surviving object's
    /// index changes -- and its hash must not. This is the whole reason the value is stored.
    #[test]
    fn a_hash_survives_the_collection_that_moves_its_object() {
        let mut heap = Heap::new();
        let doomed = heap.alloc_instance(1, alloc::vec![Value::Int32(1)]);
        let mut kept = heap.alloc_instance(1, alloc::vec![Value::Int32(2)]);
        let _ = heap.identity_hash(doomed);
        let before = heap.identity_hash(kept);
        let index_before = kept.0;

        let mut root = Value::Object(kept);
        heap.collect(&mut |visit: &mut dyn FnMut(&mut Value)| visit(&mut root));
        let Value::Object(moved) = root else { panic!("the root stopped being an object") };
        kept = moved;

        assert_ne!(kept.0, index_before, "the survivor did not actually move -- test proves nothing");
        assert_eq!(heap.identity_hash(kept), before, "the hash moved with the object");
    }

    /// And the property that keeps the table from being a leak: hashing an object does not root it,
    /// so its entry goes away with it.
    #[test]
    fn a_dead_objects_hash_is_dropped() {
        let mut heap = Heap::new();
        let doomed = heap.alloc_instance(1, alloc::vec![Value::Int32(1)]);
        let kept = heap.alloc_instance(1, alloc::vec![Value::Int32(2)]);
        let _ = heap.identity_hash(doomed);
        let _ = heap.identity_hash(kept);
        assert_eq!(heap.identity_hash_count(), 2);

        let mut root = Value::Object(kept);
        heap.collect(&mut |visit: &mut dyn FnMut(&mut Value)| visit(&mut root));

        assert_eq!(
            heap.identity_hash_count(),
            1,
            "the unreachable object's hash entry outlived it, so hashing roots"
        );
    }

    /// The table is searched by binary search, so it has to stay sorted across a compaction. A
    /// lookup that silently missed would hand the same object a SECOND hash.
    #[test]
    fn the_table_stays_searchable_after_a_compaction() {
        let mut heap = Heap::new();
        let mut kept: Vec<ObjectRef> = Vec::new();
        for n in 0..8 {
            let object = heap.alloc_instance(1, alloc::vec![Value::Int32(n)]);
            if n % 2 == 0 {
                kept.push(object);
            }
        }
        let before: Vec<i32> = kept.iter().map(|&o| heap.identity_hash(o)).collect();

        let mut roots: Vec<Value> = kept.iter().map(|&o| Value::Object(o)).collect();
        heap.collect(&mut |visit: &mut dyn FnMut(&mut Value)| {
            for value in roots.iter_mut() {
                visit(value);
            }
        });

        for (slot, want) in roots.iter().zip(before.iter()) {
            let Value::Object(reference) = slot else { panic!("a root stopped being an object") };
            assert_eq!(heap.identity_hash(*reference), *want, "a survivor was rehashed");
        }
        assert_eq!(heap.identity_hash_count(), before.len(), "the table gained or lost entries");
    }
}
