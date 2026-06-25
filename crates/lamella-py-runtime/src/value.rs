//! The universal tagged-value representation.

use lamella_gc::Ref;

/// A Python value as it lives in any slot: a tagged 32-bit word.
///
/// `Copy` because a slot assignment is a word copy; the garbage collector relocates
/// the pointer it may carry (see [`Value::trace_slot`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Value(u32);

/// The low two bits that classify a non-fixnum word.
const TAG_MASK: u32 = 0b11;
/// A heap pointer has the low two bits clear (4-aligned) and is non-zero.
const PTR_TAG: u32 = 0b00;
/// A reserved singleton (`None`/`True`/`False`/...) is `2 (mod 4)`.
const SINGLETON_TAG: u32 = 0b10;

/// `None` -- the first reserved singleton.
const NONE_BITS: u32 = 0b0010;
/// `False` -- a reserved singleton (distinct word from a `0` fixnum).
const FALSE_BITS: u32 = 0b0110;
/// `True` -- a reserved singleton (distinct word from a `1` fixnum).
const TRUE_BITS: u32 = 0b1010;
/// Callable references share the low nibble `0b1110` -- a `2 (mod 4)` immediate the
/// collector skips (a callable lives in the bytecode / runtime, not the managed heap),
/// never colliding with None=2 / False=6 / True=10 -- and split on bit 4: a module
/// FUNCTION (bit 4 clear) versus a BUILT-IN (bit 4 set). The index/id occupies bits 5..,
/// distinguished by the 5-bit [`CALLABLE_TAG_MASK`].
const FUNCTION_REF_TAG: u32 = 0b0_1110;
/// A built-in reference (`abs`/`min`/`max`/`len`/...): the callable nibble plus bit 4.
const BUILTIN_REF_TAG: u32 = 0b1_1110;
/// The low-5-bit mask that selects between a function ref and a built-in ref.
const CALLABLE_TAG_MASK: u32 = 0b1_1111;

/// The iteration sentinel `py_next` returns when an iterator is exhausted (`PY_STOP` in
/// the runtime ABI). A reserved `2 (mod 4)` immediate the GC skips, distinct from
/// None=2/False=6/True=10 and never produced by a function-ref (`32k+14`) or builtin-ref
/// (`32k+30`), so it can never be a real element.
const STOP_BITS: u32 = 0b1_0010;
/// The error sentinel a runtime-support entry returns after it raised (`PY_ERROR` in the
/// ABI): the AOT call site checks `== PY_ERROR` and branches to the EH path, the actual
/// error living in the EH runtime's current-error slot. Another reserved `2 (mod 4)`
/// immediate.
const ERROR_BITS: u32 = 0b1_0110;

/// The most negative integer representable as a fixnum (the rest overflow to a
/// bignum).
pub const FIXNUM_MIN: i32 = -(1 << 30);
/// The most positive integer representable as a fixnum.
pub const FIXNUM_MAX: i32 = (1 << 30) - 1;

impl Value {
    /// The Python `None` singleton.
    pub const NONE: Value = Value(NONE_BITS);
    /// The Python `True` singleton.
    pub const TRUE: Value = Value(TRUE_BITS);
    /// The Python `False` singleton.
    pub const FALSE: Value = Value(FALSE_BITS);
    /// The unbound/empty sentinel: a slot that holds no value (a local read before it is
    /// assigned). Distinct from `None`. The collector skips it.
    pub const UNBOUND: Value = Value(0);
    /// The iteration sentinel (`PY_STOP` in the runtime ABI): `py_next` returns it when
    /// the iterator is exhausted. A reserved value, never a user-visible object.
    pub const STOP: Value = Value(STOP_BITS);
    /// The error sentinel (`PY_ERROR`): a runtime-support entry returns it after raising;
    /// the actual error lives in the EH runtime's current-error slot. Never user-visible.
    pub const PY_ERROR: Value = Value(ERROR_BITS);

    /// Wraps `n` as a fixnum, or `None` if it falls outside the 31-bit fixnum range
    /// (the caller promotes to a bignum, or traps).
    #[must_use]
    pub const fn fixnum(n: i32) -> Option<Value> {
        if n >= FIXNUM_MIN && n <= FIXNUM_MAX {
            Some(Value(((n as u32) << 1) | 1))
        } else {
            None
        }
    }

    /// The Python `bool` for `b`.
    #[must_use]
    pub const fn from_bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    /// Tags a heap reference as a pointer value. The reference must be a real,
    /// non-null object (4-aligned and non-zero, as every [`lamella_gc`] payload is),
    /// so the tagged word equals the address with no shift.
    #[must_use]
    pub fn from_ref(reference: Ref) -> Value {
        debug_assert!(!reference.is_null(), "from_ref(null)");
        debug_assert!(reference.0 & TAG_MASK == PTR_TAG, "ref not 4-aligned");
        Value(reference.0)
    }

    /// Whether this word is a fixnum.
    #[must_use]
    pub const fn is_fixnum(self) -> bool {
        self.0 & 1 == 1
    }

    /// The integer if this is a fixnum, else `None`. Arithmetic-shifts so the sign
    /// is recovered.
    #[must_use]
    pub const fn as_fixnum(self) -> Option<i32> {
        if self.is_fixnum() {
            Some((self.0 as i32) >> 1)
        } else {
            None
        }
    }

    /// The integer value of an `int` or a `bool`, else `None`.
    ///
    /// `bool` is a subtype of `int` (Python 3.14.6 Library Reference, "Numeric Types
    /// -- int, float, complex"; the data model gives `None`'s truth value as false but
    /// `True`/`False` act as `1`/`0` in numeric and comparison contexts). So `True`
    /// yields `1` and `False` yields `0` here, while each keeps its own object identity
    /// (`True is 1` stays false). The interpreter's arithmetic and comparisons use this
    /// so `True + 1 == 2` and `0 == False`, matching CPython. `None`, heap objects, and
    /// the unbound sentinel are not numbers, so they yield `None`.
    #[must_use]
    pub const fn as_int(self) -> Option<i64> {
        if let Some(n) = self.as_fixnum() {
            Some(n as i64)
        } else if self.0 == TRUE_BITS {
            Some(1)
        } else if self.0 == FALSE_BITS {
            Some(0)
        } else {
            None
        }
    }

    /// A reference to module function `index` -- the callable `LoadGlobal` pushes and
    /// `Call` consumes. The index occupies bits 5..; it is an immediate, not a managed
    /// pointer, since a function lives in the bytecode rather than the GC heap.
    #[must_use]
    pub const fn function_ref(index: u32) -> Value {
        Value((index << 5) | FUNCTION_REF_TAG)
    }

    /// Whether this is a module-function reference.
    #[must_use]
    pub const fn is_function_ref(self) -> bool {
        self.0 & CALLABLE_TAG_MASK == FUNCTION_REF_TAG
    }

    /// The module-function index if this is a function reference, else `None`.
    #[must_use]
    pub const fn as_function_index(self) -> Option<u32> {
        if self.is_function_ref() {
            Some(self.0 >> 5)
        } else {
            None
        }
    }

    /// A reference to built-in `id` (the runtime's built-in namespace -- `abs`/`min`/
    /// `max`/`len`/...). Like a function ref, an immediate the GC skips.
    #[must_use]
    pub const fn builtin_ref(id: u32) -> Value {
        Value((id << 5) | BUILTIN_REF_TAG)
    }

    /// Whether this is a built-in reference.
    #[must_use]
    pub const fn is_builtin_ref(self) -> bool {
        self.0 & CALLABLE_TAG_MASK == BUILTIN_REF_TAG
    }

    /// The built-in id if this is a built-in reference, else `None`.
    #[must_use]
    pub const fn as_builtin_id(self) -> Option<u32> {
        if self.is_builtin_ref() {
            Some(self.0 >> 5)
        } else {
            None
        }
    }

    /// Whether this word is a heap pointer (the only case the collector relocates).
    #[must_use]
    pub const fn is_pointer(self) -> bool {
        self.0 != 0 && (self.0 & TAG_MASK == PTR_TAG)
    }

    /// The heap reference if this is a pointer, else `None`.
    #[must_use]
    pub const fn as_ref(self) -> Option<Ref> {
        if self.is_pointer() {
            Some(Ref(self.0))
        } else {
            None
        }
    }

    /// Whether this is the `None` singleton.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == NONE_BITS
    }

    /// Whether this is the unbound/empty sentinel ([`Value::UNBOUND`]).
    #[must_use]
    pub const fn is_unbound(self) -> bool {
        self.0 == 0
    }

    /// Whether this is the iteration sentinel ([`Value::STOP`]).
    #[must_use]
    pub const fn is_stop(self) -> bool {
        self.0 == STOP_BITS
    }

    /// Whether this is the error sentinel ([`Value::PY_ERROR`]).
    #[must_use]
    pub const fn is_py_error(self) -> bool {
        self.0 == ERROR_BITS
    }

    /// Whether this is one of the reserved singletons (`None`/`True`/`False`/...).
    #[must_use]
    pub const fn is_singleton(self) -> bool {
        self.0 != 0 && (self.0 & TAG_MASK == SINGLETON_TAG)
    }

    /// The raw tagged word -- for serialization, tests, and the GC's own bookkeeping.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Reconstitutes a value from a raw tagged word (the inverse of [`Value::bits`]).
    /// Used by the heap-slot read path; the word is trusted to be a valid tagged value.
    #[must_use]
    pub const fn from_bits(word: u32) -> Value {
        Value(word)
    }

    /// Truth-value testing for a `PopJumpIfFalse` / boolean context.
    ///
    /// Per the official rule (Python 3.14.6 Library Reference, "Truth Value Testing"):
    /// an object is true by default unless its class defines `__bool__()` returning
    /// `False` or `__len__()` returning `0`; the built-in false values include `None`,
    /// `False`, and zero of any numeric type. So a fixnum is true when non-zero, `None`
    /// and `False` are false, `True` is true, and a heap object is true -- the
    /// interpreter defines no `__bool__`/`__len__`, and an object with neither is true by
    /// that same rule (the customizable `py_truthy` path is a separate concern, not a
    /// deviation here).
    #[must_use]
    pub fn is_truthy(self) -> bool {
        if let Some(n) = self.as_fixnum() {
            n != 0
        } else if self.is_pointer() || self.is_function_ref() || self.is_builtin_ref() {
            true
        } else {
            self == Value::TRUE
        }
    }

    /// The garbage-collector scan-by-tag hook for one slot.
    ///
    /// If the slot holds a heap pointer, hand its [`Ref`] to `visit` (which a moving
    /// collector relocates in place) and write the relocated reference back; pointer
    /// tag is `0b00`, so re-tagging is the identity. Immediates -- fixnums, the
    /// singletons, the unbound sentinel -- are skipped untouched. This is the one
    /// rule that lets the single shared collector trace a Python slot. The caller
    /// drives it from [`lamella_gc::Heap::collect`]'s root closure.
    pub fn trace_slot(slot: &mut Value, visit: &mut dyn FnMut(&mut Ref)) {
        if let Some(mut reference) = slot.as_ref() {
            visit(&mut reference);
            *slot = Value::from_ref(reference);
        }
    }
}

impl core::fmt::Debug for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(n) = self.as_fixnum() {
            write!(f, "Value::Fixnum({n})")
        } else if self.is_none() {
            write!(f, "Value::None")
        } else if *self == Value::TRUE {
            write!(f, "Value::True")
        } else if *self == Value::FALSE {
            write!(f, "Value::False")
        } else if let Some(index) = self.as_function_index() {
            write!(f, "Value::Function({index})")
        } else if let Some(id) = self.as_builtin_id() {
            write!(f, "Value::Builtin({id})")
        } else if self.is_stop() {
            write!(f, "Value::Stop")
        } else if self.is_py_error() {
            write!(f, "Value::PyError")
        } else if self.is_unbound() {
            write!(f, "Value::Unbound")
        } else {
            write!(f, "Value::Pointer({:#x})", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixnum_round_trips_across_the_range() {
        for n in [0, 1, -1, 2, -2, 42, -42, FIXNUM_MAX, FIXNUM_MIN, 1 << 20, -(1 << 20)] {
            let v = Value::fixnum(n).expect("in range");
            assert!(v.is_fixnum());
            assert!(!v.is_pointer());
            assert!(!v.is_singleton());
            assert_eq!(v.as_fixnum(), Some(n), "round trip {n}");
            assert_eq!(v.as_ref(), None);
        }
    }

    #[test]
    fn fixnum_out_of_range_is_rejected() {
        assert_eq!(Value::fixnum(FIXNUM_MAX.wrapping_add(1)), None);
        assert_eq!(Value::fixnum(FIXNUM_MIN.wrapping_sub(1)), None);
        assert_eq!(Value::fixnum(i32::MAX), None);
        assert_eq!(Value::fixnum(i32::MIN), None);
    }

    #[test]
    fn a_pointer_word_equals_its_payload_address() {
        for addr in [4u32, 8, 12, 0x1000, 0xDEAD0] {
            let r = Ref(addr);
            let v = Value::from_ref(r);
            assert!(v.is_pointer());
            assert!(!v.is_fixnum());
            assert!(!v.is_singleton());
            assert_eq!(v.as_ref(), Some(r));
            assert_eq!(v.bits(), addr);
        }
    }

    #[test]
    fn singletons_are_distinct_and_immediate() {
        let all = [Value::NONE, Value::TRUE, Value::FALSE];
        for v in all {
            assert!(v.is_singleton());
            assert!(!v.is_fixnum());
            assert!(!v.is_pointer());
            assert_eq!(v.as_ref(), None);
            assert_eq!(v.as_fixnum(), None);
        }
        assert_ne!(Value::NONE, Value::FALSE);
        assert_ne!(Value::FALSE, Value::fixnum(0).unwrap());
        assert_ne!(Value::TRUE, Value::fixnum(1).unwrap());
        assert!(Value::NONE.is_none());
    }

    #[test]
    fn unbound_is_not_none_and_is_skipped_by_gc() {
        assert!(Value::UNBOUND.is_unbound());
        assert!(!Value::UNBOUND.is_none());
        assert!(!Value::UNBOUND.is_pointer());
        assert_eq!(Value::UNBOUND.as_ref(), None);
    }

    #[test]
    fn truthiness_matches_python_for_the_supported_types() {
        assert!(!Value::fixnum(0).unwrap().is_truthy());
        assert!(Value::fixnum(1).unwrap().is_truthy());
        assert!(Value::fixnum(-5).unwrap().is_truthy());
        assert!(!Value::NONE.is_truthy());
        assert!(!Value::FALSE.is_truthy());
        assert!(Value::TRUE.is_truthy());
        assert!(Value::from_ref(Ref(8)).is_truthy());
    }

    #[test]
    fn as_int_covers_int_and_bool_only() {
        assert_eq!(Value::fixnum(5).unwrap().as_int(), Some(5));
        assert_eq!(Value::fixnum(-3).unwrap().as_int(), Some(-3));
        assert_eq!(Value::TRUE.as_int(), Some(1));
        assert_eq!(Value::FALSE.as_int(), Some(0));
        assert_eq!(Value::NONE.as_int(), None);
        assert_eq!(Value::from_ref(Ref(8)).as_int(), None);
        assert_eq!(Value::UNBOUND.as_int(), None);
    }

    #[test]
    fn function_refs_are_distinct_immediates() {
        for idx in [0u32, 1, 2, 42, 1000] {
            let f = Value::function_ref(idx);
            assert!(f.is_function_ref());
            assert_eq!(f.as_function_index(), Some(idx));
            assert!(!f.is_fixnum());
            assert!(!f.is_pointer());
            assert_eq!(f.as_ref(), None);
            assert_eq!(f.as_int(), None);
            assert!(!f.is_none());
            assert!(f.is_truthy());
        }
        assert!(!Value::NONE.is_function_ref());
        assert!(!Value::TRUE.is_function_ref());
        assert!(!Value::FALSE.is_function_ref());
        assert!(!Value::fixnum(7).unwrap().is_function_ref());
        let mut slot = Value::function_ref(3);
        Value::trace_slot(&mut slot, &mut |r| *r = Ref(r.0 + 0x100));
        assert_eq!(slot, Value::function_ref(3));
    }

    #[test]
    fn builtin_refs_are_distinct_from_function_refs_and_immediates() {
        for id in [0u32, 1, 2, 3, 99] {
            let b = Value::builtin_ref(id);
            assert!(b.is_builtin_ref());
            assert_eq!(b.as_builtin_id(), Some(id));
            assert!(!b.is_function_ref());
            assert_eq!(b.as_function_index(), None);
            assert!(!b.is_pointer());
            assert!(!b.is_fixnum());
            assert_eq!(b.as_int(), None);
            assert!(b.is_truthy());
        }
        assert_ne!(Value::function_ref(0), Value::builtin_ref(0));
        assert!(!Value::function_ref(0).is_builtin_ref());
        assert!(!Value::builtin_ref(0).is_function_ref());
        assert!(!Value::NONE.is_builtin_ref());
        assert!(!Value::TRUE.is_builtin_ref());
        assert!(!Value::FALSE.is_builtin_ref());
    }

    #[test]
    fn reserved_sentinels_are_distinct() {
        for s in [Value::STOP, Value::PY_ERROR] {
            assert!(!s.is_fixnum());
            assert!(!s.is_pointer());
            assert!(!s.is_function_ref());
            assert!(!s.is_builtin_ref());
            assert_eq!(s.as_ref(), None);
        }
        assert!(Value::STOP.is_stop() && !Value::STOP.is_py_error());
        assert!(Value::PY_ERROR.is_py_error() && !Value::PY_ERROR.is_stop());
        for other in [
            Value::NONE,
            Value::TRUE,
            Value::FALSE,
            Value::function_ref(0),
            Value::builtin_ref(0),
        ] {
            assert_ne!(Value::STOP, other);
            assert_ne!(Value::PY_ERROR, other);
        }
        assert_ne!(Value::STOP, Value::PY_ERROR);
    }

    #[test]
    fn trace_slot_relocates_a_pointer_and_skips_immediates() {
        let mut relocate = |r: &mut Ref| *r = Ref(r.0 + 0x100);

        let mut ptr = Value::from_ref(Ref(0x40));
        Value::trace_slot(&mut ptr, &mut relocate);
        assert_eq!(ptr.as_ref(), Some(Ref(0x140)), "pointer relocated in place");

        for mut immediate in [
            Value::fixnum(7).unwrap(),
            Value::NONE,
            Value::TRUE,
            Value::FALSE,
            Value::UNBOUND,
        ] {
            let before = immediate;
            Value::trace_slot(&mut immediate, &mut relocate);
            assert_eq!(immediate, before, "immediate left untouched");
        }
    }
}
