//! The dynamic object model and its intrinsics.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use core::cmp::Ordering;

use lamella_gc::{Heap, Ref, TypeDesc};
use lamella_py_bytecode::{BinOp, CmpOp, CodeObject, Module, UnaryOp};

use crate::bigint::BigInt;
use crate::builtins::Builtin;
use crate::interp::Frame;
use crate::trap::Trap;
use crate::value::{Value, FIXNUM_MAX, FIXNUM_MIN};

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
const STR_FORMAT: u32 = 38;
const STR_RSPLIT: u32 = 39;
const STR_CASEFOLD: u32 = 40;
const STR_TRANSLATE: u32 = 41;
const STR_FORMAT_MAP: u32 = 42;
const STR_ENCODE: u32 = 43;
const STR_ISTITLE: u32 = 44;
const STR_ISPRINTABLE: u32 = 45;

/// The id of the `str` method `name`, or `None` if `str` has no such method.
/// Renders `n` in `radix` (2/8/10/16) with no sign or prefix; `upper` uppercases the hex digits.
fn format_radix(mut n: u32, radix: u32, upper: bool) -> String {
    if n == 0 {
        return String::from("0");
    }
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = Vec::new();
    while n > 0 {
        let digit = DIGITS[(n % radix) as usize];
        buf.push(if upper { digit.to_ascii_uppercase() } else { digit });
        n /= radix;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// Zero-pads the digit portion of a rendered int (the `%`-format int precision = minimum digits) to
/// at least `precision` digits, keeping a leading sign and any `0x`/`0o`/`0b` prefix in front.
fn zero_pad_int(rendered: &str, precision: usize) -> String {
    let (sign, rest) = match rendered.strip_prefix(['-', '+', ' ']) {
        Some(rest) => (&rendered[..1], rest),
        None => ("", rendered),
    };
    let (prefix, digits) = match rest.get(..2) {
        Some(p @ ("0x" | "0X" | "0o" | "0O" | "0b" | "0B")) => (p, &rest[2..]),
        _ => ("", rest),
    };
    let pad = "0".repeat(precision.saturating_sub(digits.chars().count()));
    alloc::format!("{sign}{prefix}{pad}{digits}")
}

/// Pads `s` to `width` code points with `fill`, per `align` (`<` left, `>`/`=` right, `^` centre).
fn pad_field(s: &str, width: usize, fill: char, align: char) -> String {
    let len = s.chars().count();
    if len >= width {
        return String::from(s);
    }
    let total = width - len;
    let mut out = String::new();
    match align {
        '<' => {
            out.push_str(s);
            (0..total).for_each(|_| out.push(fill));
        }
        '^' => {
            let left = total / 2;
            (0..left).for_each(|_| out.push(fill));
            out.push_str(s);
            (0..total - left).for_each(|_| out.push(fill));
        }
        _ => {
            (0..total).for_each(|_| out.push(fill));
            out.push_str(s);
        }
    }
    out
}

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
        "istitle" => Some(STR_ISTITLE),
        "isprintable" => Some(STR_ISPRINTABLE),
        "format" => Some(STR_FORMAT),
        "rsplit" => Some(STR_RSPLIT),
        "casefold" => Some(STR_CASEFOLD),
        "translate" => Some(STR_TRANSLATE),
        "format_map" => Some(STR_FORMAT_MAP),
        "encode" => Some(STR_ENCODE),
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
const DICT_POPITEM: u32 = 9;
const COUNTER_MOST_COMMON: u32 = 20;
const COUNTER_ELEMENTS: u32 = 21;
const COUNTER_TOTAL: u32 = 22;
const COUNTER_UPDATE: u32 = 23;
const COUNTER_SUBTRACT: u32 = 24;
const ODICT_MOVE_TO_END: u32 = 25;
const ODICT_POPITEM: u32 = 26;
/// A synthetic method id: a `super().__init__(*args)` that resolves to the built-in
/// `BaseException.__init__` (sets `self.args`). Reserved (`u32::MAX`) -- never a real per-type id.
const EXC_INIT: u32 = u32::MAX;
/// The no-op object-default methods (`object.__init__` / `object.__init_subclass__`) that a
/// `super()` call resolves to when no user base provides one -- a bound method that ignores its
/// args and returns `None`, so idiomatic `super().__init__()` / `super().__init_subclass__(**kw)`
/// terminate cleanly at the (implicit) object base.
const OBJECT_NOOP: u32 = u32::MAX - 1;
/// A synthetic method id for an UNBOUND built-in-exception `__init__` (`ValueError.__init__(self,
/// msg)`): the receiver is the call's FIRST argument (not a fixed bound self), so it sets `self.args`
/// on whatever instance is passed -- the unbound twin of [`EXC_INIT`], for a subclass that calls the
/// base initializer explicitly instead of via `super().__init__`.
const EXC_INIT_UNBOUND: u32 = u32::MAX - 2;
/// A synthetic method id for `callable.__call__`: dispatched by the interpreter's `call_value` (it
/// needs the driver), it invokes the bound receiver with the call's arguments (`f.__call__(x)` == `f(x)`).
pub(crate) const CALL_DUNDER: u32 = u32::MAX - 3;
/// A synthetic method id for an iterator's `__next__`: dispatched by the interpreter's `call_value`,
/// it advances the bound iterator like `next(it)` (raising `StopIteration` at exhaustion).
pub(crate) const NEXT_DUNDER: u32 = u32::MAX - 4;
/// A synthetic method id for `object.__getstate__` -- the instance state a copy of it must carry:
/// the instance `__dict__`, or `None` when there is nothing in it. A FULLY-SLOTTED instance has no
/// `__dict__` to hand out, so its state is the `(dict, slots)` pair CPython uses for one, with the
/// slot values in the second half.
pub(crate) const OBJECT_GETSTATE: u32 = u32::MAX - 6;
/// A synthetic method id for `object.__new__` -- the allocator every `__new__` chain terminates in.
/// It takes the class to instantiate from the call's FIRST ARGUMENT rather than a bound receiver,
/// because `__new__` is an implicit STATIC method: `object.__new__(cls)`, `Cls.__new__(Cls)` and
/// `super().__new__(cls)` all name the class explicitly, and the class named there is the one
/// allocated (which is how a `__new__` returning a subclass instance works).
pub(crate) const OBJECT_NEW: u32 = u32::MAX - 5;
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
const NT_ASDICT: u32 = 10;
pub(crate) const NT_REPLACE: u32 = 11;

/// The namedtuple-instance method id for `name`, or `None` (the tuple surface then applies).
fn nt_method_id(name: &str) -> Option<u32> {
    match name {
        "_asdict" => Some(NT_ASDICT),
        "_replace" => Some(NT_REPLACE),
        _ => None,
    }
}

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
        "popitem" => Some(DICT_POPITEM),
        _ => None,
    }
}

const DEQUE_APPEND: u32 = 0;
const DEQUE_APPENDLEFT: u32 = 1;
const DEQUE_POP: u32 = 2;
const DEQUE_POPLEFT: u32 = 3;
const DEQUE_EXTEND: u32 = 4;
const DEQUE_EXTENDLEFT: u32 = 5;
const DEQUE_ROTATE: u32 = 6;
const DEQUE_CLEAR: u32 = 7;
const DEQUE_COUNT: u32 = 8;
const DEQUE_REMOVE: u32 = 9;
const DEQUE_COPY: u32 = 10;

/// The method id for a `collections.deque` method `name`.
fn deque_method_id(name: &str) -> Option<u32> {
    match name {
        "append" => Some(DEQUE_APPEND),
        "appendleft" => Some(DEQUE_APPENDLEFT),
        "pop" => Some(DEQUE_POP),
        "popleft" => Some(DEQUE_POPLEFT),
        "extend" => Some(DEQUE_EXTEND),
        "extendleft" => Some(DEQUE_EXTENDLEFT),
        "rotate" => Some(DEQUE_ROTATE),
        "clear" => Some(DEQUE_CLEAR),
        "count" => Some(DEQUE_COUNT),
        "remove" => Some(DEQUE_REMOVE),
        "copy" => Some(DEQUE_COPY),
        _ => None,
    }
}

/// The Counter-specific method id for `name` (`most_common`/`elements`/`total`, plus the
/// count-adding `update`/`subtract` OVERRIDES of the dict surface), or `None` (the inherited dict
/// method then applies).
fn counter_method_id(name: &str) -> Option<u32> {
    match name {
        "most_common" => Some(COUNTER_MOST_COMMON),
        "elements" => Some(COUNTER_ELEMENTS),
        "total" => Some(COUNTER_TOTAL),
        "update" => Some(COUNTER_UPDATE),
        "subtract" => Some(COUNTER_SUBTRACT),
        _ => None,
    }
}

/// The OrderedDict-specific method id for `name` (`move_to_end`; `popitem` gains the FIFO flag),
/// or `None` (the inherited dict method then applies).
fn odict_method_id(name: &str) -> Option<u32> {
    match name {
        "move_to_end" => Some(ODICT_MOVE_TO_END),
        "popitem" => Some(ODICT_POPITEM),
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

/// `complex.conjugate()` -- the one complex method (`.real`/`.imag` are plain float attributes,
/// resolved in `getattr` directly, not bound methods).
#[cfg(feature = "complex")]
const COMPLEX_CONJUGATE: u32 = 0;

/// The method id for a `complex` method `name`.
#[cfg(feature = "complex")]
fn complex_method_id(name: &str) -> Option<u32> {
    match name {
        "conjugate" => Some(COMPLEX_CONJUGATE),
        _ => None,
    }
}

/// Which projection of a dict a view object exposes (`d.keys()` / `d.values()` / `d.items()`) --
/// derived from the view's type id, since each kind is its own type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DictViewKind {
    Keys,
    Values,
    Items,
}

/// How a user descriptor resolves an instance attribute READ (see `instance_descriptor_read`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DescriptorRead {
    /// Call this `__get__` (already bound to the descriptor) with `[instance, class]`.
    Get(Value),
    /// The instance-dict entry that shadows a non-data descriptor -- use it directly.
    Value(Value),
}

pub(crate) const LAZY_MAP: u32 = 0;
pub(crate) const LAZY_FILTER: u32 = 1;
pub(crate) const LAZY_ZIP: u32 = 2;
pub(crate) const LAZY_ENUMERATE: u32 = 3;
/// `iter(callable, sentinel)`: state = the callable, the single source element = the sentinel.
pub(crate) const LAZY_CALLABLE: u32 = 4;
/// The old-style sequence protocol: iterate an object that has `__getitem__` but no `__iter__` by
/// `obj[0]`, `obj[1]`, ... until IndexError. state = the current index, the single source = the object.
pub(crate) const LAZY_GETITEM: u32 = 5;

const BYTES_HEX: u32 = 0;
const BYTES_DECODE: u32 = 1;
const BYTEARRAY_APPEND: u32 = 2;
const BYTEARRAY_EXTEND: u32 = 3;
const BYTES_STARTSWITH: u32 = 4;
const BYTES_ENDSWITH: u32 = 5;
const BYTES_FIND: u32 = 6;
const BYTES_COUNT: u32 = 7;
const BYTES_REPLACE: u32 = 8;
const BYTES_UPPER: u32 = 9;
const BYTES_LOWER: u32 = 10;
const BYTES_SPLIT: u32 = 11;
const BYTES_STRIP: u32 = 12;
const BYTES_LSTRIP: u32 = 13;
const BYTES_RSTRIP: u32 = 14;
const BYTES_ISALPHA: u32 = 15;
const BYTES_ISDIGIT: u32 = 16;
const BYTES_ISALNUM: u32 = 17;
const BYTES_ISSPACE: u32 = 18;
const BYTES_ISUPPER: u32 = 19;
const BYTES_ISLOWER: u32 = 20;
const BYTES_ISTITLE: u32 = 21;
const BYTES_TITLE: u32 = 22;
const BYTES_CAPITALIZE: u32 = 23;
const BYTES_SWAPCASE: u32 = 24;
const BYTES_REMOVEPREFIX: u32 = 25;
const BYTES_REMOVESUFFIX: u32 = 26;
const BYTES_JOIN: u32 = 27;
const BYTES_RFIND: u32 = 28;
const BYTES_INDEX: u32 = 29;
const BYTES_RINDEX: u32 = 30;
const BYTES_CENTER: u32 = 31;
const BYTES_LJUST: u32 = 32;
const BYTES_RJUST: u32 = 33;
const BYTES_ZFILL: u32 = 34;
const BYTES_PARTITION: u32 = 35;
const BYTES_RPARTITION: u32 = 36;
const BYTES_SPLITLINES: u32 = 37;
const BYTES_EXPANDTABS: u32 = 38;
const BYTES_RSPLIT: u32 = 39;
/// `bytearray.copy()` -- a mutable bytearray with the same bytes. Only a bytearray has it: a `bytes`
/// cannot change, so CPython gives it none.
const BYTEARRAY_COPY: u32 = 40;

/// The method id for a `bytes`/`bytearray` method `name` (`mutating` allows the bytearray-only ones).
fn bytes_method_id(name: &str, mutating: bool) -> Option<u32> {
    match name {
        "hex" => Some(BYTES_HEX),
        "decode" => Some(BYTES_DECODE),
        "startswith" => Some(BYTES_STARTSWITH),
        "endswith" => Some(BYTES_ENDSWITH),
        "find" => Some(BYTES_FIND),
        "count" => Some(BYTES_COUNT),
        "replace" => Some(BYTES_REPLACE),
        "upper" => Some(BYTES_UPPER),
        "lower" => Some(BYTES_LOWER),
        "split" => Some(BYTES_SPLIT),
        "strip" => Some(BYTES_STRIP),
        "lstrip" => Some(BYTES_LSTRIP),
        "rstrip" => Some(BYTES_RSTRIP),
        "append" if mutating => Some(BYTEARRAY_APPEND),
        "extend" if mutating => Some(BYTEARRAY_EXTEND),
        "copy" if mutating => Some(BYTEARRAY_COPY),
        "isalpha" => Some(BYTES_ISALPHA),
        "isdigit" => Some(BYTES_ISDIGIT),
        "isalnum" => Some(BYTES_ISALNUM),
        "isspace" => Some(BYTES_ISSPACE),
        "isupper" => Some(BYTES_ISUPPER),
        "islower" => Some(BYTES_ISLOWER),
        "istitle" => Some(BYTES_ISTITLE),
        "title" => Some(BYTES_TITLE),
        "capitalize" => Some(BYTES_CAPITALIZE),
        "swapcase" => Some(BYTES_SWAPCASE),
        "removeprefix" => Some(BYTES_REMOVEPREFIX),
        "removesuffix" => Some(BYTES_REMOVESUFFIX),
        "join" => Some(BYTES_JOIN),
        "rfind" => Some(BYTES_RFIND),
        "index" => Some(BYTES_INDEX),
        "rindex" => Some(BYTES_RINDEX),
        "center" => Some(BYTES_CENTER),
        "ljust" => Some(BYTES_LJUST),
        "rjust" => Some(BYTES_RJUST),
        "zfill" => Some(BYTES_ZFILL),
        "partition" => Some(BYTES_PARTITION),
        "rpartition" => Some(BYTES_RPARTITION),
        "splitlines" => Some(BYTES_SPLITLINES),
        "expandtabs" => Some(BYTES_EXPANDTABS),
        "rsplit" => Some(BYTES_RSPLIT),
        _ => None,
    }
}

/// Splits `data` on line boundaries (`\n`, `\r`, `\r\n`) for `bytes.splitlines`; `keepends` keeps the
/// terminator on each line. A final line with no terminator is kept if non-empty.
fn split_lines_bytes(data: &[u8], keepends: bool) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                if keepends {
                    current.push(b'\n');
                }
                lines.push(core::mem::take(&mut current));
                i += 1;
            }
            b'\r' => {
                let crlf = data.get(i + 1) == Some(&b'\n');
                if keepends {
                    current.push(b'\r');
                    if crlf {
                        current.push(b'\n');
                    }
                }
                lines.push(core::mem::take(&mut current));
                i += if crlf { 2 } else { 1 };
            }
            other => {
                current.push(other);
                i += 1;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The ASCII case transform for a `bytes`/`bytearray` method: `title` (each word's first letter
/// upper, rest lower), `capitalize` (first byte upper, the rest lower), `swapcase` (flip each).
fn bytes_case_transform(method_id: u32, data: &[u8]) -> Vec<u8> {
    match method_id {
        BYTES_TITLE => {
            let mut out = Vec::with_capacity(data.len());
            let mut previous_is_cased = false;
            for &b in data {
                if b.is_ascii_alphabetic() {
                    out.push(if previous_is_cased {
                        b.to_ascii_lowercase()
                    } else {
                        b.to_ascii_uppercase()
                    });
                    previous_is_cased = true;
                } else {
                    out.push(b);
                    previous_is_cased = false;
                }
            }
            out
        }
        BYTES_CAPITALIZE => {
            let mut out: Vec<u8> = data.iter().map(u8::to_ascii_lowercase).collect();
            if let Some(first) = out.first_mut() {
                *first = first.to_ascii_uppercase();
            }
            out
        }
        _ => data
            .iter()
            .map(|&b| {
                if b.is_ascii_uppercase() {
                    b.to_ascii_lowercase()
                } else if b.is_ascii_lowercase() {
                    b.to_ascii_uppercase()
                } else {
                    b
                }
            })
            .collect(),
    }
}

/// An ASCII `bytes`/`bytearray` predicate (`isalpha`/`isdigit`/`isalnum`/`isspace`/`isupper`/
/// `islower`/`istitle`), no arguments -> a bool. Bytes predicates test the ASCII range only (unlike
/// the Unicode-aware `str` versions).
fn bytes_predicate(method_id: u32, data: &[u8]) -> bool {
    match method_id {
        BYTES_ISALPHA => !data.is_empty() && data.iter().all(u8::is_ascii_alphabetic),
        BYTES_ISDIGIT => !data.is_empty() && data.iter().all(u8::is_ascii_digit),
        BYTES_ISALNUM => !data.is_empty() && data.iter().all(u8::is_ascii_alphanumeric),
        BYTES_ISSPACE => {
            !data.is_empty()
                && data.iter().all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        }
        BYTES_ISUPPER => {
            let mut cased = false;
            for &b in data {
                if b.is_ascii_lowercase() {
                    return false;
                }
                cased |= b.is_ascii_uppercase();
            }
            cased
        }
        BYTES_ISLOWER => {
            let mut cased = false;
            for &b in data {
                if b.is_ascii_uppercase() {
                    return false;
                }
                cased |= b.is_ascii_lowercase();
            }
            cased
        }
        BYTES_ISTITLE => {
            let mut cased = false;
            let mut previous_is_cased = false;
            for &b in data {
                if b.is_ascii_uppercase() {
                    if previous_is_cased {
                        return false;
                    }
                    previous_is_cased = true;
                    cased = true;
                } else if b.is_ascii_lowercase() {
                    if !previous_is_cased {
                        return false;
                    }
                    cased = true;
                } else {
                    previous_is_cased = false;
                }
            }
            cased
        }
        _ => false,
    }
}

const MV_TOBYTES: u32 = 0;
const MV_TOLIST: u32 = 1;
const MV_HEX: u32 = 2;

/// `slice.indices(length)` -- the slice's only method.
const SLICE_INDICES: u32 = 0;

/// The method id for a `slice` method `name`.
fn slice_method_id(name: &str) -> Option<u32> {
    match name {
        "indices" => Some(SLICE_INDICES),
        _ => None,
    }
}

/// The method id for a `memoryview` method `name`.
fn memoryview_method_id(name: &str) -> Option<u32> {
    match name {
        "tobytes" => Some(MV_TOBYTES),
        "tolist" => Some(MV_TOLIST),
        "hex" => Some(MV_HEX),
        _ => None,
    }
}

const INT_BIT_LENGTH: u32 = 0;
const INT_BIT_COUNT: u32 = 1;
const INT_TO_BYTES: u32 = 2;
const INT_CONJUGATE: u32 = 3;
const INT_AS_INTEGER_RATIO: u32 = 4;
const INT_INDEX: u32 = 5;

/// The method id for an `int` method `name`.
fn int_method_id(name: &str) -> Option<u32> {
    match name {
        "bit_length" => Some(INT_BIT_LENGTH),
        "bit_count" => Some(INT_BIT_COUNT),
        "to_bytes" => Some(INT_TO_BYTES),
        "conjugate" => Some(INT_CONJUGATE),
        "as_integer_ratio" => Some(INT_AS_INTEGER_RATIO),
        "__index__" => Some(INT_INDEX),
        _ => None,
    }
}

const FLOAT_IS_INTEGER: u32 = 0;
const FLOAT_AS_INTEGER_RATIO: u32 = 1;
const FLOAT_CONJUGATE: u32 = 2;
const FLOAT_HEX: u32 = 3;

/// The method id for a `float` method `name`.
fn float_method_id(name: &str) -> Option<u32> {
    match name {
        "is_integer" => Some(FLOAT_IS_INTEGER),
        "as_integer_ratio" => Some(FLOAT_AS_INTEGER_RATIO),
        "conjugate" => Some(FLOAT_CONJUGATE),
        "hex" => Some(FLOAT_HEX),
        _ => None,
    }
}


/// Every dunder name the layer can carry. APPEND-ONLY: a name's index is its reserved-id offset, so
/// new dunders go at the END to keep earlier ids stable.
const DUNDER_NAMES: &[&str] = &[
    "__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__",
    "__truediv__", "__rtruediv__", "__floordiv__", "__rfloordiv__",
    "__mod__", "__rmod__", "__pow__", "__rpow__", "__divmod__", "__rdivmod__",
    "__and__", "__rand__", "__or__", "__ror__", "__xor__", "__rxor__",
    "__lshift__", "__rlshift__", "__rshift__", "__rrshift__",
    "__neg__", "__pos__", "__abs__", "__invert__",
    "__int__", "__float__", "__bool__",
    "__eq__", "__ne__", "__lt__", "__le__", "__gt__", "__ge__",
    "__hash__", "__repr__", "__str__",
    "__len__", "__getitem__", "__setitem__", "__delitem__", "__contains__", "__iter__",
];
const DUNDER_BASE: u32 = 0xFFF0_0000;

/// The reserved method id for the layer-handled dunder `name`, or `None`.
fn builtin_dunder_id(name: &str) -> Option<u32> {
    DUNDER_NAMES
        .iter()
        .position(|entry| *entry == name)
        .map(|i| DUNDER_BASE + i as u32)
}

/// The dunder name a reserved id maps back to (for dispatch), or `None` if it is not one.
fn dunder_name_of(id: u32) -> Option<&'static str> {
    id.checked_sub(DUNDER_BASE)
        .and_then(|i| DUNDER_NAMES.get(i as usize))
        .copied()
}

/// The `(operator, is-reflected)` for a binary-operator dunder (`__add__`/`__radd__`/...), else `None`.
fn binop_of_dunder(name: &str) -> Option<(BinOp, bool)> {
    Some(match name {
        "__add__" => (BinOp::Add, false),
        "__radd__" => (BinOp::Add, true),
        "__sub__" => (BinOp::Sub, false),
        "__rsub__" => (BinOp::Sub, true),
        "__mul__" => (BinOp::Mul, false),
        "__rmul__" => (BinOp::Mul, true),
        "__truediv__" => (BinOp::TrueDiv, false),
        "__rtruediv__" => (BinOp::TrueDiv, true),
        "__floordiv__" => (BinOp::FloorDiv, false),
        "__rfloordiv__" => (BinOp::FloorDiv, true),
        "__mod__" => (BinOp::Mod, false),
        "__rmod__" => (BinOp::Mod, true),
        "__pow__" => (BinOp::Pow, false),
        "__rpow__" => (BinOp::Pow, true),
        "__and__" => (BinOp::BitAnd, false),
        "__rand__" => (BinOp::BitAnd, true),
        "__or__" => (BinOp::BitOr, false),
        "__ror__" => (BinOp::BitOr, true),
        "__xor__" => (BinOp::BitXor, false),
        "__rxor__" => (BinOp::BitXor, true),
        "__lshift__" => (BinOp::LShift, false),
        "__rlshift__" => (BinOp::LShift, true),
        "__rshift__" => (BinOp::RShift, false),
        "__rrshift__" => (BinOp::RShift, true),
        _ => return None,
    })
}

/// The comparison operator for a comparison dunder (`__eq__`/...), else `None`.
fn cmpop_of_dunder(name: &str) -> Option<CmpOp> {
    Some(match name {
        "__eq__" => CmpOp::Eq,
        "__ne__" => CmpOp::Ne,
        "__lt__" => CmpOp::Lt,
        "__le__" => CmpOp::Le,
        "__gt__" => CmpOp::Gt,
        "__ge__" => CmpOp::Ge,
        _ => return None,
    })
}

/// Whether an `int`/`bool` value exposes dunder `name` (CPython's int method set: arithmetic, the
/// full bitwise family, comparisons, divmod, the unary ops incl. `__invert__`, and int/float/bool).
fn int_supports_dunder(name: &str) -> bool {
    binop_of_dunder(name).is_some()
        || cmpop_of_dunder(name).is_some()
        || matches!(
            name,
            "__divmod__"
                | "__rdivmod__"
                | "__neg__"
                | "__pos__"
                | "__abs__"
                | "__invert__"
                | "__int__"
                | "__float__"
                | "__bool__"
                | "__hash__"
                | "__repr__"
                | "__str__"
        )
}

/// Whether a `float` value exposes dunder `name` -- like int MINUS the bitwise/`__invert__` family
/// (a float has no `__and__`/`__lshift__`/`__invert__`/...).
fn float_supports_dunder(name: &str) -> bool {
    if matches!(
        name,
        "__and__"
            | "__rand__"
            | "__or__"
            | "__ror__"
            | "__xor__"
            | "__rxor__"
            | "__lshift__"
            | "__rlshift__"
            | "__rshift__"
            | "__rrshift__"
            | "__invert__"
    ) {
        return false;
    }
    binop_of_dunder(name).is_some()
        || cmpop_of_dunder(name).is_some()
        || matches!(
            name,
            "__divmod__"
                | "__rdivmod__"
                | "__neg__"
                | "__pos__"
                | "__abs__"
                | "__int__"
                | "__float__"
                | "__bool__"
                | "__hash__"
                | "__repr__"
                | "__str__"
        )
}

/// The dunders a BUILTIN TYPE OBJECT resolves to an unbound method (`int.__add__`, `str.__len__`),
/// mirroring the per-VALUE gate so `T.__d__` exists exactly when an instance of `T` exposes `__d__`
/// (the unbound dispatch re-resolves the dunder on the first argument, which must find a bound one).
/// Kept in step with [`int_supports_dunder`] / [`float_supports_dunder`] and `container_supports_dunder`.
fn type_object_supports_dunder(builtin: Builtin, name: &str) -> bool {
    use Builtin::*;
    let iter_contains = matches!(name, "__contains__" | "__iter__" | "__len__");
    let subscript = matches!(name, "__getitem__");
    let mutable_item = matches!(name, "__setitem__" | "__delitem__");
    let seq_arith = matches!(name, "__add__" | "__mul__" | "__rmul__");
    match builtin {
        Int | Bool => int_supports_dunder(name),
        Float => float_supports_dunder(name),
        Str | Bytes | Tuple => iter_contains || subscript || seq_arith,
        List | Bytearray => iter_contains || subscript || mutable_item || seq_arith,
        Dict => iter_contains || subscript || mutable_item,
        Range => iter_contains || subscript,
        Set | Frozenset => iter_contains,
        _ => false,
    }
}

/// `float.hex()`: CPython's exact hexadecimal rendering of a double -- `[sign] 0xL.MMMMMMMMMMMMMp±E`,
/// where `L` is the leading bit (1 normal, 0 subnormal/zero), `M` the 52-bit mantissa as 13 hex
/// digits (trailing zeros KEPT), and `E` the power-of-two exponent (always signed). inf/nan render
/// as their words. The inverse of [`crate::builtins::parse_hex_float`] (`float.fromhex`).
fn float_to_hex(value: f64) -> String {
    if value.is_nan() {
        return String::from("nan");
    }
    if value.is_infinite() {
        return String::from(if value < 0.0 { "-inf" } else { "inf" });
    }
    let bits = value.to_bits();
    let sign = if bits >> 63 == 1 { "-" } else { "" };
    let exp_bits = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & 0x000f_ffff_ffff_ffff;
    if exp_bits == 0 && mantissa == 0 {
        return alloc::format!("{sign}0x0.0p+0");
    }
    let (lead, exp) = if exp_bits == 0 { (0u64, -1022i64) } else { (1u64, exp_bits - 1023) };
    let exp_sign = if exp >= 0 { "+" } else { "-" };
    alloc::format!("{sign}0x{lead}.{mantissa:013x}p{exp_sign}{}", exp.abs())
}

/// The exact `(numerator, denominator)` of a finite float, as reduced BigInts (the denominator a
/// power of two). `None` for a non-finite float (`inf`/`nan`). Backs `float.as_integer_ratio`.
fn float_as_integer_ratio(f: f64) -> Option<(BigInt, BigInt)> {
    if !f.is_finite() {
        return None;
    }
    if f == 0.0 {
        return Some((BigInt::from_i128(0), BigInt::from_i128(1)));
    }
    let bits = f.to_bits();
    let sign: i128 = if bits >> 63 == 1 { -1 } else { 1 };
    let raw_exp = ((bits >> 52) & 0x7ff) as i64;
    let raw_mantissa = bits & 0x000f_ffff_ffff_ffff;
    let (mut mantissa, mut exp) = if raw_exp == 0 {
        (raw_mantissa, -1074i64)
    } else {
        (raw_mantissa | 0x0010_0000_0000_0000, raw_exp - 1075)
    };
    let tz = mantissa.trailing_zeros();
    mantissa >>= tz;
    exp += i64::from(tz);
    let magnitude = BigInt::from_i128(sign * i128::from(mantissa));
    if exp >= 0 {
        Some((magnitude.shl(exp as u64), BigInt::from_i128(1)))
    } else {
        Some((magnitude, BigInt::from_i128(1).shl((-exp) as u64)))
    }
}

const PROPERTY_GETTER: u32 = 0;
const PROPERTY_SETTER: u32 = 1;
const PROPERTY_DELETER: u32 = 2;

/// The method id for a `property` builder method `name`.
fn property_method_id(name: &str) -> Option<u32> {
    match name {
        "getter" => Some(PROPERTY_GETTER),
        "setter" => Some(PROPERTY_SETTER),
        "deleter" => Some(PROPERTY_DELETER),
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
        STR_ISTITLE => {
            let mut cased = false;
            let mut previous_is_cased = false;
            for c in s.chars() {
                let cp = c as u32;
                if is_uppercase(cp) || is_titlecase(cp) {
                    if previous_is_cased {
                        return false;
                    }
                    previous_is_cased = true;
                    cased = true;
                } else if is_lowercase(cp) {
                    if !previous_is_cased {
                        return false;
                    }
                    cased = true;
                } else {
                    previous_is_cased = false;
                }
            }
            cased
        }
        STR_ISPRINTABLE => s.chars().all(|c| {
            c == ' '
                || !matches!(
                    general_category(c as u32),
                    GeneralCategory::SpaceSeparator
                        | GeneralCategory::LineSeparator
                        | GeneralCategory::ParagraphSeparator
                        | GeneralCategory::Control
                        | GeneralCategory::Format
                        | GeneralCategory::Surrogate
                        | GeneralCategory::PrivateUse
                        | GeneralCategory::NotAssigned
                )
        }),
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

/// Renders a `float` exactly as CPython 3.14's `repr(float)`/`str(float)` does (they are identical
/// for a float): the SHORTEST decimal string that round-trips to the same double, in CPython's
/// choice of fixed vs exponential notation.
///
/// The rules, from CPython's `format_float_short` (`Python/pystrtod.c`, format code `'r'`):
/// - `nan`, `inf`, `-inf` render as those literals (lowercase; a NaN never carries a sign).
/// - Otherwise take the shortest round-trip digits `d0 d1 ...` and the scientific exponent `E`
///   (value = `d0.d1d2... x 10^E`). Rust's `{:e}` gives exactly this pair.
/// - Use EXPONENTIAL notation when `E < -4` (i.e. `E <= -5`) or `E >= 16`; the exponent is written
///   with an explicit sign and at least two digits (`1e-05`, `1e+16`, `1e+308`). A single
///   significant digit omits the point (`1e+16`, not `1.0e+16`).
/// - Else FIXED notation, with the decimal point placed after `E + 1` digits; a value with no
///   fractional part still gets a trailing `.0` (`1.0`, `100.0`) -- CPython's `Py_DTSF_ADD_DOT_0`.
///
/// The shortest round-trip scientific form of `value` (`"<mantissa>e<exp>"`, one digit before the
/// point) with CPython's tie-breaking. Rust's `{:e}` gives the shortest round-trip length but breaks
/// an exactly-halfway tie AWAY from even, whereas CPython (David Gay's `dtoa`) breaks it TO even. So
/// re-round to that same number of significant digits with `{:.*e}` (which rounds half-to-even on
/// the exact value): when the even-tie result still round-trips, it is the one CPython emits (either
/// the unique shortest, or the even member of a genuine tie); when it does not round-trip (the
/// correctly-rounded value falls outside the double's interval), Rust's shortest is already correct.
/// Verified to match CPython 3.14 on a 250k-double random differential, ties included.
fn shortest_scientific(value: f64) -> String {
    let shortest = alloc::format!("{value:e}");
    let mantissa = shortest.split('e').next().unwrap_or(&shortest);
    let sig = mantissa.chars().filter(char::is_ascii_digit).count();
    let even = alloc::format!("{value:.*e}", sig.saturating_sub(1));
    if even.parse::<f64>() == Ok(value) {
        even
    } else {
        shortest
    }
}

/// The digit sequence is the shortest that round-trips, with equidistant ties broken to EVEN --
/// see [`shortest_scientific`] for why Rust's `{:e}` alone is not enough. A 250k-double random
/// differential confirms it matches CPython 3.14 exactly (see the float corpus).
fn format_float(value: f64) -> String {
    format_float_impl(value, true)
}

/// The shared shortest-round-trip float renderer. `add_point_zero` controls CPython's
/// `Py_DTSF_ADD_DOT_0`: `true` for `float` (`1.0`, `100.0`), `false` for a `complex` PART (`(1+2j)`,
/// not `(1.0+2.0j)`) -- the only place the two formats differ (an integer-valued fixed result).
fn format_float_impl(value: f64, add_point_zero: bool) -> String {
    if value.is_nan() {
        return String::from("nan");
    }
    if value.is_infinite() {
        return String::from(if value < 0.0 { "-inf" } else { "inf" });
    }
    let scientific = shortest_scientific(value);
    let (negative, rest) = match scientific.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, scientific.as_str()),
    };
    let (mantissa, exponent) = rest.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is a valid integer");
    let mut digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let ndigits = digits.len() as i32;
    let decpt = exponent + 1;

    let body = if !(-4..16).contains(&exponent) {
        let mut mantissa_out = String::new();
        mantissa_out.push_str(&digits[..1]);
        if ndigits > 1 {
            mantissa_out.push('.');
            mantissa_out.push_str(&digits[1..]);
        }
        let sign = if exponent < 0 { '-' } else { '+' };
        alloc::format!("{mantissa_out}e{sign}{:02}", exponent.unsigned_abs())
    } else if decpt <= 0 {
        alloc::format!("0.{}{digits}", "0".repeat((-decpt) as usize))
    } else if decpt >= ndigits {
        let zeros = "0".repeat((decpt - ndigits) as usize);
        if add_point_zero {
            alloc::format!("{digits}{zeros}.0")
        } else {
            alloc::format!("{digits}{zeros}")
        }
    } else {
        alloc::format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
    };
    if negative {
        alloc::format!("-{body}")
    } else {
        body
    }
}

/// Formats a non-negative double in scientific notation with a fixed number of fractional digits,
/// in CPython's `e`/`E` style (signed, >=2-digit exponent): `1.23e+04`. `mantissa` precision is the
/// fractional-digit count.
fn float_format_scientific(magnitude: f64, precision: usize, upper: bool) -> String {
    let raw = alloc::format!("{magnitude:.precision$e}");
    let (mantissa, exponent) = raw.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("valid exponent");
    let e = if upper { 'E' } else { 'e' };
    let sign = if exponent < 0 { '-' } else { '+' };
    alloc::format!("{mantissa}{e}{sign}{:02}", exponent.unsigned_abs())
}

/// Formats a non-negative double in the general (`g`/`G`) style: `precision` significant digits, in
/// fixed notation when the exponent is in `[-4, precision)` else scientific, trailing zeros stripped
/// (kept under `#` alternate). With `keep_decimal`, a fixed result always keeps one fractional digit
/// (the default-format code's rule, `3.0` not `3`).
fn float_format_general(magnitude: f64, precision: usize, upper: bool, alternate: bool, keep_decimal: bool) -> String {
    let precision = precision.max(1);
    let sci = alloc::format!("{magnitude:.*e}", precision - 1);
    let (mantissa, exponent) = sci.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("valid exponent");
    if (-4..precision as i32).contains(&exponent) {
        let fractional = (precision as i32 - 1 - exponent) as usize;
        let fixed = alloc::format!("{magnitude:.fractional$}");
        let stripped = if alternate { fixed } else { strip_float_trailing_zeros(&fixed) };
        if keep_decimal && !stripped.contains('.') {
            alloc::format!("{stripped}.0")
        } else {
            stripped
        }
    } else {
        let mantissa = if alternate {
            String::from(mantissa)
        } else {
            strip_float_trailing_zeros(mantissa)
        };
        let e = if upper { 'E' } else { 'e' };
        let sign = if exponent < 0 { '-' } else { '+' };
        alloc::format!("{mantissa}{e}{sign}{:02}", exponent.unsigned_abs())
    }
}

/// Strips trailing zeros after a decimal point (and a now-bare trailing point): `3.140` -> `3.14`,
/// `3.000` -> `3`.
fn strip_float_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return String::from(s);
    }
    let trimmed = s.trim_end_matches('0');
    String::from(trimmed.strip_suffix('.').unwrap_or(trimmed))
}

/// Inserts a `separator` every three digits of the INTEGER part (left of any `.`): `1234567.89` with
/// `,` -> `1,234,567.89`.
fn group_integer_digits(body: &str, separator: char) -> String {
    let (integer, rest) = match body.split_once('.') {
        Some((int, frac)) => (int, alloc::format!(".{frac}")),
        None => (body, String::new()),
    };
    let mut grouped = String::new();
    let digit_count = integer.len();
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (digit_count - index) % 3 == 0 {
            grouped.push(separator);
        }
        grouped.push(digit);
    }
    grouped.push_str(&rest);
    grouped
}

/// Renders a `complex` as CPython's `repr`/`str` (identical). When the real part is a POSITIVE zero
/// only the imaginary term shows (`1j`, `-1j`, `0j`); otherwise `(real+imagj)` with the imaginary
/// term explicitly signed. Each part uses the shortest-round-trip float format WITHOUT a trailing
/// `.0` (`(1+2j)`, `(2.5-0.5j)`, `(1e+100+2e-05j)`, `(-0-1j)`).
#[cfg(feature = "complex")]
fn format_complex(real: f64, imag: f64) -> String {
    let imag_str = format_float_impl(imag, false);
    if real == 0.0 && real.is_sign_positive() {
        return alloc::format!("{imag_str}j");
    }
    let real_str = format_float_impl(real, false);
    let sign = if imag.is_sign_negative() { "" } else { "+" };
    alloc::format!("({real_str}{sign}{imag_str}j)")
}

/// CPython's `repr(bytes)` / `repr(bytearray)`: `b'...'` (or `bytearray(b'...')`), printable ASCII
/// shown literally, `\t\n\r\\` escaped, everything else as `\xNN`. The quote is `'` unless the data
/// has a `'` but no `"`.
fn bytes_repr(data: &[u8], is_bytearray: bool) -> String {
    let quote = if data.contains(&b'\'') && !data.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::new();
    if is_bytearray {
        out.push_str("bytearray(");
    }
    out.push('b');
    out.push(quote as char);
    for &byte in data {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b if b == quote => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&alloc::format!("\\x{byte:02x}")),
        }
    }
    out.push(quote as char);
    if is_bytearray {
        out.push(')');
    }
    out
}

/// Replaces every non-overlapping occurrence of `old` with `new` in `data`. An empty `old` inserts
/// `new` between (and around) every byte, matching CPython.
fn replace_bytes(data: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() {
        let mut out = Vec::with_capacity(data.len() + new.len() * (data.len() + 1));
        out.extend_from_slice(new);
        for &byte in data {
            out.push(byte);
            out.extend_from_slice(new);
        }
        return out;
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + old.len() <= data.len() && &data[i..i + old.len()] == old {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Splits `data` on non-overlapping occurrences of `sep` (which must be non-empty).
fn split_on_bytes(data: &[u8], sep: &[u8]) -> Vec<Vec<u8>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + sep.len() <= data.len() {
        if &data[i..i + sep.len()] == sep {
            parts.push(data[start..i].to_vec());
            i += sep.len();
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(data[start..].to_vec());
    parts
}

/// Splits `data` on runs of ASCII whitespace; leading/trailing whitespace yields no empty parts.
fn split_whitespace_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    data.split(u8::is_ascii_whitespace)
        .filter(|part| !part.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// `bytes.split(None, maxsplit)` with `maxsplit >= 0`: whitespace split at most `maxsplit` times from
/// the LEFT, leading whitespace skipped; the remainder keeps its internal + trailing whitespace.
fn split_whitespace_maxsplit_bytes(data: &[u8], maxsplit: usize) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    let mut i = 0;
    let n = data.len();
    while result.len() < maxsplit {
        while i < n && data[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        while i < n && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        result.push(data[start..i].to_vec());
    }
    while i < n && data[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < n {
        result.push(data[i..].to_vec());
    }
    result
}

/// `bytes.rsplit(None, maxsplit)` with `maxsplit >= 0`: the whitespace split counting from the RIGHT.
fn rsplit_whitespace_maxsplit_bytes(data: &[u8], maxsplit: usize) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    let mut i = data.len();
    while result.len() < maxsplit {
        while i > 0 && data[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        if i == 0 {
            break;
        }
        let end = i;
        while i > 0 && !data[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        result.push(data[i..end].to_vec());
    }
    while i > 0 && data[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i > 0 {
        result.push(data[..i].to_vec());
    }
    result.reverse();
    result
}

/// `str.split(None, maxsplit)` with `maxsplit >= 0`: split on runs of whitespace at most `maxsplit`
/// times from the LEFT, leading whitespace skipped; after the last cut the remainder keeps its
/// internal + trailing whitespace verbatim (only its leading whitespace is stripped).
fn split_whitespace_maxsplit(s: &str, maxsplit: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut result = Vec::new();
    let mut i = 0;
    while result.len() < maxsplit {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        result.push(chars[start..i].iter().collect());
    }
    while i < n && chars[i].is_whitespace() {
        i += 1;
    }
    if i < n {
        result.push(chars[i..].iter().collect());
    }
    result
}

/// `str.rsplit(None, maxsplit)` with `maxsplit >= 0`: the whitespace split counting from the RIGHT,
/// so the leftmost pieces stay joined (the head keeps its leading + internal whitespace verbatim).
fn rsplit_whitespace_maxsplit(s: &str, maxsplit: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut result = Vec::new();
    let mut i = chars.len();
    while result.len() < maxsplit {
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i == 0 {
            break;
        }
        let end = i;
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        result.push(chars[i..end].iter().collect());
    }
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i > 0 {
        result.push(chars[..i].iter().collect());
    }
    result.reverse();
    result
}

/// Trims the bytes in `chars` from the chosen ends of `data`.
fn strip_bytes(data: &[u8], chars: &[u8], left: bool, right: bool) -> Vec<u8> {
    let mut start = 0;
    let mut end = data.len();
    if left {
        while start < end && chars.contains(&data[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && chars.contains(&data[end - 1]) {
            end -= 1;
        }
    }
    data[start..end].to_vec()
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

/// A simulated I2C target's register file (the LSM303AGR shape): a byte array addressed by an
/// internal pointer (SUB). A write's first byte sets the pointer; the ST SUB bit-7 convention gates
/// whether READS auto-increment it (a demo that forgets `| 0x80` reads one register repeatedly,
/// exactly as on silicon). Lives at the sim layer so a C# capstone half verifies the same device.
#[cfg(not(target_os = "none"))]
#[derive(Debug)]
struct I2cSimDevice {
    registers: Vec<u8>,
    pointer: u8,
    read_auto_increment: bool,
}

/// What one handle-indexed arena costs, split by WHICH of its two costs it is.
///
/// **The split is not a nicety: the two halves are reclaimed by different mechanisms and one of them is
/// easy to overlook.** A handle is an index into a `Vec`, so every object of that kind costs a SLOT in
/// the outer vector plus a PAYLOAD hanging off it. Releasing a dead object's payload while keeping its
/// slot -- which keeps every outstanding handle valid, and is what makes such a release cheap -- recovers
/// the payload and nothing else. **The slot is charged per ALLOCATION, not per live object**, so a loop
/// that allocates forever grows the slot vector forever even when every payload is released the instant
/// it dies. Measured on a four-element list loop, 2026-07-30: 98,304 bytes of slots against 64,000 of
/// payload, so payload release alone would recover 39 percent of a table that must stop growing entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Arena {
    /// The handle table itself -- the outer vector's reserved slots, live and dead alike. Falls only
    /// when handles are REUSED (a free list), never when a payload is dropped.
    pub slots: usize,
    /// What the slots point at: a string's bytes, a container's elements, a bignum's limbs.
    pub payload: usize,
}

impl Arena {
    /// Both halves.
    #[must_use]
    pub fn total(&self) -> usize {
        self.slots + self.payload
    }
}

/// What a running program costs, broken down by where the bytes are -- the answer to
/// [`ObjectModel::footprint`].
///
/// **The breakdown is the point, not a convenience.** The object heap is the only part a collection
/// reclaims, and it is the smaller part of what a program costs on a device -- an ordinary program's
/// tables can hold an order of magnitude more than its heap. A single total would let a figure that is
/// mostly side table be read as a heap figure, which is the exact mistake this type exists to prevent.
///
/// Every figure is CAPACITY rather than length: a buffer that grew and shrank still owns what it
/// reserved, and an allocator it was taken from does not know the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Footprint {
    /// Live bytes in the object heap -- what a collection moves. Not its capacity: a device pays that
    /// once, up front, as a single block, which is a cost of the CONFIGURATION rather than of the run.
    pub objects: usize,
    /// The `str` arena: every string's bytes.
    pub strings: Arena,
    /// The `list`/`tuple`/`deque` arena: every container's elements.
    pub sequences: Arena,
    /// The `dict` arena: every dict's key/value pairs.
    pub dicts: Arena,
    /// The `set`/`frozenset` arena: every set's members.
    pub sets: Arena,
    /// The arbitrary-precision `int` arena: every bignum's limbs.
    pub bigints: Arena,
    /// The `bytes`/`bytearray` arena: every buffer's bytes.
    pub byte_buffers: Arena,
    /// The namespaces: the entry module's globals, every managed module's, the import cache, the
    /// exception classes and the per-function attribute dicts -- names as well as values.
    pub namespaces: usize,
    /// Suspended generators' frames and the returned-frame pool: locals, evaluation stacks, inline
    /// caches, open handlers and closure cells.
    pub frames: usize,
    /// Captured `print` output still waiting to be drained. Zero once an embedder installs a console,
    /// because the text leaves as it is produced -- see [`ObjectModel::set_console`].
    pub stdout: usize,
}

impl Footprint {
    /// The six handle-indexed arenas, in the order this type declares them.
    #[must_use]
    pub fn arenas(&self) -> [Arena; 6] {
        [self.strings, self.sequences, self.dicts, self.sets, self.bigints, self.byte_buffers]
    }

    /// Every byte accounted for above. **This is the quantity the unlimited-timespan bar is about**: a
    /// program with a bounded live set, allocating in a loop, must hold this steady rather than climb.
    #[must_use]
    pub fn total(&self) -> usize {
        self.objects
            + self.arenas().iter().map(Arena::total).sum::<usize>()
            + self.namespaces
            + self.frames
            + self.stdout
    }

    /// Everything OUTSIDE the object heap -- the part a collection does not touch today.
    #[must_use]
    pub fn beside_the_heap(&self) -> usize {
        self.total() - self.objects
    }

    /// The six arenas' handle tables, added up -- the part that only handle REUSE can bring down.
    #[must_use]
    pub fn arena_slots(&self) -> usize {
        self.arenas().iter().map(|arena| arena.slots).sum()
    }

    /// The six arenas' payloads, added up -- the part releasing a dead object's contents brings down.
    #[must_use]
    pub fn arena_payload(&self) -> usize {
        self.arenas().iter().map(|arena| arena.payload).sum()
    }
}

/// The freed slot of each handle arena, one list per arena. See [`ObjectModel::freed_slots`].
#[derive(Debug, Default)]
struct FreedSlots {
    strings: Vec<u32>,
    seqs: Vec<u32>,
    dicts: Vec<u32>,
    sets: Vec<u32>,
    bigints: Vec<u32>,
    byte_buffers: Vec<u32>,
}

/// Which handle arena a heap object's first payload word indexes into.
/// See [`ObjectModel::arena_of`].
#[cfg(feature = "gc-collect")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArenaKind {
    Strings,
    Seqs,
    Dicts,
    Sets,
    Bigints,
    ByteBuffers,
}

/// One flag per slot of each arena while [`ObjectModel::release_dead_arena_slots`] walks the
/// survivors: set means some surviving object still names that slot.
#[cfg(feature = "gc-collect")]
struct LiveSlots {
    strings: Vec<bool>,
    seqs: Vec<bool>,
    dicts: Vec<bool>,
    sets: Vec<bool>,
    bigints: Vec<bool>,
    byte_buffers: Vec<bool>,
}

#[cfg(feature = "gc-collect")]
impl LiveSlots {
    /// Records that `index` of `arena` is still in use. Out-of-range is ignored rather than a panic:
    /// the arenas can only have grown since the flags were sized, and a slot beyond the end was made
    /// after this pass began looking.
    fn mark(&mut self, arena: ArenaKind, index: usize) {
        let flags = match arena {
            ArenaKind::Strings => &mut self.strings,
            ArenaKind::Seqs => &mut self.seqs,
            ArenaKind::Dicts => &mut self.dicts,
            ArenaKind::Sets => &mut self.sets,
            ArenaKind::Bigints => &mut self.bigints,
            ArenaKind::ByteBuffers => &mut self.byte_buffers,
        };
        if let Some(flag) = flags.get_mut(index) {
            *flag = true;
        }
    }

    /// Marks every slot that is already on a free list, so this pass cannot free one twice.
    fn mark_all(&mut self, freed: &FreedSlots) {
        for (flags, indices) in [
            (&mut self.strings, &freed.strings),
            (&mut self.seqs, &freed.seqs),
            (&mut self.dicts, &freed.dicts),
            (&mut self.sets, &freed.sets),
            (&mut self.bigints, &freed.bigints),
            (&mut self.byte_buffers, &freed.byte_buffers),
        ] {
            for &index in indices {
                if let Some(flag) = flags.get_mut(index as usize) {
                    *flag = true;
                }
            }
        }
    }
}

/// Replaces every dead slot's payload with an empty one and offers the slot for reuse.
///
/// The payload is dropped IN PLACE and the slot keeps its index, so no handle anywhere is invalidated
/// and nothing is renumbered -- the pattern `generators` already uses. Handing the index to `freed` is
/// the other half, and the half that bounds the table: without it a slot is charged per allocation and
/// the arena grows forever however promptly payloads are released.
#[cfg(feature = "gc-collect")]
fn empty_dead_slots<T>(arena: &mut [T], live: &[bool], freed: &mut Vec<u32>, empty: impl Fn() -> T) {
    for (index, in_use) in live.iter().enumerate() {
        if !in_use {
            arena[index] = empty();
            freed.push(index as u32);
        }
    }
}

/// Takes a slot in a handle arena to hold `payload`, REUSING one a collection freed if there is one.
///
/// A free function over the two vectors rather than a method, because every caller holds `&mut self`
/// and needs to name one arena and its list as disjoint borrows.
fn take_arena_slot<T>(arena: &mut Vec<T>, freed: &mut Vec<u32>, payload: T) -> u32 {
    match freed.pop() {
        Some(index) => {
            arena[index as usize] = payload;
            index
        }
        None => {
            let index = arena.len() as u32;
            arena.push(payload);
            index
        }
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
    /// The GC type-descriptor id of `bytes` (immutable); its one-word payload indexes `byte_buffers`.
    bytes_type_id: u32,
    /// The GC type-descriptor id of `bytearray` (mutable); shares the `byte_buffers` arena.
    bytearray_type_id: u32,
    /// The GC type-descriptor id of a `long` (a big int, an i128 payload).
    long_type_id: u32,
    /// The GC type-descriptor id of a `bigint` (an arbitrary-precision int beyond i128); its
    /// one-word payload indexes the `bigints` arena. The third `int` representation (after fixnum
    /// and the i128 `long`); all three present as Python `int`.
    bigint_type_id: u32,
    /// The GC type-descriptor id of a `float` (an IEEE-754 double, an f64 payload). A heap object
    /// because `Value` is a 32-bit word -- an f64 cannot be an immediate (mirrors `long`).
    float_type_id: u32,
    /// The GC type-descriptor id of a `complex` (two f64 -- the real + imaginary parts, a 16-byte
    /// payload). Behind the `complex` capability knob.
    #[cfg(feature = "complex")]
    complex_type_id: u32,
    /// The GC type-descriptor id of a provided module (a namespace-dict wrapper).
    module_type_id: u32,
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
    /// The runtime backing for `bigint`: each arbitrary-precision int's heap object holds an index
    /// into this. Grows monotonically (str-arena pattern; the values are immutable).
    bigints: Vec<BigInt>,
    /// The runtime backing for `bytes`/`bytearray`: each object holds an index into this. A `bytes`
    /// buffer never mutates; a `bytearray` mutates its `Vec<u8>` in place (list-arena pattern).
    byte_buffers: Vec<Vec<u8>>,
    /// The runtime backing for `set`: deduped elements in insertion order. A set's heap object
    /// holds an index into this. (Iteration order is insertion, not CPython's hash order -- a
    /// documented divergence; differential tests compare sets as sets, e.g. via `sorted`.)
    sets: Vec<Vec<Value>>,
    /// Slots in each arena above whose owner has died, ready to be handed out again.
    ///
    /// **Reuse is what bounds a handle table by the peak LIVE count instead of by the total number
    /// of allocations, and without it the arenas grow forever however promptly payloads are
    /// released.** A slot is charged per allocation: measured over a loop holding at most one object,
    /// the slot half of these arenas was 43 to 83 percent of their cost and quadrupled exactly when
    /// the pass count did. Emptying a dead slot recovers only the other half.
    ///
    /// Filled by [`ObjectModel::release_dead_arena_slots`], which runs after a collection and can
    /// therefore see which slots no surviving object still names. Drained by
    /// [`take_arena_slot`]. Empty when nothing has ever been collected, so a build with no
    /// collector behaves exactly as it did.
    freed_slots: FreedSlots,
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
    /// A `MemoryError` instance built IN ADVANCE, so that running out of memory can be reported.
    ///
    /// **Telling a program it is out of memory costs an allocation -- a class chain and an instance --
    /// at the one moment there may be nothing to allocate from.** Built during a collection instead,
    /// which happens at a safe point while a quarter of the heap is still free, and held here until it
    /// is needed. A GC root while it waits, or the collection that built it would take it straight back.
    ///
    /// Taken when it is handed to a raise, and rebuilt by the next collection. A program that exhausts
    /// memory twice with no collection in between gets the old behavior for the second one, which is a
    /// fatal trap; that is a narrower gap than the one this closes and it is not pretended away.
    memory_error_reserve: Option<Value>,
    /// The module namespace (top-level name -> value): classes and other top-level bindings the
    /// module body produces, which a function reaches by `LoadGlobal`. The body mirrors its locals
    /// here as it binds them.
    globals: Vec<(String, Value)>,
    /// The import cache (CPython's `sys.modules`): an imported module name -> its module object, so
    /// a repeated `import` returns the same object and a module body runs at most once. A GC root
    /// (with `globals`), keeping every imported module and its members reachable.
    modules: Vec<(String, Value)>,
    /// The managed-module registry: the Python-authored modules bundled with the program, resolved by
    /// `name` when an `import` misses the cache and the native/host modules. Immutable for a run
    /// (installed once by the host via [`ObjectModel::set_managed_modules`]); a first import CLONES the
    /// matched `Module` out to run its body (once, then cached in `modules`), which sidesteps the
    /// borrow of running a body against `&mut self` and costs a single clone per module. Not a GC root
    /// (it holds bytecode, not heap Values).
    managed_modules: Vec<Module>,
    /// Each managed module's function table as a shared `Rc`, indexed by `module_id - 1` (module 0 is
    /// the entry, whose functions are threaded, not stored here). A CROSS-module call clones the `Rc`
    /// (cheap) to run the callee against its OWN function table, disjoint from the `&mut self` borrow.
    /// Built once by [`ObjectModel::set_managed_modules`] alongside the registry.
    managed_functions: Vec<Rc<[CodeObject]>>,
    /// Each managed module's top-level BODY as a shared `Rc`, indexed by `module_id - 1`. A module
    /// body runs on the importer's own frame stack (so the collector's safe point reaches it), which
    /// means its code is resolved once per op and cannot be a borrow into the registry.
    managed_bodies: Vec<Rc<CodeObject>>,
    /// The ENTRY module's function table (module 0) as a shared `Rc`, set by [`run_bundle`] before the
    /// entry runs. The entry's own code threads its `functions` slice, but a MANAGED module calling an
    /// entry-defined function value (home 0) reaches its code through this `Rc` -- else it would resolve
    /// the index against the caller's table. `None` for a single-file program (no managed caller exists).
    ///
    /// [`run_bundle`]: crate::interp::run_bundle
    entry_functions: Option<Rc<[CodeObject]>>,
    /// Each managed module's live GLOBAL namespace (name -> value), indexed by `module_id - 1`. A
    /// managed module's top-level bindings and its functions' `LoadGlobal`s resolve here (via
    /// [`ObjectModel::current_module_global`]), NOT the entry's `globals`, so a function defined in
    /// module M resolves its globals against M even when called from another module. Module 0 (entry)
    /// uses `globals`. A GC root (with `globals`), keeping every managed module's bindings reachable.
    managed_globals: Vec<Vec<(String, Value)>>,
    /// The module whose functions + globals the RUNNING code resolves against (0 = entry, k = managed
    /// module k). [`crate::interp`]'s `run_frames` sets it on entry to a drive and restores it on exit,
    /// so a cross-module call runs the callee in the callee's module context; `LoadGlobal` /
    /// `StoreFast` (module body) / `MakeFunction` read it. Not a GC root (a plain id).
    current_module: u16,
    /// Per-function attribute dicts (`f.tag = ...`): a function value's `bits()` -> its `__dict__`.
    /// A function value has no `__dict__` slot, so its user attributes live here, keyed by `bits()` so
    /// a bare `function_ref` (stable per def) and each PyFunction instance (distinct per closure) map
    /// correctly. A GC root (like `modules`), keeping every function attribute dict reachable.
    function_dicts: Vec<(u32, Value)>,
    /// Captured `print(...)` output (the interpreter is `no_std`, so it buffers rather than
    /// writing a stream; the host drains it).
    stdout: String,
    /// The GC type-descriptor id of the `gpio` module singleton (the clean hardware API).
    gpio_type_id: u32,
    /// The GC type-descriptor id of the `board` pin-name singleton.
    board_type_id: u32,
    /// The GC type-descriptor id of a `Pin` handle (a GC leaf of raw register words).
    pin_type_id: u32,
    /// The GC type-descriptor id of the `uart` module singleton (the clean serial API).
    uart_type_id: u32,
    /// The GC type-descriptor id of a board UART resource (`board.UART0`): one raw instance word.
    uart_resource_type_id: u32,
    /// The GC type-descriptor id of an open `Port` (a GC leaf of raw config/state words).
    uart_port_type_id: u32,
    /// The GC type-descriptor id of the `spi` module singleton (the clean SPI API).
    spi_type_id: u32,
    /// The GC type-descriptor id of a board SPI resource (`board.SPI0`): one raw instance word.
    spi_resource_type_id: u32,
    /// The GC type-descriptor id of an open `SpiBus` (a GC leaf of raw config/state words).
    spi_bus_type_id: u32,
    /// The GC type-descriptor id of the `i2c` module singleton (the clean I2C API).
    i2c_type_id: u32,
    /// The GC type-descriptor id of a board I2C resource (`board.I2C0`): one raw instance word.
    i2c_resource_type_id: u32,
    /// The GC type-descriptor id of an open `I2cBus` (a GC leaf of raw config/state words).
    i2c_bus_type_id: u32,
    /// The GC type-descriptor id of the `adc` module singleton (the clean ADC API).
    adc_type_id: u32,
    /// The GC type-descriptor id of an open FILE: `[handle@0 (raw; u32::MAX once closed), mode@4
    /// (raw; the packed mode flags), name@8 (tagged str)]`.
    file_type_id: u32,
    /// The GC type-descriptor id of a board ADC resource (`board.A0`): `(channel, pin)` raw words.
    adc_resource_type_id: u32,
    /// The GC type-descriptor id of an open ADC `Channel` (a GC leaf of raw config/state words).
    adc_channel_type_id: u32,
    /// The GC type-descriptor id of a shim SPI factory (`machine.SPI` / `busio.SPI`, a callable
    /// carrying its flavor) and a shim SPI instance (the wrapped `SpiBus` + flavor).
    pub(crate) spi_shim_factory_type_id: u32,
    pub(crate) spi_shim_type_id: u32,
    /// The GC type-descriptor id of a shim I2C factory (`machine.I2C` / `busio.I2C`) and a shim I2C
    /// instance (the wrapped `I2cBus` + flavor).
    pub(crate) i2c_shim_factory_type_id: u32,
    pub(crate) i2c_shim_type_id: u32,
    /// The GC type-descriptor id of a shim ADC factory (`machine.ADC` / `analogio.AnalogIn`) and a
    /// shim ADC instance (the wrapped `Channel` + flavor).
    pub(crate) adc_shim_factory_type_id: u32,
    pub(crate) adc_shim_type_id: u32,
    /// The GC type-descriptor id of the `analogio` module singleton (the CircuitPython ADC shim).
    analogio_type_id: u32,
    /// The GC type-descriptor id of the `busio` module singleton (the CircuitPython shim).
    busio_type_id: u32,
    /// The GC type-descriptor id of a shim UART factory (`machine.UART` / `busio.UART`, a
    /// callable carrying its flavor word).
    uart_shim_factory_type_id: u32,
    /// The GC type-descriptor id of a shim UART instance: the wrapped Port (tagged, traced) +
    /// the constructor-held timeout + the flavor.
    uart_shim_type_id: u32,
    /// The GC type-descriptor id of the `machine` module singleton.
    machine_type_id: u32,
    /// The GC type-descriptor id of a `machine.Pin` factory (a callable, carrying OUT/IN).
    pin_factory_type_id: u32,
    /// The GC type-descriptor id of the `digitalio` module singleton.
    digitalio_type_id: u32,
    /// The GC type-descriptor id of a `digitalio.DigitalInOut` factory (a callable).
    dio_factory_type_id: u32,
    /// The GC type-descriptor id of the `digitalio.Direction` enum singleton (OUTPUT/INPUT).
    direction_type_id: u32,
    /// The GC type-descriptor id of a `DigitalInOut` instance -- wraps a clean gpio `Pin` (one
    /// tagged slot), its `value`/`direction` exposed as properties.
    dio_type_id: u32,
    /// The GC type-descriptor id of a `PyFunction` -- a DEFAULTED function object
    /// `[func_index: raw u32 @0, defaults: tagged tuple|None @4, kwdefaults: tagged dict|None @8]`.
    /// A plain (non-defaulted) function stays a stateless `function_ref` immediate; only a `def`
    /// with default arguments allocates one.
    py_function_type_id: u32,
    /// The GC type-descriptor id of a generator object -- a leaf holding an index into
    /// `generators` (the arena slot of its suspended frame).
    generator_type_id: u32,
    /// The GC type-descriptor id of a `Cell` -- a heap box holding one tagged `Value`, shared
    /// (mutably) between an enclosing function and a nested closure that captures the variable.
    cell_type_id: u32,
    /// The GC type-descriptor id of a lazy iterator (`map`/`filter`/`zip`/`enumerate`): payload
    /// `[kind@0 (raw), state@4 (the function / a counter / None), sources@8 (a tuple of source
    /// iterators)]`. Produces each item on demand rather than materializing a list.
    lazy_iter_type_id: u32,
    /// The GC type-descriptor id of a `staticmethod`/`classmethod` wrapper: payload `[kind@0 (raw:
    /// 0 static, 1 class), func@4 (the wrapped function)]`. Stored in a class namespace; the getattr
    /// path unwraps it (static -> the raw function; class -> the function bound to the class).
    method_wrapper_type_id: u32,
    /// The GC type-descriptor id of a `memoryview`: payload `[base@0 (the viewed bytes/bytearray),
    /// offset@4 (raw), length@8 (raw)]`. A zero-copy 1-D view; reads/writes go straight to the base's
    /// `byte_buffers` slot (a bytearray-backed view is writable, a bytes-backed one read-only).
    memoryview_type_id: u32,
    /// The GC type-descriptor ids of the three dict views (`d.keys()`/`d.values()`/`d.items()`):
    /// payload `[dict@0 (tagged, traced)]` -- a live window onto the dict, not a snapshot. Three
    /// ids so each view is its own type (`dict_keys`/`dict_values`/`dict_items`).
    dict_keys_type_id: u32,
    dict_values_type_id: u32,
    dict_items_type_id: u32,
    /// The GC type-descriptor id of a `collections.defaultdict` -- a dict SUBTYPE: payload
    /// `[dicts-arena index@0 (raw, dict-identical), default_factory@4 (tagged)]`. Every dict
    /// operation accepts it via [`ObjectModel::dict_slot`]; only missing-key subscript, repr, and
    /// `type()` differ.
    defaultdict_type_id: u32,
    /// The GC type-descriptor ids of `collections.Counter` / `collections.OrderedDict` -- dict
    /// SUBTYPES with dict-identical one-slot payloads (see [`ObjectModel::dict_slot`]); the type id
    /// alone selects the subtype behaviors.
    counter_type_id: u32,
    ordereddict_type_id: u32,
    /// The GC type-descriptor id of a `collections.deque`: payload `[seqs-arena index@0 (raw),
    /// maxlen@4 (raw; u32::MAX = unbounded)]`. Its own type over the shared seqs arena.
    deque_type_id: u32,
    /// The GC type-descriptor ids of a namedtuple CLASS (`[name@0, fields@4]`, both tagged) and a
    /// namedtuple INSTANCE (`[seqs-arena index@0 raw, class@4 tagged]` -- a tuple SUBTYPE via
    /// [`ObjectModel::tuple_slot`]).
    ntclass_type_id: u32,
    ntinstance_type_id: u32,
    /// The GC type-descriptor id of a `property`: payload `[fget@0, fset@4, fdel@8]` (each a function
    /// or None). Stored in a class namespace; the interpreter's attribute access calls the accessor.
    property_type_id: u32,
    /// The GC type-descriptor id of an unbound built-in method (`str.lower`, `str.strip`, ...): a
    /// one-slot payload holding the method NAME. Called with the receiver as the first argument
    /// (`str.lower(s)` == `s.lower()`), so it works as a `key=`/`map` function.
    unbound_method_type_id: u32,
    /// The GC type-descriptor id of a bare `object()` instance -- an attribute-less identity token
    /// (the `object()` sentinel). Its only observable trait is its own address (`is` / `id`).
    object_base_type_id: u32,
    /// The suspended frames of live generators, indexed by a generator object's payload word.
    /// `None` = the generator is exhausted (its body returned) or currently running; `Some(frame)`
    /// = it is fresh (ip 0) or suspended at a `yield`. A suspended frame holds tagged Values
    /// (locals, eval stack) that are GC roots, so a moving collector traces each frame here. The
    /// interpreter does not auto-collect, so a finished generator's slot is not reclaimed.
    generators: Vec<Option<Frame>>,
    /// A pool of returned call frames, kept for their Vec allocations (locals/eval-stack/caches) so a
    /// hot call/return cycle reuses buffers instead of allocating a fresh frame each call. Bounded;
    /// every pooled frame is cleared of Values (holds nothing to trace).
    frame_pool: Vec<Frame>,
    /// App-claimed GPIO pins -- the one-owner-per-pin reservation. A second claim of a held pin
    /// fails LOUD (an `OSError`, CPython's already-in-use flavor), never a silent register race.
    gpio_claimed: Vec<u32>,
    /// App-claimed UART instances -- one owner per port, same fail-loud rule as pins.
    uart_claimed: Vec<u32>,
    /// The register facts of the open UART role, for a board whose values come from its generated
    /// module rather than a table in this crate. Resolved once when the role is opened -- BEFORE
    /// the bring-up sequence runs, so the host sim models the right registers from the first write
    /// -- and `None` until then, because such a board has no second source of addresses to fall
    /// back on. (One entry: the bound family exposes a single UART role.)
    resolved_uart_facts: Option<crate::tables::uart_samd21::Samd21UartFacts>,
    /// App-claimed SPI bus instances -- one owner per bus, same fail-loud rule.
    spi_claimed: Vec<u32>,
    /// App-claimed I2C bus instances -- one owner per bus, same fail-loud rule.
    i2c_claimed: Vec<u32>,
    /// Currently-open ADC channels -- each exclusively claimed; the SHARED converter block brings
    /// up on the first open (empty -> non-empty) and releases on the last close.
    adc_channels_open: Vec<u32>,
    /// Firmware-reserved pins (seeded from the target profile): a claim of one fails loud, so an
    /// app-vs-firmware conflict is caught rather than silently colliding.
    gpio_reserved: Vec<u32>,
    /// The volatile MMIO write seam: on device the runner installs `lamella_mmio::write32`; on
    /// the host it is unset and writes fall to the simulated register file.
    /// The host's clocks and its sleep, installed by whatever is embedding this runtime -- a host
    /// program, or a device's timer. There is no fallback: a runtime with no clock cannot answer
    /// what time it is, and returning zero would be a wrong answer rather than a missing one.
    /// Wall clock, in nanoseconds since the Unix epoch.
    /// Collect at every safe point rather than under pressure -- a test instrument (see
    /// `set_gc_stress`), and how the root enumeration is held honest.
    #[cfg(feature = "gc-collect")]
    gc_stress: bool,
    /// Whether a safe point collects under pressure. ON by default -- see `under_memory_pressure`.
    #[cfg(feature = "gc-collect")]
    collect_when_full: bool,
    /// How many interpreter drive loops are running, maintained by the driver itself. The safe point
    /// needs to know whether the loop reaching it is the OUTERMOST one, because only that loop's frame
    /// stack is every frame there is; see `is_outermost_drive`.
    #[cfg(feature = "gc-collect")]
    drive_nesting: u32,
    /// The embedder's console, if it installed one: `print` writes straight through instead of
    /// accumulating. `None` = capture in `stdout` and hand it over on request.
    console_fn: Option<fn(&str)>,
    /// The embedder's view of the memory this model is allocated OUT OF, as `(used, capacity)` bytes:
    /// a device's bump-arena frontier, answered in constant time by whoever owns that arena. `None` on
    /// a host, where there is no such bound -- see [`ObjectModel::set_arena_probe`].
    ///
    /// Only a build that can COLLECT has anything to do with the answer: without the collector, arena
    /// pressure is not actionable, since an allocation-only tier has nothing to reclaim.
    #[cfg(feature = "gc-collect")]
    arena_probe: Option<fn() -> (usize, usize)>,
    /// The host filesystem, installed by the embedder. `None` = no storage: every file verb REFUSES
    /// by name rather than answering an empty read or dropping a write, which would be indistinguishable
    /// from success at the call site.
    file_ops: Option<crate::fileio::FileOps>,
    clock_fn: Option<fn() -> i64>,
    /// A monotonic source, in nanoseconds from an arbitrary origin. Separate from the wall clock
    /// because only this one is guaranteed not to jump when the system clock is adjusted.
    monotonic_fn: Option<fn() -> i64>,
    /// Blocks for a count of nanoseconds.
    sleep_fn: Option<fn(i64)>,
    mmio_write_fn: Option<fn(u32, u32)>,
    /// The volatile MMIO read seam (device: `lamella_mmio::read32`; host: the sim).
    mmio_read_fn: Option<fn(u32) -> u32>,
    /// The target board whose register map the gpio layer drives (named pins, drive/direction/clock
    /// registers). Default = STM32F4; the deployment sets it via [`ObjectModel::set_board`].
    board: crate::gpio::Board,
    /// The host-only simulated register file (the default MMIO target when no seam is installed),
    /// so a driver runs and its register writes are verifiable OFF-device.
    #[cfg(not(target_os = "none"))]
    mmio_sim: alloc::collections::BTreeMap<u32, u32>,
    /// The host UART sim's pending RX bytes (a test injects; FIFO reads pop) -- the behavioral
    /// half the plain register file cannot express.
    #[cfg(not(target_os = "none"))]
    uart_sim_rx: alloc::collections::VecDeque<u8>,
    /// The host UART sim's transmitted bytes (FIFO writes append) -- the TX oracle.
    #[cfg(not(target_os = "none"))]
    uart_sim_tx: Vec<u8>,
    /// The host SPI sim's transmitted (MOSI) bytes -- the TX oracle.
    #[cfg(not(target_os = "none"))]
    spi_sim_tx: Vec<u8>,
    /// The host SPI sim's scripted MISO stream (a test queues; each TX byte consumes one as its
    /// full-duplex reply, 0x00 when exhausted).
    #[cfg(not(target_os = "none"))]
    spi_sim_respond: alloc::collections::VecDeque<u8>,
    /// The host SPI sim's replies waiting to be read back (a data-register write pushes one, a read
    /// pops it) -- so the driver's write-then-read-one transfer loop returns the queued MISO byte.
    #[cfg(not(target_os = "none"))]
    spi_sim_rx_pending: alloc::collections::VecDeque<u8>,
    /// The host sim's accumulated reset-DONE bits: a write to the board's RESETS clear-alias sets
    /// those bits, and a read of the RESETS done register reflects them. Modeled as an accumulator
    /// (not a fixed per-peripheral value) so peripherals SHARING the done register on one board --
    /// UART bit 26, SPI bit 18 -- each see their own bit cleared, not the other's.
    #[cfg(not(target_os = "none"))]
    reset_done_bits: u32,
    /// The host I2C sim's addressable devices (`addr -> register-file device`) -- a test installs
    /// them; a transaction to an absent address NACKs the ADDRESS phase.
    #[cfg(not(target_os = "none"))]
    i2c_sim_devices: alloc::collections::BTreeMap<u8, I2cSimDevice>,
    /// The host I2C sim's received bytes (a read command clocks one from the target; an
    /// IC_DATA_CMD read pops it).
    #[cfg(not(target_os = "none"))]
    i2c_sim_rx: alloc::collections::VecDeque<u8>,
    /// The host I2C sim's current transaction abort source (`IC_TX_ABRT_SOURCE`; 0 = acknowledged).
    #[cfg(not(target_os = "none"))]
    i2c_sim_abort: u32,
    /// The host I2C sim's STOP_DET latch (the bus reached STOP).
    #[cfg(not(target_os = "none"))]
    i2c_sim_stopped: bool,
    /// The host I2C sim's transaction-local flag: the next write byte is the register pointer (SUB).
    #[cfg(not(target_os = "none"))]
    i2c_sim_expect_pointer: bool,
    /// The host ADC sim's scripted conversion results (`channel -> raw count`), the analogue a test
    /// presents at each channel; a read of RESULT returns the currently-selected channel's value.
    #[cfg(not(target_os = "none"))]
    adc_sim_raw: alloc::collections::BTreeMap<u32, u32>,
    /// The host ADC sim's currently-selected channel (tracked from the CS AINSEL writes).
    #[cfg(not(target_os = "none"))]
    adc_sim_ainsel: u32,
    /// The host-only ordered log of every MMIO write, so a test can assert the exact drive
    /// sequence (e.g. a blinky's alternating set/reset) that the last-value sim map cannot show.
    #[cfg(not(target_os = "none"))]
    mmio_trace: Vec<(u32, u32)>,
    /// The `sleep_ms` delay seam (device: a timer/spin; host: a no-op, so the differential is not
    /// slowed by real sleeps).
    delay_fn: Option<fn(u32)>,
    /// A one-shot argument for the NEXT trap-raised built-in exception, so a trap site can attach
    /// context (a `KeyError`'s key, an `IndexError`/`ValueError` message) that the bare `Trap` enum
    /// cannot carry. Set right before returning the bare trap; [`ObjectModel::trap_to_exception`]
    /// takes it as the exception's single arg. Transient: consumed on the immediately following
    /// conversion (a future safe-point collector would trace it as a root while set).
    pending_trap_arg: Option<Value>,
    /// The value a generator's body returned (`return v`, or `None` when it falls through), carried
    /// from the exhausting [`crate::interp::resume_generator`] to the `StopIteration` the resumer
    /// raises (so `StopIteration.value` is `v`). Transient: set at exhaustion and consumed by the
    /// immediately following resume step, with no safe-point in between.
    generator_return: Option<Value>,
    /// An exception thrown INTO a generator suspended in a `yield from` (via `gen.throw`/`gen.close`):
    /// carried from [`crate::interp::resume_generator`] to the re-run YieldFrom arm, which forwards it
    /// to the sub-iterator (`sub.throw`). Transient: set then consumed by the immediately re-driven
    /// YieldFrom op, with no safe-point between.
    yield_from_throw: Option<Value>,
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
        let bytes_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bytearray_type_id = descs.len() as u32;
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
            payload_size: 16,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..4).map(|i| i * 4).collect(),
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
        let gpio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let board_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let pin_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::gpio::PIN_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let uart_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let uart_resource_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let uart_port_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::uart::PORT_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let busio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let uart_shim_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let uart_shim_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::uart::SHIM_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let machine_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let pin_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let digitalio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let dio_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let direction_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let dio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..1).map(|i| i * 4).collect(),
        });
        let py_function_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 20,
            ref_offsets: Vec::new(),
            tagged_offsets: (1..4).map(|i| i * 4).collect(),
        });
        let generator_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let cell_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let lazy_iter_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (1..3).map(|i| i * 4).collect(),
        });
        let method_wrapper_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![4],
        });
        let memoryview_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let dict_keys_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let dict_values_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let dict_items_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let defaultdict_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![4],
        });
        let counter_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let ordereddict_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let deque_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let ntclass_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let ntinstance_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![4],
        });
        let property_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let unbound_method_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let object_base_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let long_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 16,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bigint_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let float_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        #[cfg(feature = "complex")]
        let complex_type_id = descs.len() as u32;
        #[cfg(feature = "complex")]
        descs.push(TypeDesc {
            payload_size: 16,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let module_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let spi_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let spi_resource_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let spi_bus_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::spi::BUS_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let i2c_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let i2c_resource_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let i2c_bus_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::i2c::BUS_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let adc_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let file_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![8],
        });
        let adc_resource_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let adc_channel_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::adc::CH_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let spi_shim_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let spi_shim_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::shims::spi::SHIM_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![crate::shims::spi::SHIM_W_BUS],
        });
        let i2c_shim_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let i2c_shim_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::shims::i2c::SHIM_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![crate::shims::i2c::SHIM_W_BUS],
        });
        let adc_shim_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let adc_shim_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::shims::adc::SHIM_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![crate::shims::adc::SHIM_W_CHANNEL],
        });
        let analogio_type_id = descs.len() as u32;
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
            bigints: Vec::new(),
            byte_buffers: Vec::new(),
            sets: Vec::new(),
            freed_slots: FreedSlots::default(),
            str_type_id,
            bytes_type_id,
            bytearray_type_id,
            long_type_id,
            bigint_type_id,
            float_type_id,
            #[cfg(feature = "complex")]
            complex_type_id,
            module_type_id,
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
            memory_error_reserve: None,
            globals: Vec::new(),
            modules: Vec::new(),
            managed_modules: Vec::new(),
            managed_functions: Vec::new(),
            managed_bodies: Vec::new(),
            entry_functions: None,
            managed_globals: Vec::new(),
            current_module: 0,
            function_dicts: Vec::new(),
            stdout: String::new(),
            gpio_type_id,
            board_type_id,
            pin_type_id,
            uart_type_id,
            uart_resource_type_id,
            uart_port_type_id,
            spi_type_id,
            spi_resource_type_id,
            spi_bus_type_id,
            i2c_type_id,
            i2c_resource_type_id,
            i2c_bus_type_id,
            adc_type_id,
            file_type_id,
            #[cfg(feature = "gc-collect")]
            gc_stress: false,
            #[cfg(feature = "gc-collect")]
            collect_when_full: true,
            #[cfg(feature = "gc-collect")]
            drive_nesting: 0,
            console_fn: None,
            #[cfg(feature = "gc-collect")]
            arena_probe: None,
            file_ops: None,
            adc_resource_type_id,
            adc_channel_type_id,
            spi_shim_factory_type_id,
            spi_shim_type_id,
            i2c_shim_factory_type_id,
            i2c_shim_type_id,
            adc_shim_factory_type_id,
            adc_shim_type_id,
            analogio_type_id,
            busio_type_id,
            uart_shim_factory_type_id,
            uart_shim_type_id,
            machine_type_id,
            pin_factory_type_id,
            digitalio_type_id,
            dio_factory_type_id,
            direction_type_id,
            dio_type_id,
            py_function_type_id,
            generator_type_id,
            cell_type_id,
            lazy_iter_type_id,
            method_wrapper_type_id,
            memoryview_type_id,
            dict_keys_type_id,
            dict_values_type_id,
            dict_items_type_id,
            defaultdict_type_id,
            counter_type_id,
            ordereddict_type_id,
            deque_type_id,
            ntclass_type_id,
            ntinstance_type_id,
            property_type_id,
            unbound_method_type_id,
            object_base_type_id,
            generators: Vec::new(),
            frame_pool: Vec::new(),
            gpio_claimed: Vec::new(),
            uart_claimed: Vec::new(),
            resolved_uart_facts: None,
            spi_claimed: Vec::new(),
            i2c_claimed: Vec::new(),
            adc_channels_open: Vec::new(),
            gpio_reserved: Vec::new(),
            clock_fn: None,
            monotonic_fn: None,
            sleep_fn: None,
            mmio_write_fn: None,
            mmio_read_fn: None,
            board: crate::gpio::Board::default(),
            #[cfg(not(target_os = "none"))]
            mmio_sim: alloc::collections::BTreeMap::new(),
            #[cfg(not(target_os = "none"))]
            uart_sim_rx: alloc::collections::VecDeque::new(),
            #[cfg(not(target_os = "none"))]
            uart_sim_tx: Vec::new(),
            #[cfg(not(target_os = "none"))]
            spi_sim_tx: Vec::new(),
            #[cfg(not(target_os = "none"))]
            spi_sim_respond: alloc::collections::VecDeque::new(),
            #[cfg(not(target_os = "none"))]
            spi_sim_rx_pending: alloc::collections::VecDeque::new(),
            #[cfg(not(target_os = "none"))]
            reset_done_bits: 0,
            #[cfg(not(target_os = "none"))]
            i2c_sim_devices: alloc::collections::BTreeMap::new(),
            #[cfg(not(target_os = "none"))]
            i2c_sim_rx: alloc::collections::VecDeque::new(),
            #[cfg(not(target_os = "none"))]
            i2c_sim_abort: 0,
            #[cfg(not(target_os = "none"))]
            i2c_sim_stopped: false,
            #[cfg(not(target_os = "none"))]
            i2c_sim_expect_pointer: false,
            #[cfg(not(target_os = "none"))]
            adc_sim_raw: alloc::collections::BTreeMap::new(),
            #[cfg(not(target_os = "none"))]
            adc_sim_ainsel: 0,
            #[cfg(not(target_os = "none"))]
            mmio_trace: Vec::new(),
            delay_fn: None,
            pending_trap_arg: None,
            generator_return: None,
            yield_from_throw: None,
        }
    }

    /// The single managed-allocation chokepoint: every `new_*` heap object is allocated here, so
    /// the GC / allocation tier is chosen in ONE place (the `gc(none|bump|collected)` knob).
    /// The default (allocation-capable) tier bumps the moving
    /// heap and returns `None` when full, at which point a collected tier drives `collect()` before
    /// retrying (the interpreter never auto-collects today, so it is allocate-only / bump). The
    /// `gc-none` tier has NO managed heap: every allocation is `None` -> a loud `OutOfMemory`, so a
    /// pure fixnum / mmio / control-flow driver runs (it never allocates) while an allocating
    /// program fails fast -- upholding "runs on interpreter-P => runs on device-P" for the tiniest
    /// micros.
    #[must_use]
    fn alloc_object(&mut self, type_id: u32) -> Option<Ref> {
        #[cfg(feature = "gc-none")]
        {
            let _ = type_id;
            None
        }
        #[cfg(not(feature = "gc-none"))]
        {
            self.heap.alloc(type_id)
        }
    }

    /// Allocates a `str` from `s`, returning a heap-pointer Value. The content is interned
    /// in the string arena and the heap object holds its index.
    pub fn new_str(&mut self, s: &str) -> Result<Value, Trap> {
        let index = take_arena_slot(&mut self.strings, &mut self.freed_slots.strings, String::from(s));
        let reference = self.alloc_object(self.str_type_id).ok_or(Trap::OutOfMemory)?;
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
        } else if let Some(data) = self.bytes_value(value) {
            data.len()
        } else if self.is_memoryview(value) {
            self.memoryview_parts(value).2
        } else if let Some(elems) = self.seq_value(value) {
            elems.len()
        } else if let Some(entries) = self.dict_value(value) {
            entries.len()
        } else if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            range_len(start, stop, step).max(0) as usize
        } else if let Some(elements) = self.set_value(value) {
            elements.len()
        } else if self.is_dict_view(value) {
            self.dict_value(self.dict_view_dict(value)).map_or(0, Vec::len)
        } else if let Some(elems) = self.deque_elems(value) {
            elems.len()
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

    /// A new immutable `bytes` object over `data`.
    pub fn new_bytes(&mut self, data: Vec<u8>) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.bytes_type_id).ok_or(Trap::OutOfMemory)?;
        let index = take_arena_slot(&mut self.byte_buffers, &mut self.freed_slots.byte_buffers, data);
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// A new mutable `bytearray` object over `data`.
    pub fn new_bytearray(&mut self, data: Vec<u8>) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.bytearray_type_id).ok_or(Trap::OutOfMemory)?;
        let index = take_arena_slot(&mut self.byte_buffers, &mut self.freed_slots.byte_buffers, data);
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// The bytes a `bytes` or `bytearray` holds, or `None` for any other value.
    #[must_use]
    pub fn bytes_value(&self, value: Value) -> Option<&[u8]> {
        let reference = value.as_ref()?;
        let type_id = self.heap.type_id_of(reference);
        if type_id != self.bytes_type_id && type_id != self.bytearray_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.byte_buffers.get(index).map(Vec::as_slice)
    }

    /// The `byte_buffers` arena slot of a `bytes`/`bytearray`, for in-place mutation of a bytearray.
    fn byte_buffer_slot(&self, value: Value) -> Option<usize> {
        let reference = value.as_ref()?;
        let type_id = self.heap.type_id_of(reference);
        if type_id != self.bytes_type_id && type_id != self.bytearray_type_id {
            return None;
        }
        Some(self.heap.read_u32(reference.0) as usize)
    }

    /// Whether `value` is a `bytes` (immutable).
    #[must_use]
    pub fn is_bytes(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.bytes_type_id)
    }

    /// Whether `value` is a `bytearray` (mutable).
    #[must_use]
    pub fn is_bytearray(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.bytearray_type_id)
    }

    /// Appends `data` to a `bytearray` IN PLACE (`bytearray += bytes-like`). The object's identity
    /// is unchanged. Rejects a `bytes` receiver (immutable -- its augmented form falls back to the
    /// plain concatenation, rebinding a new `bytes`).
    pub(crate) fn bytearray_extend_in_place(&mut self, bytearray: Value, data: Vec<u8>) -> Result<(), Trap> {
        if !self.is_bytearray(bytearray) {
            return Err(Trap::TypeError);
        }
        let slot = self.byte_buffer_slot(bytearray).ok_or(Trap::TypeError)?;
        self.byte_buffers[slot].extend(data);
        Ok(())
    }

    /// Repeats a `bytearray`'s contents `count` times IN PLACE (`bytearray *= n`; a non-positive
    /// count clears, matching the plain `bytearray * n`).
    pub(crate) fn bytearray_repeat_in_place(&mut self, bytearray: Value, count: i64) -> Result<(), Trap> {
        if !self.is_bytearray(bytearray) {
            return Err(Trap::TypeError);
        }
        let slot = self.byte_buffer_slot(bytearray).ok_or(Trap::TypeError)?;
        let data = core::mem::take(&mut self.byte_buffers[slot]);
        self.byte_buffers[slot] = if count > 0 { data.repeat(count as usize) } else { Vec::new() };
        Ok(())
    }

    /// The dynamic binary-op dispatch (`py_binop`) for object operands: `str` (`+` concatenates,
    /// `* int` repeats), `list`/`tuple` (`+` concatenates the SAME kind, `* int` repeats), and the
    /// set algebra (`| & - ^`). Any other operator, or a mismatched-kind pair (`"a" + 1`,
    /// `[1] + (1,)`, `"a" - "b"`), is a `TypeError` -- Python's rules. Returns `Ok(None)` when
    /// NEITHER operand is an object, so the caller falls back to the numeric path -- the
    /// one-source-of-truth dispatch both the interpreter and the AOT `py_binop` intrinsic consume.
    pub fn py_binary(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Option<Value>, Trap> {
        if self.is_set(lhs) || self.is_frozenset(lhs) {
            return Ok(Some(self.set_binary_op(op, lhs, rhs)?));
        }
        if self.is_str(lhs) || self.is_str(rhs) {
            return Ok(Some(self.str_binary_op(op, lhs, rhs)?));
        }
        if self.bytes_value(lhs).is_some() || self.bytes_value(rhs).is_some() {
            return Ok(Some(self.bytes_binary_op(op, lhs, rhs)?));
        }
        if self.seq_value(lhs).is_some() || self.seq_value(rhs).is_some() {
            return Ok(Some(self.seq_binary_op(op, lhs, rhs)?));
        }
        if op == BinOp::BitOr && self.is_dict(lhs) && self.is_dict(rhs) {
            return Ok(Some(self.dict_merge(lhs, rhs)?));
        }
        Ok(None)
    }

    /// `dict | dict`: a new dict with the left's entries, then the right's (which override on a
    /// shared key) -- CPython 3.9's mapping union.
    fn dict_merge(&mut self, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        let mut entries = self.dict_value(lhs).cloned().unwrap_or_default();
        let right = self.dict_value(rhs).cloned().unwrap_or_default();
        for (key, value) in right {
            match entries.iter().position(|(existing, _)| self.key_eq(*existing, key)) {
                Some(slot) => entries[slot].1 = value,
                None => entries.push((key, value)),
            }
        }
        self.new_dict(entries)
    }

    /// `bytes`/`bytearray` `+` (concatenate two byte strings; the result kind follows the LEFT
    /// operand) and `* int` (repeat; a non-positive count gives empty). Any other combination is a
    /// `TypeError`.
    fn bytes_binary_op(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        match op {
            BinOp::Add => {
                match (self.bytes_value(lhs).map(<[u8]>::to_vec), self.bytes_value(rhs).map(<[u8]>::to_vec)) {
                    (Some(mut a), Some(b)) => {
                        a.extend(b);
                        if self.is_bytearray(lhs) {
                            self.new_bytearray(a)
                        } else {
                            self.new_bytes(a)
                        }
                    }
                    _ => Err(Trap::TypeError),
                }
            }
            BinOp::Mul => {
                let (data, count, bytearray) = if let Some(d) = self.bytes_value(lhs).map(<[u8]>::to_vec) {
                    (d, rhs.as_int().ok_or(Trap::TypeError)?, self.is_bytearray(lhs))
                } else {
                    let d = self.bytes_value(rhs).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?;
                    (d, lhs.as_int().ok_or(Trap::TypeError)?, self.is_bytearray(rhs))
                };
                let repeated = if count > 0 { data.repeat(count as usize) } else { Vec::new() };
                if bytearray {
                    self.new_bytearray(repeated)
                } else {
                    self.new_bytes(repeated)
                }
            }
            _ => Err(Trap::TypeError),
        }
    }

    /// `str + str` (concatenate) and `str * int` / `int * str` (repeat, a non-positive count gives
    /// `""`). Any other combination (`"a" + 1`, `"a" - "b"`, `"a" * "b"`) is a `TypeError`.
    fn str_binary_op(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        match op {
            BinOp::Add => {
                match (self.str_value(lhs).map(String::from), self.str_value(rhs).map(String::from)) {
                    (Some(mut a), Some(b)) => {
                        a.push_str(&b);
                        self.new_str(&a)
                    }
                    _ => Err(Trap::TypeError),
                }
            }
            BinOp::Mul => {
                let (text, count) = if let Some(text) = self.str_value(lhs).map(String::from) {
                    (text, rhs.as_int().ok_or(Trap::TypeError)?)
                } else {
                    let text = self.str_value(rhs).map(String::from).ok_or(Trap::TypeError)?;
                    (text, lhs.as_int().ok_or(Trap::TypeError)?)
                };
                let repeated = if count > 0 { text.repeat(count as usize) } else { String::new() };
                self.new_str(&repeated)
            }
            BinOp::Mod => {
                let template = self.str_value(lhs).map(String::from).ok_or(Trap::TypeError)?;
                let args = if self.is_tuple(rhs) {
                    self.seq_value(rhs).cloned().unwrap_or_default()
                } else {
                    alloc::vec![rhs]
                };
                let rendered = self.percent_format(&template, &args)?;
                self.new_str(&rendered)
            }
            _ => Err(Trap::TypeError),
        }
    }

    /// printf-style `%` formatting for `str % args`: `%d`/`%i` an int, `%s` str(), `%r` repr(),
    /// `%%` a literal `%`; the args are consumed left to right. A conversion with flags/width/
    /// precision (`%5d`) or an unhandled type (`%f`, `%x`) is `Unsupported` (never wrong output);
    /// too few or too many args is a `TypeError` (matching CPython's "not enough/all ... converted").
    fn percent_format(&mut self, template: &str, args: &[Value]) -> Result<String, Trap> {
        let mut out = String::new();
        let chars: Vec<char> = template.chars().collect();
        let mut i = 0;
        let mut next_arg = 0usize;
        let mut used_mapping = false;
        while i < chars.len() {
            if chars[i] != '%' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            i += 1;
            if chars.get(i) == Some(&'%') {
                out.push('%');
                i += 1;
                continue;
            }
            let mapped_arg = if chars.get(i) == Some(&'(') {
                used_mapping = true;
                i += 1;
                let mut key = String::new();
                while let Some(&c) = chars.get(i) {
                    if c == ')' {
                        break;
                    }
                    key.push(c);
                    i += 1;
                }
                if chars.get(i) != Some(&')') {
                    return Err(Trap::ValueError);
                }
                i += 1;
                let dict = *args.first().ok_or(Trap::TypeError)?;
                let found = {
                    let entries = self.dict_value(dict).ok_or(Trap::TypeError)?;
                    entries
                        .iter()
                        .find(|(k, _)| self.str_value(*k) == Some(key.as_str()))
                        .map(|(_, value)| *value)
                };
                match found {
                    Some(value) => Some(value),
                    None => return Err(self.with_message(Trap::KeyError, &key)),
                }
            } else {
                None
            };
            let mut flags = String::new();
            while chars.get(i).is_some_and(|c| matches!(c, '-' | '+' | ' ' | '0' | '#')) {
                flags.push(chars[i]);
                i += 1;
            }
            let mut width = String::new();
            while chars.get(i).is_some_and(char::is_ascii_digit) {
                width.push(chars[i]);
                i += 1;
            }
            let mut precision = 0usize;
            let mut has_precision = false;
            if chars.get(i) == Some(&'.') {
                has_precision = true;
                i += 1;
                while chars.get(i).is_some_and(char::is_ascii_digit) {
                    precision = precision * 10 + (chars[i] as usize - '0' as usize);
                    i += 1;
                }
            }
            let ty = *chars.get(i).ok_or(Trap::ValueError)?;
            i += 1;
            let arg = match mapped_arg {
                Some(value) => value,
                None => {
                    let value = *args.get(next_arg).ok_or(Trap::TypeError)?;
                    next_arg += 1;
                    value
                }
            };
            let width_n = width.parse::<usize>().unwrap_or(0);
            match ty {
                's' | 'r' => {
                    let mut body = if ty == 's' { self.display(arg) } else { self.repr(arg) };
                    if has_precision {
                        body = body.chars().take(precision).collect();
                    }
                    let align = if flags.contains('-') { '<' } else { '>' };
                    out.push_str(&pad_field(&body, width_n, ' ', align));
                }
                _ => {
                    let mut spec = String::new();
                    if flags.contains('-') {
                        spec.push('<');
                    }
                    if flags.contains('+') {
                        spec.push('+');
                    } else if flags.contains(' ') {
                        spec.push(' ');
                    }
                    if flags.contains('#') {
                        spec.push('#');
                    }
                    if flags.contains('0') && !flags.contains('-') {
                        spec.push('0');
                    }
                    let float_ty = matches!(ty, 'f' | 'F' | 'e' | 'E' | 'g' | 'G');
                    if has_precision && !float_ty {
                        let mut digit_spec = String::new();
                        if flags.contains('+') {
                            digit_spec.push('+');
                        } else if flags.contains(' ') {
                            digit_spec.push(' ');
                        }
                        if flags.contains('#') {
                            digit_spec.push('#');
                        }
                        digit_spec.push(if ty == 'i' || ty == 'u' { 'd' } else { ty });
                        let body = zero_pad_int(&self.format_value_spec(arg, &digit_spec)?, precision);
                        let align = if flags.contains('-') { '<' } else { '>' };
                        out.push_str(&pad_field(&body, width_n, ' ', align));
                    } else {
                        spec.push_str(&width);
                        if has_precision && float_ty {
                            spec.push('.');
                            spec.push_str(&alloc::format!("{precision}"));
                        }
                        spec.push(if ty == 'i' || ty == 'u' { 'd' } else { ty });
                        out.push_str(&self.format_value_spec(arg, &spec)?);
                    }
                }
            }
        }
        if !used_mapping && next_arg != args.len() {
            return Err(Trap::TypeError);
        }
        Ok(out)
    }

    /// `list + list` / `tuple + tuple` (concatenate the SAME kind) and `seq * int` / `int * seq`
    /// (repeat, a non-positive count gives an empty sequence). A mismatched-kind `+` (`[1] + (2,)`,
    /// `[1] + 2`) or any other operator is a `TypeError`.
    fn seq_binary_op(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        match op {
            BinOp::Add => {
                let both_lists = self.is_list(lhs) && self.is_list(rhs);
                let both_tuples = self.is_tuple(lhs) && self.is_tuple(rhs);
                if !both_lists && !both_tuples {
                    return Err(Trap::TypeError);
                }
                let mut elements = self.seq_value(lhs).cloned().ok_or(Trap::TypeError)?;
                let rhs_elements = self.seq_value(rhs).cloned().ok_or(Trap::TypeError)?;
                elements.extend(rhs_elements);
                if both_tuples {
                    self.new_tuple(elements)
                } else {
                    self.new_list(elements)
                }
            }
            BinOp::Mul => {
                let (sequence, count) = if self.seq_value(lhs).is_some() {
                    (lhs, rhs.as_int().ok_or(Trap::TypeError)?)
                } else {
                    (rhs, lhs.as_int().ok_or(Trap::TypeError)?)
                };
                let is_tuple = self.is_tuple(sequence);
                let base = self.seq_value(sequence).cloned().ok_or(Trap::TypeError)?;
                let mut elements = Vec::new();
                for _ in 0..count.max(0) {
                    elements.extend_from_slice(&base);
                }
                if is_tuple {
                    self.new_tuple(elements)
                } else {
                    self.new_list(elements)
                }
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
        if self.is_range(lhs) && self.is_range(rhs) && matches!(op, CmpOp::Eq | CmpOp::Ne) {
            let (a_start, a_stop, a_step) = self.range_bounds(lhs);
            let (b_start, b_stop, b_step) = self.range_bounds(rhs);
            let length = |start: i64, stop: i64, step: i64| -> i64 {
                if step > 0 && start < stop {
                    (stop - start - 1) / step + 1
                } else if step < 0 && start > stop {
                    (start - stop - 1) / (-step) + 1
                } else {
                    0
                }
            };
            let a_len = length(a_start, a_stop, a_step);
            let b_len = length(b_start, b_stop, b_step);
            let equal = a_len == b_len
                && (a_len == 0 || (a_start == b_start && (a_len == 1 || a_step == b_step)));
            let holds = if op == CmpOp::Eq { equal } else { !equal };
            return Ok(Some(Value::from_bool(holds)));
        }
        if self.is_set(lhs) || self.is_frozenset(lhs) {
            return Ok(Some(self.set_compare(op, lhs, rhs)?));
        }
        if self.byte_view(lhs).is_some() || self.byte_view(rhs).is_some() {
            return match (self.byte_view(lhs), self.byte_view(rhs)) {
                (Some(a), Some(b)) => {
                    let ord = a.cmp(b);
                    let holds = match op {
                        CmpOp::Eq => ord == Ordering::Equal,
                        CmpOp::Ne => ord != Ordering::Equal,
                        CmpOp::Lt => ord == Ordering::Less,
                        CmpOp::Le => ord != Ordering::Greater,
                        CmpOp::Gt => ord == Ordering::Greater,
                        CmpOp::Ge => ord != Ordering::Less,
                        CmpOp::Is | CmpOp::IsNot => {
                            unreachable!("is/is not handled in the Op::Compare path")
                        }
                    };
                    Ok(Some(Value::from_bool(holds)))
                }
                _ => match op {
                    CmpOp::Eq => Ok(Some(Value::FALSE)),
                    CmpOp::Ne => Ok(Some(Value::TRUE)),
                    _ => Err(Trap::TypeError),
                },
            };
        }
        let both_sequence =
            (self.is_list(lhs) && self.is_list(rhs)) || (self.is_tuple(lhs) && self.is_tuple(rhs));
        if both_sequence {
            if matches!(op, CmpOp::Eq | CmpOp::Ne) {
                let equal = self.key_eq(lhs, rhs);
                let holds = if matches!(op, CmpOp::Eq) { equal } else { !equal };
                return Ok(Some(Value::from_bool(holds)));
            }
            let ord = self.compare_ordered(lhs, rhs)?;
            let holds = match op {
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
                _ => unreachable!("==/!= handled above; is/is not in the Op::Compare path"),
            };
            return Ok(Some(Value::from_bool(holds)));
        }
        if self.is_dict(lhs) && self.is_dict(rhs) {
            return match op {
                CmpOp::Eq => Ok(Some(Value::from_bool(self.dict_equal(lhs, rhs)))),
                CmpOp::Ne => Ok(Some(Value::from_bool(!self.dict_equal(lhs, rhs)))),
                _ => Err(Trap::TypeError),
            };
        }
        if self.is_slice(lhs) && self.is_slice(rhs) {
            let (a_start, a_stop, a_step) = self.slice_components(lhs);
            let (b_start, b_stop, b_step) = self.slice_components(rhs);
            let equal = self.key_eq(a_start, b_start)
                && self.key_eq(a_stop, b_stop)
                && self.key_eq(a_step, b_step);
            return match op {
                CmpOp::Eq => Ok(Some(Value::from_bool(equal))),
                CmpOp::Ne => Ok(Some(Value::from_bool(!equal))),
                _ => Err(Trap::TypeError),
            };
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
                    CmpOp::Is | CmpOp::IsNot => {
                        unreachable!("is/is not handled in the Op::Compare path")
                    }
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
        if value.is_not_implemented() {
            return Err(Trap::TypeError);
        }
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
        if let Some(data) = self.bytes_value(value) {
            return Ok(Some(!data.is_empty()));
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
        if self.is_dict_view(value) {
            let entries = self.dict_value(self.dict_view_dict(value));
            return Ok(Some(entries.is_some_and(|e| !e.is_empty())));
        }
        if let Some(elems) = self.deque_elems(value) {
            return Ok(Some(!elems.is_empty()));
        }
        if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            return Ok(Some(range_len(start, stop, step) > 0));
        }
        if let Some(f) = self.float_value(value) {
            return Ok(Some(f != 0.0));
        }
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            return Ok(Some(re != 0.0 || im != 0.0));
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
        if self.is_deque(container) {
            if self.is_slice(index) {
                let message = "sequence index must be integer, not 'slice'";
                return Err(self.with_message(Trap::TypeError, message));
            }
            let elems = self.deque_elems(container).ok_or(Trap::TypeError)?;
            let len = elems.len() as i64;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "deque index out of range"));
            }
            return Ok(elems[at as usize]);
        }
        if self.is_memoryview(container) {
            let (base, offset, length) = self.memoryview_parts(container);
            if self.is_slice(index) {
                let reference = index.as_ref().ok_or(Trap::TypeError)?;
                let start_v = Value::from_bits(self.heap.read_u32(reference.0));
                let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
                let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
                if !step_v.is_none() && step_v.as_int() != Some(1) {
                    return Err(Trap::Unsupported);
                }
                let (start, stop) = adjust_slice(start_v, stop_v, 1, length as i64)?;
                let low = start.clamp(0, length as i64) as usize;
                let high = stop.clamp(start, length as i64) as usize;
                return self.new_memoryview(base, offset + low, high - low);
            }
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + length as i64 } else { i };
            if at < 0 || at >= length as i64 {
                return Err(self.with_message(Trap::IndexError, "index out of range"));
            }
            let byte = self.memoryview_bytes(container).ok_or(Trap::TypeError)?[at as usize];
            return Value::fixnum(i32::from(byte)).ok_or(Trap::Overflow);
        }
        if self.bytes_value(container).is_some() {
            if self.is_slice(index) {
                let selected = self.slice_bytes(container, index)?;
                return if self.is_bytearray(container) {
                    self.new_bytearray(selected)
                } else {
                    self.new_bytes(selected)
                };
            }
            let data = self.bytes_value(container).ok_or(Trap::TypeError)?;
            let len = data.len() as i64;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "index out of range"));
            }
            return Value::fixnum(i32::from(data[at as usize])).ok_or(Trap::Overflow);
        }
        if self.str_value(container).is_some() {
            if self.is_slice(index) {
                return self.str_getitem_slice(container, index);
            }
            let Some(i) = index.as_int() else {
                return Err(self.index_type_error(container, index));
            };
            let resolved = {
                let s = self.str_value(container).ok_or(Trap::TypeError)?;
                let len = s.chars().count() as i64;
                let at = if i < 0 { i + len } else { i };
                if at < 0 || at >= len {
                    None
                } else {
                    s.chars().nth(at as usize)
                }
            };
            let Some(ch) = resolved else {
                return Err(self.with_message(Trap::IndexError, "string index out of range"));
            };
            let mut buf = [0u8; 4];
            return self.new_str(ch.encode_utf8(&mut buf));
        }
        if self.seq_value(container).is_some() {
            if self.is_slice(index) {
                return self.seq_getitem_slice(container, index);
            }
            let Some(i) = index.as_int() else {
                return Err(self.index_type_error(container, index));
            };
            let (resolved, is_tuple) = {
                let elems = self.seq_value(container).ok_or(Trap::TypeError)?;
                let len = elems.len() as i64;
                let at = if i < 0 { i + len } else { i };
                let resolved = if at < 0 || at >= len { None } else { Some(elems[at as usize]) };
                (resolved, self.is_tuple(container))
            };
            match resolved {
                Some(value) => return Ok(value),
                None => {
                    let message = if is_tuple {
                        "tuple index out of range"
                    } else {
                        "list index out of range"
                    };
                    return Err(self.with_message(Trap::IndexError, message));
                }
            }
        }
        if self.dict_value(container).is_some() {
            let found = {
                let entries = self.dict_value(container).ok_or(Trap::TypeError)?;
                entries
                    .iter()
                    .find(|(k, _)| self.key_eq(*k, index))
                    .map(|(_, v)| *v)
            };
            return match found {
                Some(value) => Ok(value),
                None => {
                    self.set_trap_arg(index);
                    Err(Trap::KeyError)
                }
            };
        }
        if self.is_range(container) {
            let (start, stop, step) = self.range_bounds(container);
            let len = range_len(start, stop, step);
            if self.is_slice(index) {
                let reference = index.as_ref().ok_or(Trap::TypeError)?;
                let start_v = Value::from_bits(self.heap.read_u32(reference.0));
                let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
                let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
                let sub_step = if step_v.is_none() {
                    1
                } else {
                    let s = step_v.as_int().ok_or(Trap::TypeError)?;
                    if s == 0 {
                        return Err(Trap::ValueError);
                    }
                    s
                };
                let (sub_start, sub_stop) = adjust_slice(start_v, stop_v, sub_step, len)?;
                return self.new_range(start + sub_start * step, start + sub_stop * step, step * sub_step);
            }
            let Some(i) = index.as_int() else {
                return Err(self.index_type_error(container, index));
            };
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "range object index out of range"));
            }
            return Value::fixnum((start + at * step) as i32).ok_or(Trap::Overflow);
        }
        let message = alloc::format!("'{}' object is not subscriptable", self.type_name_of(container));
        Err(self.with_message(Trap::TypeError, &message))
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

    /// The bytes a slice selects from a `bytes`/`bytearray` (clamping, negative bounds, any step).
    fn slice_bytes(&self, container: Value, slice: Value) -> Result<Vec<u8>, Trap> {
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
        let data = self.bytes_value(container).ok_or(Trap::TypeError)?;
        let len = data.len() as i64;
        let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
        let mut out = Vec::new();
        let mut i = start;
        while (step > 0 && i < stop) || (step < 0 && i > stop) {
            if i >= 0 && i < len {
                out.push(data[i as usize]);
            }
            i += step;
        }
        Ok(out)
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
        let reference = self.alloc_object(self.slice_type_id).ok_or(Trap::OutOfMemory)?;
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

    /// The `(start, stop, step)` bounds of a slice object -- each an int `Value` or `None` (the
    /// caller has established `is_slice`). Backs `slice.start`/`.stop`/`.step`, its repr, and `==`.
    pub(crate) fn slice_components(&self, value: Value) -> (Value, Value, Value) {
        let reference = value.as_ref().expect("a slice");
        let start = Value::from_bits(self.heap.read_u32(reference.0));
        let stop = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        (start, stop, step)
    }

    /// Allocates a `range(start, stop, step)` -- a lazy int sequence (the bounds are fixnums,
    /// so an i32-range; a wider range would overflow, matching the corpus's needs).
    pub fn new_range(&mut self, start: i64, stop: i64, step: i64) -> Result<Value, Trap> {
        let s = Value::fixnum(i32::try_from(start).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let e = Value::fixnum(i32::try_from(stop).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let t = Value::fixnum(i32::try_from(step).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let reference = self.alloc_object(self.range_type_id).ok_or(Trap::OutOfMemory)?;
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

    /// The `seqs`-arena index if `value` is a `tuple` OR a tuple SUBTYPE (a namedtuple instance)
    /// -- both keep the arena index at payload offset 0, so every tuple operation that routes
    /// through here treats an instance as the tuple it is.
    fn tuple_slot(&self, value: Value) -> Option<usize> {
        self.container_slot(value, self.tuple_type_id)
            .or_else(|| self.container_slot(value, self.ntinstance_type_id))
    }

    /// The `seqs`-arena index if `value` is a `list` or `tuple` (incl. a tuple subtype).
    fn seq_slot(&self, value: Value) -> Option<usize> {
        self.container_slot(value, self.list_type_id).or_else(|| self.tuple_slot(value))
    }

    /// The `dicts`-arena index if `value` is a `dict` OR a dict SUBTYPE (`defaultdict`) -- both
    /// keep the arena index at payload offset 0, so every dict operation that routes through here
    /// treats a subtype as the dict it is; only the subtype-specific behaviors (missing-key
    /// subscript, repr, `type()`) branch on the concrete type id.
    fn dict_slot(&self, value: Value) -> Option<usize> {
        self.container_slot(value, self.dict_type_id)
            .or_else(|| self.container_slot(value, self.defaultdict_type_id))
            .or_else(|| self.container_slot(value, self.counter_type_id))
            .or_else(|| self.container_slot(value, self.ordereddict_type_id))
    }

    /// The elements if `value` is a `list` or `tuple`.
    pub(crate) fn seq_value(&self, value: Value) -> Option<&Vec<Value>> {
        self.seq_slot(value).and_then(|i| self.seqs.get(i))
    }

    /// The key/value pairs if `value` is a `dict` (or a dict subtype).
    pub(crate) fn dict_value(&self, value: Value) -> Option<&Vec<(Value, Value)>> {
        self.dict_slot(value).and_then(|i| self.dicts.get(i))
    }

    /// The value bound to the string key `name` in `dict`, or `None` if `dict` is not a dict or has
    /// no such key. Backs the class-body namespace lookup (`LoadName`).
    #[must_use]
    pub(crate) fn dict_get_str(&self, dict: Value, name: &str) -> Option<Value> {
        self.dict_value(dict)?
            .iter()
            .find(|(key, _)| self.str_value(*key) == Some(name))
            .map(|(_, value)| *value)
    }

    /// A clone of a dict's `(key, value)` pairs, if `value` is a dict (so a caller can rebuild
    /// or copy the dict without holding a borrow on the model). `dict(other_dict)`.
    #[must_use]
    pub fn dict_entries(&self, value: Value) -> Option<Vec<(Value, Value)>> {
        self.dict_value(value).cloned()
    }

    /// Allocates a `collections.defaultdict` over `pairs` (duplicate keys collapsing like a dict
    /// display) with `factory` as its `default_factory` (`Value::NONE` = no factory). A dict
    /// SUBTYPE: the arena index sits at offset 0 exactly like a dict, so every dict operation
    /// accepts it via [`ObjectModel::dict_slot`]; a missing-key subscript calls the factory.
    pub(crate) fn new_defaultdict(
        &mut self,
        factory: Value,
        pairs: Vec<(Value, Value)>,
    ) -> Result<Value, Trap> {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for (key, value) in pairs {
            match entries.iter().position(|(k, _)| self.key_eq(*k, key)) {
                Some(slot) => entries[slot].1 = value,
                None => entries.push((key, value)),
            }
        }
        let index = take_arena_slot(&mut self.dicts, &mut self.freed_slots.dicts, entries);
        let reference = self.alloc_object(self.defaultdict_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        self.heap.write_u32(reference.0 + 4, factory.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `collections.defaultdict`.
    #[must_use]
    pub fn is_defaultdict(&self, value: Value) -> bool {
        self.container_slot(value, self.defaultdict_type_id).is_some()
    }

    /// Allocates a `collections.Counter` over `entries` (already deduped/counted by the caller --
    /// the constructor and the operator arms fold counts before allocating).
    pub(crate) fn new_counter(&mut self, entries: Vec<(Value, Value)>) -> Result<Value, Trap> {
        let index = take_arena_slot(&mut self.dicts, &mut self.freed_slots.dicts, entries);
        let reference = self.alloc_object(self.counter_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `collections.Counter`.
    #[must_use]
    pub fn is_counter(&self, value: Value) -> bool {
        self.container_slot(value, self.counter_type_id).is_some()
    }

    /// A Counter's entries in DISPLAY order: count-descending, STABLE (ties keep insertion order)
    /// -- CPython's most_common ordering, shared by the repr. Entries with a non-int count keep
    /// pure insertion order (CPython's repr falls back the same way when counts don't sort).
    pub(crate) fn counter_display_entries(&self, entries: Vec<(Value, Value)>) -> Vec<(Value, Value)> {
        let mut counts = Vec::with_capacity(entries.len());
        for (_, v) in &entries {
            match self.as_i128(*v) {
                Some(n) => counts.push(n),
                None => return entries,
            }
        }
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by_key(|&i| core::cmp::Reverse(counts[i]));
        order.into_iter().map(|i| entries[i]).collect()
    }

    /// Allocates a `collections.OrderedDict` over `pairs` (duplicate keys collapsing like a dict
    /// display: last value wins, the key keeps its first position).
    pub(crate) fn new_ordereddict(&mut self, pairs: Vec<(Value, Value)>) -> Result<Value, Trap> {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for (key, value) in pairs {
            match entries.iter().position(|(k, _)| self.key_eq(*k, key)) {
                Some(slot) => entries[slot].1 = value,
                None => entries.push((key, value)),
            }
        }
        let index = take_arena_slot(&mut self.dicts, &mut self.freed_slots.dicts, entries);
        let reference = self.alloc_object(self.ordereddict_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `collections.OrderedDict`.
    #[must_use]
    pub fn is_ordereddict(&self, value: Value) -> bool {
        self.container_slot(value, self.ordereddict_type_id).is_some()
    }

    /// Allocates a `collections.deque` over `elements`, keeping only the LAST `maxlen` of them
    /// when bounded (CPython's constructor rule).
    pub(crate) fn new_deque(
        &mut self,
        mut elements: Vec<Value>,
        maxlen: Option<usize>,
    ) -> Result<Value, Trap> {
        if let Some(m) = maxlen {
            if elements.len() > m {
                elements.drain(..elements.len() - m);
            }
        }
        let index = take_arena_slot(&mut self.seqs, &mut self.freed_slots.seqs, elements);
        let reference = self.alloc_object(self.deque_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        self.heap.write_u32(reference.0 + 4, maxlen.map_or(u32::MAX, |m| m as u32));
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `collections.deque`.
    #[must_use]
    pub fn is_deque(&self, value: Value) -> bool {
        self.container_slot(value, self.deque_type_id).is_some()
    }

    /// The `seqs`-arena slot of a deque.
    fn deque_slot(&self, value: Value) -> Option<usize> {
        self.container_slot(value, self.deque_type_id)
    }

    /// A deque's elements (`None` if `value` is not a deque).
    pub(crate) fn deque_elems(&self, value: Value) -> Option<&Vec<Value>> {
        self.deque_slot(value).and_then(|i| self.seqs.get(i))
    }

    /// A deque's maxlen (`None` = unbounded); the outer `None` means `value` is not a deque.
    pub(crate) fn deque_maxlen(&self, value: Value) -> Option<Option<usize>> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.deque_type_id {
            return None;
        }
        let raw = self.heap.read_u32(reference.0 + 4);
        Some(if raw == u32::MAX { None } else { Some(raw as usize) })
    }

    /// Pushes onto a deque's BACK, evicting from the FRONT past `maxlen` (a zero maxlen keeps it
    /// empty, as CPython does).
    fn deque_push_back(&mut self, slot: usize, maxlen: Option<usize>, item: Value) {
        self.seqs[slot].push(item);
        if let Some(m) = maxlen {
            while self.seqs[slot].len() > m {
                self.seqs[slot].remove(0);
            }
        }
    }

    /// Pushes onto a deque's FRONT, evicting from the BACK past `maxlen`.
    fn deque_push_front(&mut self, slot: usize, maxlen: Option<usize>, item: Value) {
        self.seqs[slot].insert(0, item);
        if let Some(m) = maxlen {
            while self.seqs[slot].len() > m {
                self.seqs[slot].pop();
            }
        }
    }

    /// Allocates a namedtuple CLASS (`namedtuple(name, fields)` -- the factory's result): the
    /// class name plus the field-name tuple, both interned as values.
    pub(crate) fn new_ntclass(&mut self, name: &str, fields: &[String]) -> Result<Value, Trap> {
        let name_value = self.new_str(name)?;
        let mut field_values = Vec::with_capacity(fields.len());
        for field in fields {
            field_values.push(self.new_str(field)?);
        }
        let fields_tuple = self.new_tuple(field_values)?;
        let reference = self.alloc_object(self.ntclass_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, name_value.bits());
        self.heap.write_u32(reference.0 + 4, fields_tuple.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a namedtuple CLASS.
    #[must_use]
    pub fn is_ntclass(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.ntclass_type_id)
    }

    /// A namedtuple class's name (empty for a non-ntclass).
    pub(crate) fn ntclass_name(&self, class: Value) -> String {
        if !self.is_ntclass(class) {
            return String::new();
        }
        self.str_value(self.read_slot(class, 0)).map(String::from).unwrap_or_default()
    }

    /// A namedtuple class's field names, in declaration order (empty for a non-ntclass).
    pub(crate) fn ntclass_fields(&self, class: Value) -> Vec<String> {
        if !self.is_ntclass(class) {
            return Vec::new();
        }
        let fields_tuple = self.read_slot(class, 1);
        self.seq_value(fields_tuple)
            .map(|elems| {
                elems
                    .iter()
                    .map(|&f| self.str_value(f).map(String::from).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The `_fields` tuple of a namedtuple class (the stored tuple itself).
    pub(crate) fn ntclass_fields_tuple(&self, class: Value) -> Value {
        if !self.is_ntclass(class) {
            return Value::NONE;
        }
        self.read_slot(class, 1)
    }

    /// Allocates a namedtuple INSTANCE of `class` over `elements` (already bound to the fields by
    /// the caller). A tuple subtype: the elements live in the seqs arena at offset 0.
    pub(crate) fn new_ntinstance(&mut self, class: Value, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = take_arena_slot(&mut self.seqs, &mut self.freed_slots.seqs, elements);
        let reference = self.alloc_object(self.ntinstance_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        self.heap.write_u32(reference.0 + 4, class.bits());
        Ok(Value::from_ref(reference))
    }

    /// The namedtuple class of `value` if it is a namedtuple INSTANCE (`None` otherwise).
    pub(crate) fn ntinstance_class(&self, value: Value) -> Option<Value> {
        value
            .as_ref()
            .filter(|r| self.heap.type_id_of(*r) == self.ntinstance_type_id)
            .map(|r| Value::from_bits(self.heap.read_u32(r.0 + 4)))
    }

    /// Whether `value` is a namedtuple INSTANCE.
    #[must_use]
    pub fn is_ntinstance(&self, value: Value) -> bool {
        self.ntinstance_class(value).is_some()
    }

    /// The namedtuple-instance method dispatch: `_asdict` here, everything else (index/count)
    /// delegated to the tuple methods the instance inherits. `_replace` takes KEYWORDS and is
    /// handled in the interpreter's keyword-call path instead.
    pub(crate) fn call_nt_method(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        match method_id {
            NT_ASDICT => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let class = self.ntinstance_class(receiver).ok_or(Trap::TypeError)?;
                let fields = self.ntclass_fields(class);
                let elements = self.seq_value(receiver).cloned().unwrap_or_default();
                let mut pairs = Vec::with_capacity(fields.len());
                for (field, element) in fields.iter().zip(elements) {
                    let key = self.new_str(field)?;
                    pairs.push((key, element));
                }
                self.new_dict(pairs)
            }
            NT_REPLACE => Err(Trap::TypeError),
            _ => self.call_tuple_method(receiver, method_id, args),
        }
    }

    /// The interp-aware `collections.deque` method dispatch. `count`/`remove` match elements by
    /// `elem_eq` (a user `__eq__` participates) and `extend`/`extendleft` collect any iterable (a
    /// generator source works), which is why the whole dispatch takes the interpreter context.
    pub(crate) fn call_deque_method_dyn(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        let slot = self.deque_slot(receiver).ok_or(Trap::TypeError)?;
        let maxlen = self.deque_maxlen(receiver).unwrap_or(None);
        match method_id {
            DEQUE_APPEND | DEQUE_APPENDLEFT => {
                let [item] = args else {
                    return Err(Trap::TypeError);
                };
                if method_id == DEQUE_APPEND {
                    self.deque_push_back(slot, maxlen, *item);
                } else {
                    self.deque_push_front(slot, maxlen, *item);
                }
                Ok(Value::NONE)
            }
            DEQUE_POP | DEQUE_POPLEFT => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                if self.seqs[slot].is_empty() {
                    return Err(self.with_message(Trap::IndexError, "pop from an empty deque"));
                }
                Ok(if method_id == DEQUE_POP {
                    self.seqs[slot].pop().unwrap_or(Value::NONE)
                } else {
                    self.seqs[slot].remove(0)
                })
            }
            DEQUE_EXTEND | DEQUE_EXTENDLEFT => {
                let [iterable] = args else {
                    return Err(Trap::TypeError);
                };
                let items = crate::builtins::collect_iterable(self, &[*iterable], functions, depth)?;
                for item in items {
                    if method_id == DEQUE_EXTEND {
                        self.deque_push_back(slot, maxlen, item);
                    } else {
                        self.deque_push_front(slot, maxlen, item);
                    }
                }
                Ok(Value::NONE)
            }
            DEQUE_ROTATE => {
                let n = match args {
                    [] => 1i64,
                    [n] => n.as_int().ok_or(Trap::TypeError)?,
                    _ => return Err(Trap::TypeError),
                };
                let len = self.seqs[slot].len();
                if len > 0 {
                    let k = n.rem_euclid(len as i64) as usize;
                    self.seqs[slot].rotate_right(k);
                }
                Ok(Value::NONE)
            }
            DEQUE_CLEAR => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.seqs[slot].clear();
                Ok(Value::NONE)
            }
            DEQUE_COPY => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let elements = self.seqs[slot].clone();
                let maxlen = self.deque_maxlen(receiver).unwrap_or(None);
                self.new_deque(elements, maxlen)
            }
            DEQUE_COUNT => {
                let [element] = args else {
                    return Err(Trap::TypeError);
                };
                let elems = self.seqs[slot].clone();
                let mut n = 0i64;
                for e in elems {
                    if crate::interp::elem_eq(*element, e, functions, self, depth)? {
                        n += 1;
                    }
                }
                self.int_from_i128(i128::from(n))
            }
            DEQUE_REMOVE => {
                let [element] = args else {
                    return Err(Trap::TypeError);
                };
                let elems = self.seqs[slot].clone();
                for (at, e) in elems.into_iter().enumerate() {
                    if crate::interp::elem_eq(*element, e, functions, self, depth)? {
                        self.seqs[slot].remove(at);
                        return Ok(Value::NONE);
                    }
                }
                let message = "deque.remove(x): x not in deque";
                Err(self.with_message(Trap::ValueError, message))
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Replaces a dict's (or dict subtype's) entries wholesale -- the in-place Counter operators
    /// compute the result entries, then swap them in, preserving the object's identity.
    pub(crate) fn dict_replace_entries(
        &mut self,
        dict: Value,
        entries: Vec<(Value, Value)>,
    ) -> Result<(), Trap> {
        let index = self.dict_slot(dict).ok_or(Trap::TypeError)?;
        self.dicts[index] = entries;
        Ok(())
    }

    /// An int `Value` from an `i128` count: a fixnum when it fits, else a long. The Counter
    /// arithmetic's result constructor.
    pub(crate) fn int_from_i128(&mut self, n: i128) -> Result<Value, Trap> {
        if let Some(v) = i32::try_from(n).ok().and_then(Value::fixnum) {
            return Ok(v);
        }
        self.new_long(n)
    }

    /// A defaultdict's `default_factory` (`Value::NONE` for none); `None` if `value` is not a
    /// defaultdict.
    pub(crate) fn defaultdict_factory(&self, value: Value) -> Option<Value> {
        value
            .as_ref()
            .filter(|r| self.heap.type_id_of(*r) == self.defaultdict_type_id)
            .map(|r| Value::from_bits(self.heap.read_u32(r.0 + 4)))
    }

    /// Allocates a dict VIEW over `dict` (`d.keys()`/`d.values()`/`d.items()`): a one-slot object
    /// holding the dict itself. Live, not a snapshot -- every read goes through to the dict's
    /// current entries, so mutations after the view was taken are visible.
    pub(crate) fn new_dict_view(&mut self, dict: Value, kind: DictViewKind) -> Result<Value, Trap> {
        let type_id = match kind {
            DictViewKind::Keys => self.dict_keys_type_id,
            DictViewKind::Values => self.dict_values_type_id,
            DictViewKind::Items => self.dict_items_type_id,
        };
        let reference = self.alloc_object(type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, dict.bits());
        Ok(Value::from_ref(reference))
    }

    /// The view kind if `value` is a dict view, `None` otherwise.
    pub(crate) fn dict_view_kind(&self, value: Value) -> Option<DictViewKind> {
        let reference = value.as_ref()?;
        let type_id = self.heap.type_id_of(reference);
        if type_id == self.dict_keys_type_id {
            Some(DictViewKind::Keys)
        } else if type_id == self.dict_values_type_id {
            Some(DictViewKind::Values)
        } else if type_id == self.dict_items_type_id {
            Some(DictViewKind::Items)
        } else {
            None
        }
    }

    /// Whether `value` is any of the three dict views.
    pub(crate) fn is_dict_view(&self, value: Value) -> bool {
        self.dict_view_kind(value).is_some()
    }

    /// The dict a view wraps. `None` (never a valid dict) if `view` is not a view.
    pub(crate) fn dict_view_dict(&self, view: Value) -> Value {
        view.as_ref()
            .map_or(Value::NONE, |r| Value::from_bits(self.heap.read_u32(r.0)))
    }

    /// Materializes a view's CURRENT elements: the dict's keys / values / `(key, value)` tuples
    /// (the items kind allocates one tuple per entry). `None` if `value` is not a view.
    pub(crate) fn dict_view_elems(&mut self, value: Value) -> Result<Option<Vec<Value>>, Trap> {
        let Some(kind) = self.dict_view_kind(value) else {
            return Ok(None);
        };
        let entries = self.dict_value(self.dict_view_dict(value)).cloned().unwrap_or_default();
        let elems = match kind {
            DictViewKind::Keys => entries.iter().map(|(k, _)| *k).collect(),
            DictViewKind::Values => entries.iter().map(|(_, v)| *v).collect(),
            DictViewKind::Items => {
                let mut items = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    items.push(self.new_tuple(alloc::vec![key, value])?);
                }
                items
            }
        };
        Ok(Some(elems))
    }

    /// Whether `value` is a `list`.
    #[must_use]
    pub fn is_list(&self, value: Value) -> bool {
        self.container_slot(value, self.list_type_id).is_some()
    }

    /// Whether `value` is a `tuple` (a namedtuple instance included -- it IS one).
    #[must_use]
    pub fn is_tuple(&self, value: Value) -> bool {
        self.tuple_slot(value).is_some()
    }

    /// Whether `value` is a `dict`.
    #[must_use]
    pub fn is_dict(&self, value: Value) -> bool {
        self.dict_slot(value).is_some()
    }

    /// Allocates a `list` over `elements` (a mutable sequence). The elements live in the
    /// backing arena; the heap object holds the index.
    pub fn new_list(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = take_arena_slot(&mut self.seqs, &mut self.freed_slots.seqs, elements);
        let reference = self.alloc_object(self.list_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Extends a `list` with `items` IN PLACE (`list += iterable` -- the interpreter collects the
    /// iterable interp-aware first, so a generator source works). The list object's identity is
    /// unchanged, so every alias observes the growth.
    pub(crate) fn list_extend_in_place(&mut self, list: Value, items: Vec<Value>) -> Result<(), Trap> {
        let index = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
        self.seqs[index].extend(items);
        Ok(())
    }

    /// Repeats a `list`'s contents `count` times IN PLACE (`list *= n`; a non-positive count
    /// clears, matching the plain `list * n`). The list object's identity is unchanged.
    pub(crate) fn list_repeat_in_place(&mut self, list: Value, count: i64) -> Result<(), Trap> {
        let index = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
        let base = core::mem::take(&mut self.seqs[index]);
        let mut elements = Vec::new();
        for _ in 0..count.max(0) {
            elements.extend_from_slice(&base);
        }
        self.seqs[index] = elements;
        Ok(())
    }

    /// Allocates a `tuple` over `elements` (an immutable sequence).
    pub fn new_tuple(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = take_arena_slot(&mut self.seqs, &mut self.freed_slots.seqs, elements);
        let reference = self.alloc_object(self.tuple_type_id).ok_or(Trap::OutOfMemory)?;
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
        self.alloc_dict(entries)
    }

    /// Allocates a `dict` over already-deduped `entries` -- the shared tail of [`ObjectModel::new_dict`]
    /// (value-equality dedup) and [`ObjectModel::new_dict_dyn`] (interp-aware `__eq__` dedup).
    fn alloc_dict(&mut self, entries: Vec<(Value, Value)>) -> Result<Value, Trap> {
        let index = take_arena_slot(&mut self.dicts, &mut self.freed_slots.dicts, entries);
        let reference = self.alloc_object(self.dict_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `dict` from `pairs`, deduping keys interp-aware (an element's `__eq__`, so a value
    /// object collapses with an equal key; the last value wins, the key keeps its first position) --
    /// the dict literal (`BuildDict`). Mirrors [`ObjectModel::new_dict`] but consults `__eq__`; see
    /// [`ObjectModel::dict_find_dyn`] for the guard + caveat.
    pub(crate) fn new_dict_dyn(
        &mut self,
        pairs: Vec<(Value, Value)>,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        let entries = crate::interp::dedup_pairs(pairs, functions, self, depth)?;
        self.alloc_dict(entries)
    }

    /// Finds the slot of `key` in `dict`, honoring a user `__eq__` on the query key or a stored key
    /// (a value object used as a dict key looks up correctly). Fast path: when neither the query key
    /// nor any stored key is a user instance, no `__eq__` can participate, so the identity/value
    /// `key_eq` scan is exact -- no clone, no interpreter re-entry. Only when an instance is involved
    /// does it clone the keys (Values are `Copy`) and rescan with [`crate::interp::elem_eq`], which
    /// re-enters the interpreter to run `__eq__`.
    ///
    /// Caveat: the guard tests whether a KEY is *directly* a user instance; a container key that merely
    /// CONTAINS an instance (a tuple `(V(1),)`) still compares by `key_eq`, so a custom `__eq__` nested
    /// inside a tuple key is not consulted. The direct-instance-key pattern (the common case) is covered.
    pub(crate) fn dict_find_dyn(
        &mut self,
        dict: Value,
        key: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Option<usize>, Trap> {
        let i = self.dict_slot(dict).ok_or(Trap::TypeError)?;
        self.require_hashable(key)?;
        if !self.is_instance(key) {
            let mut saw_instance_key = false;
            for (idx, (k, _)) in self.dicts[i].iter().enumerate() {
                if self.key_eq(*k, key) {
                    return Ok(Some(idx));
                }
                saw_instance_key |= self.is_instance(*k);
            }
            if !saw_instance_key {
                return Ok(None);
            }
        }
        let keys: Vec<Value> = self.dicts[i].iter().map(|(k, _)| *k).collect();
        for (idx, &k) in keys.iter().enumerate() {
            if crate::interp::elem_eq(key, k, functions, self, depth)? {
                return Ok(Some(idx));
            }
        }
        Ok(None)
    }

    /// Whether any key or value of `dict` is a user class instance -- the guard that keeps a plain
    /// `dict ==` on the fast identity path ([`ObjectModel::dict_equal`]) rather than the interp-aware
    /// [`ObjectModel::dict_equal_dyn`].
    pub(crate) fn dict_has_instance(&self, dict: Value) -> bool {
        self.dict_value(dict).is_some_and(|entries| {
            entries.iter().any(|(k, v)| self.is_instance(*k) || self.is_instance(*v))
        })
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
        let index = take_arena_slot(&mut self.sets, &mut self.freed_slots.sets, deduped);
        let reference = self.alloc_object(type_id).ok_or(Trap::OutOfMemory)?;
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
    pub(crate) fn set_value(&self, value: Value) -> Option<&Vec<Value>> {
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

    /// Replaces a MUTABLE set's contents wholesale -- the in-place set operators (`set |= s`,
    /// `&=`, `-=`, `^=`) compute the result elements interp-aware, then swap them in, preserving
    /// the set object's identity for aliases. A frozenset is rejected (its augmented form falls
    /// back to the plain operator, rebinding a new frozenset).
    pub(crate) fn set_replace_elems(&mut self, set: Value, elems: Vec<Value>) -> Result<(), Trap> {
        let index = self.container_slot(set, self.set_type_id).ok_or(Trap::TypeError)?;
        self.sets[index] = elems;
        Ok(())
    }

    /// Appends `value` to a set WITHOUT a membership check -- the caller (the interp-aware set ops)
    /// has already established that no equal element is present, testing membership via an element's
    /// `__eq__`, which the model's identity-based `key_eq` cannot do.
    pub(crate) fn set_push(&mut self, set: Value, value: Value) -> Result<(), Trap> {
        let index = self.container_slot(set, self.set_type_id).ok_or(Trap::TypeError)?;
        self.sets[index].push(value);
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
    pub(crate) fn key_eq(&self, a: Value, b: Value) -> bool {
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) {
            return x == y;
        }
        if self.is_int(a) && self.is_int(b) {
            return self.as_bigint(a) == self.as_bigint(b);
        }
        if self.is_float(a) || self.is_float(b) {
            if let (Some(x), Some(y)) = (self.as_f64(a), self.as_f64(b)) {
                return x == y;
            }
        }
        if let (Some(x), Some(y)) = (self.str_value(a), self.str_value(b)) {
            return x == y;
        }
        if let (Some(x), Some(y)) = (self.byte_view(a), self.byte_view(b)) {
            return x == y;
        }
        let both_sequence =
            (self.is_list(a) && self.is_list(b)) || (self.is_tuple(a) && self.is_tuple(b));
        if both_sequence {
            if let (Some(xs), Some(ys)) = (self.seq_value(a), self.seq_value(b)) {
                return xs.len() == ys.len()
                    && xs.iter().zip(ys).all(|(&x, &y)| self.key_eq(x, y));
            }
        }
        if self.is_dict(a) && self.is_dict(b) {
            return self.dict_equal(a, b);
        }
        if (self.is_set(a) || self.is_frozenset(a)) && (self.is_set(b) || self.is_frozenset(b)) {
            let (Some(ea), Some(eb)) = (self.set_value(a), self.set_value(b)) else {
                return a == b;
            };
            return ea.len() == eb.len() && ea.iter().all(|&x| eb.iter().any(|&y| self.key_eq(x, y)));
        }
        a == b
    }

    /// Whether two dicts hold the same `key -> value` content (order-independent). Backs dict `==`
    /// and nested-dict `key_eq`.
    fn dict_equal(&self, a: Value, b: Value) -> bool {
        let (Some(ea), Some(eb)) = (self.dict_value(a), self.dict_value(b)) else {
            return false;
        };
        ea.len() == eb.len()
            && ea.iter().all(|(key, value)| {
                eb.iter()
                    .any(|(other_key, other_value)| {
                        self.key_eq(*key, *other_key) && self.key_eq(*value, *other_value)
                    })
            })
    }

    /// Whether two dicts hold the same `key -> value` content, honoring a user `__eq__` on keys and
    /// values -- the interp-aware [`ObjectModel::dict_equal`] for `dict == dict`. Guarded by
    /// [`ObjectModel::dict_has_instance`], so a plain dict `==` never reaches here.
    pub(crate) fn dict_equal_dyn(
        &mut self,
        a: Value,
        b: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<bool, Trap> {
        let (ea, eb) = match (self.dict_value(a), self.dict_value(b)) {
            (Some(ea), Some(eb)) => (ea.clone(), eb.clone()),
            _ => return Ok(false),
        };
        if ea.len() != eb.len() {
            return Ok(false);
        }
        for (key, value) in &ea {
            let mut matched = false;
            for (other_key, other_value) in &eb {
                if crate::interp::elem_eq(*key, *other_key, functions, self, depth)?
                    && crate::interp::elem_eq(*value, *other_value, functions, self, depth)?
                {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `container[index] = value` (`Op::Setitem`): a `list` stores at an int index (negative
    /// from the end, `IndexError` out of range); a `dict` inserts or updates `index` as the
    /// key. A `tuple`/`str`/other is not assignable (`TypeError`).
    pub fn py_setitem(&mut self, container: Value, index: Value, value: Value) -> Result<(), Trap> {
        if let Some(slot) = self.deque_slot(container) {
            let len = self.seqs[slot].len() as i64;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "deque index out of range"));
            }
            self.seqs[slot][at as usize] = value;
            return Ok(());
        }
        if self.is_memoryview(container) {
            if self.memoryview_is_readonly(container) {
                return Err(Trap::TypeError);
            }
            let (base, offset, length) = self.memoryview_parts(container);
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + length as i64 } else { at };
            if at < 0 || at >= length as i64 {
                return Err(self.with_message(Trap::IndexError, "index out of range"));
            }
            let byte = value.as_int().ok_or(Trap::TypeError)?;
            if !(0..=255).contains(&byte) {
                return Err(Trap::ValueError);
            }
            let slot = self.byte_buffer_slot(base).ok_or(Trap::TypeError)?;
            self.byte_buffers[slot][offset + at as usize] = byte as u8;
            return Ok(());
        }
        if self.is_bytes(container) {
            let message = "'bytes' object does not support item assignment";
            return Err(self.raise_named_exception("TypeError", message));
        }
        if self.is_bytearray(container) {
            let slot = self.byte_buffer_slot(container).ok_or(Trap::TypeError)?;
            let len = self.byte_buffers[slot].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "bytearray index out of range"));
            }
            let byte = value.as_int().ok_or(Trap::TypeError)?;
            if !(0..=255).contains(&byte) {
                return Err(Trap::ValueError);
            }
            self.byte_buffers[slot][at as usize] = byte as u8;
            return Ok(());
        }
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
        if let Some(i) = self.dict_slot(container) {
            match self.dicts[i].iter().position(|(k, _)| self.key_eq(*k, index)) {
                Some(slot) => self.dicts[i][slot].1 = value,
                None => self.dicts[i].push((index, value)),
            }
            return Ok(());
        }
        Err(Trap::TypeError)
    }

    /// The collected right-hand side of a bytearray slice assignment, as bytes. Every element must
    /// be an int in `0..=255` -- which is also what makes `b[i:j] = "text"` a TypeError rather than
    /// a surprise encoding.
    fn elements_as_bytes(&mut self, elements: &[Value]) -> Result<alloc::vec::Vec<u8>, Trap> {
        let mut bytes = alloc::vec::Vec::with_capacity(elements.len());
        for element in elements {
            let Some(byte) = element.as_int() else {
                let message =
                    "can assign only bytes, buffers, or iterables of ints in range(0, 256)";
                return Err(self.raise_named_exception("TypeError", message));
            };
            if !(0..=255).contains(&byte) {
                let message = "byte must be in range(0, 256)";
                return Err(self.raise_named_exception("ValueError", message));
            }
            bytes.push(byte as u8);
        }
        Ok(bytes)
    }

    /// `list[slice] = elements` / `bytearray[slice] = elements` (`Op::Setitem` with a slice index):
    /// replaces the slice with the already-collected RHS `elements`. A step-1 slice SPLICES (the
    /// container may change length -- `xs[1:3] = [a, b, c]`); an extended slice (step != 1) assigns
    /// element-wise and requires the RHS length to equal the slice length (else a `ValueError`).
    /// Bounds resolve exactly like a slice read (clamping, negative indices). The RHS is collected
    /// by the caller (it may be any iterable, including a generator, which needs the interpreter).
    pub fn seq_setitem_slice(&mut self, container: Value, slice: Value, elements: Vec<Value>) -> Result<(), Trap> {
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
        if self.is_bytearray(container) {
            let slot = self.byte_buffer_slot(container).ok_or(Trap::TypeError)?;
            let len = self.byte_buffers[slot].len() as i64;
            let bytes = self.elements_as_bytes(&elements)?;
            let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
            if step == 1 {
                let low = start.clamp(0, len) as usize;
                let high = stop.clamp(start, len) as usize;
                self.byte_buffers[slot].splice(low..high, bytes);
                return Ok(());
            }
            let mut indices = Vec::new();
            let mut at = start;
            while (step > 0 && at < stop) || (step < 0 && at > stop) {
                if at >= 0 && at < len {
                    indices.push(at as usize);
                }
                at += step;
            }
            if indices.len() != bytes.len() {
                let message = alloc::format!(
                    "attempt to assign bytes of size {} to extended slice of size {}",
                    bytes.len(),
                    indices.len()
                );
                return Err(self.with_message(Trap::ValueError, &message));
            }
            for (index, byte) in indices.into_iter().zip(bytes) {
                self.byte_buffers[slot][index] = byte;
            }
            return Ok(());
        }
        let i = self.container_slot(container, self.list_type_id).ok_or(Trap::TypeError)?;
        let len = self.seqs[i].len() as i64;
        let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
        if step == 1 {
            let low = start.clamp(0, len) as usize;
            let high = stop.clamp(start, len) as usize;
            self.seqs[i].splice(low..high, elements);
        } else {
            let mut indices = Vec::new();
            let mut at = start;
            while (step > 0 && at < stop) || (step < 0 && at > stop) {
                if at >= 0 && at < len {
                    indices.push(at as usize);
                }
                at += step;
            }
            if indices.len() != elements.len() {
                let message = alloc::format!(
                    "attempt to assign sequence of size {} to extended slice of size {}",
                    elements.len(),
                    indices.len()
                );
                return Err(self.with_message(Trap::ValueError, &message));
            }
            for (index, value) in indices.into_iter().zip(elements) {
                self.seqs[i][index] = value;
            }
        }
        Ok(())
    }

    /// `del container[index]` (a future `Op::DeleteItem`): a `list` removes an int index (negative
    /// from the end, `IndexError` out of range) or the elements a slice selects (the list shrinks);
    /// a `dict` removes `index` as a key (`KeyError` if absent). A `tuple`/`str`/other is a
    /// `TypeError`. An instance's `__delitem__` is dispatched by the interpreter before this.
    pub fn py_delitem(&mut self, container: Value, index: Value) -> Result<(), Trap> {
        if let Some(slot) = self.deque_slot(container) {
            let len = self.seqs[slot].len() as i64;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "deque index out of range"));
            }
            self.seqs[slot].remove(at as usize);
            return Ok(());
        }
        if self.is_bytes(container) {
            let message = "'bytes' object doesn't support item deletion";
            return Err(self.raise_named_exception("TypeError", message));
        }
        if self.is_bytearray(container) {
            if self.is_slice(index) {
                return self.seq_delitem_slice(container, index);
            }
            let slot = self.byte_buffer_slot(container).ok_or(Trap::TypeError)?;
            let len = self.byte_buffers[slot].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                let message = "bytearray index out of range";
                return Err(self.with_message(Trap::IndexError, message));
            }
            self.byte_buffers[slot].remove(at as usize);
            return Ok(());
        }
        if let Some(i) = self.container_slot(container, self.list_type_id) {
            if self.is_slice(index) {
                return self.seq_delitem_slice(container, index);
            }
            let len = self.seqs[i].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "list assignment index out of range"));
            }
            self.seqs[i].remove(at as usize);
            return Ok(());
        }
        if let Some(i) = self.dict_slot(container) {
            return match self.dicts[i].iter().position(|(k, _)| self.key_eq(*k, index)) {
                Some(slot) => {
                    self.dicts[i].remove(slot);
                    Ok(())
                }
                None => {
                    self.set_trap_arg(index);
                    Err(Trap::KeyError)
                }
            };
        }
        Err(Trap::TypeError)
    }

    /// `del list[slice]` -- removes the elements the slice selects; the list shrinks. Bounds resolve
    /// exactly like a slice read.
    fn seq_delitem_slice(&mut self, container: Value, slice: Value) -> Result<(), Trap> {
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
        if self.is_bytearray(container) {
            let slot = self.byte_buffer_slot(container).ok_or(Trap::TypeError)?;
            let len = self.byte_buffers[slot].len() as i64;
            let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
            if step == 1 {
                let low = start.clamp(0, len) as usize;
                let high = stop.clamp(start, len) as usize;
                self.byte_buffers[slot].drain(low..high);
                return Ok(());
            }
            let mut indices = Vec::new();
            let mut at = start;
            while (step > 0 && at < stop) || (step < 0 && at > stop) {
                if at >= 0 && at < len {
                    indices.push(at as usize);
                }
                at += step;
            }
            indices.sort_unstable();
            for index in indices.into_iter().rev() {
                self.byte_buffers[slot].remove(index);
            }
            return Ok(());
        }
        let i = self.container_slot(container, self.list_type_id).ok_or(Trap::TypeError)?;
        let len = self.seqs[i].len() as i64;
        let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
        if step == 1 {
            let low = start.clamp(0, len) as usize;
            let high = stop.clamp(start, len) as usize;
            self.seqs[i].drain(low..high);
        } else {
            let mut indices = Vec::new();
            let mut at = start;
            while (step > 0 && at < stop) || (step < 0 && at > stop) {
                if at >= 0 && at < len {
                    indices.push(at as usize);
                }
                at += step;
            }
            indices.sort_unstable();
            for index in indices.into_iter().rev() {
                self.seqs[i].remove(index);
            }
        }
        Ok(())
    }

    /// `element in container` (`Op::Contains`): substring for `str`, membership for a
    /// `list`/`tuple` (any element equals), key membership for a `dict`.
    pub fn py_contains(&self, container: Value, element: Value) -> Result<bool, Trap> {
        if let Some(s) = self.str_value(container) {
            let sub = self.str_value(element).ok_or(Trap::TypeError)?;
            return Ok(s.contains(sub));
        }
        if let Some(data) = self.byte_view(container) {
            if let Some(byte) = element.as_int() {
                if !(0..=255).contains(&byte) {
                    return Err(Trap::ValueError);
                }
                return Ok(data.contains(&(byte as u8)));
            }
            let needle = self.byte_view(element).ok_or(Trap::TypeError)?;
            return Ok(needle.is_empty() || data.windows(needle.len()).any(|w| w == needle));
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
        if self.is_range(container) {
            let x = if let Some(n) = element.as_int() {
                n
            } else if let Some(f) = self.as_f64(element) {
                if f.is_finite() && f % 1.0 == 0.0 {
                    f as i64
                } else {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            };
            let (start, stop, step) = self.range_bounds(container);
            let in_bounds = if step > 0 { x >= start && x < stop } else { x <= start && x > stop };
            return Ok(in_bounds && (x - start) % step == 0);
        }
        Err(Trap::TypeError)
    }

    /// `container[index]` interp-aware ([`Op::Subscript`]): a `dict` looks its key up by an element's
    /// `__eq__` via [`ObjectModel::dict_find_dyn`] (a value object keys correctly); every other
    /// container defers to [`ObjectModel::py_getitem`].
    pub(crate) fn py_getitem_dyn(
        &mut self,
        container: Value,
        index: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        if let Some(i) = self.dict_slot(container) {
            return match self.dict_find_dyn(container, index, functions, depth)? {
                Some(slot) if slot < self.dicts[i].len() => Ok(self.dicts[i][slot].1),
                _ => {
                    if let Some(factory) = self.defaultdict_factory(container) {
                        if factory != Value::NONE {
                            let value =
                                crate::interp::call_value(factory, &[], functions, self, depth + 1)?;
                            let slot = self.dict_slot(container).ok_or(Trap::TypeError)?;
                            self.dicts[slot].push((index, value));
                            return Ok(value);
                        }
                    }
                    if self.is_counter(container) {
                        return Value::fixnum(0).ok_or(Trap::Overflow);
                    }
                    self.set_trap_arg(index);
                    Err(Trap::KeyError)
                }
            };
        }
        self.py_getitem(container, index)
    }

    /// `container[index] = value` interp-aware ([`Op::Setitem`], [`Op::DictInsert`]): a `dict` finds
    /// the key by `__eq__` (updating in place) or appends a new entry; every other container defers to
    /// [`ObjectModel::py_setitem`].
    pub(crate) fn py_setitem_dyn(
        &mut self,
        container: Value,
        index: Value,
        value: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<(), Trap> {
        if let Some(i) = self.dict_slot(container) {
            match self.dict_find_dyn(container, index, functions, depth)? {
                Some(slot) if slot < self.dicts[i].len() => self.dicts[i][slot].1 = value,
                _ => self.dicts[i].push((index, value)),
            }
            return Ok(());
        }
        self.py_setitem(container, index, value)
    }

    /// `del container[index]` interp-aware ([`Op::DeleteItem`]): a `dict` finds the key by `__eq__`
    /// and removes it (`KeyError`, carrying the key, if absent); every other container defers to
    /// [`ObjectModel::py_delitem`].
    pub(crate) fn py_delitem_dyn(
        &mut self,
        container: Value,
        index: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<(), Trap> {
        if let Some(i) = self.dict_slot(container) {
            return match self.dict_find_dyn(container, index, functions, depth)? {
                Some(slot) if slot < self.dicts[i].len() => {
                    self.dicts[i].remove(slot);
                    Ok(())
                }
                _ => {
                    self.set_trap_arg(index);
                    Err(Trap::KeyError)
                }
            };
        }
        self.py_delitem(container, index)
    }

    /// `element in container` interp-aware ([`Op::Contains`]): a `dict` tests key membership by
    /// `__eq__` via [`ObjectModel::dict_find_dyn`]; every other container defers to
    /// [`ObjectModel::py_contains`]. (Set membership is handled inline in the interpreter loop.)
    pub(crate) fn py_contains_dyn(
        &mut self,
        container: Value,
        element: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<bool, Trap> {
        if self.is_dict(container) {
            return Ok(self.dict_find_dyn(container, element, functions, depth)?.is_some());
        }
        if let Some(elements) = self.seq_value(container).cloned() {
            for candidate in elements {
                if crate::interp::elem_eq(element, candidate, functions, self, depth)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        self.py_contains(container, element)
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
        if value.is_ellipsis() {
            return String::from("Ellipsis");
        }
        if value.is_not_implemented() {
            return String::from("NotImplemented");
        }
        if self.is_object_base(value) {
            let addr = value.as_ref().map_or(0, |r| r.0);
            return alloc::format!("<object object at 0x{addr:016x}>");
        }
        if self.is_user_function(value) {
            let qualname = self.function_qualname(value).unwrap_or_default();
            let shown = if qualname.contains("<lambda") { String::from("<lambda>") } else { qualname };
            return alloc::format!("<function {shown} at 0x{:016x}>", value.bits());
        }
        if self.is_py_bound(value) {
            let func = self.bound_func(value);
            let qualname = self.function_qualname(func).unwrap_or_default();
            let receiver = self.repr(self.bound_self(value));
            return alloc::format!("<bound method {qualname} of {receiver}>");
        }
        if self.is_bound_method(value) || self.is_unbound_method(value) {
            let kind = self.type_name_of(self.bound_receiver(value));
            return alloc::format!("<built-in method of {kind} object at 0x{:016x}>", value.bits());
        }
        if self.is_file(value) {
            return self.file_repr(value);
        }
        if let Some(n) = value.as_fixnum() {
            return alloc::format!("{n}");
        }
        if let Some(n) = self.long_value(value) {
            return alloc::format!("{n}");
        }
        if let Some(big) = self.bigint_value(value) {
            return big.to_decimal_string();
        }
        if let Some(f) = self.float_value(value) {
            return format_float(f);
        }
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            return format_complex(re, im);
        }
        if let Some(s) = self.str_value(value) {
            return str_repr(s);
        }
        if let Some(data) = self.bytes_value(value) {
            return bytes_repr(data, self.is_bytearray(value));
        }
        if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            return if step == 1 {
                alloc::format!("range({start}, {stop})")
            } else {
                alloc::format!("range({start}, {stop}, {step})")
            };
        }
        if self.is_slice(value) {
            let (start, stop, step) = self.slice_components(value);
            return alloc::format!("slice({}, {}, {})", self.repr(start), self.repr(stop), self.repr(step));
        }
        if let Some(class) = self.ntinstance_class(value) {
            let fields = self.ntclass_fields(class);
            let elems = self.seq_value(value).cloned().unwrap_or_default();
            let inner = fields
                .iter()
                .zip(&elems)
                .map(|(f, &e)| alloc::format!("{f}={}", self.repr(e)))
                .collect::<Vec<_>>()
                .join(", ");
            return alloc::format!("{}({inner})", self.ntclass_name(class));
        }
        if self.is_ntclass(value) {
            return alloc::format!("<class '__main__.{}'>", self.ntclass_name(value));
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
        if let Some(elems) = self.deque_elems(value) {
            let inner = elems.iter().map(|&e| self.repr(e)).collect::<Vec<_>>().join(", ");
            return match self.deque_maxlen(value).unwrap_or(None) {
                Some(m) => alloc::format!("deque([{inner}], maxlen={m})"),
                None => alloc::format!("deque([{inner}])"),
            };
        }
        if let Some(kind) = self.dict_view_kind(value) {
            let entries = self.dict_value(self.dict_view_dict(value)).cloned().unwrap_or_default();
            let (name, inner) = match kind {
                DictViewKind::Keys => (
                    "dict_keys",
                    entries.iter().map(|&(k, _)| self.repr(k)).collect::<Vec<_>>().join(", "),
                ),
                DictViewKind::Values => (
                    "dict_values",
                    entries.iter().map(|&(_, v)| self.repr(v)).collect::<Vec<_>>().join(", "),
                ),
                DictViewKind::Items => (
                    "dict_items",
                    entries
                        .iter()
                        .map(|&(k, v)| alloc::format!("({}, {})", self.repr(k), self.repr(v)))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            };
            return alloc::format!("{name}([{inner}])");
        }
        if let Some(entries) = self.dict_value(value) {
            if self.is_counter(value) {
                if entries.is_empty() {
                    return String::from("Counter()");
                }
                let entries = self.counter_display_entries(entries.clone());
                let inner = entries
                    .iter()
                    .map(|(k, v)| alloc::format!("{}: {}", self.repr(*k), self.repr(*v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                return alloc::format!("Counter({{{inner}}})");
            }
            let inner = entries
                .iter()
                .map(|(k, v)| alloc::format!("{}: {}", self.repr(*k), self.repr(*v)))
                .collect::<Vec<_>>()
                .join(", ");
            if let Some(factory) = self.defaultdict_factory(value) {
                return alloc::format!("defaultdict({}, {{{inner}}})", self.repr(factory));
            }
            if self.is_ordereddict(value) {
                return if inner.is_empty() {
                    String::from("OrderedDict()")
                } else {
                    alloc::format!("OrderedDict({{{inner}}})")
                };
            }
            return alloc::format!("{{{inner}}}");
        }
        if let Some(id) = value.as_builtin_id() {
            if let Some(builtin) = Builtin::from_id(id) {
                return if builtin.is_type() {
                    alloc::format!("<class '{}'>", builtin.python_name())
                } else {
                    alloc::format!("<built-in function {}>", builtin.python_name())
                };
            }
            if let Some(name) = crate::stdlib::stdlib_name(id) {
                return if crate::stdlib::stdlib_is_type(id) {
                    let module = crate::stdlib::stdlib_module_of(id).unwrap_or("builtins");
                    alloc::format!("<class '{module}.{name}'>")
                } else {
                    alloc::format!("<built-in function {name}>")
                };
            }
        }
        if self.is_class(value) {
            let name = String::from(self.str_value(self.read_slot(value, 0)).unwrap_or("?"));
            let namespace = self.read_slot(value, 2);
            let module = self.dict_get_str(namespace, "__module__").and_then(|m| self.str_value(m));
            return match module {
                Some(m) if m != "builtins" => alloc::format!("<class '{m}.{name}'>"),
                _ => alloc::format!("<class '{name}'>"),
            };
        }
        if self.is_instance(value) {
            let name = self.instance_class_name(value).unwrap_or("object");
            if self.is_exception_value(value) {
                let args = self
                    .instance_attr(value, "args")
                    .and_then(|a| self.seq_value(a))
                    .map(|elements| {
                        elements.iter().map(|&e| self.repr(e)).collect::<Vec<_>>().join(", ")
                    })
                    .unwrap_or_default();
                return alloc::format!("{name}({args})");
            }
            let class = self.read_slot(value, 0);
            let module = self
                .find_in_class(class, "__module__")
                .and_then(|m| self.str_value(m))
                .unwrap_or("builtins");
            let addr = value.as_ref().map_or(0, |r| r.0);
            return alloc::format!("<{module}.{name} object at 0x{addr:016x}>");
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
        } else if self.is_instance(value) {
            self.instance_display(value)
        } else {
            self.repr(value)
        };
        self.new_str(&rendered)
    }

    /// `hash(value)` for a hashable value: int/bool `n` -> `n` (except `hash(-1) == -2`, CPython's
    /// error sentinel); `None` -> a stable constant; `str`/`tuple` -> a deterministic FNV-1a hash
    /// folded into the fixnum range (NOT CPython's randomized hash -- the value differs but is
    /// stable within a run). `list`/`dict`/`set` (and a tuple containing one) are unhashable ->
    /// `TypeError`, matching CPython.
    pub fn py_hash(&self, value: Value) -> Result<Value, Trap> {
        if let Some(n) = value.as_int() {
            let h = if n == -1 { -2 } else { n };
            return Value::fixnum(i32::try_from(h).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow);
        }
        if let Some(big) = self.long_value(value) {
            let bits = big as u128;
            let folded = (bits ^ (bits >> 64)) as u32;
            return Value::fixnum((folded & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow);
        }
        if let Some(bigint) = self.bigint_value(value) {
            let mut folded: u32 = if bigint.is_negative() { 2_166_136_261 } else { 16_777_619 };
            for byte in bigint.to_decimal_string().bytes() {
                folded = folded.wrapping_mul(16_777_619) ^ u32::from(byte);
            }
            return Value::fixnum((folded & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow);
        }
        if let Some(f) = self.float_value(value) {
            return Ok(self.py_hash_float(f));
        }
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            let hash_real = i64::from(self.py_hash_float(re).as_fixnum().unwrap_or(0));
            let hash_imag = i64::from(self.py_hash_float(im).as_fixnum().unwrap_or(0));
            let combined = hash_real.wrapping_add(1_000_003_i64.wrapping_mul(hash_imag));
            return Value::fixnum((combined & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow);
        }
        if value.is_none() {
            return Value::fixnum(0).ok_or(Trap::Overflow);
        }
        let folded = self.hash_nonint(value)?;
        Value::fixnum((folded & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow)
    }

    /// The deterministic FNV-1a hash of a `str`/`tuple` (recursive), or a `TypeError` for an
    /// unhashable value. Backs [`ObjectModel::py_hash`] for the non-int cases.
    fn hash_nonint(&self, value: Value) -> Result<u32, Trap> {
        if let Some(s) = self.str_value(value) {
            let mut hash: u32 = 2_166_136_261;
            for byte in s.bytes() {
                hash ^= u32::from(byte);
                hash = hash.wrapping_mul(16_777_619);
            }
            return Ok(hash);
        }
        if self.is_tuple(value) {
            let elements = self.seq_value(value).ok_or(Trap::TypeError)?;
            let mut hash: u32 = 2_166_136_261;
            for &element in elements {
                let element_hash = self.py_hash(element)?.as_fixnum().unwrap_or(0) as u32;
                hash ^= element_hash;
                hash = hash.wrapping_mul(16_777_619);
            }
            return Ok(hash);
        }
        if self.is_frozenset(value) {
            let elements = self.set_value(value).ok_or(Trap::TypeError)?;
            let mut hash: u32 = 2_166_136_261;
            for &element in elements {
                hash ^= self.py_hash(element)?.as_fixnum().unwrap_or(0) as u32;
            }
            return Ok(hash);
        }
        if self.is_object_base(value) {
            if let Some(reference) = value.as_ref() {
                return Ok(reference.0.wrapping_mul(2_654_435_761));
            }
        }
        Err(Trap::TypeError)
    }

    /// `hash(float)`, folded into the fixnum range. The load-bearing invariant is Python's numeric
    /// hash rule: a value that equals an int hashes EQUAL to that int (`hash(2.0) == hash(2)`), so
    /// mixed int/float dict and set keys collapse; `+0.0` and `-0.0` both equal int `0`, so they
    /// hash alike. `inf`/`-inf` use CPython's `+-314159` sentinels; a `NaN` gets a stable `0` (its
    /// value is never CPython's object-identity hash, but a NaN never matches anything anyway).
    /// A non-integral float uses a deterministic fold of its bits (stable within a run, not
    /// CPython's `_Py_HashDouble` value -- the same documented divergence as the str/tuple hash).
    #[must_use]
    fn py_hash_float(&self, value: f64) -> Value {
        let fixnum = |n: i32| Value::fixnum(n).unwrap_or(Value::fixnum(0).unwrap());
        if value.is_nan() {
            return fixnum(0);
        }
        if value.is_infinite() {
            return fixnum(if value > 0.0 { 314_159 } else { -314_159 });
        }
        if libm::floor(value) == value && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&value) {
            let n = i128::from(value as i64);
            if n >= i128::from(FIXNUM_MIN) && n <= i128::from(FIXNUM_MAX) {
                return fixnum(if n == -1 { -2 } else { n as i32 });
            }
            let bits = n as u128;
            return fixnum(((bits ^ (bits >> 64)) as u32 & 0x3FFF_FFFF) as i32);
        }
        let bits = value.to_bits();
        fixnum(((bits ^ (bits >> 32)) as u32 & 0x3FFF_FFFF) as i32)
    }

    /// An `int` value from an `i128`, normalized: one that fits the fixnum range stays a fixnum (so
    /// `10**10 == 10**10` and small results never allocate); a bigger one becomes a heap `long`.
    /// A result outside i128 is a `Trap::Overflow`; callers that need more precision use `new_bigint`.
    pub fn new_long(&mut self, n: i128) -> Result<Value, Trap> {
        if n >= i128::from(FIXNUM_MIN) && n <= i128::from(FIXNUM_MAX) {
            return Value::fixnum(n as i32).ok_or(Trap::Overflow);
        }
        let reference = self.alloc_object(self.long_type_id).ok_or(Trap::OutOfMemory)?;
        let bits = n as u128;
        for word in 0..4u32 {
            self.heap.write_u32(reference.0 + word * 4, (bits >> (word * 32)) as u32);
        }
        Ok(Value::from_ref(reference))
    }

    /// The i128 a `long` holds, or `None` if `value` is not a long.
    #[must_use]
    pub fn long_value(&self, value: Value) -> Option<i128> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.long_type_id {
            return None;
        }
        let mut bits: u128 = 0;
        for word in 0..4u32 {
            bits |= u128::from(self.heap.read_u32(reference.0 + word * 4)) << (word * 32);
        }
        Some(bits as i128)
    }

    /// The integer value of `value` as an i128 -- a fixnum/bool or a `long`; `None` for a non-int.
    /// The single reader the arithmetic/comparison core uses so it treats both int kinds uniformly.
    #[must_use]
    pub fn as_i128(&self, value: Value) -> Option<i128> {
        if let Some(n) = value.as_int() {
            return Some(i128::from(n));
        }
        self.long_value(value)
    }

    /// Whether `value` is a `long` (a big int); a plain int-valued fixnum is NOT a long.
    #[must_use]
    pub fn is_long(&self, value: Value) -> bool {
        self.long_value(value).is_some()
    }

    /// An `int` from a [`BigInt`], normalized DOWN to the smallest representation: a value that fits
    /// the fixnum range stays a fixnum, one that fits `i128` becomes a `long`, and only a truly
    /// arbitrary-precision value allocates a `bigint`. So the three int tiers never overlap and a
    /// shrunk result (`big - big`) collapses back automatically.
    pub fn new_bigint(&mut self, value: BigInt) -> Result<Value, Trap> {
        if let Some(n) = value.to_i128() {
            return self.new_long(n);
        }
        let reference = self.alloc_object(self.bigint_type_id).ok_or(Trap::OutOfMemory)?;
        let index = take_arena_slot(&mut self.bigints, &mut self.freed_slots.bigints, value);
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// The [`BigInt`] a `bigint` object holds, or `None` if `value` is not a bigint (a fixnum/long is
    /// NOT a bigint -- use [`ObjectModel::as_bigint`] to read any int as a `BigInt`).
    #[must_use]
    pub fn bigint_value(&self, value: Value) -> Option<&BigInt> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.bigint_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.bigints.get(index)
    }

    /// Any int -- a fixnum/bool, an i128 `long`, or a `bigint` -- as a [`BigInt`]; `None` for a
    /// non-int. The reader the arbitrary-precision arithmetic core uses so it treats all three int
    /// tiers uniformly (the `BigInt` sibling of [`ObjectModel::as_i128`]).
    #[must_use]
    pub fn as_bigint(&self, value: Value) -> Option<BigInt> {
        if let Some(n) = self.as_i128(value) {
            return Some(BigInt::from_i128(n));
        }
        self.bigint_value(value).cloned()
    }

    /// Whether `value` is a `bigint` (an int beyond the i128 range); a fixnum/`long` is NOT a bigint.
    #[must_use]
    pub fn is_bigint(&self, value: Value) -> bool {
        self.bigint_value(value).is_some()
    }

    /// Whether `value` is any Python `int` (a fixnum, a `bool`, an i128 `long`, or a `bigint`) --
    /// the three integer tiers plus `bool` (an int subtype). NOT a float/complex.
    #[must_use]
    pub fn is_int(&self, value: Value) -> bool {
        value.is_fixnum()
            || value == Value::TRUE
            || value == Value::FALSE
            || self.is_long(value)
            || self.is_bigint(value)
    }

    /// A heap `float` holding the IEEE-754 double `n`. Unlike `new_long`, a float is ALWAYS boxed
    /// (there is no immediate float form -- `Value` is a 32-bit word, an f64 does not fit), so even
    /// `0.0`/`1.0` allocate. Mirrors `new_long`'s heap-leaf storage (two u32 words of raw bits).
    pub fn new_float(&mut self, n: f64) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.float_type_id).ok_or(Trap::OutOfMemory)?;
        let bits = n.to_bits();
        self.heap.write_u32(reference.0, bits as u32);
        self.heap.write_u32(reference.0 + 4, (bits >> 32) as u32);
        Ok(Value::from_ref(reference))
    }

    /// The f64 a `float` holds, or `None` if `value` is not a float (an int/bool is NOT a float --
    /// use [`ObjectModel::as_f64`] for the numeric-coercion reader).
    #[must_use]
    pub fn float_value(&self, value: Value) -> Option<f64> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.float_type_id {
            return None;
        }
        let low = u64::from(self.heap.read_u32(reference.0));
        let high = u64::from(self.heap.read_u32(reference.0 + 4));
        Some(f64::from_bits((high << 32) | low))
    }

    /// The value of `value` as an f64 for MIXED int/float arithmetic and comparison: a fixnum/bool,
    /// a `long`, or a `float` all read here (an int widens to the nearest double); `None` for a
    /// non-number. The reader the float arithmetic/comparison core uses so `2 + 3.5` and `1 < 1.5`
    /// coerce the int operand -- the float sibling of [`ObjectModel::as_i128`].
    #[must_use]
    pub fn as_f64(&self, value: Value) -> Option<f64> {
        if let Some(n) = value.as_int() {
            return Some(n as f64);
        }
        if let Some(big) = self.long_value(value) {
            return Some(big as f64);
        }
        if let Some(bigint) = self.bigint_value(value) {
            return Some(bigint.to_f64());
        }
        self.float_value(value)
    }

    /// Whether `value` is a `float`; an int-valued fixnum/long is NOT a float (Python keeps the types
    /// distinct -- `1 is not 1.0`, `type(1) is not type(1.0)`).
    #[must_use]
    pub fn is_float(&self, value: Value) -> bool {
        self.float_value(value).is_some()
    }

    /// A heap `complex` from its real and imaginary parts. Always boxed (two f64 do not fit an
    /// immediate). Behind the `complex` knob.
    #[cfg(feature = "complex")]
    pub fn new_complex(&mut self, real: f64, imag: f64) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.complex_type_id).ok_or(Trap::OutOfMemory)?;
        for (word, part) in [real, imag].into_iter().enumerate() {
            let bits = part.to_bits();
            self.heap.write_u32(reference.0 + word as u32 * 8, bits as u32);
            self.heap.write_u32(reference.0 + word as u32 * 8 + 4, (bits >> 32) as u32);
        }
        Ok(Value::from_ref(reference))
    }

    /// The `(real, imag)` a `complex` holds, or `None` if `value` is not a complex.
    #[cfg(feature = "complex")]
    #[must_use]
    pub fn complex_value(&self, value: Value) -> Option<(f64, f64)> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.complex_type_id {
            return None;
        }
        let read = |offset: u32| {
            let low = u64::from(self.heap.read_u32(reference.0 + offset));
            let high = u64::from(self.heap.read_u32(reference.0 + offset + 4));
            f64::from_bits((high << 32) | low)
        };
        Some((read(0), read(8)))
    }

    /// The value of `value` as a `(real, imag)` pair for MIXED complex arithmetic: an int/bool, a
    /// `long`, or a `float` promotes to `(x, 0.0)`, and a `complex` reads directly; `None` for a
    /// non-number. Lets `2 + 3j` and `(1+2j) + 1` coerce the real operand -- the complex sibling of
    /// [`ObjectModel::as_f64`].
    #[cfg(feature = "complex")]
    #[must_use]
    pub fn as_complex(&self, value: Value) -> Option<(f64, f64)> {
        if let Some(re) = self.as_f64(value) {
            return Some((re, 0.0));
        }
        self.complex_value(value)
    }

    /// Whether `value` is a `complex`.
    #[cfg(feature = "complex")]
    #[must_use]
    pub fn is_complex(&self, value: Value) -> bool {
        self.complex_value(value).is_some()
    }

    /// Wraps a namespace dict in a module object; `module.member` resolves `member` in that dict.
    pub fn new_module(&mut self, namespace: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.module_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, namespace.bits());
        Ok(Value::from_ref(reference))
    }

    /// The namespace dict a module wraps; the precondition is that `value` is a module object.
    #[must_use]
    pub fn module_namespace(&self, value: Value) -> Value {
        self.read_slot(value, 0)
    }

    /// Whether `value` is a provided module object.
    #[must_use]
    pub fn is_module_object(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.module_type_id)
    }

    /// Provides a module `name` backed by `namespace` (a dict of its members), bound as a global so
    /// a program reaches it as `name.member` -- the injection path the native machine/board modules
    /// use, here for a Python-authored namespace.
    pub fn provide_module(&mut self, name: &str, namespace: Value) -> Result<(), Trap> {
        let module = self.new_module(namespace)?;
        self.set_global(name, module);
        Ok(())
    }

    /// Imports the module `name`, returning its module object: a cached `sys.modules` entry if it
    /// was imported before (so `import` is idempotent and a module body runs at most once), else a
    /// native stdlib module built on demand and cached, else a `ModuleNotFoundError`. Backs
    /// [`crate::interp`]'s `ImportName` op for NATIVE-only resolution -- the interpreter-aware
    /// resolver ([`crate::interp`]'s `resolve_import`) tries this first (native wins) and falls
    /// through to a managed Python-authored module when it returns `None`.
    pub fn import_module(&mut self, name: &str) -> Result<Value, Trap> {
        match self.import_builtin_module(name) {
            Some(result) => result,
            None => Err(self.module_not_found(name)),
        }
    }

    /// Resolves a BUILT-IN import: a cached `sys.modules` entry, a host-provided module bound as a
    /// global, or a native stdlib module built on demand (and cached). Returns `None` when `name`
    /// matches none of these, so the interpreter-aware resolver can fall through to a managed module
    /// (a Python-authored one, whose body it runs). Split from [`ObjectModel::import_module`] because
    /// running a managed body is interpreter-level (it needs the driver), which the model cannot do.
    pub(crate) fn import_builtin_module(&mut self, name: &str) -> Option<Result<Value, Trap>> {
        if let Some((_, module)) = self.modules.iter().find(|(n, _)| n == name) {
            return Some(Ok(*module));
        }
        if let Some(module) = self.get_global(name).filter(|&g| self.is_module_object(g)) {
            self.modules.push((String::from(name), module));
            return Some(Ok(module));
        }
        if let Some(result) = crate::stdlib::build_module(name, self) {
            return Some(result.inspect(|&module| {
                self.modules.push((String::from(name), module));
            }));
        }
        None
    }

    /// The `ModuleNotFoundError` for an unresolved import `name` ("No module named '...'").
    pub(crate) fn module_not_found(&mut self, name: &str) -> Trap {
        let message = alloc::format!("No module named '{name}'");
        self.raise_named_exception("ModuleNotFoundError", &message)
    }

    /// Installs the managed-module registry -- the Python-authored modules bundled with the program,
    /// resolved by `name` on an `import` that misses the native/host modules. Set once by the host
    /// before running the entry module (a single-file program installs an empty registry, the
    /// default). Also builds each module's function-table `Rc` (for cross-module calls) and sizes its
    /// global namespace. Module ids are the registry position + 1 (module 0 is the entry).
    pub fn set_managed_modules(&mut self, modules: Vec<Module>) {
        self.managed_functions = modules
            .iter()
            .map(|m| Rc::from(m.functions.clone().into_boxed_slice()))
            .collect();
        self.managed_bodies = modules.iter().map(|m| Rc::new(m.body.clone())).collect();
        self.managed_globals = modules.iter().map(|_| Vec::new()).collect();
        self.managed_modules = modules;
    }

    /// The module id (registry position + 1) of the managed module named `name`, or `None`. Module 0
    /// is the entry, so managed ids start at 1.
    #[must_use]
    pub(crate) fn managed_module_id(&self, name: &str) -> Option<u16> {
        self.managed_modules
            .iter()
            .position(|m| m.name == name)
            .map(|i| (i + 1) as u16)
    }

    /// A shared clone of managed module `module_id`'s top-level body code, or `None`.
    ///
    /// An `Rc` rather than a copy because the body is resolved ONCE PER OP while its frame is on the
    /// driver's stack -- the module body runs there like any other frame, so the collector's safe
    /// point sees it -- and it has to be reachable without a borrow into the registry, which the
    /// running code is free to mutate.
    #[must_use]
    pub(crate) fn managed_module_body_rc(&self, module_id: u16) -> Option<Rc<CodeObject>> {
        self.managed_bodies.get((module_id.checked_sub(1)?) as usize).cloned()
    }

    /// A shared clone of module `module_id`'s function table, or `None` for an out-of-range id (or the
    /// entry when no bundle installed one). Module 0 is the ENTRY (its `Rc`, set by `run_bundle`), so a
    /// managed module calling an entry-defined function reaches the entry's code, not the caller's;
    /// `module_id >= 1` is a managed module. A cross-module call clones this `Rc` (cheap) to run the
    /// callee against its own table, disjoint from the `&mut self` borrow.
    #[must_use]
    pub(crate) fn managed_functions_rc(&self, module_id: u16) -> Option<Rc<[CodeObject]>> {
        match module_id {
            0 => self.entry_functions.clone(),
            k => self.managed_functions.get((k - 1) as usize).cloned(),
        }
    }

    /// Installs the ENTRY module's function table (module 0) as a shared `Rc`, so a managed module
    /// calling an entry-defined function value resolves its code against the entry. Set by `run_bundle`
    /// before the entry runs.
    pub(crate) fn set_entry_functions(&mut self, functions: Rc<[CodeObject]>) {
        self.entry_functions = Some(functions);
    }

    /// The module whose functions + globals running code resolves against (0 = entry). Read by
    /// `LoadGlobal` / the module body's `StoreFast` / `MakeFunction` / the call dispatch.
    #[must_use]
    pub(crate) fn current_module(&self) -> u16 {
        self.current_module
    }

    /// Sets the current module id, returning the previous one. `run_frames` sets it on entry to a drive
    /// and restores it on exit, so a cross-module call runs in the callee's module context.
    pub(crate) fn set_current_module(&mut self, module_id: u16) -> u16 {
        core::mem::replace(&mut self.current_module, module_id)
    }

    /// The value bound to `name` in the CURRENT module's global namespace (entry -> `globals`, a managed
    /// module -> its `managed_globals` slot), or `None`. The module-aware `get_global`.
    #[must_use]
    pub(crate) fn current_module_global(&self, name: &str) -> Option<Value> {
        match self.current_module {
            0 => self.get_global(name),
            k => self
                .managed_globals
                .get((k - 1) as usize)?
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| *v),
        }
    }

    /// Binds (or rebinds) `name` in the CURRENT module's global namespace -- the module-aware
    /// `set_global`, used by a module body's top-level `StoreFast`.
    pub(crate) fn set_current_module_global(&mut self, name: &str, value: Value) {
        match self.current_module {
            0 => self.set_global(name, value),
            k => {
                if let Some(slot) = self.managed_globals.get_mut((k - 1) as usize) {
                    match slot.iter_mut().find(|(n, _)| n == name) {
                        Some(entry) => entry.1 = value,
                        None => slot.push((String::from(name), value)),
                    }
                }
            }
        }
    }

    /// Removes `name` from the CURRENT module's global namespace (a module-level `del name`), the
    /// counterpart of [`Self::set_current_module_global`]. Returns whether it was bound.
    pub(crate) fn delete_current_module_global(&mut self, name: &str) -> bool {
        let slot = match self.current_module {
            0 => &mut self.globals,
            k => match self.managed_globals.get_mut((k - 1) as usize) {
                Some(slot) => slot,
                None => return false,
            },
        };
        let before = slot.len();
        slot.retain(|(n, _)| n != name);
        slot.len() != before
    }

    /// `from m import *`: binds `module`'s public names into the CURRENT module's globals. The
    /// exported set is the module's `__all__` (a list/tuple of names) if defined, else every top-level
    /// name not starting with `_`, per CPython. Each bound via [`Self::set_current_module_global`].
    pub(crate) fn import_star(&mut self, module: Value) -> Result<(), Trap> {
        let namespace = self.module_namespace(module);
        let bindings: Vec<(String, Value)> = match self.dict_get_str(namespace, "__all__") {
            Some(all) => {
                let names = self.seq_value(all).ok_or(Trap::TypeError)?.clone();
                let mut out = Vec::with_capacity(names.len());
                for name_value in names {
                    let name = String::from(self.str_value(name_value).ok_or(Trap::TypeError)?);
                    if let Some(value) = self.dict_get_str(namespace, &name) {
                        out.push((name, value));
                    }
                }
                out
            }
            None => {
                let entries = self.dict_value(namespace).cloned().unwrap_or_default();
                entries
                    .into_iter()
                    .filter_map(|(key, value)| {
                        let name = self.str_value(key)?;
                        if name.starts_with('_') {
                            None
                        } else {
                            Some((String::from(name), value))
                        }
                    })
                    .collect()
            }
        };
        for (name, value) in bindings {
            self.set_current_module_global(&name, value);
        }
        Ok(())
    }

    /// A clone of managed module `module_id`'s populated global namespace pairs -- the source for its
    /// namespace dict, built once after its body runs. Empty for the entry or an out-of-range id.
    #[must_use]
    pub(crate) fn managed_module_globals(&self, module_id: u16) -> Vec<(String, Value)> {
        module_id
            .checked_sub(1)
            .and_then(|i| self.managed_globals.get(i as usize))
            .cloned()
            .unwrap_or_default()
    }

    /// Records `module` under `name` in the import cache (`sys.modules`). A managed module is cached
    /// here BEFORE its body runs, so a circular import sees the in-progress module and terminates.
    pub(crate) fn cache_module(&mut self, name: &str, module: Value) {
        self.modules.push((String::from(name), module));
    }

    /// Removes `name` from the import cache -- used when a managed module's body raises during import,
    /// so a later import retries the body (CPython drops a failed import from `sys.modules`).
    pub(crate) fn uncache_module(&mut self, name: &str) {
        self.modules.retain(|(n, _)| n != name);
    }

    /// Builds a module namespace dict from a managed module body's populated globals -- each name is
    /// interned as a `str` key (so `dict_get_str` / attribute access resolve it) mapping to its value.
    pub(crate) fn namespace_from_globals(
        &mut self,
        globals: Vec<(String, Value)>,
    ) -> Result<Value, Trap> {
        let mut pairs = Vec::with_capacity(globals.len());
        for (name, value) in globals {
            let key = self.new_str(&name)?;
            pairs.push((key, value));
        }
        self.new_dict(pairs)
    }

    /// The current module's global namespace as (name, value) pairs -- module 0 (the entry) reads
    /// `self.globals`; a managed module reads its own stored globals. Backs `globals()` and the
    /// module-scope `locals()`.
    pub(crate) fn current_module_globals(&self) -> Vec<(String, Value)> {
        if self.current_module() == 0 {
            self.globals.clone()
        } else {
            self.managed_module_globals(self.current_module())
        }
    }

    /// Replaces module object `module`'s namespace dict (slot 0) -- used once, after a managed body
    /// finishes, to swap its cached-empty namespace for the populated one. The precondition is that
    /// `module` is a module object.
    pub(crate) fn set_module_namespace(&mut self, module: Value, namespace: Value) {
        let reference = module.as_ref().expect("a module object");
        self.heap.write_u32(reference.0, namespace.bits());
    }

    /// Reads member `name` off an imported `module` -- `from module import name`. The member is
    /// resolved in the module's namespace; a missing member is an `ImportError` ("cannot import
    /// name 'name' from 'module'"), matching CPython (not the `AttributeError` a plain `module.name`
    /// attribute read would give). Backs [`crate::interp`]'s `ImportFrom` op.
    pub fn import_from(&mut self, module: Value, name: &str) -> Result<Value, Trap> {
        if self.is_module_object(module) {
            let namespace = self.module_namespace(module);
            if let Some(value) = self.dict_get_str(namespace, name) {
                return Ok(value);
            }
        }
        let module_name = self
            .modules
            .iter()
            .find(|(_, m)| *m == module)
            .map_or_else(String::new, |(n, _)| n.clone());
        let message = alloc::format!("cannot import name '{name}' from '{module_name}'");
        Err(self.raise_named_exception("ImportError", &message))
    }

    /// `iter(iterable)` (`Op::GetIter`): an iterator over a `str`/`list`/`tuple`/`dict` (a
    /// dict iterates its keys). A non-iterable value is a `TypeError`.
    /// A fresh `Cell` boxing `value` (the closure primitive: a shared mutable box for a variable
    /// captured by a nested function).
    pub fn new_cell(&mut self, value: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.cell_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, value.bits());
        Ok(Value::from_ref(reference))
    }

    /// The value a `Cell` holds. `TypeError` if `cell` is not a Cell (an interpreter invariant).
    pub fn cell_get(&self, cell: Value) -> Result<Value, Trap> {
        let reference = cell.as_ref().ok_or(Trap::TypeError)?;
        if self.heap.type_id_of(reference) != self.cell_type_id {
            return Err(Trap::TypeError);
        }
        Ok(Value::from_bits(self.heap.read_u32(reference.0)))
    }

    /// Stores `value` into a `Cell` (a `nonlocal`/enclosing-scope write is visible to every holder).
    pub fn cell_set(&mut self, cell: Value, value: Value) -> Result<(), Trap> {
        let reference = cell.as_ref().ok_or(Trap::TypeError)?;
        if self.heap.type_id_of(reference) != self.cell_type_id {
            return Err(Trap::TypeError);
        }
        self.heap.write_u32(reference.0, value.bits());
        Ok(())
    }

    /// Whether `value` is a `Cell`.
    #[must_use]
    pub fn is_cell(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.cell_type_id)
    }

    /// A new lazy iterator of `kind` (`LAZY_MAP`/`LAZY_FILTER`/`LAZY_ZIP`/`LAZY_ENUMERATE`) over
    /// `sources` (a tuple of source iterators), carrying `state` (the map/filter function, the
    /// enumerate counter, or `None`).
    pub fn new_lazy_iter(&mut self, kind: u32, state: Value, sources: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.lazy_iter_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, kind);
        self.heap.write_u32(reference.0 + 4, state.bits());
        self.heap.write_u32(reference.0 + 8, sources.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a lazy iterator (map/filter/zip/enumerate).
    #[must_use]
    pub fn is_lazy_iter(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.lazy_iter_type_id)
    }

    /// The kind tag of a lazy iterator.
    pub(crate) fn lazy_iter_kind(&self, value: Value) -> u32 {
        let reference = value.as_ref().expect("a lazy iterator");
        self.heap.read_u32(reference.0)
    }

    /// A lazy iterator's state slot (the map/filter function, the enumerate counter, or `None`).
    pub(crate) fn lazy_iter_state(&self, value: Value) -> Value {
        let reference = value.as_ref().expect("a lazy iterator");
        Value::from_bits(self.heap.read_u32(reference.0 + 4))
    }

    /// Overwrites a lazy iterator's state slot (the enumerate counter advances in place).
    pub(crate) fn lazy_iter_set_state(&mut self, value: Value, state: Value) {
        let reference = value.as_ref().expect("a lazy iterator");
        self.heap.write_u32(reference.0 + 4, state.bits());
    }

    /// The source iterators of a lazy iterator (the elements of its sources tuple).
    pub(crate) fn lazy_iter_sources(&self, value: Value) -> Vec<Value> {
        let reference = value.as_ref().expect("a lazy iterator");
        let sources = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        self.seq_value(sources).cloned().unwrap_or_default()
    }

    /// A `staticmethod(func)` wrapper (`is_class` false) or `classmethod(func)` (`is_class` true).
    pub fn new_method_wrapper(&mut self, func: Value, is_class: bool) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.method_wrapper_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, u32::from(is_class));
        self.heap.write_u32(reference.0 + 4, func.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `staticmethod`/`classmethod` wrapper.
    #[must_use]
    pub fn is_method_wrapper(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.method_wrapper_type_id)
    }

    /// Whether a method wrapper is a `classmethod` (else a `staticmethod`).
    pub(crate) fn method_wrapper_is_class(&self, value: Value) -> bool {
        let reference = value.as_ref().expect("a method wrapper");
        self.heap.read_u32(reference.0) != 0
    }

    /// The function a `staticmethod`/`classmethod` wrapper holds.
    fn method_wrapper_func(&self, value: Value) -> Value {
        let reference = value.as_ref().expect("a method wrapper");
        Value::from_bits(self.heap.read_u32(reference.0 + 4))
    }

    /// Resolves a class member `found` for attribute access on `owner` (an instance or the class):
    /// a `staticmethod` yields the raw function; a `classmethod` binds the function to the class; a
    /// plain function binds to `owner` when it is an instance, else stays unbound. `class_value` is
    /// the class to bind a classmethod to.
    /// Whether `value` is a user-defined Python function -- a bare module `function_ref` OR a
    /// `PyFunction` (a function carrying default args or captured cells). Used to decide both whether a
    /// class member binds as a method (a defaulted method is a PyFunction, so it must bind too) and
    /// whether an attribute get/set targets a function object (`f.tag`).
    pub(crate) fn is_user_function(&self, value: Value) -> bool {
        value.as_function_index().is_some() || self.is_py_function(value)
    }

    /// The raw callable for an implicit-classmethod dunder (`__class_getitem__` / `__init_subclass__`)
    /// defined in `class`'s MRO, or `None` if absent. A classmethod/staticmethod wrapper is unwrapped
    /// to its underlying function; a plain function is returned as-is. The caller invokes it with
    /// `cls` prepended -- CPython treats both dunders as implicit classmethods regardless of how the
    /// user wrote them.
    pub(crate) fn class_method_dunder(&self, class: Value, name: &str) -> Option<Value> {
        let found = self.find_in_class(class, name)?;
        Some(if self.is_method_wrapper(found) {
            self.method_wrapper_func(found)
        } else {
            found
        })
    }

    /// The raw `__init_subclass__` inherited from `class`'s BASES -- the class's OWN namespace is
    /// skipped, since a class's `__init_subclass__` governs its future subclasses, not itself. A
    /// classmethod wrapper is unwrapped to its function. `None` if no base defines one. Called at
    /// class creation with `cls` = the new class (CPython's class-creation hook).
    pub(crate) fn inherited_init_subclass(&self, class: Value) -> Option<Value> {
        for ancestor in self.class_mro_vec(class).into_iter().skip(1) {
            let namespace = self.read_slot(ancestor, 2);
            if let Some(found) = self.dict_lookup_str(namespace, "__init_subclass__") {
                return Some(if self.is_method_wrapper(found) {
                    self.method_wrapper_func(found)
                } else {
                    found
                });
            }
        }
        None
    }

    fn bind_class_member(&mut self, found: Value, owner: Value, class_value: Value) -> Result<Value, Trap> {
        if self.is_method_wrapper(found) {
            let func = self.method_wrapper_func(found);
            return if self.method_wrapper_is_class(found) {
                self.new_py_bound(class_value, func)
            } else {
                Ok(func)
            };
        }
        if self.is_user_function(found) && self.is_instance(owner) {
            return self.new_py_bound(owner, found);
        }
        Ok(found)
    }

    /// A `memoryview` over `[offset .. offset + length)` of `base` (a `bytes`/`bytearray`). Zero-copy:
    /// reads and (for a bytearray base) writes go straight to `base`'s buffer.
    pub fn new_memoryview(&mut self, base: Value, offset: usize, length: usize) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.memoryview_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, base.bits());
        self.heap.write_u32(reference.0 + 4, offset as u32);
        self.heap.write_u32(reference.0 + 8, length as u32);
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `memoryview`.
    #[must_use]
    pub fn is_memoryview(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.memoryview_type_id)
    }

    /// A memoryview's `(base, offset, length)`.
    fn memoryview_parts(&self, value: Value) -> (Value, usize, usize) {
        let reference = value.as_ref().expect("a memoryview");
        let base = Value::from_bits(self.heap.read_u32(reference.0));
        let offset = self.heap.read_u32(reference.0 + 4) as usize;
        let length = self.heap.read_u32(reference.0 + 8) as usize;
        (base, offset, length)
    }

    /// The bytes a memoryview covers (`base`'s buffer sliced to the view's window), or `None` if
    /// `value` is not a memoryview.
    fn memoryview_bytes(&self, value: Value) -> Option<&[u8]> {
        if !self.is_memoryview(value) {
            return None;
        }
        let (base, offset, length) = self.memoryview_parts(value);
        self.bytes_value(base)?.get(offset..offset + length)
    }

    /// Whether a memoryview is read-only (it views a `bytes`; a `bytearray` view is writable).
    fn memoryview_is_readonly(&self, value: Value) -> bool {
        let (base, _, _) = self.memoryview_parts(value);
        self.is_bytes(base)
    }

    /// The bytes a `bytes`/`bytearray`/`memoryview` exposes, so the three compare and hash by content.
    fn byte_view(&self, value: Value) -> Option<&[u8]> {
        self.bytes_value(value).or_else(|| self.memoryview_bytes(value))
    }

    /// A `property` from its accessors (`fget`/`fset`/`fdel`, each a function or `None`).
    pub fn new_property(&mut self, fget: Value, fset: Value, fdel: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.property_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, fget.bits());
        self.heap.write_u32(reference.0 + 4, fset.bits());
        self.heap.write_u32(reference.0 + 8, fdel.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `property`.
    #[must_use]
    pub fn is_property(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.property_type_id)
    }

    /// An unbound built-in method wrapping the method `name` (e.g. `str.lower`).
    pub fn new_unbound_method(&mut self, name: &str) -> Result<Value, Trap> {
        let name = self.new_str(name)?;
        let reference = self.alloc_object(self.unbound_method_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, name.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is an unbound built-in method.
    #[must_use]
    pub fn is_unbound_method(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.unbound_method_type_id)
    }

    /// The method name an unbound built-in method wraps (`str.lower` -> `"lower"`).
    #[must_use]
    pub fn unbound_method_name(&self, value: Value) -> Value {
        let reference = value.as_ref().expect("an unbound method");
        Value::from_bits(self.heap.read_u32(reference.0))
    }

    /// A bare `object()` instance -- an attribute-less identity token (the sentinel idiom). Each call
    /// returns a distinct object, so `object() is object()` is False.
    pub fn new_object_base(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.object_base_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a bare `object()` instance.
    #[must_use]
    pub fn is_object_base(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.object_base_type_id)
    }

    /// A property's accessors as `(fget, fset, fdel)` (each a function or `None`).
    pub fn property_accessors(&self, value: Value) -> (Value, Value, Value) {
        let reference = value.as_ref().expect("a property");
        (
            Value::from_bits(self.heap.read_u32(reference.0)),
            Value::from_bits(self.heap.read_u32(reference.0 + 4)),
            Value::from_bits(self.heap.read_u32(reference.0 + 8)),
        )
    }

    /// The `property` an instance's class defines for `name`, if any -- so an assignment or delete on
    /// that attribute routes through the property's setter/deleter instead of the instance `__dict__`.
    pub(crate) fn class_property(&self, instance: Value, name: &str) -> Option<Value> {
        if !self.is_instance(instance) {
            return None;
        }
        let class = self.read_slot(instance, 0);
        let found = self.find_in_class(class, name)?;
        self.is_property(found).then_some(found)
    }

    /// A fresh built-in iterator over `iterable` (str/bytes/list/tuple/dict/range/set); `iter()` of
    /// an existing iterator returns it unchanged. `py_next` advances it.
    pub fn new_iter(&mut self, iterable: Value) -> Result<Value, Trap> {
        if self.is_iter(iterable) || self.is_lazy_iter(iterable) {
            return Ok(iterable);
        }
        let iterable_ok = self.str_value(iterable).is_some()
            || self.bytes_value(iterable).is_some()
            || self.is_memoryview(iterable)
            || self.seq_value(iterable).is_some()
            || self.dict_value(iterable).is_some()
            || self.is_range(iterable)
            || self.set_value(iterable).is_some()
            || self.is_dict_view(iterable)
            || self.is_deque(iterable);
        if !iterable_ok {
            return Err(Trap::TypeError);
        }
        let reference = self.alloc_object(self.iter_type_id).ok_or(Trap::OutOfMemory)?;
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
        if let Some(data) = self.bytes_value(container) {
            if pos >= data.len() {
                return Ok(None);
            }
            let byte = data[pos];
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Value::fixnum(i32::from(byte)).ok_or(Trap::Overflow).map(Some);
        }
        if self.is_memoryview(container) {
            let Some(&byte) = self.memoryview_bytes(container).and_then(|data| data.get(pos)) else {
                return Ok(None);
            };
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Value::fixnum(i32::from(byte)).ok_or(Trap::Overflow).map(Some);
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
        if let Some(elems) = self.deque_elems(container) {
            let Some(&element) = elems.get(pos) else {
                return Ok(None);
            };
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(element));
        }
        if let Some(kind) = self.dict_view_kind(container) {
            let dict = self.dict_view_dict(container);
            let Some(&(key, value)) =
                self.dict_value(dict).and_then(|entries| entries.get(pos))
            else {
                return Ok(None);
            };
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return match kind {
                DictViewKind::Keys => Ok(Some(key)),
                DictViewKind::Values => Ok(Some(value)),
                DictViewKind::Items => Ok(Some(self.new_tuple(alloc::vec![key, value])?)),
            };
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
    pub fn new_class(&mut self, name: Value, bases: Value, namespace: Value) -> Result<Value, Trap> {
        let mut base_list: Vec<Value> = if self.is_tuple(bases) {
            self.seq_value(bases).cloned().unwrap_or_default()
        } else if self.is_class(bases) {
            alloc::vec![bases]
        } else {
            Vec::new()
        };
        base_list.retain(|b| b.as_builtin_id() != Some(crate::builtins::Builtin::Object.id()));
        for &b in &base_list {
            if !self.is_class(b) {
                return Err(Trap::TypeError);
            }
        }
        let reference = self.alloc_object(self.class_type_id).ok_or(Trap::OutOfMemory)?;
        let class = Value::from_ref(reference);
        self.heap.write_u32(reference.0, name.bits());
        self.heap.write_u32(reference.0 + 4, Value::NONE.bits());
        self.heap.write_u32(reference.0 + 8, namespace.bits());
        self.heap.write_u32(reference.0 + 12, Value::NONE.bits());
        let bases_tuple = self.new_tuple(base_list.clone())?;
        self.heap.write_u32(reference.0 + 4, bases_tuple.bits());
        let mro_vec = self.c3_linearize(class, &base_list)?;
        let mro_tuple = self.new_tuple(mro_vec)?;
        self.heap.write_u32(reference.0 + 12, mro_tuple.bits());
        Ok(class)
    }

    /// The C3 linearization of a class whose direct bases are `bases` -- the method resolution order
    /// `[class, ...ancestors...]`. C3 merges the parents' MROs plus the parents list, preserving each
    /// parent's local precedence and the monotonicity that makes cooperative multiple inheritance sound.
    /// An inconsistent hierarchy (no order satisfies every parent) is a `TypeError`, like CPython. Single
    /// inheritance and no-base fall out as the n=1 / n=0 cases (no special path). There is no modeled
    /// `object` root, so a baseless class linearizes to just `[class]`.
    fn c3_linearize(&self, class: Value, bases: &[Value]) -> Result<Vec<Value>, Trap> {
        let mut seqs: Vec<Vec<Value>> = bases.iter().map(|&b| self.class_mro_vec(b)).collect();
        if !bases.is_empty() {
            seqs.push(bases.to_vec());
        }
        let mut result = alloc::vec![class];
        loop {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() {
                break;
            }
            let head = seqs
                .iter()
                .map(|s| s[0])
                .find(|&h| !seqs.iter().any(|t| t[1..].contains(&h)))
                .ok_or(Trap::TypeError)?;
            result.push(head);
            for s in &mut seqs {
                s.retain(|&c| c != head);
            }
        }
        Ok(result)
    }

    /// A class's `__mro__` as a `Vec` (the linearized lookup order, starting with the class itself), or
    /// `[class]` for a class whose MRO slot is unset (defensive -- every class built by `new_class` has
    /// one). Empty for a non-class value.
    #[must_use]
    fn class_mro_vec(&self, class: Value) -> Vec<Value> {
        if !self.is_class(class) {
            return Vec::new();
        }
        let mro = self.read_slot(class, 3);
        let vec = self.seq_value(mro).cloned().unwrap_or_default();
        if vec.is_empty() {
            alloc::vec![class]
        } else {
            vec
        }
    }

    /// Allocates an instance of `class` with a fresh empty `__dict__` (the first half of
    /// calling a type; `__init__` runs in the interpreter's Call arm).
    pub fn new_object(&mut self, class: Value) -> Result<Value, Trap> {
        let dict = self.new_dict(Vec::new())?;
        let reference = self.alloc_object(self.instance_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, class.bits());
        self.heap.write_u32(reference.0 + 4, dict.bits());
        Ok(Value::from_ref(reference))
    }

    /// `object.__new__(class, *args, **kwargs)`: the allocator every `__new__` chain terminates in.
    /// Reached both as the default construction of a class that overrides no `__new__` and as an
    /// explicit `object.__new__(cls)` / `super().__new__(cls)` inside one that does.
    ///
    /// Extra arguments are only tolerated when something downstream will consume them, which is what
    /// makes `C(5)` legal for a class whose `__init__` takes a value and an error for one that takes
    /// nothing. The two messages are CPython's own and say different things: reaching the allocator
    /// with arguments it cannot use is the CALLER's mistake when `__new__` was overridden (that
    /// `__new__` should have consumed them), and the CLASS's when nothing was overridden at all.
    pub fn object_new(
        &mut self,
        class: Value,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        if !self.is_class(class) {
            let kind = self.type_name_of(class);
            let message = alloc::format!("object.__new__(X): X is not a type object ({kind})");
            return Err(self.raise_named_exception("TypeError", &message));
        }
        if !posargs.is_empty() || !kwargs.is_empty() {
            if self.find_in_class(class, "__new__").is_some() {
                let message =
                    "object.__new__() takes exactly one argument (the type to instantiate)";
                return Err(self.raise_named_exception("TypeError", message));
            }
            if self.find_in_class(class, "__init__").is_none() && !self.is_exception_class(class) {
                let name = String::from(self.str_value(self.read_slot(class, 0)).unwrap_or("object"));
                let message = alloc::format!("{name}() takes no arguments");
                return Err(self.raise_named_exception("TypeError", &message));
            }
        }
        self.new_object(class)
    }

    /// Every attribute name `value` actually has, sorted -- what `dir(value)` reports.
    ///
    /// **Every name it returns resolves.** That is the property that makes the answer usable, and it is
    /// what CPython's own `dir` does NOT promise: its list is documented as a convenience rather than a
    /// rigorously defined set, and for a runtime whose object model is smaller than CPython's the
    /// choice is between a list that describes THIS runtime and a list that names things `getattr`
    /// would then refuse. A name that is not there is a gap someone can see; a name that is there and
    /// does not work is a bug someone has to debug.
    ///
    /// For an instance, a class or a module the answer is EXHAUSTIVE, because their names live in
    /// dictionaries this can read. For a built-in value it is the method surface of its type plus the
    /// dunders that type exposes -- maintained as a list, and a test asserts every entry resolves.
    #[cfg(feature = "introspection")]
    pub fn dir_names(&mut self, value: Value) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        if self.is_instance(value) {
            let dict = self.read_slot(value, 1);
            if let Some(entries) = self.dict_value(dict) {
                for (key, _) in entries.clone() {
                    if let Some(name) = self.str_value(key) {
                        names.push(String::from(name));
                    }
                }
            }
            let class = self.read_slot(value, 0);
            names.extend(self.class_dir_names(class));
            if self.instance_slots(value).is_none() {
                names.push(String::from("__dict__"));
            }
            names.extend(INSTANCE_DUNDERS.iter().map(|n| String::from(*n)));
            if self.is_exception_value(value) {
                names.extend(EXCEPTION_ATTRIBUTES.iter().map(|n| String::from(*n)));
            }
            return sorted_unique(names);
        }
        if self.is_class(value) {
            names.extend(self.class_dir_names(value));
            names.extend(CLASS_ATTRIBUTES.iter().map(|n| String::from(*n)));
            return sorted_unique(names);
        }
        if self.is_module_object(value) {
            if let Some(entries) = self.dict_value(self.module_namespace(value)) {
                for (key, _) in entries.clone() {
                    if let Some(name) = self.str_value(key) {
                        names.push(String::from(name));
                    }
                }
            }
            return sorted_unique(names);
        }
        if self.is_user_function(value) {
            names.extend(FUNCTION_ATTRIBUTES.iter().map(|n| String::from(*n)));
            if let Some(dict) = self.function_dicts.iter().find(|(k, _)| *k == value.bits()) {
                if let Some(entries) = self.dict_value(dict.1) {
                    for (key, _) in entries.clone() {
                        if let Some(name) = self.str_value(key) {
                            names.push(String::from(name));
                        }
                    }
                }
            }
            return sorted_unique(names);
        }
        if self.is_py_bound(value) {
            names.extend(BOUND_METHOD_ATTRIBUTES.iter().map(|n| String::from(*n)));
            return sorted_unique(names);
        }
        for name in builtin_type_method_names(self, value) {
            names.push(String::from(name));
        }
        for name in crate::object::DUNDER_NAMES.iter() {
            if self.builtin_supports_dunder(value, name) {
                names.push(String::from(*name));
            }
        }
        names.push(String::from("__class__"));
        sorted_unique(names)
    }

    /// The str keys of `mapping`, sorted -- what a no-argument `dir()` reports over the frame's
    /// bindings. Kept beside `dir_names` because it answers the same question about a different source.
    pub fn sorted_key_names(&mut self, mapping: Value) -> Result<Value, Trap> {
        let mut names: Vec<String> = Vec::new();
        if let Some(entries) = self.dict_value(mapping) {
            for (key, _) in entries.clone() {
                if let Some(name) = self.str_value(key) {
                    names.push(String::from(name));
                }
            }
        }
        names.sort();
        names.dedup();
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            entries.push(self.new_str(&name)?);
        }
        self.new_list(entries)
    }

    /// The names a class and its bases provide -- the MRO's namespaces, in order.
    #[cfg(feature = "introspection")]
    fn class_dir_names(&mut self, class: Value) -> Vec<String> {
        let mut names = Vec::new();
        for ancestor in self.class_mro_vec(class) {
            let namespace = self.read_slot(ancestor, 2);
            if let Some(entries) = self.dict_value(namespace) {
                for (key, _) in entries.clone() {
                    if let Some(name) = self.str_value(key) {
                        names.push(String::from(name));
                    }
                }
            }
        }
        names
    }

    /// `object.__getstate__()`: the instance state a copy has to carry over, in the shape CPython
    /// gives it. An ordinary instance's state is its `__dict__`, or `None` when the instance has
    /// nothing set -- `None` rather than an empty dict, so a caller can tell "no state" from "state
    /// that happens to be empty" without inspecting it.
    ///
    /// A FULLY-SLOTTED instance has no `__dict__` to hand out, so its state is the `(dict, slots)`
    /// PAIR: `None` for the absent dict and a SNAPSHOT of the slot values in the second half. That is
    /// the shape CPython gives a slotted object, and a caller that restores state has to handle both
    /// halves anyway, because a class can acquire `__slots__` without its readers changing.
    ///
    /// The dict half is the instance's own dict, not a snapshot of it -- the same object `__dict__`
    /// hands out, as in CPython, so state read before a later attribute write reflects it.
    pub fn object_getstate(&mut self, instance: Value) -> Result<Value, Trap> {
        if !self.is_instance(instance) {
            return Ok(Value::NONE);
        }
        let dict = self.read_slot(instance, 1);
        let entries = self.dict_value(dict).cloned().unwrap_or_default();
        if self.instance_slots(instance).is_some() {
            let slots = self.new_dict(entries)?;
            return self.new_tuple(alloc::vec![Value::NONE, slots]);
        }
        if entries.is_empty() {
            return Ok(Value::NONE);
        }
        Ok(dict)
    }

    /// The `__new__` in `class`'s MRO, ready to be called WITH THE CLASS AS ITS FIRST ARGUMENT --
    /// `None` when the chain terminates in [`Self::object_new`]. `__new__` is an implicit STATIC
    /// method, so a plainly written one is returned raw (its `cls` parameter is an ordinary first
    /// parameter) and an explicit `staticmethod` unwraps to the same shape; an explicit
    /// `classmethod` keeps its bound class, which is how CPython ends up passing the class twice.
    pub(crate) fn find_new(&mut self, class: Value) -> Result<Option<Value>, Trap> {
        let Some(found) = self.find_in_class(class, "__new__") else {
            return Ok(None);
        };
        if self.is_method_wrapper(found) {
            let func = self.method_wrapper_func(found);
            if self.method_wrapper_is_class(found) {
                return Ok(Some(self.new_py_bound(class, func)?));
            }
            return Ok(Some(func));
        }
        Ok(Some(found))
    }

    /// The default construction of a class that has NO user `__init__` but is called with arguments:
    /// a `BaseException` subclass stores its positional args (so `str(exc)` renders the message and
    /// `exc.args` works, like `BaseException.__init__`); any other class ignores them, which
    /// [`Self::object_new`] has already refused for a class that overrides nothing.
    pub fn init_default_args(&mut self, instance: Value, args: &[Value]) -> Result<(), Trap> {
        if self.exception_classes.is_empty() {
            return Ok(());
        }
        let base_exception = self.exc_class_lookup("BaseException").ok_or(Trap::Malformed)?;
        if self.is_instance_of(instance, base_exception) {
            let args_tuple = self.new_tuple(args.to_vec())?;
            self.py_setattr_instance(instance, "args", args_tuple)?;
        }
        Ok(())
    }

    /// Allocates a bound Python method `[receiver, func]` -- the value `LoadAttr` yields for
    /// a function found on an instance's class; `Call` prepends the receiver as `self`.
    pub fn new_py_bound(&mut self, receiver: Value, func: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.py_bound_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, receiver.bits());
        self.heap.write_u32(reference.0 + 4, func.bits());
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `PyFunction` -- a DEFAULTED function `[func_index, defaults, kwdefaults, .., home]`.
    /// Only a `def` (or lambda) carrying default args needs this; a plain function stays a `function_ref`
    /// immediate. `defaults` is a tuple (or `None`); `kwdefaults` a dict (or `None`); `home` is the home
    /// module id (0 = entry) a cross-module call resolves the function against.
    pub fn new_py_function(
        &mut self,
        func_index: u32,
        defaults: Value,
        kwdefaults: Value,
        home: u16,
    ) -> Result<Value, Trap> {
        let reference = self
            .heap
            .alloc(self.py_function_type_id)
            .ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, func_index);
        self.heap.write_u32(reference.0 + 4, defaults.bits());
        self.heap.write_u32(reference.0 + 8, kwdefaults.bits());
        self.heap.write_u32(reference.0 + 12, Value::NONE.bits());
        self.heap.write_u32(reference.0 + 16, u32::from(home));
        Ok(Value::from_ref(reference))
    }

    /// A closure: a `PyFunction` carrying the `cells` (a tuple of Cells) it captured from the
    /// enclosing frame. `MakeFunction` with the CLOSURE flag builds this; calling it seeds the
    /// freevar half of the new frame's deref array.
    pub fn new_closure(
        &mut self,
        func_index: u32,
        defaults: Value,
        kwdefaults: Value,
        cells: Value,
        home: u16,
    ) -> Result<Value, Trap> {
        let function = self.new_py_function(func_index, defaults, kwdefaults, home)?;
        let reference = function.as_ref().ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0 + 12, cells.bits());
        Ok(function)
    }

    /// The captured cells of a `PyFunction` (the closure's freevar cells as a `Vec`, or empty for a
    /// plain function -- `cells` is `None`).
    #[must_use]
    pub fn py_function_cells(&self, func: Value) -> Vec<Value> {
        let Some(reference) = func.as_ref() else {
            return Vec::new();
        };
        let cells = Value::from_bits(self.heap.read_u32(reference.0 + 12));
        self.seq_value(cells).cloned().unwrap_or_default()
    }

    /// Whether `value` is a `PyFunction` (a defaulted function object).
    #[must_use]
    pub fn is_py_function(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.py_function_type_id)
    }

    /// The module-function index a `PyFunction` refers to.
    #[must_use]
    pub fn py_function_index(&self, func: Value) -> u32 {
        let reference = func.as_ref().expect("a PyFunction");
        self.heap.read_u32(reference.0)
    }

    /// The HOME module id a `PyFunction` carries (0 = entry).
    #[must_use]
    pub fn py_function_home(&self, func: Value) -> u16 {
        func.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + 16) as u16)
    }

    /// The home module id of any callable function VALUE -- a bare `function_ref`'s packed home, a
    /// `PyFunction`'s home field, else 0 (the entry). The single funnel the call dispatch reads to
    /// route a cross-module call to the callee's module.
    #[must_use]
    pub(crate) fn function_home(&self, func: Value) -> u16 {
        if let Some(home) = func.function_home_module() {
            home
        } else if self.is_py_function(func) {
            self.py_function_home(func)
        } else {
            0
        }
    }

    /// The positional DEFAULTS of a `PyFunction` as a vector (the defaults tuple's elements, or
    /// empty if it has none). They align to the trailing positional parameters at bind time.
    #[must_use]
    pub fn py_function_defaults(&self, func: Value) -> Vec<Value> {
        let reference = func.as_ref().expect("a PyFunction");
        let defaults = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        self.seq_value(defaults).cloned().unwrap_or_default()
    }

    /// The keyword-only DEFAULTS of a `PyFunction` -- the kwdefaults dict (a `{name: value}` map), or
    /// `None` if it has none. Bound by name to the keyword-only parameters at bind time (via
    /// [`ObjectModel::dict_get_str`]).
    #[must_use]
    pub fn py_function_kwdefaults(&self, func: Value) -> Value {
        match func.as_ref() {
            Some(reference) => Value::from_bits(self.heap.read_u32(reference.0 + 8)),
            None => Value::NONE,
        }
    }

    /// Allocates a generator object owning `frame` (its fresh, not-yet-run activation, with args
    /// already bound to locals), returning the heap value. The body does not run until the first
    /// resume; the frame lives in the `generators` arena and the heap object holds its index +
    /// `home_module` (the module whose functions + globals a resume runs the body against).
    pub fn new_generator(&mut self, frame: Frame, home_module: u16) -> Result<Value, Trap> {
        let index = self.generators.len() as u32;
        self.generators.push(Some(frame));
        let reference = self.alloc_object(self.generator_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        self.heap.write_u32(reference.0 + 4, u32::from(home_module));
        Ok(Value::from_ref(reference))
    }

    /// The home module id a generator carries (the module a resume runs its body against; 0 = entry).
    #[must_use]
    pub(crate) fn generator_module(&self, generator: Value) -> u16 {
        generator.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + 4) as u16)
    }

    /// Takes a recycled frame from the pool (its Vec buffers ready to reuse), or `None` if empty.
    pub(crate) fn take_pooled_frame(&mut self) -> Option<Frame> {
        self.frame_pool.pop()
    }

    /// Returns a finished call frame to the pool (cleared of Values, buffers kept), bounded so a
    /// deep-but-transient call chain does not permanently retain buffers.
    pub(crate) fn recycle_frame(&mut self, mut frame: Frame) {
        const FRAME_POOL_CAP: usize = 256;
        if self.frame_pool.len() < FRAME_POOL_CAP {
            frame.clear_for_reuse();
            self.frame_pool.push(frame);
        }
    }

    /// Whether `value` is a generator object.
    #[must_use]
    pub fn is_generator(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.generator_type_id)
    }

    /// Takes the suspended frame OUT of generator `gen` (leaving the slot `None`), or `None` if it
    /// has already been taken -- the generator is exhausted, or is currently running (a re-entrant
    /// resume). Pair with [`ObjectModel::put_generator_frame`] to suspend it again after a yield.
    pub fn take_generator_frame(&mut self, generator: Value) -> Option<Frame> {
        let reference = generator.as_ref()?;
        if self.heap.type_id_of(reference) != self.generator_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.generators.get_mut(index)?.take()
    }

    /// Suspends `frame` back into generator `gen` after it yielded, so the next resume continues
    /// it. The slot was emptied by [`ObjectModel::take_generator_frame`]; leaving it `None`
    /// instead (never calling this) marks the generator exhausted.
    pub fn put_generator_frame(&mut self, generator: Value, frame: Frame) {
        if let Some(reference) = generator.as_ref() {
            let index = self.heap.read_u32(reference.0) as usize;
            if let Some(slot) = self.generators.get_mut(index) {
                *slot = Some(frame);
            }
        }
    }

    /// Whether `value` is a user class object.
    #[must_use]
    pub fn is_class(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.class_type_id)
    }

    /// The class object of a class instance (its `type`); the precondition is that `value` is an
    /// instance ([`ObjectModel::is_instance`]).
    #[must_use]
    pub fn instance_class(&self, value: Value) -> Value {
        self.read_slot(value, 0)
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
        let reference = self.alloc_object(self.super_type_id).ok_or(Trap::OutOfMemory)?;
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
        let instance_type = if self.is_instance(receiver) {
            self.read_slot(receiver, 0)
        } else {
            class
        };
        let mro = self.class_mro_vec(instance_type);
        let start = mro.iter().position(|&c| c == class).map_or(mro.len(), |i| i + 1);
        let mut found = None;
        for &c in &mro[start..] {
            let namespace = self.read_slot(c, 2);
            if let Some(f) = self.dict_lookup_str(namespace, name) {
                found = Some(f);
                break;
            }
        }
        let found = match found {
            Some(found) => found,
            None => {
                if name == "__init__" && self.is_exception_value(receiver) {
                    return self.new_bound_method(receiver, EXC_INIT);
                }
                if name == "__new__" {
                    return self.new_bound_method(Value::NONE, OBJECT_NEW);
                }
                if matches!(name, "__init__" | "__init_subclass__") {
                    return self.new_bound_method(receiver, OBJECT_NOOP);
                }
                return Err(Trap::AttributeError);
            }
        };
        if self.is_method_wrapper(found) {
            let func = self.method_wrapper_func(found);
            return if self.method_wrapper_is_class(found) {
                let through = if self.is_class(receiver) { receiver } else { instance_type };
                self.new_py_bound(through, func)
            } else {
                Ok(func)
            };
        }
        if self.is_user_function(found) {
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

    /// Resolves `name` in `class`'s namespace, then along its MRO (the C3 linearization); `None` if
    /// unbound. The MRO walk replaces the single base chain, so multiple inheritance resolves correctly.
    fn find_in_class(&self, class: Value, name: &str) -> Option<Value> {
        for c in self.class_mro_vec(class) {
            let namespace = self.read_slot(c, 2);
            if let Some(found) = self.dict_lookup_str(namespace, name) {
                return Some(found);
            }
        }
        None
    }

    /// `instance.name` (`Op::LoadAttr` on a class instance): the instance `__dict__` first
    /// (returned as-is), then the class + base chain -- a function there binds to the
    /// instance (a [`Self::new_py_bound`]), a non-function is a class attribute; otherwise
    /// `AttributeError`.
    pub fn py_getattr_instance(&mut self, instance: Value, name: &str) -> Result<Value, Trap> {
        let dict = self.read_slot(instance, 1);
        if name == "__dict__" {
            if self.instance_slots(instance).is_some() {
                return Err(Trap::AttributeError);
            }
            return Ok(dict);
        }
        if let Some(found) = self.dict_lookup_str(dict, name) {
            return Ok(found);
        }
        let class = self.read_slot(instance, 0);
        if let Some(found) = self.find_in_class(class, name) {
            return self.bind_class_member(found, instance, class);
        }
        if matches!(name, "__cause__" | "__context__") && self.is_exception_value(instance) {
            return Ok(Value::NONE);
        }
        if name == "__suppress_context__" && self.is_exception_value(instance) {
            return Ok(Value::FALSE);
        }
        if name == "__init__" {
            if self.is_exception_value(instance) {
                return self.new_bound_method(instance, EXC_INIT);
            }
            return self.new_bound_method(instance, OBJECT_NOOP);
        }
        if name == "__new__" {
            let class = self.read_slot(instance, 0);
            return match self.find_new(class)? {
                Some(new) => Ok(new),
                None => self.new_bound_method(Value::NONE, OBJECT_NEW),
            };
        }
        if name == "__getstate__" {
            return self.new_bound_method(instance, OBJECT_GETSTATE);
        }
        if name == "value"
            && self
                .exc_class_lookup("StopIteration")
                .is_some_and(|class| self.is_instance_of(instance, class))
        {
            let value = self
                .instance_attr(instance, "args")
                .and_then(|args| self.seq_value(args))
                .and_then(|elements| elements.first().copied())
                .unwrap_or(Value::NONE);
            return Ok(value);
        }
        Err(Trap::AttributeError)
    }

    /// The union of `__slots__` names when EVERY class in the instance's MRO is slotted (so the
    /// instance has NO `__dict__`), else `None` (some class admits a dict). Gates both `__slots__`
    /// write enforcement and the `__dict__` attribute's absence on a fully-slotted instance.
    fn instance_slots(&self, instance: Value) -> Option<Vec<String>> {
        let mro = self.class_mro_vec(self.instance_class(instance));
        if mro.is_empty() {
            return None;
        }
        let mut slots = Vec::new();
        for class in &mro {
            match self.class_own_slots(*class) {
                Some(names) => slots.extend(names),
                None => return None,
            }
        }
        Some(slots)
    }

    /// `instance.name = value` (`Op::SetAttr`): stores into the instance `__dict__`.
    pub fn py_setattr_instance(&mut self, instance: Value, name: &str, value: Value) -> Result<(), Trap> {
        if let Some(slots) = self.instance_slots(instance) {
            if !slots.iter().any(|s| s == name) {
                let class_name = String::from(self.instance_class_name(instance).unwrap_or(""));
                let message = alloc::format!(
                    "'{class_name}' object has no attribute '{name}' and no __dict__ for setting new attributes"
                );
                return Err(self.with_message(Trap::AttributeError, &message));
            }
        }
        let key = self.new_str(name)?;
        let dict = self.read_slot(instance, 1);
        self.py_setitem(dict, key, value)
    }

    /// The restrictive `__slots__` names a class declares in its OWN namespace (a single str, or a
    /// tuple/list of str), or `None` when the class declares no `__slots__` OR lists `__dict__` (both
    /// mean instances keep a `__dict__`, so nothing is restricted). Read per-class (not the base chain)
    /// so the caller can require EVERY class in the MRO to be slotted before it enforces.
    fn class_own_slots(&self, class: Value) -> Option<Vec<String>> {
        let namespace = self.read_slot(class, 2);
        let declared = self.dict_lookup_str(namespace, "__slots__")?;
        let mut names = Vec::new();
        if let Some(single) = self.str_value(declared) {
            names.push(String::from(single));
        } else if let Some(elements) = self.seq_value(declared).cloned() {
            for element in elements {
                names.push(String::from(self.str_value(element)?));
            }
        }
        if names.iter().any(|n| n == "__dict__") {
            return None;
        }
        Some(names)
    }

    /// `C.name = value` (`Op::SetAttr` on a class object): stores into the class's OWN namespace dict
    /// (slot 2), never a base's -- so a class decorator can mutate the class it returns
    /// (`cls.tagged = True`), a class-level rebinding (`C.count = 0`) works, and instances then read
    /// the new value through the class. The base chain is the READ path ([`ObjectModel::find_in_class`]);
    /// a write always targets this class, matching CPython.
    pub fn py_setattr_class(&mut self, class: Value, name: &str, value: Value) -> Result<(), Trap> {
        let key = self.new_str(name)?;
        let namespace = self.read_slot(class, 2);
        self.py_setitem(namespace, key, value)
    }

    /// `f.name = value` (`Op::SetAttr` on a function object): a function has no `__dict__` slot, so its
    /// user attributes live in the `function_dicts` side-table, keyed by the function's identity. The
    /// dict is created on first write. Reads go through [`ObjectModel::function_attr`].
    pub fn py_setattr_function(&mut self, func: Value, name: &str, value: Value) -> Result<(), Trap> {
        let dict = self.function_dict_or_create(func);
        let key = self.new_str(name)?;
        self.py_setitem(dict, key, value)
    }

    /// The attribute `__dict__` for function `func`, creating an empty one on first access. Keyed by
    /// `func.bits()` -- a bare `function_ref` is stable per def, a PyFunction distinct per instance.
    fn function_dict_or_create(&mut self, func: Value) -> Value {
        let key = func.bits();
        if let Some((_, dict)) = self.function_dicts.iter().find(|(k, _)| *k == key) {
            return *dict;
        }
        let dict = self.new_dict(Vec::new()).unwrap_or(Value::NONE);
        self.function_dicts.push((key, dict));
        dict
    }

    /// The qualified name a user function carries in its code object (`f.__qualname__`), resolving
    /// the code against the function's HOME module (a `function_ref` immediate's packed index/home,
    /// or a `PyFunction`'s fields), or `None` if it is not a user function.
    fn function_qualname(&self, func: Value) -> Option<String> {
        let index = if let Some(idx) = func.as_function_index() {
            idx
        } else if self.is_py_function(func) {
            self.py_function_index(func)
        } else {
            return None;
        };
        let functions = self.managed_functions_rc(self.function_home(func))?;
        functions.get(index as usize).map(|code| code.name.clone())
    }

    /// The docstring on `func`'s code object, or `None` when it has none (or is not a user function).
    /// Resolved against the function's HOME module exactly as its qualified name is, so a cross-module
    /// function reports its own.
    #[must_use]
    fn function_doc(&self, func: Value) -> Option<String> {
        let index = if let Some(idx) = func.as_function_index() {
            idx
        } else if self.is_py_function(func) {
            self.py_function_index(func)
        } else {
            return None;
        };
        let functions = self.managed_functions_rc(self.function_home(func))?;
        functions.get(index as usize)?.doc.clone()
    }

    /// The value of function attribute `name` (`f.tag`), or `None` if `func` has no such attribute.
    #[must_use]
    fn function_attr(&self, func: Value, name: &str) -> Option<Value> {
        let key = func.bits();
        let dict = self.function_dicts.iter().find(|(k, _)| *k == key).map(|(_, d)| *d)?;
        self.dict_get_str(dict, name)
    }

    /// Deletes the named attribute from `instance`'s `__dict__` (`delattr(obj, name)` / `del
    /// obj.name`). An `AttributeError` if the value is not an instance or has no such attribute.
    pub fn py_delattr_instance(&mut self, instance: Value, name: &str) -> Result<(), Trap> {
        if !self.is_instance(instance) {
            return Err(Trap::AttributeError);
        }
        let dict = self.read_slot(instance, 1);
        let index = self
            .container_slot(dict, self.dict_type_id)
            .ok_or(Trap::AttributeError)?;
        let entries = core::mem::take(&mut self.dicts[index]);
        let mut found = false;
        let kept: Vec<(Value, Value)> = entries
            .into_iter()
            .filter(|(key, _)| {
                let matches = self.str_value(*key) == Some(name);
                found |= matches;
                !matches
            })
            .collect();
        self.dicts[index] = kept;
        if found {
            Ok(())
        } else {
            Err(Trap::AttributeError)
        }
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
        if self.is_user_function(found) {
            self.new_py_bound(instance, found).ok()
        } else {
            None
        }
    }

    /// How `instance.name` resolves when a USER descriptor (a class attribute whose own class defines
    /// `__get__`) sits on the class. `Get` = call its `__get__(instance, class)`; `Value` = the
    /// instance-dict entry that shadows a NON-data descriptor. `None` for the common case (no such
    /// class attr, or a method/property/plain value -- those fall through to the normal read path).
    /// Encodes CPython's precedence: a DATA descriptor (also `__set__`/`__delete__`) wins over the
    /// instance dict; a NON-data descriptor (only `__get__`) yields to it.
    pub(crate) fn instance_descriptor_read(
        &mut self,
        instance: Value,
        name: &str,
    ) -> Option<DescriptorRead> {
        if !self.is_instance(instance) {
            return None;
        }
        let class = self.read_slot(instance, 0);
        let attr = self.find_in_class(class, name)?;
        let get = self.find_dunder(attr, "__get__")?;
        let is_data = self.find_dunder(attr, "__set__").is_some()
            || self.find_dunder(attr, "__delete__").is_some();
        if is_data {
            return Some(DescriptorRead::Get(get));
        }
        if let Some(value) = self.instance_attr(instance, name) {
            return Some(DescriptorRead::Value(value));
        }
        Some(DescriptorRead::Get(get))
    }

    /// The `__set__` (bound to the descriptor) for `instance.name` when a user data descriptor sits on
    /// the class, else `None` -- so `instance.name = value` routes through it before the instance-dict
    /// store. (`property` stays a native fast path; this is for USER descriptors.)
    pub(crate) fn instance_set_descriptor(&mut self, instance: Value, name: &str) -> Option<Value> {
        if !self.is_instance(instance) {
            return None;
        }
        let class = self.read_slot(instance, 0);
        let attr = self.find_in_class(class, name)?;
        self.find_dunder(attr, "__set__")
    }

    /// The `__delete__` (bound to the descriptor) for `del instance.name` when a user data descriptor
    /// sits on the class, else `None`.
    pub(crate) fn instance_delete_descriptor(&mut self, instance: Value, name: &str) -> Option<Value> {
        if !self.is_instance(instance) {
            return None;
        }
        let class = self.read_slot(instance, 0);
        let attr = self.find_in_class(class, name)?;
        self.find_dunder(attr, "__delete__")
    }

    /// The `(name, __set_name__ bound to the class-body value)` pairs for every entry in `class`'s own
    /// namespace whose value's class defines `__set_name__` -- the class-creation walk that lets a
    /// descriptor learn the attribute name it was assigned to. Order follows the namespace dict.
    pub(crate) fn set_name_hooks(&mut self, class: Value) -> Vec<(Value, Value)> {
        let namespace = self.read_slot(class, 2);
        let Some(entries) = self.dict_value(namespace).cloned() else {
            return Vec::new();
        };
        let mut hooks = Vec::new();
        for (key, value) in entries {
            if let Some(hook) = self.find_dunder(value, "__set_name__") {
                hooks.push((key, hook));
            }
        }
        hooks
    }

    /// Builds the built-in exception class hierarchy on first use (idempotent), from the shared
    /// [`lamella_py_bytecode::EXCEPTION_HIERARCHY`] table (the one definition every engine
    /// derives from). Each entry's base is built before it; `""` is the root's
    /// (BaseException's) base.
    fn ensure_exception_types(&mut self) {
        if !self.exception_classes.is_empty() {
            return;
        }
        for &(name, base_name) in lamella_py_bytecode::EXCEPTION_HIERARCHY {
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
        self.is_instance_of(exc, target)
    }

    /// Whether `value` is a class instance whose class is `class` or one of its ancestors (anywhere on
    /// the class's MRO). Backs `MatchExc`/`except E` and the `isinstance` built-in for user classes.
    #[must_use]
    pub fn is_instance_of(&self, value: Value, class: Value) -> bool {
        if !self.is_instance(value) {
            return false;
        }
        let ty = self.read_slot(value, 0);
        self.is_subclass_of(ty, class)
    }

    /// Whether user class `cls` derives from user class `target` -- `target` anywhere on `cls`'s MRO
    /// (which includes `cls` itself). Backs `issubclass`. The MRO membership test covers multiple
    /// inheritance, where a single base chain would miss a second base.
    #[must_use]
    pub fn is_subclass_of(&self, cls: Value, target: Value) -> bool {
        self.class_mro_vec(cls).contains(&target)
    }

    /// Maps a raised interpreter [`Trap`] to a fresh instance of the matching built-in
    /// exception (so `except IndexError:` catches a real index error); `None` for the
    /// internal/fatal traps, which are not catchable Python exceptions.
    pub fn trap_to_exception(&mut self, trap: Trap) -> Option<Value> {
        if matches!(trap, Trap::OutOfMemory) {
            if let Some(reserved) = self.memory_error_reserve.take() {
                self.pending_trap_arg = None;
                return Some(reserved);
            }
        }
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
            Trap::OutOfMemory => "MemoryError",
            Trap::Raised | Trap::StackUnderflow | Trap::Unsupported | Trap::Malformed => {
                return None;
            }
        };
        let mut arg = self.pending_trap_arg.take();
        if arg.is_none() {
            arg = self.default_trap_message(trap);
        }
        let class = self.exception_class(name)?;
        let instance = self.new_object(class).ok()?;
        if let Some(arg) = arg {
            let _ = self.init_default_args(instance, &[arg]);
        }
        Some(instance)
    }

    /// The constant CPython message for the traps whose text never varies; `None` for traps whose
    /// message is data-dependent (attached at the site via `pending_trap_arg`) or absent.
    fn default_trap_message(&mut self, trap: Trap) -> Option<Value> {
        let message = match trap {
            Trap::ZeroDivisionError => "division by zero",
            Trap::RecursionError => "maximum recursion depth exceeded",
            _ => return None,
        };
        self.new_str(message).ok()
    }

    /// Attaches a one-shot context argument to the next trap-raised exception (see
    /// [`ObjectModel::pending_trap_arg`]). Call immediately before returning the bare trap.
    pub(crate) fn set_trap_arg(&mut self, arg: Value) {
        self.pending_trap_arg = Some(arg);
    }

    /// Attaches `message` (as the exception's text) to the next raised exception and returns
    /// `trap`, for `return Err(self.with_message(Trap::IndexError, "list index out of range"))`.
    pub(crate) fn with_message(&mut self, trap: Trap, message: &str) -> Trap {
        if let Ok(text) = self.new_str(message) {
            self.set_trap_arg(text);
        }
        trap
    }

    /// A fresh, no-argument instance of the named built-in exception (e.g. `GeneratorExit`).
    pub(crate) fn new_exception(&mut self, name: &str) -> Result<Value, Trap> {
        let class = self.exception_class(name).ok_or(Trap::Malformed)?;
        self.new_object(class)
    }

    /// Raises the named built-in exception with `message`: stashes the instance in the pending slot
    /// and returns `Trap::Raised` (for `return Err(self.raise_named_exception("RuntimeError", ...))`).
    pub(crate) fn raise_named_exception(&mut self, name: &str, message: &str) -> Trap {
        match self.new_exception(name) {
            Ok(exc) => {
                if !message.is_empty() {
                    if let Ok(text) = self.new_str(message) {
                        let _ = self.init_default_args(exc, &[text]);
                    }
                }
                self.set_pending_exception(exc);
                Trap::Raised
            }
            Err(trap) => trap,
        }
    }

    /// Raises the named exception carrying a single VALUE argument (not a string message) -- e.g. a
    /// generator's `return v` surfaces as `StopIteration(v)`, whose `.value` reads `v`. A `None` value
    /// raises the bare form (`args == ()`, so `.value` is `None`), matching CPython.
    pub(crate) fn raise_named_exception_with_value(&mut self, name: &str, value: Value) -> Trap {
        match self.new_exception(name) {
            Ok(exc) => {
                if value != Value::NONE {
                    let _ = self.init_default_args(exc, &[value]);
                }
                self.set_pending_exception(exc);
                Trap::Raised
            }
            Err(trap) => trap,
        }
    }

    /// Stashes the value a generator's body returned, for the resumer to read as the raised
    /// `StopIteration.value` ([`ObjectModel::take_generator_return`]). See the field's doc.
    pub(crate) fn set_generator_return(&mut self, value: Value) {
        self.generator_return = Some(value);
    }

    /// Takes the stashed generator return value (leaving `None`), or `None` if none was set (a
    /// re-exhausted generator raises a bare `StopIteration`, value `None`).
    pub(crate) fn take_generator_return(&mut self) -> Option<Value> {
        self.generator_return.take()
    }

    /// Stashes an exception thrown into a generator suspended in a `yield from`, for the re-run
    /// YieldFrom arm to forward into the sub-iterator ([`ObjectModel::take_yield_from_throw`]).
    pub(crate) fn set_yield_from_throw(&mut self, exc: Value) {
        self.yield_from_throw = Some(exc);
    }

    /// Takes the stashed `yield from` throw (leaving `None`): `Some` marks a throw/close resume of a
    /// delegating generator, `None` an ordinary send/next resume.
    pub(crate) fn take_yield_from_throw(&mut self) -> Option<Value> {
        self.yield_from_throw.take()
    }

    /// The Python type name of `value` -- `type(value).__name__`: `int` / `str` / `list` / `NoneType`
    /// / a user class's own name / ... -- for a diagnostic like a `TypeError` message. Degrades to
    /// `type` for a class object and `object` for the few values whose metatype is not modeled (a
    /// function, a module), which are rare as a failing operand.
    pub(crate) fn type_name_of(&self, value: Value) -> String {
        if value == Value::NONE {
            return String::from("NoneType");
        }
        if let Some(class) = crate::builtins::type_of(value, self) {
            if let Some(builtin) = class.as_builtin_id().and_then(crate::builtins::Builtin::from_id) {
                return String::from(builtin.python_name());
            }
            if let Some(name) = class.as_builtin_id().and_then(crate::stdlib::stdlib_name) {
                return String::from(name);
            }
            if let Some(name) = self.str_value(self.read_slot(class, 0)) {
                return String::from(name);
            }
        }
        if self.is_class(value) {
            return String::from("type");
        }
        String::from("object")
    }

    /// The `__name__` of a class VALUE itself (`int` / a user class's name slot) -- for a diagnostic
    /// like a match class-pattern arity error. Unlike [`Self::type_name_of`] (which names a value's
    /// TYPE), this names the class object passed in. Empty for a non-class.
    pub(crate) fn class_display_name(&self, class: Value) -> String {
        if let Some(builtin) = class.as_builtin_id().and_then(crate::builtins::Builtin::from_id) {
            return String::from(builtin.python_name());
        }
        self.str_value(self.read_slot(class, 0)).map(String::from).unwrap_or_default()
    }

    /// The display name of module `id`: the entry module (0) is `__main__` (as under `python x.py`),
    /// a managed module its registered name. Used to stamp a class's `__module__`.
    #[must_use]
    fn module_display_name(&self, id: u16) -> String {
        match id {
            0 => String::from("__main__"),
            k => self
                .managed_modules
                .get((k - 1) as usize)
                .map_or_else(String::new, |m| m.name.clone()),
        }
    }

    /// Stamps class `class` with its defining module as `__module__` in its namespace (CPython's own
    /// mechanism), so `Cls.__module__` reads it and `repr(Cls)` qualifies as `module.Name`. Called at
    /// class creation with the current module id.
    pub(crate) fn set_class_module(&mut self, class: Value, module_id: u16) -> Result<(), Trap> {
        let name = self.module_display_name(module_id);
        let namespace = self.read_slot(class, 2);
        let key = self.new_str("__module__")?;
        let value = self.new_str(&name)?;
        self.py_setitem(namespace, key, value)
    }

    /// The `TypeError` for indexing a sequence with a non-int, non-slice index -- CPython 3.14's
    /// per-container message: `string indices must be integers, not 'X'` for a str; `KIND indices must
    /// be integers or slices, not X` for a list/tuple/range. Attached as a trap arg (like the sibling
    /// IndexError), at the per-container index site in [`Self::py_getitem`].
    fn index_type_error(&mut self, container: Value, index: Value) -> Trap {
        let index_type = self.type_name_of(index);
        let message = if self.is_str(container) {
            alloc::format!("string indices must be integers, not '{index_type}'")
        } else {
            let kind = if self.is_tuple(container) {
                "tuple"
            } else if self.is_range(container) {
                "range"
            } else {
                "list"
            };
            alloc::format!("{kind} indices must be integers or slices, not {index_type}")
        };
        self.with_message(Trap::TypeError, &message)
    }

    /// The `TypeError` for a binary operator with no applicable operation on `lhs`/`rhs` -- a raised
    /// exception whose message matches CPython 3.14. The default is `unsupported operand type(s) for
    /// OP: 'L' and 'R'` (with `** or pow()` naming `**`); the sequence cases are special: `+` on a
    /// str/list/tuple reports `can only concatenate L (not "R") to L`, `+` on a bytes/bytearray
    /// reports `can't concat R to L`, and `*` of a sequence (incl. bytes) by a non-int reports
    /// `can't multiply sequence by non-int of type 'X'`. Called at the `Op::Binary` chokepoint on a
    /// bare `Trap::TypeError`, so it never overrides a message a user dunder raised (that arrives
    /// as `Trap::Raised`, carrying its own exception).
    pub(crate) fn binop_type_error(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Trap {
        let message = self.binop_type_error_message(op, lhs, rhs, false);
        self.raise_named_exception("TypeError", &message)
    }

    /// The `TypeError` for an augmented assignment ([`Op::InplaceBinOp`]) with no applicable
    /// operation: identical to [`ObjectModel::binop_type_error`] except the default spelling names
    /// the AUGMENTED operator (`unsupported operand type(s) for -=: ...`, and `**=` where the plain
    /// form says `** or pow()`) -- CPython renders the `=` there, while the concatenate/multiply
    /// sequence messages keep the plain-op text.
    pub(crate) fn inplace_binop_type_error(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Trap {
        let message = self.binop_type_error_message(op, lhs, rhs, true);
        self.raise_named_exception("TypeError", &message)
    }

    /// The message body shared by [`ObjectModel::binop_type_error`] (plain spelling) and
    /// [`ObjectModel::inplace_binop_type_error`] (augmented spelling).
    fn binop_type_error_message(
        &mut self,
        op: BinOp,
        lhs: Value,
        rhs: Value,
        augmented: bool,
    ) -> alloc::string::String {
        let lt = self.type_name_of(lhs);
        let rt = self.type_name_of(rhs);
        let lhs_seq = self.is_str(lhs) || self.is_list(lhs) || self.is_tuple(lhs);
        let rhs_seq = self.is_str(rhs) || self.is_list(rhs) || self.is_tuple(rhs);
        let lhs_bytes = self.bytes_value(lhs).is_some();
        let rhs_bytes = self.bytes_value(rhs).is_some();
        match op {
            BinOp::Add if lhs_seq => alloc::format!("can only concatenate {lt} (not \"{rt}\") to {lt}"),
            BinOp::Add if lhs_bytes => alloc::format!("can't concat {rt} to {lt}"),
            BinOp::Mul if lhs_seq || lhs_bytes => {
                alloc::format!("can't multiply sequence by non-int of type '{rt}'")
            }
            BinOp::Mul if rhs_seq || rhs_bytes => {
                alloc::format!("can't multiply sequence by non-int of type '{lt}'")
            }
            _ => {
                let sym = match op {
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::FloorDiv => "//",
                    BinOp::Mod => "%",
                    BinOp::BitAnd => "&",
                    BinOp::BitOr => "|",
                    BinOp::BitXor => "^",
                    BinOp::LShift => "<<",
                    BinOp::RShift => ">>",
                    BinOp::TrueDiv => "/",
                    BinOp::Pow if augmented => "**",
                    BinOp::Pow => "** or pow()",
                    BinOp::MatMul => "@",
                };
                let eq = if augmented { "=" } else { "" };
                alloc::format!("unsupported operand type(s) for {sym}{eq}: '{lt}' and '{rt}'")
            }
        }
    }

    /// The `TypeError` for an ORDERING comparison (`< <= > >=`) between values that do not support it
    /// -- a raised exception matching CPython 3.14's `'OP' not supported between instances of 'L' and
    /// 'R'`. Only ordering comparisons reach here: `==`/`!=` fall back to identity (never a TypeError)
    /// and `is`/`is not` are handled before the dispatch. Called on a bare `Trap::TypeError`, so a user
    /// comparison dunder that itself raised (`Trap::Raised`) keeps its own message.
    pub(crate) fn compare_type_error(&mut self, op: CmpOp, lhs: Value, rhs: Value) -> Trap {
        let lt = self.type_name_of(lhs);
        let rt = self.type_name_of(rhs);
        let sym = match op {
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Is | CmpOp::IsNot => "is",
        };
        let message = alloc::format!("'{sym}' not supported between instances of '{lt}' and '{rt}'");
        self.raise_named_exception("TypeError", &message)
    }

    /// The `TypeError` for `len(x)` on a value with no length -- CPython 3.14's `object of type 'X' has
    /// no len()`. Raised at the `len` built-in on a bare `Trap::TypeError` from [`Self::py_len`].
    pub(crate) fn len_type_error(&mut self, value: Value) -> Trap {
        let name = self.type_name_of(value);
        self.raise_named_exception("TypeError", &alloc::format!("object of type '{name}' has no len()"))
    }

    /// The `TypeError` for using `value` where a runtime operation requires it to be `what` (`callable`
    /// / `iterable` / `subscriptable` / ...) -- CPython 3.14's `'X' object is not WHAT`. Raised at the
    /// operation's source (e.g. calling a non-callable, iterating a non-iterable) on the type decision.
    pub(crate) fn object_is_not(&mut self, value: Value, what: &str) -> Trap {
        let name = self.type_name_of(value);
        self.raise_named_exception("TypeError", &alloc::format!("'{name}' object is not {what}"))
    }

    /// The `TypeError` for a unary operator (`- + ~`) with no applicable operand -- CPython's `bad
    /// operand type for unary OP: 'X'`. (`not` never errors, so it does not reach here.)
    pub(crate) fn unary_type_error(&mut self, op: UnaryOp, value: Value) -> Trap {
        let name = self.type_name_of(value);
        let sym = match op {
            UnaryOp::Neg => "-",
            UnaryOp::Pos => "+",
            UnaryOp::Invert => "~",
        };
        self.raise_named_exception("TypeError", &alloc::format!("bad operand type for unary {sym}: '{name}'"))
    }

    /// The `AttributeError` for `value.name` where `name` is not an attribute of `value` -- CPython
    /// 3.14's `'X' object has no attribute 'NAME'`. Raised at the attribute-access site on a bare
    /// `Trap::AttributeError` (an attribute miss), so it does not disturb `getattr`'s own contract
    /// (`hasattr` / `getattr(o, n, default)` still catch the bare trap and never see this message).
    pub(crate) fn attribute_error(&mut self, value: Value, name: &str) -> Trap {
        let type_name = self.tp_name_of(value);
        self.raise_named_exception(
            "AttributeError",
            &alloc::format!("'{type_name}' object has no attribute '{name}'"),
        )
    }

    /// The type name as CPython's `tp_name` spells it in hash/attribute error messages: the
    /// C-implemented collections types carry their DOTTED name (`collections.deque`), everything
    /// else (including the pure-Python `Counter`) the plain [`ObjectModel::type_name_of`]. Only
    /// those message sites use this; `type(x).__name__` and the operator messages stay undotted,
    /// exactly as CPython behaves.
    pub(crate) fn tp_name_of(&self, value: Value) -> String {
        if let Some(id) =
            crate::stdlib::stdlib_type_of(value, self).and_then(|class| class.as_builtin_id())
        {
            if crate::stdlib::stdlib_tp_name_dotted(id) {
                if let (Some(module), Some(name)) =
                    (crate::stdlib::stdlib_module_of(id), crate::stdlib::stdlib_name(id))
                {
                    return alloc::format!("{module}.{name}");
                }
            }
        }
        self.type_name_of(value)
    }

    /// Whether the currently pending exception is an instance of the named class -- consumed by the
    /// generator close() protocol (a GeneratorExit escaping cleanly ends the close).
    pub(crate) fn pending_exception_is(&self, name: &str) -> bool {
        match (self.pending_exception, self.exc_class_lookup(name)) {
            (Some(exc), Some(class)) => self.is_instance_of(exc, class),
            _ => false,
        }
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
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            return format_complex(re, im);
        }
        if let Some(n) = value.as_fixnum() {
            alloc::format!("{n}")
        } else if let Some(n) = self.long_value(value) {
            alloc::format!("{n}")
        } else if let Some(big) = self.bigint_value(value) {
            big.to_decimal_string()
        } else if let Some(f) = self.float_value(value) {
            format_float(f)
        } else if value == Value::TRUE {
            String::from("True")
        } else if value == Value::FALSE {
            String::from("False")
        } else if value.is_none() {
            String::from("None")
        } else if let Some(s) = self.str_value(value) {
            String::from(s)
        } else if self.is_instance(value) {
            self.instance_display(value)
        } else {
            self.repr(value)
        }
    }

    /// `str()` of a class instance: an exception renders as its MESSAGE (the str of its single arg,
    /// `""` for none, or the args tuple's repr for several -- Python's `str(exc)`); any other
    /// instance falls back to its `repr` (`<ClassName object>`).
    fn instance_display(&self, instance: Value) -> String {
        if self.is_exception_value(instance) {
            return self.exception_message(instance);
        }
        self.repr(instance)
    }

    /// Whether `value` is an exception instance (derives `BaseException`). Uses the immutable
    /// class lookup: if the exception types are not built yet, no exception instance exists, so
    /// this is `false`.
    #[must_use]
    fn is_exception_value(&self, value: Value) -> bool {
        self.exc_class_lookup("BaseException")
            .is_some_and(|base| self.is_instance_of(value, base))
    }

    /// Whether `value` is an exception CLASS -- a class object with `BaseException` in its MRO
    /// (`ValueError`, or a user subclass of one). The class twin of [`is_exception_value`], which
    /// tests an exception INSTANCE. Assumes the hierarchy is built (an exception class value implies it).
    fn is_exception_class(&self, value: Value) -> bool {
        if !self.is_class(value) {
            return false;
        }
        self.exc_class_lookup("BaseException")
            .is_some_and(|base| self.class_mro_vec(value).contains(&base))
    }

    /// The str message of an exception instance: `str(arg)` for a single stored arg, `""` for none,
    /// else the args tuple's repr (Python's `BaseException.__str__`).
    fn exception_message(&self, instance: Value) -> String {
        let Some(args) = self.instance_attr(instance, "args") else {
            return String::new();
        };
        let key_error = self
            .exc_class_lookup("KeyError")
            .is_some_and(|class| self.is_instance_of(instance, class));
        match self.seq_value(args) {
            Some(elements) if elements.len() == 1 => {
                if key_error {
                    self.repr(elements[0])
                } else {
                    self.display(elements[0])
                }
            }
            Some(elements) if elements.is_empty() => String::new(),
            _ => self.repr(args),
        }
    }

    /// An instance attribute read straight from its `__dict__` (no method/MRO resolution), or
    /// `None`. For rendering an exception's stored `args` / another instance's class.
    #[must_use]
    fn instance_attr(&self, instance: Value, name: &str) -> Option<Value> {
        let dict = self.read_slot(instance, 1);
        for (key, value) in self.dict_value(dict)? {
            if self.str_value(*key) == Some(name) {
                return Some(*value);
            }
        }
        None
    }

    /// Records `active` as `exception.__context__` for implicit chaining, but only if unset -- so a
    /// trap/builtin-raised exception raised WHILE another is being handled chains to it like CPython,
    /// without clobbering the `__context__` an explicit `raise` (Op::Raise) already set. A no-op when
    /// `active` IS the exception (a re-raise does not chain to itself). The unwind path calls this.
    pub(crate) fn chain_context_if_unset(&mut self, exception: Value, active: Value) -> Result<(), Trap> {
        if active == exception || self.instance_attr(exception, "__context__").is_some() {
            return Ok(());
        }
        self.py_setattr_instance(exception, "__context__", active)
    }

    /// The name of a class instance's class (its class object's name slot), or `None`.
    #[must_use]
    fn instance_class_name(&self, instance: Value) -> Option<&str> {
        let class = self.read_slot(instance, 0);
        self.str_value(self.read_slot(class, 0))
    }

    /// Raises `TypeError: unhashable type: '<Class>'` if `value` is a user instance whose class defines
    /// `__eq__` but not `__hash__` -- CPython makes such a class unhashable (defining `__eq__` nulls the
    /// inherited `__hash__`), so it cannot be a set element or dict key. A non-instance, or an instance
    /// whose class defines both dunders (or neither), is hashable. Our sets/dicts linear-scan `__eq__`
    /// and never call `__hash__`, so this is the guard that rejects an unhashable key/element the way
    /// CPython's hashing does.
    ///
    /// Caveat: the test is by MRO presence, so a subclass that adds `__eq__` while a BASE supplies
    /// `__hash__` is (leniently) treated as hashable, where CPython would null it -- a rare pattern.
    pub(crate) fn require_hashable(&mut self, value: Value) -> Result<(), Trap> {
        if !self.is_instance(value) {
            return Ok(());
        }
        if self.find_dunder(value, "__eq__").is_some() && self.find_dunder(value, "__hash__").is_none() {
            let name = self.instance_class_name(value).map(String::from);
            let message = match name.as_deref() {
                Some(class) => alloc::format!("unhashable type: '{class}'"),
                None => String::from("unhashable type"),
            };
            return Err(self.with_message(Trap::TypeError, &message));
        }
        Ok(())
    }

    /// Whether the arena is under enough pressure to be worth collecting -- asked at a SAFE POINT, not
    /// at an allocation, because a collection needs the interpreter's live frames as roots and an
    /// allocation deep inside this model cannot see them.
    ///
    /// Three quarters full by default. Under the stress knob, ALWAYS: see [`Self::set_gc_stress`].
    #[cfg(feature = "gc-collect")]
    #[must_use]
    pub fn under_memory_pressure(&self) -> bool {
        if self.gc_stress {
            return true;
        }
        if !self.collect_when_full {
            return false;
        }
        if let Some(probe) = self.arena_probe {
            let (used, capacity) = probe();
            if used.saturating_mul(4) >= capacity.saturating_mul(3) {
                return true;
            }
        }
        let used = self.heap.used() as usize;
        let capacity = self.heap.capacity();
        used.saturating_mul(4) >= capacity.saturating_mul(3)
    }

    /// Installs the embedder's view of the memory this model is allocated out of, as
    /// `(used, capacity)` bytes -- so a collection can be driven by what is actually running out.
    ///
    /// **Without one, pressure is judged by the object heap, which is the smaller part of what a
    /// program costs on a device and can sit comfortably below its trigger while the arena around it
    /// fills.** Every allocation this model makes beyond the heap block -- a string's bytes, a
    /// container's elements, a namespace -- comes from the embedder's allocator, and only the embedder
    /// knows how much of it is left. A firmware answers from its bump pointer in constant time.
    ///
    /// The same shape as [`Self::set_clock`], [`Self::set_console`] and [`Self::set_file_ops`]: absent
    /// by default, and its absence changes behavior rather than being papered over. What a host gets
    /// instead is the heap ratio alone, which is sound there because a host arena has no bound to be
    /// near -- and which still bounds a device, one allocation per heap object, just not tightly.
    #[cfg(feature = "gc-collect")]
    pub fn set_arena_probe(&mut self, probe: fn() -> (usize, usize)) {
        self.arena_probe = Some(probe);
    }

    /// Records that a driver loop has started, and answers whether it is the OUTERMOST one -- the
    /// question the safe point actually needs, and the reason it is asked here rather than computed by
    /// the driver.
    ///
    /// A collection needs EVERY live frame as a root, and a driver loop can only see the frames it owns.
    /// The interpreter's loop is re-entered for a cross-module call, a generator resume and a dunder
    /// dispatch, so a nested loop that collected would reclaim the locals of the loops beneath it. What
    /// makes a loop the outermost is therefore a property of the RUNTIME NESTING, and this counter is the
    /// only thing that observes it directly: the call depth a driver is handed is passed in by its
    /// caller, so a call site that forgets to increment it makes a nested loop claim to be the outermost
    /// one -- which is unsound, and which no reading of that call site reveals. Three corpus rows failed
    /// exactly that way. Counting here cannot be forgotten, because a loop that does not announce itself
    /// does not run.
    #[cfg(feature = "gc-collect")]
    pub(crate) fn enter_drive(&mut self) -> bool {
        self.drive_nesting += 1;
        self.drive_nesting == 1
    }

    /// Records that a driver loop has finished. Pairs with [`Self::enter_drive`] on every exit path.
    #[cfg(feature = "gc-collect")]
    pub(crate) fn leave_drive(&mut self) {
        self.drive_nesting = self.drive_nesting.saturating_sub(1);
    }

    /// Whether a safe point collects once the arena is three quarters full. **ON by default.** Turning
    /// it off makes the arena grow-only, so it must hold a program's TOTAL allocation rather than its
    /// live set -- which is what a setup-then-loop device tier may prefer, since an arena that fills up
    /// is a clean failure and a collection is work at an unpredictable moment.
    #[cfg(feature = "gc-collect")]
    pub fn set_collect_when_full(&mut self, on: bool) {
        self.collect_when_full = on;
    }

    /// Collects at EVERY safe point instead of under pressure. **A test instrument, and the reason the
    /// root set can be trusted**: a root that is not enumerated is only a defect when a collection
    /// happens while it holds the only reference to something, which under normal pressure might be one
    /// run in a thousand. Collecting on every op turns that into every run, so the whole corpus becomes
    /// a root-coverage test.
    #[cfg(feature = "gc-collect")]
    pub fn set_gc_stress(&mut self, on: bool) {
        self.gc_stress = on;
    }

    /// Reclaims unreachable objects, moving the survivors down -- **except those a container arena's
    /// slot still holds alive, which includes every reference CYCLE.** `extra_roots` reports the roots
    /// this model cannot see: the interpreter's live frame stack, which lives in the driver.
    ///
    /// **Every field below is a root because container CONTENTS live outside the arena**: a list's
    /// elements are a host-side `Vec<Value>` and only its small header is an arena object, so a moving
    /// collector has to be told about the Vec. That is why this is an enumeration rather than a graph
    /// walk, and why a missed field would silently reclaim live data -- which is what
    /// [`Self::set_gc_stress`] exists to catch.
    ///
    /// **What that enumeration costs, stated because a caller cannot see it from the name.** Every
    /// slot of the `seqs`/`dicts`/`sets` arenas is reported unconditionally, so a container is rooted
    /// whether or not anything can still reach it. An ACYCLIC dead container is reclaimed anyway --
    /// its own header is not reachable from any slot, so the header dies, its slot is cleared, and its
    /// contents stop being rooted one collection later. A CYCLE is not: for `a -> b -> a`, `a`'s slot
    /// marks `b`'s header and `b`'s slot marks `a`'s, so neither header dies, neither slot clears, and
    /// the pair survives every collection there will ever be. A parent/child link is enough to make
    /// one. Reachability from the genuine roots is what would decide this correctly; the arenas being
    /// roots is what stands in for it today.
    ///
    /// The heap calls its root closure TWICE (once to mark, once to relocate), so this must report the
    /// same slots both times. It does: nothing here consumes what it visits.
    #[cfg(feature = "gc-collect")]
    pub fn collect(&mut self, extra_roots: &mut ExtraRoots<'_>) {
        self.reserve_memory_error();
        let ObjectModel {
            heap,
            seqs,
            dicts,
            sets,
            exception_classes,
            pending_exception,
            memory_error_reserve,
            globals,
            modules,
            managed_globals,
            function_dicts,
            generators,
            frame_pool,
            pending_trap_arg,
            generator_return,
            yield_from_throw,
            ..
        } = self;
        heap.collect(|visit| {
            for seq in seqs.iter_mut() {
                for slot in seq.iter_mut() {
                    Value::trace_slot(slot, visit);
                }
            }
            for dict in dicts.iter_mut() {
                for (key, value) in dict.iter_mut() {
                    Value::trace_slot(key, visit);
                    Value::trace_slot(value, visit);
                }
            }
            for set in sets.iter_mut() {
                for slot in set.iter_mut() {
                    Value::trace_slot(slot, visit);
                }
            }
            for (_, value) in globals.iter_mut() {
                Value::trace_slot(value, visit);
            }
            for namespace in managed_globals.iter_mut() {
                for (_, value) in namespace.iter_mut() {
                    Value::trace_slot(value, visit);
                }
            }
            for (_, value) in modules.iter_mut() {
                Value::trace_slot(value, visit);
            }
            for (_, value) in exception_classes.iter_mut() {
                Value::trace_slot(value, visit);
            }
            for (_, value) in function_dicts.iter_mut() {
                Value::trace_slot(value, visit);
            }
            for slot in [
                pending_exception.as_mut(),
                pending_trap_arg.as_mut(),
                generator_return.as_mut(),
                yield_from_throw.as_mut(),
                memory_error_reserve.as_mut(),
            ]
            .into_iter()
            .flatten()
            {
                Value::trace_slot(slot, visit);
            }
            for suspended in generators.iter_mut().flatten() {
                suspended.trace(visit);
            }
            for pooled in frame_pool.iter_mut() {
                pooled.trace(visit);
            }
            extra_roots(visit);
        });
        self.release_dead_arena_slots();
    }

    /// Empties every handle-arena slot no surviving object still names, and returns the slot itself
    /// for reuse. **Without this a collection reclaims the object heap and nothing beside it, which on
    /// a device is the smaller part of what a program costs.**
    ///
    /// ## Why this runs AFTER the collection instead of computing liveness itself
    ///
    /// The plan this came from had the model mark its own object graph first, and then hand only the
    /// live slots to `heap.collect`. That would free a dead slot one collection sooner, and it would do
    /// it by REIMPLEMENTING REACHABILITY -- a second root enumeration, able to disagree with the first,
    /// where a root left out silently frees a slot that is still in use. This crate has already paid
    /// for one root list that could go stale and did not need a second.
    ///
    /// So liveness is not recomputed here: it is READ OFF the collection that just ran. Compaction
    /// leaves the survivors packed from the base to the bump pointer, each one a header word followed
    /// by its payload, so walking them costs one pass and cannot disagree with the collector about what
    /// is alive -- **it is the collector's own answer.**
    ///
    /// The price is one cycle of lag, and only for the three arenas whose payloads hold `Value`s: a
    /// dead container's ELEMENTS were traced as roots during the collection that has just finished, so
    /// they outlive it by one round and die in the next, once this pass has emptied the slot that was
    /// keeping them. `strings`, `bigints` and `byte_buffers` hold no `Value`s and have no lag at all.
    /// The high-water stays bounded either way, which is what the bar asks.
    ///
    /// ## What it depends on, stated because it is another crate's shape
    ///
    /// That the heap packs survivors contiguously from [`lamella_gc::heap::ALIGN`], each object being
    /// [`lamella_gc::heap::HEADER_SIZE`] bytes of header plus a payload padded to `ALIGN`. All of it is
    /// public API, and only a PINNED survivor would break it by leaving a gap -- which this model never
    /// asks for, because it calls `collect` and not `collect_with_pins`. **The walk asserts it lands
    /// exactly on the bump pointer**, so if that shape ever changes this fails loudly and immediately
    /// rather than quietly freeing a slot that belongs to somebody.
    #[cfg(feature = "gc-collect")]
    fn release_dead_arena_slots(&mut self) {
        let mut live = LiveSlots {
            strings: alloc::vec![false; self.strings.len()],
            seqs: alloc::vec![false; self.seqs.len()],
            dicts: alloc::vec![false; self.dicts.len()],
            sets: alloc::vec![false; self.sets.len()],
            bigints: alloc::vec![false; self.bigints.len()],
            byte_buffers: alloc::vec![false; self.byte_buffers.len()],
        };
        live.mark_all(&self.freed_slots);

        let top = self.heap.top();
        let mut header_addr = lamella_gc::heap::ALIGN;
        while header_addr < top {
            let type_id = self.heap.read_u32(header_addr);
            let payload_addr = header_addr + lamella_gc::heap::HEADER_SIZE;
            if let Some(arena) = self.arena_of(type_id) {
                live.mark(arena, self.heap.read_u32(payload_addr) as usize);
            }
            let payload_size = self.heap.type_descs()[type_id as usize].payload_size;
            header_addr = payload_addr + payload_size.next_multiple_of(lamella_gc::heap::ALIGN);
        }
        assert_eq!(
            header_addr, top,
            "the survivor walk overran the bump pointer, so the heap no longer packs objects the way \
             this pass assumes and slots would be freed at random -- refusing to guess"
        );

        empty_dead_slots(&mut self.strings, &live.strings, &mut self.freed_slots.strings, String::new);
        empty_dead_slots(&mut self.seqs, &live.seqs, &mut self.freed_slots.seqs, Vec::new);
        empty_dead_slots(&mut self.dicts, &live.dicts, &mut self.freed_slots.dicts, Vec::new);
        empty_dead_slots(&mut self.sets, &live.sets, &mut self.freed_slots.sets, Vec::new);
        empty_dead_slots(&mut self.bigints, &live.bigints, &mut self.freed_slots.bigints, BigInt::zero);
        empty_dead_slots(
            &mut self.byte_buffers,
            &live.byte_buffers,
            &mut self.freed_slots.byte_buffers,
            Vec::new,
        );
    }

    /// The handle arena a heap object of `type_id` indexes into, or `None` for a type that owns its
    /// whole payload. Every arena-backed type keeps that index in its FIRST payload word, which is what
    /// lets one walk serve all six.
    ///
    /// **A type missing from this list has its slot freed while it is still in use**, and the two
    /// things that catch it are the same two that hold the root enumeration honest: the whole corpus
    /// under `LAMELLA_GC_STRESS=1`, where a reused slot makes a small program answer wrongly, and
    /// `arena_slot_reuse.rs`, which builds one of every arena-backed kind and reads it back across a
    /// collection.
    #[cfg(feature = "gc-collect")]
    fn arena_of(&self, type_id: u32) -> Option<ArenaKind> {
        if type_id == self.str_type_id {
            return Some(ArenaKind::Strings);
        }
        if type_id == self.list_type_id
            || type_id == self.tuple_type_id
            || type_id == self.deque_type_id
            || type_id == self.ntinstance_type_id
        {
            return Some(ArenaKind::Seqs);
        }
        if type_id == self.dict_type_id
            || type_id == self.defaultdict_type_id
            || type_id == self.counter_type_id
            || type_id == self.ordereddict_type_id
        {
            return Some(ArenaKind::Dicts);
        }
        if type_id == self.set_type_id || type_id == self.frozenset_type_id {
            return Some(ArenaKind::Sets);
        }
        if type_id == self.bigint_type_id {
            return Some(ArenaKind::Bigints);
        }
        if type_id == self.bytes_type_id || type_id == self.bytearray_type_id {
            return Some(ArenaKind::ByteBuffers);
        }
        None
    }

    /// Builds the `MemoryError` that will be handed over when the heap runs out, if one is not already
    /// waiting. See [`ObjectModel::memory_error_reserve`].
    ///
    /// Costs nothing until the first collection, so a program that never comes near the limit never
    /// pays for the exception hierarchy -- the same laziness the built-in exception classes already
    /// have. Failure to build is silently accepted: it means the heap was too full even here, which is
    /// the state this exists to report and not one it can do anything about.
    #[cfg(feature = "gc-collect")]
    fn reserve_memory_error(&mut self) {
        if self.memory_error_reserve.is_some() {
            return;
        }
        if let Some(class) = self.exception_class("MemoryError") {
            if let Ok(instance) = self.new_object(class) {
                self.memory_error_reserve = Some(instance);
            }
        }
    }

    /// Installs a console the embedder owns: every `print` goes STRAIGHT to it and nothing is retained.
    ///
    /// Without one, output accumulates until [`Self::take_stdout`] drains it, which is what a host
    /// harness and a browser want -- they ask for a whole run's output at the end. **A device cannot
    /// afford that**: the accumulation is a program's TOTAL output held in an arena measured in tens of
    /// kilobytes, and a program that runs until the power goes off has no end at which to hand it over.
    /// A firmware installs its UART here and the text leaves as it is produced.
    pub fn set_console(&mut self, console: fn(&str)) {
        self.console_fn = Some(console);
    }

    /// Appends a `print()` line (already formatted) plus a newline to the captured output, or writes it
    /// straight through to an installed console.
    pub fn write_line(&mut self, line: &str) {
        if let Some(console) = self.console_fn {
            console(line);
            console("\n");
            return;
        }
        self.stdout.push_str(line);
        self.stdout.push('\n');
    }

    /// Appends `text` to the captured output WITHOUT a trailing newline -- for `print(..., end=s)`,
    /// which supplies its own terminator -- or writes it straight through to an installed console.
    pub fn write(&mut self, text: &str) {
        if let Some(console) = self.console_fn {
            console(text);
            return;
        }
        self.stdout.push_str(text);
    }

    /// Drains the captured `print` output. Always EMPTY when a console is installed, because nothing
    /// was captured -- the text already left through it.
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

    /// What this model currently costs, broken down by where the bytes are.
    ///
    /// **Ask this rather than the heap's occupancy, and the difference is not a refinement.** A host
    /// hands [`ObjectModel::new`] an arena that IS the object heap, so on a host the heap's occupancy
    /// looks like a RAM budget and is not one; a device takes the heap as ONE block from an arena and
    /// then allocates every table below out of what is left. A program whose heap occupancy is flat
    /// while its string arena climbs is on its way to exhausting a device and looks perfectly steady
    /// from the heap. That is the observed case rather than a hypothetical one.
    ///
    /// **What this does NOT count, stated so the number is not read as more than it is:** the decoded
    /// bundle (`managed_modules` / `managed_functions` -- a program's CODE, fixed once the program is
    /// loaded, and the largest thing left out: importing one bundled module costs tens of kilobytes),
    /// the type table (built whole at construction and never grown), the host-only peripheral
    /// simulators, and the heap's unused capacity. Everything counted here is data a running program
    /// produces, which is the part that can climb.
    ///
    /// **Cost, and the reason it is a walk rather than a running counter.** It is O(live slots), which
    /// is cheap where it is asked -- a gate, a report, a check before a large allocation -- and far too
    /// expensive to ask per op, so it is deliberately NOT what the collector's trigger consults.
    /// A counter maintained at every arena mutation would be O(1), and it was rejected: it would make
    /// this number depend on every present and future mutation site remembering to update it, and a
    /// property that lives in a convention across dozens of call sites is not a property -- the same
    /// lesson the drive-nesting counter came from. The quantity a trigger actually needs is the arena's
    /// own frontier, which an embedder can answer exactly and in constant time; a number reconstructed
    /// here could at best agree with it.
    ///
    /// Held to the system allocator by `tests/footprint_accounting.rs`, which weighs a program's run on
    /// a scale this model does not control: a table added and not accounted for shows up there as an
    /// allocator delta with no matching footprint delta, without anyone having thought of the table.
    #[must_use]
    pub fn footprint(&self) -> Footprint {
        let strings = Arena {
            slots: self.strings.capacity() * size_of::<String>(),
            payload: self.strings.iter().map(String::capacity).sum(),
        };
        let sequences = Arena {
            slots: self.seqs.capacity() * size_of::<Vec<Value>>(),
            payload: self.seqs.iter().map(|seq| seq.capacity() * size_of::<Value>()).sum(),
        };
        let dicts = Arena {
            slots: self.dicts.capacity() * size_of::<Vec<(Value, Value)>>(),
            payload: self
                .dicts
                .iter()
                .map(|dict| dict.capacity() * size_of::<(Value, Value)>())
                .sum(),
        };
        let sets = Arena {
            slots: self.sets.capacity() * size_of::<Vec<Value>>(),
            payload: self.sets.iter().map(|set| set.capacity() * size_of::<Value>()).sum(),
        };
        let bigints = Arena {
            slots: self.bigints.capacity() * size_of::<BigInt>(),
            payload: self.bigints.iter().map(BigInt::footprint).sum(),
        };
        let byte_buffers = Arena {
            slots: self.byte_buffers.capacity() * size_of::<Vec<u8>>(),
            payload: self.byte_buffers.iter().map(Vec::capacity).sum(),
        };

        let named = |entries: &[(String, Value)]| -> usize {
            entries.iter().map(|(name, _)| name.capacity()).sum::<usize>()
        };
        let namespaces = self.globals.capacity() * size_of::<(String, Value)>()
            + named(&self.globals)
            + self.managed_globals.capacity() * size_of::<Vec<(String, Value)>>()
            + self
                .managed_globals
                .iter()
                .map(|namespace| {
                    namespace.capacity() * size_of::<(String, Value)>() + named(namespace)
                })
                .sum::<usize>()
            + self.modules.capacity() * size_of::<(String, Value)>()
            + named(&self.modules)
            + self.exception_classes.capacity() * size_of::<(&'static str, Value)>()
            + self.function_dicts.capacity() * size_of::<(u32, Value)>();

        let frames = self.generators.capacity() * size_of::<Option<Frame>>()
            + self.generators.iter().flatten().map(Frame::footprint).sum::<usize>()
            + self.frame_pool.capacity() * size_of::<Frame>()
            + self.frame_pool.iter().map(Frame::footprint).sum::<usize>();

        Footprint {
            objects: self.heap.used() as usize,
            strings,
            sequences,
            dicts,
            sets,
            bigints,
            byte_buffers,
            namespaces,
            frames,
            stdout: self.stdout.capacity(),
        }
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
        let reference = self.alloc_object(type_id).ok_or(Trap::OutOfMemory)?;
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
        if name == "__class__" {
            if let Some(class) = crate::builtins::type_of(obj, self) {
                return Ok(class);
            }
        }
        if name == "__call__" && crate::builtins::value_is_callable(obj, self) {
            return self.new_bound_method(obj, CALL_DUNDER);
        }
        if name == "__next__" && (self.is_iter(obj) || self.is_lazy_iter(obj)) {
            return self.new_bound_method(obj, NEXT_DUNDER);
        }
        if let Some(id) = builtin_dunder_id(name) {
            if self.builtin_supports_dunder(obj, name) {
                return self.new_bound_method(obj, id);
            }
        }
        if let Some(id) = obj.as_builtin_id() {
            if name == "__name__" {
                if let Some(builtin) = Builtin::from_id(id) {
                    return self.new_str(builtin.python_name());
                }
                if let Some(stdlib_name) = crate::stdlib::stdlib_name(id) {
                    return self.new_str(stdlib_name);
                }
            }
            if id == Builtin::Object.id() && name == "__new__" {
                return self.new_bound_method(Value::NONE, OBJECT_NEW);
            }
            if id == Builtin::Dict.id() && name == "fromkeys" {
                return Ok(Value::builtin_ref(Builtin::DictFromkeys.id()));
            }
            if id == Builtin::Int.id() && name == "from_bytes" {
                return Ok(Value::builtin_ref(Builtin::IntFromBytes.id()));
            }
            if id == Builtin::Bytes.id() && name == "fromhex" {
                return Ok(Value::builtin_ref(Builtin::BytesFromhex.id()));
            }
            if id == Builtin::Float.id() && name == "fromhex" {
                return Ok(Value::builtin_ref(Builtin::FloatFromhex.id()));
            }
            if id == Builtin::Str.id() && name == "maketrans" {
                return Ok(Value::builtin_ref(Builtin::StrMaketrans.id()));
            }
            let unbound = (id == Builtin::Str.id() && str_method_id(name).is_some())
                || (id == Builtin::Int.id() && int_method_id(name).is_some())
                || (id == Builtin::Bytes.id() && bytes_method_id(name, false).is_some());
            if unbound {
                return self.new_unbound_method(name);
            }
            if let Some(builtin) = Builtin::from_id(id) {
                if type_object_supports_dunder(builtin, name) {
                    return self.new_unbound_method(name);
                }
            }
            return Err(Trap::AttributeError);
        }
        if self.is_int(obj) {
            match name {
                "numerator" | "real" => {
                    return Ok(match obj {
                        Value::TRUE => Value::fixnum(1).ok_or(Trap::Overflow)?,
                        Value::FALSE => Value::fixnum(0).ok_or(Trap::Overflow)?,
                        _ => obj,
                    });
                }
                "denominator" => return Value::fixnum(1).ok_or(Trap::Overflow),
                "imag" => return Value::fixnum(0).ok_or(Trap::Overflow),
                "is_integer" => return Ok(Value::builtin_ref(Builtin::IntIsInteger.id())),
                "from_bytes" => return Ok(Value::builtin_ref(Builtin::IntFromBytes.id())),
                _ => {}
            }
            let method_id = int_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if self.is_float(obj) {
            match name {
                "real" => return Ok(obj),
                "imag" => return self.new_float(0.0),
                _ => {}
            }
            let method_id = float_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if self.is_user_function(obj) {
            if let Some(value) = self.function_attr(obj, name) {
                return Ok(value);
            }
            if name == "__doc__" {
                return match self.function_doc(obj) {
                    Some(doc) => self.new_str(&doc),
                    None => Ok(Value::NONE),
                };
            }
            if matches!(name, "__name__" | "__qualname__") {
                if let Some(qualname) = self.function_qualname(obj) {
                    if qualname.contains("<lambda") {
                        return self.new_str("<lambda>");
                    }
                    if name == "__qualname__" {
                        return self.new_str(&qualname);
                    }
                    return self.new_str(qualname.rsplit('.').next().unwrap_or(&qualname));
                }
            }
            return Err(Trap::AttributeError);
        }
        let reference = obj.as_ref().ok_or(Trap::AttributeError)?;
        let type_id = self.heap.type_id_of(reference);
        if type_id == self.str_type_id {
            if name == "maketrans" {
                return Ok(Value::builtin_ref(Builtin::StrMaketrans.id()));
            }
            let method_id = str_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.bytes_type_id || type_id == self.bytearray_type_id {
            let method_id = bytes_method_id(name, type_id == self.bytearray_type_id)
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.memoryview_type_id {
            let (base, _, length) = self.memoryview_parts(obj);
            return match name {
                "readonly" => Ok(Value::from_bool(self.memoryview_is_readonly(obj))),
                "obj" => Ok(base),
                "nbytes" => Value::fixnum(length as i32).ok_or(Trap::Overflow),
                "itemsize" | "ndim" => Value::fixnum(1).ok_or(Trap::Overflow),
                "format" => self.new_str("B"),
                "shape" => {
                    let n = Value::fixnum(length as i32).ok_or(Trap::Overflow)?;
                    self.new_tuple(alloc::vec![n])
                }
                _ => {
                    let method_id = memoryview_method_id(name).ok_or(Trap::AttributeError)?;
                    self.new_bound_method(obj, method_id)
                }
            };
        }
        if type_id == self.slice_type_id {
            let (start, stop, step) = self.slice_components(obj);
            return match name {
                "start" => Ok(start),
                "stop" => Ok(stop),
                "step" => Ok(step),
                _ => {
                    let method_id = slice_method_id(name).ok_or(Trap::AttributeError)?;
                    self.new_bound_method(obj, method_id)
                }
            };
        }
        if type_id == self.range_type_id {
            let (start, stop, step) = self.range_bounds(obj);
            let as_int = |n: i64| Value::fixnum(i32::try_from(n).unwrap_or(0)).ok_or(Trap::Overflow);
            return match name {
                "start" => as_int(start),
                "stop" => as_int(stop),
                "step" => as_int(step),
                _ => Err(Trap::AttributeError),
            };
        }
        if type_id == self.property_type_id {
            let (fget, fset, fdel) = self.property_accessors(obj);
            return match name {
                "fget" => Ok(fget),
                "fset" => Ok(fset),
                "fdel" => Ok(fdel),
                _ => {
                    let method_id = property_method_id(name).ok_or(Trap::AttributeError)?;
                    self.new_bound_method(obj, method_id)
                }
            };
        }
        if type_id == self.list_type_id {
            let method_id = list_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.dict_type_id {
            let method_id = dict_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.defaultdict_type_id {
            if name == "default_factory" {
                return Ok(self.defaultdict_factory(obj).unwrap_or(Value::NONE));
            }
            let method_id = dict_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.counter_type_id {
            let method_id = counter_method_id(name)
                .or_else(|| dict_method_id(name))
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.ordereddict_type_id {
            let method_id = odict_method_id(name)
                .or_else(|| dict_method_id(name))
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.deque_type_id {
            if name == "maxlen" {
                return match self.deque_maxlen(obj).unwrap_or(None) {
                    Some(m) => Value::fixnum(m as i32).ok_or(Trap::Overflow),
                    None => Ok(Value::NONE),
                };
            }
            let method_id = deque_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.ntinstance_type_id {
            let class = self.ntinstance_class(obj).unwrap_or(Value::NONE);
            if name == "_fields" {
                return Ok(self.ntclass_fields_tuple(class));
            }
            let fields = self.ntclass_fields(class);
            if let Some(at) = fields.iter().position(|f| f == name) {
                let elems = self.seq_value(obj).ok_or(Trap::AttributeError)?;
                return elems.get(at).copied().ok_or(Trap::AttributeError);
            }
            let method_id = nt_method_id(name)
                .or_else(|| tuple_method_id(name))
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.ntclass_type_id {
            if name == "_fields" {
                return Ok(self.ntclass_fields_tuple(obj));
            }
            if name == "__name__" {
                return Ok(self.read_slot(obj, 0));
            }
            return Err(Trap::AttributeError);
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
        #[cfg(feature = "complex")]
        if type_id == self.complex_type_id {
            let (re, im) = self.complex_value(obj).ok_or(Trap::AttributeError)?;
            return match name {
                "real" => self.new_float(re),
                "imag" => self.new_float(im),
                _ => {
                    let method_id = complex_method_id(name).ok_or(Trap::AttributeError)?;
                    self.new_bound_method(obj, method_id)
                }
            };
        }
        if type_id == self.generator_type_id {
            let method_id = crate::interp::generator_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.module_type_id {
            let namespace = self.read_slot(obj, 0);
            let index = self
                .container_slot(namespace, self.dict_type_id)
                .ok_or(Trap::AttributeError)?;
            return self.dicts[index]
                .iter()
                .find(|(key, _)| self.str_value(*key) == Some(name))
                .map(|(_, value)| *value)
                .ok_or(Trap::AttributeError);
        }
        if type_id == self.gpio_type_id {
            let method_id = crate::gpio::gpio_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.board_type_id {
            if let Some(pin) = self.board.pin_id(name) {
                return Value::fixnum(pin as i32).ok_or(Trap::Overflow);
            }
            if let Some(instance) = self.board.uart_instance(name) {
                return self.new_uart_resource(instance);
            }
            if let Some(instance) = self.board.spi_instance(name) {
                return self.new_spi_resource(instance);
            }
            if let Some(instance) = self.board.i2c_instance(name) {
                return self.new_i2c_resource(instance);
            }
            if let Some((channel, pin)) = self.board.adc_resource(name) {
                return self.new_adc_resource(channel, pin);
            }
            if let Some((tx, rx)) = self.board.uart_default_pins(0) {
                match name {
                    "TX" => return Value::fixnum(tx as i32).ok_or(Trap::Overflow),
                    "RX" => return Value::fixnum(rx as i32).ok_or(Trap::Overflow),
                    _ => {}
                }
            }
            return Err(Trap::AttributeError);
        }
        if type_id == self.pin_type_id {
            let method_id = crate::gpio::pin_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.uart_type_id {
            let method_id = crate::uart::uart_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.uart_port_type_id {
            use crate::uart::{PORT_W_BAUDRATE, PORT_W_DATA_BITS, PORT_W_PARITY, PORT_W_STOP_BITS};
            match name {
                "baudrate" => {
                    let value = self.port_word(obj, PORT_W_BAUDRATE);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                "data_bits" => {
                    let value = self.port_word(obj, PORT_W_DATA_BITS);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                "stop_bits" => {
                    let value = self.port_word(obj, PORT_W_STOP_BITS);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                "parity" => {
                    let code = self.port_word(obj, PORT_W_PARITY);
                    return self.new_str(crate::uart::parity_name(code));
                }
                _ => {}
            }
            let method_id = crate::uart::port_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.spi_type_id {
            let method_id = crate::spi::spi_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.spi_bus_type_id {
            use crate::spi::{bit_order_name, BUS_W_BIT_ORDER, BUS_W_FREQUENCY, BUS_W_MODE};
            match name {
                "frequency" => {
                    let value = self.spi_bus_word(obj, BUS_W_FREQUENCY);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                "mode" => {
                    let value = self.spi_bus_word(obj, BUS_W_MODE);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                "bit_order" => {
                    let code = self.spi_bus_word(obj, BUS_W_BIT_ORDER);
                    return self.new_str(bit_order_name(code));
                }
                _ => {}
            }
            let method_id = crate::spi::spi_bus_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.i2c_type_id {
            let method_id = crate::i2c::i2c_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.i2c_bus_type_id {
            if name == "frequency" {
                let value = self.i2c_bus_word(obj, crate::i2c::BUS_W_FREQUENCY);
                return Value::fixnum(value as i32).ok_or(Trap::Overflow);
            }
            let method_id = crate::i2c::i2c_bus_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.adc_type_id {
            let method_id = crate::adc::adc_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.file_type_id {
            if crate::fileio::file_attribute(name) {
                return self.file_attribute_value(obj, name);
            }
            let method_id = crate::fileio::file_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.adc_channel_type_id {
            match name {
                "bits" => {
                    let value = self.adc_channel_word(obj, crate::adc::CH_W_BITS);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                "reference_uv" => {
                    let value = self.adc_channel_word(obj, crate::adc::CH_W_REFERENCE_UV);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                _ => {}
            }
            let method_id = crate::adc::adc_channel_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.busio_type_id {
            if name == "UART" {
                return self.uart_shim_factory_singleton(crate::uart::SHIM_FLAVOR_BUSIO);
            }
            if name == "SPI" {
                return self.spi_shim_factory(crate::shims::spi::SHIM_FLAVOR_BUSIO);
            }
            if name == "I2C" {
                return self.i2c_shim_factory(crate::shims::i2c::SHIM_FLAVOR_BUSIO);
            }
            return Err(Trap::AttributeError);
        }
        if type_id == self.uart_shim_type_id {
            let flavor = self.shim_word(obj, crate::uart::SHIM_W_FLAVOR);
            let port = self.shim_port(obj);
            match name {
                "in_waiting" if flavor == crate::uart::SHIM_FLAVOR_BUSIO => {
                    let (_instance, facts) = self.port_require_open(port)?;
                    let count = self.uart_rx_count(&facts);
                    return Value::fixnum(count as i32).ok_or(Trap::Overflow);
                }
                "baudrate" if flavor == crate::uart::SHIM_FLAVOR_BUSIO => {
                    let value = self.port_word(port, crate::uart::PORT_W_BAUDRATE);
                    return Value::fixnum(value as i32).ok_or(Trap::Overflow);
                }
                _ => {}
            }
            let method_id = crate::uart::uart_shim_method_id(flavor, name)
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.machine_type_id {
            if name == "UART" {
                return self.uart_shim_factory_singleton(crate::uart::SHIM_FLAVOR_MACHINE);
            }
            if name == "SPI" {
                return self.spi_shim_factory(crate::shims::spi::SHIM_FLAVOR_MACHINE);
            }
            if name == "I2C" {
                return self.i2c_shim_factory(crate::shims::i2c::SHIM_FLAVOR_MACHINE);
            }
            if name == "ADC" {
                return self.adc_shim_factory(crate::shims::adc::SHIM_FLAVOR_MACHINE);
            }
            if name == "Pin" {
                return self.pin_factory_singleton();
            }
            return Err(Trap::AttributeError);
        }
        if type_id == self.analogio_type_id {
            if name == "AnalogIn" {
                return self.adc_shim_factory(crate::shims::adc::SHIM_FLAVOR_ANALOGIO);
            }
            return Err(Trap::AttributeError);
        }
        if type_id == self.adc_shim_type_id {
            let flavor = self.adc_shim_flavor(obj);
            if flavor == crate::shims::adc::SHIM_FLAVOR_ANALOGIO {
                let channel = self.adc_shim_channel(obj);
                if name == "value" {
                    return self.call_adc_channel_method(channel, crate::adc::CH_READ_U16, &[]);
                }
                if name == "reference_voltage" {
                    let uv = self.adc_channel_word(channel, crate::adc::CH_W_REFERENCE_UV);
                    return self.new_float(f64::from(uv) / 1_000_000.0);
                }
            }
            let method_id = crate::shims::adc::adc_shim_method_id(flavor, name)
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.spi_shim_factory_type_id {
            return match name {
                "MSB" => Ok(Value::fixnum(0).unwrap()),
                "LSB" => Ok(Value::fixnum(1).unwrap()),
                _ => Err(Trap::AttributeError),
            };
        }
        if type_id == self.spi_shim_type_id {
            let flavor = self.spi_shim_flavor(obj);
            let method_id = crate::shims::spi::spi_shim_method_id(flavor, name)
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.i2c_shim_type_id {
            let flavor = self.i2c_shim_flavor(obj);
            let method_id = crate::shims::i2c::i2c_shim_method_id(flavor, name)
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.pin_factory_type_id {
            let mode = crate::gpio::machine_pin_const(name).ok_or(Trap::AttributeError)?;
            return Value::fixnum(mode as i32).ok_or(Trap::Overflow);
        }
        if type_id == self.digitalio_type_id {
            return match name {
                "DigitalInOut" => self.dio_factory_singleton(),
                "Direction" => self.direction_singleton(),
                _ => Err(Trap::AttributeError),
            };
        }
        if type_id == self.direction_type_id {
            let d = crate::gpio::direction_const(name).ok_or(Trap::AttributeError)?;
            return Value::fixnum(d as i32).ok_or(Trap::Overflow);
        }
        if type_id == self.dio_type_id {
            return match name {
                "value" => self.dio_read_value(obj),
                "direction" => {
                    let pin = self.dio_pin(obj);
                    let reference = pin.as_ref().ok_or(Trap::TypeError)?;
                    let mode = self.heap.read_u32(reference.0 + crate::gpio::PIN_W_MODE * 4);
                    Value::fixnum(mode as i32).ok_or(Trap::Overflow)
                }
                _ => Err(Trap::AttributeError),
            };
        }
        if type_id == self.class_type_id {
            if name == "__name__" {
                return Ok(self.read_slot(obj, 0));
            }
            if name == "__qualname__" {
                return Ok(self.read_slot(obj, 0));
            }
            if name == "__module__" {
                return match self.find_in_class(obj, "__module__") {
                    Some(module) => Ok(module),
                    None => self.new_str("builtins"),
                };
            }
            if name == "__dict__" {
                return Ok(self.read_slot(obj, 2));
            }
            if name == "__init__"
                && self.is_exception_class(obj)
                && self.find_in_class(obj, "__init__").is_none()
            {
                return self.new_bound_method(Value::NONE, EXC_INIT_UNBOUND);
            }
            if name == "__init__" && self.find_in_class(obj, "__init__").is_none() {
                return self.new_bound_method(Value::NONE, OBJECT_NOOP);
            }
            if name == "__init_subclass__" && self.find_in_class(obj, "__init_subclass__").is_none() {
                return self.new_bound_method(obj, OBJECT_NOOP);
            }
            if name == "__new__" && self.find_in_class(obj, "__new__").is_none() {
                return self.new_bound_method(Value::NONE, OBJECT_NEW);
            }
            let found = self.find_in_class(obj, name).ok_or(Trap::AttributeError)?;
            return self.bind_class_member(found, obj, obj);
        }
        if type_id == self.instance_type_id {
            return self.py_getattr_instance(obj, name);
        }
        if type_id == self.super_type_id {
            return self.py_getattr_super(obj, name);
        }
        if type_id == self.py_bound_type_id {
            let func = self.bound_func(obj);
            return match name {
                "__func__" => Ok(func),
                "__self__" => Ok(self.bound_self(obj)),
                "__doc__" | "__name__" | "__qualname__" => {
                    let mut inner = InlineCache::empty();
                    self.getattr(func, name, &mut inner)
                }
                _ => Err(Trap::AttributeError),
            };
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


    /// Installs the volatile MMIO seam (on device the runner passes `lamella_mmio::write32` /
    /// `read32`). The host leaves it unset and uses the simulated register file, so a driver runs
    /// and is verifiable off-device.
    pub fn set_mmio(&mut self, write: fn(u32, u32), read: fn(u32) -> u32) {
        self.mmio_write_fn = Some(write);
        self.mmio_read_fn = Some(read);
    }

    /// `_fs.listdir(path)` -- the names directly inside a directory, as a list of str.
    pub fn fs_listdir(&mut self, path: &str) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        match (ops.listdir)(path) {
            Ok(names) => {
                let mut entries = Vec::with_capacity(names.len());
                for name in names {
                    entries.push(self.new_str(&name)?);
                }
                self.new_list(entries)
            }
            Err(error) => Err(self.file_error(error, path)),
        }
    }

    /// `_fs.remove(path)` -- delete a file.
    pub fn fs_remove(&mut self, path: &str) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        match (ops.remove)(path) {
            Ok(()) => Ok(Value::NONE),
            Err(error) => Err(self.file_error(error, path)),
        }
    }

    /// `_fs.mkdir(path)` -- create a directory (its parent must exist).
    pub fn fs_mkdir(&mut self, path: &str) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        match (ops.mkdir)(path) {
            Ok(()) => Ok(Value::NONE),
            Err(error) => Err(self.file_error(error, path)),
        }
    }

    /// `_fs.rmdir(path)` -- remove an EMPTY directory.
    pub fn fs_rmdir(&mut self, path: &str) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        match (ops.rmdir)(path) {
            Ok(()) => Ok(Value::NONE),
            Err(error) => Err(self.file_error(error, path)),
        }
    }

    /// `_fs.rename(src, dst)`.
    pub fn fs_rename(&mut self, from: &str, to: &str) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        match (ops.rename)(from, to) {
            Ok(()) => Ok(Value::NONE),
            Err(error) => Err(self.file_error(error, from)),
        }
    }

    /// `_fs.kind(path)` -- `(is_directory, size)`. The one call `os.path`'s predicates are built on:
    /// asking what a path IS answers exists / isfile / isdir / getsize at once, and raising for a
    /// path that is not there is what lets `exists()` be written as a caught refusal.
    pub fn fs_kind(&mut self, path: &str) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        match (ops.kind)(path) {
            Ok(crate::fileio::PathKind::Directory) => {
                let size = Value::fixnum(0).ok_or(Trap::Overflow)?;
                self.new_tuple(alloc::vec![Value::TRUE, size])
            }
            Ok(crate::fileio::PathKind::File(size)) => {
                let size = Value::fixnum(size as i32).ok_or(Trap::Overflow)?;
                self.new_tuple(alloc::vec![Value::FALSE, size])
            }
            Err(error) => Err(self.file_error(error, path)),
        }
    }

    /// Installs the host filesystem. A host program passes its own; a device passes its storage
    /// driver; firmware with no storage passes nothing, and every file verb then refuses by name.
    pub fn set_file_ops(&mut self, ops: crate::fileio::FileOps) {
        self.file_ops = Some(ops);
    }

    /// The installed filesystem, or the refusal for a runtime that has none.
    fn require_file_ops(&mut self) -> Result<crate::fileio::FileOps, Trap> {
        match self.file_ops {
            Some(ops) => Ok(ops),
            None => {
                let message = "no filesystem is available in this runtime";
                Err(self.raise_named_exception("OSError", message))
            }
        }
    }

    /// The exception a failed host operation raises: CPython's `[Errno n] text: 'path'`, under
    /// CPython's class for that errno -- so a program catching `FileNotFoundError` catches ours.
    fn file_error(&mut self, error: crate::fileio::FileError, path: &str) -> Trap {
        let (number, text) = error.errno();
        let message = alloc::format!("[Errno {number}] {text}: '{path}'");
        self.raise_named_exception(error.exception(), &message)
    }

    /// `open(path, mode)`: opens the host file and returns the object that reads or writes it.
    pub fn open_file(&mut self, path: &str, mode: crate::fileio::FileMode) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        let handle = match (ops.open)(path, mode) {
            Ok(handle) => handle,
            Err(error) => return Err(self.file_error(error, path)),
        };
        let name = self.new_str(path)?;
        let reference = self.alloc_object(self.file_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, handle);
        self.heap.write_u32(reference.0 + 4, pack_file_mode(mode));
        self.heap.write_u32(reference.0 + 8, name.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is an open-or-closed file object.
    #[must_use]
    pub fn is_file(&self, value: Value) -> bool {
        self.is_type(value, self.file_type_id)
    }

    /// A file's host handle, or `None` once it has been closed.
    fn file_handle(&self, file: Value) -> Option<u32> {
        let reference = file.as_ref()?;
        let raw = self.heap.read_u32(reference.0);
        (raw != u32::MAX).then_some(raw)
    }

    /// A file's mode.
    fn file_mode(&self, file: Value) -> crate::fileio::FileMode {
        let raw = file.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + 4));
        unpack_file_mode(raw)
    }

    /// A file's `name` -- the path as it was given to `open`.
    fn file_name_value(&self, file: Value) -> Value {
        file.as_ref().map_or(Value::NONE, |r| Value::from_bits(self.heap.read_u32(r.0 + 8)))
    }

    /// The path a file was opened with, for an error message.
    fn file_path(&self, file: Value) -> String {
        self.str_value(self.file_name_value(file)).map(String::from).unwrap_or_default()
    }

    /// `repr(file)` -- CPython's, including the wrapper class it would be an instance of, so a
    /// printed file is recognizable to someone who knows CPython's.
    pub(crate) fn file_repr(&self, file: Value) -> String {
        let mode = self.file_mode(file);
        let path = self.file_path(file);
        if self.file_handle(file).is_none() {
            return alloc::format!("<_io.{} name='{path}' mode='{}' [closed]>",
                file_wrapper_name(mode), mode.as_str());
        }
        if mode.binary {
            alloc::format!("<_io.{} name='{path}' mode='{}'>", file_wrapper_name(mode), mode.as_str())
        } else {
            alloc::format!(
                "<_io.{} name='{path}' mode='{}' encoding='utf-8'>",
                file_wrapper_name(mode),
                mode.as_str()
            )
        }
    }

    /// Reads up to `limit` bytes (all of it when `None`), through the seam in chunks.
    fn file_read_bytes(&mut self, file: Value, limit: Option<usize>) -> Result<Vec<u8>, Trap> {
        let ops = self.require_file_ops()?;
        let handle = match self.file_handle(file) {
            Some(handle) => handle,
            None => return Err(crate::fileio::closed_file_error(self)),
        };
        if !self.file_mode(file).read {
            let message = "not readable";
            return Err(self.raise_named_exception("OSError", message));
        }
        let mut out: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let want = match limit {
                Some(limit) => {
                    if out.len() >= limit {
                        break;
                    }
                    core::cmp::min(limit - out.len(), chunk.len())
                }
                None => chunk.len(),
            };
            match (ops.read)(handle, &mut chunk[..want]) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(error) => {
                    let path = self.file_path(file);
                    return Err(self.file_error(error, &path));
                }
            }
        }
        Ok(out)
    }

    /// Turns bytes just read into what the mode says they are: `bytes`, or a `str` with newlines
    /// translated. Invalid UTF-8 in text mode is a UnicodeDecodeError rather than a replacement
    /// character, because silently substituting one changes data.
    fn file_decode(&mut self, file: Value, data: Vec<u8>) -> Result<Value, Trap> {
        if self.file_mode(file).binary {
            return self.new_bytes(data);
        }
        let translated = crate::fileio::translate_newlines(&data);
        match core::str::from_utf8(&translated) {
            Ok(text) => self.new_str(text),
            Err(e) => {
                let at = e.valid_up_to();
                let message = alloc::format!(
                    "'utf-8' codec can't decode byte 0x{:02x} in position {at}: invalid start byte",
                    translated.get(at).copied().unwrap_or(0)
                );
                Err(self.raise_named_exception("ValueError", &message))
            }
        }
    }

    /// Reads one line, INCLUDING its newline; an empty result means end of file.
    fn file_read_line(&mut self, file: Value, limit: Option<usize>) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        let handle = match self.file_handle(file) {
            Some(handle) => handle,
            None => return Err(crate::fileio::closed_file_error(self)),
        };
        let mut out: Vec<u8> = Vec::new();
        let mut one = [0u8; 1];
        loop {
            if limit.is_some_and(|limit| out.len() >= limit) {
                break;
            }
            match (ops.read)(handle, &mut one) {
                Ok(0) => break,
                Ok(_) => {
                    out.push(one[0]);
                    if one[0] == b'\n' {
                        break;
                    }
                }
                Err(error) => {
                    let path = self.file_path(file);
                    return Err(self.file_error(error, &path));
                }
            }
        }
        self.file_decode(file, out)
    }

    /// Writes `value` (a str in text mode, bytes-like in binary), returning the count CPython
    /// returns: characters for text, bytes for binary.
    fn file_write_value(&mut self, file: Value, value: Value) -> Result<Value, Trap> {
        let ops = self.require_file_ops()?;
        let handle = match self.file_handle(file) {
            Some(handle) => handle,
            None => return Err(crate::fileio::closed_file_error(self)),
        };
        let mode = self.file_mode(file);
        if !mode.write {
            let message = "not writable";
            return Err(self.raise_named_exception("OSError", message));
        }
        let (data, reported) = if mode.binary {
            let bytes = match self.bytes_value(value) {
                Some(bytes) => bytes.to_vec(),
                None => {
                    let kind = self.type_name_of(value);
                    let message =
                        alloc::format!("a bytes-like object is required, not '{kind}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            };
            let count = bytes.len();
            (bytes, count)
        } else {
            let text = match self.str_value(value) {
                Some(text) => String::from(text),
                None => {
                    let kind = self.type_name_of(value);
                    let message = alloc::format!("write() argument must be str, not {kind}");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            };
            let count = text.chars().count();
            (text.into_bytes(), count)
        };
        let mut written = 0;
        while written < data.len() {
            match (ops.write)(handle, &data[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(error) => {
                    let path = self.file_path(file);
                    return Err(self.file_error(error, &path));
                }
            }
        }
        Value::fixnum(reported as i32).ok_or(Trap::Overflow)
    }

    /// Dispatches a file method. The context-manager pair and the iterator protocol are here too:
    /// `with open(...)` and `for line in f` are how a file is used, not extras.
    pub(crate) fn call_file_method(
        &mut self,
        file: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::fileio::*;
        match method_id {
            FILE_READ => {
                let limit = match args {
                    [] => None,
                    [n] if n.is_none() => None,
                    [n] => {
                        let want = n.as_int().ok_or(Trap::TypeError)?;
                        if want < 0 {
                            None
                        } else {
                            Some(want as usize)
                        }
                    }
                    _ => return Err(Trap::TypeError),
                };
                let data = self.file_read_bytes(file, limit)?;
                self.file_decode(file, data)
            }
            FILE_READLINE => {
                let limit = match args {
                    [] => None,
                    [n] => {
                        let want = n.as_int().ok_or(Trap::TypeError)?;
                        (want >= 0).then_some(want as usize)
                    }
                    _ => return Err(Trap::TypeError),
                };
                self.file_read_line(file, limit)
            }
            FILE_READLINES => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let mut lines: Vec<Value> = Vec::new();
                loop {
                    let line = self.file_read_line(file, None)?;
                    if self.file_line_is_empty(line) {
                        break;
                    }
                    lines.push(line);
                }
                self.new_list(lines)
            }
            FILE_WRITE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                self.file_write_value(file, *value)
            }
            FILE_WRITELINES => {
                let [values] = args else {
                    return Err(Trap::TypeError);
                };
                let items = self.seq_value(*values).cloned().ok_or(Trap::TypeError)?;
                for item in items {
                    self.file_write_value(file, item)?;
                }
                Ok(Value::NONE)
            }
            FILE_CLOSE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                if let Some(handle) = self.file_handle(file) {
                    let ops = self.require_file_ops()?;
                    let result = (ops.close)(handle);
                    if let Some(reference) = file.as_ref() {
                        self.heap.write_u32(reference.0, u32::MAX);
                    }
                    if let Err(error) = result {
                        let path = self.file_path(file);
                        return Err(self.file_error(error, &path));
                    }
                }
                Ok(Value::NONE)
            }
            FILE_FLUSH => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let ops = self.require_file_ops()?;
                match self.file_handle(file) {
                    Some(handle) => match (ops.flush)(handle) {
                        Ok(()) => Ok(Value::NONE),
                        Err(error) => {
                            let path = self.file_path(file);
                            Err(self.file_error(error, &path))
                        }
                    },
                    None => Err(crate::fileio::closed_file_error(self)),
                }
            }
            FILE_SEEK => {
                let (offset, whence) = match args {
                    [offset] => (offset.as_int().ok_or(Trap::TypeError)?, 0),
                    [offset, whence] => (
                        offset.as_int().ok_or(Trap::TypeError)?,
                        whence.as_int().ok_or(Trap::TypeError)?,
                    ),
                    _ => return Err(Trap::TypeError),
                };
                if !(0..=2).contains(&whence) {
                    let message = alloc::format!("invalid whence ({whence}, should be 0, 1 or 2)");
                    return Err(self.raise_named_exception("ValueError", &message));
                }
                let ops = self.require_file_ops()?;
                match self.file_handle(file) {
                    Some(handle) => match (ops.seek)(handle, offset, whence as u8) {
                        Ok(position) => Value::fixnum(position as i32).ok_or(Trap::Overflow),
                        Err(error) => {
                            let path = self.file_path(file);
                            Err(self.file_error(error, &path))
                        }
                    },
                    None => Err(crate::fileio::closed_file_error(self)),
                }
            }
            FILE_TELL => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let ops = self.require_file_ops()?;
                match self.file_handle(file) {
                    Some(handle) => match (ops.tell)(handle) {
                        Ok(position) => Value::fixnum(position as i32).ok_or(Trap::Overflow),
                        Err(error) => {
                            let path = self.file_path(file);
                            Err(self.file_error(error, &path))
                        }
                    },
                    None => Err(crate::fileio::closed_file_error(self)),
                }
            }
            FILE_TRUNCATE => {
                let message = "truncate() is not supported by this runtime";
                Err(self.raise_named_exception("OSError", message))
            }
            FILE_ENTER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                Ok(file)
            }
            FILE_EXIT => {
                self.call_file_method(file, FILE_CLOSE, &[])?;
                Ok(Value::FALSE)
            }
            FILE_ITER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                Ok(file)
            }
            FILE_NEXT => {
                let line = self.file_read_line(file, None)?;
                if self.file_line_is_empty(line) {
                    return Err(self.raise_named_exception("StopIteration", ""));
                }
                Ok(line)
            }
            FILE_READABLE => Ok(Value::from_bool(self.file_mode(file).read)),
            FILE_WRITABLE => Ok(Value::from_bool(self.file_mode(file).write)),
            FILE_SEEKABLE => Ok(Value::TRUE),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Whether a line just read is the empty one that means end of file -- in either mode, since a
    /// text read yields a `str` and a binary read `bytes`.
    fn file_line_is_empty(&self, line: Value) -> bool {
        if let Some(text) = self.str_value(line) {
            return text.is_empty();
        }
        self.bytes_value(line).is_none_or(<[u8]>::is_empty)
    }

    /// A file's read-only attributes: the path it was opened with, its mode string, and whether it
    /// has been closed.
    fn file_attribute_value(&mut self, file: Value, name: &str) -> Result<Value, Trap> {
        match name {
            "name" => Ok(self.file_name_value(file)),
            "mode" => {
                let mode = self.file_mode(file);
                self.new_str(mode.as_str())
            }
            "closed" => Ok(Value::from_bool(self.file_handle(file).is_none())),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Installs the host's clocks and sleep: a wall clock in nanoseconds since the Unix epoch, a
    /// monotonic source in nanoseconds from any origin, and a blocking sleep. A host program passes
    /// its own; a device passes its timer.
    ///
    /// This crate cannot read a clock itself -- it has no platform underneath it -- so leaving these
    /// unset means `time` REPORTS that it has no clock rather than answering with a zero that a
    /// caller would have no way to recognize as fabricated.
    pub fn set_clock(&mut self, clock: fn() -> i64, monotonic: fn() -> i64, sleep: fn(i64)) {
        self.clock_fn = Some(clock);
        self.monotonic_fn = Some(monotonic);
        self.sleep_fn = Some(sleep);
    }

    /// The wall clock, or a loud error naming what is missing.
    pub(crate) fn now_ns(&mut self) -> Result<i64, Trap> {
        match self.clock_fn {
            Some(clock) => Ok(clock()),
            None => Err(self.no_clock()),
        }
    }

    /// The monotonic clock, or a loud error.
    pub(crate) fn monotonic_ns(&mut self) -> Result<i64, Trap> {
        match self.monotonic_fn {
            Some(clock) => Ok(clock()),
            None => Err(self.no_clock()),
        }
    }

    /// Blocks for `nanos`, or a loud error. A non-positive count returns at once, as a sleep of no
    /// time should.
    pub(crate) fn sleep_ns(&mut self, nanos: i64) -> Result<(), Trap> {
        match self.sleep_fn {
            Some(sleep) => {
                if nanos > 0 {
                    sleep(nanos);
                }
                Ok(())
            }
            None => Err(self.no_clock()),
        }
    }

    fn no_clock(&mut self) -> Trap {
        let message = "this runtime has no clock installed, so it cannot tell the time";
        self.raise_named_exception("OSError", message)
    }

    /// Selects the target board whose register map the gpio layer drives (`board.LED` resolution + the
    /// drive/direction/clock registers). Set once by the deployment before running.
    /// Orthogonal to the MMIO seam (which is host-sim vs on-device).
    pub fn set_board(&mut self, board: crate::gpio::Board) {
        self.board = board;
    }

    /// A volatile 32-bit register write: through the installed seam on device, else into the host
    /// simulated register file (and the ordered write trace). The sim additionally MODELS the
    /// selected board's UART FIFO: a write to it captures the TX byte (the byte in bits 7..0).
    pub fn mmio_write(&mut self, address: u32, value: u32) {
        if let Some(write) = self.mmio_write_fn {
            write(address, value);
            return;
        }
        #[cfg(not(target_os = "none"))]
        {
            if let Some(facts) = self.board.uart_facts(0, self.resolved_uart_facts.as_ref()) {
                if address == facts.fifo {
                    self.uart_sim_tx.push((value & 0xFF) as u8);
                }
            }
            self.spi_sim_write(address, value);
            self.i2c_sim_write(address, value);
            self.adc_sim_write(address, value);
            if let Some((clear_alias, _done)) = self.board.reset_regs() {
                if address == clear_alias {
                    self.reset_done_bits |= value;
                }
            }
            self.mmio_sim.insert(address, value);
            self.mmio_trace.push((address, value));
        }
    }

    /// A volatile 32-bit register read: through the installed seam on device, else from the host
    /// simulated register file (0 for a register never written). The sim MODELS the selected
    /// board's UART where a plain last-value file cannot: the status register's RX count reflects
    /// the injected bytes (TX always reads drained), a FIFO read POPS one RX byte, and the
    /// config-latch register self-clears instantly -- so the driver's real poll loops terminate.
    pub fn mmio_read(&mut self, address: u32) -> u32 {
        if let Some(read) = self.mmio_read_fn {
            return read(address);
        }
        #[cfg(not(target_os = "none"))]
        {
            if let Some((_clear_alias, done)) = self.board.reset_regs() {
                if address == done {
                    return self.reset_done_bits;
                }
            }
            if let Some(facts) = self.board.uart_facts(0, self.resolved_uart_facts.as_ref()) {
                match facts.status {
                    crate::uart::UartStatus::Counts { status, rx_shift, rx_mask, tx_shift, tx_mask }
                        if address == status =>
                    {
                        let rx = (self.uart_sim_rx.len() as u32).min(facts.fifo_depth);
                        let tx = 0u32;
                        return ((rx & rx_mask) << rx_shift) | ((tx & tx_mask) << tx_shift);
                    }
                    crate::uart::UartStatus::Flags { flags, rx_empty_mask } if address == flags => {
                        return if self.uart_sim_rx.is_empty() { rx_empty_mask } else { 0 };
                    }
                    crate::uart::UartStatus::FlagsReady { flags, tx_ready_mask, rx_ready_mask }
                        if address == flags =>
                    {
                        let rx = if self.uart_sim_rx.is_empty() { 0 } else { rx_ready_mask };
                        return tx_ready_mask | rx;
                    }
                    _ => {}
                }
                if address == facts.fifo {
                    return u32::from(self.uart_sim_rx.pop_front().unwrap_or(0));
                }
                if facts.self_clear_reg != 0 && address == facts.self_clear_reg {
                    return 0;
                }
                for &(reg, value) in facts.sim_ready {
                    if address == reg {
                        return value;
                    }
                }
            }
            if let Some(value) = self.spi_sim_read(address) {
                return value;
            }
            if let Some(value) = self.i2c_sim_read(address) {
                return value;
            }
            if let Some(value) = self.adc_sim_read(address) {
                return value;
            }
            self.mmio_sim.get(&address).copied().unwrap_or(0)
        }
        #[cfg(target_os = "none")]
        0
    }

    /// Queues RX bytes into the host UART sim (the test-side peer writing to us).
    #[cfg(not(target_os = "none"))]
    pub fn uart_sim_inject_rx(&mut self, data: &[u8]) {
        self.uart_sim_rx.extend(data.iter().copied());
    }

    /// The bytes the program has transmitted through the host UART sim -- the TX oracle.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn uart_sim_tx(&self) -> &[u8] {
        &self.uart_sim_tx
    }


    /// The `uart` module singleton. Bind it as the global `uart` so a program reaches
    /// `uart.open(...)`.
    pub fn uart_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.uart_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `uart` singleton.
    #[must_use]
    pub fn is_uart(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.uart_type_id)
    }

    /// Whether `value` is an open-or-closed `Port`.
    #[must_use]
    pub fn is_uart_port(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.uart_port_type_id)
    }

    /// Allocates a board UART resource handle (`board.UART0`) carrying its instance number.
    fn new_uart_resource(&mut self, instance: u32) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.uart_resource_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, instance);
        Ok(Value::from_ref(reference))
    }

    /// The instance number if `value` is a board UART resource.
    fn uart_resource_instance(&self, value: Value) -> Option<u32> {
        value
            .as_ref()
            .filter(|r| self.heap.type_id_of(*r) == self.uart_resource_type_id)
            .map(|r| self.heap.read_u32(r.0))
    }

    /// Allocates an OPEN `Port` over `instance` with the validated `config` echoed into its
    /// read-only property words.
    fn new_port(&mut self, instance: u32, config: &crate::uart::UartConfig) -> Result<Value, Trap> {
        use crate::uart::*;
        let reference = self.alloc_object(self.uart_port_type_id).ok_or(Trap::OutOfMemory)?;
        let base = reference.0;
        self.heap.write_u32(base + PORT_W_INSTANCE * 4, instance);
        self.heap.write_u32(base + PORT_W_OPEN * 4, 1);
        self.heap.write_u32(base + PORT_W_BAUDRATE * 4, config.baudrate);
        self.heap.write_u32(base + PORT_W_DATA_BITS * 4, config.data_bits);
        self.heap.write_u32(base + PORT_W_PARITY * 4, config.parity);
        self.heap.write_u32(base + PORT_W_STOP_BITS * 4, config.stop_bits);
        Ok(Value::from_ref(reference))
    }

    /// One raw word of a `Port`'s payload.
    pub(crate) fn port_word(&self, port: Value, word: u32) -> u32 {
        port.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + word * 4))
    }

    fn port_set_word(&mut self, port: Value, word: u32, value: u32) {
        if let Some(r) = port.as_ref() {
            self.heap.write_u32(r.0 + word * 4, value);
        }
    }

    /// Replays one driver-sequence step over the MMIO seam. The polls are the driver's REAL
    /// loops: on device they spin on the hardware bit; on the host sim the modeled registers
    /// (self-clearing latch, drained TX count) satisfy them immediately.
    fn apply_uart_op(&mut self, op: crate::uart::UartOp) {
        use crate::uart::UartOp;
        match op {
            UartOp::Write { reg, value } => self.mmio_write(reg, value),
            UartOp::PollEq { reg, mask, want } => while self.mmio_read(reg) & mask != want {},
            UartOp::PollBelow { reg, mask, below } => while self.mmio_read(reg) & mask >= below {},
        }
    }

    /// The bytes known to be immediately readable: the exact count on a counted-status chip,
    /// 0 or 1 on a flag-style chip (all the silicon can say there) -- `any()`'s contract.
    fn uart_rx_count(&mut self, facts: &crate::uart::UartFacts) -> u32 {
        match facts.status {
            crate::uart::UartStatus::Counts { status, rx_shift, rx_mask, .. } => {
                (self.mmio_read(status) >> rx_shift) & rx_mask
            }
            crate::uart::UartStatus::Flags { flags, rx_empty_mask } => {
                u32::from(self.mmio_read(flags) & rx_empty_mask == 0)
            }
            crate::uart::UartStatus::FlagsReady { flags, rx_ready_mask, .. } => {
                u32::from(self.mmio_read(flags) & rx_ready_mask != 0)
            }
        }
    }

    /// Parses a `timeout_ms` argument: `None` = block, `0` = never block, `n` = a deadline of
    /// `n` integer milliseconds (times are integer ms so the surface is identical on a no-float
    /// profile; the tri-state is CPython's settimeout semantics in that unit).
    fn parse_timeout_ms(&mut self, value: Value) -> Result<crate::uart::UartTimeout, Trap> {
        use crate::uart::UartTimeout;
        if value.is_none() {
            return Ok(UartTimeout::Blocking);
        }
        let Some(ms) = value.as_int() else {
            return Err(self.with_message(Trap::TypeError, "timeout_ms must be an int or None"));
        };
        if ms < 0 {
            return Err(self.with_message(Trap::ValueError, "timeout_ms must be >= 0"));
        }
        if ms == 0 {
            return Ok(UartTimeout::Poll);
        }
        Ok(UartTimeout::DeadlineMs(u32::try_from(ms).unwrap_or(u32::MAX)))
    }

    /// Waits (per `timeout`) until at least one RX byte is buffered; `false` = timed out with
    /// none. On the host sim there is no concurrent producer, so a finite deadline with no data
    /// is an immediate expiry (observably equivalent), and a would-block-forever read fails
    /// LOUD rather than hanging the harness. On device, blocking spins the status register; a
    /// finite deadline needs the tick seam the threading arc brings, so until then it is
    /// rejected loudly there.
    fn uart_wait_rx(
        &mut self,
        facts: &crate::uart::UartFacts,
        timeout: crate::uart::UartTimeout,
    ) -> Result<bool, Trap> {
        use crate::uart::UartTimeout;
        if self.uart_rx_count(facts) > 0 {
            return Ok(true);
        }
        let on_device = self.mmio_read_fn.is_some();
        match timeout {
            UartTimeout::Poll => Ok(false),
            UartTimeout::Blocking => {
                if !on_device {
                    let message = "uart read would block forever (no rx data on the host sim)";
                    return Err(self.raise_named_exception("RuntimeError", message));
                }
                loop {
                    if self.uart_rx_count(facts) > 0 {
                        return Ok(true);
                    }
                }
            }
            UartTimeout::DeadlineMs(ms) => {
                if !on_device {
                    return Ok(false);
                }
                let message = alloc::format!(
                    "a finite timeout_ms ({ms} ms) needs a tick source (not wired on-device yet); use timeout_ms=None or 0"
                );
                Err(self.with_message(Trap::Unsupported, &message))
            }
        }
    }

    /// Pops up to `max` more RX bytes (all buffered when `None`) into `out`.
    fn uart_pop_available(
        &mut self,
        facts: &crate::uart::UartFacts,
        max: Option<usize>,
        out: &mut alloc::vec::Vec<u8>,
    ) {
        let mut taken = 0usize;
        loop {
            if let Some(m) = max {
                if taken >= m {
                    return;
                }
            }
            if self.uart_rx_count(facts) == 0 {
                return;
            }
            out.push((self.mmio_read(facts.fifo) & 0xFF) as u8);
            taken += 1;
        }
    }

    /// The instance + facts of an OPEN port (`ValueError` after `close`, CPython's file text).
    fn port_require_open(&mut self, port: Value) -> Result<(u32, crate::uart::UartFacts), Trap> {
        use crate::uart::{PORT_W_INSTANCE, PORT_W_OPEN};
        if self.port_word(port, PORT_W_OPEN) == 0 {
            return Err(self.with_message(Trap::ValueError, "I/O operation on closed port"));
        }
        let instance = self.port_word(port, PORT_W_INSTANCE);
        let facts = self
            .board
            .uart_facts(instance, self.resolved_uart_facts.as_ref())
            .ok_or(Trap::Malformed)?;
        Ok((instance, facts))
    }

    /// `uart.open(resource, **config)`: validates the line config, CLAIMS the instance (one
    /// owner per port; a second open raises `OSError: UART0 in use`), replays the board's
    /// bring-up sequence, and returns the `Port`.
    fn uart_open(&mut self, posargs: &[Value], kwargs: &[(&str, Value)]) -> Result<Value, Trap> {
        use crate::uart::{parity_code, UartConfig};
        let [resource] = posargs else {
            let message = "open() takes exactly one positional argument (the board UART resource)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let instance = if let Some(role) =
            self.str_value(*resource).map(alloc::string::ToString::to_string)
        {
            match self.board {
                crate::gpio::Board::Samd21Xpro => {
                    let facts = crate::board_binding::samd21_uart_facts(self, &role)?;
                    self.resolved_uart_facts = Some(facts);
                    0
                }
                _ => {
                    let message =
                        "this board's uart does not take a role handle yet -- name its port \
                         (e.g. board.UART0)";
                    return Err(self.with_message(Trap::Unsupported, message));
                }
            }
        } else if let Some(instance) = self.uart_resource_instance(*resource) {
            instance
        } else {
            let message = "open() expects a board UART role handle (e.g. board.VCP) or port \
                           (e.g. board.UART0)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let mut config = UartConfig::default();
        for &(name, value) in kwargs {
            match name {
                "baudrate" => {
                    let baud = value.as_int().unwrap_or(0);
                    if baud <= 0 || baud > i64::from(u32::MAX) {
                        let message = "baudrate must be a positive integer";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                    config.baudrate = baud as u32;
                }
                "data_bits" => match value.as_int() {
                    Some(bits @ 5..=8) => config.data_bits = bits as u32,
                    Some(9) => {
                        let message = "data_bits=9 is not supported";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                    _ => {
                        let message = "data_bits must be 5, 6, 7 or 8";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                },
                "parity" => {
                    let Some(code) = self.str_value(value).and_then(parity_code) else {
                        let message = "parity must be 'none', 'even' or 'odd'";
                        return Err(self.with_message(Trap::ValueError, message));
                    };
                    config.parity = code;
                }
                "stop_bits" => match value.as_int() {
                    Some(stop @ 1..=2) => config.stop_bits = stop as u32,
                    _ => {
                        let message = "stop_bits must be 1 or 2";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                },
                other => {
                    let message =
                        alloc::format!("open() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        if self.uart_claimed.contains(&instance) {
            let message = alloc::format!("UART{instance} in use");
            return Err(self.raise_named_exception("OSError", &message));
        }
        let ops = match self
            .board
            .uart_open_ops(instance, &config, self.resolved_uart_facts.as_ref())
        {
            None => {
                let message = "uart is not supported on this board yet";
                return Err(self.with_message(Trap::Unsupported, message));
            }
            Some(Err(crate::uart::UartConfigError::BaudOutOfRange)) => {
                let message = "baudrate out of range for this uart";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Err(crate::uart::UartConfigError::ParityNotTabled)) => {
                let message = "this uart does not support the requested data bits / parity / stop bits";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Ok(ops)) => ops,
        };
        self.uart_claimed.push(instance);
        for op in ops {
            self.apply_uart_op(op);
        }
        self.new_port(instance, &config)
    }

    /// Dispatches a `uart` module method, positional form.
    pub(crate) fn call_uart_method(
        &mut self,
        _uart: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        match method_id {
            crate::uart::UART_OPEN => self.uart_open(args, &[]),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a keyword call on the `uart` module or a `Port` (the `Op::CallKw` path):
    /// `uart.open(baudrate=...)`, `port.read(timeout_ms=...)`.
    pub(crate) fn call_uart_bound_kw(
        &mut self,
        receiver: Value,
        method_id: u32,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        use crate::uart::*;
        if self.is_uart(receiver) {
            return match method_id {
                UART_OPEN => self.uart_open(posargs, kwargs),
                _ => Err(Trap::AttributeError),
            };
        }
        let mut timeout_value = Value::NONE;
        for &(name, value) in kwargs {
            let reads = matches!(
                method_id,
                PORT_READ | PORT_READ_EXACTLY | PORT_READINTO | PORT_READLINE
            );
            if name == "timeout_ms" && reads {
                timeout_value = value;
            } else {
                let message =
                    alloc::format!("this method got an unexpected keyword argument '{name}'");
                return Err(self.raise_named_exception("TypeError", &message));
            }
        }
        self.port_dispatch(receiver, method_id, posargs, timeout_value)
    }

    /// Dispatches a `Port` method, positional form (`timeout_ms` defaults to blocking).
    pub(crate) fn call_port_method(
        &mut self,
        port: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        self.port_dispatch(port, method_id, args, Value::NONE)
    }

    /// The one `Port` dispatch both call forms funnel through.
    fn port_dispatch(
        &mut self,
        port: Value,
        method_id: u32,
        args: &[Value],
        timeout_value: Value,
    ) -> Result<Value, Trap> {
        use crate::uart::*;
        match method_id {
            PORT_ANY => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let (_instance, facts) = self.port_require_open(port)?;
                let count = self.uart_rx_count(&facts);
                Value::fixnum(count as i32).ok_or(Trap::Overflow)
            }
            PORT_READ => {
                let max = match args {
                    [] => None,
                    [n] if n.is_none() => None,
                    [n] => match n.as_int() {
                        Some(n) if n >= 0 => Some(n as usize),
                        _ => {
                            let message = "read size must be an int >= 0 or None";
                            return Err(self.with_message(Trap::ValueError, message));
                        }
                    },
                    _ => return Err(Trap::TypeError),
                };
                let timeout = self.parse_timeout_ms(timeout_value)?;
                let (_instance, facts) = self.port_require_open(port)?;
                let mut out = alloc::vec::Vec::new();
                if max != Some(0) && self.uart_wait_rx(&facts, timeout)? {
                    self.uart_pop_available(&facts, max, &mut out);
                }
                self.new_bytes(out)
            }
            PORT_READ_EXACTLY => {
                let [n] = args else {
                    return Err(Trap::TypeError);
                };
                let Some(n) = n.as_int().filter(|&n| n >= 0) else {
                    let message = "read_exactly size must be an int >= 0";
                    return Err(self.with_message(Trap::ValueError, message));
                };
                let n = n as usize;
                let timeout = self.parse_timeout_ms(timeout_value)?;
                let (_instance, facts) = self.port_require_open(port)?;
                let mut out = alloc::vec::Vec::with_capacity(n);
                while out.len() < n {
                    if !self.uart_wait_rx(&facts, timeout)? {
                        let message =
                            alloc::format!("read_exactly: got {} of {} bytes", out.len(), n);
                        return Err(self.raise_named_exception("TimeoutError", &message));
                    }
                    let remaining = n - out.len();
                    self.uart_pop_available(&facts, Some(remaining), &mut out);
                }
                self.new_bytes(out)
            }
            PORT_READINTO => {
                let [buf] = args else {
                    return Err(Trap::TypeError);
                };
                if !self.is_bytearray(*buf) {
                    let message = "readinto() argument must be a bytearray";
                    return Err(self.with_message(Trap::TypeError, message));
                }
                let capacity = self.bytes_value(*buf).map_or(0, <[u8]>::len);
                let timeout = self.parse_timeout_ms(timeout_value)?;
                let (_instance, facts) = self.port_require_open(port)?;
                let mut out = alloc::vec::Vec::new();
                if capacity > 0 && self.uart_wait_rx(&facts, timeout)? {
                    self.uart_pop_available(&facts, Some(capacity), &mut out);
                }
                if let Some(slot) = self.byte_buffer_slot(*buf) {
                    for (at, &byte) in out.iter().enumerate() {
                        self.byte_buffers[slot][at] = byte;
                    }
                }
                Value::fixnum(out.len() as i32).ok_or(Trap::Overflow)
            }
            PORT_READLINE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let timeout = self.parse_timeout_ms(timeout_value)?;
                let (_instance, facts) = self.port_require_open(port)?;
                let mut out = alloc::vec::Vec::new();
                loop {
                    if !self.uart_wait_rx(&facts, timeout)? {
                        break;
                    }
                    let byte = (self.mmio_read(facts.fifo) & 0xFF) as u8;
                    out.push(byte);
                    if byte == b'\n' {
                        break;
                    }
                }
                self.new_bytes(out)
            }
            PORT_WRITE => {
                let [data] = args else {
                    return Err(Trap::TypeError);
                };
                let Some(data) = self.bytes_value(*data).map(<[u8]>::to_vec) else {
                    let message = alloc::format!(
                        "a bytes-like object is required, not '{}'",
                        self.type_name_of(*data)
                    );
                    return Err(self.raise_named_exception("TypeError", &message));
                };
                let (instance, _facts) = self.port_require_open(port)?;
                let board = self.board;
                let resolved = self.resolved_uart_facts;
                for &byte in &data {
                    for op in board.uart_tx_byte_ops(instance, byte, resolved.as_ref()) {
                        self.apply_uart_op(op);
                    }
                }
                Value::fixnum(data.len() as i32).ok_or(Trap::Overflow)
            }
            PORT_FLUSH => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let (instance, _facts) = self.port_require_open(port)?;
                let board = self.board;
                let resolved = self.resolved_uart_facts;
                for op in board.uart_flush_ops(instance, resolved.as_ref()) {
                    self.apply_uart_op(op);
                }
                Ok(Value::NONE)
            }
            PORT_DISCARD_INPUT => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let (_instance, facts) = self.port_require_open(port)?;
                while self.uart_rx_count(&facts) > 0 {
                    self.mmio_read(facts.fifo);
                }
                Ok(Value::NONE)
            }
            PORT_CLOSE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.port_close(port);
                Ok(Value::NONE)
            }
            PORT_ENTER => Ok(port),
            PORT_EXIT => {
                self.port_close(port);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Closes a port: releases the instance claim and marks it closed (idempotent, like a
    /// CPython file's close).
    fn port_close(&mut self, port: Value) {
        use crate::uart::{PORT_W_INSTANCE, PORT_W_OPEN};
        if self.port_word(port, PORT_W_OPEN) == 0 {
            return;
        }
        let instance = self.port_word(port, PORT_W_INSTANCE);
        self.uart_claimed.retain(|&claimed| claimed != instance);
        self.port_set_word(port, PORT_W_OPEN, 0);
    }


    /// Allocates a GC-leaf object of `type_id` with its payload words set from `words`.
    pub(crate) fn alloc_leaf(&mut self, type_id: u32, words: &[u32]) -> Result<Value, Trap> {
        let reference = self.alloc_object(type_id).ok_or(Trap::OutOfMemory)?;
        for (i, &word) in words.iter().enumerate() {
            self.heap.write_u32(reference.0 + (i as u32) * 4, word);
        }
        Ok(Value::from_ref(reference))
    }

    /// One raw payload word of a GC-leaf object.
    pub(crate) fn leaf_word(&self, obj: Value, word: u32) -> u32 {
        obj.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + word * 4))
    }

    /// Sets one raw payload word of a GC-leaf object.
    pub(crate) fn leaf_set_word(&mut self, obj: Value, word: u32, value: u32) {
        if let Some(r) = obj.as_ref() {
            self.heap.write_u32(r.0 + word * 4, value);
        }
    }

    /// Whether `value` is a heap object of the given GC type id (the shared `is_*` shape, so a
    /// sibling module can test a shim's own type without reaching into the private heap).
    pub(crate) fn is_type(&self, value: Value, type_id: u32) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == type_id)
    }

    /// The selected target board (so a sibling shim module can reach the per-board pin facts).
    pub(crate) fn board(&self) -> crate::gpio::Board {
        self.board
    }

    /// Copies `data` into the prefix of a `bytearray` (up to its capacity), returning the count --
    /// the `readinto`/`readfrom_into` fill path for a shim that reads through the standard verbs.
    pub(crate) fn fill_bytearray_prefix(&mut self, buf: Value, data: &[u8]) -> Result<usize, Trap> {
        if !self.is_bytearray(buf) {
            return Err(self.with_message(Trap::TypeError, "expected a bytearray to read into"));
        }
        let capacity = self.bytes_value(buf).map_or(0, <[u8]>::len);
        let n = data.len().min(capacity);
        if let Some(slot) = self.byte_buffer_slot(buf) {
            for (at, &byte) in data.iter().take(n).enumerate() {
                self.byte_buffers[slot][at] = byte;
            }
        }
        Ok(n)
    }

    /// A shim's pin argument (`clock=`/`sda=`/...) must MATCH the table-fixed pin -- `None` (omitted)
    /// passes, a differing pin fails loud rather than silently driving the wrong line.
    pub(crate) fn shim_require_pin(
        &mut self,
        given: Value,
        expected: u32,
        which: &str,
    ) -> Result<(), Trap> {
        if given.is_none() || given.as_int() == Some(i64::from(expected)) {
            return Ok(());
        }
        let message = alloc::format!(
            "{which} is fixed to pin {expected} by this board's table (pass that pin, or omit it)"
        );
        Err(self.with_message(Trap::ValueError, &message))
    }


    /// The `spi` module singleton. Bind it as the global `spi` so a program reaches `spi.open(...)`.
    pub fn spi_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.spi_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `spi` singleton.
    #[must_use]
    pub fn is_spi(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.spi_type_id)
    }

    /// Whether `value` is an open-or-closed `SpiBus`.
    #[must_use]
    pub fn is_spi_bus(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.spi_bus_type_id)
    }

    /// Allocates a board SPI resource handle (`board.SPI0`) carrying its instance number.
    pub(crate) fn new_spi_resource(&mut self, instance: u32) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.spi_resource_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, instance);
        Ok(Value::from_ref(reference))
    }

    /// The instance number if `value` is a board SPI resource.
    fn spi_resource_instance(&self, value: Value) -> Option<u32> {
        value
            .as_ref()
            .filter(|r| self.heap.type_id_of(*r) == self.spi_resource_type_id)
            .map(|r| self.heap.read_u32(r.0))
    }

    /// Allocates an OPEN `SpiBus` over `instance` with the realized rate + config echoed in.
    fn new_spi_bus(
        &mut self,
        instance: u32,
        config: &crate::spi::SpiConfig,
        realized: u32,
        cs_pin: u32,
    ) -> Result<Value, Trap> {
        use crate::spi::*;
        let reference = self.alloc_object(self.spi_bus_type_id).ok_or(Trap::OutOfMemory)?;
        let base = reference.0;
        self.heap.write_u32(base + BUS_W_INSTANCE * 4, instance);
        self.heap.write_u32(base + BUS_W_OPEN * 4, 1);
        self.heap.write_u32(base + BUS_W_FREQUENCY * 4, realized);
        self.heap.write_u32(base + BUS_W_MODE * 4, config.mode);
        self.heap.write_u32(base + BUS_W_BIT_ORDER * 4, config.bit_order);
        self.heap.write_u32(base + BUS_W_CS_PIN * 4, cs_pin);
        Ok(Value::from_ref(reference))
    }

    /// One raw word of a `SpiBus`'s payload.
    fn spi_bus_word(&self, bus: Value, word: u32) -> u32 {
        bus.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + word * 4))
    }

    fn spi_bus_set_word(&mut self, bus: Value, word: u32, value: u32) {
        if let Some(r) = bus.as_ref() {
            self.heap.write_u32(r.0 + word * 4, value);
        }
    }

    /// Replays one SPI driver-sequence step over the MMIO seam; a `ReadInto` returns the inbound
    /// (full-duplex MISO) byte it read, everything else `None`.
    fn apply_spi_op(&mut self, op: crate::spi::SpiOp) -> Option<u32> {
        use crate::spi::SpiOp;
        match op {
            SpiOp::Write { reg, value } => {
                self.mmio_write(reg, value);
                None
            }
            SpiOp::PollEq { reg, mask, want } => {
                while self.mmio_read(reg) & mask != want {}
                None
            }
            SpiOp::ReadInto { reg, mask } => Some(self.mmio_read(reg) & mask),
        }
    }

    /// Asserts the managed chip-select (drives it LOW = selected); a raw bus ([`NO_CS`]) no-ops.
    fn spi_cs_assert(&mut self, cs_pin: u32) {
        if cs_pin == crate::spi::NO_CS {
            return;
        }
        let regs = self.board.pin_regs(cs_pin);
        self.mmio_write(regs.clr_reg, regs.clr_val);
    }

    /// Deasserts the managed chip-select (drives it HIGH = idle/deselected); a raw bus no-ops.
    fn spi_cs_deassert(&mut self, cs_pin: u32) {
        if cs_pin == crate::spi::NO_CS {
            return;
        }
        let regs = self.board.pin_regs(cs_pin);
        self.mmio_write(regs.set_reg, regs.set_val);
    }

    /// Clocks `out` full-duplex, returning the `out.len()` bytes clocked in simultaneously.
    fn spi_clock_bytes(&mut self, instance: u32, out: &[u8]) -> alloc::vec::Vec<u8> {
        let board = self.board;
        let mut inbound = alloc::vec::Vec::with_capacity(out.len());
        for &byte in out {
            for op in board.spi_transfer_byte_ops(instance, byte) {
                if let Some(rx) = self.apply_spi_op(op) {
                    inbound.push(rx as u8);
                }
            }
        }
        inbound
    }

    /// `spi.open(resource, **config)`: validates the config, claims the BUS (+ the managed CS pin,
    /// atomically -- a failed CS claim releases the bus), replays the bring-up, drives CS idle HIGH,
    /// and returns the `SpiBus`.
    pub(crate) fn spi_open(&mut self, posargs: &[Value], kwargs: &[(&str, Value)]) -> Result<Value, Trap> {
        use crate::spi::*;
        let [resource] = posargs else {
            let message = "open() takes exactly one positional argument (the board SPI resource)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let Some(instance) = self.spi_resource_instance(*resource) else {
            let message = "open() expects a board SPI resource (e.g. board.SPI0)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let mut config = SpiConfig::default();
        let mut cs = Value::NONE;
        for &(name, value) in kwargs {
            match name {
                "frequency" => {
                    let hz = value.as_int().unwrap_or(0);
                    if hz <= 0 || hz > i64::from(u32::MAX) {
                        let message = "frequency must be a positive integer";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                    config.frequency = hz as u32;
                }
                "mode" => match value.as_int() {
                    Some(mode @ 0..=3) => config.mode = mode as u32,
                    _ => {
                        let message = "mode must be 0, 1, 2 or 3 (CPOL<<1 | CPHA)";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                },
                "bit_order" => {
                    let Some(code) = self.str_value(value).and_then(bit_order_code) else {
                        let message = "bit_order must be 'msb' or 'lsb'";
                        return Err(self.with_message(Trap::ValueError, message));
                    };
                    config.bit_order = code;
                }
                "cs" => cs = value,
                other => {
                    let message =
                        alloc::format!("open() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        let cs_pin = if cs.is_none() {
            NO_CS
        } else {
            let Some(pin) = cs.as_int().and_then(|n| u32::try_from(n).ok()) else {
                let message = "cs must be a board pin (e.g. board.GP5) or None";
                return Err(self.with_message(Trap::TypeError, message));
            };
            if self.board.spi_function_pins(instance).contains(&pin) {
                let message = alloc::format!(
                    "cs=pin {pin} is muxed to the SPI function by this board's table; name a free gpio"
                );
                return Err(self.with_message(Trap::ValueError, &message));
            }
            pin
        };
        if self.spi_claimed.contains(&instance) {
            let message = alloc::format!("SPI{instance} in use");
            return Err(self.raise_named_exception("OSError", &message));
        }
        let (ops, realized) = match self.board.spi_open_ops(instance, &config) {
            None => {
                let message = "spi is not supported on this board yet";
                return Err(self.with_message(Trap::Unsupported, message));
            }
            Some(Err(SpiConfigError::BaudUnreachable)) => {
                let message = "frequency is below this spi's divider floor";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Err(SpiConfigError::BitOrderNotTabled)) => {
                let message =
                    "bit_order='lsb' is not in this chip's table (the PL022 is MSB-first only)";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Ok(pair)) => pair,
        };
        self.spi_claimed.push(instance);
        if cs_pin != NO_CS {
            if let Err(err) = self.claim_pin(cs_pin) {
                self.spi_claimed.retain(|&claimed| claimed != instance);
                return Err(err);
            }
        }
        for op in ops {
            self.apply_spi_op(op);
        }
        if cs_pin != NO_CS {
            let board = self.board;
            for op in board.open_ops(cs_pin, true) {
                self.apply_reg_op(op);
            }
            let regs = board.pin_regs(cs_pin);
            self.mmio_write(regs.set_reg, regs.set_val);
        }
        self.new_spi_bus(instance, &config, realized, cs_pin)
    }

    /// Dispatches a `spi` module method, positional form.
    pub(crate) fn call_spi_method(
        &mut self,
        _spi: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        match method_id {
            crate::spi::SPI_OPEN => self.spi_open(args, &[]),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a keyword call on the `spi` module or a `SpiBus` (`Op::CallKw`):
    /// `spi.open(baudrate=..., cs=...)`, `bus.read(n, fill=...)`.
    pub(crate) fn call_spi_bound_kw(
        &mut self,
        receiver: Value,
        method_id: u32,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        use crate::spi::*;
        if self.is_spi(receiver) {
            return match method_id {
                SPI_OPEN => self.spi_open(posargs, kwargs),
                _ => Err(Trap::AttributeError),
            };
        }
        let mut fill = Value::NONE;
        for &(name, value) in kwargs {
            if name == "fill" && method_id == BUS_READ {
                fill = value;
            } else {
                let message =
                    alloc::format!("this method got an unexpected keyword argument '{name}'");
                return Err(self.raise_named_exception("TypeError", &message));
            }
        }
        self.spi_bus_dispatch(receiver, method_id, posargs, fill)
    }

    /// Dispatches a `SpiBus` method, positional form (`fill` defaults to 0x00).
    pub(crate) fn call_spi_bus_method(
        &mut self,
        bus: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        self.spi_bus_dispatch(bus, method_id, args, Value::NONE)
    }

    /// The instance of an OPEN bus (`ValueError` after `close`).
    fn spi_bus_require_open(&mut self, bus: Value) -> Result<u32, Trap> {
        use crate::spi::{BUS_W_INSTANCE, BUS_W_OPEN};
        if self.spi_bus_word(bus, BUS_W_OPEN) == 0 {
            return Err(self.with_message(Trap::ValueError, "I/O operation on closed spi bus"));
        }
        Ok(self.spi_bus_word(bus, BUS_W_INSTANCE))
    }

    /// The one `SpiBus` dispatch both call forms funnel through. Managed CS brackets every whole
    /// operation with ONE assert/deassert pair (the family wire contract).
    pub(crate) fn spi_bus_dispatch(
        &mut self,
        bus: Value,
        method_id: u32,
        args: &[Value],
        fill: Value,
    ) -> Result<Value, Trap> {
        use crate::spi::*;
        match method_id {
            BUS_TRANSFER => {
                let [out] = args else {
                    return Err(Trap::TypeError);
                };
                let Some(out) = self.bytes_value(*out).map(<[u8]>::to_vec) else {
                    return Err(self.spi_needs_bytes(*out));
                };
                let instance = self.spi_bus_require_open(bus)?;
                let cs = self.spi_bus_word(bus, BUS_W_CS_PIN);
                self.spi_cs_assert(cs);
                let inbound = self.spi_clock_bytes(instance, &out);
                self.spi_cs_deassert(cs);
                self.new_bytes(inbound)
            }
            BUS_WRITE => {
                let [data] = args else {
                    return Err(Trap::TypeError);
                };
                let Some(data) = self.bytes_value(*data).map(<[u8]>::to_vec) else {
                    return Err(self.spi_needs_bytes(*data));
                };
                let instance = self.spi_bus_require_open(bus)?;
                let cs = self.spi_bus_word(bus, BUS_W_CS_PIN);
                self.spi_cs_assert(cs);
                let _ = self.spi_clock_bytes(instance, &data);
                self.spi_cs_deassert(cs);
                Value::fixnum(data.len() as i32).ok_or(Trap::Overflow)
            }
            BUS_READ => {
                let [n] = args else {
                    return Err(Trap::TypeError);
                };
                let Some(n) = n.as_int().filter(|&n| n >= 0) else {
                    let message = "read size must be an int >= 0";
                    return Err(self.with_message(Trap::ValueError, message));
                };
                let fill_byte = if fill.is_none() {
                    0u8
                } else {
                    match fill.as_int() {
                        Some(b @ 0..=255) => b as u8,
                        _ => {
                            let message = "fill must be a byte value in 0..255";
                            return Err(self.with_message(Trap::ValueError, message));
                        }
                    }
                };
                let instance = self.spi_bus_require_open(bus)?;
                let cs = self.spi_bus_word(bus, BUS_W_CS_PIN);
                self.spi_cs_assert(cs);
                let out = alloc::vec![fill_byte; n as usize];
                let inbound = self.spi_clock_bytes(instance, &out);
                self.spi_cs_deassert(cs);
                self.new_bytes(inbound)
            }
            BUS_TRANSFER_INTO => {
                let [out, into] = args else {
                    return Err(Trap::TypeError);
                };
                let Some(out) = self.bytes_value(*out).map(<[u8]>::to_vec) else {
                    return Err(self.spi_needs_bytes(*out));
                };
                if !self.is_bytearray(*into) {
                    let message = "transfer_into() destination must be a bytearray";
                    return Err(self.with_message(Trap::TypeError, message));
                }
                let capacity = self.bytes_value(*into).map_or(0, <[u8]>::len);
                if capacity < out.len() {
                    let message = "transfer_into() destination is smaller than the source";
                    return Err(self.with_message(Trap::ValueError, message));
                }
                let instance = self.spi_bus_require_open(bus)?;
                let cs = self.spi_bus_word(bus, BUS_W_CS_PIN);
                self.spi_cs_assert(cs);
                let inbound = self.spi_clock_bytes(instance, &out);
                self.spi_cs_deassert(cs);
                if let Some(slot) = self.byte_buffer_slot(*into) {
                    for (at, &byte) in inbound.iter().enumerate() {
                        self.byte_buffers[slot][at] = byte;
                    }
                }
                Value::fixnum(inbound.len() as i32).ok_or(Trap::Overflow)
            }
            BUS_CLOSE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.spi_bus_close(bus);
                Ok(Value::NONE)
            }
            BUS_ENTER => Ok(bus),
            BUS_EXIT => {
                self.spi_bus_close(bus);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// The `a bytes-like object is required, not 'X'` TypeError for an SPI verb's data argument.
    fn spi_needs_bytes(&mut self, value: Value) -> Trap {
        let message =
            alloc::format!("a bytes-like object is required, not '{}'", self.type_name_of(value));
        self.raise_named_exception("TypeError", &message)
    }

    /// Closes a bus: releases the instance claim AND the managed CS pin (idempotent, like a file).
    fn spi_bus_close(&mut self, bus: Value) {
        use crate::spi::{BUS_W_CS_PIN, BUS_W_INSTANCE, BUS_W_OPEN, NO_CS};
        if self.spi_bus_word(bus, BUS_W_OPEN) == 0 {
            return;
        }
        let instance = self.spi_bus_word(bus, BUS_W_INSTANCE);
        let cs = self.spi_bus_word(bus, BUS_W_CS_PIN);
        self.spi_claimed.retain(|&claimed| claimed != instance);
        if cs != NO_CS {
            self.release_pin(cs);
        }
        self.spi_bus_set_word(bus, BUS_W_OPEN, 0);
    }

    /// Reconfigures an OPEN bus in place (the `busio.SPI.configure` shim path): re-runs the SSP
    /// block reprogram UNDER the held claim (no release, no steal window) and WITHOUT the board-
    /// shared clock bring-up, then updates the read-only echoes. The public standard's
    /// "reconfigure = close + reopen" is untouched.
    pub(crate) fn spi_reconfigure(
        &mut self,
        bus: Value,
        config: &crate::spi::SpiConfig,
    ) -> Result<(), Trap> {
        use crate::spi::*;
        let instance = self.spi_bus_word(bus, BUS_W_INSTANCE);
        let (ops, realized) = match self.board.spi_reconfigure_ops(instance, config) {
            None => {
                let message = "spi reconfigure is not supported on this board";
                return Err(self.with_message(Trap::Unsupported, message));
            }
            Some(Err(SpiConfigError::BaudUnreachable)) => {
                let message = "frequency is below this spi's divider floor";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Err(SpiConfigError::BitOrderNotTabled)) => {
                let message =
                    "bit_order='lsb' is not in this chip's table (the PL022 is MSB-first only)";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Ok(pair)) => pair,
        };
        if realized == self.spi_bus_word(bus, BUS_W_FREQUENCY)
            && config.mode == self.spi_bus_word(bus, BUS_W_MODE)
            && config.bit_order == self.spi_bus_word(bus, BUS_W_BIT_ORDER)
        {
            return Ok(());
        }
        for op in ops {
            self.apply_spi_op(op);
        }
        self.spi_bus_set_word(bus, BUS_W_FREQUENCY, realized);
        self.spi_bus_set_word(bus, BUS_W_MODE, config.mode);
        self.spi_bus_set_word(bus, BUS_W_BIT_ORDER, config.bit_order);
        Ok(())
    }

    /// The host SPI sim's write side-effect: a data-register write captures the MOSI byte and
    /// queues its full-duplex reply (the scripted MISO stream, 0x00 when exhausted).
    #[cfg(not(target_os = "none"))]
    fn spi_sim_write(&mut self, address: u32, value: u32) {
        if let Some(facts) = self.board.spi_facts(0) {
            if address == facts.data_reg {
                self.spi_sim_tx.push((value & 0xFF) as u8);
                let reply = self.spi_sim_respond.pop_front().unwrap_or(0);
                self.spi_sim_rx_pending.push_back(reply);
            }
        }
    }

    /// The host SPI sim's read: the status register (idle flags, RX-ready when a reply is queued),
    /// the data register (pops one queued reply), and the reset-done ready bits.
    #[cfg(not(target_os = "none"))]
    fn spi_sim_read(&mut self, address: u32) -> Option<u32> {
        let facts = self.board.spi_facts(0)?;
        if address == facts.status_reg {
            let mut status = facts.status_idle_flags;
            if !self.spi_sim_rx_pending.is_empty() {
                status |= facts.status_rx_ready;
            }
            return Some(status);
        }
        if address == facts.data_reg {
            return Some(u32::from(self.spi_sim_rx_pending.pop_front().unwrap_or(0)));
        }
        for &(reg, value) in facts.sim_ready {
            if address == reg {
                return Some(value);
            }
        }
        None
    }

    /// Queues MISO bytes into the host SPI sim (the scripted device response). Each byte the
    /// program clocks out consumes one as its full-duplex reply; an exhausted queue reads 0x00
    /// (a test convention -- a real undriven MISO is indeterminate).
    #[cfg(not(target_os = "none"))]
    pub fn spi_sim_respond(&mut self, data: &[u8]) {
        self.spi_sim_respond.extend(data.iter().copied());
    }

    /// The bytes the program has clocked out through the host SPI sim (MOSI) -- the TX oracle.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn spi_sim_tx(&self) -> &[u8] {
        &self.spi_sim_tx
    }


    /// The `i2c` module singleton. Bind it as the global `i2c` so a program reaches `i2c.open(...)`.
    pub fn i2c_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.i2c_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `i2c` singleton.
    #[must_use]
    pub fn is_i2c(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.i2c_type_id)
    }

    /// Whether `value` is an open-or-closed `I2cBus`.
    #[must_use]
    pub fn is_i2c_bus(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.i2c_bus_type_id)
    }

    /// Allocates a board I2C resource handle (`board.I2C0`) carrying its instance number.
    pub(crate) fn new_i2c_resource(&mut self, instance: u32) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.i2c_resource_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, instance);
        Ok(Value::from_ref(reference))
    }

    /// The instance number if `value` is a board I2C resource.
    fn i2c_resource_instance(&self, value: Value) -> Option<u32> {
        value
            .as_ref()
            .filter(|r| self.heap.type_id_of(*r) == self.i2c_resource_type_id)
            .map(|r| self.heap.read_u32(r.0))
    }

    /// Allocates an OPEN `I2cBus` over `instance` with the realized SCL rate echoed in.
    fn new_i2c_bus(&mut self, instance: u32, realized: u32) -> Result<Value, Trap> {
        use crate::i2c::*;
        let reference = self.alloc_object(self.i2c_bus_type_id).ok_or(Trap::OutOfMemory)?;
        let base = reference.0;
        self.heap.write_u32(base + BUS_W_INSTANCE * 4, instance);
        self.heap.write_u32(base + BUS_W_OPEN * 4, 1);
        self.heap.write_u32(base + BUS_W_FREQUENCY * 4, realized);
        Ok(Value::from_ref(reference))
    }

    /// One raw word of an `I2cBus`'s payload.
    fn i2c_bus_word(&self, bus: Value, word: u32) -> u32 {
        bus.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + word * 4))
    }

    fn i2c_bus_set_word(&mut self, bus: Value, word: u32, value: u32) {
        if let Some(r) = bus.as_ref() {
            self.heap.write_u32(r.0 + word * 4, value);
        }
    }

    /// Replays one I2C init step over the MMIO seam.
    fn apply_i2c_op(&mut self, op: crate::i2c::I2cOp) {
        use crate::i2c::I2cOp;
        match op {
            I2cOp::Write { reg, value } => self.mmio_write(reg, value),
            I2cOp::PollEq { reg, mask, want } => while self.mmio_read(reg) & mask != want {},
        }
    }

    /// `i2c.open(resource, **config)`: validates the config, claims the bus, replays the bring-up,
    /// returns the `I2cBus`.
    pub(crate) fn i2c_open(&mut self, posargs: &[Value], kwargs: &[(&str, Value)]) -> Result<Value, Trap> {
        use crate::i2c::*;
        let [resource] = posargs else {
            let message = "open() takes exactly one positional argument (the board I2C resource)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let Some(instance) = self.i2c_resource_instance(*resource) else {
            let message = "open() expects a board I2C resource (e.g. board.I2C0)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let mut config = I2cConfig::default();
        for &(name, value) in kwargs {
            match name {
                "frequency" => {
                    let freq = value.as_int().unwrap_or(0);
                    if freq <= 0 || freq > i64::from(u32::MAX) {
                        let message = "frequency must be a positive integer";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                    config.frequency = freq as u32;
                }
                other => {
                    let message =
                        alloc::format!("open() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        if self.i2c_claimed.contains(&instance) {
            let message = alloc::format!("I2C{instance} in use");
            return Err(self.raise_named_exception("OSError", &message));
        }
        let (ops, realized) = match self.board.i2c_open_ops(instance, &config) {
            None => {
                let message = "i2c is not supported on this board yet";
                return Err(self.with_message(Trap::Unsupported, message));
            }
            Some(Err(I2cConfigError::FrequencyUnreachable)) => {
                let message = "frequency is out of range for this i2c";
                return Err(self.with_message(Trap::ValueError, message));
            }
            Some(Ok(pair)) => pair,
        };
        self.i2c_claimed.push(instance);
        for op in ops {
            self.apply_i2c_op(op);
        }
        self.new_i2c_bus(instance, realized)
    }

    /// The retarget window: the DW_apb_i2c latches IC_TAR only while disabled.
    fn i2c_retarget(&mut self, facts: &crate::i2c::I2cFacts, addr: u8) {
        self.mmio_write(facts.enable, 0);
        self.mmio_write(facts.tar, u32::from(addr));
        self.mmio_write(facts.enable, 1);
    }

    /// A multi-byte master write (START + addr + bytes + STOP, or the abort), raising on NACK
    /// AFTER the abort-clear recovery -- an ADDRESS NACK and a DATA NACK carry distinct messages.
    fn i2c_write(
        &mut self,
        facts: &crate::i2c::I2cFacts,
        addr: u8,
        data: &[u8],
    ) -> Result<(), Trap> {
        if data.is_empty() {
            return Ok(());
        }
        self.i2c_retarget(facts, addr);
        for (i, &byte) in data.iter().enumerate() {
            while self.mmio_read(facts.status) & facts.status_tfnf == 0 {}
            let stop = if i + 1 == data.len() { facts.cmd_stop } else { 0 };
            self.mmio_write(facts.data_cmd, u32::from(byte) | stop);
        }
        while self.mmio_read(facts.raw_intr_stat) & facts.intr_tx_empty == 0 {}
        let abort = self.mmio_read(facts.abort_source);
        if abort != 0 {
            self.mmio_read(facts.clr_tx_abrt);
        }
        while self.mmio_read(facts.raw_intr_stat) & facts.intr_stop_det == 0 {}
        self.mmio_read(facts.clr_stop_det);
        if abort != 0 {
            return Err(self.i2c_nack_error(facts, addr, abort));
        }
        Ok(())
    }

    /// A multi-byte master read (START + addr + R, n bytes + STOP), raising on NACK.
    fn i2c_read(
        &mut self,
        facts: &crate::i2c::I2cFacts,
        addr: u8,
        n: usize,
    ) -> Result<alloc::vec::Vec<u8>, Trap> {
        if n == 0 {
            return Ok(alloc::vec::Vec::new());
        }
        self.i2c_retarget(facts, addr);
        let mut out = alloc::vec::Vec::with_capacity(n);
        for i in 0..n {
            while self.mmio_read(facts.status) & facts.status_tfnf == 0 {}
            let stop = if i + 1 == n { facts.cmd_stop } else { 0 };
            self.mmio_write(facts.data_cmd, facts.cmd_read | stop);
            if self.mmio_read(facts.raw_intr_stat) & facts.intr_tx_abrt != 0 {
                let abort = self.mmio_read(facts.abort_source);
                self.mmio_read(facts.clr_tx_abrt);
                return Err(self.i2c_nack_error(facts, addr, abort));
            }
            while self.mmio_read(facts.rxflr) == 0 {}
            out.push((self.mmio_read(facts.data_cmd) & 0xFF) as u8);
        }
        Ok(out)
    }

    /// Write-then-read joined by a repeated START (the register-read shape): the `out` bytes with
    /// NO stop, then `n` reads (RESTART on the first, STOP on the last). `n` >= 1.
    fn i2c_write_then_read(
        &mut self,
        facts: &crate::i2c::I2cFacts,
        addr: u8,
        out: &[u8],
        n: usize,
    ) -> Result<alloc::vec::Vec<u8>, Trap> {
        self.i2c_retarget(facts, addr);
        for &byte in out {
            while self.mmio_read(facts.status) & facts.status_tfnf == 0 {}
            self.mmio_write(facts.data_cmd, u32::from(byte));
        }
        let mut result = alloc::vec::Vec::with_capacity(n);
        for i in 0..n {
            while self.mmio_read(facts.status) & facts.status_tfnf == 0 {}
            let mut cmd = facts.cmd_read;
            if i == 0 {
                cmd |= facts.cmd_restart;
            }
            if i + 1 == n {
                cmd |= facts.cmd_stop;
            }
            self.mmio_write(facts.data_cmd, cmd);
            if self.mmio_read(facts.raw_intr_stat) & facts.intr_tx_abrt != 0 {
                let abort = self.mmio_read(facts.abort_source);
                self.mmio_read(facts.clr_tx_abrt);
                return Err(self.i2c_nack_error(facts, addr, abort));
            }
            while self.mmio_read(facts.rxflr) == 0 {}
            result.push((self.mmio_read(facts.data_cmd) & 0xFF) as u8);
        }
        Ok(result)
    }

    /// A one-byte read probe: returns whether the address ACKs (present), without raising -- the
    /// scanner's primitive, keeping absence in the DATA path, not the exception path.
    fn i2c_try_probe(&mut self, facts: &crate::i2c::I2cFacts, addr: u8) -> bool {
        self.i2c_retarget(facts, addr);
        while self.mmio_read(facts.status) & facts.status_tfnf == 0 {}
        self.mmio_write(facts.data_cmd, facts.cmd_read | facts.cmd_stop);
        if self.mmio_read(facts.raw_intr_stat) & facts.intr_tx_abrt != 0 {
            self.mmio_read(facts.abort_source);
            self.mmio_read(facts.clr_tx_abrt);
            return false;
        }
        while self.mmio_read(facts.rxflr) == 0 {}
        self.mmio_read(facts.data_cmd);
        true
    }

    /// The `OSError` for a NACK: an ADDRESS NACK vs a DATA-byte NACK, the phase named (the type is
    /// the family contract; the message text is per-language and non-tier-stable).
    fn i2c_nack_error(&mut self, facts: &crate::i2c::I2cFacts, addr: u8, abort: u32) -> Trap {
        let message = if abort & facts.abrt_data_nack != 0 {
            alloc::format!("data byte not acknowledged by address {addr:#04x}")
        } else {
            alloc::format!("no acknowledgment from address {addr:#04x}")
        };
        self.raise_named_exception("OSError", &message)
    }

    /// Parses a 7-bit address argument.
    fn i2c_addr_arg(&mut self, value: Value) -> Result<u8, Trap> {
        match value.as_int() {
            Some(a @ 0..=127) => Ok(a as u8),
            _ => Err(self.with_message(Trap::ValueError, "address must be an int in 0..127")),
        }
    }

    /// Parses a register-byte argument.
    fn i2c_byte_arg(&mut self, value: Value) -> Result<u8, Trap> {
        match value.as_int() {
            Some(b @ 0..=255) => Ok(b as u8),
            _ => Err(self.with_message(Trap::ValueError, "register must be a byte in 0..255")),
        }
    }

    /// Parses a byte-count argument.
    fn i2c_count_arg(&mut self, value: Value) -> Result<usize, Trap> {
        match value.as_int() {
            Some(n) if n >= 0 => Ok(n as usize),
            _ => Err(self.with_message(Trap::ValueError, "count must be an int >= 0")),
        }
    }

    /// The `a bytes-like object is required, not 'X'` TypeError for an I2C verb's data argument.
    fn i2c_needs_bytes(&mut self, value: Value) -> Trap {
        let message =
            alloc::format!("a bytes-like object is required, not '{}'", self.type_name_of(value));
        self.raise_named_exception("TypeError", &message)
    }

    /// The instance + facts of an OPEN bus (`ValueError` after `close`).
    fn i2c_bus_require_open(&mut self, bus: Value) -> Result<crate::i2c::I2cFacts, Trap> {
        use crate::i2c::{BUS_W_INSTANCE, BUS_W_OPEN};
        if self.i2c_bus_word(bus, BUS_W_OPEN) == 0 {
            return Err(self.with_message(Trap::ValueError, "I/O operation on closed i2c bus"));
        }
        let instance = self.i2c_bus_word(bus, BUS_W_INSTANCE);
        self.board.i2c_facts(instance).ok_or(Trap::Malformed)
    }

    /// Dispatches an `i2c` module method, positional form.
    pub(crate) fn call_i2c_method(
        &mut self,
        _i2c: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        match method_id {
            crate::i2c::I2C_OPEN => self.i2c_open(args, &[]),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a keyword call on the `i2c` module or an `I2cBus` (`Op::CallKw`):
    /// `i2c.open(frequency=...)`, `bus.read_register(addr, reg, n=...)`.
    pub(crate) fn call_i2c_bound_kw(
        &mut self,
        receiver: Value,
        method_id: u32,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        use crate::i2c::*;
        if self.is_i2c(receiver) {
            return match method_id {
                I2C_OPEN => self.i2c_open(posargs, kwargs),
                _ => Err(Trap::AttributeError),
            };
        }
        let mut kw_n = Value::NONE;
        for &(name, value) in kwargs {
            if name == "n" && method_id == BUS_READ_REGISTER {
                kw_n = value;
            } else {
                let message =
                    alloc::format!("this method got an unexpected keyword argument '{name}'");
                return Err(self.raise_named_exception("TypeError", &message));
            }
        }
        self.i2c_bus_dispatch(receiver, method_id, posargs, kw_n)
    }

    /// Dispatches an `I2cBus` method, positional form.
    pub(crate) fn call_i2c_bus_method(
        &mut self,
        bus: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        self.i2c_bus_dispatch(bus, method_id, args, Value::NONE)
    }

    /// The one `I2cBus` dispatch both call forms funnel through.
    pub(crate) fn i2c_bus_dispatch(
        &mut self,
        bus: Value,
        method_id: u32,
        args: &[Value],
        kw_n: Value,
    ) -> Result<Value, Trap> {
        use crate::i2c::*;
        match method_id {
            BUS_SCAN => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let facts = self.i2c_bus_require_open(bus)?;
                let mut found = alloc::vec::Vec::new();
                for addr in SCAN_START..=SCAN_END {
                    if self.i2c_try_probe(&facts, addr) {
                        found.push(Value::fixnum(i32::from(addr)).ok_or(Trap::Overflow)?);
                    }
                }
                self.new_list(found)
            }
            BUS_PROBE => {
                let [addr] = args else {
                    return Err(Trap::TypeError);
                };
                let addr = self.i2c_addr_arg(*addr)?;
                let facts = self.i2c_bus_require_open(bus)?;
                Ok(Value::from_bool(self.i2c_try_probe(&facts, addr)))
            }
            BUS_READ => {
                let [addr, n] = args else {
                    return Err(Trap::TypeError);
                };
                let addr = self.i2c_addr_arg(*addr)?;
                let n = self.i2c_count_arg(*n)?;
                let facts = self.i2c_bus_require_open(bus)?;
                let bytes = self.i2c_read(&facts, addr, n)?;
                self.new_bytes(bytes)
            }
            BUS_WRITE => {
                let [addr, data] = args else {
                    return Err(Trap::TypeError);
                };
                let addr = self.i2c_addr_arg(*addr)?;
                let Some(data) = self.bytes_value(*data).map(<[u8]>::to_vec) else {
                    return Err(self.i2c_needs_bytes(*data));
                };
                let facts = self.i2c_bus_require_open(bus)?;
                self.i2c_write(&facts, addr, &data)?;
                Value::fixnum(data.len() as i32).ok_or(Trap::Overflow)
            }
            BUS_WRITE_THEN_READ => {
                let [addr, out, n] = args else {
                    return Err(Trap::TypeError);
                };
                let addr = self.i2c_addr_arg(*addr)?;
                let Some(out) = self.bytes_value(*out).map(<[u8]>::to_vec) else {
                    return Err(self.i2c_needs_bytes(*out));
                };
                let n = self.i2c_count_arg(*n)?;
                if n == 0 {
                    let message = "write_then_read count must be >= 1";
                    return Err(self.with_message(Trap::ValueError, message));
                }
                let facts = self.i2c_bus_require_open(bus)?;
                let bytes = self.i2c_write_then_read(&facts, addr, &out, n)?;
                self.new_bytes(bytes)
            }
            BUS_READ_REGISTER => {
                let (addr, register, n) = match args {
                    [addr, register] => (self.i2c_addr_arg(*addr)?, self.i2c_byte_arg(*register)?, 1),
                    [addr, register, n] => (
                        self.i2c_addr_arg(*addr)?,
                        self.i2c_byte_arg(*register)?,
                        self.i2c_count_arg(*n)?,
                    ),
                    _ => return Err(Trap::TypeError),
                };
                let n = if kw_n.is_none() { n } else { self.i2c_count_arg(kw_n)? };
                if n == 0 {
                    let message = "read_register n must be >= 1";
                    return Err(self.with_message(Trap::ValueError, message));
                }
                let facts = self.i2c_bus_require_open(bus)?;
                let bytes = self.i2c_write_then_read(&facts, addr, &[register], n)?;
                self.new_bytes(bytes)
            }
            BUS_WRITE_REGISTER => {
                let [addr, register, data] = args else {
                    return Err(Trap::TypeError);
                };
                let addr = self.i2c_addr_arg(*addr)?;
                let register = self.i2c_byte_arg(*register)?;
                let Some(data) = self.bytes_value(*data).map(<[u8]>::to_vec) else {
                    return Err(self.i2c_needs_bytes(*data));
                };
                let facts = self.i2c_bus_require_open(bus)?;
                let mut payload = alloc::vec![register];
                payload.extend_from_slice(&data);
                self.i2c_write(&facts, addr, &payload)?;
                Value::fixnum(data.len() as i32).ok_or(Trap::Overflow)
            }
            BUS_CLOSE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.i2c_bus_close(bus);
                Ok(Value::NONE)
            }
            BUS_ENTER => Ok(bus),
            BUS_EXIT => {
                self.i2c_bus_close(bus);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Closes a bus: releases the instance claim (idempotent, like a file's close).
    fn i2c_bus_close(&mut self, bus: Value) {
        use crate::i2c::{BUS_W_INSTANCE, BUS_W_OPEN};
        if self.i2c_bus_word(bus, BUS_W_OPEN) == 0 {
            return;
        }
        let instance = self.i2c_bus_word(bus, BUS_W_INSTANCE);
        self.i2c_claimed.retain(|&claimed| claimed != instance);
        self.i2c_bus_set_word(bus, BUS_W_OPEN, 0);
    }

    /// The host I2C sim's write side-effect: a retarget (IC_TAR) starts a fresh transaction; an
    /// IC_DATA_CMD word clocks a read/write against the addressed device (absent = ADDRESS NACK).
    #[cfg(not(target_os = "none"))]
    fn i2c_sim_write(&mut self, address: u32, value: u32) {
        let Some(facts) = self.board.i2c_facts(0) else {
            return;
        };
        if address == facts.tar {
            self.i2c_sim_abort = 0;
            self.i2c_sim_rx.clear();
            self.i2c_sim_stopped = false;
            self.i2c_sim_expect_pointer = true;
            return;
        }
        if address != facts.data_cmd || self.i2c_sim_abort != 0 {
            return;
        }
        let target = (self.mmio_sim.get(&facts.tar).copied().unwrap_or(0) & 0x7F) as u8;
        let is_read = value & facts.cmd_read != 0;
        let has_stop = value & facts.cmd_stop != 0;
        let expect_pointer = self.i2c_sim_expect_pointer;
        match self.i2c_sim_devices.get_mut(&target) {
            None => self.i2c_sim_abort = facts.abrt_addr_nack,
            Some(device) => {
                if is_read {
                    let at = device.pointer as usize;
                    let byte = device.registers.get(at).copied().unwrap_or(0xFF);
                    if device.read_auto_increment && at < device.registers.len() {
                        device.pointer = device.pointer.wrapping_add(1);
                    }
                    self.i2c_sim_rx.push_back(byte);
                } else if expect_pointer {
                    device.pointer = (value & 0x7F) as u8;
                    device.read_auto_increment = value & 0x80 != 0;
                    self.i2c_sim_expect_pointer = false;
                } else {
                    let at = device.pointer as usize;
                    if at < device.registers.len() {
                        device.registers[at] = (value & 0xFF) as u8;
                        device.pointer = device.pointer.wrapping_add(1);
                    } else {
                        self.i2c_sim_abort = facts.abrt_data_nack;
                    }
                }
            }
        }
        if has_stop || self.i2c_sim_abort != 0 {
            self.i2c_sim_stopped = true;
        }
    }

    /// The host I2C sim's read: IC_STATUS (room), IC_RAW_INTR_STAT (completion/abort/stop),
    /// IC_TX_ABRT_SOURCE, the clear-on-read registers, IC_RXFLR, and the RX pop via IC_DATA_CMD.
    #[cfg(not(target_os = "none"))]
    fn i2c_sim_read(&mut self, address: u32) -> Option<u32> {
        let facts = self.board.i2c_facts(0)?;
        if address == facts.status {
            return Some(facts.status_tfnf);
        }
        if address == facts.raw_intr_stat {
            let mut value = facts.intr_tx_empty;
            if self.i2c_sim_abort != 0 {
                value |= facts.intr_tx_abrt;
            }
            if self.i2c_sim_stopped {
                value |= facts.intr_stop_det;
            }
            return Some(value);
        }
        if address == facts.abort_source {
            return Some(self.i2c_sim_abort);
        }
        if address == facts.clr_tx_abrt {
            self.i2c_sim_abort = 0;
            return Some(0);
        }
        if address == facts.clr_stop_det {
            self.i2c_sim_stopped = false;
            return Some(0);
        }
        if address == facts.rxflr {
            return Some(self.i2c_sim_rx.len() as u32);
        }
        if address == facts.data_cmd {
            return Some(u32::from(self.i2c_sim_rx.pop_front().unwrap_or(0)));
        }
        None
    }

    /// Installs a simulated I2C target: a register-file device at `addr` (an 8-bit SUB pointer,
    /// SUB-bit-7-gated read auto-increment -- the LSM303AGR shape). A transaction to an address
    /// with no installed device NACKs, so a sensor demo runs off-device.
    #[cfg(not(target_os = "none"))]
    pub fn i2c_sim_add_device(&mut self, addr: u8, registers: &[u8]) {
        self.i2c_sim_devices.insert(
            addr,
            I2cSimDevice { registers: registers.to_vec(), pointer: 0, read_auto_increment: false },
        );
    }


    /// The `adc` module singleton. Bind it as the global `adc` so a program reaches `adc.open(...)`.
    pub fn adc_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.adc_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `adc` singleton.
    #[must_use]
    pub fn is_adc(&self, value: Value) -> bool {
        self.is_type(value, self.adc_type_id)
    }

    /// The `analogio` module singleton (the CircuitPython ADC shim namespace). Bind it as the
    /// global `analogio` so a program reaches `analogio.AnalogIn(...)`.
    pub fn analogio_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.analogio_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is an open-or-closed ADC `Channel`.
    #[must_use]
    pub fn is_adc_channel(&self, value: Value) -> bool {
        self.is_type(value, self.adc_channel_type_id)
    }

    /// Allocates a board ADC resource handle (`board.A0` / `board.TEMP_SENSOR`) carrying its
    /// `(channel, pin)`.
    pub(crate) fn new_adc_resource(&mut self, channel: u32, pin: u32) -> Result<Value, Trap> {
        self.alloc_leaf(self.adc_resource_type_id, &[channel, pin])
    }

    /// Whether `value` is a board ADC resource.
    pub(crate) fn is_adc_resource(&self, value: Value) -> bool {
        self.is_type(value, self.adc_resource_type_id)
    }

    /// The `(channel, pin)` if `value` is a board ADC resource.
    fn adc_resource_parts(&self, value: Value) -> Option<(u32, u32)> {
        value
            .as_ref()
            .filter(|r| self.heap.type_id_of(*r) == self.adc_resource_type_id)
            .map(|r| (self.heap.read_u32(r.0), self.heap.read_u32(r.0 + 4)))
    }

    /// Allocates an OPEN `Channel`.
    fn new_adc_channel(
        &mut self,
        channel: u32,
        pin: u32,
        bits: u32,
        reference_uv: u32,
    ) -> Result<Value, Trap> {
        self.alloc_leaf(self.adc_channel_type_id, &[channel, 1, pin, bits, reference_uv])
    }

    /// One raw word of a `Channel`'s payload.
    fn adc_channel_word(&self, channel: Value, word: u32) -> u32 {
        self.leaf_word(channel, word)
    }

    /// Replays one ADC init step over the MMIO seam.
    fn apply_adc_op(&mut self, op: crate::adc::AdcOp) {
        use crate::adc::AdcOp;
        match op {
            AdcOp::Write { reg, value } => self.mmio_write(reg, value),
            AdcOp::PollEq { reg, mask, want } => while self.mmio_read(reg) & mask != want {},
        }
    }

    /// `adc.open(resource)`: claims the channel (exclusive) + its pin (a pin-backed channel, through
    /// the SAME one-owner pool as gpio), brings the SHARED converter block up on the first open, and
    /// preps an external channel's pad.
    pub(crate) fn adc_open(&mut self, args: &[Value]) -> Result<Value, Trap> {
        use crate::adc::NO_PIN;
        let [resource] = args else {
            let message = "open() takes exactly one positional argument (the board ADC resource)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let Some((channel, pin)) = self.adc_resource_parts(*resource) else {
            let message = "open() expects a board ADC resource (e.g. board.A0 or board.TEMP_SENSOR)";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let Some(facts) = self.board.adc_facts() else {
            let message = "adc is not supported on this board yet";
            return Err(self.with_message(Trap::Unsupported, message));
        };
        if self.adc_channels_open.contains(&channel) {
            let message = alloc::format!("ADC channel {channel} in use");
            return Err(self.raise_named_exception("OSError", &message));
        }
        if pin != NO_PIN {
            self.claim_pin(pin)?;
        }
        let first_open = self.adc_channels_open.is_empty();
        self.adc_channels_open.push(channel);
        if first_open {
            for op in self.board.adc_block_init_ops() {
                self.apply_adc_op(op);
            }
        }
        if pin != NO_PIN {
            let board = self.board;
            for op in board.adc_pad_analog_ops(pin) {
                self.apply_adc_op(op);
            }
        }
        self.new_adc_channel(channel, pin, facts.bits, facts.reference_uv)
    }

    /// The instance + facts of an OPEN channel (`ValueError` after `close`).
    fn adc_channel_require_open(&mut self, channel: Value) -> Result<(u32, crate::adc::AdcFacts), Trap> {
        use crate::adc::{CH_W_CHANNEL, CH_W_OPEN};
        if self.adc_channel_word(channel, CH_W_OPEN) == 0 {
            return Err(self.with_message(Trap::ValueError, "I/O operation on closed adc channel"));
        }
        let index = self.adc_channel_word(channel, CH_W_CHANNEL);
        let facts = self.board.adc_facts().ok_or(Trap::Malformed)?;
        Ok((index, facts))
    }

    /// One single-shot conversion on `channel`: select AINSEL, START_ONCE, poll READY, discard an
    /// errored sample (re-run), read the count (the official pico-sdk protocol).
    fn adc_read_raw(&mut self, facts: &crate::adc::AdcFacts, channel: u32) -> u32 {
        loop {
            self.mmio_write(facts.cs, (channel << 12) | facts.cs_enabled);
            self.mmio_write(facts.cs, (channel << 12) | facts.cs_start);
            while self.mmio_read(facts.cs) & facts.cs_ready == 0 {}
            if self.mmio_read(facts.cs) & facts.cs_err != 0 {
                continue;
            }
            return self.mmio_read(facts.result) & facts.result_mask;
        }
    }

    /// Dispatches an `adc` module method, positional form.
    pub(crate) fn call_adc_method(
        &mut self,
        _adc: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        match method_id {
            crate::adc::ADC_OPEN => self.adc_open(args),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `Channel` method, positional form (no verb takes arguments).
    pub(crate) fn call_adc_channel_method(
        &mut self,
        channel: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::adc::*;
        match method_id {
            CH_READ_U16 => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let (index, facts) = self.adc_channel_require_open(channel)?;
                let raw = self.adc_read_raw(&facts, index);
                Value::fixnum(normalize_u16(raw, facts.bits) as i32).ok_or(Trap::Overflow)
            }
            CH_READ_RAW => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let (index, facts) = self.adc_channel_require_open(channel)?;
                let raw = self.adc_read_raw(&facts, index);
                Value::fixnum(raw as i32).ok_or(Trap::Overflow)
            }
            CH_READ_UV => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let (index, facts) = self.adc_channel_require_open(channel)?;
                let raw = self.adc_read_raw(&facts, index);
                let uv = raw_to_microvolts(raw, facts.bits, facts.reference_uv);
                Value::fixnum(uv as i32).ok_or(Trap::Overflow)
            }
            CH_CLOSE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.adc_channel_close(channel);
                Ok(Value::NONE)
            }
            CH_ENTER => Ok(channel),
            CH_EXIT => {
                self.adc_channel_close(channel);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Closes a channel: releases the exclusive channel claim + its pin (idempotent). The shared
    /// converter block re-inits on the next first-open after all channels close.
    fn adc_channel_close(&mut self, channel: Value) {
        use crate::adc::{CH_W_CHANNEL, CH_W_OPEN, CH_W_PIN, NO_PIN};
        if self.adc_channel_word(channel, CH_W_OPEN) == 0 {
            return;
        }
        let index = self.adc_channel_word(channel, CH_W_CHANNEL);
        let pin = self.adc_channel_word(channel, CH_W_PIN);
        self.adc_channels_open.retain(|&c| c != index);
        if pin != NO_PIN {
            self.release_pin(pin);
        }
        self.leaf_set_word(channel, CH_W_OPEN, 0);
    }

    /// The host ADC sim's write side-effect: a CS write carries AINSEL (the selected channel), so
    /// the next RESULT read returns THAT channel's scripted conversion.
    #[cfg(not(target_os = "none"))]
    fn adc_sim_write(&mut self, address: u32, value: u32) {
        if let Some(facts) = self.board.adc_facts() {
            if address == facts.cs {
                self.adc_sim_ainsel = (value >> 12) & 0xF;
            }
        }
    }

    /// The host ADC sim's read: the clk generator reports ENABLED, CS reports READY (ERR clear),
    /// and RESULT returns the currently-selected channel's scripted count.
    #[cfg(not(target_os = "none"))]
    fn adc_sim_read(&mut self, address: u32) -> Option<u32> {
        let facts = self.board.adc_facts()?;
        if address == facts.clk_ctrl {
            return Some(facts.clk_enabled);
        }
        if address == facts.cs {
            return Some(facts.cs_ready);
        }
        if address == facts.result {
            return Some(self.adc_sim_raw.get(&self.adc_sim_ainsel).copied().unwrap_or(0));
        }
        None
    }

    /// Scripts the conversion result at `channel` (at the table's resolution) for the host ADC sim,
    /// so a temperature/analogue demo runs off-device.
    #[cfg(not(target_os = "none"))]
    pub fn adc_sim_set(&mut self, channel: u32, raw: u32) {
        self.adc_sim_raw.insert(channel, raw);
    }


    /// The `busio` module singleton (the CircuitPython shim namespace).
    pub fn busio_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.busio_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A shim UART factory carrying its flavor (`machine.UART` / `busio.UART`).
    fn uart_shim_factory_singleton(&mut self, flavor: u32) -> Result<Value, Trap> {
        let reference =
            self.alloc_object(self.uart_shim_factory_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, flavor);
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a shim UART factory.
    #[must_use]
    pub fn is_uart_shim_factory(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.uart_shim_factory_type_id)
    }

    /// Whether `value` is a shim UART instance.
    #[must_use]
    pub fn is_uart_shim(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.uart_shim_type_id)
    }

    fn shim_word(&self, shim: Value, word: u32) -> u32 {
        shim.as_ref().map_or(0, |r| self.heap.read_u32(r.0 + word * 4))
    }

    /// The standard Port a shim instance wraps.
    fn shim_port(&self, shim: Value) -> Value {
        Value::from_bits(self.shim_word(shim, crate::uart::SHIM_W_PORT))
    }

    /// A shim's tx=/rx= pin arguments must MATCH the table-fixed pins -- the table wires the
    /// pins, so a differing override fails loud rather than silently transmitting elsewhere.
    fn shim_check_pin(&mut self, given: Value, expected: u32, which: &str) -> Result<(), Trap> {
        if given.is_none() {
            return Ok(());
        }
        if given.as_int() == Some(i64::from(expected)) {
            return Ok(());
        }
        let message = alloc::format!(
            "{which} is fixed to pin {expected} by this board's table (pass board.{}, or omit it)",
            if which == "tx" { "TX" } else { "RX" }
        );
        Err(self.with_message(Trap::ValueError, &message))
    }

    /// Constructs a shim UART: translates the shimmed API's constructor surface onto the clean
    /// `uart.open`, holding the shim-only state (the implicit timeout) on the shim.
    pub(crate) fn call_uart_shim_factory(
        &mut self,
        factory: Value,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        use crate::uart::*;
        let flavor = factory.as_ref().map_or(0, |r| self.heap.read_u32(r.0));
        let mut config = UartConfig::default();
        let mut timeout_ms: u32 = if flavor == SHIM_FLAVOR_MACHINE { 0 } else { 1000 };
        let mut instance: u32 = 0;
        let mut tx_arg = Value::NONE;
        let mut rx_arg = Value::NONE;
        if flavor == SHIM_FLAVOR_MACHINE {
            match posargs {
                [id] => {
                    instance = id.as_int().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| {
                        self.with_message(Trap::ValueError, "UART id must be a small int")
                    })?;
                }
                _ => {
                    let message = "UART(id, ...) takes the port id as its one positional argument";
                    return Err(self.with_message(Trap::TypeError, message));
                }
            }
            config.baudrate = 9600;
        } else {
            match posargs {
                [] => {}
                [tx] => tx_arg = *tx,
                [tx, rx] => {
                    tx_arg = *tx;
                    rx_arg = *rx;
                }
                _ => return Err(Trap::TypeError),
            }
            config.baudrate = 9600;
        }
        for &(name, value) in kwargs {
            match name {
                "baudrate" => {
                    let baud = value.as_int().unwrap_or(0);
                    if baud <= 0 || baud > i64::from(u32::MAX) {
                        let message = "baudrate must be a positive integer";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                    config.baudrate = baud as u32;
                }
                "bits" => match value.as_int() {
                    Some(bits @ 5..=8) => config.data_bits = bits as u32,
                    _ => {
                        let message = "bits must be 5, 6, 7 or 8";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                },
                "parity" => {
                    config.parity = if value.is_none() {
                        PARITY_NONE
                    } else if flavor == SHIM_FLAVOR_MACHINE && value.as_int() == Some(0) {
                        PARITY_EVEN
                    } else if flavor == SHIM_FLAVOR_MACHINE && value.as_int() == Some(1) {
                        PARITY_ODD
                    } else {
                        let message = if flavor == SHIM_FLAVOR_MACHINE {
                            "parity must be None, 0 (even) or 1 (odd)"
                        } else {
                            "the busio Parity enum is not modeled yet (parity=None only)"
                        };
                        return Err(self.with_message(Trap::ValueError, message));
                    };
                }
                "stop" => match value.as_int() {
                    Some(stop @ 1..=2) => config.stop_bits = stop as u32,
                    _ => {
                        let message = "stop must be 1 or 2";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                },
                "timeout" => {
                    timeout_ms = if flavor == SHIM_FLAVOR_MACHINE {
                        value.as_int().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| {
                            self.with_message(Trap::ValueError, "timeout must be an int (ms)")
                        })?
                    } else {
                        let seconds = self
                            .float_value(value)
                            .or_else(|| value.as_int().map(|n| n as f64))
                            .filter(|&t| t >= 0.0)
                            .ok_or_else(|| {
                                self.with_message(
                                    Trap::ValueError,
                                    "timeout must be a non-negative number of seconds",
                                )
                            })?;
                        (seconds * 1000.0) as u32
                    };
                }
                "timeout_char" => {
                    if value.as_int().is_none() {
                        let message = "timeout_char must be an int (ms)";
                        return Err(self.with_message(Trap::ValueError, message));
                    }
                }
                "receiver_buffer_size" => {
                    if value.as_int().is_none() {
                        return Err(Trap::TypeError);
                    }
                }
                "tx" => tx_arg = value,
                "rx" => rx_arg = value,
                other => {
                    let message =
                        alloc::format!("UART() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        let Some((tx_pin, rx_pin)) = self.board.uart_default_pins(instance) else {
            let message = "no UART with that id on this board";
            return Err(self.with_message(Trap::ValueError, message));
        };
        self.shim_check_pin(tx_arg, tx_pin, "tx")?;
        self.shim_check_pin(rx_arg, rx_pin, "rx")?;
        let resource = self.new_uart_resource(instance)?;
        let baud = Value::fixnum(config.baudrate as i32).ok_or(Trap::Overflow)?;
        let bits = Value::fixnum(config.data_bits as i32).ok_or(Trap::Overflow)?;
        let stop = Value::fixnum(config.stop_bits as i32).ok_or(Trap::Overflow)?;
        let parity = self.new_str(crate::uart::parity_name(config.parity))?;
        let pairs = [
            ("baudrate", baud),
            ("data_bits", bits),
            ("parity", parity),
            ("stop_bits", stop),
        ];
        let port = self.uart_open(&[resource], &pairs)?;
        let reference = self.alloc_object(self.uart_shim_type_id).ok_or(Trap::OutOfMemory)?;
        let base = reference.0;
        self.heap.write_u32(base + SHIM_W_PORT * 4, port.bits());
        self.heap.write_u32(base + SHIM_W_TIMEOUT_MS * 4, timeout_ms);
        self.heap.write_u32(base + SHIM_W_FLAVOR * 4, flavor);
        Ok(Value::from_ref(reference))
    }

    /// Dispatches a shim UART method: each translates onto the standard Port dispatch with the
    /// shim's held timeout, then re-wraps the result in the shimmed API's convention (`None`
    /// where the standard returns empty).
    pub(crate) fn call_uart_shim_method(
        &mut self,
        shim: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::uart::*;
        let port = self.shim_port(shim);
        let timeout_ms = self.shim_word(shim, SHIM_W_TIMEOUT_MS);
        let timeout = Value::fixnum(timeout_ms as i32).ok_or(Trap::Overflow)?;
        match method_id {
            SHIM_READ => {
                let result = self.port_dispatch(port, PORT_READ, args, timeout)?;
                self.shim_none_when_empty(result)
            }
            SHIM_READLINE => {
                let result = self.port_dispatch(port, PORT_READLINE, args, timeout)?;
                self.shim_none_when_empty(result)
            }
            SHIM_READINTO => {
                let result = self.port_dispatch(port, PORT_READINTO, args, timeout)?;
                if result.as_int() == Some(0) { Ok(Value::NONE) } else { Ok(result) }
            }
            SHIM_WRITE => self.port_dispatch(port, PORT_WRITE, args, Value::NONE),
            SHIM_ANY => self.port_dispatch(port, PORT_ANY, args, Value::NONE),
            SHIM_FLUSH => self.port_dispatch(port, PORT_FLUSH, args, Value::NONE),
            SHIM_RESET_INPUT => self.port_dispatch(port, PORT_DISCARD_INPUT, args, Value::NONE),
            SHIM_DEINIT => {
                self.port_close(port);
                Ok(Value::NONE)
            }
            SHIM_ENTER => Ok(shim),
            SHIM_EXIT => {
                self.port_close(port);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// The shimmed APIs return `None` where the standard returns `b""` -- the wrapping lives
    /// here, in the shim, never in the standard surface.
    fn shim_none_when_empty(&self, result: Value) -> Result<Value, Trap> {
        if self.bytes_value(result).is_some_and(<[u8]>::is_empty) {
            return Ok(Value::NONE);
        }
        Ok(result)
    }

    /// The host simulated register file (last value written per address) -- a test oracle.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn mmio_sim(&self) -> &alloc::collections::BTreeMap<u32, u32> {
        &self.mmio_sim
    }

    /// The host ordered log of every MMIO write -- the drive-sequence oracle for a test.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn mmio_trace(&self) -> &[(u32, u32)] {
        &self.mmio_trace
    }

    /// Installs the `sleep_ms` delay seam (device: a timer/spin). The host leaves it a no-op, so
    /// the differential is not slowed by real sleeps.
    pub fn set_delay(&mut self, delay: fn(u32)) {
        self.delay_fn = Some(delay);
    }

    /// `sleep_ms(ms)`: the installed delay on device, a no-op on the host.
    pub fn delay_ms(&mut self, ms: u32) {
        if let Some(delay) = self.delay_fn {
            delay(ms);
        }
    }

    /// Seeds the firmware-reserved pins (from the target profile) so an app claim of one fails
    /// loud -- never a silent app-vs-firmware register race.
    pub fn reserve_firmware_pins(&mut self, pins: &[u32]) {
        for &pin in pins {
            if !self.gpio_reserved.contains(&pin) {
                self.gpio_reserved.push(pin);
            }
        }
    }

    /// Claims `pin` for the app. Fails LOUD with `OSError` (`pin N in use` -- CPython's
    /// resource-busy flavor, like a socket bind on a taken address) if the pin is already
    /// app-claimed or firmware-reserved -- one owner per pin.
    fn claim_pin(&mut self, pin: u32) -> Result<(), Trap> {
        if self.gpio_reserved.contains(&pin) || self.gpio_claimed.contains(&pin) {
            let message = alloc::format!("pin {pin} in use");
            return Err(self.raise_named_exception("OSError", &message));
        }
        self.gpio_claimed.push(pin);
        Ok(())
    }

    /// Releases an app-claimed pin (`Pin.deinit`), so it can be opened again.
    fn release_pin(&mut self, pin: u32) {
        self.gpio_claimed.retain(|&p| p != pin);
    }

    /// The `gpio` module singleton (the clean hardware API). Bind it as the global `gpio` so a
    /// program reaches `gpio.output(...)`.
    pub fn gpio_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.gpio_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// The `board` pin-name singleton. Bind it as the global `board` so a program reaches
    /// `board.LED` etc.
    pub fn board_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.board_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `gpio` singleton.
    #[must_use]
    pub fn is_gpio(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.gpio_type_id)
    }

    /// Whether `value` is a `Pin`.
    #[must_use]
    pub fn is_pin(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.pin_type_id)
    }

    /// Allocates a `Pin` handle over the precomputed drive registers `regs` in `mode`, current
    /// output state low.
    fn new_pin(&mut self, mode: u32, regs: &crate::gpio::PinRegs) -> Result<Value, Trap> {
        use crate::gpio::*;
        let reference = self.alloc_object(self.pin_type_id).ok_or(Trap::OutOfMemory)?;
        let base = reference.0;
        self.heap.write_u32(base + PIN_W_ID * 4, regs.pin_id);
        self.heap.write_u32(base + PIN_W_SET_REG * 4, regs.set_reg);
        self.heap.write_u32(base + PIN_W_SET_VAL * 4, regs.set_val);
        self.heap.write_u32(base + PIN_W_CLR_REG * 4, regs.clr_reg);
        self.heap.write_u32(base + PIN_W_CLR_VAL * 4, regs.clr_val);
        self.heap.write_u32(base + PIN_W_READ_REG * 4, regs.read_reg);
        self.heap.write_u32(base + PIN_W_READ_MASK * 4, regs.read_mask);
        self.heap.write_u32(base + PIN_W_CUR * 4, 0);
        self.heap.write_u32(base + PIN_W_MODE * 4, mode);
        Ok(Value::from_ref(reference))
    }

    /// Replays one board setup [`RegOp`](crate::gpio::RegOp) over the MMIO seam: an outright write (an
    /// atomic set/clear alias, a function-select, a pad config), or a read-modify-write (a clock-enable
    /// bit, a two-bit direction field).
    fn apply_reg_op(&mut self, op: crate::gpio::RegOp) {
        use crate::gpio::RegOp;
        match op {
            RegOp::Write { reg, value } => self.mmio_write(reg, value),
            RegOp::SetBits { reg, set_mask } => {
                let value = self.mmio_read(reg) | set_mask;
                self.mmio_write(reg, value);
            }
            RegOp::ClearAndSet { reg, clear_mask, set_value } => {
                let value = (self.mmio_read(reg) & !clear_mask) | set_value;
                self.mmio_write(reg, value);
            }
        }
    }

    /// Opens `pin` in the given direction: claims it (fail-loud), brings the port up and sets the
    /// pin's direction by replaying the selected board's ordered setup ops (clock ungate / peripheral
    /// un-reset, pad, function-select, direction), and returns a `Pin`. Shared by the clean `gpio` API
    /// and its shims.
    fn open_pin(&mut self, pin: u32, output: bool) -> Result<Value, Trap> {
        use crate::gpio::{PIN_MODE_INPUT, PIN_MODE_OUTPUT};
        let board = self.board;
        if !board.gpio_supported() {
            return Err(self.with_message(Trap::Unsupported, "gpio is not supported on this board yet"));
        }
        if pin > board.max_pin() {
            return Err(Trap::ValueError);
        }
        self.claim_pin(pin)?;
        for op in board.open_ops(pin, output) {
            self.apply_reg_op(op);
        }
        let regs = board.pin_regs(pin);
        let mode = if output { PIN_MODE_OUTPUT } else { PIN_MODE_INPUT };
        self.new_pin(mode, &regs)
    }

    /// Dispatches a `gpio` method (`gpio.output(pin)` / `gpio.input(pin)`): opens the pin.
    fn call_gpio_method(
        &mut self,
        _gpio: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::gpio::{GPIO_INPUT, GPIO_OUTPUT};
        let pin = match args {
            [p] => {
                u32::try_from(p.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::ValueError)?
            }
            _ => return Err(Trap::TypeError),
        };
        let output = match method_id {
            GPIO_OUTPUT => true,
            GPIO_INPUT => false,
            _ => return Err(Trap::AttributeError),
        };
        self.open_pin(pin, output)
    }

    /// The `machine` module singleton. Bind it as the global `machine`.
    pub fn machine_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.machine_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A fresh `machine.Pin` factory (the callable class carrying OUT/IN + constructing pins).
    fn pin_factory_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.pin_factory_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `machine` singleton.
    #[must_use]
    pub fn is_machine(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.machine_type_id)
    }

    /// Whether `value` is a `machine.Pin` factory (a callable).
    #[must_use]
    pub fn is_pin_factory(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.pin_factory_type_id)
    }

    /// Constructs `machine.Pin(id[, mode])`: opens the pin and returns it. The
    /// result IS a clean gpio `Pin` -- `value`/`on`/`off`/`toggle` are the same methods.
    pub(crate) fn call_pin_factory(&mut self, args: &[Value]) -> Result<Value, Trap> {
        use crate::gpio::{MACHINE_PIN_IN, MACHINE_PIN_OUT};
        let (id, mode) = match args {
            [id] => (*id, MACHINE_PIN_OUT),
            [id, mode] => (
                *id,
                u32::try_from(mode.as_int().ok_or(Trap::TypeError)?).unwrap_or(MACHINE_PIN_IN),
            ),
            _ => return Err(Trap::TypeError),
        };
        let pin =
            u32::try_from(id.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::ValueError)?;
        self.open_pin(pin, mode == MACHINE_PIN_OUT)
    }

    /// The `digitalio` module singleton. Bind it as the global `digitalio`.
    pub fn digitalio_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.digitalio_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A fresh `digitalio.DigitalInOut` factory (callable).
    fn dio_factory_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.dio_factory_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A fresh `digitalio.Direction` enum singleton (OUTPUT/INPUT).
    fn direction_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.direction_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `digitalio` singleton.
    #[must_use]
    pub fn is_digitalio(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.digitalio_type_id)
    }

    /// Whether `value` is a `digitalio.DigitalInOut` factory (a callable).
    #[must_use]
    pub fn is_dio_factory(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.dio_factory_type_id)
    }

    /// Whether `value` is a `DigitalInOut` instance.
    #[must_use]
    fn is_dio(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.dio_type_id)
    }

    /// Constructs `digitalio.DigitalInOut(pin)`: opens the pin (input by default) and wraps it as a `DigitalInOut` whose `value`/`direction` are properties.
    pub(crate) fn call_dio_factory(&mut self, args: &[Value]) -> Result<Value, Trap> {
        let pin_id = match args {
            [p] => {
                u32::try_from(p.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::ValueError)?
            }
            _ => return Err(Trap::TypeError),
        };
        let pin = self.open_pin(pin_id, false)?;
        let reference = self.alloc_object(self.dio_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, pin.bits());
        Ok(Value::from_ref(reference))
    }

    /// The clean gpio `Pin` a `DigitalInOut` wraps.
    fn dio_pin(&self, dio: Value) -> Value {
        self.read_slot(dio, 0)
    }

    /// Reconfigures a `Pin`'s direction in place (replays the board's direction ops + rewrites the
    /// mode word); the one-time port bring-up (clock / un-reset, pad, function-select) already ran at
    /// open.
    fn set_pin_direction(&mut self, pin: Value, output: bool) -> Result<(), Trap> {
        use crate::gpio::{PIN_MODE_INPUT, PIN_MODE_OUTPUT, PIN_W_ID, PIN_W_MODE};
        let board = self.board;
        let reference = pin.as_ref().ok_or(Trap::TypeError)?;
        let pin_id = self.heap.read_u32(reference.0 + PIN_W_ID * 4);
        for op in board.direction_ops(pin_id, output) {
            self.apply_reg_op(op);
        }
        let mode = if output { PIN_MODE_OUTPUT } else { PIN_MODE_INPUT };
        self.heap.write_u32(reference.0 + PIN_W_MODE * 4, mode);
        Ok(())
    }

    /// `dio.value` (getattr): the pin's level as a `bool` -- the last driven value for an output,
    /// or the sampled input for an input.
    fn dio_read_value(&mut self, dio: Value) -> Result<Value, Trap> {
        use crate::gpio::*;
        let pin = self.dio_pin(dio);
        let reference = pin.as_ref().ok_or(Trap::TypeError)?;
        let mode = self.heap.read_u32(reference.0 + PIN_W_MODE * 4);
        let bit = if mode == PIN_MODE_OUTPUT {
            self.heap.read_u32(reference.0 + PIN_W_CUR * 4)
        } else {
            let read_reg = self.heap.read_u32(reference.0 + PIN_W_READ_REG * 4);
            let read_mask = self.heap.read_u32(reference.0 + PIN_W_READ_MASK * 4);
            u32::from(self.mmio_read(read_reg) & read_mask != 0)
        };
        Ok(Value::from_bool(bit != 0))
    }

    /// `dio.value = x` / `dio.direction = d` (attribute SET on a `DigitalInOut`); any other native
    /// object has no settable attribute (`AttributeError`, as before).
    pub(crate) fn py_setattr_native(
        &mut self,
        object: Value,
        name: &str,
        value: Value,
    ) -> Result<(), Trap> {
        use crate::gpio::{PIN_HIGH, PIN_LOW, PIN_MODE_OUTPUT};
        if self.is_dio(object) {
            let pin = self.dio_pin(object);
            match name {
                "value" => {
                    let high = value.as_int().map_or_else(|| value.is_truthy(), |n| n != 0);
                    self.call_pin_method(pin, if high { PIN_HIGH } else { PIN_LOW }, &[])?;
                    return Ok(());
                }
                "direction" => {
                    let output = value.as_int().unwrap_or(0) == i64::from(PIN_MODE_OUTPUT);
                    self.set_pin_direction(pin, output)?;
                    return Ok(());
                }
                _ => return Err(Trap::AttributeError),
            }
        }
        Err(Trap::AttributeError)
    }

    /// Dispatches a `Pin` method: `high`/`on` and `low`/`off` drive the output, `toggle` flips
    /// it, `value` reads (no argument) or writes (one), `read` samples the input, `deinit`
    /// releases the reservation. Each drive is a single register write through the MMIO seam.
    fn call_pin_method(
        &mut self,
        pin: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::gpio::*;
        let reference = pin.as_ref().ok_or(Trap::TypeError)?;
        let base = reference.0;
        let pin_id = self.heap.read_u32(base + PIN_W_ID * 4);
        let set_reg = self.heap.read_u32(base + PIN_W_SET_REG * 4);
        let set_val = self.heap.read_u32(base + PIN_W_SET_VAL * 4);
        let clr_reg = self.heap.read_u32(base + PIN_W_CLR_REG * 4);
        let clr_val = self.heap.read_u32(base + PIN_W_CLR_VAL * 4);
        let read_reg = self.heap.read_u32(base + PIN_W_READ_REG * 4);
        let read_mask = self.heap.read_u32(base + PIN_W_READ_MASK * 4);
        let cur = self.heap.read_u32(base + PIN_W_CUR * 4);
        match method_id {
            PIN_HIGH => {
                self.mmio_write(set_reg, set_val);
                self.heap.write_u32(base + PIN_W_CUR * 4, 1);
                Ok(Value::NONE)
            }
            PIN_LOW => {
                self.mmio_write(clr_reg, clr_val);
                self.heap.write_u32(base + PIN_W_CUR * 4, 0);
                Ok(Value::NONE)
            }
            PIN_TOGGLE => {
                if cur == 0 {
                    self.mmio_write(set_reg, set_val);
                    self.heap.write_u32(base + PIN_W_CUR * 4, 1);
                } else {
                    self.mmio_write(clr_reg, clr_val);
                    self.heap.write_u32(base + PIN_W_CUR * 4, 0);
                }
                Ok(Value::NONE)
            }
            PIN_VALUE => match args {
                [] => {
                    let bit = i32::from(self.mmio_read(read_reg) & read_mask != 0);
                    Value::fixnum(bit).ok_or(Trap::Overflow)
                }
                [x] => {
                    let high = x.as_int().ok_or(Trap::TypeError)? != 0;
                    if high {
                        self.mmio_write(set_reg, set_val);
                        self.heap.write_u32(base + PIN_W_CUR * 4, 1);
                    } else {
                        self.mmio_write(clr_reg, clr_val);
                        self.heap.write_u32(base + PIN_W_CUR * 4, 0);
                    }
                    Ok(Value::NONE)
                }
                _ => Err(Trap::TypeError),
            },
            PIN_READ => {
                let bit = i32::from(self.mmio_read(read_reg) & read_mask != 0);
                Value::fixnum(bit).ok_or(Trap::Overflow)
            }
            PIN_DEINIT => {
                self.release_pin(pin_id);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Whether `value` is a bound method (the callable a `str.method` reference produces).
    #[must_use]
    pub fn is_bound_method(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.bound_method_type_id)
    }

    /// Renders a `str.format(*args)` template: `{}` takes the next positional argument, `{N}` the
    /// N-th, and `{{` / `}}` are literal braces; each field is rendered with `str()` ([`display`]).
    /// An out-of-range index is an `IndexError`. A `{:spec}` field is applied through the format-spec
    /// mini-language below; named fields (`{name}`) are not supported.
    /// Renders `value` under a format spec `[[fill]align][sign][#][0][width][,][.prec][type]`. Supports
    /// the int presentation types (d/x/X/o/b/c), str (s), and the float types (f/F/e/E/g/G/%), plus
    /// alignment/width/fill/sign/zero-pad, str precision (truncation), and digit grouping (`,` / `_`).
    pub(crate) fn format_value_spec(&self, value: Value, spec: &str) -> Result<String, Trap> {
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        let (mut fill, mut align) = (' ', '\0');
        if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
            fill = chars[0];
            align = chars[1];
            i = 2;
        } else if chars.first().is_some_and(|c| matches!(c, '<' | '>' | '^' | '=')) {
            align = chars[0];
            i = 1;
        }
        let mut sign = '-';
        if chars.get(i).is_some_and(|c| matches!(c, '+' | '-' | ' ')) {
            sign = chars[i];
            i += 1;
        }
        let mut alternate = false;
        if chars.get(i) == Some(&'#') {
            alternate = true;
            i += 1;
        }
        if chars.get(i) == Some(&'0') {
            if align == '\0' {
                align = '=';
                fill = '0';
            }
            i += 1;
        }
        let mut width = 0usize;
        while chars.get(i).is_some_and(char::is_ascii_digit) {
            width = width * 10 + (chars[i] as usize - '0' as usize);
            i += 1;
        }
        let mut grouping = None;
        if chars.get(i).is_some_and(|c| matches!(c, ',' | '_')) {
            grouping = Some(chars[i]);
            i += 1;
        }
        let mut precision = None;
        if chars.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0usize;
            while chars.get(i).is_some_and(char::is_ascii_digit) {
                p = p * 10 + (chars[i] as usize - '0' as usize);
                i += 1;
            }
            precision = Some(p);
        }
        let type_char = chars.get(i).copied();
        if type_char.is_some() {
            i += 1;
        }
        if i != chars.len() {
            return Err(Trap::Unsupported);
        }

        let float_type = matches!(type_char, Some('f' | 'F' | 'e' | 'E' | 'g' | 'G' | '%'));
        if let Some(n) = value.as_int().filter(|_| !float_type) {
            let magnitude = n.unsigned_abs() as u32;
            let (digits, prefix) = match type_char.unwrap_or('d') {
                'd' | 'n' => (format_radix(magnitude, 10, false), ""),
                'x' => (format_radix(magnitude, 16, false), if alternate { "0x" } else { "" }),
                'X' => (format_radix(magnitude, 16, true), if alternate { "0X" } else { "" }),
                'o' => (format_radix(magnitude, 8, false), if alternate { "0o" } else { "" }),
                'b' => (format_radix(magnitude, 2, false), if alternate { "0b" } else { "" }),
                'c' => {
                    let cp = u32::try_from(n).map_err(|_| Trap::ValueError)?;
                    let ch = char::from_u32(cp).ok_or(Trap::ValueError)?;
                    let mut buf = [0u8; 4];
                    let align = if align == '\0' { '<' } else { align };
                    return Ok(pad_field(ch.encode_utf8(&mut buf), width, fill, align));
                }
                _ => return Err(Trap::Unsupported),
            };
            let digits = match grouping {
                Some(separator) => group_integer_digits(&digits, separator),
                None => digits,
            };
            let sign_str = if n < 0 {
                "-"
            } else {
                match sign {
                    '+' => "+",
                    ' ' => " ",
                    _ => "",
                }
            };
            let align = if align == '\0' { '>' } else { align };
            if align == '=' {
                let head = sign_str.chars().count() + prefix.chars().count() + digits.chars().count();
                let mut out = String::new();
                out.push_str(sign_str);
                out.push_str(prefix);
                (0..width.saturating_sub(head)).for_each(|_| out.push(fill));
                out.push_str(&digits);
                return Ok(out);
            }
            Ok(pad_field(&alloc::format!("{sign_str}{prefix}{digits}"), width, fill, align))
        } else if let Some(s) = self.str_value(value) {
            if !matches!(type_char, None | Some('s')) {
                return Err(Trap::Unsupported);
            }
            let body: String = match precision {
                Some(p) => s.chars().take(p).collect(),
                None => String::from(s),
            };
            let align = if align == '\0' { '<' } else { align };
            Ok(pad_field(&body, width, fill, align))
        } else if let Some(f) = self.float_value(value).or_else(|| value.as_int().map(|n| n as f64)) {
            let negative = f < 0.0 || (f == 0.0 && f.is_sign_negative());
            let magnitude = if negative { -f } else { f };
            let upper = matches!(type_char, Some('E' | 'F' | 'G'));
            let mut body = if magnitude.is_nan() {
                String::from(if upper { "NAN" } else { "nan" })
            } else if magnitude.is_infinite() {
                String::from(if upper { "INF" } else { "inf" })
            } else {
                match type_char {
                    Some('f' | 'F') => alloc::format!("{magnitude:.*}", precision.unwrap_or(6)),
                    Some('e' | 'E') => float_format_scientific(magnitude, precision.unwrap_or(6), upper),
                    Some('g' | 'G') => {
                        float_format_general(magnitude, precision.unwrap_or(6), upper, alternate, false)
                    }
                    Some('%') => alloc::format!("{:.*}%", precision.unwrap_or(6), magnitude * 100.0),
                    None => match precision {
                        None => format_float(magnitude),
                        Some(p) => float_format_general(magnitude, p, false, alternate, true),
                    },
                    _ => return Err(Trap::Unsupported),
                }
            };
            if let Some(separator) = grouping {
                if magnitude.is_finite() {
                    body = group_integer_digits(&body, separator);
                }
            }
            let sign_str = if negative {
                "-"
            } else {
                match sign {
                    '+' => "+",
                    ' ' => " ",
                    _ => "",
                }
            };
            let align = if align == '\0' { '>' } else { align };
            if align == '=' {
                let head = sign_str.chars().count() + body.chars().count();
                let mut out = String::from(sign_str);
                (0..width.saturating_sub(head)).for_each(|_| out.push(fill));
                out.push_str(&body);
                return Ok(out);
            }
            Ok(pad_field(&alloc::format!("{sign_str}{body}"), width, fill, align))
        } else {
            Err(Trap::Unsupported)
        }
    }

    /// Applies a `str.format` field conversion (`!r`/`!s`/`!a`) to `value`, producing a string value;
    /// `None` returns it unchanged. `!r` and `!a` use `repr` (a close approximation of `ascii()` for
    /// the ASCII content this subset handles), `!s` uses `str`. An unknown conversion is a `ValueError`.
    fn apply_conversion(&mut self, value: Value, conversion: Option<&str>) -> Result<Value, Trap> {
        match conversion {
            None => Ok(value),
            Some("r" | "a") => {
                let s = self.repr(value);
                self.new_str(&s)
            }
            Some("s") => {
                let s = self.display(value);
                self.new_str(&s)
            }
            Some(_) => Err(Trap::ValueError),
        }
    }

    pub(crate) fn format_template(
        &mut self,
        template: &str,
        args: &[Value],
        kwargs: &[(&str, Value)],
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<String, Trap> {
        let mut out = String::new();
        let mut chars = template.chars().peekable();
        let mut auto_index = 0usize;
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '{' => {
                    let mut field = String::new();
                    let mut brace_depth = 1usize;
                    let mut closed = false;
                    for fc in chars.by_ref() {
                        match fc {
                            '{' => brace_depth += 1,
                            '}' => {
                                brace_depth -= 1;
                                if brace_depth == 0 {
                                    closed = true;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        field.push(fc);
                    }
                    if !closed {
                        return Err(Trap::Unsupported);
                    }
                    let (name_conv, spec) = match field.split_once(':') {
                        Some((nc, s)) => (nc, Some(s)),
                        None => (field.as_str(), None),
                    };
                    let (name, conversion) = match name_conv.split_once('!') {
                        Some((n, c)) => (n, Some(c)),
                        None => (name_conv, None),
                    };
                    let arg = self.resolve_field_arg(name, args, kwargs, &mut auto_index)?;
                    let arg = self.apply_conversion(arg, conversion)?;
                    let resolved = match spec {
                        None => String::new(),
                        Some(spec) => self.resolve_nested_spec(spec, args, kwargs, &mut auto_index)?,
                    };
                    if let Some(method) = self.find_dunder(arg, "__format__") {
                        let spec_value = self.new_str(&resolved)?;
                        let result =
                            crate::interp::call_value(method, &[spec_value], functions, self, depth)?;
                        let text = self.str_value(result).ok_or(Trap::TypeError)?;
                        out.push_str(text);
                    } else if spec.is_none() {
                        out.push_str(&self.display(arg));
                    } else {
                        out.push_str(&self.format_value_spec(arg, &resolved)?);
                    }
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '}' => return Err(Trap::Unsupported),
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// Resolves a format field NAME to its argument: an empty name is the next auto-numbered
    /// positional (`{}`), digits an explicit positional (`{0}`), otherwise a keyword (`{name}`, a
    /// `KeyError` if absent). `auto_index` advances only for an empty name, and is shared across the
    /// template and any nested specs so auto-numbering stays sequential.
    fn resolve_field_arg(
        &mut self,
        name: &str,
        args: &[Value],
        kwargs: &[(&str, Value)],
        auto_index: &mut usize,
    ) -> Result<Value, Trap> {
        let base_end = name.find(['.', '[']).unwrap_or(name.len());
        let base = &name[..base_end];
        let value = if base.is_empty() {
            let i = *auto_index;
            *auto_index += 1;
            args.get(i).copied().ok_or(Trap::IndexError)?
        } else if let Ok(index) = base.parse::<usize>() {
            args.get(index).copied().ok_or(Trap::IndexError)?
        } else if let Some(&(_, v)) = kwargs.iter().find(|(k, _)| *k == base) {
            v
        } else {
            let key = self.new_str(base)?;
            self.set_trap_arg(key);
            return Err(Trap::KeyError);
        };
        self.apply_field_accessors(value, &name[base_end..])
    }

    /// Walks a format field's accessor chain (`.attr` / `[key]`) after the base argument. A bracket
    /// key of all digits is an INTEGER index (`{0[1]}` -> list element 1); any other key is a string
    /// (`{0[name]}` -> mapping lookup), matching str.format. An attribute reads via the object model.
    fn apply_field_accessors(&mut self, mut value: Value, mut rest: &str) -> Result<Value, Trap> {
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('.') {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                let mut cache = InlineCache::empty();
                value = self.getattr(value, &after[..end], &mut cache)?;
                rest = &after[end..];
            } else if let Some(after) = rest.strip_prefix('[') {
                let close = after.find(']').ok_or(Trap::Unsupported)?;
                let key = &after[..close];
                let index = if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
                    Value::fixnum(key.parse::<i32>().map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?
                } else {
                    self.new_str(key)?
                };
                value = self.py_getitem(value, index)?;
                rest = &after[close + 1..];
            } else {
                return Err(Trap::Unsupported);
            }
        }
        Ok(value)
    }

    /// Resolves the replacement fields inside a format SPEC one nesting level deep -- `str.format`
    /// allows a field's spec to reference other arguments (`"{x:>{w}}".format(x="hi", w=8)` -> the
    /// `{w}` becomes `8`, then `>8` right-aligns). A nested field is a plain `[name]` with no spec or
    /// conversion of its own (PEP 3101 permits only one level); `{{`/`}}` still escape. Shares
    /// `auto_index` with the enclosing template.
    fn resolve_nested_spec(
        &mut self,
        spec: &str,
        args: &[Value],
        kwargs: &[(&str, Value)],
        auto_index: &mut usize,
    ) -> Result<String, Trap> {
        if !spec.contains('{') {
            return Ok(String::from(spec));
        }
        let mut out = String::new();
        let mut chars = spec.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for fc in chars.by_ref() {
                        if fc == '}' {
                            closed = true;
                            break;
                        }
                        name.push(fc);
                    }
                    if !closed {
                        return Err(Trap::Unsupported);
                    }
                    let arg = self.resolve_field_arg(&name, args, kwargs, auto_index)?;
                    out.push_str(&self.display(arg));
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '}' => return Err(Trap::Unsupported),
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// `str.format_map(mapping)`: renders `template`, resolving each `{name}` field by looking the
    /// name up (as a str key) in `mapping` (a dict). A missing key is a `KeyError`; a `:spec` is not
    /// yet supported. `{{`/`}}` escape to a literal brace.
    fn format_with_map(&self, template: &str, mapping: Value) -> Result<String, Trap> {
        let entries = self.dict_value(mapping).ok_or(Trap::TypeError)?.clone();
        let mut out = String::new();
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '{' => {
                    let mut field = String::new();
                    let mut closed = false;
                    for fc in chars.by_ref() {
                        if fc == '}' {
                            closed = true;
                            break;
                        }
                        field.push(fc);
                    }
                    if !closed {
                        return Err(Trap::Unsupported);
                    }
                    let (name, spec) = match field.split_once(':') {
                        Some((n, s)) => (n, Some(s)),
                        None => (field.as_str(), None),
                    };
                    let value = entries
                        .iter()
                        .find_map(|(k, v)| (self.str_value(*k) == Some(name)).then_some(*v))
                        .ok_or(Trap::KeyError)?;
                    match spec {
                        None => out.push_str(&self.display(value)),
                        Some(spec) => out.push_str(&self.format_value_spec(value, spec)?),
                    }
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '}' => return Err(Trap::Unsupported),
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// Whether `method_id` is a builtin-value dunder wrapper (routes to `dispatch_builtin_dunder`).
    pub(crate) fn is_builtin_dunder_method(&self, method_id: u32) -> bool {
        dunder_name_of(method_id).is_some()
    }

    /// Whether builtin VALUE `obj` exposes dunder `name` (so getattr returns a bound method-wrapper
    /// and `hasattr` reports it). Gated per value-type to match CPython.
    fn builtin_supports_dunder(&self, obj: Value, name: &str) -> bool {
        if self.is_int(obj) {
            int_supports_dunder(name)
        } else if self.is_float(obj) {
            float_supports_dunder(name)
        } else {
            self.container_supports_dunder(obj, name)
        }
    }

    /// The container/sequence-protocol dunders a builtin container/str value exposes (`__len__`,
    /// `__getitem__`, `__iter__`, `__contains__`, and `__setitem__`/`__delitem__` on the mutable
    /// ones). Gated by the concrete type so `hasattr` is exact -- e.g. a tuple has no `__setitem__`.
    fn container_supports_dunder(&self, obj: Value, name: &str) -> bool {
        let subscriptable = self.is_str(obj)
            || self.byte_view(obj).is_some()
            || self.seq_value(obj).is_some()
            || self.is_dict(obj)
            || self.is_range(obj);
        let iterable = subscriptable
            || self.is_set(obj)
            || self.is_frozenset(obj)
            || self.is_deque(obj)
            || self.is_dict_view(obj);
        let mutable_item = self.is_list(obj) || self.is_dict(obj) || self.is_bytearray(obj);
        let sequence =
            self.is_str(obj) || self.byte_view(obj).is_some() || self.seq_value(obj).is_some();
        match name {
            "__len__" => self.py_len(obj).is_ok(),
            "__getitem__" => subscriptable,
            "__setitem__" | "__delitem__" => mutable_item,
            "__contains__" => iterable,
            "__iter__" => iterable,
            "__add__" | "__mul__" | "__rmul__" => sequence,
            "__eq__" | "__ne__" | "__lt__" | "__le__" | "__gt__" | "__ge__" => {
                self.is_str(obj)
                    || self.byte_view(obj).is_some()
                    || self.seq_value(obj).is_some()
                    || self.is_dict(obj)
                    || self.is_set(obj)
                    || self.is_frozenset(obj)
                    || self.is_range(obj)
                    || self.is_deque(obj)
            }
            "__and__" | "__or__" | "__xor__" | "__sub__" | "__rand__" | "__ror__" | "__rxor__"
            | "__rsub__" => self.is_set(obj) || self.is_frozenset(obj),
            _ => false,
        }
    }

    /// Whether a COMPARISON dunder on `receiver` acts on `other` directly, or defers with
    /// NotImplemented. A builtin container compares only with its own kind: `[1].__eq__((1,))` is
    /// NotImplemented, not False -- the False a program sees from `==` comes later, after BOTH
    /// sides have declined and the default identity comparison answers.
    /// The comparison a builtin value's exposed dunder performs, when it is one. The interpreter
    /// uses this to route `[1].__eq__([1])` through the FULL comparison path rather than the
    /// model-only one -- comparing two lists may have to call an element's own `__eq__`, which
    /// needs the driver.
    pub(crate) fn builtin_dunder_comparison(&self, method_id: u32) -> Option<CmpOp> {
        cmpop_of_dunder(dunder_name_of(method_id)?)
    }

    pub(crate) fn comparison_dunder_accepts(&self, receiver: Value, other: Value) -> bool {
        if self.is_int(receiver) || self.is_float(receiver) {
            return self.numeric_dunder_accepts(receiver, other);
        }
        if self.is_str(receiver) {
            return self.is_str(other);
        }
        if self.is_bytes(receiver) {
            return self.is_bytes(other);
        }
        if self.is_bytearray(receiver) {
            return self.is_bytearray(other);
        }
        if self.is_list(receiver) {
            return self.is_list(other);
        }
        if self.is_tuple(receiver) {
            return self.is_tuple(other);
        }
        if self.is_dict(receiver) {
            return self.is_dict(other);
        }
        if self.is_set(receiver) || self.is_frozenset(receiver) {
            return self.is_set(other) || self.is_frozenset(other);
        }
        if self.is_range(receiver) {
            return self.is_range(other);
        }
        if self.is_deque(receiver) {
            return self.is_deque(other);
        }
        false
    }

    /// Whether a numeric dunder on `receiver` handles `other` DIRECTLY, else it is `NotImplemented`
    /// (deferring to the other operand's reflected dunder). Matches CPython: an int/bool dunder
    /// handles only int/bool operands (`(5).__add__(2.5)` is NotImplemented); a float dunder also
    /// handles a float (`(2.5).__add__(5)` is 7.5).
    fn numeric_dunder_accepts(&self, receiver: Value, other: Value) -> bool {
        if self.is_int(other) {
            true
        } else if self.is_float(other) {
            self.is_float(receiver)
        } else {
            false
        }
    }

    /// The full `a OP b` result -- the str/seq operators then the numeric path, exactly as
    /// [`crate::interp::dispatch_binary`] composes them. Used by the arithmetic dunders.
    fn full_binary(&mut self, op: BinOp, a: Value, b: Value) -> Result<Value, Trap> {
        match self.py_binary(op, a, b) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => crate::interp::binary(op, a, b, self),
            Err(other) => Err(other),
        }
    }

    /// `int(number)`: an int is itself (a bool normalizes to 1/0); a float truncates toward zero
    /// (NaN/inf error, magnitude beyond i128 overflows), matching the `int` builtin.
    fn number_to_int(&mut self, value: Value) -> Result<Value, Trap> {
        if value == Value::TRUE {
            return Value::fixnum(1).ok_or(Trap::Overflow);
        }
        if value == Value::FALSE {
            return Value::fixnum(0).ok_or(Trap::Overflow);
        }
        if self.is_int(value) {
            return Ok(value);
        }
        let Some(f) = self.float_value(value) else {
            return Err(Trap::TypeError);
        };
        if f.is_nan() {
            return Err(self.with_message(Trap::ValueError, "cannot convert float NaN to integer"));
        }
        if f.is_infinite() {
            return Err(self.with_message(Trap::Overflow, "cannot convert float infinity to integer"));
        }
        if !(-1.701_411_834_604_692_3e38..1.701_411_834_604_692_3e38).contains(&f) {
            return Err(Trap::Overflow);
        }
        self.new_long(f as i128)
    }

    /// Runs a builtin value's dunder (a reserved id from [`builtin_dunder_id`]). A binary/comparison
    /// dunder returns `NotImplemented` for an operand its type does not handle (CPython:
    /// `(2).__add__("s")` is NotImplemented, not the `2 + "s"` TypeError); the rest act directly.
    fn dispatch_builtin_dunder(&mut self, receiver: Value, id: u32, args: &[Value]) -> Result<Value, Trap> {
        let name = dunder_name_of(id).ok_or(Trap::Malformed)?;
        if let Some((op, reflected)) = binop_of_dunder(name) {
            let [other] = args else {
                return Err(Trap::TypeError);
            };
            let (a, b) = if reflected { (*other, receiver) } else { (receiver, *other) };
            if !self.is_int(receiver) && !self.is_float(receiver) {
                return match self.full_binary(op, a, b) {
                    Ok(value) => Ok(value),
                    Err(Trap::TypeError) if matches!(name, "__mul__" | "__rmul__") => {
                        let message = alloc::format!(
                            "'{}' object cannot be interpreted as an integer",
                            self.type_name_of(*other)
                        );
                        Err(self.with_message(Trap::TypeError, &message))
                    }
                    Err(Trap::TypeError) => Err(self.binop_type_error(op, a, b)),
                    Err(other) => Err(other),
                };
            }
            if !self.numeric_dunder_accepts(receiver, *other) {
                return Ok(Value::NOT_IMPLEMENTED);
            }
            return match self.full_binary(op, a, b) {
                Ok(value) => Ok(value),
                Err(Trap::TypeError) => Ok(Value::NOT_IMPLEMENTED),
                Err(other) => Err(other),
            };
        }
        if let Some(op) = cmpop_of_dunder(name) {
            let [other] = args else {
                return Err(Trap::TypeError);
            };
            if !self.comparison_dunder_accepts(receiver, *other) {
                return Ok(Value::NOT_IMPLEMENTED);
            }
            if self.is_dict(receiver) && !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                return Ok(Value::NOT_IMPLEMENTED);
            }
            return match crate::interp::compare(op, receiver, *other, self) {
                Ok(value) => Ok(value),
                Err(Trap::TypeError) => Ok(Value::NOT_IMPLEMENTED),
                Err(other) => Err(other),
            };
        }
        match name {
            "__divmod__" | "__rdivmod__" => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                if !self.numeric_dunder_accepts(receiver, *other) {
                    return Ok(Value::NOT_IMPLEMENTED);
                }
                let (a, b) =
                    if name == "__rdivmod__" { (*other, receiver) } else { (receiver, *other) };
                match (self.full_binary(BinOp::FloorDiv, a, b), self.full_binary(BinOp::Mod, a, b)) {
                    (Ok(quotient), Ok(remainder)) => {
                        self.new_tuple(alloc::vec![quotient, remainder])
                    }
                    (Err(Trap::TypeError), _) | (_, Err(Trap::TypeError)) => {
                        Ok(Value::NOT_IMPLEMENTED)
                    }
                    (Err(other), _) | (_, Err(other)) => Err(other),
                }
            }
            "__neg__" => crate::interp::unary(UnaryOp::Neg, receiver, self),
            "__pos__" => crate::interp::unary(UnaryOp::Pos, receiver, self),
            "__invert__" => crate::interp::unary(UnaryOp::Invert, receiver, self),
            "__abs__" => {
                if let Some(f) = self.float_value(receiver) {
                    return self.new_float(f.abs());
                }
                let zero = Value::fixnum(0).ok_or(Trap::Overflow)?;
                if crate::interp::compare(CmpOp::Lt, receiver, zero, self)? == Value::TRUE {
                    crate::interp::unary(UnaryOp::Neg, receiver, self)
                } else {
                    self.number_to_int(receiver)
                }
            }
            "__int__" => self.number_to_int(receiver),
            "__float__" => {
                let f = self
                    .float_value(receiver)
                    .or_else(|| self.as_i128(receiver).map(|n| n as f64));
                self.new_float(f.ok_or(Trap::TypeError)?)
            }
            "__bool__" => Ok(Value::from_bool(self.py_truthy(receiver)?.unwrap_or(true))),
            "__hash__" => self.py_hash(receiver),
            "__repr__" => {
                let rendered = self.repr(receiver);
                self.new_str(&rendered)
            }
            "__str__" => {
                let rendered = self.display(receiver);
                self.new_str(&rendered)
            }
            "__len__" => self.py_len(receiver),
            "__getitem__" => {
                let [index] = args else {
                    return Err(Trap::TypeError);
                };
                self.py_getitem(receiver, *index)
            }
            "__setitem__" => {
                let [index, value] = args else {
                    return Err(Trap::TypeError);
                };
                self.py_setitem(receiver, *index, *value)?;
                Ok(Value::NONE)
            }
            "__delitem__" => {
                let [index] = args else {
                    return Err(Trap::TypeError);
                };
                self.py_delitem(receiver, *index)?;
                Ok(Value::NONE)
            }
            "__contains__" => {
                let [element] = args else {
                    return Err(Trap::TypeError);
                };
                Ok(Value::from_bool(self.py_contains(receiver, *element)?))
            }
            "__iter__" => self.new_iter(receiver),
            _ => Err(Trap::Malformed),
        }
    }

    /// Calls a bound method -- the `Call` dispatch when [`ObjectModel::is_bound_method`]. Reads the
    /// stored `[receiver, method_id]` and runs the receiver's method: a builtin value's exposed
    /// dunder, list/dict/set/tuple/gpio/pin methods, else a `str` method (Python 3.14.6 "String
    /// Methods"). A wrong argument count, or a wrong-typed argument, is a `TypeError`.
    pub fn call_bound_method(&mut self, callee: Value, args: &[Value]) -> Result<Value, Trap> {
        let reference = callee.as_ref().ok_or(Trap::TypeError)?;
        let receiver = Value::from_bits(self.heap.read_u32(reference.0));
        let method_id = self.heap.read_u32(reference.0 + 4);
        if method_id == EXC_INIT && self.is_instance(receiver) {
            self.init_default_args(receiver, args)?;
            return Ok(Value::NONE);
        }
        if method_id == EXC_INIT_UNBOUND {
            let (instance, rest) = args.split_first().ok_or(Trap::TypeError)?;
            self.init_default_args(*instance, rest)?;
            return Ok(Value::NONE);
        }
        if method_id == OBJECT_GETSTATE {
            if !args.is_empty() {
                let message = "__getstate__() takes no arguments";
                return Err(self.raise_named_exception("TypeError", message));
            }
            return self.object_getstate(receiver);
        }
        if method_id == OBJECT_NEW {
            let (class, rest) = match args.split_first() {
                Some(split) => split,
                None => {
                    let message = "object.__new__(): not enough arguments";
                    return Err(self.raise_named_exception("TypeError", message));
                }
            };
            return self.object_new(*class, rest, &[]);
        }
        if method_id == OBJECT_NOOP {
            return Ok(Value::NONE);
        }
        if dunder_name_of(method_id).is_some() {
            return self.dispatch_builtin_dunder(receiver, method_id, args);
        }
        if self.is_file(receiver) {
            return self.call_file_method(receiver, method_id, args);
        }
        if self.is_list(receiver) {
            return self.call_list_method(receiver, method_id, args);
        }
        if self.is_dict(receiver) {
            return self.call_dict_method(receiver, method_id, args);
        }
        if self.is_set(receiver) || self.is_frozenset(receiver) {
            return self.call_set_method(receiver, method_id, args);
        }
        if self.is_ntinstance(receiver) {
            return self.call_nt_method(receiver, method_id, args);
        }
        if self.is_tuple(receiver) {
            return self.call_tuple_method(receiver, method_id, args);
        }
        #[cfg(feature = "complex")]
        if self.is_complex(receiver) {
            return self.call_complex_method(receiver, method_id, args);
        }
        if self.is_bytes(receiver) || self.is_bytearray(receiver) {
            return self.call_bytes_method(receiver, method_id, args);
        }
        if self.is_memoryview(receiver) {
            return self.call_memoryview_method(receiver, method_id, args);
        }
        if self.is_slice(receiver) {
            return self.call_slice_method(receiver, method_id, args);
        }
        if self.is_int(receiver) {
            return self.call_int_method(receiver, method_id, args);
        }
        if self.is_float(receiver) {
            return self.call_float_method(receiver, method_id, args);
        }
        if self.is_property(receiver) {
            return self.call_property_method(receiver, method_id, args);
        }
        if self.is_gpio(receiver) {
            return self.call_gpio_method(receiver, method_id, args);
        }
        if self.is_pin(receiver) {
            return self.call_pin_method(receiver, method_id, args);
        }
        if self.is_uart(receiver) {
            return self.call_uart_method(receiver, method_id, args);
        }
        if self.is_uart_port(receiver) {
            return self.call_port_method(receiver, method_id, args);
        }
        if self.is_spi(receiver) {
            return self.call_spi_method(receiver, method_id, args);
        }
        if self.is_spi_bus(receiver) {
            return self.call_spi_bus_method(receiver, method_id, args);
        }
        if self.is_i2c(receiver) {
            return self.call_i2c_method(receiver, method_id, args);
        }
        if self.is_i2c_bus(receiver) {
            return self.call_i2c_bus_method(receiver, method_id, args);
        }
        if self.is_adc(receiver) {
            return self.call_adc_method(receiver, method_id, args);
        }
        if self.is_adc_channel(receiver) {
            return self.call_adc_channel_method(receiver, method_id, args);
        }
        if self.is_uart_shim(receiver) {
            return self.call_uart_shim_method(receiver, method_id, args);
        }
        if self.is_spi_shim(receiver) {
            return self.call_spi_shim_method(receiver, method_id, args, &[]);
        }
        if self.is_i2c_shim(receiver) {
            return self.call_i2c_shim_method(receiver, method_id, args, &[]);
        }
        if self.is_adc_shim(receiver) {
            return self.call_adc_shim_method(receiver, method_id, args);
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
            STR_FORMAT => {
                let template = self.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
                let rendered = self.format_template(&template, args, &[], &[], 0)?;
                self.new_str(&rendered)
            }
            STR_FORMAT_MAP => {
                let [mapping] = args else {
                    return Err(Trap::TypeError);
                };
                let template = self.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
                let rendered = self.format_with_map(&template, *mapping)?;
                self.new_str(&rendered)
            }
            STR_RSPLIT => {
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
                    None if maxsplit < 0 => s.split_whitespace().map(String::from).collect(),
                    None => rsplit_whitespace_maxsplit(&s, maxsplit as usize),
                    Some(sep) => {
                        if sep.is_empty() {
                            return Err(Trap::ValueError);
                        }
                        let all: Vec<&str> = s.split(sep.as_str()).collect();
                        if maxsplit < 0 || all.len() <= maxsplit as usize + 1 {
                            all.into_iter().map(String::from).collect()
                        } else {
                            let keep_from = all.len() - maxsplit as usize;
                            let head = all[..keep_from].join(sep.as_str());
                            let mut parts = alloc::vec![head];
                            parts.extend(all[keep_from..].iter().map(|p| String::from(*p)));
                            parts
                        }
                    }
                };
                let mut elems = Vec::with_capacity(parts.len());
                for p in &parts {
                    elems.push(self.new_str(p)?);
                }
                self.new_list(elems)
            }
            STR_CASEFOLD => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let folded = self.str_value(receiver).ok_or(Trap::TypeError)?.to_lowercase();
                self.new_str(&folded)
            }
            STR_ENCODE => {
                let name = match args {
                    [] => String::from("utf8"),
                    [encoding] => self.str_value(*encoding).map(String::from).ok_or(Trap::TypeError)?,
                    _ => return Err(Trap::TypeError),
                };
                let text = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                match name.to_ascii_lowercase().replace('-', "").as_str() {
                    "utf8" | "ascii" => self.new_bytes(text.into_bytes()),
                    _ => Err(Trap::Unsupported),
                }
            }
            STR_TRANSLATE => {
                let [table] = args else {
                    return Err(Trap::TypeError);
                };
                let entries = self.dict_value(*table).ok_or(Trap::TypeError)?.clone();
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut out = String::new();
                for ch in s.chars() {
                    let ordinal = ch as i64;
                    let replacement = entries
                        .iter()
                        .find_map(|(k, v)| (k.as_int() == Some(ordinal)).then_some(*v));
                    match replacement {
                        None => out.push(ch),
                        Some(r) if r.is_none() => {}
                        Some(r) => {
                            if let Some(ord) = r.as_int() {
                                let cp = u32::try_from(ord).map_err(|_| Trap::ValueError)?;
                                out.push(char::from_u32(cp).ok_or(Trap::ValueError)?);
                            } else if let Some(repl) = self.str_value(r) {
                                out.push_str(repl);
                            } else {
                                return Err(Trap::TypeError);
                            }
                        }
                    }
                }
                self.new_str(&out)
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
                    return Err(self.with_message(Trap::ValueError, "substring not found"));
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
            | STR_ISDECIMAL | STR_ISNUMERIC | STR_ISASCII | STR_ISIDENTIFIER | STR_ISTITLE
            | STR_ISPRINTABLE => {
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
                    None if maxsplit < 0 => s.split_whitespace().map(String::from).collect(),
                    None => split_whitespace_maxsplit(&s, maxsplit as usize),
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
    /// `list.index` / `tuple.index`: the first index in `[start, stop)` whose element equals the first
    /// argument (negative bounds count from the end, clamped), else `ValueError`. Args are
    /// `(value[, start[, stop]])`; `slot` is the sequence's arena slot.
    fn seq_index(&self, slot: usize, args: &[Value]) -> Result<Value, Trap> {
        let len = self.seqs[slot].len() as i64;
        let clamp = |i: i64| (if i < 0 { i + len } else { i }).clamp(0, len) as usize;
        let (value, lo, hi) = match args {
            [v] => (*v, 0usize, len as usize),
            [v, s] => (*v, clamp(s.as_int().ok_or(Trap::TypeError)?), len as usize),
            [v, s, e] => (
                *v,
                clamp(s.as_int().ok_or(Trap::TypeError)?),
                clamp(e.as_int().ok_or(Trap::TypeError)?),
            ),
            _ => return Err(Trap::TypeError),
        };
        let hi = hi.max(lo);
        match self.seqs[slot][lo..hi].iter().position(|e| self.key_eq(*e, value)) {
            Some(p) => Value::fixnum((p + lo) as i32).ok_or(Trap::Overflow),
            None => Err(Trap::ValueError),
        }
    }

    /// The `__eq__`-dependent list methods (`count`/`index`/`remove`), dispatched interp-aware so a
    /// custom element `__eq__` decides membership -- mirroring the set/dict/deque `_dyn` seams. The
    /// arena-free methods (append/pop/sort/...) fall through to the model-only `call_bound_method`.
    pub(crate) fn call_list_method_dyn(
        &mut self,
        callee: Value,
        list: Value,
        method_id: u32,
        args: &[Value],
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        let slot = match self.container_slot(list, self.list_type_id) {
            Some(slot) => slot,
            None => return self.call_bound_method(callee, args),
        };
        match method_id {
            LIST_COUNT => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let elements = self.seqs[slot].clone();
                let mut count = 0i32;
                for element in elements {
                    if crate::interp::elem_eq(*value, element, functions, self, depth)? {
                        count = count.checked_add(1).ok_or(Trap::Overflow)?;
                    }
                }
                Value::fixnum(count).ok_or(Trap::Overflow)
            }
            LIST_INDEX => {
                let elements = self.seqs[slot].clone();
                let len = elements.len() as i64;
                let clamp = |i: i64| (if i < 0 { i + len } else { i }).clamp(0, len) as usize;
                let (value, lo, hi) = match args {
                    [v] => (*v, 0usize, len as usize),
                    [v, s] => (*v, clamp(s.as_int().ok_or(Trap::TypeError)?), len as usize),
                    [v, s, e] => (
                        *v,
                        clamp(s.as_int().ok_or(Trap::TypeError)?),
                        clamp(e.as_int().ok_or(Trap::TypeError)?),
                    ),
                    _ => return Err(Trap::TypeError),
                };
                let hi = hi.max(lo);
                for (offset, element) in elements[lo..hi].iter().enumerate() {
                    if crate::interp::elem_eq(value, *element, functions, self, depth)? {
                        return Value::fixnum((lo + offset) as i32).ok_or(Trap::Overflow);
                    }
                }
                Err(self.with_message(Trap::ValueError, "list.index(x): x not in list"))
            }
            LIST_REMOVE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let elements = self.seqs[slot].clone();
                let mut found = None;
                for (position, element) in elements.iter().enumerate() {
                    if crate::interp::elem_eq(*value, *element, functions, self, depth)? {
                        found = Some(position);
                        break;
                    }
                }
                match found {
                    Some(position) => {
                        self.seqs[slot].remove(position);
                        Ok(Value::NONE)
                    }
                    None => Err(self.with_message(Trap::ValueError, "list.remove(x): x not in list")),
                }
            }
            LIST_EXTEND => {
                let [iterable] = args else {
                    return Err(Trap::TypeError);
                };
                let items = crate::builtins::collect_iterable(self, &[*iterable], functions, depth)?;
                let slot = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
                self.seqs[slot].extend(items);
                Ok(Value::NONE)
            }
            _ => self.call_bound_method(callee, args),
        }
    }

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
            LIST_INDEX => match self.seq_index(index, args) {
                Err(Trap::ValueError) => {
                    Err(self.with_message(Trap::ValueError, "list.index(x): x not in list"))
                }
                other => other,
            },
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

    /// Dispatches a `dict` method: `get(k[, default])` (no KeyError), and `keys`/`values`/`items`
    /// returning live view objects (`dict_keys`/`dict_values`/`dict_items`, as CPython does).
    fn call_dict_method(&mut self, dict: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.dict_slot(dict).ok_or(Trap::TypeError)?;
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
            DICT_KEYS => self.new_dict_view(dict, DictViewKind::Keys),
            DICT_VALUES => self.new_dict_view(dict, DictViewKind::Values),
            DICT_ITEMS => self.new_dict_view(dict, DictViewKind::Items),
            DICT_UPDATE => {
                let other = match args {
                    [] => return Ok(Value::NONE),
                    [other] => other,
                    _ => return Err(Trap::TypeError),
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
                if let Some(factory) = self.defaultdict_factory(dict) {
                    return self.new_defaultdict(factory, copy);
                }
                if self.is_counter(dict) {
                    return self.new_counter(copy);
                }
                if self.is_ordereddict(dict) {
                    return self.new_ordereddict(copy);
                }
                self.new_dict(copy)
            }
            DICT_POPITEM => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                match self.dicts[index].pop() {
                    Some((key, value)) => self.new_tuple(alloc::vec![key, value]),
                    None => {
                        let message = self.new_str("popitem(): dictionary is empty")?;
                        self.set_trap_arg(message);
                        Err(Trap::KeyError)
                    }
                }
            }
            COUNTER_MOST_COMMON => {
                let n = match args {
                    [] => None,
                    [n] if n.is_none() => None,
                    [n] => Some(n.as_int().ok_or(Trap::TypeError)?),
                    _ => return Err(Trap::TypeError),
                };
                let entries = self.counter_display_entries(self.dicts[index].clone());
                let take = match n {
                    Some(n) if n >= 0 => (n as usize).min(entries.len()),
                    Some(_) => 0,
                    None => entries.len(),
                };
                let mut items = Vec::with_capacity(take);
                for &(key, count) in entries.iter().take(take) {
                    items.push(self.new_tuple(alloc::vec![key, count])?);
                }
                self.new_list(items)
            }
            COUNTER_ELEMENTS => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let entries = self.dicts[index].clone();
                let mut out = Vec::new();
                for (key, count) in entries {
                    let n = self.as_i128(count).unwrap_or(0);
                    for _ in 0..n.max(0) {
                        out.push(key);
                    }
                }
                let list = self.new_list(out)?;
                self.new_iter(list)
            }
            COUNTER_TOTAL => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let mut total: i128 = 0;
                for &(_, count) in &self.dicts[index] {
                    total += self.as_i128(count).ok_or(Trap::TypeError)?;
                }
                self.int_from_i128(total)
            }
            ODICT_POPITEM => {
                let last = match args {
                    [] => true,
                    [l] => self.py_truthy(*l)?.unwrap_or_else(|| l.is_truthy()),
                    _ => return Err(Trap::TypeError),
                };
                if self.dicts[index].is_empty() {
                    let message = self.new_str("dictionary is empty")?;
                    self.set_trap_arg(message);
                    return Err(Trap::KeyError);
                }
                let (key, value) = if last {
                    self.dicts[index].pop().unwrap_or((Value::NONE, Value::NONE))
                } else {
                    self.dicts[index].remove(0)
                };
                self.new_tuple(alloc::vec![key, value])
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// The interp-aware `dict` method dispatch: the equality-sensitive lookups (`get`/`pop`/
    /// `setdefault`) and `update` test the key by an element's `__eq__` (threaded through the
    /// interpreter, via [`ObjectModel::dict_find_dyn`]), so a value-object key resolves correctly. The
    /// equality-free methods (keys/values/items/clear/copy/popitem) delegate to the plain
    /// [`ObjectModel::call_dict_method`].
    pub(crate) fn call_dict_method_dyn(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        let index = self.dict_slot(receiver).ok_or(Trap::TypeError)?;
        match method_id {
            DICT_GET => {
                let (key, default) = match args {
                    [k] => (*k, Value::NONE),
                    [k, d] => (*k, *d),
                    _ => return Err(Trap::TypeError),
                };
                match self.dict_find_dyn(receiver, key, functions, depth)? {
                    Some(slot) if slot < self.dicts[index].len() => Ok(self.dicts[index][slot].1),
                    _ => Ok(default),
                }
            }
            DICT_POP => {
                let (key, default) = match args {
                    [k] => (*k, None),
                    [k, d] => (*k, Some(*d)),
                    _ => return Err(Trap::TypeError),
                };
                match self.dict_find_dyn(receiver, key, functions, depth)? {
                    Some(slot) if slot < self.dicts[index].len() => Ok(self.dicts[index].remove(slot).1),
                    _ => default.ok_or(Trap::KeyError),
                }
            }
            DICT_SETDEFAULT => {
                let (key, default) = match args {
                    [k] => (*k, Value::NONE),
                    [k, d] => (*k, *d),
                    _ => return Err(Trap::TypeError),
                };
                match self.dict_find_dyn(receiver, key, functions, depth)? {
                    Some(slot) if slot < self.dicts[index].len() => Ok(self.dicts[index][slot].1),
                    _ => {
                        self.dicts[index].push((key, default));
                        Ok(default)
                    }
                }
            }
            DICT_UPDATE => {
                let other = match args {
                    [] => return Ok(Value::NONE),
                    [other] => *other,
                    _ => return Err(Trap::TypeError),
                };
                let pairs = if let Some(entries) = self.dict_entries(other) {
                    entries
                } else {
                    let iterator = self.new_iter(other)?;
                    let mut kv = Vec::new();
                    while let Some(pair) = self.py_next(iterator)? {
                        let parts = self.unpack_sequence(pair, 2)?;
                        kv.push((parts[0], parts[1]));
                    }
                    kv
                };
                for (key, value) in pairs {
                    match self.dict_find_dyn(receiver, key, functions, depth)? {
                        Some(slot) if slot < self.dicts[index].len() => self.dicts[index][slot].1 = value,
                        _ => self.dicts[index].push((key, value)),
                    }
                }
                Ok(Value::NONE)
            }
            COUNTER_UPDATE | COUNTER_SUBTRACT => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let sign: i128 = if method_id == COUNTER_UPDATE { 1 } else { -1 };
                let deltas: Vec<(Value, i128)> = if let Some(entries) = self.dict_entries(*other) {
                    let mut kv = Vec::with_capacity(entries.len());
                    for (key, count) in entries {
                        kv.push((key, self.as_i128(count).ok_or(Trap::TypeError)?));
                    }
                    kv
                } else {
                    let items =
                        crate::builtins::collect_iterable(self, &[*other], functions, depth)?;
                    items.into_iter().map(|key| (key, 1)).collect()
                };
                for (key, delta) in deltas {
                    match self.dict_find_dyn(receiver, key, functions, depth)? {
                        Some(slot) if slot < self.dicts[index].len() => {
                            let current =
                                self.as_i128(self.dicts[index][slot].1).ok_or(Trap::TypeError)?;
                            let updated = self.int_from_i128(current + sign * delta)?;
                            self.dicts[index][slot].1 = updated;
                        }
                        _ => {
                            let fresh = self.int_from_i128(sign * delta)?;
                            self.dicts[index].push((key, fresh));
                        }
                    }
                }
                Ok(Value::NONE)
            }
            ODICT_MOVE_TO_END => {
                let (key, last) = match args {
                    [k] => (*k, true),
                    [k, l] => (*k, self.py_truthy(*l)?.unwrap_or_else(|| l.is_truthy())),
                    _ => return Err(Trap::TypeError),
                };
                match self.dict_find_dyn(receiver, key, functions, depth)? {
                    Some(slot) if slot < self.dicts[index].len() => {
                        let entry = self.dicts[index].remove(slot);
                        if last {
                            self.dicts[index].push(entry);
                        } else {
                            self.dicts[index].insert(0, entry);
                        }
                        Ok(Value::NONE)
                    }
                    _ => {
                        self.set_trap_arg(key);
                        Err(Trap::KeyError)
                    }
                }
            }
            _ => self.call_dict_method(receiver, method_id, args),
        }
    }

    /// `dict.fromkeys(iterable, value)`: a new dict with each distinct element of `iterable` as a
    /// key, all mapped to `value`. Keys dedup interp-aware (a value object collapses with an equal
    /// one via its `__eq__`); every value is the same, so first- vs last-wins is moot.
    pub fn new_dict_fromkeys(
        &mut self,
        iterable: Value,
        value: Value,
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        let keys = self.collect_elements(iterable)?;
        let pairs: Vec<(Value, Value)> = keys.into_iter().map(|key| (key, value)).collect();
        self.new_dict_dyn(pairs, functions, depth)
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
        let index = self.tuple_slot(tuple).ok_or(Trap::TypeError)?;
        match method_id {
            TUPLE_INDEX => match self.seq_index(index, args) {
                Err(Trap::ValueError) => {
                    Err(self.with_message(Trap::ValueError, "tuple.index(x): x not in tuple"))
                }
                other => other,
            },
            TUPLE_COUNT => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let n = self.seqs[index].iter().filter(|e| self.key_eq(**e, *value)).count();
                Value::fixnum(n as i32).ok_or(Trap::Overflow)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `property` builder method (`getter`/`setter`/`deleter`): returns a NEW property
    /// with that one accessor replaced by `args[0]` (so `x = x.setter(f)` adds a setter).
    fn call_property_method(&mut self, receiver: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let [func] = args else {
            return Err(Trap::TypeError);
        };
        let (fget, fset, fdel) = self.property_accessors(receiver);
        match method_id {
            PROPERTY_GETTER => self.new_property(*func, fset, fdel),
            PROPERTY_SETTER => self.new_property(fget, *func, fdel),
            PROPERTY_DELETER => self.new_property(fget, fset, *func),
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `float` method: `is_integer()` (finite with no fractional part), `conjugate()`
    /// (returns the float), and `as_integer_ratio()` -> the exact `(numerator, denominator)`.
    fn call_float_method(&mut self, receiver: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        if !args.is_empty() {
            return Err(Trap::TypeError);
        }
        let f = self.float_value(receiver).ok_or(Trap::TypeError)?;
        match method_id {
            FLOAT_IS_INTEGER => Ok(Value::from_bool(f.is_finite() && libm::floor(f) == f)),
            FLOAT_HEX => {
                let hex = float_to_hex(f);
                self.new_str(&hex)
            }
            FLOAT_CONJUGATE => Ok(receiver),
            FLOAT_AS_INTEGER_RATIO => {
                let (num, den) = float_as_integer_ratio(f).ok_or(Trap::ValueError)?;
                let numerator = self.new_bigint(num)?;
                let denominator = self.new_bigint(den)?;
                self.new_tuple(alloc::vec![numerator, denominator])
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches an `int` method: `bit_length()` / `bit_count()` (arbitrary precision), `conjugate()`
    /// (an int is its own conjugate), and `to_bytes(length, byteorder)` -> big/little-endian `bytes`
    /// (non-negative, fits `length` bytes; a signed or bigint conversion is not supported here).
    fn call_int_method(&mut self, receiver: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        match method_id {
            INT_BIT_LENGTH => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let bits = self.as_bigint(receiver).ok_or(Trap::TypeError)?.bit_length();
                self.new_long(i128::from(bits))
            }
            INT_BIT_COUNT => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let count = self.as_bigint(receiver).ok_or(Trap::TypeError)?.bit_count();
                self.new_long(i128::from(count))
            }
            INT_CONJUGATE | INT_INDEX => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.number_to_int(receiver)
            }
            INT_AS_INTEGER_RATIO => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let one = Value::fixnum(1).ok_or(Trap::Overflow)?;
                self.new_tuple(alloc::vec![receiver, one])
            }
            INT_TO_BYTES => {
                let (length, byteorder) = match args {
                    [] => (1, String::from("big")),
                    [len] => (len.as_int().ok_or(Trap::TypeError)?, String::from("big")),
                    [len, order] => (
                        len.as_int().ok_or(Trap::TypeError)?,
                        self.str_value(*order).map(String::from).ok_or(Trap::TypeError)?,
                    ),
                    _ => return Err(Trap::TypeError),
                };
                self.int_to_bytes(receiver, length, &byteorder, false)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// `int.to_bytes` for any int tier, including a bignum. Works in [`BigInt`] throughout rather
    /// than a machine word, because the whole point of the verb is producing a byte string wider
    /// than one -- an `i128` route would refuse exactly the values worth converting.
    pub(crate) fn int_to_bytes(
        &mut self,
        receiver: Value,
        length: i64,
        byteorder: &str,
        signed: bool,
    ) -> Result<Value, Trap> {
        if length < 0 {
            let message = "length argument must be non-negative";
            return Err(self.raise_named_exception("ValueError", message));
        }
        if byteorder != "little" && byteorder != "big" {
            let message = "byteorder must be either 'little' or 'big'";
            return Err(self.raise_named_exception("ValueError", message));
        }
        let value = self.as_bigint(receiver).ok_or(Trap::TypeError)?;
        let length = length as usize;
        let bits = 8 * length as u64;
        let span = BigInt::from_i128(1).shl(bits);
        let zero = BigInt::from_i128(0);

        if value < zero && !signed {
            let message = "can't convert negative int to unsigned";
            return Err(self.raise_named_exception("OverflowError", message));
        }
        let (low, high) = if !signed {
            (zero.clone(), span.sub(&BigInt::from_i128(1)))
        } else if bits == 0 {
            (zero.clone(), zero.clone())
        } else {
            let half = BigInt::from_i128(1).shl(bits - 1);
            (half.neg(), half.sub(&BigInt::from_i128(1)))
        };
        if value < low || value > high {
            let message = "int too big to convert";
            return Err(self.raise_named_exception("OverflowError", message));
        }

        let mut encoded = if value < zero { value.add(&span) } else { value };
        let base = BigInt::from_i128(256);
        let mut bytes = alloc::vec![0u8; length];
        for byte in bytes.iter_mut() {
            let (quotient, remainder) = encoded.divmod(&base).ok_or(Trap::Malformed)?;
            *byte = remainder.to_i128().unwrap_or(0) as u8;
            encoded = quotient;
        }
        if byteorder == "big" {
            bytes.reverse();
        }
        self.new_bytes(bytes)
    }

    /// `int.from_bytes` for any width, signed or not.
    pub(crate) fn int_from_bytes(
        &mut self,
        data: &[u8],
        byteorder: &str,
        signed: bool,
    ) -> Result<Value, Trap> {
        if byteorder != "little" && byteorder != "big" {
            let message = "byteorder must be either 'little' or 'big'";
            return Err(self.raise_named_exception("ValueError", message));
        }
        let mut big_endian: alloc::vec::Vec<u8> = data.to_vec();
        if byteorder == "little" {
            big_endian.reverse();
        }
        let base = BigInt::from_i128(256);
        let mut result = BigInt::from_i128(0);
        for byte in &big_endian {
            result = result.mul(&base).add(&BigInt::from_i128(i128::from(*byte)));
        }
        if signed && big_endian.first().is_some_and(|top| top & 0x80 != 0) {
            let span = BigInt::from_i128(1).shl(8 * big_endian.len() as u64);
            result = result.sub(&span);
        }
        self.new_bigint(result)
    }

    /// Dispatches a `memoryview` method (all no-argument): `tobytes()` copies out to `bytes`,
    /// `tolist()` to a list of ints, `hex()` to the lowercase hex string.
    /// `slice.indices(length)` -- the `(start, stop, step)` a slice resolves to over a sequence of
    /// that length, with negatives folded in and the ends clamped. This is what a container written
    /// in Python calls to implement its own slicing, so the arithmetic belongs here once rather than
    /// in each of them.
    fn call_slice_method(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        if method_id != SLICE_INDICES {
            return Err(Trap::AttributeError);
        }
        let [length] = args else {
            return Err(Trap::TypeError);
        };
        let Some(length) = self.as_i128(*length) else {
            return Err(Trap::TypeError);
        };
        if length < 0 {
            let message = "length should not be negative";
            return Err(self.raise_named_exception("ValueError", message));
        }
        let (start, stop, step) = self.slice_components(receiver);
        let step = match self.as_i128(step) {
            None => 1,
            Some(0) => {
                let message = "slice step cannot be zero";
                return Err(self.raise_named_exception("ValueError", message));
            }
            Some(n) => n,
        };
        let (lower, upper) = if step < 0 { (-1, length - 1) } else { (0, length) };
        let resolve = |value: Option<i128>, default: i128| -> i128 {
            match value {
                None => default,
                Some(n) if n < 0 => core::cmp::max(n + length, lower),
                Some(n) => core::cmp::min(n, upper),
            }
        };
        let start = resolve(self.as_i128(start), if step < 0 { upper } else { lower });
        let stop = resolve(self.as_i128(stop), if step < 0 { lower } else { upper });
        let start = self.new_bigint(BigInt::from_i128(start))?;
        let stop = self.new_bigint(BigInt::from_i128(stop))?;
        let step = self.new_bigint(BigInt::from_i128(step))?;
        self.new_tuple(alloc::vec![start, stop, step])
    }

    fn call_memoryview_method(&mut self, receiver: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        if !args.is_empty() {
            return Err(Trap::TypeError);
        }
        let data = self.memoryview_bytes(receiver).ok_or(Trap::TypeError)?.to_vec();
        match method_id {
            MV_TOBYTES => self.new_bytes(data),
            MV_TOLIST => {
                let mut elements = Vec::with_capacity(data.len());
                for &byte in &data {
                    elements.push(Value::fixnum(i32::from(byte)).ok_or(Trap::Overflow)?);
                }
                self.new_list(elements)
            }
            MV_HEX => {
                let mut hex = String::new();
                for byte in &data {
                    hex.push_str(&alloc::format!("{byte:02x}"));
                }
                self.new_str(&hex)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `bytes`/`bytearray` method: `hex()` -> the lowercase hex string; `decode()` ->
    /// the utf-8 `str` (an invalid sequence is a `ValueError`); `append(int)` / `extend(bytes-like)`
    /// mutate a bytearray in place (returning None).
    fn call_bytes_method(&mut self, receiver: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        match method_id {
            BYTES_HEX => {
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let (sep_arg, group) = match args {
                    [] => (None, 1i64),
                    [sep] => (Some(*sep), 1),
                    [sep, n] => (Some(*sep), n.as_int().ok_or(Trap::TypeError)?),
                    _ => return Err(Trap::TypeError),
                };
                let sep = match sep_arg {
                    None => None,
                    Some(value) => {
                        let owned = String::from(self.str_value(value).ok_or(Trap::TypeError)?);
                        if owned.chars().count() != 1 {
                            return Err(self.with_message(Trap::ValueError, "sep must be length 1."));
                        }
                        Some(owned)
                    }
                };
                let len = data.len() as i64;
                let mut hex = String::new();
                for (i, &byte) in data.iter().enumerate() {
                    let idx = i as i64;
                    if let Some(ref sep) = sep {
                        let boundary = match group {
                            0 => false,
                            g if g > 0 => (len - idx) % g == 0,
                            g => idx % -g == 0,
                        };
                        if idx != 0 && boundary {
                            hex.push_str(sep);
                        }
                    }
                    hex.push_str(&alloc::format!("{byte:02x}"));
                }
                self.new_str(&hex)
            }
            BYTES_DECODE => {
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let text = core::str::from_utf8(&data).map_err(|_| Trap::ValueError)?;
                let owned = String::from(text);
                self.new_str(&owned)
            }
            BYTEARRAY_APPEND => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let byte = value.as_int().ok_or(Trap::TypeError)?;
                if !(0..=255).contains(&byte) {
                    return Err(Trap::ValueError);
                }
                let slot = self.byte_buffer_slot(receiver).ok_or(Trap::TypeError)?;
                self.byte_buffers[slot].push(byte as u8);
                Ok(Value::NONE)
            }
            BYTEARRAY_COPY => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let bytes = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                self.new_bytearray(bytes)
            }
            BYTEARRAY_EXTEND => {
                let [source] = args else {
                    return Err(Trap::TypeError);
                };
                let extra = self.bytes_value(*source).ok_or(Trap::TypeError)?.to_vec();
                let slot = self.byte_buffer_slot(receiver).ok_or(Trap::TypeError)?;
                self.byte_buffers[slot].extend(extra);
                Ok(Value::NONE)
            }
            BYTES_ISALPHA | BYTES_ISDIGIT | BYTES_ISALNUM | BYTES_ISSPACE | BYTES_ISUPPER
            | BYTES_ISLOWER | BYTES_ISTITLE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                Ok(Value::from_bool(bytes_predicate(method_id, data)))
            }
            BYTES_STARTSWITH | BYTES_ENDSWITH => {
                let [prefix] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*prefix).ok_or(Trap::TypeError)?;
                let matches = if method_id == BYTES_STARTSWITH {
                    data.starts_with(needle)
                } else {
                    data.ends_with(needle)
                };
                Ok(Value::from_bool(matches))
            }
            BYTES_FIND => {
                let [sub] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*sub).ok_or(Trap::TypeError)?;
                let index = if needle.is_empty() {
                    0
                } else {
                    data.windows(needle.len()).position(|w| w == needle).map_or(-1, |p| p as i64)
                };
                Value::fixnum(i32::try_from(index).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)
            }
            BYTES_COUNT => {
                let [sub] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*sub).ok_or(Trap::TypeError)?;
                let count = if needle.is_empty() {
                    data.len() + 1
                } else {
                    let mut n = 0;
                    let mut i = 0;
                    while i + needle.len() <= data.len() {
                        if &data[i..i + needle.len()] == needle {
                            n += 1;
                            i += needle.len();
                        } else {
                            i += 1;
                        }
                    }
                    n
                };
                Value::fixnum(count as i32).ok_or(Trap::Overflow)
            }
            BYTES_REPLACE => {
                let [old, new] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let old = self.bytes_value(*old).ok_or(Trap::TypeError)?.to_vec();
                let new = self.bytes_value(*new).ok_or(Trap::TypeError)?.to_vec();
                let replaced = replace_bytes(&data, &old, &new);
                if self.is_bytearray(receiver) {
                    self.new_bytearray(replaced)
                } else {
                    self.new_bytes(replaced)
                }
            }
            BYTES_UPPER | BYTES_LOWER => {
                let mut data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                for byte in &mut data {
                    *byte = if method_id == BYTES_UPPER {
                        byte.to_ascii_uppercase()
                    } else {
                        byte.to_ascii_lowercase()
                    };
                }
                if self.is_bytearray(receiver) {
                    self.new_bytearray(data)
                } else {
                    self.new_bytes(data)
                }
            }
            BYTES_SPLIT | BYTES_RSPLIT => {
                let (sep, maxsplit) = match args {
                    [] => (None, -1i64),
                    [s] if s.is_none() => (None, -1),
                    [s] => (Some(self.bytes_value(*s).ok_or(Trap::TypeError)?.to_vec()), -1),
                    [s, m] => {
                        let limit = m.as_int().ok_or(Trap::TypeError)?;
                        let sep = if s.is_none() {
                            None
                        } else {
                            Some(self.bytes_value(*s).ok_or(Trap::TypeError)?.to_vec())
                        };
                        (sep, limit)
                    }
                    _ => return Err(Trap::TypeError),
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let bytearray = self.is_bytearray(receiver);
                let right = method_id == BYTES_RSPLIT;
                let parts: Vec<Vec<u8>> = match sep {
                    None if maxsplit < 0 => split_whitespace_bytes(&data),
                    None if right => rsplit_whitespace_maxsplit_bytes(&data, maxsplit as usize),
                    None => split_whitespace_maxsplit_bytes(&data, maxsplit as usize),
                    Some(sep) => {
                        if sep.is_empty() {
                            return Err(Trap::ValueError);
                        }
                        let all = split_on_bytes(&data, &sep);
                        if maxsplit < 0 || all.len() <= maxsplit as usize + 1 {
                            all
                        } else if right {
                            let keep_from = all.len() - maxsplit as usize;
                            let mut head = Vec::new();
                            for (idx, part) in all[..keep_from].iter().enumerate() {
                                if idx > 0 {
                                    head.extend_from_slice(&sep);
                                }
                                head.extend_from_slice(part);
                            }
                            let mut parts = alloc::vec![head];
                            parts.extend(all[keep_from..].iter().cloned());
                            parts
                        } else {
                            let mut parts: Vec<Vec<u8>> = all[..maxsplit as usize].to_vec();
                            let mut tail = Vec::new();
                            for (idx, part) in all[maxsplit as usize..].iter().enumerate() {
                                if idx > 0 {
                                    tail.extend_from_slice(&sep);
                                }
                                tail.extend_from_slice(part);
                            }
                            parts.push(tail);
                            parts
                        }
                    }
                };
                let mut elements = Vec::with_capacity(parts.len());
                for part in parts {
                    elements.push(if bytearray {
                        self.new_bytearray(part)?
                    } else {
                        self.new_bytes(part)?
                    });
                }
                self.new_list(elements)
            }
            BYTES_STRIP | BYTES_LSTRIP | BYTES_RSTRIP => {
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let bytearray = self.is_bytearray(receiver);
                let chars = match args {
                    [] => alloc::vec![b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c],
                    [set] => self.bytes_value(*set).ok_or(Trap::TypeError)?.to_vec(),
                    _ => return Err(Trap::TypeError),
                };
                let stripped = strip_bytes(
                    &data,
                    &chars,
                    method_id != BYTES_RSTRIP,
                    method_id != BYTES_LSTRIP,
                );
                if bytearray {
                    self.new_bytearray(stripped)
                } else {
                    self.new_bytes(stripped)
                }
            }
            BYTES_TITLE | BYTES_CAPITALIZE | BYTES_SWAPCASE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let out = bytes_case_transform(method_id, data);
                if self.is_bytearray(receiver) {
                    self.new_bytearray(out)
                } else {
                    self.new_bytes(out)
                }
            }
            BYTES_REMOVEPREFIX | BYTES_REMOVESUFFIX => {
                let [affix] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let affix = self.bytes_value(*affix).ok_or(Trap::TypeError)?.to_vec();
                let stripped = if method_id == BYTES_REMOVEPREFIX {
                    data.strip_prefix(affix.as_slice()).map(<[u8]>::to_vec)
                } else {
                    data.strip_suffix(affix.as_slice()).map(<[u8]>::to_vec)
                };
                let result = stripped.unwrap_or(data);
                if self.is_bytearray(receiver) {
                    self.new_bytearray(result)
                } else {
                    self.new_bytes(result)
                }
            }
            BYTES_JOIN => {
                let [iterable] = args else {
                    return Err(Trap::TypeError);
                };
                let sep = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let elements = self.seq_value(*iterable).ok_or(Trap::TypeError)?.to_vec();
                let mut result: Vec<u8> = Vec::new();
                for (i, &elem) in elements.iter().enumerate() {
                    if i > 0 {
                        result.extend_from_slice(&sep);
                    }
                    result.extend_from_slice(self.bytes_value(elem).ok_or(Trap::TypeError)?);
                }
                if self.is_bytearray(receiver) {
                    self.new_bytearray(result)
                } else {
                    self.new_bytes(result)
                }
            }
            BYTES_RFIND | BYTES_INDEX | BYTES_RINDEX => {
                let [sub] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*sub).ok_or(Trap::TypeError)?;
                let from_right = method_id == BYTES_RFIND || method_id == BYTES_RINDEX;
                let pos = if needle.is_empty() {
                    Some(if from_right { data.len() } else { 0 })
                } else if from_right {
                    data.windows(needle.len()).rposition(|w| w == needle)
                } else {
                    data.windows(needle.len()).position(|w| w == needle)
                };
                let index = match pos {
                    Some(p) => p as i64,
                    None if method_id == BYTES_INDEX || method_id == BYTES_RINDEX => {
                        return Err(self.with_message(Trap::ValueError, "subsection not found"));
                    }
                    None => -1,
                };
                Value::fixnum(i32::try_from(index).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)
            }
            BYTES_LJUST | BYTES_RJUST | BYTES_CENTER => {
                let (width, fill) = match args {
                    [w] => (w.as_int().ok_or(Trap::TypeError)?, b' '),
                    [w, f] => match self.bytes_value(*f).ok_or(Trap::TypeError)? {
                        [b] => (w.as_int().ok_or(Trap::TypeError)?, *b),
                        _ => return Err(Trap::TypeError),
                    },
                    _ => return Err(Trap::TypeError),
                };
                let width = width.max(0) as usize;
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let out = if data.len() >= width {
                    data
                } else {
                    let pad = width - data.len();
                    let (left, right) = match method_id {
                        BYTES_LJUST => (0, pad),
                        BYTES_RJUST => (pad, 0),
                        _ => {
                            let left = pad / 2 + (pad & width & 1);
                            (left, pad - left)
                        }
                    };
                    let mut r = Vec::with_capacity(width);
                    r.resize(left, fill);
                    r.extend_from_slice(&data);
                    r.resize(r.len() + right, fill);
                    r
                };
                if self.is_bytearray(receiver) {
                    self.new_bytearray(out)
                } else {
                    self.new_bytes(out)
                }
            }
            BYTES_ZFILL => {
                let [width] = args else {
                    return Err(Trap::TypeError);
                };
                let width = width.as_int().ok_or(Trap::TypeError)?.max(0) as usize;
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let out = if data.len() >= width {
                    data
                } else {
                    let pad = width - data.len();
                    let mut r = Vec::with_capacity(width);
                    if matches!(data.first(), Some(b'+' | b'-')) {
                        r.push(data[0]);
                        r.resize(1 + pad, b'0');
                        r.extend_from_slice(&data[1..]);
                    } else {
                        r.resize(pad, b'0');
                        r.extend_from_slice(&data);
                    }
                    r
                };
                if self.is_bytearray(receiver) {
                    self.new_bytearray(out)
                } else {
                    self.new_bytes(out)
                }
            }
            BYTES_PARTITION | BYTES_RPARTITION => {
                let [sep] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let sep = self.bytes_value(*sep).ok_or(Trap::TypeError)?.to_vec();
                if sep.is_empty() {
                    return Err(Trap::ValueError);
                }
                let bytearray = self.is_bytearray(receiver);
                let found = if method_id == BYTES_PARTITION {
                    data.windows(sep.len()).position(|w| w == sep.as_slice())
                } else {
                    data.windows(sep.len()).rposition(|w| w == sep.as_slice())
                };
                let (before, mid, after) = match found {
                    Some(p) => (data[..p].to_vec(), sep.clone(), data[p + sep.len()..].to_vec()),
                    None if method_id == BYTES_PARTITION => (data, Vec::new(), Vec::new()),
                    None => (Vec::new(), Vec::new(), data),
                };
                let b0 = if bytearray { self.new_bytearray(before)? } else { self.new_bytes(before)? };
                let b1 = if bytearray { self.new_bytearray(mid)? } else { self.new_bytes(mid)? };
                let b2 = if bytearray { self.new_bytearray(after)? } else { self.new_bytes(after)? };
                self.new_tuple(alloc::vec![b0, b1, b2])
            }
            BYTES_SPLITLINES => {
                let keepends = match args {
                    [] => false,
                    [k] => self.py_truthy(*k)?.unwrap_or(false),
                    _ => return Err(Trap::TypeError),
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let bytearray = self.is_bytearray(receiver);
                let mut elems = Vec::new();
                for line in split_lines_bytes(&data, keepends) {
                    elems.push(if bytearray {
                        self.new_bytearray(line)?
                    } else {
                        self.new_bytes(line)?
                    });
                }
                self.new_list(elems)
            }
            BYTES_EXPANDTABS => {
                let tabsize = match args {
                    [] => 8,
                    [t] => t.as_int().ok_or(Trap::TypeError)?,
                    _ => return Err(Trap::TypeError),
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let mut out = Vec::with_capacity(data.len());
                let mut column: i64 = 0;
                for &b in &data {
                    match b {
                        b'\t' => {
                            if tabsize > 0 {
                                let spaces = tabsize - (column % tabsize);
                                out.resize(out.len() + spaces as usize, b' ');
                                column += spaces;
                            }
                        }
                        b'\n' | b'\r' => {
                            out.push(b);
                            column = 0;
                        }
                        _ => {
                            out.push(b);
                            column += 1;
                        }
                    }
                }
                if self.is_bytearray(receiver) {
                    self.new_bytearray(out)
                } else {
                    self.new_bytes(out)
                }
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `complex` method: `conjugate()` flips the sign of the imaginary part.
    #[cfg(feature = "complex")]
    fn call_complex_method(&mut self, complex: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let (re, im) = self.complex_value(complex).ok_or(Trap::TypeError)?;
        match method_id {
            COMPLEX_CONJUGATE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.new_complex(re, -im)
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
            SET_UNION | SET_INTERSECTION | SET_DIFFERENCE => {
                let mut acc = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                for other in args {
                    let b = self.collect_elements(*other)?;
                    acc = match method_id {
                        SET_UNION => self.set_union_elems(&acc, &b),
                        SET_INTERSECTION => self.set_filter_elems(&acc, &b, true),
                        _ => self.set_filter_elems(&acc, &b, false),
                    };
                }
                if frozen {
                    self.new_frozenset(acc)
                } else {
                    self.new_set(acc)
                }
            }
            SET_SYMMETRIC_DIFFERENCE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let mut r = self.set_filter_elems(&a, &b, false);
                r.extend(self.set_filter_elems(&b, &a, false));
                if frozen {
                    self.new_frozenset(r)
                } else {
                    self.new_set(r)
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
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                for other in args {
                    let b = self.collect_elements(*other)?;
                    for e in b {
                        if !self.sets[slot].iter().any(|x| self.key_eq(*x, e)) {
                            self.sets[slot].push(e);
                        }
                    }
                }
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// The interp-aware `set`/`frozenset` method dispatch: the equality-sensitive methods (the
    /// algebra union/intersection/difference/symmetric_difference, the predicates issubset/
    /// issuperset/isdisjoint, the mutators add/discard/remove/update) test membership via an
    /// element's `__eq__` (threaded through the interpreter), so a user object dedups in a set. The
    /// equality-free methods (copy/clear/pop) delegate to the plain [`ObjectModel::call_set_method`].
    pub(crate) fn call_set_method_dyn(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
        functions: &[CodeObject],
        depth: usize,
    ) -> Result<Value, Trap> {
        let frozen = self.is_frozenset(receiver);
        match method_id {
            SET_UNION | SET_INTERSECTION | SET_DIFFERENCE => {
                let mut acc = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                for other in args {
                    let b = self.collect_elements(*other)?;
                    acc = match method_id {
                        SET_UNION => crate::interp::union_elems_dyn(&acc, &b, functions, self, depth)?,
                        SET_INTERSECTION => {
                            crate::interp::filter_elems_dyn(&acc, &b, true, functions, self, depth)?
                        }
                        _ => crate::interp::filter_elems_dyn(&acc, &b, false, functions, self, depth)?,
                    };
                }
                if frozen {
                    self.new_frozenset(acc)
                } else {
                    self.new_set(acc)
                }
            }
            SET_SYMMETRIC_DIFFERENCE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let mut r = crate::interp::filter_elems_dyn(&a, &b, false, functions, self, depth)?;
                r.extend(crate::interp::filter_elems_dyn(&b, &a, false, functions, self, depth)?);
                if frozen {
                    self.new_frozenset(r)
                } else {
                    self.new_set(r)
                }
            }
            SET_ISSUBSET | SET_ISSUPERSET | SET_ISDISJOINT => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let result = match method_id {
                    SET_ISSUBSET => crate::interp::subset_dyn(&a, &b, functions, self, depth)?,
                    SET_ISSUPERSET => crate::interp::subset_dyn(&b, &a, functions, self, depth)?,
                    _ => crate::interp::disjoint_dyn(&a, &b, functions, self, depth)?,
                };
                Ok(Value::from_bool(result))
            }
            SET_ADD => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                if !crate::interp::elems_contain(*value, &a, functions, self, depth)? {
                    self.set_push(receiver, *value)?;
                }
                Ok(Value::NONE)
            }
            SET_DISCARD | SET_REMOVE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                self.require_hashable(*value)?;
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let mut position = None;
                for (i, &e) in a.iter().enumerate() {
                    if crate::interp::elem_eq(*value, e, functions, self, depth)? {
                        position = Some(i);
                        break;
                    }
                }
                match position {
                    Some(p) => {
                        let slot =
                            self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                        self.sets[slot].remove(p);
                        Ok(Value::NONE)
                    }
                    None if method_id == SET_REMOVE => Err(Trap::KeyError),
                    None => Ok(Value::NONE),
                }
            }
            SET_UPDATE => {
                for other in args {
                    let b = self.collect_elements(*other)?;
                    for e in b {
                        let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                        if !crate::interp::elems_contain(e, &a, functions, self, depth)? {
                            self.set_push(receiver, e)?;
                        }
                    }
                }
                Ok(Value::NONE)
            }
            _ => self.call_set_method(receiver, method_id, args),
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
            CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not handled in the Op::Compare path"),
        };
        Ok(Value::from_bool(value))
    }

    /// Python's ordering comparison for `sorted`/`min`/`max`/`list.sort`/the ordering operators:
    /// int/bool numerically, str lexicographically (by code point), and tuple/list element-wise
    /// (lexicographic, recursive, shorter-is-less when one is a prefix). Comparing two different
    /// orderable kinds -- or any unorderable type -- is a `TypeError`, matching CPython (`1 < "a"`
    /// raises). Only same-kind sequences compare (`[1] < (1,)` is a TypeError).
    pub(crate) fn compare_ordered(&self, a: Value, b: Value) -> Result<Ordering, Trap> {
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) {
            return Ok(x.cmp(&y));
        }
        if self.is_int(a) && self.is_int(b) {
            if let (Some(x), Some(y)) = (self.as_bigint(a), self.as_bigint(b)) {
                return Ok(x.cmp(&y));
            }
        }
        if self.is_float(a) || self.is_float(b) {
            if let (Some(x), Some(y)) = (self.as_f64(a), self.as_f64(b)) {
                return Ok(x.partial_cmp(&y).unwrap_or(Ordering::Equal));
            }
        }
        if let (Some(x), Some(y)) = (self.str_value(a), self.str_value(b)) {
            return Ok(x.cmp(y));
        }
        if let (Some(x), Some(y)) = (self.byte_view(a), self.byte_view(b)) {
            return Ok(x.cmp(y));
        }
        let same_sequence =
            (self.is_tuple(a) && self.is_tuple(b)) || (self.is_list(a) && self.is_list(b));
        if same_sequence {
            if let (Some(xs), Some(ys)) = (self.seq_value(a), self.seq_value(b)) {
                let mut i = 0;
                while i < xs.len() && i < ys.len() {
                    match self.compare_ordered(xs[i], ys[i])? {
                        Ordering::Equal => i += 1,
                        non_equal => return Ok(non_equal),
                    }
                }
                return Ok(xs.len().cmp(&ys.len()));
            }
        }
        Err(Trap::TypeError)
    }

    /// Sorts `elements` in place by Python ordering ([`ObjectModel::compare_ordered`]), stably; an
    /// unorderable pair is a `TypeError`. Shared by `list.sort` and the `sorted` built-in.
    pub(crate) fn sort_values(&self, elements: &mut [Value]) -> Result<(), Trap> {
        let mut error = None;
        elements.sort_by(|a, b| match self.compare_ordered(*a, *b) {
            Ok(ordering) => ordering,
            Err(trap) => {
                error = error.or(Some(trap));
                Ordering::Equal
            }
        });
        match error {
            Some(trap) => Err(trap),
            None => Ok(()),
        }
    }

    /// Reorders `pairs` (sort-key, element) IN PLACE so that reading `pairs[i].1` gives the elements
    /// sorted by their key -- all-int numerically or all-str lexicographically (a mixed/unorderable
    /// set of keys is a `TypeError`), stably, DESCENDING when `reverse`. Backs `sorted(key=...,
    /// reverse=...)`: ties keep their original order (Python's stable sort), so `reverse` is a
    /// directional stable sort, not an ascending sort reversed (which would flip equal-key ties).
    pub(crate) fn sort_pairs_by_key(
        &self,
        pairs: &mut [(Value, Value)],
        reverse: bool,
    ) -> Result<(), Trap> {
        let mut error = None;
        pairs.sort_by(|a, b| match self.compare_ordered(a.0, b.0) {
            Ok(ordering) => {
                if reverse {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
            Err(trap) => {
                error = error.or(Some(trap));
                Ordering::Equal
            }
        });
        match error {
            Some(trap) => Err(trap),
            None => Ok(()),
        }
    }

    /// Whether `callee` is a bound `int.to_bytes` -- an int method with a keyword surface
    /// (`signed`, which CPython makes keyword-only, so it can be reached no other way).
    #[must_use]
    /// The RECEIVER's type has to be checked, not just the method id: ids are numbered per type, so
    /// `INT_TO_BYTES` is also `LIST_SORT`, `STR_STARTSWITH` and a dozen others. Matching the id
    /// alone silently hijacks every one of them.
    pub fn is_int_to_bytes_bound(&self, callee: Value) -> bool {
        callee.as_ref().is_some_and(|reference| {
            self.heap.type_id_of(reference) == self.bound_method_type_id && {
                let receiver = Value::from_bits(self.heap.read_u32(reference.0));
                let method_id = self.heap.read_u32(reference.0 + 4);
                self.is_int(receiver) && method_id == INT_TO_BYTES
            }
        })
    }

    /// Whether `callee` is a bound `list.sort` method -- a built-in method with a keyword
    /// surface (`sort(key=None, reverse=False)`), which the interpreter's `CallKw` routes specially.
    #[must_use]
    pub fn is_list_sort_bound(&self, callee: Value) -> bool {
        callee.as_ref().is_some_and(|reference| {
            self.heap.type_id_of(reference) == self.bound_method_type_id && {
                let receiver = Value::from_bits(self.heap.read_u32(reference.0));
                let method_id = self.heap.read_u32(reference.0 + 4);
                self.is_list(receiver) && method_id == LIST_SORT
            }
        })
    }

    /// Whether `callee` is a bound `dict.update` -- the interpreter intercepts it so the keyword form
    /// `d.update(a=1, b=2)` (and `d.update(other, a=1)`) merges the keywords too.
    #[must_use]
    pub(crate) fn is_dict_update_bound(&self, callee: Value) -> bool {
        callee.as_ref().is_some_and(|reference| {
            self.heap.type_id_of(reference) == self.bound_method_type_id && {
                let receiver = Value::from_bits(self.heap.read_u32(reference.0));
                let method_id = self.heap.read_u32(reference.0 + 4);
                self.is_dict(receiver) && method_id == DICT_UPDATE
            }
        })
    }

    /// The receiver a bound method is bound to (its `self`); the precondition is that `callee` is a
    /// bound method (e.g. [`ObjectModel::is_list_sort_bound`]).
    #[must_use]
    pub fn bound_receiver(&self, callee: Value) -> Value {
        let reference = callee.as_ref().expect("a bound method");
        Value::from_bits(self.heap.read_u32(reference.0))
    }

    /// The method id a bound method carries (payload word at offset 4, a raw int, not a tagged
    /// slot); the precondition is that `callee` is a bound method.
    #[must_use]
    pub fn bound_method_id(&self, callee: Value) -> u32 {
        let reference = callee.as_ref().expect("a bound method");
        self.heap.read_u32(reference.0 + 4)
    }

    /// Whether `callee` is a bound `str.join` -- the interpreter intercepts it to iterate ANY
    /// iterable argument (a str, a lazy map/filter, a generator), not just a materialized sequence.
    #[must_use]
    pub(crate) fn is_str_join(&self, callee: Value) -> bool {
        self.is_bound_method(callee)
            && self.bound_method_id(callee) == STR_JOIN
            && self.is_str(self.bound_receiver(callee))
    }

    /// Whether `callee` is a bound `str.format` -- the built-in method with a keyword surface
    /// (`"{name}".format(name=v)`), so the keyword-call path routes it to the template renderer.
    pub(crate) fn is_str_format_bound(&self, callee: Value) -> bool {
        self.is_bound_method(callee)
            && self.bound_method_id(callee) == STR_FORMAT
            && self.is_str(self.bound_receiver(callee))
    }

    /// A clone of a list/tuple's elements (so a caller can compute over them without holding a
    /// borrow on the model), or `None` if `value` is not a sequence.
    #[must_use]
    pub fn seq_elements(&self, value: Value) -> Option<Vec<Value>> {
        self.seq_value(value).cloned()
    }

    /// Sorts list `receiver` IN PLACE for `list.sort(key=, reverse=)`: by `keys[i]` (element i's
    /// precomputed sort key, or the element itself when `keys` is `None`), descending when
    /// `reverse`. `keys`, when given, must have one entry per element. Returns `None`.
    pub fn list_sort_in_place(
        &mut self,
        receiver: Value,
        keys: Option<Vec<Value>>,
        reverse: bool,
    ) -> Result<Value, Trap> {
        let index = self.seq_slot(receiver).ok_or(Trap::TypeError)?;
        let mut elements = core::mem::take(&mut self.seqs[index]);
        let outcome = self.sort_elements(&mut elements, keys, reverse);
        self.seqs[index] = elements;
        outcome.map(|()| Value::NONE)
    }

    /// The in-place sort behind [`ObjectModel::list_sort_in_place`]: no key sorts the elements
    /// directly (reverse flips), a key sorts (element, key) pairs by key (directional stable, so
    /// ties keep original order under reverse).
    fn sort_elements(
        &self,
        elements: &mut [Value],
        keys: Option<Vec<Value>>,
        reverse: bool,
    ) -> Result<(), Trap> {
        match keys {
            None => {
                self.sort_values(elements)?;
                if reverse {
                    elements.reverse();
                }
                Ok(())
            }
            Some(keys) => {
                if keys.len() != elements.len() {
                    return Err(Trap::TypeError);
                }
                let mut pairs: Vec<(Value, Value)> =
                    keys.into_iter().zip(elements.iter().copied()).collect();
                self.sort_pairs_by_key(&mut pairs, reverse)?;
                for (slot, (_, element)) in pairs.into_iter().enumerate() {
                    elements[slot] = element;
                }
                Ok(())
            }
        }
    }
}

/// The method names each built-in type answers, for `dir()`. GENERATED FROM the method-id tables in
/// this file rather than typed out, and a test asserts every entry still resolves through the table it
/// came from -- so a list cannot drift into naming a method that is not there, which is the one thing
/// `dir()` must never do.
#[cfg(feature = "introspection")]
const STR_METHOD_NAMES: &[&str] = &["capitalize", "casefold", "center", "count", "encode", "endswith", "expandtabs", "find", "format", "format_map", "index", "isalnum", "isalpha", "isascii", "isdecimal", "isdigit", "isidentifier", "islower", "isnumeric", "isprintable", "isspace", "istitle", "isupper", "join", "ljust", "lower", "lstrip", "partition", "removeprefix", "removesuffix", "replace", "rfind", "rindex", "rjust", "rpartition", "rsplit", "rstrip", "split", "splitlines", "startswith", "strip", "swapcase", "title", "translate", "upper", "zfill"];

#[cfg(feature = "introspection")]
const LIST_METHOD_NAMES: &[&str] = &["append", "clear", "copy", "count", "extend", "index", "insert", "pop", "remove", "reverse", "sort"];

#[cfg(feature = "introspection")]
const DICT_METHOD_NAMES: &[&str] = &["clear", "copy", "get", "items", "keys", "pop", "popitem", "setdefault", "update", "values"];

#[cfg(feature = "introspection")]
const SET_METHOD_NAMES: &[&str] = &["copy", "difference", "intersection", "isdisjoint", "issubset", "issuperset", "symmetric_difference", "union"];

/// The six a `set` has and a `frozenset` does not, because they mutate. Split for the same reason the
/// bytearray-only names are: one method table serves both types, and one of them refuses these.
#[cfg(feature = "introspection")]
const SET_ONLY_METHOD_NAMES: &[&str] = &["add", "clear", "discard", "pop", "remove", "update"];

#[cfg(feature = "introspection")]
const TUPLE_METHOD_NAMES: &[&str] = &["count", "index"];

#[cfg(feature = "introspection")]
const INT_METHOD_NAMES: &[&str] = &["__index__", "as_integer_ratio", "bit_count", "bit_length", "conjugate", "denominator", "from_bytes", "imag", "is_integer", "numerator", "real", "to_bytes"];

#[cfg(feature = "introspection")]
const FLOAT_METHOD_NAMES: &[&str] = &["as_integer_ratio", "conjugate", "hex", "imag", "is_integer", "real"];

#[cfg(feature = "introspection")]
const BYTES_METHOD_NAMES: &[&str] = &["capitalize", "center", "count", "decode", "endswith", "expandtabs", "find", "hex", "index", "isalnum", "isalpha", "isdigit", "islower", "isspace", "istitle", "isupper", "join", "ljust", "lower", "lstrip", "partition", "removeprefix", "removesuffix", "replace", "rfind", "rindex", "rjust", "rpartition", "rsplit", "rstrip", "split", "splitlines", "startswith", "strip", "swapcase", "title", "upper", "zfill"];

/// The three a BYTEARRAY has and `bytes` does not, because they mutate. Listing them for `bytes`
/// would name attributes it refuses -- the one thing this list must never do.
#[cfg(feature = "introspection")]
const BYTEARRAY_ONLY_METHOD_NAMES: &[&str] = &["append", "copy", "extend"];

#[cfg(feature = "introspection")]
const DEQUE_METHOD_NAMES: &[&str] = &["append", "appendleft", "clear", "copy", "count", "extend", "extendleft", "pop", "popleft", "remove", "rotate"];

#[cfg(feature = "introspection")]
const COUNTER_METHOD_NAMES: &[&str] = &["elements", "most_common", "subtract", "total", "update"];

#[cfg(feature = "introspection")]
const ODICT_METHOD_NAMES: &[&str] = &["move_to_end", "popitem"];

/// The method names of `value`'s built-in type. A dict SUBTYPE reports its own methods AND the dict
/// surface it inherits, which is what makes `dir(Counter())` include both `most_common` and `keys`.
#[cfg(feature = "introspection")]
fn builtin_type_method_names(model: &ObjectModel, value: Value) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    if model.is_str(value) {
        names.extend_from_slice(STR_METHOD_NAMES);
    } else if model.is_bytes(value) || model.is_bytearray(value) {
        names.extend_from_slice(BYTES_METHOD_NAMES);
        if model.is_bytearray(value) {
            names.extend_from_slice(BYTEARRAY_ONLY_METHOD_NAMES);
        }
    } else if model.is_list(value) {
        names.extend_from_slice(LIST_METHOD_NAMES);
    } else if model.is_deque(value) {
        names.extend_from_slice(DEQUE_METHOD_NAMES);
    } else if model.is_dict(value) {
        if model.is_counter(value) {
            names.extend_from_slice(COUNTER_METHOD_NAMES);
        }
        if model.is_ordereddict(value) {
            names.extend_from_slice(ODICT_METHOD_NAMES);
        }
        if model.is_defaultdict(value) {
            names.push("default_factory");
        }
        names.extend_from_slice(DICT_METHOD_NAMES);
    } else if model.is_set(value) || model.is_frozenset(value) {
        names.extend_from_slice(SET_METHOD_NAMES);
        if model.is_set(value) {
            names.extend_from_slice(SET_ONLY_METHOD_NAMES);
        }
    } else if model.is_tuple(value) {
        names.extend_from_slice(TUPLE_METHOD_NAMES);
    } else if model.is_int(value) {
        names.extend_from_slice(INT_METHOD_NAMES);
    } else if model.is_float(value) {
        names.extend_from_slice(FLOAT_METHOD_NAMES);
    }
    names
}

/// Roots the model cannot see, reported by the caller: the interpreter's live frame stack. Named
/// because the nesting reads badly inline -- it is a function that is handed the collector's visitor and
/// calls it once per slot it owns.
#[cfg(feature = "gc-collect")]
pub type ExtraRoots<'a> = dyn FnMut(&mut dyn FnMut(&mut lamella_gc::Ref)) + 'a;

/// The dunders EVERY instance answers, whatever its class wrote -- the object base's own surface as
/// this runtime actually has it. Deliberately NOT CPython's full 24: `__reduce__`/`__reduce_ex__`
/// (the pickle protocol -- and `copy` refuses an object that defines one, so claiming every object
/// has them would break that refusal), `__weakref__` (there are no weak references here),
/// `__subclasshook__` (no abstract base classes), and `__doc__`/`__firstlineno__`/
/// `__static_attributes__` (compile-time metadata the code object does not carry) are absent from
/// this list because they are absent from the runtime.
#[cfg(feature = "introspection")]
const INSTANCE_DUNDERS: &[&str] = &["__class__", "__getstate__", "__init__", "__new__"];

/// What a user function answers: its identity from the code object, plus anything set on it.
#[cfg(feature = "introspection")]
const FUNCTION_ATTRIBUTES: &[&str] = &["__doc__", "__name__", "__qualname__"];

/// What a method bound to an instance answers -- the function's identity, and the two halves it was
/// made of.
#[cfg(feature = "introspection")]
const BOUND_METHOD_ATTRIBUTES: &[&str] =
    &["__doc__", "__func__", "__name__", "__qualname__", "__self__"];

/// What every class answers, beyond its own namespace and its bases'.
#[cfg(feature = "introspection")]
const CLASS_ATTRIBUTES: &[&str] = &[
    "__class__", "__dict__", "__init__", "__init_subclass__", "__module__", "__name__", "__new__",
    "__qualname__",
];

/// What an exception instance answers beyond an ordinary one.
#[cfg(feature = "introspection")]
const EXCEPTION_ATTRIBUTES: &[&str] = &["__cause__", "__context__", "__suppress_context__", "args"];

/// Sorts and de-duplicates the collected names, which is the order `dir()` reports.
#[cfg(feature = "introspection")]
fn sorted_unique(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

/// Packs a file mode into the raw word a file object stores.
fn pack_file_mode(mode: crate::fileio::FileMode) -> u32 {
    u32::from(mode.read)
        | u32::from(mode.write) << 1
        | u32::from(mode.append) << 2
        | u32::from(mode.binary) << 3
        | u32::from(mode.exclusive) << 4
        | u32::from(mode.truncate) << 5
}

/// Unpacks what [`pack_file_mode`] stored.
fn unpack_file_mode(raw: u32) -> crate::fileio::FileMode {
    crate::fileio::FileMode {
        read: raw & 1 != 0,
        write: raw & 2 != 0,
        append: raw & 4 != 0,
        binary: raw & 8 != 0,
        exclusive: raw & 16 != 0,
        truncate: raw & 32 != 0,
    }
}

/// The CPython wrapper class a file with this mode would be an instance of -- used only for `repr`,
/// so a printed file is recognizable to a reader who knows CPython's.
fn file_wrapper_name(mode: crate::fileio::FileMode) -> &'static str {
    if !mode.binary {
        return "TextIOWrapper";
    }
    match (mode.read, mode.write) {
        (true, true) => "BufferedRandom",
        (false, true) => "BufferedWriter",
        _ => "BufferedReader",
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
    fn str_format_substitutes_positional_fields() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let t1 = model.new_str("{} + {} = {}").unwrap();
        let b1 = model.getattr(t1, "format", &mut InlineCache::empty()).unwrap();
        let r1 = model.call_bound_method(b1, &[n(1), n(2), n(3)]).unwrap();
        assert_eq!(model.str_value(r1), Some("1 + 2 = 3"));
        let t2 = model.new_str("{0}{1}{0} {{x}}").unwrap();
        let b2 = model.getattr(t2, "format", &mut InlineCache::empty()).unwrap();
        let r2 = model.call_bound_method(b2, &[n(7), n(8)]).unwrap();
        assert_eq!(model.str_value(r2), Some("787 {x}"));
        let t3 = model.new_str("{} {}").unwrap();
        let b3 = model.getattr(t3, "format", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(b3, &[n(1)]), Err(Trap::IndexError));
    }

    #[test]
    fn list_sort_in_place_no_key_and_with_keys() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let l = model.new_list(alloc::vec![n(3), n(1), n(2)]).unwrap();
        model.list_sort_in_place(l, None, true).unwrap();
        assert_eq!(model.repr(l), "[3, 2, 1]");
        let l2 = model.new_list(alloc::vec![n(10), n(20), n(30)]).unwrap();
        model.list_sort_in_place(l2, Some(alloc::vec![n(3), n(1), n(2)]), false).unwrap();
        assert_eq!(model.repr(l2), "[20, 30, 10]");
        let l3 = model.new_list(alloc::vec![n(100), n(200)]).unwrap();
        model.list_sort_in_place(l3, Some(alloc::vec![n(1), n(1)]), true).unwrap();
        assert_eq!(model.repr(l3), "[100, 200]");
    }

    #[test]
    fn exception_instances_render_their_message() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let value_error = model.exception_class("ValueError").unwrap();
        let exc = model.new_object(value_error).unwrap();
        let msg = model.new_str("bad input").unwrap();
        model.init_default_args(exc, &[msg]).unwrap();
        assert_eq!(model.display(exc), "bad input");
        let key_error = model.exception_class("KeyError").unwrap();
        let ke = model.new_object(key_error).unwrap();
        let key = model.new_str("missing").unwrap();
        model.init_default_args(ke, &[key]).unwrap();
        assert_eq!(model.display(ke), "'missing'");
        let empty = model.new_object(value_error).unwrap();
        assert_eq!(model.display(empty), "");
    }

    #[test]
    fn repr_of_builtin_types_and_functions() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        assert_eq!(model.repr(Value::builtin_ref(Builtin::Int.id())), "<class 'int'>");
        assert_eq!(model.repr(Value::builtin_ref(Builtin::List.id())), "<class 'list'>");
        assert_eq!(model.repr(Value::builtin_ref(Builtin::Abs.id())), "<built-in function abs>");
        let mut cache = InlineCache::empty();
        let int_name = model
            .getattr(Value::builtin_ref(Builtin::Int.id()), "__name__", &mut cache)
            .unwrap();
        assert_eq!(model.str_value(int_name), Some("int"));
    }

    #[test]
    fn py_delattr_removes_an_instance_attribute() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let class = model.exception_class("ValueError").unwrap();
        let obj = model.new_object(class).unwrap();
        model.py_setattr_instance(obj, "x", Value::fixnum(1).unwrap()).unwrap();
        let mut cache = InlineCache::empty();
        assert_eq!(model.getattr(obj, "x", &mut cache).unwrap().as_fixnum(), Some(1));
        model.py_delattr_instance(obj, "x").unwrap();
        assert_eq!(model.getattr(obj, "x", &mut cache), Err(Trap::AttributeError));
        assert_eq!(model.py_delattr_instance(obj, "x"), Err(Trap::AttributeError));
    }

    #[test]
    fn provided_module_resolves_members_as_attributes() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let answer_key = model.new_str("answer").unwrap();
        let name_key = model.new_str("name").unwrap();
        let name_val = model.new_str("lib").unwrap();
        let namespace = model
            .new_dict(alloc::vec![
                (answer_key, Value::fixnum(42).unwrap()),
                (name_key, name_val),
            ])
            .unwrap();
        model.provide_module("mylib", namespace).unwrap();
        let module = model.get_global("mylib").unwrap();
        assert!(model.is_module_object(module));
        let mut cache = InlineCache::empty();
        assert_eq!(model.getattr(module, "answer", &mut cache).unwrap().as_fixnum(), Some(42));
        let name = model.getattr(module, "name", &mut cache).unwrap();
        assert_eq!(model.str_value(name), Some("lib"));
        assert_eq!(model.getattr(module, "nope", &mut cache), Err(Trap::AttributeError));
    }

    #[test]
    fn new_long_normalizes_and_round_trips() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let small = model.new_long(42).unwrap();
        assert_eq!(small.as_fixnum(), Some(42));
        assert!(!model.is_long(small));
        let big = model.new_long(1_000_000_000_000_000_000).unwrap();
        assert!(model.is_long(big));
        assert_eq!(model.long_value(big), Some(1_000_000_000_000_000_000));
        assert_eq!(model.as_i128(big), Some(1_000_000_000_000_000_000));
        assert_eq!(model.display(big), "1000000000000000000");
        assert_eq!(model.as_i128(Value::fixnum(7).unwrap()), Some(7));
    }

    #[test]
    fn new_float_round_trips_and_coerces() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        for &n in &[0.0, -0.0, 1.0, 0.1, -3.5, 1e300, f64::INFINITY] {
            let v = model.new_float(n).unwrap();
            assert!(model.is_float(v));
            assert!(!model.is_long(v));
            assert_eq!(model.float_value(v).unwrap().to_bits(), n.to_bits());
            assert_eq!(model.as_f64(v), Some(n));
        }
        assert_eq!(model.as_f64(Value::fixnum(7).unwrap()), Some(7.0));
        assert_eq!(model.as_f64(Value::TRUE), Some(1.0));
        let big = model.new_long(1_000_000_000_000_000_000).unwrap();
        assert_eq!(model.as_f64(big), Some(1e18));
        assert!(!model.is_float(Value::fixnum(7).unwrap()));
        let nan = model.new_float(f64::NAN).unwrap();
        assert!(model.is_float(nan));
        assert!(model.float_value(nan).unwrap().is_nan());
    }

    #[cfg(feature = "complex")]
    #[test]
    fn new_complex_round_trips_and_reprs() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let c = model.new_complex(1.5, -2.5).unwrap();
        assert!(model.is_complex(c));
        assert_eq!(model.complex_value(c), Some((1.5, -2.5)));
        assert_eq!(model.as_complex(Value::fixnum(3).unwrap()), Some((3.0, 0.0)));
        assert_eq!(model.as_complex(c), Some((1.5, -2.5)));
        let f = model.new_float(1.5).unwrap();
        assert!(!model.is_complex(f));
        assert!(model.float_value(c).is_none());
    }

    #[cfg(feature = "complex")]
    #[test]
    fn format_complex_matches_cpython_repr() {
        let cases: &[(f64, f64, &str)] = &[
            (0.0, 0.0, "0j"),
            (0.0, 1.0, "1j"),
            (0.0, -1.0, "-1j"),
            (1.0, 2.0, "(1+2j)"),
            (3.0, -4.0, "(3-4j)"),
            (1.0, 0.0, "(1+0j)"),
            (2.5, -0.5, "(2.5-0.5j)"),
            (-0.0, -1.0, "(-0-1j)"),
            (-0.0, -0.0, "(-0-0j)"),
            (1e100, 2e-5, "(1e+100+2e-05j)"),
        ];
        for &(re, im, expected) in cases {
            assert_eq!(format_complex(re, im), expected, "complex({re}, {im})");
        }
    }

    #[test]
    fn format_float_matches_cpython_repr() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (-1.0, "-1.0"),
            (3.0, "3.0"),
            (0.5, "0.5"),
            (0.1, "0.1"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0 / 3.0, "0.3333333333333333"),
            (10.0 / 3.0, "3.3333333333333335"),
            (100.0, "100.0"),
            (123.456, "123.456"),
            (1.2345678901234567, "1.2345678901234567"),
            (0.1234567890123456, "0.1234567890123456"),
            (1e15, "1000000000000000.0"),
            (1234567890123456.0, "1234567890123456.0"),
            (1e16, "1e+16"),
            (1e17, "1e+17"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            (1e20, "1e+20"),
            (1e100, "1e+100"),
            (1e308, "1e+308"),
            (1e-4, "0.0001"),
            (0.000123, "0.000123"),
            (1e-5, "1e-05"),
            (1e-100, "1e-100"),
            (1e-308, "1e-308"),
            (-0.1, "-0.1"),
            (-1e16, "-1e+16"),
            (-1e-5, "-1e-05"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for &(value, expected) in cases {
            assert_eq!(format_float(value), expected, "repr({value:e})");
        }
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn format_value_spec_float_types_match_python() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let cases: &[(&str, f64, &str)] = &[
            (".2f", 3.14159, "3.14"),
            (".0f", 3.14159, "3"),
            ("08.2f", 3.14159, "00003.14"),
            ("+.2f", 3.14159, "+3.14"),
            ("^10.2f", 3.14159, "   3.14   "),
            (".2e", 12345.678, "1.23e+04"),
            (".0e", 12345.678, "1e+04"),
            ("E", 3.14159, "3.141590E+00"),
            ("g", 1234567.0, "1.23457e+06"),
            (".3g", 0.0001234, "0.000123"),
            (".1%", 0.5, "50.0%"),
            (".3", 3.0, "3.0"),
            (",.2f", 1234567.89, "1,234,567.89"),
            ("_.0f", 1234567.0, "1_234_567"),
            (".2f", -0.0, "-0.00"),
            (".2f", f64::INFINITY, "inf"),
            ("+f", f64::NAN, "+nan"),
            ("F", f64::INFINITY, "INF"),
        ];
        for &(spec, value, expected) in cases {
            let v = model.new_float(value).unwrap();
            assert_eq!(model.format_value_spec(v, spec).unwrap(), expected, "format({value:e}, {spec:?})");
        }
    }

    #[test]
    fn py_hash_matches_python_for_ints_and_rejects_unhashables() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        assert_eq!(model.py_hash(Value::fixnum(5).unwrap()).unwrap().as_fixnum(), Some(5));
        assert_eq!(model.py_hash(Value::fixnum(-1).unwrap()).unwrap().as_fixnum(), Some(-2));
        assert_eq!(model.py_hash(Value::TRUE).unwrap().as_fixnum(), Some(1));
        let s = model.new_str("abc").unwrap();
        assert_eq!(model.py_hash(s).unwrap(), model.py_hash(s).unwrap());
        let t = model.new_tuple(alloc::vec![Value::fixnum(1).unwrap(), s]).unwrap();
        assert_eq!(model.py_hash(t).unwrap(), model.py_hash(t).unwrap());
        let l = model.new_list(alloc::vec![Value::fixnum(1).unwrap()]).unwrap();
        assert_eq!(model.py_hash(l), Err(Trap::TypeError));
    }

    #[test]
    fn new_dict_fromkeys_dedups_keys() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let keys = model
            .new_list(alloc::vec![
                Value::fixnum(1).unwrap(),
                Value::fixnum(1).unwrap(),
                Value::fixnum(2).unwrap(),
            ])
            .unwrap();
        let zero = Value::fixnum(0).unwrap();
        let d = model.new_dict_fromkeys(keys, zero, &[], 0).unwrap();
        assert_eq!(model.py_len(d).unwrap().as_fixnum(), Some(2));
    }

    #[test]
    fn trap_to_exception_carries_context_and_constant_messages() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let key = model.new_str("missing").unwrap();
        model.set_trap_arg(key);
        let exc = model.trap_to_exception(Trap::KeyError).unwrap();
        assert_eq!(model.display(exc), "'missing'");
        let zde = model.trap_to_exception(Trap::ZeroDivisionError).unwrap();
        assert_eq!(model.display(zde), "division by zero");
        let trap = model.with_message(Trap::IndexError, "list index out of range");
        let ie = model.trap_to_exception(trap).unwrap();
        assert_eq!(model.display(ie), "list index out of range");
        let te = model.trap_to_exception(Trap::TypeError).unwrap();
        assert_eq!(model.display(te), "");
    }

    #[test]
    fn format_value_spec_covers_int_and_str() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let n = Value::fixnum(42).unwrap();
        assert_eq!(model.format_value_spec(n, "05d").unwrap(), "00042");
        assert_eq!(model.format_value_spec(n, "x").unwrap(), "2a");
        assert_eq!(model.format_value_spec(Value::fixnum(255).unwrap(), "#X").unwrap(), "0XFF");
        assert_eq!(model.format_value_spec(n, ">5").unwrap(), "   42");
        assert_eq!(model.format_value_spec(Value::fixnum(-42).unwrap(), "+d").unwrap(), "-42");
        let s = model.new_str("abcdef").unwrap();
        assert_eq!(model.format_value_spec(s, ".3").unwrap(), "abc");
        assert_eq!(model.format_value_spec(s, "^8").unwrap(), " abcdef ");
        assert_eq!(model.format_value_spec(n, ".2f").unwrap(), "42.00");
        assert_eq!(model.format_value_spec(n, "08.2f").unwrap(), "00042.00");
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
    fn slice_assignment_matches_python() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |i: i32| Value::fixnum(i).unwrap();
        fn run(model: &mut ObjectModel, base: &[i32], s: Value, e: Value, k: Value, rhs: Vec<Value>) -> Result<String, Trap> {
            let list = model.new_list(base.iter().map(|&i| Value::fixnum(i).unwrap()).collect())?;
            let slice = model.new_slice(s, e, k)?;
            model.seq_setitem_slice(list, slice, rhs)?;
            Ok(model.repr(list))
        }
        let five = [1, 2, 3, 4, 5];
        let none = Value::NONE;
        assert_eq!(run(&mut model, &five, n(1), n(3), none, alloc::vec![n(10), n(20), n(30)]).unwrap(), "[1, 10, 20, 30, 4, 5]");
        assert_eq!(run(&mut model, &five, n(1), n(4), none, alloc::vec![n(99)]).unwrap(), "[1, 99, 5]");
        assert_eq!(run(&mut model, &five, n(1), n(1), none, alloc::vec![n(10), n(20)]).unwrap(), "[1, 10, 20, 2, 3, 4, 5]");
        assert_eq!(run(&mut model, &five, n(1), n(3), none, alloc::vec![]).unwrap(), "[1, 4, 5]");
        assert_eq!(run(&mut model, &five, n(-2), none, none, alloc::vec![n(99)]).unwrap(), "[1, 2, 3, 99]");
        assert_eq!(run(&mut model, &five, n(3), n(1), none, alloc::vec![n(88)]).unwrap(), "[1, 2, 3, 88, 4, 5]");
        assert_eq!(run(&mut model, &five, none, none, none, alloc::vec![n(7), n(8)]).unwrap(), "[7, 8]");
        assert_eq!(run(&mut model, &five, none, none, n(2), alloc::vec![n(10), n(20), n(30)]).unwrap(), "[10, 2, 20, 4, 30]");
        assert_eq!(run(&mut model, &[1, 2, 3], none, none, n(-1), alloc::vec![n(10), n(20), n(30)]).unwrap(), "[30, 20, 10]");
        assert!(matches!(run(&mut model, &five, none, none, n(2), alloc::vec![n(1), n(2)]), Err(Trap::ValueError)));
    }

    #[test]
    fn cell_boxes_and_shares_a_value() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let cell = model.new_cell(Value::fixnum(5).unwrap()).unwrap();
        assert!(model.is_cell(cell));
        assert!(!model.is_cell(Value::fixnum(5).unwrap()));
        assert_eq!(model.cell_get(cell).unwrap().as_fixnum(), Some(5));
        model.cell_set(cell, Value::fixnum(10).unwrap()).unwrap();
        assert_eq!(model.cell_get(cell).unwrap().as_fixnum(), Some(10));
        let s = model.new_str("captured").unwrap();
        let boxed = model.new_cell(s).unwrap();
        assert_eq!(model.str_value(model.cell_get(boxed).unwrap()), Some("captured"));
        assert!(matches!(model.cell_get(Value::NONE), Err(Trap::TypeError)));
    }

    #[test]
    fn closure_carries_captured_cells() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let plain = model.new_py_function(0, Value::NONE, Value::NONE, 0).unwrap();
        assert!(model.py_function_cells(plain).is_empty());
        let c1 = model.new_cell(Value::fixnum(1).unwrap()).unwrap();
        let c2 = model.new_cell(Value::fixnum(2).unwrap()).unwrap();
        let cells = model.new_tuple(alloc::vec![c1, c2]).unwrap();
        let closure = model.new_closure(0, Value::NONE, Value::NONE, cells, 0).unwrap();
        let captured = model.py_function_cells(closure);
        assert_eq!(captured.len(), 2);
        assert_eq!(model.cell_get(captured[0]).unwrap().as_fixnum(), Some(1));
        model.cell_set(captured[1], Value::fixnum(20).unwrap()).unwrap();
        assert_eq!(model.cell_get(captured[1]).unwrap().as_fixnum(), Some(20));
    }

    #[test]
    fn staticmethod_classmethod_unwrap_on_attribute_access() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let func = Value::function_ref(0);
        let sm = model.new_method_wrapper(func, false).unwrap();
        let cm = model.new_method_wrapper(func, true).unwrap();
        assert!(model.is_method_wrapper(sm) && !model.method_wrapper_is_class(sm));
        assert!(model.method_wrapper_is_class(cm));
        let name = model.new_str("C").unwrap();
        let sm_key = model.new_str("sm").unwrap();
        let cm_key = model.new_str("cm").unwrap();
        let plain_key = model.new_str("plain").unwrap();
        let namespace = model
            .new_dict(alloc::vec![(sm_key, sm), (cm_key, cm), (plain_key, func)])
            .unwrap();
        let class = model.new_class(name, Value::NONE, namespace).unwrap();
        let instance = model.new_object(class).unwrap();
        let sm_attr = model.py_getattr_instance(instance, "sm").unwrap();
        assert_eq!(sm_attr, func);
        let cm_attr = model.py_getattr_instance(instance, "cm").unwrap();
        assert!(model.is_py_bound(cm_attr));
        let plain_attr = model.py_getattr_instance(instance, "plain").unwrap();
        assert!(model.is_py_bound(plain_attr));
    }

    #[test]
    fn del_item_matches_python() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |i: i32| Value::fixnum(i).unwrap();
        let none = Value::NONE;
        fn del_index(model: &mut ObjectModel, base: &[i32], index: Value) -> Result<String, Trap> {
            let list = model.new_list(base.iter().map(|&i| Value::fixnum(i).unwrap()).collect())?;
            model.py_delitem(list, index)?;
            Ok(model.repr(list))
        }
        fn del_slice(model: &mut ObjectModel, base: &[i32], s: Value, e: Value, k: Value) -> Result<String, Trap> {
            let list = model.new_list(base.iter().map(|&i| Value::fixnum(i).unwrap()).collect())?;
            let slice = model.new_slice(s, e, k)?;
            model.py_delitem(list, slice)?;
            Ok(model.repr(list))
        }
        let five = [1, 2, 3, 4, 5];
        assert_eq!(del_index(&mut model, &five, n(1)).unwrap(), "[1, 3, 4, 5]");
        assert_eq!(del_index(&mut model, &five, n(-1)).unwrap(), "[1, 2, 3, 4]");
        assert!(matches!(del_index(&mut model, &five, n(10)), Err(Trap::IndexError)));
        assert_eq!(del_slice(&mut model, &five, n(1), n(3), none).unwrap(), "[1, 4, 5]");
        assert_eq!(del_slice(&mut model, &five, none, none, n(2)).unwrap(), "[2, 4]");
        assert_eq!(del_slice(&mut model, &five, n(1), none, n(2)).unwrap(), "[1, 3, 5]");
        let d = model.new_dict(alloc::vec![(n(1), n(10)), (n(2), n(20))]).unwrap();
        model.py_delitem(d, n(1)).unwrap();
        assert_eq!(model.repr(d), "{2: 20}");
        assert!(matches!(model.py_delitem(d, n(9)), Err(Trap::KeyError)));
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
    fn getattr_binds_a_defaulted_method() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let name = model.new_str("C").unwrap();
        let key_m = model.new_str("m").unwrap();
        let defaults = model.new_tuple(alloc::vec![n(1)]).unwrap();
        let m = model.new_py_function(0, defaults, Value::NONE, 0).unwrap();
        let ns = model.new_dict(alloc::vec![(key_m, m)]).unwrap();
        let class = model.new_class(name, Value::NONE, ns).unwrap();
        let obj = model.new_object(class).unwrap();
        let bound = model.py_getattr_instance(obj, "m").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_self(bound), obj);
        assert!(model.is_py_function(model.bound_func(bound)));
    }

    /// Builds a class named `name` with direct `bases` (a tuple, a single class, or `None`) and the
    /// given namespace members. Used by the multiple-inheritance tests.
    fn class_with(model: &mut ObjectModel, name: &str, bases: Value, members: &[(&str, Value)]) -> Value {
        let name_v = model.new_str(name).unwrap();
        let mut pairs = Vec::new();
        for (k, v) in members {
            let key = model.new_str(k).unwrap();
            pairs.push((key, *v));
        }
        let ns = model.new_dict(pairs).unwrap();
        model.new_class(name_v, bases, ns).unwrap()
    }

    #[test]
    fn multiple_inheritance_resolves_via_the_c3_mro() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let a = class_with(&mut model, "A", Value::NONE, &[("a", n(1)), ("shared", n(10))]);
        let ba = model.new_tuple(alloc::vec![a]).unwrap();
        let b = class_with(&mut model, "B", ba, &[("shared", n(20))]);
        let ca = model.new_tuple(alloc::vec![a]).unwrap();
        let c = class_with(&mut model, "C", ca, &[("c", n(3)), ("shared", n(30))]);
        let bc = model.new_tuple(alloc::vec![b, c]).unwrap();
        let d = class_with(&mut model, "D", bc, &[]);
        assert_eq!(model.class_mro_vec(d), alloc::vec![d, b, c, a]);
        let obj = model.new_object(d).unwrap();
        assert_eq!(model.py_getattr_instance(obj, "shared").unwrap().as_fixnum(), Some(20));
        assert_eq!(model.py_getattr_instance(obj, "a").unwrap().as_fixnum(), Some(1));
        assert_eq!(model.py_getattr_instance(obj, "c").unwrap().as_fixnum(), Some(3));
        for base in [a, b, c, d] {
            assert!(model.is_instance_of(obj, base));
            assert!(model.is_subclass_of(d, base));
        }
        assert!(!model.is_subclass_of(b, c));
        let unrelated = class_with(&mut model, "Z", Value::NONE, &[]);
        assert!(!model.is_instance_of(obj, unrelated));
    }

    #[test]
    fn super_resolves_the_next_class_in_the_instance_mro() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let a = class_with(&mut model, "A", Value::NONE, &[("who", n(1))]);
        let ta = model.new_tuple(alloc::vec![a]).unwrap();
        let b = class_with(&mut model, "B", ta, &[("who", n(2))]);
        let ta2 = model.new_tuple(alloc::vec![a]).unwrap();
        let c = class_with(&mut model, "C", ta2, &[("who", n(3))]);
        let bc = model.new_tuple(alloc::vec![b, c]).unwrap();
        let d = class_with(&mut model, "D", bc, &[]);
        let obj = model.new_object(d).unwrap();
        let super_b = model.new_super(b, obj).unwrap();
        assert_eq!(model.py_getattr_super(super_b, "who").unwrap().as_fixnum(), Some(3));
        let super_c = model.new_super(c, obj).unwrap();
        assert_eq!(model.py_getattr_super(super_c, "who").unwrap().as_fixnum(), Some(1));
        let super_a = model.new_super(a, obj).unwrap();
        assert_eq!(model.py_getattr_super(super_a, "who"), Err(Trap::AttributeError));
    }

    #[test]
    fn an_inconsistent_hierarchy_is_a_type_error() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let a = class_with(&mut model, "A", Value::NONE, &[]);
        let b = class_with(&mut model, "B", Value::NONE, &[]);
        let ab = model.new_tuple(alloc::vec![a, b]).unwrap();
        let x = class_with(&mut model, "X", ab, &[]);
        let ba = model.new_tuple(alloc::vec![b, a]).unwrap();
        let y = class_with(&mut model, "Y", ba, &[]);
        let xy = model.new_tuple(alloc::vec![x, y]).unwrap();
        let name = model.new_str("Z").unwrap();
        let ns = model.new_dict(Vec::new()).unwrap();
        assert_eq!(model.new_class(name, xy, ns), Err(Trap::TypeError));
    }

    #[test]
    fn single_inheritance_and_no_base_linearize_trivially() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let a = class_with(&mut model, "A", Value::NONE, &[]);
        let ta = model.new_tuple(alloc::vec![a]).unwrap();
        let b = class_with(&mut model, "B", ta, &[]);
        let tb = model.new_tuple(alloc::vec![b]).unwrap();
        let c = class_with(&mut model, "C", tb, &[]);
        assert_eq!(model.class_mro_vec(a), alloc::vec![a]);
        assert_eq!(model.class_mro_vec(c), alloc::vec![c, b, a]);
        let d = class_with(&mut model, "D", a, &[]);
        assert_eq!(model.class_mro_vec(d), alloc::vec![d, a]);
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
    fn stopiteration_value_and_generator_return_stash() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let trap = model.raise_named_exception_with_value("StopIteration", Value::fixnum(99).unwrap());
        assert_eq!(trap, Trap::Raised);
        let exc = model.take_pending_exception().unwrap();
        assert_eq!(model.py_getattr_instance(exc, "value").unwrap(), Value::fixnum(99).unwrap());

        model.raise_named_exception_with_value("StopIteration", Value::NONE);
        let bare = model.take_pending_exception().unwrap();
        assert_eq!(model.py_getattr_instance(bare, "value").unwrap(), Value::NONE);

        let value_error = model.exception_class("ValueError").unwrap();
        let other = model.new_object(value_error).unwrap();
        assert_eq!(model.py_getattr_instance(other, "value"), Err(Trap::AttributeError));

        assert!(model.take_generator_return().is_none());
        model.set_generator_return(Value::fixnum(7).unwrap());
        assert_eq!(model.take_generator_return(), Some(Value::fixnum(7).unwrap()));
        assert!(model.take_generator_return().is_none());
    }

    #[test]
    fn import_star_binds_all_or_public_names() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let f = |n: i32| Value::fixnum(n).unwrap();

        let (alpha, hidden, beta) = (
            model.new_str("alpha").unwrap(),
            model.new_str("_hidden").unwrap(),
            model.new_str("beta").unwrap(),
        );
        let ns1 = model.new_dict(alloc::vec![(alpha, f(1)), (hidden, f(2)), (beta, f(3))]).unwrap();
        let module1 = model.new_module(ns1).unwrap();
        model.import_star(module1).unwrap();
        assert_eq!(model.current_module_global("alpha"), Some(f(1)));
        assert_eq!(model.current_module_global("beta"), Some(f(3)));
        assert_eq!(model.current_module_global("_hidden"), None);

        let gamma_name = model.new_str("gamma").unwrap();
        let all_list = model.new_list(alloc::vec![gamma_name]).unwrap();
        let (all_key, gamma, delta) = (
            model.new_str("__all__").unwrap(),
            model.new_str("gamma").unwrap(),
            model.new_str("delta").unwrap(),
        );
        let ns2 =
            model.new_dict(alloc::vec![(all_key, all_list), (gamma, f(10)), (delta, f(20))]).unwrap();
        let module2 = model.new_module(ns2).unwrap();
        model.import_star(module2).unwrap();
        assert_eq!(model.current_module_global("gamma"), Some(f(10)));
        assert_eq!(model.current_module_global("delta"), None);
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
        let keys_view = model.call_bound_method(keys, &[]).unwrap();
        assert_eq!(model.dict_view_kind(keys_view), Some(DictViewKind::Keys));
        assert_eq!(model.py_len(keys_view).unwrap().as_fixnum(), Some(1));
        assert_eq!(model.repr(keys_view), "dict_keys([1])");
        model.py_setitem(dict, n(2), n(20)).unwrap();
        assert_eq!(model.py_len(keys_view).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.repr(keys_view), "dict_keys([1, 2])");

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

    #[test]
    fn compare_ordered_covers_ints_strs_and_tuples() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let i = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(model.compare_ordered(i(1), i(2)), Ok(Ordering::Less));
        assert_eq!(model.compare_ordered(i(5), i(5)), Ok(Ordering::Equal));
        assert_eq!(model.compare_ordered(Value::TRUE, i(0)), Ok(Ordering::Greater));
        let a = model.new_str("apple").unwrap();
        let b = model.new_str("banana").unwrap();
        assert_eq!(model.compare_ordered(a, b), Ok(Ordering::Less));
        let t1 = model.new_tuple(alloc::vec![i(1), i(2)]).unwrap();
        let t2 = model.new_tuple(alloc::vec![i(1), i(3)]).unwrap();
        let t3 = model.new_tuple(alloc::vec![i(1)]).unwrap();
        assert_eq!(model.compare_ordered(t1, t2), Ok(Ordering::Less));
        assert_eq!(model.compare_ordered(t3, t1), Ok(Ordering::Less));
        assert_eq!(model.compare_ordered(i(1), a), Err(Trap::TypeError));
        assert_eq!(model.compare_ordered(t1, a), Err(Trap::TypeError));
    }

    #[test]
    fn py_binary_concatenates_and_repeats_strs_and_sequences() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let i = |n: i32| Value::fixnum(n).unwrap();
        let ab = model.new_str("ab").unwrap();
        let cat = model.py_binary(BinOp::Add, ab, ab).unwrap().unwrap();
        assert_eq!(model.str_value(cat), Some("abab"));
        let rep = model.py_binary(BinOp::Mul, ab, i(3)).unwrap().unwrap();
        assert_eq!(model.str_value(rep), Some("ababab"));
        let rep_rev = model.py_binary(BinOp::Mul, i(2), ab).unwrap().unwrap();
        assert_eq!(model.str_value(rep_rev), Some("abab"));
        let l1 = model.new_list(alloc::vec![i(1), i(2)]).unwrap();
        let l2 = model.new_list(alloc::vec![i(3)]).unwrap();
        let lcat = model.py_binary(BinOp::Add, l1, l2).unwrap().unwrap();
        assert_eq!(model.repr(lcat), "[1, 2, 3]");
        let lrep = model.py_binary(BinOp::Mul, l1, i(2)).unwrap().unwrap();
        assert_eq!(model.repr(lrep), "[1, 2, 1, 2]");
        let t1 = model.new_tuple(alloc::vec![i(1)]).unwrap();
        let t2 = model.new_tuple(alloc::vec![i(2)]).unwrap();
        let tcat = model.py_binary(BinOp::Add, t1, t2).unwrap().unwrap();
        assert_eq!(model.repr(tcat), "(1, 2)");
        assert_eq!(model.py_binary(BinOp::Add, l1, t1), Err(Trap::TypeError));
        let empty = model.py_binary(BinOp::Mul, l1, i(0)).unwrap().unwrap();
        assert_eq!(model.repr(empty), "[]");
        assert_eq!(model.py_binary(BinOp::Add, i(1), i(2)).unwrap(), None);
    }
}
