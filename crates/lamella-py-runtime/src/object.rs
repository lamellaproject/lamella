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
const STR_SPLIT: u32 = 16;
const STR_ISDECIMAL: u32 = 17;
const STR_ISNUMERIC: u32 = 18;
const STR_JOIN: u32 = 19;
const STR_RFIND: u32 = 20;
const STR_INDEX: u32 = 21;
const STR_RINDEX: u32 = 22;
const STR_CAPITALIZE: u32 = 23;
const STR_TITLE: u32 = 24;
const STR_SWAPCASE: u32 = 25;
const STR_SPLITLINES: u32 = 26;
const STR_REMOVEPREFIX: u32 = 27;
const STR_REMOVESUFFIX: u32 = 28;
const STR_ZFILL: u32 = 29;
const STR_LJUST: u32 = 30;
const STR_RJUST: u32 = 31;
const STR_CENTER: u32 = 32;
const STR_PARTITION: u32 = 33;
const STR_RPARTITION: u32 = 34;
const STR_EXPANDTABS: u32 = 35;
const STR_ISASCII: u32 = 36;
const STR_ISIDENTIFIER: u32 = 37;

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
        "split" => Some(STR_SPLIT),
        "isdecimal" => Some(STR_ISDECIMAL),
        "isnumeric" => Some(STR_ISNUMERIC),
        "join" => Some(STR_JOIN),
        "rfind" => Some(STR_RFIND),
        "index" => Some(STR_INDEX),
        "rindex" => Some(STR_RINDEX),
        "capitalize" => Some(STR_CAPITALIZE),
        "title" => Some(STR_TITLE),
        "swapcase" => Some(STR_SWAPCASE),
        "splitlines" => Some(STR_SPLITLINES),
        "removeprefix" => Some(STR_REMOVEPREFIX),
        "removesuffix" => Some(STR_REMOVESUFFIX),
        "zfill" => Some(STR_ZFILL),
        "ljust" => Some(STR_LJUST),
        "rjust" => Some(STR_RJUST),
        "center" => Some(STR_CENTER),
        "partition" => Some(STR_PARTITION),
        "rpartition" => Some(STR_RPARTITION),
        "expandtabs" => Some(STR_EXPANDTABS),
        "isascii" => Some(STR_ISASCII),
        "isidentifier" => Some(STR_ISIDENTIFIER),
        _ => None,
    }
}

const LIST_APPEND: u32 = 0;
const LIST_POP: u32 = 1;
const LIST_SORT: u32 = 2;
const LIST_REVERSE: u32 = 3;
const LIST_INSERT: u32 = 4;
const LIST_REMOVE: u32 = 5;
const LIST_INDEX: u32 = 6;
const LIST_COUNT: u32 = 7;
const LIST_EXTEND: u32 = 8;
const LIST_CLEAR: u32 = 9;
const LIST_COPY: u32 = 10;
const DICT_GET: u32 = 0;
const DICT_KEYS: u32 = 1;
const DICT_VALUES: u32 = 2;
const DICT_ITEMS: u32 = 3;
const DICT_UPDATE: u32 = 4;
const DICT_POP: u32 = 5;
const DICT_SETDEFAULT: u32 = 6;
const DICT_CLEAR: u32 = 7;
const DICT_COPY: u32 = 8;
const SET_UNION: u32 = 0;
const SET_INTERSECTION: u32 = 1;
const SET_DIFFERENCE: u32 = 2;
const SET_SYMMETRIC_DIFFERENCE: u32 = 3;
const SET_ISSUBSET: u32 = 4;
const SET_ISSUPERSET: u32 = 5;
const SET_ISDISJOINT: u32 = 6;
const SET_COPY: u32 = 7;
const SET_ADD: u32 = 8;
const SET_DISCARD: u32 = 9;
const SET_REMOVE: u32 = 10;
const SET_CLEAR: u32 = 11;
const SET_POP: u32 = 12;
const SET_UPDATE: u32 = 13;
const TUPLE_INDEX: u32 = 0;
const TUPLE_COUNT: u32 = 1;

/// The `list`-method id for `name`, or `None`.
fn list_method_id(name: &str) -> Option<u32> {
    match name {
        "append" => Some(LIST_APPEND),
        "pop" => Some(LIST_POP),
        "sort" => Some(LIST_SORT),
        "reverse" => Some(LIST_REVERSE),
        "insert" => Some(LIST_INSERT),
        "remove" => Some(LIST_REMOVE),
        "index" => Some(LIST_INDEX),
        "count" => Some(LIST_COUNT),
        "extend" => Some(LIST_EXTEND),
        "clear" => Some(LIST_CLEAR),
        "copy" => Some(LIST_COPY),
        _ => None,
    }
}

/// The `dict`-method id for `name`, or `None`.
fn dict_method_id(name: &str) -> Option<u32> {
    match name {
        "get" => Some(DICT_GET),
        "keys" => Some(DICT_KEYS),
        "values" => Some(DICT_VALUES),
        "items" => Some(DICT_ITEMS),
        "update" => Some(DICT_UPDATE),
        "pop" => Some(DICT_POP),
        "setdefault" => Some(DICT_SETDEFAULT),
        "clear" => Some(DICT_CLEAR),
        "copy" => Some(DICT_COPY),
        _ => None,
    }
}

/// The method id for a `set` method `name` -- the full mutable surface.
fn set_method_id(name: &str) -> Option<u32> {
    match name {
        "union" => Some(SET_UNION),
        "intersection" => Some(SET_INTERSECTION),
        "difference" => Some(SET_DIFFERENCE),
        "symmetric_difference" => Some(SET_SYMMETRIC_DIFFERENCE),
        "issubset" => Some(SET_ISSUBSET),
        "issuperset" => Some(SET_ISSUPERSET),
        "isdisjoint" => Some(SET_ISDISJOINT),
        "copy" => Some(SET_COPY),
        "add" => Some(SET_ADD),
        "discard" => Some(SET_DISCARD),
        "remove" => Some(SET_REMOVE),
        "clear" => Some(SET_CLEAR),
        "pop" => Some(SET_POP),
        "update" => Some(SET_UPDATE),
        _ => None,
    }
}

/// The method id for a `tuple` method `name` -- the immutable sequence queries.
fn tuple_method_id(name: &str) -> Option<u32> {
    match name {
        "index" => Some(TUPLE_INDEX),
        "count" => Some(TUPLE_COUNT),
        _ => None,
    }
}

/// The method id for a `frozenset` method `name` -- the read-only subset only (a frozenset is
/// immutable, so `add`/`discard`/`pop`/... are not attributes).
fn frozenset_method_id(name: &str) -> Option<u32> {
    match name {
        "union" => Some(SET_UNION),
        "intersection" => Some(SET_INTERSECTION),
        "difference" => Some(SET_DIFFERENCE),
        "symmetric_difference" => Some(SET_SYMMETRIC_DIFFERENCE),
        "issubset" => Some(SET_ISSUBSET),
        "issuperset" => Some(SET_ISSUPERSET),
        "isdisjoint" => Some(SET_ISDISJOINT),
        "copy" => Some(SET_COPY),
        _ => None,
    }
}

/// Whether `s` satisfies a `str` predicate (`isdigit`/`isalpha`/`isalnum`/`isspace`/
/// `isupper`/`islower`, Python 3.14.6 "String Methods"). The category predicates require
/// at least one character; `isupper`/`islower` require at least one CASED character and
/// that every cased character has that case. Classification is exact vs CPython: the
/// predicates derive from the shared [`lamella_unicode`] UCD properties (validated against
/// CPython's `unicodedata` + `str` methods over every code point), not from Rust's `char`
/// classification (which uses the broader `Alphabetic`/`White_Space` properties and diverges
/// on combining marks, superscript digits, CJK numerics, and the separator controls).
fn str_predicate(method_id: u32, s: &str) -> bool {
    use lamella_unicode::{
        general_category, is_lowercase, is_uppercase, is_white_space, is_xid_continue,
        is_xid_start, numeric_level, GeneralCategory,
    };
    let is_titlecase = |cp: u32| general_category(cp) == GeneralCategory::TitlecaseLetter;
    match method_id {
        STR_ISDIGIT => !s.is_empty() && s.chars().all(|c| numeric_level(c as u32) >= 2),
        STR_ISDECIMAL => !s.is_empty() && s.chars().all(|c| numeric_level(c as u32) >= 3),
        STR_ISNUMERIC => !s.is_empty() && s.chars().all(|c| numeric_level(c as u32) >= 1),
        STR_ISALPHA => !s.is_empty() && s.chars().all(|c| general_category(c as u32).is_letter()),
        STR_ISALNUM => {
            !s.is_empty()
                && s.chars().all(|c| {
                    let cp = c as u32;
                    general_category(cp).is_letter() || numeric_level(cp) >= 1
                })
        }
        STR_ISSPACE => {
            !s.is_empty()
                && s.chars().all(|c| {
                    let cp = c as u32;
                    is_white_space(cp) || (0x1c..=0x1f).contains(&cp)
                })
        }
        STR_ISUPPER => {
            let mut cased = false;
            for c in s.chars() {
                let cp = c as u32;
                if is_lowercase(cp) || is_titlecase(cp) {
                    return false;
                }
                cased |= is_uppercase(cp);
            }
            cased
        }
        STR_ISLOWER => {
            let mut cased = false;
            for c in s.chars() {
                let cp = c as u32;
                if is_uppercase(cp) || is_titlecase(cp) {
                    return false;
                }
                cased |= is_lowercase(cp);
            }
            cased
        }
        STR_ISASCII => s.chars().all(|c| (c as u32) < 0x80),
        STR_ISIDENTIFIER => match s.chars().next() {
            None => false,
            Some(first) => {
                (first == '_' || is_xid_start(first as u32))
                    && s.chars().skip(1).all(|c| is_xid_continue(c as u32))
            }
        },
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

/// The Python `repr()` of a string: single quotes, switching to double quotes if the string
/// contains a `'` but no `"`; backslash, the quote, and the common control chars are escaped.
/// (Escaping of exotic non-printables is an ASCII-faithful refinement.)
fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&alloc::format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The number of elements in `range(start, stop, step)` (CPython's length formula).
fn range_len(start: i64, stop: i64, step: i64) -> i64 {
    if step > 0 {
        if start >= stop {
            0
        } else {
            (stop - start - 1) / step + 1
        }
    } else if start <= stop {
        0
    } else {
        (start - stop - 1) / (-step) + 1
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
    /// The GC type-descriptor id of a `slice(start, stop, step)`; it follows the bound method.
    slice_type_id: u32,
    /// The runtime backing for `list`/`tuple`: each container's heap object holds an index
    /// into this, and its elements (tagged Values) live in the indexed `Vec`. A list mutates
    /// its `Vec` in place; a tuple never does. (str-arena pattern; the GC-faithful
    /// variable-size container object is a follow-up on the tagged-trace seam.)
    seqs: Vec<Vec<Value>>,
    /// The runtime backing for `dict`: insertion-ordered key/value pairs (Python dicts
    /// preserve insertion order). A dict's heap object holds an index into this.
    dicts: Vec<Vec<(Value, Value)>>,
    /// The runtime backing for `set`: deduped elements in insertion order. A set's heap object
    /// holds an index into this. (Iteration order is insertion, not CPython's hash order -- a
    /// documented divergence; differential tests compare sets as sets, e.g. via `sorted`.)
    sets: Vec<Vec<Value>>,
    /// The GC type-descriptor id of a `list`; it follows the slice.
    list_type_id: u32,
    /// The GC type-descriptor id of a `tuple`; it follows the list.
    tuple_type_id: u32,
    /// The GC type-descriptor id of a `dict`; it follows the tuple.
    dict_type_id: u32,
    /// The GC type-descriptor id of an iterator (over str/list/tuple/dict); follows dict.
    iter_type_id: u32,
    /// The GC type-descriptor id of a user CLASS object `[name, base, namespace-dict]`.
    class_type_id: u32,
    /// The GC type-descriptor id of a user class INSTANCE `[type, __dict__]`.
    instance_type_id: u32,
    /// The GC type-descriptor id of a bound Python method `[self, func-ref]`.
    py_bound_type_id: u32,
    /// The GC type-descriptor id of a `range(start, stop, step)` (a lazy int sequence).
    range_type_id: u32,
    /// The GC type-descriptor id of a `set` (a deduped collection); follows range.
    set_type_id: u32,
    /// The GC type-descriptor id of a `super` object `[class, self]`; follows set.
    super_type_id: u32,
    /// The GC type-descriptor id of a `frozenset` (an immutable set); shares the `sets` arena.
    frozenset_type_id: u32,
    /// The built-in exception classes (name -> class object), built lazily on first use so a
    /// program that never touches exceptions never allocates them. `BaseException` down to the
    /// concrete leaves; a raised interpreter [`Trap`] instantiates the matching one.
    exception_classes: Vec<(&'static str, Value)>,
    /// The exception currently in flight while it propagates to a handler (set by a `raise`
    /// and carried across call frames -- a [`Trap::Raised`] is the signal, this is the object).
    pending_exception: Option<Value>,
    /// The module namespace (top-level name -> value): classes and other top-level bindings the
    /// module body produces, which a function reaches by `LoadGlobal`. The body mirrors its locals
    /// here as it binds them.
    globals: Vec<(String, Value)>,
    /// Captured `print(...)` output (the interpreter is `no_std`, so it buffers rather than
    /// writing a stream; the host drains it).
    stdout: String,
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
                tagged_offsets: (0..u32::from(t.num_slots)).map(|i| i * 4).collect(),
            })
            .collect();
        let str_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bound_method_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..1).map(|i| i * 4).collect(),
        });
        let slice_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let list_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let tuple_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let dict_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let iter_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..1).map(|i| i * 4).collect(),
        });
        let class_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let instance_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let py_bound_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let range_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let set_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let super_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let frozenset_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        ObjectModel {
            heap: Heap::new(heap_capacity, descs),
            types,
            strings: Vec::new(),
            seqs: Vec::new(),
            dicts: Vec::new(),
            sets: Vec::new(),
            str_type_id,
            bound_method_type_id,
            slice_type_id,
            list_type_id,
            tuple_type_id,
            dict_type_id,
            iter_type_id,
            class_type_id,
            instance_type_id,
            py_bound_type_id,
            range_type_id,
            set_type_id,
            super_type_id,
            frozenset_type_id,
            exception_classes: Vec::new(),
            pending_exception: None,
            globals: Vec::new(),
            stdout: String::new(),
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
        let n = if let Some(s) = self.str_value(value) {
            s.chars().count()
        } else if let Some(elems) = self.seq_value(value) {
            elems.len()
        } else if let Some(entries) = self.dict_value(value) {
            entries.len()
        } else if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            range_len(start, stop, step).max(0) as usize
        } else if let Some(elements) = self.set_value(value) {
            elements.len()
        } else {
            return Err(Trap::TypeError);
        };
        Value::fixnum(n as i32).ok_or(Trap::Overflow)
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
        if self.is_set(lhs) || self.is_frozenset(lhs) {
            return Ok(Some(self.set_binary_op(op, lhs, rhs)?));
        }
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
        if self.is_set(lhs) || self.is_frozenset(lhs) {
            return Ok(Some(self.set_compare(op, lhs, rhs)?));
        }
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

    /// Python truthiness of `value`: `None`/`False`/`0`/`""`/empty container/empty range are
    /// false; a non-empty str/container, a non-zero int, and any other object (e.g. a class
    /// instance) are true. Always `Ok(Some(_))` for the value subset we have (the `Option` keeps
    /// the seam for a future `__bool__`/`__len__` dispatch that could defer).
    pub fn py_truthy(&self, value: Value) -> Result<Option<bool>, Trap> {
        if value.is_none() || value == Value::FALSE {
            return Ok(Some(false));
        }
        if value == Value::TRUE {
            return Ok(Some(true));
        }
        if let Some(n) = value.as_fixnum() {
            return Ok(Some(n != 0));
        }
        if let Some(s) = self.str_value(value) {
            return Ok(Some(!s.is_empty()));
        }
        if let Some(elems) = self.seq_value(value) {
            return Ok(Some(!elems.is_empty()));
        }
        if let Some(entries) = self.dict_value(value) {
            return Ok(Some(!entries.is_empty()));
        }
        if let Some(elements) = self.set_value(value) {
            return Ok(Some(!elements.is_empty()));
        }
        if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            return Ok(Some(range_len(start, stop, step) > 0));
        }
        Ok(Some(true))
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
        if self.str_value(container).is_some() {
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
            return self.new_str(ch.encode_utf8(&mut buf));
        }
        if self.seq_value(container).is_some() {
            if self.is_slice(index) {
                return self.seq_getitem_slice(container, index);
            }
            let elems = self.seq_value(container).ok_or(Trap::TypeError)?;
            let len = elems.len() as i64;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(Trap::IndexError);
            }
            return Ok(elems[at as usize]);
        }
        if let Some(entries) = self.dict_value(container) {
            for (k, v) in entries {
                if self.key_eq(*k, index) {
                    return Ok(*v);
                }
            }
            return Err(Trap::KeyError);
        }
        if self.is_range(container) {
            let (start, stop, step) = self.range_bounds(container);
            let len = range_len(start, stop, step);
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(Trap::IndexError);
            }
            return Value::fixnum((start + at * step) as i32).ok_or(Trap::Overflow);
        }
        Err(Trap::TypeError)
    }

    /// `str` slicing -- `container[slice]`. Reads the slice's `[start, stop, step]` and
    /// builds the substring per Python 3.14.6 (`slice.indices`): a `None` start/stop takes
    /// its default for the step direction, a negative bound counts from the end, out-of-range
    /// bounds CLAMP (no IndexError, unlike integer indexing), and the step may be negative
    /// (reversing). `step == 0` is a `ValueError`; a non-int, non-`None` bound a `TypeError`.
    /// `seq[i:j:k]` -- a new `list` (from a list) or `tuple` (from a tuple) of the selected
    /// elements, with CPython slice semantics (clamping bounds, no IndexError, negative step).
    fn seq_getitem_slice(&mut self, container: Value, slice: Value) -> Result<Value, Trap> {
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
        let is_tuple = self.is_tuple(container);
        let selected: Vec<Value> = {
            let elems = self.seq_value(container).ok_or(Trap::TypeError)?;
            let len = elems.len() as i64;
            let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
            let mut out = Vec::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                if i >= 0 && i < len {
                    out.push(elems[i as usize]);
                }
                i += step;
            }
            out
        };
        if is_tuple {
            self.new_tuple(selected)
        } else {
            self.new_list(selected)
        }
    }

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

    /// Allocates a `range(start, stop, step)` -- a lazy int sequence (the bounds are fixnums,
    /// so an i32-range; a wider range would overflow, matching the corpus's needs).
    pub fn new_range(&mut self, start: i64, stop: i64, step: i64) -> Result<Value, Trap> {
        let s = Value::fixnum(i32::try_from(start).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let e = Value::fixnum(i32::try_from(stop).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let t = Value::fixnum(i32::try_from(step).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let reference = self.heap.alloc(self.range_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, s.bits());
        self.heap.write_u32(reference.0 + 4, e.bits());
        self.heap.write_u32(reference.0 + 8, t.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `range`.
    #[must_use]
    pub fn is_range(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.range_type_id)
    }

    /// Whether `value` is an iterator object (the value [`Self::new_iter`] produces).
    #[must_use]
    pub fn is_iter(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.iter_type_id)
    }

    /// The `(start, stop, step)` of a range (the caller has established `is_range`).
    fn range_bounds(&self, value: Value) -> (i64, i64, i64) {
        let reference = value.as_ref().expect("a range");
        let start = Value::from_bits(self.heap.read_u32(reference.0)).as_int().unwrap_or(0);
        let stop = Value::from_bits(self.heap.read_u32(reference.0 + 4)).as_int().unwrap_or(0);
        let step = Value::from_bits(self.heap.read_u32(reference.0 + 8)).as_int().unwrap_or(1);
        (start, stop, step)
    }

    /// The backing-arena index of `value` if its heap object has type `type_id`.
    fn container_slot(&self, value: Value, type_id: u32) -> Option<usize> {
        let reference = value.as_ref()?;
        (self.heap.type_id_of(reference) == type_id).then(|| self.heap.read_u32(reference.0) as usize)
    }

    /// The `seqs`-arena index if `value` is a `list` or `tuple`.
    fn seq_slot(&self, value: Value) -> Option<usize> {
        self.container_slot(value, self.list_type_id)
            .or_else(|| self.container_slot(value, self.tuple_type_id))
    }

    /// The elements if `value` is a `list` or `tuple`.
    fn seq_value(&self, value: Value) -> Option<&Vec<Value>> {
        self.seq_slot(value).and_then(|i| self.seqs.get(i))
    }

    /// The key/value pairs if `value` is a `dict`.
    fn dict_value(&self, value: Value) -> Option<&Vec<(Value, Value)>> {
        self.container_slot(value, self.dict_type_id)
            .and_then(|i| self.dicts.get(i))
    }

    /// A clone of a dict's `(key, value)` pairs, if `value` is a dict (so a caller can rebuild
    /// or copy the dict without holding a borrow on the model). `dict(other_dict)`.
    #[must_use]
    pub fn dict_entries(&self, value: Value) -> Option<Vec<(Value, Value)>> {
        self.dict_value(value).cloned()
    }

    /// Whether `value` is a `list`.
    #[must_use]
    pub fn is_list(&self, value: Value) -> bool {
        self.container_slot(value, self.list_type_id).is_some()
    }

    /// Whether `value` is a `tuple`.
    #[must_use]
    pub fn is_tuple(&self, value: Value) -> bool {
        self.container_slot(value, self.tuple_type_id).is_some()
    }

    /// Whether `value` is a `dict`.
    #[must_use]
    pub fn is_dict(&self, value: Value) -> bool {
        self.container_slot(value, self.dict_type_id).is_some()
    }

    /// Allocates a `list` over `elements` (a mutable sequence). The elements live in the
    /// backing arena; the heap object holds the index.
    pub fn new_list(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = self.seqs.len() as u32;
        self.seqs.push(elements);
        let reference = self.heap.alloc(self.list_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `tuple` over `elements` (an immutable sequence).
    pub fn new_tuple(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = self.seqs.len() as u32;
        self.seqs.push(elements);
        let reference = self.heap.alloc(self.tuple_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `dict` over `pairs`, in insertion order, collapsing duplicate keys with
    /// the last value winning (Python `{...}` display semantics; the key keeps its first
    /// position).
    pub fn new_dict(&mut self, pairs: Vec<(Value, Value)>) -> Result<Value, Trap> {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for (key, value) in pairs {
            match entries.iter().position(|(k, _)| self.key_eq(*k, key)) {
                Some(slot) => entries[slot].1 = value,
                None => entries.push((key, value)),
            }
        }
        let index = self.dicts.len() as u32;
        self.dicts.push(entries);
        let reference = self.heap.alloc(self.dict_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `set`/`frozenset` over `elements`, deduped by value equality in first-seen
    /// order, into the shared arena under `type_id`.
    fn alloc_set(&mut self, elements: Vec<Value>, type_id: u32) -> Result<Value, Trap> {
        let mut deduped: Vec<Value> = Vec::new();
        for element in elements {
            if !deduped.iter().any(|e| self.key_eq(*e, element)) {
                deduped.push(element);
            }
        }
        let index = self.sets.len() as u32;
        self.sets.push(deduped);
        let reference = self.heap.alloc(type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `set` over `elements`, deduped by value equality, in first-seen order.
    pub fn new_set(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let type_id = self.set_type_id;
        self.alloc_set(elements, type_id)
    }

    /// Allocates a `frozenset` over `elements` (an immutable set).
    pub fn new_frozenset(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let type_id = self.frozenset_type_id;
        self.alloc_set(elements, type_id)
    }

    /// Whether `value` is a `set`.
    #[must_use]
    pub fn is_set(&self, value: Value) -> bool {
        self.container_slot(value, self.set_type_id).is_some()
    }

    /// Whether `value` is a `frozenset`.
    #[must_use]
    pub fn is_frozenset(&self, value: Value) -> bool {
        self.container_slot(value, self.frozenset_type_id).is_some()
    }

    /// The elements if `value` is a `set` or `frozenset` (both back onto the shared arena, so
    /// every read op -- len, `in`, iteration, repr, truthiness -- works for either).
    fn set_value(&self, value: Value) -> Option<&Vec<Value>> {
        let slot = self
            .container_slot(value, self.set_type_id)
            .or_else(|| self.container_slot(value, self.frozenset_type_id))?;
        self.sets.get(slot)
    }

    /// Adds `value` to the set (a no-op if an equal element is present) -- `set.add` and the
    /// `SetAdd` comprehension op.
    pub fn set_add(&mut self, set: Value, value: Value) -> Result<(), Trap> {
        let index = self.container_slot(set, self.set_type_id).ok_or(Trap::TypeError)?;
        if !self.sets[index].iter().any(|e| self.key_eq(*e, value)) {
            self.sets[index].push(value);
        }
        Ok(())
    }

    /// Appends `value` to the list in place -- `list.append` and the `ListAppend` comprehension op.
    pub fn list_append(&mut self, list: Value, value: Value) -> Result<(), Trap> {
        let index = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
        self.seqs[index].push(value);
        Ok(())
    }

    /// Python value equality for container keys/membership over the value subset we have:
    /// `int`/`bool` compare numerically (so `True == 1`), `str` by content, everything else
    /// by identity (`None`, the same object). Enough for `in`, dict keys, and `==` on these.
    fn key_eq(&self, a: Value, b: Value) -> bool {
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            return x == y;
        }
        if let (Some(x), Some(y)) = (self.str_value(a), self.str_value(b)) {
            return x == y;
        }
        a == b
    }

    /// `container[index] = value` (`Op::Setitem`): a `list` stores at an int index (negative
    /// from the end, `IndexError` out of range); a `dict` inserts or updates `index` as the
    /// key. A `tuple`/`str`/other is not assignable (`TypeError`).
    pub fn py_setitem(&mut self, container: Value, index: Value, value: Value) -> Result<(), Trap> {
        if let Some(i) = self.container_slot(container, self.list_type_id) {
            let len = self.seqs[i].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                return Err(Trap::IndexError);
            }
            self.seqs[i][at as usize] = value;
            return Ok(());
        }
        if let Some(i) = self.container_slot(container, self.dict_type_id) {
            match self.dicts[i].iter().position(|(k, _)| self.key_eq(*k, index)) {
                Some(slot) => self.dicts[i][slot].1 = value,
                None => self.dicts[i].push((index, value)),
            }
            return Ok(());
        }
        Err(Trap::TypeError)
    }

    /// `element in container` (`Op::Contains`): substring for `str`, membership for a
    /// `list`/`tuple` (any element equals), key membership for a `dict`.
    pub fn py_contains(&self, container: Value, element: Value) -> Result<bool, Trap> {
        if let Some(s) = self.str_value(container) {
            let sub = self.str_value(element).ok_or(Trap::TypeError)?;
            return Ok(s.contains(sub));
        }
        if let Some(elems) = self.seq_value(container) {
            return Ok(elems.iter().any(|&e| self.key_eq(e, element)));
        }
        if let Some(entries) = self.dict_value(container) {
            return Ok(entries.iter().any(|(k, _)| self.key_eq(*k, element)));
        }
        if let Some(elements) = self.set_value(container) {
            return Ok(elements.iter().any(|&e| self.key_eq(e, element)));
        }
        Err(Trap::TypeError)
    }

    /// The Python `repr()` of `value` over the value subset we have, so a container (and its
    /// elements) prints as CPython does. A top-level `str` is printed raw by `print()`, but a
    /// `str` nested in a container is repr'd (quoted); this is the quoted form.
    #[must_use]
    pub fn repr(&self, value: Value) -> String {
        if value == Value::TRUE {
            return String::from("True");
        }
        if value == Value::FALSE {
            return String::from("False");
        }
        if value.is_none() {
            return String::from("None");
        }
        if let Some(n) = value.as_fixnum() {
            return alloc::format!("{n}");
        }
        if let Some(s) = self.str_value(value) {
            return str_repr(s);
        }
        if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            return if step == 1 {
                alloc::format!("range({start}, {stop})")
            } else {
                alloc::format!("range({start}, {stop}, {step})")
            };
        }
        if let Some(elems) = self.seq_value(value) {
            let is_tuple = self.is_tuple(value);
            let len = elems.len();
            let inner = elems
                .iter()
                .map(|&e| self.repr(e))
                .collect::<Vec<_>>()
                .join(", ");
            return if is_tuple {
                if len == 1 {
                    alloc::format!("({inner},)")
                } else {
                    alloc::format!("({inner})")
                }
            } else {
                alloc::format!("[{inner}]")
            };
        }
        if let Some(elements) = self.set_value(value) {
            let frozen = self.is_frozenset(value);
            if elements.is_empty() {
                return String::from(if frozen { "frozenset()" } else { "set()" });
            }
            let inner = elements
                .iter()
                .map(|&e| self.repr(e))
                .collect::<Vec<_>>()
                .join(", ");
            return if frozen {
                alloc::format!("frozenset({{{inner}}})")
            } else {
                alloc::format!("{{{inner}}}")
            };
        }
        if let Some(entries) = self.dict_value(value) {
            let inner = entries
                .iter()
                .map(|(k, v)| alloc::format!("{}: {}", self.repr(*k), self.repr(*v)))
                .collect::<Vec<_>>()
                .join(", ");
            return alloc::format!("{{{inner}}}");
        }
        alloc::format!("{value:?}")
    }

    /// `str(value)` (the Python builtin): a `str` is returned unchanged; an int/bool/None
    /// render as `print()` shows them; a container uses its `repr`. Allocates a new `str`
    /// (except when `value` is already one).
    pub fn py_str(&mut self, value: Value) -> Result<Value, Trap> {
        if self.is_str(value) {
            return Ok(value);
        }
        let rendered = if let Some(n) = value.as_fixnum() {
            alloc::format!("{n}")
        } else if value == Value::TRUE {
            String::from("True")
        } else if value == Value::FALSE {
            String::from("False")
        } else if value.is_none() {
            String::from("None")
        } else {
            self.repr(value)
        };
        self.new_str(&rendered)
    }

    /// `iter(iterable)` (`Op::GetIter`): an iterator over a `str`/`list`/`tuple`/`dict` (a
    /// dict iterates its keys). A non-iterable value is a `TypeError`.
    pub fn new_iter(&mut self, iterable: Value) -> Result<Value, Trap> {
        if self.is_iter(iterable) {
            return Ok(iterable);
        }
        let iterable_ok = self.str_value(iterable).is_some()
            || self.seq_value(iterable).is_some()
            || self.dict_value(iterable).is_some()
            || self.is_range(iterable)
            || self.set_value(iterable).is_some();
        if !iterable_ok {
            return Err(Trap::TypeError);
        }
        let reference = self.heap.alloc(self.iter_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, iterable.bits());
        self.heap.write_u32(reference.0 + 4, 0);
        Ok(Value::from_ref(reference))
    }

    /// Advances an iterator (`Op::ForIter`): `Some(value)` on the next element, `None` at
    /// exhaustion. The iterator stores its container + position; this reads the position-th
    /// element (a sequence element / a dict key / a 1-char `str`) and advances the position.
    pub fn py_next(&mut self, iterator: Value) -> Result<Option<Value>, Trap> {
        let reference = iterator.as_ref().ok_or(Trap::TypeError)?;
        if self.heap.type_id_of(reference) != self.iter_type_id {
            return Err(Trap::TypeError);
        }
        let container = Value::from_bits(self.heap.read_u32(reference.0));
        let pos = self.heap.read_u32(reference.0 + 4) as usize;
        if self.is_range(container) {
            let (start, stop, step) = self.range_bounds(container);
            if pos as i64 >= range_len(start, stop, step) {
                return Ok(None);
            }
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            let element = start + pos as i64 * step;
            return Ok(Some(Value::fixnum(element as i32).ok_or(Trap::Overflow)?));
        }
        if let Some(elems) = self.seq_value(container) {
            if pos >= elems.len() {
                return Ok(None);
            }
            let element = elems[pos];
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(element));
        }
        if let Some(entries) = self.dict_value(container) {
            if pos >= entries.len() {
                return Ok(None);
            }
            let key = entries[pos].0;
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(key));
        }
        if let Some(elements) = self.set_value(container) {
            if pos >= elements.len() {
                return Ok(None);
            }
            let element = elements[pos];
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(element));
        }
        if self.str_value(container).is_some() {
            let ch = {
                let s = self.str_value(container).ok_or(Trap::TypeError)?;
                match s.chars().nth(pos) {
                    Some(c) => c,
                    None => return Ok(None),
                }
            };
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            let mut buf = [0u8; 4];
            return Ok(Some(self.new_str(ch.encode_utf8(&mut buf))?));
        }
        Err(Trap::TypeError)
    }

    /// Reads tagged slot `i` (4-byte) of a heap object, as a [`Value`]. The caller has
    /// established `value` is the expected heap kind.
    fn read_slot(&self, value: Value, i: u32) -> Value {
        let reference = value.as_ref().expect("a heap object");
        Value::from_bits(self.heap.read_u32(reference.0 + i * 4))
    }

    /// Allocates a class object `[name, base, namespace]` (`Op::BuildClass`). `base` is a
    /// class or `None`; `namespace` is the class body's dict (methods + class attributes).
    pub fn new_class(&mut self, name: Value, base: Value, namespace: Value) -> Result<Value, Trap> {
        let reference = self.heap.alloc(self.class_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, name.bits());
        self.heap.write_u32(reference.0 + 4, base.bits());
        self.heap.write_u32(reference.0 + 8, namespace.bits());
        Ok(Value::from_ref(reference))
    }

    /// Allocates an instance of `class` with a fresh empty `__dict__` (the first half of
    /// calling a type; `__init__` runs in the interpreter's Call arm).
    pub fn new_object(&mut self, class: Value) -> Result<Value, Trap> {
        let dict = self.new_dict(Vec::new())?;
        let reference = self.heap.alloc(self.instance_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, class.bits());
        self.heap.write_u32(reference.0 + 4, dict.bits());
        Ok(Value::from_ref(reference))
    }

    /// Allocates a bound Python method `[receiver, func]` -- the value `LoadAttr` yields for
    /// a function found on an instance's class; `Call` prepends the receiver as `self`.
    pub fn new_py_bound(&mut self, receiver: Value, func: Value) -> Result<Value, Trap> {
        let reference = self.heap.alloc(self.py_bound_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, receiver.bits());
        self.heap.write_u32(reference.0 + 4, func.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a user class object.
    #[must_use]
    pub fn is_class(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.class_type_id)
    }

    /// Whether `value` is a user class instance.
    #[must_use]
    pub fn is_instance(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.instance_type_id)
    }

    /// Whether `value` is a bound Python method.
    #[must_use]
    pub fn is_py_bound(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.py_bound_type_id)
    }

    /// The receiver (`self`) of a bound Python method.
    #[must_use]
    pub fn bound_self(&self, bound: Value) -> Value {
        self.read_slot(bound, 0)
    }

    /// The function of a bound Python method.
    #[must_use]
    pub fn bound_func(&self, bound: Value) -> Value {
        self.read_slot(bound, 1)
    }

    /// Allocates a `super` object `[class, self]` -- the `super()` of a method of `class`.
    pub fn new_super(&mut self, class: Value, receiver: Value) -> Result<Value, Trap> {
        let reference = self.heap.alloc(self.super_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, class.bits());
        self.heap.write_u32(reference.0 + 4, receiver.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `super` object.
    #[must_use]
    pub fn is_super(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.super_type_id)
    }

    /// `super().name`: resolve `name` from the base of the super's class (the MRO after it),
    /// bound to the super's `self` -- a function there binds (single inheritance), a non-function
    /// is returned as-is; otherwise `AttributeError`.
    pub fn py_getattr_super(&mut self, super_obj: Value, name: &str) -> Result<Value, Trap> {
        let class = self.read_slot(super_obj, 0);
        let receiver = self.read_slot(super_obj, 1);
        let base = self.read_slot(class, 1);
        let found = self.find_in_class(base, name).ok_or(Trap::AttributeError)?;
        if found.as_function_index().is_some() {
            self.new_py_bound(receiver, found)
        } else {
            Ok(found)
        }
    }

    /// Looks up the str-keyed `name` in `dict`, or `None`.
    fn dict_lookup_str(&self, dict: Value, name: &str) -> Option<Value> {
        let entries = self.dict_value(dict)?;
        entries
            .iter()
            .find(|(k, _)| self.str_value(*k) == Some(name))
            .map(|(_, v)| *v)
    }

    /// Resolves `name` in `class`'s namespace, then up the base chain; `None` if unbound.
    fn find_in_class(&self, class: Value, name: &str) -> Option<Value> {
        let mut current = class;
        while self.is_class(current) {
            let namespace = self.read_slot(current, 2);
            if let Some(found) = self.dict_lookup_str(namespace, name) {
                return Some(found);
            }
            current = self.read_slot(current, 1);
        }
        None
    }

    /// `instance.name` (`Op::LoadAttr` on a class instance): the instance `__dict__` first
    /// (returned as-is), then the class + base chain -- a function there binds to the
    /// instance (a [`Self::new_py_bound`]), a non-function is a class attribute; otherwise
    /// `AttributeError`.
    pub fn py_getattr_instance(&mut self, instance: Value, name: &str) -> Result<Value, Trap> {
        let dict = self.read_slot(instance, 1);
        if let Some(found) = self.dict_lookup_str(dict, name) {
            return Ok(found);
        }
        let class = self.read_slot(instance, 0);
        if let Some(found) = self.find_in_class(class, name) {
            if found.as_function_index().is_some() {
                return self.new_py_bound(instance, found);
            }
            return Ok(found);
        }
        Err(Trap::AttributeError)
    }

    /// `instance.name = value` (`Op::SetAttr`): stores into the instance `__dict__`.
    pub fn py_setattr_instance(&mut self, instance: Value, name: &str, value: Value) -> Result<(), Trap> {
        let key = self.new_str(name)?;
        let dict = self.read_slot(instance, 1);
        self.py_setitem(dict, key, value)
    }

    /// Resolves `__init__` on `class` (or its bases), if the class defines a constructor.
    #[must_use]
    pub fn find_init(&self, class: Value) -> Option<Value> {
        self.find_in_class(class, "__init__")
    }

    /// The dunder method `name` (e.g. `"__len__"`) bound to `instance`, if its class defines it
    /// as a function; `None` otherwise (or if `instance` is not a class instance). The caller
    /// invokes the returned bound method to run the dunder.
    pub fn find_dunder(&mut self, instance: Value, name: &str) -> Option<Value> {
        if !self.is_instance(instance) {
            return None;
        }
        let class = self.read_slot(instance, 0);
        let found = self.find_in_class(class, name)?;
        if found.as_function_index().is_some() {
            self.new_py_bound(instance, found).ok()
        } else {
            None
        }
    }

    /// Builds the built-in exception class hierarchy on first use (idempotent). Each entry's
    /// base is built before it; `""` is the root's (BaseException's) base.
    fn ensure_exception_types(&mut self) {
        if !self.exception_classes.is_empty() {
            return;
        }
        const HIERARCHY: &[(&str, &str)] = &[
            ("BaseException", ""),
            ("Exception", "BaseException"),
            ("ArithmeticError", "Exception"),
            ("ZeroDivisionError", "ArithmeticError"),
            ("OverflowError", "ArithmeticError"),
            ("LookupError", "Exception"),
            ("IndexError", "LookupError"),
            ("KeyError", "LookupError"),
            ("AttributeError", "Exception"),
            ("NameError", "Exception"),
            ("UnboundLocalError", "NameError"),
            ("TypeError", "Exception"),
            ("ValueError", "Exception"),
            ("RuntimeError", "Exception"),
            ("RecursionError", "RuntimeError"),
            ("StopIteration", "Exception"),
        ];
        for &(name, base_name) in HIERARCHY {
            let name_value = match self.new_str(name) {
                Ok(v) => v,
                Err(_) => return,
            };
            let base = if base_name.is_empty() {
                Value::NONE
            } else {
                self.exc_class_lookup(base_name).unwrap_or(Value::NONE)
            };
            let namespace = match self.new_dict(Vec::new()) {
                Ok(v) => v,
                Err(_) => return,
            };
            if let Ok(class) = self.new_class(name_value, base, namespace) {
                self.exception_classes.push((name, class));
            } else {
                return;
            }
        }
    }

    /// The built-in exception class named `name`, or `None` (assumes the hierarchy is built).
    fn exc_class_lookup(&self, name: &str) -> Option<Value> {
        self.exception_classes
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| *c)
    }

    /// The built-in exception class named `name`, building the hierarchy on first use.
    pub fn exception_class(&mut self, name: &str) -> Option<Value> {
        self.ensure_exception_types();
        self.exc_class_lookup(name)
    }

    /// Whether `exc` (a class instance) is an instance of `target` -- i.e. `target` is on its
    /// class's base chain. The basis for `MatchExc` / `except E`.
    #[must_use]
    pub fn exception_isinstance(&self, exc: Value, target: Value) -> bool {
        if !self.is_instance(exc) {
            return false;
        }
        let mut current = self.read_slot(exc, 0);
        while self.is_class(current) {
            if current == target {
                return true;
            }
            current = self.read_slot(current, 1);
        }
        false
    }

    /// Maps a raised interpreter [`Trap`] to a fresh instance of the matching built-in
    /// exception (so `except IndexError:` catches a real index error); `None` for the
    /// internal/fatal traps, which are not catchable Python exceptions.
    pub fn trap_to_exception(&mut self, trap: Trap) -> Option<Value> {
        let name = match trap {
            Trap::TypeError => "TypeError",
            Trap::AttributeError => "AttributeError",
            Trap::IndexError => "IndexError",
            Trap::KeyError => "KeyError",
            Trap::ValueError => "ValueError",
            Trap::ZeroDivisionError => "ZeroDivisionError",
            Trap::NameError => "NameError",
            Trap::UnboundLocal => "UnboundLocalError",
            Trap::RecursionError => "RecursionError",
            Trap::Overflow => "OverflowError",
            Trap::Raised
            | Trap::StackUnderflow
            | Trap::Unsupported
            | Trap::OutOfMemory
            | Trap::Malformed => {
                return None;
            }
        };
        let class = self.exception_class(name)?;
        self.new_object(class).ok()
    }

    /// Resolves the operand of `raise` (`Op::Raise` argc 1): a class is instantiated no-arg,
    /// an instance is used as-is; the result must derive from `BaseException` (else `TypeError`
    /// -- "exceptions must derive from BaseException").
    pub fn raise_value(&mut self, value: Value) -> Result<Value, Trap> {
        self.ensure_exception_types();
        let base_exception = self.exc_class_lookup("BaseException").ok_or(Trap::Malformed)?;
        let instance = if self.is_class(value) {
            self.new_object(value)?
        } else {
            value
        };
        if self.exception_isinstance(instance, base_exception) {
            Ok(instance)
        } else {
            Err(Trap::TypeError)
        }
    }

    /// Sets the in-flight exception (a `raise`'s object) for the interpreter's exception-table
    /// search to pick up.
    pub fn set_pending_exception(&mut self, exception: Value) {
        self.pending_exception = Some(exception);
    }

    /// Takes the in-flight exception, clearing the slot.
    pub fn take_pending_exception(&mut self) -> Option<Value> {
        self.pending_exception.take()
    }

    /// Binds (or rebinds) the module-global `name`.
    pub fn set_global(&mut self, name: &str, value: Value) {
        if let Some(slot) = self.globals.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value;
        } else {
            self.globals.push((String::from(name), value));
        }
    }

    /// The value bound to module-global `name`, or `None`.
    #[must_use]
    pub fn get_global(&self, name: &str) -> Option<Value> {
        self.globals.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    /// Renders `value` the way `print()` shows it: an int as decimal, a top-level `str` raw, the
    /// singletons by name, a container via its `repr`.
    #[must_use]
    pub fn display(&self, value: Value) -> String {
        if let Some(n) = value.as_fixnum() {
            alloc::format!("{n}")
        } else if value == Value::TRUE {
            String::from("True")
        } else if value == Value::FALSE {
            String::from("False")
        } else if value.is_none() {
            String::from("None")
        } else if let Some(s) = self.str_value(value) {
            String::from(s)
        } else {
            self.repr(value)
        }
    }

    /// Appends a `print()` line (already formatted) plus a newline to the captured output.
    pub fn write_line(&mut self, line: &str) {
        self.stdout.push_str(line);
        self.stdout.push('\n');
    }

    /// Drains the captured `print` output.
    pub fn take_stdout(&mut self) -> String {
        core::mem::take(&mut self.stdout)
    }

    /// Unpacks an iterable into exactly `count` elements (`a, b = x`); a length mismatch is a
    /// `ValueError` ("not enough" / "too many values to unpack"). Works over any iterable.
    pub fn unpack_sequence(&mut self, value: Value, count: usize) -> Result<Vec<Value>, Trap> {
        let iterator = self.new_iter(value)?;
        let mut elements = Vec::new();
        while let Some(element) = self.py_next(iterator)? {
            elements.push(element);
        }
        if elements.len() != count {
            return Err(Trap::ValueError);
        }
        Ok(elements)
    }

    /// Unpacks an iterable for a starred target `a, *b, c = x`: the `before` head elements, then a
    /// LIST of the middle (`len - before - after` elements), then the `after` tail elements, in
    /// target order. Fewer than `before + after` elements is a `ValueError`. Works over any
    /// iterable.
    pub fn unpack_ex(
        &mut self,
        value: Value,
        before: usize,
        after: usize,
    ) -> Result<Vec<Value>, Trap> {
        let iterator = self.new_iter(value)?;
        let mut elements = Vec::new();
        while let Some(element) = self.py_next(iterator)? {
            elements.push(element);
        }
        if elements.len() < before + after {
            return Err(Trap::ValueError);
        }
        let middle_end = elements.len() - after;
        let middle = self.new_list(elements[before..middle_end].to_vec())?;
        let mut targets = Vec::with_capacity(before + 1 + after);
        targets.extend_from_slice(&elements[..before]);
        targets.push(middle);
        targets.extend_from_slice(&elements[middle_end..]);
        Ok(targets)
    }

    /// The class name of an exception instance (`"IndexError"`, ...), for reporting an
    /// uncaught exception; `None` if `exc` is not a class instance with a `str` class name.
    #[must_use]
    pub fn exception_type_name(&self, exc: Value) -> Option<&str> {
        if !self.is_instance(exc) {
            return None;
        }
        let class = self.read_slot(exc, 0);
        if !self.is_class(class) {
            return None;
        }
        self.str_value(self.read_slot(class, 0))
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
        if type_id == self.list_type_id {
            let method_id = list_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.dict_type_id {
            let method_id = dict_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.set_type_id {
            let method_id = set_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.frozenset_type_id {
            let method_id = frozenset_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.tuple_type_id {
            let method_id = tuple_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.class_type_id {
            return self.find_in_class(obj, name).ok_or(Trap::AttributeError);
        }
        if type_id == self.instance_type_id {
            return self.py_getattr_instance(obj, name);
        }
        if type_id == self.super_type_id {
            return self.py_getattr_super(obj, name);
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
        if self.is_list(receiver) {
            return self.call_list_method(receiver, method_id, args);
        }
        if self.is_dict(receiver) {
            return self.call_dict_method(receiver, method_id, args);
        }
        if self.is_set(receiver) || self.is_frozenset(receiver) {
            return self.call_set_method(receiver, method_id, args);
        }
        if self.is_tuple(receiver) {
            return self.call_tuple_method(receiver, method_id, args);
        }
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
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                let test = |affix: &str| {
                    if method_id == STR_STARTSWITH {
                        window.starts_with(affix)
                    } else {
                        window.ends_with(affix)
                    }
                };
                let holds = if let Some(affix) = self.str_value(affix) {
                    test(affix)
                } else if self.is_tuple(affix) {
                    let elems = self.seq_value(affix).ok_or(Trap::TypeError)?;
                    let mut any = false;
                    for &e in elems {
                        if test(self.str_value(e).ok_or(Trap::TypeError)?) {
                            any = true;
                            break;
                        }
                    }
                    any
                } else {
                    return Err(Trap::TypeError);
                };
                Ok(Value::from_bool(holds))
            }
            STR_FIND | STR_RFIND | STR_INDEX | STR_RINDEX => {
                let (sub, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let sub = self.str_value(sub).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                let from_right = method_id == STR_RFIND || method_id == STR_RINDEX;
                let found = if from_right { window.rfind(sub) } else { window.find(sub) };
                let index = match found {
                    Some(byte_offset) => a as i32 + window[..byte_offset].chars().count() as i32,
                    None => -1,
                };
                if index < 0 && (method_id == STR_INDEX || method_id == STR_RINDEX) {
                    return Err(Trap::ValueError);
                }
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
            STR_ISDIGIT | STR_ISALPHA | STR_ISALNUM | STR_ISSPACE | STR_ISUPPER | STR_ISLOWER
            | STR_ISDECIMAL | STR_ISNUMERIC | STR_ISASCII | STR_ISIDENTIFIER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                Ok(Value::from_bool(str_predicate(method_id, s)))
            }
            STR_SPLIT => {
                let (sep, maxsplit) = match args {
                    [] => (None, -1i64),
                    [sep] if sep.is_none() => (None, -1),
                    [sep] => (Some(String::from(self.str_value(*sep).ok_or(Trap::TypeError)?)), -1),
                    [sep, ms] => {
                        let limit = ms.as_int().ok_or(Trap::TypeError)?;
                        let sep = if sep.is_none() {
                            None
                        } else {
                            Some(String::from(self.str_value(*sep).ok_or(Trap::TypeError)?))
                        };
                        (sep, limit)
                    }
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let parts: Vec<String> = match &sep {
                    None => s.split_whitespace().map(String::from).collect(),
                    Some(sep) => {
                        if sep.is_empty() {
                            return Err(Trap::ValueError);
                        }
                        if maxsplit < 0 {
                            s.split(sep.as_str()).map(String::from).collect()
                        } else {
                            s.splitn(maxsplit as usize + 1, sep.as_str())
                                .map(String::from)
                                .collect()
                        }
                    }
                };
                let mut elems = Vec::with_capacity(parts.len());
                for p in &parts {
                    elems.push(self.new_str(p)?);
                }
                self.new_list(elems)
            }
            STR_JOIN => {
                let sep = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let iterable = match args {
                    [it] => *it,
                    _ => return Err(Trap::TypeError),
                };
                let parts: Vec<String> = {
                    let elems = self.seq_value(iterable).ok_or(Trap::TypeError)?;
                    let mut parts = Vec::with_capacity(elems.len());
                    for &e in elems {
                        parts.push(String::from(self.str_value(e).ok_or(Trap::TypeError)?));
                    }
                    parts
                };
                self.new_str(&parts.join(&sep))
            }
            STR_CAPITALIZE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                let mut chars = s.chars();
                if let Some(first) = chars.next() {
                    result.extend(first.to_uppercase());
                    result.extend(chars.flat_map(char::to_lowercase));
                }
                self.new_str(&result)
            }
            STR_TITLE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                let mut prev_cased = false;
                for c in s.chars() {
                    let cased = c.is_alphabetic();
                    if cased && !prev_cased {
                        result.extend(c.to_uppercase());
                    } else if cased {
                        result.extend(c.to_lowercase());
                    } else {
                        result.push(c);
                    }
                    prev_cased = cased;
                }
                self.new_str(&result)
            }
            STR_SWAPCASE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                for c in s.chars() {
                    if c.is_uppercase() {
                        result.extend(c.to_lowercase());
                    } else if c.is_lowercase() {
                        result.extend(c.to_uppercase());
                    } else {
                        result.push(c);
                    }
                }
                self.new_str(&result)
            }
            STR_SPLITLINES => {
                let keepends = match args {
                    [] => false,
                    [k] => self.py_truthy(*k)?.unwrap_or(false),
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut lines: Vec<String> = Vec::new();
                let mut current = String::new();
                let mut chars = s.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '\n' => {
                            if keepends {
                                current.push('\n');
                            }
                            lines.push(core::mem::take(&mut current));
                        }
                        '\r' => {
                            let crlf = chars.peek() == Some(&'\n');
                            if crlf {
                                chars.next();
                            }
                            if keepends {
                                current.push('\r');
                                if crlf {
                                    current.push('\n');
                                }
                            }
                            lines.push(core::mem::take(&mut current));
                        }
                        _ => current.push(c),
                    }
                }
                if !current.is_empty() {
                    lines.push(current);
                }
                let mut elems = Vec::with_capacity(lines.len());
                for line in &lines {
                    elems.push(self.new_str(line)?);
                }
                self.new_list(elems)
            }
            STR_REMOVEPREFIX | STR_REMOVESUFFIX => {
                let [affix] = args else {
                    return Err(Trap::TypeError);
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let affix = String::from(self.str_value(*affix).ok_or(Trap::TypeError)?);
                let result = if method_id == STR_REMOVEPREFIX {
                    s.strip_prefix(&affix).unwrap_or(&s)
                } else {
                    s.strip_suffix(&affix).unwrap_or(&s)
                };
                self.new_str(result)
            }
            STR_ZFILL => {
                let [width] = args else {
                    return Err(Trap::TypeError);
                };
                let width = width.as_int().ok_or(Trap::TypeError)?.max(0) as usize;
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let len = s.chars().count();
                let result = if len >= width {
                    s
                } else {
                    let pad = "0".repeat(width - len);
                    let mut chars = s.chars();
                    if matches!(s.chars().next(), Some('+' | '-')) {
                        let sign = chars.next().unwrap_or('+');
                        let mut r = String::new();
                        r.push(sign);
                        r.push_str(&pad);
                        r.push_str(chars.as_str());
                        r
                    } else {
                        let mut r = pad;
                        r.push_str(&s);
                        r
                    }
                };
                self.new_str(&result)
            }
            STR_LJUST | STR_RJUST | STR_CENTER => {
                let (width, fill) = match args {
                    [w] => (w.as_int().ok_or(Trap::TypeError)?, ' '),
                    [w, f] => {
                        let fs = self.str_value(*f).ok_or(Trap::TypeError)?;
                        let mut fc = fs.chars();
                        match (fc.next(), fc.next()) {
                            (Some(c), None) => (w.as_int().ok_or(Trap::TypeError)?, c),
                            _ => return Err(Trap::TypeError),
                        }
                    }
                    _ => return Err(Trap::TypeError),
                };
                let width = width.max(0) as usize;
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let len = s.chars().count();
                if len >= width {
                    return self.new_str(&s);
                }
                let pad = width - len;
                let (left, right) = match method_id {
                    STR_LJUST => (0, pad),
                    STR_RJUST => (pad, 0),
                    _ => {
                        let left = pad / 2 + (pad & width & 1);
                        (left, pad - left)
                    }
                };
                let mut result = String::new();
                for _ in 0..left {
                    result.push(fill);
                }
                result.push_str(&s);
                for _ in 0..right {
                    result.push(fill);
                }
                self.new_str(&result)
            }
            STR_PARTITION | STR_RPARTITION => {
                let [sep_val] = args else {
                    return Err(Trap::TypeError);
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let sep = String::from(self.str_value(*sep_val).ok_or(Trap::TypeError)?);
                if sep.is_empty() {
                    return Err(Trap::ValueError);
                }
                let split = if method_id == STR_PARTITION {
                    s.find(&sep)
                } else {
                    s.rfind(&sep)
                };
                let (before, mid, after) = match split {
                    Some(byte) => (&s[..byte], sep.as_str(), &s[byte + sep.len()..]),
                    None if method_id == STR_PARTITION => (s.as_str(), "", ""),
                    None => ("", "", s.as_str()),
                };
                let parts = alloc::vec![
                    self.new_str(before)?,
                    self.new_str(mid)?,
                    self.new_str(after)?,
                ];
                self.new_tuple(parts)
            }
            STR_EXPANDTABS => {
                let tabsize = match args {
                    [] => 8,
                    [t] => t.as_int().ok_or(Trap::TypeError)?,
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                let mut column: i64 = 0;
                for c in s.chars() {
                    match c {
                        '\t' => {
                            if tabsize > 0 {
                                let spaces = tabsize - (column % tabsize);
                                for _ in 0..spaces {
                                    result.push(' ');
                                }
                                column += spaces;
                            }
                        }
                        '\n' | '\r' => {
                            result.push(c);
                            column = 0;
                        }
                        _ => {
                            result.push(c);
                            column += 1;
                        }
                    }
                }
                self.new_str(&result)
            }
            _ => Err(Trap::Malformed),
        }
    }

    /// Dispatches a `list` method: `append(x)` (-> None), `pop([i])` (-> the removed element,
    /// default last, `IndexError` on empty / out of range).
    fn call_list_method(&mut self, list: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
        match method_id {
            LIST_APPEND => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                self.seqs[index].push(*value);
                Ok(Value::NONE)
            }
            LIST_POP => {
                let len = self.seqs[index].len();
                if len == 0 {
                    return Err(Trap::IndexError);
                }
                let at = match args {
                    [] => len - 1,
                    [idx] => {
                        let i = idx.as_int().ok_or(Trap::TypeError)?;
                        let i = if i < 0 { i + len as i64 } else { i };
                        if i < 0 || i >= len as i64 {
                            return Err(Trap::IndexError);
                        }
                        i as usize
                    }
                    _ => return Err(Trap::TypeError),
                };
                Ok(self.seqs[index].remove(at))
            }
            LIST_SORT => {
                let mut elements = core::mem::take(&mut self.seqs[index]);
                let outcome = self.sort_values(&mut elements);
                self.seqs[index] = elements;
                outcome.map(|()| Value::NONE)
            }
            LIST_REVERSE => {
                self.seqs[index].reverse();
                Ok(Value::NONE)
            }
            LIST_INSERT => {
                let [at, value] = args else {
                    return Err(Trap::TypeError);
                };
                let len = self.seqs[index].len() as i64;
                let mut i = at.as_int().ok_or(Trap::TypeError)?;
                if i < 0 {
                    i = (i + len).max(0);
                }
                let pos = i.min(len) as usize;
                self.seqs[index].insert(pos, *value);
                Ok(Value::NONE)
            }
            LIST_REMOVE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                match self.seqs[index].iter().position(|e| self.key_eq(*e, *value)) {
                    Some(p) => {
                        self.seqs[index].remove(p);
                        Ok(Value::NONE)
                    }
                    None => Err(Trap::ValueError),
                }
            }
            LIST_INDEX => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                match self.seqs[index].iter().position(|e| self.key_eq(*e, *value)) {
                    Some(p) => Value::fixnum(p as i32).ok_or(Trap::Overflow),
                    None => Err(Trap::ValueError),
                }
            }
            LIST_COUNT => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let n = self.seqs[index]
                    .iter()
                    .filter(|e| self.key_eq(**e, *value))
                    .count();
                Value::fixnum(n as i32).ok_or(Trap::Overflow)
            }
            LIST_EXTEND => {
                let [iterable] = args else {
                    return Err(Trap::TypeError);
                };
                let iterator = self.new_iter(*iterable)?;
                let mut items = Vec::new();
                while let Some(item) = self.py_next(iterator)? {
                    items.push(item);
                }
                self.seqs[index].extend(items);
                Ok(Value::NONE)
            }
            LIST_CLEAR => {
                self.seqs[index].clear();
                Ok(Value::NONE)
            }
            LIST_COPY => {
                let copy = self.seqs[index].clone();
                self.new_list(copy)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `dict` method: `get(k[, default])` (no KeyError), `keys`/`values`/`items`.
    /// keys/values/items return a new `list` (a cut: CPython returns live views; iteration and
    /// `list(...)` over them match).
    fn call_dict_method(&mut self, dict: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.container_slot(dict, self.dict_type_id).ok_or(Trap::TypeError)?;
        match method_id {
            DICT_GET => {
                let (key, default) = match args {
                    [k] => (*k, Value::NONE),
                    [k, d] => (*k, *d),
                    _ => return Err(Trap::TypeError),
                };
                let found = self.dicts[index]
                    .iter()
                    .find(|(k, _)| self.key_eq(*k, key))
                    .map(|(_, v)| *v);
                Ok(found.unwrap_or(default))
            }
            DICT_KEYS => {
                let keys: Vec<Value> = self.dicts[index].iter().map(|(k, _)| *k).collect();
                self.new_list(keys)
            }
            DICT_VALUES => {
                let values: Vec<Value> = self.dicts[index].iter().map(|(_, v)| *v).collect();
                self.new_list(values)
            }
            DICT_ITEMS => {
                let pairs = self.dicts[index].clone();
                let mut items = Vec::with_capacity(pairs.len());
                for (key, value) in pairs {
                    items.push(self.new_tuple(alloc::vec![key, value])?);
                }
                self.new_list(items)
            }
            DICT_UPDATE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let pairs = if let Some(entries) = self.dict_entries(*other) {
                    entries
                } else {
                    let iterator = self.new_iter(*other)?;
                    let mut kv = Vec::new();
                    while let Some(pair) = self.py_next(iterator)? {
                        let parts = self.unpack_sequence(pair, 2)?;
                        kv.push((parts[0], parts[1]));
                    }
                    kv
                };
                for (key, value) in pairs {
                    match self.dicts[index].iter().position(|(k, _)| self.key_eq(*k, key)) {
                        Some(slot) => self.dicts[index][slot].1 = value,
                        None => self.dicts[index].push((key, value)),
                    }
                }
                Ok(Value::NONE)
            }
            DICT_POP => {
                let (key, default) = match args {
                    [k] => (*k, None),
                    [k, d] => (*k, Some(*d)),
                    _ => return Err(Trap::TypeError),
                };
                match self.dicts[index].iter().position(|(k, _)| self.key_eq(*k, key)) {
                    Some(slot) => Ok(self.dicts[index].remove(slot).1),
                    None => default.ok_or(Trap::KeyError),
                }
            }
            DICT_SETDEFAULT => {
                let (key, default) = match args {
                    [k] => (*k, Value::NONE),
                    [k, d] => (*k, *d),
                    _ => return Err(Trap::TypeError),
                };
                match self.dicts[index].iter().position(|(k, _)| self.key_eq(*k, key)) {
                    Some(slot) => Ok(self.dicts[index][slot].1),
                    None => {
                        self.dicts[index].push((key, default));
                        Ok(default)
                    }
                }
            }
            DICT_CLEAR => {
                self.dicts[index].clear();
                Ok(Value::NONE)
            }
            DICT_COPY => {
                let copy = self.dicts[index].clone();
                self.new_dict(copy)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Collects any iterable into an owned `Vec` (a set/frozenset or list/tuple is cloned, else
    /// the iterator protocol drives it) -- the argument side of the set operations.
    fn collect_elements(&mut self, value: Value) -> Result<Vec<Value>, Trap> {
        if let Some(elems) = self.set_value(value) {
            return Ok(elems.clone());
        }
        if let Some(elems) = self.seq_value(value) {
            return Ok(elems.clone());
        }
        let iterator = self.new_iter(value)?;
        let mut elems = Vec::new();
        while let Some(item) = self.py_next(iterator)? {
            elems.push(item);
        }
        Ok(elems)
    }

    /// The union of `a` and `b`: `a`'s elements, then `b`'s new ones.
    fn set_union_elems(&self, a: &[Value], b: &[Value]) -> Vec<Value> {
        let mut result = a.to_vec();
        for &e in b {
            if !result.iter().any(|x| self.key_eq(*x, e)) {
                result.push(e);
            }
        }
        result
    }

    /// The elements of `a` that are (intersection) / are not (difference) also in `b`.
    fn set_filter_elems(&self, a: &[Value], b: &[Value], keep_common: bool) -> Vec<Value> {
        a.iter()
            .copied()
            .filter(|&x| b.iter().any(|&y| self.key_eq(x, y)) == keep_common)
            .collect()
    }

    /// Whether every element of `a` is in `b` (`a` is a subset of `b`).
    fn set_subset(&self, a: &[Value], b: &[Value]) -> bool {
        a.iter().all(|&x| b.iter().any(|&y| self.key_eq(x, y)))
    }

    /// Whether `a` and `b` share no element.
    fn set_disjoint(&self, a: &[Value], b: &[Value]) -> bool {
        !a.iter().any(|&x| b.iter().any(|&y| self.key_eq(x, y)))
    }

    /// Dispatches a `tuple` method: `index(x)` (the first position, `ValueError` if absent) and
    /// `count(x)` -- the immutable sequence reads over the shared arena.
    fn call_tuple_method(&mut self, tuple: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.container_slot(tuple, self.tuple_type_id).ok_or(Trap::TypeError)?;
        let [value] = args else {
            return Err(Trap::TypeError);
        };
        match method_id {
            TUPLE_INDEX => match self.seqs[index].iter().position(|e| self.key_eq(*e, *value)) {
                Some(p) => Value::fixnum(p as i32).ok_or(Trap::Overflow),
                None => Err(Trap::ValueError),
            },
            TUPLE_COUNT => {
                let n = self.seqs[index].iter().filter(|e| self.key_eq(**e, *value)).count();
                Value::fixnum(n as i32).ok_or(Trap::Overflow)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `set`/`frozenset` method. The algebra (union/intersection/difference/
    /// symmetric_difference) returns a NEW set of the receiver's kind; the predicates
    /// (issubset/issuperset/isdisjoint) a bool; the mutators (add/discard/remove/clear/pop/
    /// update) act in place (only a mutable set reaches them). An argument is any iterable.
    fn call_set_method(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        let frozen = self.is_frozenset(receiver);
        match method_id {
            SET_COPY => {
                let elems = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                if frozen {
                    self.new_frozenset(elems)
                } else {
                    self.new_set(elems)
                }
            }
            SET_UNION | SET_INTERSECTION | SET_DIFFERENCE | SET_SYMMETRIC_DIFFERENCE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let result = match method_id {
                    SET_UNION => self.set_union_elems(&a, &b),
                    SET_INTERSECTION => self.set_filter_elems(&a, &b, true),
                    SET_DIFFERENCE => self.set_filter_elems(&a, &b, false),
                    _ => {
                        let mut r = self.set_filter_elems(&a, &b, false);
                        r.extend(self.set_filter_elems(&b, &a, false));
                        r
                    }
                };
                if frozen {
                    self.new_frozenset(result)
                } else {
                    self.new_set(result)
                }
            }
            SET_ISSUBSET | SET_ISSUPERSET | SET_ISDISJOINT => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let result = match method_id {
                    SET_ISSUBSET => self.set_subset(&a, &b),
                    SET_ISSUPERSET => self.set_subset(&b, &a),
                    _ => self.set_disjoint(&a, &b),
                };
                Ok(Value::from_bool(result))
            }
            SET_ADD => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                self.set_add(receiver, *value)?;
                Ok(Value::NONE)
            }
            SET_DISCARD | SET_REMOVE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                match self.sets[slot].iter().position(|e| self.key_eq(*e, *value)) {
                    Some(p) => {
                        self.sets[slot].remove(p);
                        Ok(Value::NONE)
                    }
                    None if method_id == SET_REMOVE => Err(Trap::KeyError),
                    None => Ok(Value::NONE),
                }
            }
            SET_CLEAR => {
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                self.sets[slot].clear();
                Ok(Value::NONE)
            }
            SET_POP => {
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                if self.sets[slot].is_empty() {
                    return Err(Trap::KeyError);
                }
                Ok(self.sets[slot].remove(0))
            }
            SET_UPDATE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let b = self.collect_elements(*other)?;
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                for e in b {
                    if !self.sets[slot].iter().any(|x| self.key_eq(*x, e)) {
                        self.sets[slot].push(e);
                    }
                }
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// `set <op> set` for the `| & - ^` operators (both operands must be sets/frozensets); the
    /// result takes the LEFT operand's kind.
    pub(crate) fn set_binary_op(&mut self, op: BinOp, a: Value, b: Value) -> Result<Value, Trap> {
        let a_elems = self.set_value(a).ok_or(Trap::TypeError)?.clone();
        let b_elems = self.set_value(b).ok_or(Trap::TypeError)?.clone();
        let result = match op {
            BinOp::BitOr => self.set_union_elems(&a_elems, &b_elems),
            BinOp::BitAnd => self.set_filter_elems(&a_elems, &b_elems, true),
            BinOp::Sub => self.set_filter_elems(&a_elems, &b_elems, false),
            BinOp::BitXor => {
                let mut r = self.set_filter_elems(&a_elems, &b_elems, false);
                r.extend(self.set_filter_elems(&b_elems, &a_elems, false));
                r
            }
            _ => return Err(Trap::TypeError),
        };
        if self.is_frozenset(a) {
            self.new_frozenset(result)
        } else {
            self.new_set(result)
        }
    }

    /// `set <cmp> other`: == / != by element equality (a non-set `other` simply compares unequal,
    /// not an error); < <= > >= are (proper) subset/superset and require `other` to be a set.
    pub(crate) fn set_compare(&self, op: CmpOp, a: Value, b: Value) -> Result<Value, Trap> {
        let a_elems = self.set_value(a).ok_or(Trap::TypeError)?;
        let b_set = self.set_value(b);
        let value = match op {
            CmpOp::Eq | CmpOp::Ne => {
                let equal = match b_set {
                    Some(b_elems) => {
                        self.set_subset(a_elems, b_elems) && self.set_subset(b_elems, a_elems)
                    }
                    None => false,
                };
                if matches!(op, CmpOp::Ne) {
                    !equal
                } else {
                    equal
                }
            }
            CmpOp::Le => self.set_subset(a_elems, b_set.ok_or(Trap::TypeError)?),
            CmpOp::Ge => self.set_subset(b_set.ok_or(Trap::TypeError)?, a_elems),
            CmpOp::Lt => {
                let b_elems = b_set.ok_or(Trap::TypeError)?;
                self.set_subset(a_elems, b_elems) && !self.set_subset(b_elems, a_elems)
            }
            CmpOp::Gt => {
                let b_elems = b_set.ok_or(Trap::TypeError)?;
                self.set_subset(b_elems, a_elems) && !self.set_subset(a_elems, b_elems)
            }
        };
        Ok(Value::from_bool(value))
    }

    /// Sorts `elements` in place by Python ordering: all-int numerically, all-str
    /// lexicographically; a mixed or otherwise unorderable set is a `TypeError`. Shared by
    /// `list.sort` and the `sorted` built-in.
    pub(crate) fn sort_values(&self, elements: &mut [Value]) -> Result<(), Trap> {
        if elements.iter().all(|e| e.as_int().is_some()) {
            elements.sort_by_key(|e| e.as_int().unwrap_or(0));
        } else if elements.iter().all(|e| self.str_value(*e).is_some()) {
            let mut keyed: Vec<(String, Value)> = elements
                .iter()
                .map(|e| (String::from(self.str_value(*e).unwrap_or("")), *e))
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            for (slot, (_, value)) in keyed.into_iter().enumerate() {
                elements[slot] = value;
            }
        } else {
            return Err(Trap::TypeError);
        }
        Ok(())
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
        assert_eq!(model.py_truthy(one).unwrap(), Some(true));
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
    fn list_tuple_dict_basics() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();

        let list = model.new_list(alloc::vec![n(10), n(20), n(30)]).unwrap();
        assert!(model.is_list(list));
        assert_eq!(model.py_len(list).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.py_getitem(list, n(0)).unwrap().as_fixnum(), Some(10));
        assert_eq!(model.py_getitem(list, n(-1)).unwrap().as_fixnum(), Some(30));
        assert_eq!(model.py_getitem(list, n(5)), Err(Trap::IndexError));
        model.py_setitem(list, n(1), n(99)).unwrap();
        assert_eq!(model.py_getitem(list, n(1)).unwrap().as_fixnum(), Some(99));
        assert!(model.py_contains(list, n(99)).unwrap());
        assert!(!model.py_contains(list, n(7)).unwrap());

        let tup = model.new_tuple(alloc::vec![n(1), n(2)]).unwrap();
        assert!(model.is_tuple(tup));
        assert_eq!(model.py_getitem(tup, n(1)).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.py_setitem(tup, n(0), n(5)), Err(Trap::TypeError));

        let dict = model.new_dict(alloc::vec![(n(1), n(10)), (n(2), n(20))]).unwrap();
        assert!(model.is_dict(dict));
        assert_eq!(model.py_len(dict).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.py_getitem(dict, n(1)).unwrap().as_fixnum(), Some(10));
        assert_eq!(model.py_getitem(dict, n(9)), Err(Trap::KeyError));
        assert!(model.py_contains(dict, n(2)).unwrap());
        model.py_setitem(dict, n(3), n(30)).unwrap();
        assert_eq!(model.py_getitem(dict, n(3)).unwrap().as_fixnum(), Some(30));
        model.py_setitem(dict, n(1), n(11)).unwrap();
        assert_eq!(model.py_getitem(dict, n(1)).unwrap().as_fixnum(), Some(11));
        let dup = model.new_dict(alloc::vec![(n(1), n(1)), (n(1), n(2))]).unwrap();
        assert_eq!(model.py_len(dup).unwrap().as_fixnum(), Some(1));
        assert_eq!(model.py_getitem(dup, n(1)).unwrap().as_fixnum(), Some(2));
    }

    #[test]
    fn iteration_over_containers() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let list = model.new_list(alloc::vec![n(10), n(20)]).unwrap();
        let it = model.new_iter(list).unwrap();
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(10));
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(20));
        assert_eq!(model.py_next(it).unwrap(), None);
        assert_eq!(model.py_next(it).unwrap(), None);
        let s = model.new_str("hi").unwrap();
        let it = model.new_iter(s).unwrap();
        let c0 = model.py_next(it).unwrap().unwrap();
        assert_eq!(model.str_value(c0), Some("h"));
        let d = model.new_dict(alloc::vec![(n(5), n(50)), (n(6), n(60))]).unwrap();
        let it = model.new_iter(d).unwrap();
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(5));
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(6));
        assert_eq!(model.py_next(it).unwrap(), None);
        assert_eq!(model.new_iter(n(3)), Err(Trap::TypeError));
    }

    #[test]
    fn str_split_and_tuple_affixes() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let s = model.new_str("a,b,c").unwrap();
        let comma = model.new_str(",").unwrap();
        let m = model.getattr(s, "split", &mut InlineCache::empty()).unwrap();
        let parts = model.call_bound_method(m, &[comma]).unwrap();
        assert!(model.is_list(parts));
        assert_eq!(model.py_len(parts).unwrap().as_fixnum(), Some(3));
        let abc = model.new_str("abc").unwrap();
        let empty = model.new_str("").unwrap();
        let m = model.getattr(abc, "split", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(m, &[empty]), Err(Trap::ValueError));
        let hello = model.new_str("hello").unwrap();
        let he = model.new_str("he").unwrap();
        let xy = model.new_str("xy").unwrap();
        let affixes = model.new_tuple(alloc::vec![xy, he]).unwrap();
        let m = model.getattr(hello, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(m, &[affixes]).unwrap(), Value::TRUE);
        let list_affix = model.new_list(alloc::vec![he]).unwrap();
        let m = model.getattr(hello, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(m, &[list_affix]), Err(Trap::TypeError));
    }

    #[test]
    fn py_str_renders_like_cpython() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n123 = model.py_str(Value::fixnum(123).unwrap()).unwrap();
        assert_eq!(model.str_value(n123), Some("123"));
        let t = model.py_str(Value::TRUE).unwrap();
        assert_eq!(model.str_value(t), Some("True"));
        let none = model.py_str(Value::NONE).unwrap();
        assert_eq!(model.str_value(none), Some("None"));
        let hello = model.new_str("hello").unwrap();
        assert_eq!(model.py_str(hello).unwrap(), hello);
        let list = model
            .new_list(alloc::vec![Value::fixnum(1).unwrap(), Value::fixnum(2).unwrap()])
            .unwrap();
        let rendered = model.py_str(list).unwrap();
        assert_eq!(model.str_value(rendered), Some("[1, 2]"));
    }

    #[test]
    fn classes_substrate() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let name_a = model.new_str("A").unwrap();
        let key_m = model.new_str("m").unwrap();
        let key_k = model.new_str("k").unwrap();
        let ns_a = model
            .new_dict(alloc::vec![(key_m, Value::function_ref(0)), (key_k, n(10))])
            .unwrap();
        let class_a = model.new_class(name_a, Value::NONE, ns_a).unwrap();
        assert!(model.is_class(class_a));
        let obj = model.new_object(class_a).unwrap();
        assert!(model.is_instance(obj));

        assert_eq!(model.py_getattr_instance(obj, "k").unwrap().as_fixnum(), Some(10));
        let bound = model.py_getattr_instance(obj, "m").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_self(bound), obj);
        assert_eq!(model.bound_func(bound).as_function_index(), Some(0));
        assert_eq!(model.py_getattr_instance(obj, "nope"), Err(Trap::AttributeError));

        model.py_setattr_instance(obj, "x", n(42)).unwrap();
        assert_eq!(model.py_getattr_instance(obj, "x").unwrap().as_fixnum(), Some(42));
        model.py_setattr_instance(obj, "k", n(99)).unwrap();
        assert_eq!(model.py_getattr_instance(obj, "k").unwrap().as_fixnum(), Some(99));
        assert!(model.find_init(class_a).is_none());

        let name_b = model.new_str("B").unwrap();
        let key_init = model.new_str("__init__").unwrap();
        let ns_b = model
            .new_dict(alloc::vec![(key_init, Value::function_ref(1))])
            .unwrap();
        let class_b = model.new_class(name_b, class_a, ns_b).unwrap();
        let obj_b = model.new_object(class_b).unwrap();
        let bound_b = model.py_getattr_instance(obj_b, "m").unwrap();
        assert!(model.is_py_bound(bound_b));
        assert_eq!(model.bound_func(bound_b).as_function_index(), Some(0));
        assert_eq!(model.py_getattr_instance(obj_b, "k").unwrap().as_fixnum(), Some(10));
        assert_eq!(model.find_init(class_b).unwrap().as_function_index(), Some(1));
    }

    #[test]
    fn str_predicates_match_cpython_at_the_unicode_edges() {
        assert!(str_predicate(STR_ISALPHA, "café"));
        assert!(!str_predicate(STR_ISALPHA, "a1"));
        assert!(!str_predicate(STR_ISALPHA, "\u{0345}"));
        assert!(str_predicate(STR_ISDIGIT, "\u{00b2}"));
        assert!(!str_predicate(STR_ISDECIMAL, "\u{00b2}"));
        assert!(str_predicate(STR_ISDECIMAL, "123"));
        assert!(str_predicate(STR_ISNUMERIC, "\u{00bd}"));
        assert!(!str_predicate(STR_ISDIGIT, "\u{00bd}"));
        assert!(str_predicate(STR_ISNUMERIC, "\u{4e00}"));
        assert!(str_predicate(STR_ISSPACE, "\u{001c}"));
        assert!(!str_predicate(STR_ISSPACE, ""));
        assert!(str_predicate(STR_ISUPPER, "ABC"));
        assert!(!str_predicate(STR_ISUPPER, "A\u{01c5}"));
        assert!(str_predicate(STR_ISLOWER, "abc"));
        assert!(!str_predicate(STR_ISLOWER, "a\u{01c5}"));
    }

    #[test]
    fn sequence_slicing_and_join() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let list = model.new_list(alloc::vec![n(1), n(2), n(3), n(4), n(5)]).unwrap();
        let sl = model.new_slice(n(1), n(4), Value::NONE).unwrap();
        let r = model.py_getitem(list, sl).unwrap();
        assert!(model.is_list(r));
        assert_eq!(model.py_len(r).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.py_getitem(r, n(0)).unwrap().as_fixnum(), Some(2));
        let tup = model.new_tuple(alloc::vec![n(7), n(8), n(9)]).unwrap();
        let sl2 = model.new_slice(n(1), Value::NONE, Value::NONE).unwrap();
        let rt = model.py_getitem(tup, sl2).unwrap();
        assert!(model.is_tuple(rt));
        assert_eq!(model.py_len(rt).unwrap().as_fixnum(), Some(2));
        let sep = model.new_str(", ").unwrap();
        let a = model.new_str("a").unwrap();
        let b = model.new_str("b").unwrap();
        let items = model.new_list(alloc::vec![a, b]).unwrap();
        let join = model.getattr(sep, "join", &mut InlineCache::empty()).unwrap();
        let joined = model.call_bound_method(join, &[items]).unwrap();
        assert_eq!(model.str_value(joined), Some("a, b"));
    }

    #[test]
    fn exception_hierarchy_isinstance_and_trap_mapping() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let index_error = model.exception_class("IndexError").unwrap();
        let lookup_error = model.exception_class("LookupError").unwrap();
        let exception = model.exception_class("Exception").unwrap();
        let base = model.exception_class("BaseException").unwrap();
        let value_error = model.exception_class("ValueError").unwrap();
        assert!(model.is_class(index_error));

        let exc = model.new_object(index_error).unwrap();
        assert!(model.exception_isinstance(exc, index_error));
        assert!(model.exception_isinstance(exc, lookup_error));
        assert!(model.exception_isinstance(exc, exception));
        assert!(model.exception_isinstance(exc, base));
        assert!(!model.exception_isinstance(exc, value_error));

        let from_trap = model.trap_to_exception(Trap::KeyError).unwrap();
        let key_error = model.exception_class("KeyError").unwrap();
        assert!(model.exception_isinstance(from_trap, key_error));
        assert!(model.exception_isinstance(from_trap, lookup_error));
        assert!(model.trap_to_exception(Trap::Malformed).is_none());

        let raised = model.raise_value(value_error).unwrap();
        assert!(model.exception_isinstance(raised, value_error));
        assert_eq!(model.raise_value(Value::fixnum(5).unwrap()), Err(Trap::TypeError));
    }

    #[test]
    fn list_and_dict_methods() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let empty_cache = || InlineCache::empty();

        let list = model.new_list(alloc::vec![n(1), n(2)]).unwrap();
        let append = model.getattr(list, "append", &mut empty_cache()).unwrap();
        model.call_bound_method(append, &[n(3)]).unwrap();
        assert_eq!(model.py_len(list).unwrap().as_fixnum(), Some(3));
        let pop = model.getattr(list, "pop", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(pop, &[]).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.py_len(list).unwrap().as_fixnum(), Some(2));
        let empty = model.new_list(Vec::new()).unwrap();
        let pop_e = model.getattr(empty, "pop", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(pop_e, &[]), Err(Trap::IndexError));

        let dict = model.new_dict(alloc::vec![(n(1), n(10))]).unwrap();
        let get = model.getattr(dict, "get", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(get, &[n(1)]).unwrap().as_fixnum(), Some(10));
        let get2 = model.getattr(dict, "get", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(get2, &[n(9), n(99)]).unwrap().as_fixnum(), Some(99));
        let keys = model.getattr(dict, "keys", &mut empty_cache()).unwrap();
        let key_list = model.call_bound_method(keys, &[]).unwrap();
        assert!(model.is_list(key_list));
        assert_eq!(model.py_len(key_list).unwrap().as_fixnum(), Some(1));

        assert_eq!(
            model.getattr(list, "nope", &mut empty_cache()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn range_object() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let r = model.new_range(2, 10, 2).unwrap();
        assert!(model.is_range(r));
        assert_eq!(model.py_len(r).unwrap().as_fixnum(), Some(4));
        assert_eq!(model.py_getitem(r, n(0)).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.py_getitem(r, n(3)).unwrap().as_fixnum(), Some(8));
        assert_eq!(model.py_getitem(r, n(-1)).unwrap().as_fixnum(), Some(8));
        assert_eq!(model.py_getitem(r, n(4)), Err(Trap::IndexError));
        let it = model.new_iter(r).unwrap();
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(2));
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(4));
        let empty = model.new_range(5, 5, 1).unwrap();
        assert_eq!(model.py_len(empty).unwrap().as_fixnum(), Some(0));
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
        assert_eq!(model.py_truthy(r).unwrap(), Some(true));
    }

    #[test]
    fn set_object() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let s = model.new_set(alloc::vec![n(1), n(2), n(2), n(3), n(1)]).unwrap();
        assert!(model.is_set(s));
        assert_eq!(model.py_len(s).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.repr(s), "{1, 2, 3}");
        assert!(model.py_contains(s, n(2)).unwrap());
        assert!(!model.py_contains(s, n(5)).unwrap());
        assert_eq!(model.py_truthy(s).unwrap(), Some(true));
        model.set_add(s, n(2)).unwrap();
        model.set_add(s, n(4)).unwrap();
        assert_eq!(model.py_len(s).unwrap().as_fixnum(), Some(4));
        let empty = model.new_set(Vec::new()).unwrap();
        assert_eq!(model.repr(empty), "set()");
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
    }

    #[test]
    fn frozenset_object() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let fs = model.new_frozenset(alloc::vec![n(1), n(2), n(2), n(3)]).unwrap();
        assert!(model.is_frozenset(fs));
        assert!(!model.is_set(fs));
        assert_eq!(model.py_len(fs).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.repr(fs), "frozenset({1, 2, 3})");
        assert!(model.py_contains(fs, n(2)).unwrap());
        assert!(!model.py_contains(fs, n(7)).unwrap());
        assert_eq!(model.py_truthy(fs).unwrap(), Some(true));
        assert_eq!(model.set_add(fs, n(9)), Err(Trap::TypeError));
        let empty = model.new_frozenset(Vec::new()).unwrap();
        assert_eq!(model.repr(empty), "frozenset()");
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
    }

    #[test]
    fn set_algebra() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let a = model.new_set(alloc::vec![n(1), n(2), n(3)]).unwrap();
        let b = model.new_set(alloc::vec![n(3), n(4), n(5)]).unwrap();
        let union = model.set_binary_op(BinOp::BitOr, a, b).unwrap();
        assert_eq!(model.py_len(union).unwrap().as_fixnum(), Some(5));
        let inter = model.set_binary_op(BinOp::BitAnd, a, b).unwrap();
        assert_eq!(model.py_len(inter).unwrap().as_fixnum(), Some(1));
        assert!(model.py_contains(inter, n(3)).unwrap());
        let diff = model.set_binary_op(BinOp::Sub, a, b).unwrap();
        assert_eq!(model.py_len(diff).unwrap().as_fixnum(), Some(2));
        let symdiff = model.set_binary_op(BinOp::BitXor, a, b).unwrap();
        assert_eq!(model.py_len(symdiff).unwrap().as_fixnum(), Some(4));
        let a2 = model.new_set(alloc::vec![n(3), n(2), n(1)]).unwrap();
        assert_eq!(model.set_compare(CmpOp::Eq, a, a2).unwrap(), Value::TRUE);
        let one = model.new_set(alloc::vec![n(1)]).unwrap();
        assert_eq!(model.set_compare(CmpOp::Lt, one, a).unwrap(), Value::TRUE);
        assert_eq!(model.set_compare(CmpOp::Lt, a, a2).unwrap(), Value::FALSE);
        let list = model.new_list(alloc::vec![n(1), n(2), n(3)]).unwrap();
        assert_eq!(model.set_compare(CmpOp::Eq, a, list).unwrap(), Value::FALSE);
        let fa = model.new_frozenset(alloc::vec![n(1)]).unwrap();
        let fu = model.set_binary_op(BinOp::BitOr, fa, b).unwrap();
        assert!(model.is_frozenset(fu));
        assert_eq!(model.set_binary_op(BinOp::Add, a, b), Err(Trap::TypeError));
        assert_eq!(model.set_compare(CmpOp::Lt, a, list), Err(Trap::TypeError));
    }

    #[test]
    fn super_resolves_base_method() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let m_key = model.new_str("m").unwrap();
        let base_name = model.new_str("Base").unwrap();
        let base_ns = model.new_dict(alloc::vec![(m_key, Value::function_ref(0))]).unwrap();
        let base = model.new_class(base_name, Value::NONE, base_ns).unwrap();
        let der_name = model.new_str("Derived").unwrap();
        let der_ns = model.new_dict(alloc::vec![(m_key, Value::function_ref(1))]).unwrap();
        let derived = model.new_class(der_name, base, der_ns).unwrap();
        let instance = model.new_object(derived).unwrap();
        let sup = model.new_super(derived, instance).unwrap();
        assert!(model.is_super(sup));
        let bound = model.py_getattr_super(sup, "m").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_func(bound).as_function_index(), Some(0));
        assert_eq!(model.bound_self(bound), instance);
    }

    #[test]
    fn find_dunder_resolves_class_methods() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let name = model.new_str("C").unwrap();
        let key = model.new_str("__len__").unwrap();
        let ns = model
            .new_dict(alloc::vec![(key, Value::function_ref(0))])
            .unwrap();
        let class = model.new_class(name, Value::NONE, ns).unwrap();
        let obj = model.new_object(class).unwrap();
        let bound = model.find_dunder(obj, "__len__").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_self(bound), obj);
        assert_eq!(model.bound_func(bound).as_function_index(), Some(0));
        assert!(model.find_dunder(obj, "__str__").is_none());
        assert!(model.find_dunder(Value::fixnum(5).unwrap(), "__len__").is_none());
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
