//! The dynamic object model and its intrinsics.

use alloc::string::String;
use alloc::vec::Vec;

use core::cmp::Ordering;

use lamella_gc::{Heap, TypeDesc};
use lamella_py_bytecode::{BinOp, CmpOp};

use crate::trap::Trap;
use crate::value::Value;

/// The `str` method ids stored in a bound method's payload (Python 3.14.6 "String
/// Methods"). The set grows as methods are added.
const STR_UPPER: u32 = 0;
const STR_LOWER: u32 = 1;
const STR_STARTSWITH: u32 = 2;
const STR_ENDSWITH: u32 = 3;
const STR_FIND: u32 = 4;
const STR_STRIP: u32 = 5;
const STR_LSTRIP: u32 = 6;
const STR_RSTRIP: u32 = 7;
const STR_REPLACE: u32 = 8;
const STR_COUNT: u32 = 9;
const STR_ISDIGIT: u32 = 10;
const STR_ISALPHA: u32 = 11;
const STR_ISALNUM: u32 = 12;
const STR_ISSPACE: u32 = 13;
const STR_ISUPPER: u32 = 14;
const STR_ISLOWER: u32 = 15;

/// The id of the `str` method `name`, or `None` if `str` has no such method.
fn str_method_id(name: &str) -> Option<u32> {
    match name {
        "upper" => Some(STR_UPPER),
        "lower" => Some(STR_LOWER),
        "startswith" => Some(STR_STARTSWITH),
        "endswith" => Some(STR_ENDSWITH),
        "find" => Some(STR_FIND),
        "strip" => Some(STR_STRIP),
        "lstrip" => Some(STR_LSTRIP),
        "rstrip" => Some(STR_RSTRIP),
        "replace" => Some(STR_REPLACE),
        "count" => Some(STR_COUNT),
        "isdigit" => Some(STR_ISDIGIT),
        "isalpha" => Some(STR_ISALPHA),
        "isalnum" => Some(STR_ISALNUM),
        "isspace" => Some(STR_ISSPACE),
        "isupper" => Some(STR_ISUPPER),
        "islower" => Some(STR_ISLOWER),
        _ => None,
    }
}

/// Whether `s` satisfies a `str` predicate (`isdigit`/`isalpha`/`isalnum`/`isspace`/
/// `isupper`/`islower`, Python 3.14.6 "String Methods"). The category predicates require
/// at least one character; `isupper`/`islower` require at least one CASED character and
/// that every cased character has that case. Character classes use Rust's Unicode
/// classification, which matches CPython for ASCII and the common cases; exact agreement
/// on the Unicode-category edges (e.g. `Numeric_Type=Digit` superscripts, titlecase, the
/// bidirectional whitespace controls) is a Unicode-database refinement.
fn str_predicate(method_id: u32, s: &str) -> bool {
    match method_id {
        STR_ISDIGIT => !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()),
        STR_ISALPHA => !s.is_empty() && s.chars().all(char::is_alphabetic),
        STR_ISALNUM => !s.is_empty() && s.chars().all(char::is_alphanumeric),
        STR_ISSPACE => !s.is_empty() && s.chars().all(char::is_whitespace),
        STR_ISUPPER => {
            let mut cased = false;
            for c in s.chars() {
                if c.is_lowercase() {
                    return false;
                }
                cased |= c.is_uppercase();
            }
            cased
        }
        STR_ISLOWER => {
            let mut cased = false;
            for c in s.chars() {
                if c.is_uppercase() {
                    return false;
                }
                cased |= c.is_lowercase();
            }
            cased
        }
        _ => false,
    }
}

/// Parses a `(affix_or_sub[, start[, end]])` argument list for `startswith`/`endswith`/
/// `find`: the first argument is the str to match (checked by the caller); the optional
/// `start`/`end` are slice bounds -- an `int` (a slice index) or `None` for the default.
/// A wrong count, or a non-int / non-`None` bound, is a `TypeError`.
fn affix_and_bounds(args: &[Value]) -> Result<(Value, Option<i64>, Option<i64>), Trap> {
    fn bound(v: Value) -> Result<Option<i64>, Trap> {
        if v.is_none() {
            Ok(None)
        } else {
            Ok(Some(v.as_int().ok_or(Trap::TypeError)?))
        }
    }
    match args {
        [affix] => Ok((*affix, None, None)),
        [affix, start] => Ok((*affix, bound(*start)?, None)),
        [affix, start, end] => Ok((*affix, bound(*start)?, bound(*end)?)),
        _ => Err(Trap::TypeError),
    }
}

/// Normalizes Python slice bounds `[start:end]` over `len` code points: a negative bound
/// counts from the end (`+ len`), then both clamp to `[0, len]`; an absent bound defaults
/// to `0` (start) or `len` (end). The returned `(start, end)` may have `start > end`,
/// which denotes an empty range.
fn normalize_bounds(start: Option<i64>, end: Option<i64>, len: i64) -> (i64, i64) {
    fn norm(i: i64, len: i64) -> i64 {
        (if i < 0 { i + len } else { i }).clamp(0, len)
    }
    (
        start.map_or(0, |i| norm(i, len)),
        end.map_or(len, |i| norm(i, len)),
    )
}

/// The substring spanning code points `[a, b)` of `s` (empty if `a >= b`). Indexing is by
/// code point -- `s[a:b]` in Python terms, not a byte slice.
fn cp_slice(s: &str, a: i64, b: i64) -> &str {
    if a >= b {
        return "";
    }
    let byte = |cp: i64| s.char_indices().nth(cp as usize).map_or(s.len(), |(i, _)| i);
    &s[byte(a)..byte(b)]
}

/// Python slice-bound adjustment for `[start:stop:step]` over `len` code points
/// (`PySlice_Unpack` + `PySlice_AdjustIndices`, Python 3.14.6 `slice.indices`): a `None`
/// bound takes its default for the step direction, a negative bound counts from the end,
/// and an out-of-range bound CLAMPS. Returns the `(start, stop)` to iterate with `step`. A
/// non-int, non-`None` bound is a `TypeError`.
fn adjust_slice(start_v: Value, stop_v: Value, step: i64, len: i64) -> Result<(i64, i64), Trap> {
    let clamp = |bound: i64| {
        if bound < 0 {
            let shifted = bound + len;
            if shifted < 0 {
                if step < 0 {
                    -1
                } else {
                    0
                }
            } else {
                shifted
            }
        } else if bound >= len {
            if step < 0 {
                len - 1
            } else {
                len
            }
        } else {
            bound
        }
    };
    let start = if start_v.is_none() {
        if step < 0 {
            len - 1
        } else {
            0
        }
    } else {
        clamp(start_v.as_int().ok_or(Trap::TypeError)?)
    };
    let stop = if stop_v.is_none() {
        if step < 0 {
            -1
        } else {
            len
        }
    } else {
        clamp(stop_v.as_int().ok_or(Trap::TypeError)?)
    };
    Ok((start, stop))
}

/// A Python type's metadata: a name and a fixed set of named attribute
/// slots. One Python type corresponds to one GC type-descriptor id (its index in the
/// [`ObjectModel`]'s type table), so an instance's header word names both.
///
/// This object model has no inheritance, descriptors, or instance `__dict__`: attributes are
/// a small fixed set resolved to slot indices. The MRO walk and the descriptor protocol
/// arrive with the full object model.
#[derive(Debug, Clone)]
pub struct PyType {
    name: String,
    attrs: Vec<(String, u16)>,
    num_slots: u16,
}

impl PyType {
    /// A type whose attributes are `attr_names`, assigned slots `0, 1, 2, ...` in order.
    #[must_use]
    pub fn with_slots(name: &str, attr_names: &[&str]) -> PyType {
        let attrs = attr_names
            .iter()
            .enumerate()
            .map(|(i, n)| (String::from(*n), i as u16))
            .collect();
        PyType {
            name: String::from(name),
            attrs,
            num_slots: attr_names.len() as u16,
        }
    }

    /// The type's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The number of attribute slots an instance reserves.
    #[must_use]
    pub fn num_slots(&self) -> u16 {
        self.num_slots
    }

    /// The slot index of attribute `name`, or `None` if the type has no such attribute.
    /// A linear scan -- the attribute sets are tiny; the inline cache
    /// keeps it off the hot path anyway.
    #[must_use]
    pub fn slot_of(&self, name: &str) -> Option<u16> {
        self.attrs
            .iter()
            .find(|(attr, _)| attr == name)
            .map(|&(_, slot)| slot)
    }
}

/// One call site's inline cache for attribute access (PEP 659 style).
///
/// A `LoadAttr` site always loads the *same* attribute name, so the cache keys on the
/// receiver's type id alone: on a type match the resolved slot is reused and the name
/// lookup is skipped. The cache stores no reference, so it survives a moving collection
/// untouched (type ids and slot offsets are stable across compaction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineCache {
    type_id: u32,
    slot: u16,
    valid: bool,
}

impl Default for InlineCache {
    fn default() -> InlineCache {
        InlineCache::empty()
    }
}

impl InlineCache {
    /// A cold cache (no resolved type).
    #[must_use]
    pub const fn empty() -> InlineCache {
        InlineCache {
            type_id: 0,
            slot: 0,
            valid: false,
        }
    }

    /// The cached slot if `type_id` matches the last resolution (a cache *hit*), else
    /// `None` (a *miss*, which the caller resolves and records with [`InlineCache::fill`]).
    #[must_use]
    pub fn lookup(&self, type_id: u32) -> Option<u16> {
        if self.valid && self.type_id == type_id {
            Some(self.slot)
        } else {
            None
        }
    }

    /// Records a fresh resolution: a subsequent [`InlineCache::lookup`] of the same `type_id`
    /// will hit.
    pub fn fill(&mut self, type_id: u32, slot: u16) {
        self.type_id = type_id;
        self.slot = slot;
        self.valid = true;
    }
}

/// The dynamic object space: the shared heap plus the type table that gives each
/// heap object's header word a Python meaning.
///
/// The type table is indexed by GC type-descriptor id, so `heap.type_id_of(obj)` names
/// the [`PyType`] directly. The table is built once up front (the heap
/// needs its descriptors at construction); growing it dynamically (user-defined classes
/// at runtime) is a separate concern.
#[derive(Debug)]
pub struct ObjectModel {
    heap: Heap,
    types: Vec<PyType>,
    /// The runtime string arena: a `str`'s heap object holds an index into this, and the
    /// string bytes live here. The arena grows monotonically (strings are not reclaimed).
    strings: Vec<String>,
    /// The GC type-descriptor id of the `str` type; it follows the user types.
    str_type_id: u32,
    /// The GC type-descriptor id of a bound method (`str.method`); it follows `str`.
    bound_method_type_id: u32,
    /// The GC type-descriptor id of a `slice(start, stop, step)`; it follows the bound method.
    slice_type_id: u32,
}

impl ObjectModel {
    /// Builds an object space for `types`, with a heap of `heap_capacity` bytes. Each
    /// type's GC descriptor reserves `num_slots` tagged-value words and lists no bare
    /// reference fields (the slots are traced by tag -- see the module note). The `str`
    /// type is appended after them.
    #[must_use]
    pub fn new(types: Vec<PyType>, heap_capacity: usize) -> ObjectModel {
        let mut descs: Vec<TypeDesc> = types
            .iter()
            .map(|t| TypeDesc {
                payload_size: u32::from(t.num_slots) * 4,
                ref_offsets: Vec::new(),
            })
            .collect();
        let str_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
        });
        let bound_method_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
        });
        let slice_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
        });
        ObjectModel {
            heap: Heap::new(heap_capacity, descs),
            types,
            strings: Vec::new(),
            str_type_id,
            bound_method_type_id,
            slice_type_id,
        }
    }

    /// Allocates a `str` from `s`, returning a heap-pointer Value. The content is interned
    /// in the string arena and the heap object holds its index.
    pub fn new_str(&mut self, s: &str) -> Result<Value, Trap> {
        let index = self.strings.len() as u32;
        self.strings.push(String::from(s));
        let reference = self.heap.alloc(self.str_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// The string content if `value` is a `str`, else `None`.
    #[must_use]
    pub fn str_value(&self, value: Value) -> Option<&str> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.str_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.strings.get(index).map(String::as_str)
    }

    /// `len(value)` -- the built-in length. Handles `str` (its number of
    /// Unicode code points, per Python `len(str)`); containers and the `__len__` protocol
    /// arrive with the full object model. A value with no length is a `TypeError`.
    pub fn py_len(&self, value: Value) -> Result<Value, Trap> {
        if let Some(s) = self.str_value(value) {
            return Value::fixnum(s.chars().count() as i32).ok_or(Trap::Overflow);
        }
        Err(Trap::TypeError)
    }

    /// Whether `value` is a `str`.
    #[must_use]
    pub fn is_str(&self, value: Value) -> bool {
        self.str_value(value).is_some()
    }

    /// The dynamic binary-op dispatch (`py_binop`) for object operands -- currently `str`.
    /// `str + str` concatenates; any other operator, or a `str` mixed with a non-`str`,
    /// is a `TypeError` (Python: `"a" + 1` and `"a" - "b"` raise; `str * int` repetition
    /// is unsupported). Returns `Ok(None)` when NEITHER operand is an object, so the
    /// caller falls back to the numeric path -- the one-source-of-truth dispatch both the
    /// interpreter and the AOT `py_binop` intrinsic consume.
    pub fn py_binary(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Option<Value>, Trap> {
        let a = self.str_value(lhs).map(String::from);
        let b = self.str_value(rhs).map(String::from);
        match (a, b) {
            (None, None) => Ok(None),
            (Some(a), Some(b)) if op == BinOp::Add => {
                let mut s = a;
                s.push_str(&b);
                Ok(Some(self.new_str(&s)?))
            }
            _ => Err(Trap::TypeError),
        }
    }

    /// The dynamic comparison dispatch (`py_compare`) for object operands. `str`/`str`
    /// compares by code point (Python 3.14.6, "Comparisons"); a `str` against a non-`str`
    /// is unequal for `==`/`!=` but a `TypeError` for the ordering operators (Python:
    /// `"a" == 1` is `False`, `"a" < 1` raises). `Ok(None)` when neither operand is an
    /// object, so the caller falls back to the numeric / identity path.
    pub fn py_compare(&self, op: CmpOp, lhs: Value, rhs: Value) -> Result<Option<Value>, Trap> {
        match (self.str_value(lhs), self.str_value(rhs)) {
            (None, None) => Ok(None),
            (Some(a), Some(b)) => {
                let ord = a.cmp(b);
                let holds = match op {
                    CmpOp::Eq => ord == Ordering::Equal,
                    CmpOp::Ne => ord != Ordering::Equal,
                    CmpOp::Lt => ord == Ordering::Less,
                    CmpOp::Le => ord != Ordering::Greater,
                    CmpOp::Gt => ord == Ordering::Greater,
                    CmpOp::Ge => ord != Ordering::Less,
                };
                Ok(Some(Value::from_bool(holds)))
            }
            _ => match op {
                CmpOp::Eq => Ok(Some(Value::FALSE)),
                CmpOp::Ne => Ok(Some(Value::TRUE)),
                _ => Err(Trap::TypeError),
            },
        }
    }

    /// The dynamic truthiness dispatch (`py_truthy`) for object operands -- a `str` is
    /// true when non-empty (Python: an empty string is false). `Ok(None)` when `value` is
    /// not an object, so the caller falls back to [`Value::is_truthy`].
    pub fn py_truthy(&self, value: Value) -> Result<Option<bool>, Trap> {
        if let Some(s) = self.str_value(value) {
            return Ok(Some(!s.is_empty()));
        }
        Ok(None)
    }

    /// The dynamic subscript dispatch (`py_getitem`) for `container[index]` -- currently
    /// `str`. A `str` indexes by code point (Python 3.14.6, Common Sequence Operations):
    /// the index is an `int` (`bool` too, an int subtype), a negative index counts from
    /// the end (`len + i`), an index outside `[-len, len)` is an `IndexError`, and the
    /// result is a length-1 `str` (Python has no char type). A non-`int` index is a
    /// `TypeError`, as is subscripting a non-subscriptable value. (Slicing and
    /// store-subscript are separate operations; `str` is immutable.) Containers join this
    /// dispatch later -- the one-source-of-truth path the interpreter and the AOT
    /// `py_getitem` intrinsic both consume.
    pub fn py_getitem(&mut self, container: Value, index: Value) -> Result<Value, Trap> {
        if self.str_value(container).is_none() {
            return Err(Trap::TypeError);
        }
        if self.is_slice(index) {
            return self.str_getitem_slice(container, index);
        }
        let ch = {
            let s = self.str_value(container).ok_or(Trap::TypeError)?;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let len = s.chars().count() as i64;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(Trap::IndexError);
            }
            s.chars().nth(at as usize).ok_or(Trap::IndexError)?
        };
        let mut buf = [0u8; 4];
        self.new_str(ch.encode_utf8(&mut buf))
    }

    /// `str` slicing -- `container[slice]`. Reads the slice's `[start, stop, step]` and
    /// builds the substring per Python 3.14.6 (`slice.indices`): a `None` start/stop takes
    /// its default for the step direction, a negative bound counts from the end, out-of-range
    /// bounds CLAMP (no IndexError, unlike integer indexing), and the step may be negative
    /// (reversing). `step == 0` is a `ValueError`; a non-int, non-`None` bound a `TypeError`.
    fn str_getitem_slice(&mut self, container: Value, slice: Value) -> Result<Value, Trap> {
        let reference = slice.as_ref().ok_or(Trap::TypeError)?;
        let start_v = Value::from_bits(self.heap.read_u32(reference.0));
        let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        let step = if step_v.is_none() {
            1
        } else {
            let step = step_v.as_int().ok_or(Trap::TypeError)?;
            if step == 0 {
                return Err(Trap::ValueError);
            }
            step
        };
        let out = {
            let s = self.str_value(container).ok_or(Trap::TypeError)?;
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
            let mut out = String::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                if i >= 0 && i < len {
                    out.push(chars[i as usize]);
                }
                i += step;
            }
            out
        };
        self.new_str(&out)
    }

    /// Builds a `slice(start, stop, step)` object (each bound an int or `None`) -- the value
    /// `Op::BuildSlice` pushes and `Subscript` consumes. A small GC-leaf heap object.
    pub fn new_slice(&mut self, start: Value, stop: Value, step: Value) -> Result<Value, Trap> {
        let reference = self.heap.alloc(self.slice_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, start.bits());
        self.heap.write_u32(reference.0 + 4, stop.bits());
        self.heap.write_u32(reference.0 + 8, step.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a slice object (the value `Op::BuildSlice` produces).
    #[must_use]
    pub fn is_slice(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.slice_type_id)
    }

    /// The shared heap (for the collector, and for tests that drive a collection).
    #[must_use]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The shared heap, mutably (to drive a collection over external roots).
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The type with id `type_id`, if any.
    #[must_use]
    pub fn type_of(&self, type_id: u32) -> Option<&PyType> {
        self.types.get(type_id as usize)
    }

    /// Allocates an instance of `type_id`, initializing its attribute slots from
    /// `attrs` (one tagged value per slot, in slot order). `attrs` must have exactly
    /// the type's slot count.
    pub fn new_instance(&mut self, type_id: u32, attrs: &[Value]) -> Result<Value, Trap> {
        let ty = self.types.get(type_id as usize).ok_or(Trap::Malformed)?;
        if attrs.len() != usize::from(ty.num_slots) {
            return Err(Trap::Malformed);
        }
        let reference = self.heap.alloc(type_id).ok_or(Trap::OutOfMemory)?;
        for (i, value) in attrs.iter().enumerate() {
            self.heap.write_u32(reference.0 + (i as u32) * 4, value.bits());
        }
        Ok(Value::from_ref(reference))
    }

    /// `py_getattr`: the value of attribute `name` on `obj` -- equivalent to `obj.name`,
    /// the built-in `getattr(object, name)` (Python 3.14.6 Library Reference, "Built-in
    /// Functions"). Uses and updates the call-site inline `cache`: on a hit the resolved
    /// slot is reused, on a miss the type's attribute table is consulted and recorded.
    ///
    /// A failed attribute reference raises `AttributeError` ("Built-in Exceptions").
    /// That includes a receiver that is not a heap object here: in Python `(1).x` and
    /// `None.x` raise `AttributeError`, not `TypeError`, because those values DO support
    /// attribute references and merely lack the name -- `TypeError` is reserved for an
    /// object that does not support attribute references at all, which the supported
    /// types have none of. The full default lookup (data descriptors on the type, then
    /// the instance `__dict__`, then non-data descriptors / class attributes, then
    /// `__getattr__`; data model, "Customizing attribute access") is narrowed here to a
    /// fixed per-type slot table -- a simplification of the full lookup, not a deviation in the
    /// observable result for the subset.
    pub fn getattr(
        &mut self,
        obj: Value,
        name: &str,
        cache: &mut InlineCache,
    ) -> Result<Value, Trap> {
        let reference = obj.as_ref().ok_or(Trap::AttributeError)?;
        let type_id = self.heap.type_id_of(reference);
        if type_id == self.str_type_id {
            let method_id = str_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        let slot = match cache.lookup(type_id) {
            Some(slot) => slot,
            None => {
                let ty = self.types.get(type_id as usize).ok_or(Trap::Malformed)?;
                let slot = ty.slot_of(name).ok_or(Trap::AttributeError)?;
                cache.fill(type_id, slot);
                slot
            }
        };
        let word = self.heap.read_u32(reference.0 + u32::from(slot) * 4);
        Ok(Value::from_bits(word))
    }

    /// Binds str method `method_id` to `receiver`, returning a callable bound-method
    /// object (`receiver.method`). A heap object holding `[receiver, method_id]`, read back
    /// at the call. (`alloc` never relocates, so the receiver stays valid across it.)
    fn new_bound_method(&mut self, receiver: Value, method_id: u32) -> Result<Value, Trap> {
        let reference = self
            .heap
            .alloc(self.bound_method_type_id)
            .ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, receiver.bits());
        self.heap.write_u32(reference.0 + 4, method_id);
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a bound method (the callable a `str.method` reference produces).
    #[must_use]
    pub fn is_bound_method(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.bound_method_type_id)
    }

    /// Calls a bound method -- the `Call` dispatch when [`ObjectModel::is_bound_method`].
    /// Reads the stored `[receiver, method_id]` and runs the str method (Python 3.14.6
    /// "String Methods"): `upper`/`lower` (no args) return a cased copy, `startswith`/
    /// `endswith` (one str arg) a bool, `find` (one str arg) the lowest code-point index
    /// or -1. A wrong argument count, or a non-str argument, is a `TypeError`.
    pub fn call_bound_method(&mut self, callee: Value, args: &[Value]) -> Result<Value, Trap> {
        let reference = callee.as_ref().ok_or(Trap::TypeError)?;
        let receiver = Value::from_bits(self.heap.read_u32(reference.0));
        let method_id = self.heap.read_u32(reference.0 + 4);
        match method_id {
            STR_UPPER | STR_LOWER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let cased = if method_id == STR_UPPER {
                    s.to_uppercase()
                } else {
                    s.to_lowercase()
                };
                self.new_str(&cased)
            }
            STR_STARTSWITH | STR_ENDSWITH => {
                let (affix, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let affix = self.str_value(affix).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                let holds = if method_id == STR_STARTSWITH {
                    window.starts_with(affix)
                } else {
                    window.ends_with(affix)
                };
                Ok(Value::from_bool(holds))
            }
            STR_FIND => {
                let (sub, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let sub = self.str_value(sub).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                let index = match window.find(sub) {
                    Some(byte_offset) => a as i32 + window[..byte_offset].chars().count() as i32,
                    None => -1,
                };
                Value::fixnum(index).ok_or(Trap::Overflow)
            }
            STR_STRIP | STR_LSTRIP | STR_RSTRIP => {
                let chars = match args {
                    [] => None,
                    [c] if c.is_none() => None,
                    [c] => Some(*c),
                    _ => return Err(Trap::TypeError),
                };
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let trimmed = match chars {
                    None => match method_id {
                        STR_STRIP => s.trim(),
                        STR_LSTRIP => s.trim_start(),
                        _ => s.trim_end(),
                    },
                    Some(chars) => {
                        let set = self.str_value(chars).ok_or(Trap::TypeError)?;
                        match method_id {
                            STR_STRIP => s.trim_matches(|c| set.contains(c)),
                            STR_LSTRIP => s.trim_start_matches(|c| set.contains(c)),
                            _ => s.trim_end_matches(|c| set.contains(c)),
                        }
                    }
                };
                let trimmed = String::from(trimmed);
                self.new_str(&trimmed)
            }
            STR_REPLACE => {
                let (old, new, count) = match args {
                    [old, new] => (*old, *new, -1i64),
                    [old, new, count] => (*old, *new, count.as_int().ok_or(Trap::TypeError)?),
                    _ => return Err(Trap::TypeError),
                };
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let old = self.str_value(old).ok_or(Trap::TypeError)?;
                let new = self.str_value(new).ok_or(Trap::TypeError)?;
                let replaced = if count < 0 {
                    s.replace(old, new)
                } else {
                    s.replacen(old, new, count as usize)
                };
                self.new_str(&replaced)
            }
            STR_COUNT => {
                let (sub, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let sub = self.str_value(sub).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                Value::fixnum(window.matches(sub).count() as i32).ok_or(Trap::Overflow)
            }
            STR_ISDIGIT | STR_ISALPHA | STR_ISALNUM | STR_ISSPACE | STR_ISUPPER | STR_ISLOWER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                Ok(Value::from_bool(str_predicate(method_id, s)))
            }
            _ => Err(Trap::Malformed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point_model() -> (ObjectModel, Value) {
        let mut model = ObjectModel::new(alloc::vec![PyType::with_slots("Point", &["x", "y"])], 4096);
        let obj = model
            .new_instance(0, &[Value::fixnum(7).unwrap(), Value::fixnum(9).unwrap()])
            .unwrap();
        (model, obj)
    }

    #[test]
    fn str_round_trips_and_reports_codepoint_len() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("héllo").unwrap();
        assert!(s.is_pointer());
        assert_eq!(model.str_value(s), Some("héllo"));
        assert_eq!(model.py_len(s).unwrap().as_fixnum(), Some(5));
        assert_eq!(model.str_value(Value::fixnum(1).unwrap()), None);
        assert_eq!(model.py_len(Value::NONE), Err(Trap::TypeError));
        let upper = model.getattr(s, "upper", &mut InlineCache::empty()).unwrap();
        assert!(model.is_bound_method(upper));
        assert_eq!(
            model.getattr(s, "nope", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn str_methods_upper_lower_via_bound_method() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("Héllo").unwrap();
        assert!(!model.is_bound_method(s));
        let upper = model.getattr(s, "upper", &mut InlineCache::empty()).unwrap();
        assert!(model.is_bound_method(upper));
        let up = model.call_bound_method(upper, &[]).unwrap();
        assert_eq!(model.str_value(up), Some("HÉLLO"));
        let lower = model.getattr(s, "lower", &mut InlineCache::empty()).unwrap();
        let lo = model.call_bound_method(lower, &[]).unwrap();
        assert_eq!(model.str_value(lo), Some("héllo"));
        let again = model.getattr(s, "upper", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(again, &[s]), Err(Trap::TypeError));
    }

    #[test]
    fn str_dispatch_binary_compare_truthy() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let a = model.new_str("ab").unwrap();
        let b = model.new_str("cd").unwrap();
        let one = Value::fixnum(1).unwrap();

        let cat = model.py_binary(BinOp::Add, a, b).unwrap().unwrap();
        assert_eq!(model.str_value(cat), Some("abcd"));
        assert_eq!(model.py_binary(BinOp::Add, a, one), Err(Trap::TypeError));
        assert_eq!(model.py_binary(BinOp::Sub, a, b), Err(Trap::TypeError));
        assert_eq!(model.py_binary(BinOp::Add, one, one).unwrap(), None);

        let a2 = model.new_str("ab").unwrap();
        assert_eq!(model.py_compare(CmpOp::Eq, a, a2).unwrap(), Some(Value::TRUE));
        assert_eq!(model.py_compare(CmpOp::Lt, a, b).unwrap(), Some(Value::TRUE));
        assert_eq!(model.py_compare(CmpOp::Eq, a, one).unwrap(), Some(Value::FALSE));
        assert_eq!(model.py_compare(CmpOp::Lt, a, one), Err(Trap::TypeError));
        assert_eq!(model.py_compare(CmpOp::Eq, one, one).unwrap(), None);

        let empty = model.new_str("").unwrap();
        assert_eq!(model.py_truthy(a).unwrap(), Some(true));
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
        assert_eq!(model.py_truthy(one).unwrap(), None);
    }

    #[test]
    fn str_getitem_indexes_by_code_point() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("abc").unwrap();
        for (i, expect) in [(0, "a"), (2, "c"), (-1, "c"), (-3, "a")] {
            let r = model.py_getitem(s, Value::fixnum(i).unwrap()).unwrap();
            assert_eq!(model.str_value(r), Some(expect));
        }
        let r = model.py_getitem(s, Value::TRUE).unwrap();
        assert_eq!(model.str_value(r), Some("b"));
        assert_eq!(model.py_getitem(s, Value::fixnum(3).unwrap()), Err(Trap::IndexError));
        assert_eq!(model.py_getitem(s, Value::fixnum(-4).unwrap()), Err(Trap::IndexError));
        assert_eq!(model.py_getitem(s, s), Err(Trap::TypeError));
        let five = Value::fixnum(5).unwrap();
        assert_eq!(model.py_getitem(five, Value::fixnum(0).unwrap()), Err(Trap::TypeError));
        let cafe = model.new_str("café").unwrap();
        let at3 = model.py_getitem(cafe, Value::fixnum(3).unwrap()).unwrap();
        assert_eq!(model.str_value(at3), Some("é"));
        let neg1 = model.py_getitem(cafe, Value::fixnum(-1).unwrap()).unwrap();
        assert_eq!(model.str_value(neg1), Some("é"));
    }

    #[test]
    fn str_methods_startswith_endswith_find() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("héllo wörld").unwrap();
        let he = model.new_str("hé").unwrap();
        let ld = model.new_str("ld").unwrap();
        let wo = model.new_str("wö").unwrap();
        let zz = model.new_str("zz").unwrap();

        let sw = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw, &[he]).unwrap(), Value::TRUE);
        let sw2 = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw2, &[ld]).unwrap(), Value::FALSE);
        let ew = model.getattr(s, "endswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(ew, &[ld]).unwrap(), Value::TRUE);

        let f1 = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f1, &[wo]).unwrap().as_fixnum(), Some(6));
        let f2 = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f2, &[zz]).unwrap().as_fixnum(), Some(-1));

        let f3 = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(
            model.call_bound_method(f3, &[Value::fixnum(1).unwrap()]),
            Err(Trap::TypeError)
        );
        let sw3 = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw3, &[]), Err(Trap::TypeError));
    }

    #[test]
    fn str_methods_with_start_end_bounds() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("hello world").unwrap();
        let o = model.new_str("o").unwrap();
        let lo = model.new_str("lo").unwrap();
        let wor = model.new_str("wor").unwrap();
        let n = |v: i32| Value::fixnum(v).unwrap();

        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, n(5)]).unwrap().as_fixnum(), Some(7));
        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[wor, n(0), n(5)]).unwrap().as_fixnum(), Some(-1));
        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, n(-3)]).unwrap().as_fixnum(), Some(-1));

        let sw = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw, &[wor, n(6)]).unwrap(), Value::TRUE);
        let ew = model.getattr(s, "endswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(ew, &[lo, n(0), n(5)]).unwrap(), Value::TRUE);

        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, lo]), Err(Trap::TypeError));
        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, n(0), n(1), n(2)]), Err(Trap::TypeError));
    }

    #[test]
    fn str_methods_strip_replace_count() {
        let mut model = ObjectModel::new(Vec::new(), 4096);

        let s = model.new_str("  hi  ").unwrap();
        let bm = model.getattr(s, "strip", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[]).unwrap();
        assert_eq!(model.str_value(r), Some("hi"));

        let url = model.new_str("www.example.com").unwrap();
        let set = model.new_str("cmowz.").unwrap();
        let bm = model.getattr(url, "strip", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[set]).unwrap();
        assert_eq!(model.str_value(r), Some("example"));

        let spam = model.new_str("spam, spam, spam").unwrap();
        let old = model.new_str("spam").unwrap();
        let new = model.new_str("eggs").unwrap();
        let bm = model.getattr(spam, "replace", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[old, new]).unwrap();
        assert_eq!(model.str_value(r), Some("eggs, eggs, eggs"));
        let bm = model.getattr(spam, "replace", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[old, new, Value::fixnum(1).unwrap()]).unwrap();
        assert_eq!(model.str_value(r), Some("eggs, spam, spam"));

        let bm = model.getattr(spam, "count", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(bm, &[old]).unwrap().as_fixnum(), Some(3));
        let bm = model.getattr(spam, "count", &mut InlineCache::empty()).unwrap();
        let five = Value::fixnum(5).unwrap();
        assert_eq!(model.call_bound_method(bm, &[old, five]).unwrap().as_fixnum(), Some(2));
    }

    #[test]
    fn str_methods_predicates() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let cases: &[(&str, &str, bool)] = &[
            ("0123", "isdigit", true),
            ("12a", "isdigit", false),
            ("", "isdigit", false),
            ("abcDEF", "isalpha", true),
            ("abc1", "isalpha", false),
            ("abc123", "isalnum", true),
            ("a b", "isalnum", false),
            ("  \t\n", "isspace", true),
            (" a ", "isspace", false),
            ("BANANA", "isupper", true),
            ("BANANA1", "isupper", true),
            ("Banana", "isupper", false),
            ("123", "isupper", false),
            ("banana", "islower", true),
            ("baNana", "islower", false),
        ];
        for &(text, method, expected) in cases {
            let s = model.new_str(text).unwrap();
            let bm = model.getattr(s, method, &mut InlineCache::empty()).unwrap();
            let got = model.call_bound_method(bm, &[]).unwrap();
            assert_eq!(got, Value::from_bool(expected), "{text:?}.{method}()");
        }
    }

    #[test]
    fn str_slicing() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("hello").unwrap();
        let n = |v: i32| Value::fixnum(v).unwrap();
        let slice = |m: &mut ObjectModel, a: Value, b: Value, st: Value| {
            let sl = m.new_slice(a, b, st).unwrap();
            m.py_getitem(s, sl)
        };
        let r = slice(&mut model, n(1), n(4), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("ell"));
        let r = slice(&mut model, Value::NONE, Value::NONE, Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("hello"));
        let r = slice(&mut model, Value::NONE, Value::NONE, n(-1)).unwrap();
        assert_eq!(model.str_value(r), Some("olleh"));
        let r = slice(&mut model, n(-3), n(-1), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("ll"));
        let r = slice(&mut model, Value::NONE, Value::NONE, n(2)).unwrap();
        assert_eq!(model.str_value(r), Some("hlo"));
        let r = slice(&mut model, n(2), n(99), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("llo"));
        let r = slice(&mut model, n(4), n(1), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some(""));
        assert_eq!(slice(&mut model, Value::NONE, Value::NONE, n(0)), Err(Trap::ValueError));
        assert_eq!(slice(&mut model, s, Value::NONE, Value::NONE), Err(Trap::TypeError));
        assert_eq!(model.py_getitem(s, n(99)), Err(Trap::IndexError));
    }

    #[test]
    fn getattr_reads_the_right_slot() {
        let (mut model, obj) = point_model();
        let mut cx = InlineCache::empty();
        let mut cy = InlineCache::empty();
        assert_eq!(model.getattr(obj, "x", &mut cx).unwrap().as_fixnum(), Some(7));
        assert_eq!(model.getattr(obj, "y", &mut cy).unwrap().as_fixnum(), Some(9));
    }

    #[test]
    fn inline_cache_misses_then_hits() {
        let (mut model, obj) = point_model();
        let mut cache = InlineCache::empty();
        assert_eq!(cache.lookup(0), None);
        assert_eq!(model.getattr(obj, "x", &mut cache).unwrap().as_fixnum(), Some(7));
        assert_eq!(cache.lookup(0), Some(0));
        assert_eq!(model.getattr(obj, "x", &mut cache).unwrap().as_fixnum(), Some(7));
    }

    #[test]
    fn unknown_attribute_is_attribute_error() {
        let (mut model, obj) = point_model();
        assert_eq!(
            model.getattr(obj, "z", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn attribute_access_on_a_non_object_is_attribute_error() {
        let (mut model, _obj) = point_model();
        assert_eq!(
            model.getattr(Value::fixnum(1).unwrap(), "x", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
        assert_eq!(
            model.getattr(Value::NONE, "x", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn two_instances_share_one_filled_cache() {
        let mut model = ObjectModel::new(alloc::vec![PyType::with_slots("Point", &["x", "y"])], 4096);
        let a = model.new_instance(0, &[Value::fixnum(1).unwrap(), Value::fixnum(2).unwrap()]).unwrap();
        let b = model.new_instance(0, &[Value::fixnum(3).unwrap(), Value::fixnum(4).unwrap()]).unwrap();
        let mut cache = InlineCache::empty();
        assert_eq!(model.getattr(a, "x", &mut cache).unwrap().as_fixnum(), Some(1));
        assert_eq!(cache.lookup(0), Some(0));
        assert_eq!(model.getattr(b, "x", &mut cache).unwrap().as_fixnum(), Some(3));
    }
}
