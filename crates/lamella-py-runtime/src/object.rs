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

/// The id of the `str` method `name`, or `None` if `str` has no such method.
fn str_method_id(name: &str) -> Option<u32> {
    match name {
        "upper" => Some(STR_UPPER),
        "lower" => Some(STR_LOWER),
        "startswith" => Some(STR_STARTSWITH),
        "endswith" => Some(STR_ENDSWITH),
        "find" => Some(STR_FIND),
        _ => None,
    }
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
        ObjectModel {
            heap: Heap::new(heap_capacity, descs),
            types,
            strings: Vec::new(),
            str_type_id,
            bound_method_type_id,
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
        let ch = {
            let Some(s) = self.str_value(container) else {
                return Err(Trap::TypeError);
            };
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
                if args.len() != 1 {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let affix = self.str_value(args[0]).ok_or(Trap::TypeError)?;
                let holds = if method_id == STR_STARTSWITH {
                    s.starts_with(affix)
                } else {
                    s.ends_with(affix)
                };
                Ok(Value::from_bool(holds))
            }
            STR_FIND => {
                if args.len() != 1 {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let sub = self.str_value(args[0]).ok_or(Trap::TypeError)?;
                let index = match s.find(sub) {
                    Some(byte_offset) => s[..byte_offset].chars().count() as i32,
                    None => -1,
                };
                Value::fixnum(index).ok_or(Trap::Overflow)
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
