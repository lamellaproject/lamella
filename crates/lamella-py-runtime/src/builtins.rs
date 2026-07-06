//! The runtime's built-in functions -- the first slice of the `builtins` namespace.

use core::cmp::Ordering;

use alloc::string::String;
use alloc::vec::Vec;

use lamella_py_bytecode::{BinOp, CmpOp, CodeObject};

use crate::bigint::BigInt;
use crate::interp::{binary, bigint_pow, call_value, iterator_for, py_next_value};
use crate::object::{InlineCache, ObjectModel, LAZY_ENUMERATE, LAZY_FILTER, LAZY_MAP, LAZY_ZIP};
use crate::trap::Trap;
use crate::value::Value;

/// A built-in, identified by a stable id (the value a built-in reference carries). The
/// set widens as the dynamic surface grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Builtin {
    /// `abs(x)` -- the absolute value.
    Abs = 0,
    /// `min(a, b, ...)` -- the smallest argument.
    Min = 1,
    /// `max(a, b, ...)` -- the largest argument.
    Max = 2,
    /// `len(s)` -- the number of items.
    Len = 3,
    /// `str(x)` -- the human-readable string.
    Str = 4,
    /// `list([iterable])` -- a new list of the iterable's items (empty if omitted).
    List = 5,
    /// `tuple([iterable])` -- a new tuple of the iterable's items (empty if omitted).
    Tuple = 6,
    /// `print(*args)` -- write the space-joined arguments + a newline.
    Print = 7,
    /// `range([start,] stop[, step])` -- a lazy int sequence.
    Range = 8,
    /// `enumerate(iterable[, start])` -- `(index, item)` pairs (eager: a list of tuples).
    Enumerate = 9,
    /// `sum(iterable[, start])` -- the integer sum.
    Sum = 10,
    /// `sorted(iterable)` -- a new sorted list (int or str elements).
    Sorted = 11,
    /// `bool([x])` -- the truth value (`False` with no argument).
    Bool = 12,
    /// `repr(x)` -- the `repr` string.
    Repr = 13,
    /// `int([x])` -- an int from an int/bool/str (`0` with no argument).
    Int = 14,
    /// `iter(x)` -- an iterator over `x` (dispatches `__iter__` on an instance).
    Iter = 15,
    /// `set([iterable])` -- a new set of the iterable's items (empty if omitted).
    Set = 16,
    /// `map(func, *iterables)` -- `func` applied across the zipped iterables (eager: a list).
    Map = 17,
    /// `filter(func_or_None, iterable)` -- items where `func(x)` (or `x`, if `None`) is truthy.
    Filter = 18,
    /// `zip(*iterables)` -- tuples of corresponding items, truncated to the shortest (a list).
    Zip = 19,
    /// `any(iterable)` -- `True` if any item is truthy.
    Any = 20,
    /// `all(iterable)` -- `True` if every item is truthy (`True` when empty).
    All = 21,
    /// `dict([pairs])` -- a new dict (empty, a copy of a dict, or from `(key, value)` pairs).
    Dict = 22,
    /// `reversed(seq)` -- an iterator over a sequence (str/list/tuple/range/dict) back to front.
    Reversed = 23,
    /// `chr(i)` -- the one-character string for code point `i`.
    Chr = 24,
    /// `ord(c)` -- the code point of a one-character string.
    Ord = 25,
    /// `divmod(a, b)` -- the `(a // b, a % b)` pair (Python floor division + modulo).
    Divmod = 26,
    /// `pow(base, exp[, mod])` -- `base ** exp`, optionally modulo `mod`.
    Pow = 27,
    /// `hex(i)` -- the `0x...` string (Python sign convention).
    Hex = 28,
    /// `bin(i)` -- the `0b...` string.
    Bin = 29,
    /// `oct(i)` -- the `0o...` string.
    Oct = 30,
    /// `frozenset([iterable])` -- a new immutable set (empty if omitted).
    Frozenset = 31,
    /// `callable(x)` -- whether `x` can be called (a function, builtin, class, or bound method).
    Callable = 32,
    /// `next(iterator[, default])` -- the next item, or `default` / `StopIteration` at the end.
    Next = 33,
    /// `sleep_ms(ms)` -- pause `ms` milliseconds. A Lamella hardware convenience: the delay seam
    /// is a no-op on the host and a timer/spin on device. `time.sleep`/`time.sleep_ms` (the
    /// module form) arrive with the import system.
    SleepMs = 34,
    /// `round(x[, ndigits])` -- round to `ndigits` (round-half-to-even). Integer x, integer result.
    Round = 35,
    /// `isinstance(x, classinfo)` -- whether `x` is an instance of the type (a built-in type, a
    /// user class + its bases, or a tuple of those).
    Isinstance = 36,
    /// `type(x)` -- the type object of `x` (a built-in type IS its constructor, so `type(5) is int`).
    Type = 37,
    /// `getattr(obj, name[, default])` -- the named attribute, or `default` / `AttributeError`.
    Getattr = 38,
    /// `hasattr(obj, name)` -- whether the attribute access succeeds.
    Hasattr = 39,
    /// `setattr(obj, name, value)` -- set the named attribute.
    Setattr = 40,
    /// `delattr(obj, name)` -- delete the named attribute.
    Delattr = 41,
    /// `hash(x)` -- the hash of a hashable value (int/bool/None/str/tuple); list/dict/set are a
    /// `TypeError`. Matches CPython for ints; str/tuple use a deterministic (non-CPython) hash.
    Hash = 42,
    /// `dict.fromkeys(iterable[, value])` -- the static factory reached via `getattr(dict,
    /// "fromkeys")`; not a global name (no `builtin_id` entry).
    DictFromkeys = 43,
    /// `float([x])` -- a float from an int/bool/float/str (`0.0` with no argument; parses `inf`/
    /// `nan`/decimal text, allowing a leading sign, surrounding whitespace, and `_` separators).
    Float = 44,
    /// `complex([real[, imag]])` -- a complex number. Behind the `complex` capability knob.
    #[cfg(feature = "complex")]
    Complex = 45,
    /// `id(x)` -- a stable integer identity for `x` (its tagged word, so `id(a) == id(b)` iff
    /// `a is b`). Not CPython's address value, but the same identity relation.
    Id = 46,
    /// `bytes([source[, encoding]])` -- an immutable byte string.
    Bytes = 47,
    /// `bytearray([source[, encoding]])` -- a mutable byte string.
    Bytearray = 48,
    /// `issubclass(cls, classinfo)` -- whether `cls` derives from the type (or any in a tuple).
    Issubclass = 49,
    /// `slice([start,] stop[, step])` -- a slice object (as `a[start:stop:step]` builds implicitly).
    Slice = 50,
    /// `staticmethod(func)` -- a method that receives no implicit first argument.
    Staticmethod = 51,
    /// `classmethod(func)` -- a method that receives the class as its first argument.
    Classmethod = 52,
    /// `memoryview(obj)` -- a zero-copy 1-D view over a bytes/bytearray buffer.
    Memoryview = 53,
    /// `format(value[, spec])` -- render `value` under the format-spec mini-language.
    Format = 54,
    /// `int.from_bytes(bytes, byteorder)` -- reached via the `int` type (not a global name).
    IntFromBytes = 55,
    /// `bytes.fromhex(str)` -- reached via the `bytes` type (not a global name).
    BytesFromhex = 56,
}

impl Builtin {
    /// The built-in for `id`, or `None` if unknown.
    #[must_use]
    pub fn from_id(id: u32) -> Option<Builtin> {
        match id {
            0 => Some(Builtin::Abs),
            1 => Some(Builtin::Min),
            2 => Some(Builtin::Max),
            3 => Some(Builtin::Len),
            4 => Some(Builtin::Str),
            5 => Some(Builtin::List),
            6 => Some(Builtin::Tuple),
            7 => Some(Builtin::Print),
            8 => Some(Builtin::Range),
            9 => Some(Builtin::Enumerate),
            10 => Some(Builtin::Sum),
            11 => Some(Builtin::Sorted),
            12 => Some(Builtin::Bool),
            13 => Some(Builtin::Repr),
            14 => Some(Builtin::Int),
            15 => Some(Builtin::Iter),
            16 => Some(Builtin::Set),
            17 => Some(Builtin::Map),
            18 => Some(Builtin::Filter),
            19 => Some(Builtin::Zip),
            20 => Some(Builtin::Any),
            21 => Some(Builtin::All),
            22 => Some(Builtin::Dict),
            23 => Some(Builtin::Reversed),
            24 => Some(Builtin::Chr),
            25 => Some(Builtin::Ord),
            26 => Some(Builtin::Divmod),
            27 => Some(Builtin::Pow),
            28 => Some(Builtin::Hex),
            29 => Some(Builtin::Bin),
            30 => Some(Builtin::Oct),
            31 => Some(Builtin::Frozenset),
            32 => Some(Builtin::Callable),
            33 => Some(Builtin::Next),
            34 => Some(Builtin::SleepMs),
            35 => Some(Builtin::Round),
            36 => Some(Builtin::Isinstance),
            37 => Some(Builtin::Type),
            38 => Some(Builtin::Getattr),
            39 => Some(Builtin::Hasattr),
            40 => Some(Builtin::Setattr),
            41 => Some(Builtin::Delattr),
            42 => Some(Builtin::Hash),
            43 => Some(Builtin::DictFromkeys),
            44 => Some(Builtin::Float),
            #[cfg(feature = "complex")]
            45 => Some(Builtin::Complex),
            46 => Some(Builtin::Id),
            47 => Some(Builtin::Bytes),
            48 => Some(Builtin::Bytearray),
            49 => Some(Builtin::Issubclass),
            50 => Some(Builtin::Slice),
            51 => Some(Builtin::Staticmethod),
            52 => Some(Builtin::Classmethod),
            53 => Some(Builtin::Memoryview),
            54 => Some(Builtin::Format),
            55 => Some(Builtin::IntFromBytes),
            56 => Some(Builtin::BytesFromhex),
            _ => None,
        }
    }

    /// The built-in's stable id.
    #[must_use]
    pub fn id(self) -> u32 {
        self as u32
    }

    /// The built-in's Python name (for `repr` / `__name__`).
    #[must_use]
    pub fn python_name(self) -> &'static str {
        match self {
            Builtin::Abs => "abs",
            Builtin::Min => "min",
            Builtin::Max => "max",
            Builtin::Len => "len",
            Builtin::Str => "str",
            Builtin::List => "list",
            Builtin::Tuple => "tuple",
            Builtin::Print => "print",
            Builtin::Range => "range",
            Builtin::Enumerate => "enumerate",
            Builtin::Sum => "sum",
            Builtin::Sorted => "sorted",
            Builtin::Bool => "bool",
            Builtin::Repr => "repr",
            Builtin::Int => "int",
            Builtin::Iter => "iter",
            Builtin::Set => "set",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Zip => "zip",
            Builtin::Any => "any",
            Builtin::All => "all",
            Builtin::Dict => "dict",
            Builtin::Reversed => "reversed",
            Builtin::Chr => "chr",
            Builtin::Ord => "ord",
            Builtin::Divmod => "divmod",
            Builtin::Pow => "pow",
            Builtin::Hex => "hex",
            Builtin::Bin => "bin",
            Builtin::Oct => "oct",
            Builtin::Frozenset => "frozenset",
            Builtin::Callable => "callable",
            Builtin::Next => "next",
            Builtin::SleepMs => "sleep_ms",
            Builtin::Round => "round",
            Builtin::Isinstance => "isinstance",
            Builtin::Type => "type",
            Builtin::Getattr => "getattr",
            Builtin::Hasattr => "hasattr",
            Builtin::Setattr => "setattr",
            Builtin::Delattr => "delattr",
            Builtin::Hash => "hash",
            Builtin::DictFromkeys => "fromkeys",
            Builtin::Float => "float",
            #[cfg(feature = "complex")]
            Builtin::Complex => "complex",
            Builtin::Id => "id",
            Builtin::Bytes => "bytes",
            Builtin::Bytearray => "bytearray",
            Builtin::Issubclass => "issubclass",
            Builtin::Slice => "slice",
            Builtin::Staticmethod => "staticmethod",
            Builtin::Classmethod => "classmethod",
            Builtin::Memoryview => "memoryview",
            Builtin::Format => "format",
            Builtin::IntFromBytes => "from_bytes",
            Builtin::BytesFromhex => "fromhex",
        }
    }

    /// Whether this built-in is a TYPE (`int`, `list`, ... -- their `repr` is `<class 'name'>` and
    /// they are what `type()` returns), versus a plain built-in function (`abs`, `len`, ...).
    #[must_use]
    pub fn is_type(self) -> bool {
        #[cfg(feature = "complex")]
        if matches!(self, Builtin::Complex) {
            return true;
        }
        matches!(
            self,
            Builtin::Int
                | Builtin::Float
                | Builtin::Bool
                | Builtin::Str
                | Builtin::Bytes
                | Builtin::Bytearray
                | Builtin::Memoryview
                | Builtin::List
                | Builtin::Tuple
                | Builtin::Dict
                | Builtin::Set
                | Builtin::Frozenset
                | Builtin::Range
                | Builtin::Type
        )
    }
}

/// `type(x)`: the type object of `x` -- a built-in value's type is its constructor built-in (so
/// `type(5) is int`), a class instance's type is its class object. `None` for a value whose type
/// has no representation yet (e.g. `None`, a function) -> the caller raises.
fn type_of(value: Value, model: &ObjectModel) -> Option<Value> {
    if model.is_instance(value) {
        return Some(model.instance_class(value));
    }
    #[cfg(feature = "complex")]
    if model.is_complex(value) {
        return Some(Value::builtin_ref(Builtin::Complex.id()));
    }
    let builtin = if value.is_fixnum() || model.is_long(value) || model.is_bigint(value) {
        Builtin::Int
    } else if model.is_float(value) {
        Builtin::Float
    } else if value == Value::TRUE || value == Value::FALSE {
        Builtin::Bool
    } else if model.is_str(value) {
        Builtin::Str
    } else if model.is_bytes(value) {
        Builtin::Bytes
    } else if model.is_bytearray(value) {
        Builtin::Bytearray
    } else if model.is_memoryview(value) {
        Builtin::Memoryview
    } else if model.is_list(value) {
        Builtin::List
    } else if model.is_tuple(value) {
        Builtin::Tuple
    } else if model.is_dict(value) {
        Builtin::Dict
    } else if model.is_set(value) {
        Builtin::Set
    } else if model.is_frozenset(value) {
        Builtin::Frozenset
    } else if model.is_range(value) {
        Builtin::Range
    } else if model.is_lazy_iter(value) {
        match model.lazy_iter_kind(value) {
            LAZY_MAP => Builtin::Map,
            LAZY_FILTER => Builtin::Filter,
            LAZY_ZIP => Builtin::Zip,
            LAZY_ENUMERATE => Builtin::Enumerate,
            _ => return None,
        }
    } else if model.is_method_wrapper(value) {
        if model.method_wrapper_is_class(value) {
            Builtin::Classmethod
        } else {
            Builtin::Staticmethod
        }
    } else {
        return None;
    };
    Some(Value::builtin_ref(builtin.id()))
}

/// The built-in id for `name`, or `None` if `name` is not a built-in -- so the
/// interpreter can fall a `LoadGlobal` back to the built-in namespace.
#[must_use]
pub fn builtin_id(name: &str) -> Option<u32> {
    let builtin = match name {
        "abs" => Builtin::Abs,
        "min" => Builtin::Min,
        "max" => Builtin::Max,
        "len" => Builtin::Len,
        "str" => Builtin::Str,
        "list" => Builtin::List,
        "tuple" => Builtin::Tuple,
        "print" => Builtin::Print,
        "range" => Builtin::Range,
        "enumerate" => Builtin::Enumerate,
        "sum" => Builtin::Sum,
        "sorted" => Builtin::Sorted,
        "bool" => Builtin::Bool,
        "repr" => Builtin::Repr,
        "int" => Builtin::Int,
        "float" => Builtin::Float,
        #[cfg(feature = "complex")]
        "complex" => Builtin::Complex,
        "id" => Builtin::Id,
        "bytes" => Builtin::Bytes,
        "bytearray" => Builtin::Bytearray,
        "issubclass" => Builtin::Issubclass,
        "slice" => Builtin::Slice,
        "staticmethod" => Builtin::Staticmethod,
        "classmethod" => Builtin::Classmethod,
        "memoryview" => Builtin::Memoryview,
        "format" => Builtin::Format,
        "iter" => Builtin::Iter,
        "set" => Builtin::Set,
        "map" => Builtin::Map,
        "filter" => Builtin::Filter,
        "zip" => Builtin::Zip,
        "any" => Builtin::Any,
        "all" => Builtin::All,
        "dict" => Builtin::Dict,
        "reversed" => Builtin::Reversed,
        "chr" => Builtin::Chr,
        "ord" => Builtin::Ord,
        "divmod" => Builtin::Divmod,
        "pow" => Builtin::Pow,
        "hex" => Builtin::Hex,
        "bin" => Builtin::Bin,
        "oct" => Builtin::Oct,
        "frozenset" => Builtin::Frozenset,
        "callable" => Builtin::Callable,
        "next" => Builtin::Next,
        "sleep_ms" => Builtin::SleepMs,
        "round" => Builtin::Round,
        "isinstance" => Builtin::Isinstance,
        "type" => Builtin::Type,
        "getattr" => Builtin::Getattr,
        "hasattr" => Builtin::Hasattr,
        "setattr" => Builtin::Setattr,
        "delattr" => Builtin::Delattr,
        "hash" => Builtin::Hash,
        _ => return None,
    };
    Some(builtin.id())
}

/// Calls built-in `id` with `args` (Python 3.14.6 "Built-in Functions").
pub fn call_builtin(
    id: u32,
    args: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    match Builtin::from_id(id).ok_or(Trap::Malformed)? {
        Builtin::Abs => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            #[cfg(feature = "complex")]
            if let Some((re, im)) = model.complex_value(args[0]) {
                return model.new_float(libm::hypot(re, im));
            }
            if let Some(f) = model.float_value(args[0]) {
                return model.new_float(libm::fabs(f));
            }
            if let Some(big) = model.bigint_value(args[0]) {
                return model.new_bigint(big.abs());
            }
            let n = model.as_i128(args[0]).ok_or(Trap::TypeError)?;
            model.new_long(n.checked_abs().ok_or(Trap::Overflow)?)
        }
        Builtin::Min => min_max(args, Ordering::Less, functions, model, depth),
        Builtin::Max => min_max(args, Ordering::Greater, functions, model, depth),
        Builtin::Len => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            if let Some(len_method) = model.find_dunder(args[0], "__len__") {
                return call_value(len_method, &[], functions, model, depth);
            }
            model.py_len(args[0])
        }
        Builtin::Str => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let rendered = display_arg(args[0], functions, model, depth)?;
            model.new_str(&rendered)
        }
        Builtin::List => {
            let elems = collect_iterable(model, args, functions, depth)?;
            model.new_list(elems)
        }
        Builtin::Tuple => {
            let elems = collect_iterable(model, args, functions, depth)?;
            model.new_tuple(elems)
        }
        Builtin::Print => {
            let mut line = String::new();
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    line.push(' ');
                }
                let part = display_arg(*arg, functions, model, depth)?;
                line.push_str(&part);
            }
            model.write_line(&line);
            Ok(Value::NONE)
        }
        Builtin::Range => {
            let (start, stop, step) = match args {
                [stop] => (0, stop.as_int().ok_or(Trap::TypeError)?, 1),
                [start, stop] => (
                    start.as_int().ok_or(Trap::TypeError)?,
                    stop.as_int().ok_or(Trap::TypeError)?,
                    1,
                ),
                [start, stop, step] => {
                    let step = step.as_int().ok_or(Trap::TypeError)?;
                    if step == 0 {
                        return Err(Trap::ValueError);
                    }
                    (
                        start.as_int().ok_or(Trap::TypeError)?,
                        stop.as_int().ok_or(Trap::TypeError)?,
                        step,
                    )
                }
                _ => return Err(Trap::TypeError),
            };
            model.new_range(start, stop, step)
        }
        Builtin::Enumerate => {
            let (iterable, start) = match args {
                [it] => (*it, 0i64),
                [it, s] => (*it, s.as_int().ok_or(Trap::TypeError)?),
                _ => return Err(Trap::TypeError),
            };
            let source = iterator_for(iterable, functions, model, depth)?;
            let sources = model.new_tuple(alloc::vec![source])?;
            let start_value = model.new_long(i128::from(start))?;
            model.new_lazy_iter(LAZY_ENUMERATE, start_value, sources)
        }
        Builtin::Sum => {
            let (iterable, start) = match args {
                [it] => (*it, Value::fixnum(0).ok_or(Trap::Overflow)?),
                [it, s] => (*it, *s),
                _ => return Err(Trap::TypeError),
            };
            let elements = collect_iterable(model, &[iterable], functions, depth)?;
            let mut acc = start;
            for element in elements {
                acc = binary(BinOp::Add, acc, element, model)?;
            }
            Ok(acc)
        }
        Builtin::Sorted => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let elements = collect_iterable(model, &[args[0]], functions, depth)?;
            sorted_list(elements, functions, model, depth)
        }
        Builtin::Bool => match args {
            [] => Ok(Value::FALSE),
            [x] => Ok(Value::from_bool(crate::interp::py_truthy_dyn(*x, functions, model, depth)?)),
            _ => Err(Trap::TypeError),
        },
        Builtin::Repr => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let rendered = repr_arg(args[0], functions, model, depth)?;
            model.new_str(&rendered)
        }
        Builtin::Int => match args {
            [] => Value::fixnum(0).ok_or(Trap::Overflow),
            [x] => {
                if let Some(f) = model.float_value(*x) {
                    if f.is_nan() {
                        return Err(model.with_message(Trap::ValueError, "cannot convert float NaN to integer"));
                    }
                    if f.is_infinite() {
                        return Err(model.with_message(Trap::Overflow, "cannot convert float infinity to integer"));
                    }
                    if !(-1.701_411_834_604_692_3e38..1.701_411_834_604_692_3e38).contains(&f) {
                        return Err(Trap::Overflow);
                    }
                    model.new_long(f as i128)
                } else if let Some(n) = x.as_int() {
                    Value::fixnum(i32::try_from(n).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)
                } else if model.is_long(*x) || model.is_bigint(*x) {
                    Ok(*x)
                } else if model.is_str(*x) {
                    let raw = model.str_value(*x).map(String::from).unwrap_or_default();
                    let cleaned = strip_underscores(raw.trim());
                    if let Some(n) = cleaned.as_deref().and_then(|s| s.parse::<i128>().ok()) {
                        model.new_long(n)
                    } else if let Some(big) = cleaned.as_deref().and_then(BigInt::from_decimal_str) {
                        model.new_bigint(big)
                    } else {
                        let message = alloc::format!(
                            "invalid literal for int() with base 10: {}",
                            model.repr(*x)
                        );
                        Err(model.with_message(Trap::ValueError, &message))
                    }
                } else {
                    Err(Trap::TypeError)
                }
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::Float => match args {
            [] => model.new_float(0.0),
            [x] => {
                if let Some(f) = model.as_f64(*x) {
                    model.new_float(f)
                } else if model.is_str(*x) {
                    let text = model.str_value(*x).map(String::from).unwrap_or_default();
                    match parse_python_float(&text) {
                        Some(f) => model.new_float(f),
                        None => {
                            let message = alloc::format!(
                                "could not convert string to float: {}",
                                model.repr(*x)
                            );
                            Err(model.with_message(Trap::ValueError, &message))
                        }
                    }
                } else {
                    Err(Trap::TypeError)
                }
            }
            _ => Err(Trap::TypeError),
        },
        #[cfg(feature = "complex")]
        Builtin::Complex => match args {
            [] => model.new_complex(0.0, 0.0),
            [x] => {
                if let Some((re, im)) = model.as_complex(*x) {
                    model.new_complex(re, im)
                } else if model.is_str(*x) {
                    let text = model.str_value(*x).map(String::from).unwrap_or_default();
                    match parse_python_complex(&text) {
                        Some((re, im)) => model.new_complex(re, im),
                        None => Err(model.with_message(
                            Trap::ValueError,
                            "complex() arg is a malformed string",
                        )),
                    }
                } else {
                    Err(Trap::TypeError)
                }
            }
            [re, im] => {
                let (ar, ai) = model.as_complex(*re).ok_or(Trap::TypeError)?;
                let (br, bi) = model.as_complex(*im).ok_or(Trap::TypeError)?;
                if model.is_complex(*re) || model.is_complex(*im) {
                    model.new_complex(ar - bi, ai + br)
                } else {
                    model.new_complex(ar, br)
                }
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::Iter => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            iterator_for(args[0], functions, model, depth)
        }
        Builtin::Set => {
            let elems = collect_iterable(model, args, functions, depth)?;
            model.new_set(elems)
        }
        Builtin::Map => {
            if args.len() < 2 {
                return Err(Trap::TypeError);
            }
            let sources = source_iters(model, &args[1..], functions, depth)?;
            model.new_lazy_iter(LAZY_MAP, args[0], sources)
        }
        Builtin::Filter => {
            if args.len() != 2 {
                return Err(Trap::TypeError);
            }
            let sources = source_iters(model, &args[1..2], functions, depth)?;
            model.new_lazy_iter(LAZY_FILTER, args[0], sources)
        }
        Builtin::Zip => {
            let sources = source_iters(model, args, functions, depth)?;
            model.new_lazy_iter(LAZY_ZIP, Value::NONE, sources)
        }
        Builtin::Any => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let elements = collect_iterable(model, args, functions, depth)?;
            for element in elements {
                if model.py_truthy(element)?.unwrap_or(true) {
                    return Ok(Value::TRUE);
                }
            }
            Ok(Value::FALSE)
        }
        Builtin::All => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let elements = collect_iterable(model, args, functions, depth)?;
            for element in elements {
                if !model.py_truthy(element)?.unwrap_or(true) {
                    return Ok(Value::FALSE);
                }
            }
            Ok(Value::TRUE)
        }
        Builtin::Dict => match args {
            [] => model.new_dict(Vec::new()),
            [arg] => {
                if model.is_dict(*arg) {
                    let copy = model.dict_entries(*arg).unwrap_or_default();
                    model.new_dict(copy)
                } else {
                    let pairs = collect_iterable(model, &[*arg], functions, depth)?;
                    let mut kv = Vec::with_capacity(pairs.len());
                    for pair in pairs {
                        let parts = model.unpack_sequence(pair, 2)?;
                        kv.push((parts[0], parts[1]));
                    }
                    model.new_dict(kv)
                }
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::Reversed => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let arg = args[0];
            let reversible = model.str_value(arg).is_some()
                || model.is_list(arg)
                || model.is_tuple(arg)
                || model.is_range(arg)
                || model.is_dict(arg);
            if !reversible {
                return Err(Trap::TypeError);
            }
            let mut elements = collect_iterable(model, &[arg], functions, depth)?;
            elements.reverse();
            let list = model.new_list(elements)?;
            model.new_iter(list)
        }
        Builtin::Chr => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let code = args[0].as_int().ok_or(Trap::TypeError)?;
            let ch = u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .ok_or(Trap::ValueError)?;
            let mut buf = [0u8; 4];
            model.new_str(ch.encode_utf8(&mut buf))
        }
        Builtin::Ord => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let s = model.str_value(args[0]).ok_or(Trap::TypeError)?;
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Value::fixnum(c as i32).ok_or(Trap::Overflow),
                _ => Err(Trap::TypeError),
            }
        }
        Builtin::Divmod => {
            let [a, b] = args else {
                return Err(Trap::TypeError);
            };
            let quotient = binary(BinOp::FloorDiv, *a, *b, model)?;
            let remainder = binary(BinOp::Mod, *a, *b, model)?;
            model.new_tuple(alloc::vec![quotient, remainder])
        }
        Builtin::Pow => match args {
            [base, exp] => {
                if model.is_float(*base) || model.is_float(*exp) {
                    let b = model.as_f64(*base).ok_or(Trap::TypeError)?;
                    let e = model.as_f64(*exp).ok_or(Trap::TypeError)?;
                    return crate::interp::float_pow(b, e, model);
                }
                let e = exp.as_int().ok_or(Trap::TypeError)?;
                if e < 0 {
                    let b = model.as_f64(*base).ok_or(Trap::TypeError)?;
                    return crate::interp::float_pow(b, e as f64, model);
                }
                let exp_u = u32::try_from(e).map_err(|_| Trap::Overflow)?;
                if let Some(b) = model.as_i128(*base) {
                    if let Some(result) = b.checked_pow(exp_u) {
                        return model.new_long(result);
                    }
                }
                let base_big = model.as_bigint(*base).ok_or(Trap::TypeError)?;
                model.new_bigint(bigint_pow(&base_big, exp_u))
            }
            [base, exp, modulus] => {
                let base = model.as_bigint(*base).ok_or(Trap::TypeError)?;
                let exponent = exp.as_int().ok_or(Trap::TypeError)?;
                let modulus = model.as_bigint(*modulus).ok_or(Trap::TypeError)?;
                if exponent < 0 || modulus.is_zero() {
                    return Err(Trap::ValueError);
                }
                let reduce = |x: &BigInt| x.divmod(&modulus).map(|(_, r)| r).ok_or(Trap::ValueError);
                let mut acc = reduce(&BigInt::from_i128(1))?;
                let mut base_mod = reduce(&base)?;
                let mut bits = exponent as u64;
                while bits > 0 {
                    if bits & 1 == 1 {
                        acc = reduce(&acc.mul(&base_mod))?;
                    }
                    base_mod = reduce(&base_mod.mul(&base_mod))?;
                    bits >>= 1;
                }
                model.new_bigint(acc)
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::Hex => format_radix(model, args, "0x", 16),
        Builtin::Bin => format_radix(model, args, "0b", 2),
        Builtin::Oct => format_radix(model, args, "0o", 8),
        Builtin::Frozenset => {
            let elems = collect_iterable(model, args, functions, depth)?;
            model.new_frozenset(elems)
        }
        Builtin::Callable => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let x = args[0];
            let callable = x.as_function_index().is_some()
                || x.as_builtin_id().is_some()
                || model.is_class(x)
                || model.is_bound_method(x)
                || model.is_py_bound(x)
                || model.is_py_function(x);
            Ok(Value::from_bool(callable))
        }
        Builtin::Next => {
            let (iterator, default) = match args {
                [it] => (*it, None),
                [it, d] => (*it, Some(*d)),
                _ => return Err(Trap::TypeError),
            };
            if !model.is_iter(iterator)
                && !model.is_generator(iterator)
                && !model.is_lazy_iter(iterator)
                && model.find_dunder(iterator, "__next__").is_none()
            {
                return Err(Trap::TypeError);
            }
            match py_next_value(iterator, functions, model, depth)? {
                Some(value) => Ok(value),
                None => match default {
                    Some(d) => Ok(d),
                    None => {
                        let class = model
                            .exception_class("StopIteration")
                            .ok_or(Trap::Malformed)?;
                        let instance = model.new_object(class)?;
                        model.set_pending_exception(instance);
                        Err(Trap::Raised)
                    }
                },
            }
        }
        Builtin::SleepMs => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let ms = args[0].as_int().ok_or(Trap::TypeError)?;
            model.delay_ms(u32::try_from(ms).unwrap_or(0));
            Ok(Value::NONE)
        }
        Builtin::Round => {
            let (value, ndigits) = match args {
                [x] => (*x, None),
                [x, n] => (*x, Some(n.as_int().ok_or(Trap::TypeError)?)),
                _ => return Err(Trap::TypeError),
            };
            if let Some(f) = model.float_value(value) {
                round_float(f, ndigits, model)
            } else if let Some(x) = model.as_i128(value) {
                model.new_long(round_half_even(x, ndigits.unwrap_or(0)))
            } else {
                Err(Trap::TypeError)
            }
        }
        Builtin::Isinstance => {
            let [value, classinfo] = args else {
                return Err(Trap::TypeError);
            };
            Ok(Value::from_bool(isinstance_of(*value, *classinfo, model)?))
        }
        Builtin::Issubclass => {
            let [cls, classinfo] = args else {
                return Err(Trap::TypeError);
            };
            Ok(Value::from_bool(issubclass_of(*cls, *classinfo, model)?))
        }
        Builtin::Slice => {
            let (start, stop, step) = match args {
                [stop] => (Value::NONE, *stop, Value::NONE),
                [start, stop] => (*start, *stop, Value::NONE),
                [start, stop, step] => (*start, *stop, *step),
                _ => return Err(Trap::TypeError),
            };
            model.new_slice(start, stop, step)
        }
        Builtin::Staticmethod => {
            let [func] = args else { return Err(Trap::TypeError); };
            model.new_method_wrapper(*func, false)
        }
        Builtin::Classmethod => {
            let [func] = args else { return Err(Trap::TypeError); };
            model.new_method_wrapper(*func, true)
        }
        Builtin::Memoryview => {
            let [obj] = args else { return Err(Trap::TypeError); };
            let len = model.bytes_value(*obj).map(<[u8]>::len).ok_or(Trap::TypeError)?;
            model.new_memoryview(*obj, 0, len)
        }
        Builtin::Format => {
            let (value, spec) = match args {
                [v] => (*v, String::new()),
                [v, s] => (*v, model.str_value(*s).map(String::from).ok_or(Trap::TypeError)?),
                _ => return Err(Trap::TypeError),
            };
            if let Some(method) = model.find_dunder(value, "__format__") {
                let spec_value = model.new_str(&spec)?;
                let result = call_value(method, &[spec_value], functions, model, depth + 1)?;
                return Ok(result);
            }
            let rendered = model.format_value_spec(value, &spec)?;
            model.new_str(&rendered)
        }
        Builtin::IntFromBytes => {
            let (bytes, byteorder) = match args {
                [b, order] => (
                    model.bytes_value(*b).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?,
                    model.str_value(*order).map(String::from).ok_or(Trap::TypeError)?,
                ),
                _ => return Err(Trap::TypeError),
            };
            let ordered: alloc::vec::Vec<u8> = match byteorder.as_str() {
                "big" => bytes,
                "little" => bytes.into_iter().rev().collect(),
                _ => return Err(Trap::ValueError),
            };
            let base = crate::bigint::BigInt::from_i128(256);
            let mut result = crate::bigint::BigInt::from_i128(0);
            for byte in &ordered {
                result = result.mul(&base).add(&crate::bigint::BigInt::from_i128(i128::from(*byte)));
            }
            model.new_bigint(result)
        }
        Builtin::BytesFromhex => {
            let [s] = args else { return Err(Trap::TypeError); };
            let hex = model.str_value(*s).map(String::from).ok_or(Trap::TypeError)?;
            let digits: alloc::vec::Vec<char> = hex.chars().filter(|c| !c.is_whitespace()).collect();
            if digits.len() % 2 != 0 {
                return Err(Trap::ValueError);
            }
            let mut bytes = alloc::vec::Vec::with_capacity(digits.len() / 2);
            for pair in digits.chunks(2) {
                let hi = pair[0].to_digit(16).ok_or(Trap::ValueError)?;
                let lo = pair[1].to_digit(16).ok_or(Trap::ValueError)?;
                bytes.push((hi * 16 + lo) as u8);
            }
            model.new_bytes(bytes)
        }
        Builtin::Type => {
            let [value] = args else {
                return Err(Trap::TypeError);
            };
            type_of(*value, model).ok_or(Trap::Unsupported)
        }
        Builtin::Getattr => {
            let (obj, name, default) = match args {
                [obj, name] => (*obj, *name, None),
                [obj, name, default] => (*obj, *name, Some(*default)),
                _ => return Err(Trap::TypeError),
            };
            let attr = String::from(model.str_value(name).ok_or(Trap::TypeError)?);
            let mut cache = InlineCache::empty();
            match model.getattr(obj, &attr, &mut cache) {
                Ok(value) => Ok(value),
                Err(Trap::AttributeError) => default.ok_or(Trap::AttributeError),
                Err(other) => Err(other),
            }
        }
        Builtin::Hasattr => {
            let [obj, name] = args else {
                return Err(Trap::TypeError);
            };
            let attr = String::from(model.str_value(*name).ok_or(Trap::TypeError)?);
            let mut cache = InlineCache::empty();
            match model.getattr(*obj, &attr, &mut cache) {
                Ok(_) => Ok(Value::TRUE),
                Err(Trap::AttributeError) => Ok(Value::FALSE),
                Err(other) => Err(other),
            }
        }
        Builtin::Setattr => {
            let [obj, name, value] = args else {
                return Err(Trap::TypeError);
            };
            let attr = String::from(model.str_value(*name).ok_or(Trap::TypeError)?);
            if model.is_instance(*obj) {
                model.py_setattr_instance(*obj, &attr, *value)?;
            } else {
                model.py_setattr_native(*obj, &attr, *value)?;
            }
            Ok(Value::NONE)
        }
        Builtin::Delattr => {
            let [obj, name] = args else {
                return Err(Trap::TypeError);
            };
            let attr = String::from(model.str_value(*name).ok_or(Trap::TypeError)?);
            model.py_delattr_instance(*obj, &attr)?;
            Ok(Value::NONE)
        }
        Builtin::Hash => {
            let [x] = args else {
                return Err(Trap::TypeError);
            };
            if let Some(method) = model.find_dunder(*x, "__hash__") {
                return call_value(method, &[], functions, model, depth + 1);
            }
            model.py_hash(*x)
        }
        Builtin::Id => {
            let [x] = args else {
                return Err(Trap::TypeError);
            };
            model.new_bigint(crate::bigint::BigInt::from_i128(i128::from(x.bits())))
        }
        Builtin::Bytes => {
            let data = bytes_from_args(args, model, functions, depth)?;
            model.new_bytes(data)
        }
        Builtin::Bytearray => {
            let data = bytes_from_args(args, model, functions, depth)?;
            model.new_bytearray(data)
        }
        Builtin::DictFromkeys => {
            let (iterable, value) = match args {
                [it] => (*it, Value::NONE),
                [it, v] => (*it, *v),
                _ => return Err(Trap::TypeError),
            };
            model.new_dict_fromkeys(iterable, value)
        }
    }
}

/// `isinstance(value, classinfo)`: whether `value` is of type `classinfo` -- a built-in type
/// constructor (`int`/`str`/`list`/`tuple`/`dict`/`set`/`frozenset`/`bool`) used as a type, a user
/// class (checked against its base chain), or a tuple of those (any match). A non-type `classinfo`
/// is a `TypeError`.
fn isinstance_of(value: Value, classinfo: Value, model: &ObjectModel) -> Result<bool, Trap> {
    if model.is_tuple(classinfo) {
        for class in model.seq_elements(classinfo).unwrap_or_default() {
            if isinstance_of(value, class, model)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if let Some(id) = classinfo.as_builtin_id() {
        let matches = match Builtin::from_id(id) {
            Some(Builtin::Bool) => value == Value::TRUE || value == Value::FALSE,
            Some(Builtin::Int) => model.is_int(value),
            Some(Builtin::Float) => model.is_float(value),
            #[cfg(feature = "complex")]
            Some(Builtin::Complex) => model.is_complex(value),
            Some(Builtin::Str) => model.is_str(value),
            Some(Builtin::Bytes) => model.is_bytes(value),
            Some(Builtin::Bytearray) => model.is_bytearray(value),
            Some(Builtin::Memoryview) => model.is_memoryview(value),
            Some(Builtin::List) => model.is_list(value),
            Some(Builtin::Tuple) => model.is_tuple(value),
            Some(Builtin::Dict) => model.is_dict(value),
            Some(Builtin::Set) => model.is_set(value),
            Some(Builtin::Frozenset) => model.is_frozenset(value),
            _ => return Err(Trap::TypeError),
        };
        return Ok(matches);
    }
    if model.is_class(classinfo) {
        return Ok(model.is_instance_of(value, classinfo));
    }
    Err(Trap::TypeError)
}

/// Whether `cls` is a subclass of `classinfo` (a class, a built-in type, or a tuple). `cls` must be
/// a class/type (else a `TypeError`). Built-in types follow the subtype rules (`bool` <: `int`,
/// otherwise identity); a user class walks its base chain. (There is no `object` root type, so a
/// user class is not reported as a subclass of a built-in type -- a small documented divergence.)
fn issubclass_of(cls: Value, classinfo: Value, model: &ObjectModel) -> Result<bool, Trap> {
    let is_type = |v: Value| {
        model.is_class(v) || v.as_builtin_id().and_then(Builtin::from_id).is_some_and(Builtin::is_type)
    };
    if !is_type(cls) {
        return Err(Trap::TypeError);
    }
    if model.is_tuple(classinfo) {
        for target in model.seq_elements(classinfo).unwrap_or_default() {
            if issubclass_of(cls, target, model)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if !is_type(classinfo) {
        return Err(Trap::TypeError);
    }
    if model.is_class(cls) && model.is_class(classinfo) {
        return Ok(model.is_subclass_of(cls, classinfo));
    }
    if let (Some(a), Some(b)) = (cls.as_builtin_id(), classinfo.as_builtin_id()) {
        return Ok(match (Builtin::from_id(a), Builtin::from_id(b)) {
            (Some(x), Some(y)) if x == y => true,
            (Some(Builtin::Bool), Some(Builtin::Int)) => true,
            _ => false,
        });
    }
    Ok(false)
}

/// `round(x, ndigits)` for an integer `x`, returning an integer (as CPython does for an int input):
/// a non-negative `ndigits` leaves `x` unchanged; a negative `ndigits` rounds to the nearest
/// 10^(-ndigits) with round-half-to-even (banker's rounding). A scale beyond i128 rounds to 0.
fn round_half_even(x: i128, ndigits: i64) -> i128 {
    if ndigits >= 0 {
        return x;
    }
    let Some(factor) = 10i128.checked_pow((-ndigits) as u32) else {
        return 0;
    };
    let down = x.div_euclid(factor) * factor;
    let remainder = x.rem_euclid(factor);
    let half = factor / 2;
    if remainder < half {
        down
    } else if remainder > half {
        down + factor
    } else if (down / factor) % 2 == 0 {
        down
    } else {
        down + factor
    }
}

/// Calls a built-in with KEYWORD arguments -- the `Op::CallKw` path for a built-in callee. Only the
/// built-ins with a keyword surface are handled (`sorted(key=, reverse=)`, `dict(**kwargs)`); the
/// rest reject an unexpected keyword with a `TypeError`, as CPython does. Positional built-in calls
/// go through [`call_builtin`]. `kwargs` is non-empty (a keyword call emits `CallKw`).
pub fn call_builtin_kw(
    id: u32,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    match Builtin::from_id(id).ok_or(Trap::Malformed)? {
        Builtin::Sorted => sorted_kw(posargs, kwargs, functions, model, depth),
        Builtin::Dict => dict_kw(posargs, kwargs, functions, model, depth),
        Builtin::Min => min_max_kw(posargs, kwargs, Ordering::Less, functions, model, depth),
        Builtin::Max => min_max_kw(posargs, kwargs, Ordering::Greater, functions, model, depth),
        Builtin::Print => print_kw(posargs, kwargs, functions, model, depth),
        _ => Err(Trap::TypeError),
    }
}

/// `print(*args, sep=' ', end='\n')`: the args rendered with str() and joined by `sep`, then `end`.
/// A `str` or `None` (the default) `sep`/`end`; `flush=` is accepted and ignored; other keywords
/// (`file=`) are a `TypeError`.
fn print_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let separator = |value: Value, default: &str, model: &ObjectModel| -> Result<String, Trap> {
        if value.is_none() {
            Ok(String::from(default))
        } else {
            model.str_value(value).map(String::from).ok_or(Trap::TypeError)
        }
    };
    let mut sep = String::from(" ");
    let mut end = String::from("\n");
    for &(name, value) in kwargs {
        match name {
            "sep" => sep = separator(value, " ", model)?,
            "end" => end = separator(value, "\n", model)?,
            "flush" => {}
            _ => return Err(Trap::TypeError),
        }
    }
    let mut out = String::new();
    for (i, arg) in posargs.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        out.push_str(&display_arg(*arg, functions, model, depth)?);
    }
    out.push_str(&end);
    model.write(&out);
    Ok(Value::NONE)
}

/// `sorted(iterable, *, key=None, reverse=False)`: collect the iterable, optionally map each element
/// through `key`, and sort by the (int/str) keys -- descending, ties-original-order, when `reverse`.
fn sorted_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let [iterable] = posargs else {
        return Err(Trap::TypeError);
    };
    let mut key = Value::NONE;
    let mut reverse = false;
    for &(name, value) in kwargs {
        match name {
            "key" => key = value,
            "reverse" => reverse = model.py_truthy(value)?.unwrap_or(true),
            _ => return Err(Trap::TypeError),
        }
    }
    let elements = collect_iterable(model, &[*iterable], functions, depth)?;
    if key.is_none() {
        let mut sorted = elements;
        if sorted.iter().any(|&e| model.is_instance(e)) {
            sort_values_dyn(&mut sorted, functions, model, depth)?;
        } else {
            model.sort_values(&mut sorted)?;
        }
        if reverse {
            sorted.reverse();
        }
        model.new_list(sorted)
    } else {
        let mut pairs = Vec::with_capacity(elements.len());
        for element in elements {
            let k = call_value(key, &[element], functions, model, depth)?;
            pairs.push((k, element));
        }
        model.sort_pairs_by_key(&mut pairs, reverse)?;
        let sorted: Vec<Value> = pairs.into_iter().map(|(_, element)| element).collect();
        model.new_list(sorted)
    }
}

/// `dict(**kwargs)` / `dict(mapping_or_pairs, **kwargs)`: a dict whose keyword arguments become
/// string-keyed entries (a later keyword updates an existing key), starting from the optional
/// positional mapping / iterable-of-pairs.
fn dict_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let mut entries: Vec<(Value, Value)> = match posargs {
        [] => Vec::new(),
        [arg] if model.is_dict(*arg) => model.dict_entries(*arg).unwrap_or_default(),
        [arg] => {
            let pairs = collect_iterable(model, &[*arg], functions, depth)?;
            let mut kv = Vec::with_capacity(pairs.len());
            for pair in pairs {
                let parts = model.unpack_sequence(pair, 2)?;
                kv.push((parts[0], parts[1]));
            }
            kv
        }
        _ => return Err(Trap::TypeError),
    };
    for &(name, value) in kwargs {
        match entries.iter().position(|(k, _)| model.str_value(*k) == Some(name)) {
            Some(i) => entries[i].1 = value,
            None => {
                let key = model.new_str(name)?;
                entries.push((key, value));
            }
        }
    }
    model.new_dict(entries)
}

/// Formats the single int argument in `radix` with `prefix`, using Python's sign convention
/// (`-0x..` for negatives, never a two's-complement form).
fn format_radix(
    model: &mut ObjectModel,
    args: &[Value],
    prefix: &str,
    radix: u8,
) -> Result<Value, Trap> {
    let [arg] = args else {
        return Err(Trap::TypeError);
    };
    let n = arg.as_int().ok_or(Trap::TypeError)?;
    let (sign, mag) = if n < 0 {
        ("-", n.unsigned_abs())
    } else {
        ("", n as u64)
    };
    let body = match radix {
        16 => alloc::format!("{mag:x}"),
        8 => alloc::format!("{mag:o}"),
        _ => alloc::format!("{mag:b}"),
    };
    let rendered = alloc::format!("{sign}{prefix}{body}");
    model.new_str(&rendered)
}

/// Calls a no-argument, string-returning dunder (`__str__`/`__repr__`) and returns its result as a
/// String (falling back to the default rendering if it does not return a str).
fn call_str_dunder(
    method: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<String, Trap> {
    let result = call_value(method, &[], functions, model, depth)?;
    Ok(model
        .str_value(result)
        .map(String::from)
        .unwrap_or_else(|| model.display(result)))
}

/// The display form of `value` for `print`/`str`: an instance's `__str__`, else its `__repr__`
/// (Python's `str` falls back to `repr`), else the default rendering (int/str/container/...).
fn display_arg(
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<String, Trap> {
    if let Some(method) = model.find_dunder(value, "__str__") {
        return call_str_dunder(method, functions, model, depth);
    }
    if let Some(method) = model.find_dunder(value, "__repr__") {
        return call_str_dunder(method, functions, model, depth);
    }
    if model.seq_value(value).is_some()
        || model.set_value(value).is_some()
        || model.dict_value(value).is_some()
    {
        return repr_arg(value, functions, model, depth);
    }
    Ok(model.display(value))
}

/// The repr form of `value` for `repr()`: an instance's `__repr__`, else the default repr. Recurses
/// into containers so a nested instance uses its own `__repr__` (`print([obj])`), which `model.repr`
/// cannot do without the interpreter. A self-referential container is a `RecursionError`.
fn repr_arg(
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<String, Trap> {
    if depth > 256 {
        return Err(Trap::RecursionError);
    }
    if let Some(method) = model.find_dunder(value, "__repr__") {
        return call_str_dunder(method, functions, model, depth);
    }
    if let Some(elems) = model.seq_value(value).cloned() {
        let is_tuple = model.is_tuple(value);
        let mut parts = Vec::with_capacity(elems.len());
        for element in &elems {
            parts.push(repr_arg(*element, functions, model, depth + 1)?);
        }
        let inner = parts.join(", ");
        return Ok(if is_tuple {
            if elems.len() == 1 {
                alloc::format!("({inner},)")
            } else {
                alloc::format!("({inner})")
            }
        } else {
            alloc::format!("[{inner}]")
        });
    }
    if let Some(elements) = model.set_value(value).cloned() {
        let frozen = model.is_frozenset(value);
        if elements.is_empty() {
            return Ok(String::from(if frozen { "frozenset()" } else { "set()" }));
        }
        let mut parts = Vec::with_capacity(elements.len());
        for element in &elements {
            parts.push(repr_arg(*element, functions, model, depth + 1)?);
        }
        let inner = parts.join(", ");
        return Ok(if frozen {
            alloc::format!("frozenset({{{inner}}})")
        } else {
            alloc::format!("{{{inner}}}")
        });
    }
    if let Some(entries) = model.dict_value(value).cloned() {
        let mut parts = Vec::with_capacity(entries.len());
        for (key, val) in &entries {
            let key = repr_arg(*key, functions, model, depth + 1)?;
            let val = repr_arg(*val, functions, model, depth + 1)?;
            parts.push(alloc::format!("{key}: {val}"));
        }
        return Ok(alloc::format!("{{{}}}", parts.join(", ")));
    }
    Ok(model.repr(value))
}

/// Sorts the collected elements into a new list: int elements numerically, str lexicographically;
/// a mixed or otherwise unorderable set is a `TypeError`.
/// `a < b`, honoring a user class's `__lt__` (via the reflected comparison protocol), else the
/// built-in ordering (`compare_ordered` over int/str/tuple).
fn less_than_dyn(
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<bool, Trap> {
    if let Some(value) = crate::interp::try_compare_dunder(CmpOp::Lt, a, b, functions, model, depth)? {
        return Ok(value == Value::TRUE);
    }
    Ok(model.compare_ordered(a, b)? == Ordering::Less)
}

/// Orders `a` vs `b` for min/max, honoring `__lt__`: Less if `a<b`, Greater if `b<a`, else Equal.
fn compare_dyn(
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Ordering, Trap> {
    if less_than_dyn(a, b, functions, model, depth)? {
        Ok(Ordering::Less)
    } else if less_than_dyn(b, a, functions, model, depth)? {
        Ok(Ordering::Greater)
    } else {
        Ok(Ordering::Equal)
    }
}

/// A stable insertion sort using `__lt__`-aware comparison -- the path when instances are present
/// (the model's fast `compare_ordered` sort has no interpreter context to call `__lt__`). O(n^2),
/// fine at teaching scale.
fn sort_values_dyn(
    elements: &mut [Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<(), Trap> {
    for i in 1..elements.len() {
        let mut j = i;
        while j > 0 && less_than_dyn(elements[j], elements[j - 1], functions, model, depth)? {
            elements.swap(j, j - 1);
            j -= 1;
        }
    }
    Ok(())
}

/// Sorts `elements` into a new list: `__lt__`-aware when any element is an instance, else the fast
/// built-in ordering.
fn sorted_list(
    mut elements: Vec<Value>,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if elements.iter().any(|&e| model.is_instance(e)) {
        sort_values_dyn(&mut elements, functions, model, depth)?;
    } else {
        model.sort_values(&mut elements)?;
    }
    model.new_list(elements)
}

/// Builds the byte buffer for `bytes(...)` / `bytearray(...)`: no arg -> empty; an int `n` -> `n`
/// zero bytes; a `bytes`/`bytearray` -> a copy; an iterable of ints in `range(0, 256)`; or
/// `(str, encoding)` -> the string encoded (utf-8 / ascii). A non-int element is a `TypeError`, an
/// out-of-range int a `ValueError`, an unsupported encoding `Unsupported`.
fn bytes_from_args(
    args: &[Value],
    model: &mut ObjectModel,
    functions: &[CodeObject],
    depth: usize,
) -> Result<Vec<u8>, Trap> {
    match args {
        [] => Ok(Vec::new()),
        [source] => {
            if let Some(n) = source.as_int() {
                if n < 0 {
                    return Err(Trap::ValueError);
                }
                return Ok(alloc::vec![0u8; n as usize]);
            }
            if let Some(data) = model.bytes_value(*source) {
                return Ok(data.to_vec());
            }
            if model.is_str(*source) {
                return Err(Trap::TypeError);
            }
            let elements = collect_iterable(model, &[*source], functions, depth)?;
            let mut out = Vec::with_capacity(elements.len());
            for element in elements {
                let byte = element.as_int().ok_or(Trap::TypeError)?;
                if !(0..=255).contains(&byte) {
                    return Err(Trap::ValueError);
                }
                out.push(byte as u8);
            }
            Ok(out)
        }
        [source, encoding] => {
            let text = model.str_value(*source).map(String::from).ok_or(Trap::TypeError)?;
            let name = model.str_value(*encoding).map(String::from).ok_or(Trap::TypeError)?;
            match name.to_ascii_lowercase().replace('-', "").as_str() {
                "utf8" | "ascii" => Ok(text.into_bytes()),
                _ => Err(Trap::Unsupported),
            }
        }
        _ => Err(Trap::TypeError),
    }
}

/// Builds the sources tuple for a lazy `map`/`filter`/`zip`: one live iterator per iterable argument
/// (so they advance in lock-step on demand rather than being materialized up front).
fn source_iters(
    model: &mut ObjectModel,
    iterables: &[Value],
    functions: &[CodeObject],
    depth: usize,
) -> Result<Value, Trap> {
    let mut iters = Vec::with_capacity(iterables.len());
    for iterable in iterables {
        iters.push(iterator_for(*iterable, functions, model, depth)?);
    }
    model.new_tuple(iters)
}

/// Drains `list(...)`/`tuple(...)`'s optional single iterable argument into a vector (empty for
/// the no-argument form), via the iterator protocol so any iterable works. Also the RHS collector
/// for slice assignment.
pub(crate) fn collect_iterable(
    model: &mut ObjectModel,
    args: &[Value],
    functions: &[CodeObject],
    depth: usize,
) -> Result<Vec<Value>, Trap> {
    let iterable = match args {
        [] => return Ok(Vec::new()),
        [iterable] => *iterable,
        _ => return Err(Trap::TypeError),
    };
    let iterator = iterator_for(iterable, functions, model, depth)?;
    let mut elems = Vec::new();
    while let Some(item) = py_next_value(iterator, functions, model, depth)? {
        elems.push(item);
    }
    Ok(elems)
}

/// Parses a `str` the way Python's `float()` does: surrounding whitespace stripped, an optional
/// leading sign, decimal/exponent forms (`.5`, `5.`, `1e10`), the `inf`/`infinity`/`nan` literals
/// (case-insensitive), and `_` digit separators. `None` for anything else (the caller raises a
/// `ValueError`). Rust's `f64` parser already accepts exactly this grammar EXCEPT the underscores
/// and whitespace, which are handled here.
fn parse_python_float(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    strip_underscores(trimmed)?.parse::<f64>().ok()
}

/// Removes Python's numeric `_` separators, which must sit BETWEEN two digits (`1_000` is valid,
/// `_1`/`1_`/`1__0`/`1_.0` are not). `None` for a misplaced underscore; the original string when
/// there is none.
fn strip_underscores(s: &str) -> Option<String> {
    if !s.contains('_') {
        return Some(String::from(s));
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'_' {
            let prev = i.checked_sub(1).map(|j| bytes[j]);
            let next = bytes.get(i + 1).copied();
            if !prev.is_some_and(|c| c.is_ascii_digit()) || !next.is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
        } else {
            out.push(b as char);
        }
    }
    Some(out)
}

/// Parses a `str` the way Python's `complex()` does: optional surrounding whitespace and one layer
/// of parentheses, then `real`, `imagj`, or `real±imagj` (the `j`/`J` suffix marks the imaginary
/// term; a bare `j` is `1j`), each part a float grammar (incl. `inf`/`nan`, `_` separators). `None`
/// for a malformed string (the caller raises a `ValueError`).
#[cfg(feature = "complex")]
fn parse_python_complex(s: &str) -> Option<(f64, f64)> {
    let mut trimmed = s.trim();
    if let Some(inner) = trimmed.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        trimmed = inner.trim();
    }
    if trimmed.is_empty() {
        return None;
    }
    let cleaned = strip_underscores(trimmed)?;
    let text = cleaned.as_str();
    let Some(body) = text.strip_suffix(['j', 'J']) else {
        return Some((parse_python_float(text)?, 0.0));
    };
    let bytes = body.as_bytes();
    let split = (1..bytes.len()).rev().find(|&i| {
        matches!(bytes[i], b'+' | b'-') && !matches!(bytes[i - 1], b'e' | b'E')
    });
    match split {
        Some(i) => Some((parse_python_float(&body[..i])?, parse_imag_coeff(&body[i..])?)),
        None => Some((0.0, parse_imag_coeff(body)?)),
    }
}

/// The coefficient of an imaginary term (the text before `j`): `""`/`"+"` -> `1`, `"-"` -> `-1`
/// (a bare or signed `j`), else a float.
#[cfg(feature = "complex")]
fn parse_imag_coeff(s: &str) -> Option<f64> {
    match s {
        "" | "+" => Some(1.0),
        "-" => Some(-1.0),
        _ => parse_python_float(s),
    }
}

/// `round(float[, ndigits])`: with no `ndigits`, the nearest integer with ties to even, as an `int`
/// (`round(2.5) == 2`); with `ndigits`, the value rounded to that many places, as a `float`
/// (`round(2.675, 2) == 2.67`). A non-finite float with no `ndigits` has no integer value.
fn round_float(f: f64, ndigits: Option<i64>, model: &mut ObjectModel) -> Result<Value, Trap> {
    match ndigits {
        None => {
            if f.is_nan() {
                return Err(model.with_message(Trap::ValueError, "cannot convert float NaN to integer"));
            }
            if f.is_infinite() {
                return Err(model.with_message(Trap::Overflow, "cannot convert float infinity to integer"));
            }
            let rounded = round_half_even_f64(f);
            if !(-1.701_411_834_604_692_3e38..1.701_411_834_604_692_3e38).contains(&rounded) {
                return Err(Trap::Overflow);
            }
            model.new_long(rounded as i128)
        }
        Some(n) => model.new_float(round_to_digits(f, n)),
    }
}

/// The nearest integral `f64` to `f`, ties to even (`2.5 -> 2.0`, `3.5 -> 4.0`, `-0.5 -> 0.0`).
fn round_half_even_f64(f: f64) -> f64 {
    let floor = libm::floor(f);
    let diff = f - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else if libm::fmod(floor, 2.0) == 0.0 {
        floor
    } else {
        floor + 1.0
    }
}

/// `round(f, ndigits)` as a float. For `ndigits >= 0`, format to that many places -- Rust's
/// formatter rounds half-to-even on the EXACT value, matching CPython's `float.__round__` -- and
/// re-parse. For a negative `ndigits`, scale by `10^(-ndigits)`, round to even, and scale back.
fn round_to_digits(f: f64, ndigits: i64) -> f64 {
    if !f.is_finite() {
        return f;
    }
    if ndigits >= 0 {
        let places = ndigits.min(323) as usize;
        alloc::format!("{f:.places$}").parse::<f64>().unwrap_or(f)
    } else {
        let scale = libm::pow(10.0, (-ndigits) as f64);
        round_half_even_f64(f / scale) * scale
    }
}

/// `min`/`max` (`keep` = `Less` for min, `Greater` for max) over the positional arguments
/// (`max(a, b, ...)`, two or more) OR a single iterable (`max(iterable)`) -- Python's two call
/// forms. Orders by [`ObjectModel::compare_ordered`] (int/str/tuple), keeping the FIRST extreme
/// (Python's tie behavior). An empty iterable is a `ValueError`. The keyword form (`key=`/
/// `default=`) is [`min_max_kw`].
fn min_max(
    args: &[Value],
    keep: Ordering,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let elements = match args {
        [] => return Err(Trap::TypeError),
        [single] => collect_iterable(model, &[*single], functions, depth)?,
        many => many.to_vec(),
    };
    min_max_impl(elements, keep, Value::NONE, None, functions, model, depth)
}

/// `min`/`max` with keyword arguments (`key=None, default=<sentinel>`): a single iterable mapped
/// through `key` (each element's ordering key), returning `default` for an empty iterable (else a
/// `ValueError`), keeping the FIRST extreme on ties.
fn min_max_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    keep: Ordering,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let mut key = Value::NONE;
    let mut default = None;
    for &(name, value) in kwargs {
        match name {
            "key" => key = value,
            "default" => default = Some(value),
            _ => return Err(Trap::TypeError),
        }
    }
    let elements = match posargs {
        [] => return Err(Trap::TypeError),
        [single] => collect_iterable(model, &[*single], functions, depth)?,
        many => many.to_vec(),
    };
    min_max_impl(elements, keep, key, default, functions, model, depth)
}

/// The shared min/max fold: orders `elements` by their `key`-mapped ordering key (the element
/// itself when `key` is `None`), keeping the FIRST element whose key is the extreme (`keep`). An
/// empty input yields `default` (or a `ValueError` when there is none).
fn min_max_impl(
    elements: Vec<Value>,
    keep: Ordering,
    key: Value,
    default: Option<Value>,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let mapped = |element: Value, model: &mut ObjectModel| -> Result<Value, Trap> {
        if key.is_none() {
            Ok(element)
        } else {
            call_value(key, &[element], functions, model, depth)
        }
    };
    let mut iter = elements.into_iter();
    let mut best = match iter.next() {
        Some(first) => first,
        None => return default.ok_or(Trap::ValueError),
    };
    let mut best_key = mapped(best, model)?;
    for element in iter {
        let element_key = mapped(element, model)?;
        if compare_dyn(element_key, best_key, functions, model, depth)? == keep {
            best = element;
            best_key = element_key;
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_half_even_matches_python_bankers_rounding() {
        assert_eq!(round_half_even(17, 1), 17);
        assert_eq!(round_half_even(5, 0), 5);
        assert_eq!(round_half_even(-8, 0), -8);
        assert_eq!(round_half_even(123, -1), 120);
        assert_eq!(round_half_even(125, -1), 120);
        assert_eq!(round_half_even(135, -1), 140);
        assert_eq!(round_half_even(15, -1), 20);
        assert_eq!(round_half_even(25, -1), 20);
        assert_eq!(round_half_even(-15, -1), -20);
        assert_eq!(round_half_even(150, -2), 200);
        assert_eq!(round_half_even(250, -2), 200);
    }

    #[test]
    fn isinstance_of_builtin_types_and_tuples() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let five = Value::fixnum(5).unwrap();
        let text = model.new_str("x").unwrap();
        let int_ty = Value::builtin_ref(Builtin::Int.id());
        let str_ty = Value::builtin_ref(Builtin::Str.id());
        let bool_ty = Value::builtin_ref(Builtin::Bool.id());
        assert_eq!(isinstance_of(five, int_ty, &model), Ok(true));
        assert_eq!(isinstance_of(five, str_ty, &model), Ok(false));
        assert_eq!(isinstance_of(text, str_ty, &model), Ok(true));
        assert_eq!(isinstance_of(Value::TRUE, int_ty, &model), Ok(true));
        assert_eq!(isinstance_of(five, bool_ty, &model), Ok(false));
        let types = model.new_tuple(alloc::vec![int_ty, str_ty]).unwrap();
        assert_eq!(isinstance_of(text, types, &model), Ok(true));
        assert_eq!(isinstance_of(Value::NONE, types, &model), Ok(false));
        assert_eq!(
            isinstance_of(five, Value::builtin_ref(Builtin::Abs.id()), &model),
            Err(Trap::TypeError)
        );
    }

    #[test]
    fn type_of_maps_values_to_their_constructor_type() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let int_ty = Value::builtin_ref(Builtin::Int.id());
        assert_eq!(type_of(Value::fixnum(5).unwrap(), &model), Some(int_ty));
        assert_eq!(type_of(Value::TRUE, &model), Some(Value::builtin_ref(Builtin::Bool.id())));
        let text = model.new_str("x").unwrap();
        assert_eq!(type_of(text, &model), Some(Value::builtin_ref(Builtin::Str.id())));
        assert_eq!(type_of(Value::fixnum(1).unwrap(), &model), Some(int_ty));
        assert_eq!(type_of(Value::NONE, &model), None);
        assert_eq!(Builtin::Int.python_name(), "int");
        assert_eq!(Builtin::Abs.python_name(), "abs");
        assert!(Builtin::Int.is_type() && !Builtin::Abs.is_type());
    }
}
