//! The runtime's built-in functions -- the first slice of the `builtins` namespace.

use core::cmp::Ordering;

use alloc::string::String;
use alloc::vec::Vec;

use lamella_py_bytecode::{BinOp, CmpOp, CodeObject};

use crate::bigint::BigInt;
use crate::interp::{
    binary, bigint_pow, call_value, coerce_index, getattr_hooked, iterator_for, py_next_value,
    set_attr,
};
use crate::object::{
    DictViewKind, InlineCache, ObjectModel, LAZY_CALLABLE, LAZY_ENUMERATE, LAZY_FILTER, LAZY_MAP,
    LAZY_ZIP,
};
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
    /// `property(fget[, fset[, fdel]])` -- a computed-attribute descriptor.
    Property = 57,
    /// `ascii(obj)` -- like repr(), but escape every non-ASCII character.
    Ascii = 58,
    /// `str.maketrans(x, y[, z])` -- reached via the `str` type (not a global name).
    StrMaketrans = 59,
    /// `__match_class__(subject, cls, count)` -- the runtime half of a POSITIONAL class pattern
    /// (`case Cls(p0, ...)`): a compiler-internal helper (not user Python) the match desugar calls to
    /// extract the positional values to bind. Returns a `count`-tuple of the extracted values, or
    /// `None` on a non-match; raises TypeError on an arity mismatch. Follows CPython's `__match_args__`
    /// + builtin self-match rules.
    MatchClassPositional = 60,
    /// `type(None)` -- the type object of the `None` singleton (CPython's `NoneType`). Not a builtin
    /// NAME (unreachable as a bare `NoneType`); reached only via `type(None)`.
    NoneType = 61,
    /// `type(...)` -- the type object of the `Ellipsis` singleton (CPython's `ellipsis`). Not a
    /// builtin NAME; reached only via `type(...)`.
    EllipsisType = 62,
    /// `float.fromhex(s)` -- the classmethod parsing a hexadecimal floating-point string. Reached as
    /// a type-level attribute off `float`, not a bare builtin name.
    FloatFromhex = 63,
    /// `type(d.keys())` -- the `dict_keys` view type object. Not a builtin NAME; reached only via
    /// `type(...)`. Calling it raises (CPython: cannot create 'dict_keys' instances).
    DictKeysType = 64,
    /// `type(d.values())` -- the `dict_values` view type object (same reachability as DictKeysType).
    DictValuesType = 65,
    /// `type(d.items())` -- the `dict_items` view type object (same reachability as DictKeysType).
    DictItemsType = 66,
    /// `type(NotImplemented)` -- the type object of the `NotImplemented` singleton (CPython's
    /// `NotImplementedType`). Not a builtin NAME; reached only via `type(NotImplemented)`.
    NotImplementedType = 67,
    /// `vars(obj)` -- `obj.__dict__` (an instance/class/module namespace). The no-argument form
    /// (CPython's `locals()`) is unsupported here (no frame-locals access from a builtin).
    Vars = 68,
    /// `object` -- the universal base type. `object()` builds a bare, attribute-less instance whose
    /// only distinguishing trait is IDENTITY (the `_MISSING = object()` sentinel idiom); every value
    /// is an `isinstance` of it and every type an `issubclass`.
    Object = 69,
    /// `globals()` -- the current module's global namespace as a dict (a fresh snapshot each call, so
    /// it reflects state up to the call; not the live mapping object CPython returns).
    Globals = 70,
    /// `locals()` -- the current frame's local bindings as a dict. At module level it is the globals
    /// (as CPython). The no-argument call is intercepted at the call site (it needs the frame); a stray
    /// argument, or an indirect call with no visible frame, is handled here.
    Locals = 71,
    /// The type of a `def` or a `lambda` -- what `type(f)` answers. A TYPE OBJECT ONLY: no name binds
    /// to it (in CPython it is reached as `types.FunctionType`) and it constructs nothing.
    FunctionType = 72,
    /// The type of a built-in function and of a built-in method bound to a value (`len`, `[].append`,
    /// `"a".upper`) -- one type for both, as in CPython. A type object only.
    BuiltinFunctionType = 73,
    /// The type of a method bound to a user instance (`instance.method`). A type object only.
    MethodType = 74,
    /// The type of a generator object -- what calling a generator function returns. A type object only.
    GeneratorType = 75,
    /// The type of a module object. A type object only.
    ModuleType = 76,
    /// `open(path, mode='r')` -- the file object that reads or writes a host file. Refuses when the
    /// embedder installed no filesystem, rather than answering an empty read or dropping a write.
    Open = 77,
    /// `dir([obj])` -- the attribute names `obj` actually has, sorted. With no argument, the names
    /// bound in the calling frame (intercepted at the call site, which is where the frame is).
    Dir = 78,
    /// `(5).is_integer()` -- always True. An int IS an integer; the method exists so a caller holding
    /// a number can ask without knowing whether it is an int or a float.
    IntIsInteger = 79,
    /// The type of a coroutine object -- what calling an `async def` returns. A type object only.
    /// DISTINCT from [`Builtin::GeneratorType`] because a program can tell them apart: a coroutine is
    /// not an iterator, and `await` accepts one where `yield from` does not.
    CoroutineType = 80,
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
            57 => Some(Builtin::Property),
            58 => Some(Builtin::Ascii),
            59 => Some(Builtin::StrMaketrans),
            60 => Some(Builtin::MatchClassPositional),
            61 => Some(Builtin::NoneType),
            62 => Some(Builtin::EllipsisType),
            63 => Some(Builtin::FloatFromhex),
            64 => Some(Builtin::DictKeysType),
            65 => Some(Builtin::DictValuesType),
            66 => Some(Builtin::DictItemsType),
            67 => Some(Builtin::NotImplementedType),
            68 => Some(Builtin::Vars),
            69 => Some(Builtin::Object),
            70 => Some(Builtin::Globals),
            71 => Some(Builtin::Locals),
            72 => Some(Builtin::FunctionType),
            73 => Some(Builtin::BuiltinFunctionType),
            74 => Some(Builtin::MethodType),
            75 => Some(Builtin::GeneratorType),
            76 => Some(Builtin::ModuleType),
            77 => Some(Builtin::Open),
            78 => Some(Builtin::Dir),
            79 => Some(Builtin::IntIsInteger),
            80 => Some(Builtin::CoroutineType),
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
            Builtin::Property => "property",
            Builtin::Ascii => "ascii",
            Builtin::StrMaketrans => "maketrans",
            Builtin::MatchClassPositional => "__match_class__",
            Builtin::NoneType => "NoneType",
            Builtin::EllipsisType => "ellipsis",
            Builtin::FloatFromhex => "fromhex",
            Builtin::DictKeysType => "dict_keys",
            Builtin::DictValuesType => "dict_values",
            Builtin::DictItemsType => "dict_items",
            Builtin::NotImplementedType => "NotImplementedType",
            Builtin::Vars => "vars",
            Builtin::Object => "object",
            Builtin::Globals => "globals",
            Builtin::Locals => "locals",
            Builtin::FunctionType => "function",
            Builtin::BuiltinFunctionType => "builtin_function_or_method",
            Builtin::MethodType => "method",
            Builtin::GeneratorType => "generator",
            Builtin::CoroutineType => "coroutine",
            Builtin::ModuleType => "module",
            Builtin::Open => "open",
            Builtin::Dir => "dir",
            Builtin::IntIsInteger => "is_integer",
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
                | Builtin::Slice
                | Builtin::Type
                | Builtin::NoneType
                | Builtin::EllipsisType
                | Builtin::DictKeysType
                | Builtin::DictValuesType
                | Builtin::DictItemsType
                | Builtin::NotImplementedType
                | Builtin::Object
        )
    }
}

/// Whether `x` is callable -- the `callable()` builtin's test, shared with the argument checks
/// that require a callable (e.g. `defaultdict`'s factory).
#[must_use]
pub(crate) fn value_is_callable(x: Value, model: &ObjectModel) -> bool {
    x.as_function_index().is_some()
        || x.as_builtin_id().is_some()
        || model.is_class(x)
        || model.is_ntclass(x)
        || model.is_bound_method(x)
        || model.is_py_bound(x)
        || model.is_py_function(x)
}

/// `type(x)`: the type object of `x` -- a built-in value's type is its constructor built-in (so
/// `type(5) is int`), a class instance's type is its class object. `None` for a value whose type
/// has no representation yet (e.g. `None`, a function) -> the caller raises.
pub(crate) fn type_of(value: Value, model: &ObjectModel) -> Option<Value> {
    if value == Value::NONE {
        return Some(Value::builtin_ref(Builtin::NoneType.id()));
    }
    if value.is_ellipsis() {
        return Some(Value::builtin_ref(Builtin::EllipsisType.id()));
    }
    if value.is_not_implemented() {
        return Some(Value::builtin_ref(Builtin::NotImplementedType.id()));
    }
    if model.is_instance(value) {
        return Some(model.instance_class(value));
    }
    if let Some(class) = model.ntinstance_class(value) {
        return Some(class);
    }
    if let Some(stdlib_type) = crate::stdlib::stdlib_type_of(value, model) {
        return Some(stdlib_type);
    }
    #[cfg(feature = "complex")]
    if model.is_complex(value) {
        return Some(Value::builtin_ref(Builtin::Complex.id()));
    }
    if model.is_object_base(value) {
        return Some(Value::builtin_ref(Builtin::Object.id()));
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
    } else if model.is_property(value) {
        Builtin::Property
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
    } else if let Some(kind) = model.dict_view_kind(value) {
        match kind {
            DictViewKind::Keys => Builtin::DictKeysType,
            DictViewKind::Values => Builtin::DictValuesType,
            DictViewKind::Items => Builtin::DictItemsType,
        }
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
    } else if model.is_slice(value) {
        Builtin::Slice
    } else if model.is_class(value) || model.is_ntclass(value) {
        Builtin::Type
    } else if let Some(id) = value.as_builtin_id() {
        let is_type_object = Builtin::from_id(id).is_some_and(Builtin::is_type)
            || crate::stdlib::stdlib_is_type(id);
        if is_type_object {
            Builtin::Type
        } else {
            Builtin::BuiltinFunctionType
        }
    } else if value.as_function_index().is_some() || model.is_py_function(value) {
        Builtin::FunctionType
    } else if model.is_py_bound(value) {
        Builtin::MethodType
    } else if model.is_bound_method(value) || model.is_unbound_method(value) {
        Builtin::BuiltinFunctionType
    } else if model.is_generator(value) {
        Builtin::GeneratorType
    } else if model.is_coroutine(value) {
        Builtin::CoroutineType
    } else if model.is_module_object(value) {
        Builtin::ModuleType
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
        "property" => Builtin::Property,
        "ascii" => Builtin::Ascii,
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
        "vars" => Builtin::Vars,
        "object" => Builtin::Object,
        "open" => Builtin::Open,
        "dir" => Builtin::Dir,
        "globals" => Builtin::Globals,
        "locals" => Builtin::Locals,
        "hash" => Builtin::Hash,
        "__match_class__" => Builtin::MatchClassPositional,
        _ => return None,
    };
    Some(builtin.id())
}

/// Whether `cls` is one of CPython's `Py_TPFLAGS_MATCH_SELF` builtin types -- those for which a single
/// positional class-pattern sub-pattern binds the subject ITSELF (they carry no `__match_args__`):
/// int, float, bool, str, bytes, bytearray, list, tuple, dict, set, frozenset.
fn is_self_match_type(cls: Value) -> bool {
    matches!(
        cls.as_builtin_id().and_then(Builtin::from_id),
        Some(
            Builtin::Int
                | Builtin::Float
                | Builtin::Bool
                | Builtin::Str
                | Builtin::Bytes
                | Builtin::Bytearray
                | Builtin::List
                | Builtin::Tuple
                | Builtin::Dict
                | Builtin::Set
                | Builtin::Frozenset
        )
    )
}

/// Calls built-in `id` with `args` (Python 3.14.6 "Built-in Functions").
pub fn call_builtin(
    id: u32,
    args: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if id >= crate::stdlib::STDLIB_BASE {
        return crate::stdlib::call_stdlib(id, args, functions, model, depth);
    }
    match Builtin::from_id(id).ok_or(Trap::Malformed)? {
        Builtin::Abs => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            #[cfg(feature = "complex")]
            if let Some((re, im)) = model.complex_value(args[0]) {
                return model.new_float(libm::hypot(re, im));
            }
            #[cfg(feature = "float")]
            if let Some(f) = model.float_value(args[0]) {
                return model.new_float(libm::fabs(f));
            }
            if let Some(big) = model.bigint_value(args[0]) {
                return model.new_bigint(big.abs());
            }
            if let Some(method) = model.find_dunder(args[0], "__abs__") {
                return call_value(method, &[], functions, model, depth);
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
            match model.py_len(args[0]) {
                Err(Trap::TypeError) => Err(model.len_type_error(args[0])),
                other => other,
            }
        }
        Builtin::Str => match args {
            [] => model.new_str(""),
            [arg] => {
                let rendered = display_arg(*arg, functions, model, depth)?;
                model.new_str(&rendered)
            }
            _ => Err(Trap::TypeError),
        },
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
            let index = |v: Value, model: &mut ObjectModel| {
                coerce_index(v, functions, model, depth)?.as_int().ok_or(Trap::TypeError)
            };
            let (start, stop, step) = match args {
                [stop] => (0, index(*stop, model)?, 1),
                [start, stop] => (index(*start, model)?, index(*stop, model)?, 1),
                [start, stop, step] => {
                    let (start, stop) = (index(*start, model)?, index(*stop, model)?);
                    let step = index(*step, model)?;
                    if step == 0 {
                        return Err(Trap::ValueError);
                    }
                    (start, stop, step)
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
            if model.is_str(start) {
                return Err(model
                    .with_message(Trap::TypeError, "sum() can't sum strings [use ''.join(seq) instead]"));
            }
            if model.is_bytes(start) {
                return Err(model
                    .with_message(Trap::TypeError, "sum() can't sum bytes [use b''.join(seq) instead]"));
            }
            if model.is_bytearray(start) {
                return Err(model
                    .with_message(Trap::TypeError, "sum() can't sum bytearray [use b''.join(seq) instead]"));
            }
            let elements = collect_iterable(model, &[iterable], functions, depth)?;
            #[cfg(feature = "float")]
            {
                let all_numeric = model.as_f64(start).is_some()
                    && elements.iter().all(|&e| model.as_f64(e).is_some());
                let any_float =
                    model.is_float(start) || elements.iter().any(|&e| model.is_float(e));
                if all_numeric && any_float {
                    let start = model.as_f64(start).unwrap_or(0.0);
                    let total =
                        neumaier_sum(start, elements.iter().map(|&e| model.as_f64(e).unwrap_or(0.0)));
                    return model.new_float(total);
                }
            }
            let mut acc = start;
            for element in elements {
                acc = crate::interp::dispatch_binary(BinOp::Add, acc, element, functions, model, depth)?;
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
        Builtin::Ascii => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let rendered = repr_arg(args[0], functions, model, depth)?;
            model.new_str(&ascii_escape(&rendered))
        }
        Builtin::StrMaketrans => {
            let chars = |model: &ObjectModel, v: Value| {
                model
                    .str_bytes(v)
                    .map(|s| crate::object::code_points(s).collect::<alloc::vec::Vec<u32>>())
            };
            let (from, to, drop) = match args {
                [x, y] => (chars(model, *x), chars(model, *y), None),
                [x, y, z] => (chars(model, *x), chars(model, *y), Some(chars(model, *z))),
                _ => return Err(Trap::TypeError),
            };
            let from = from.ok_or(Trap::TypeError)?;
            let to = to.ok_or(Trap::TypeError)?;
            if from.len() != to.len() {
                return Err(Trap::ValueError);
            }
            let mut entries = alloc::vec::Vec::new();
            for (a, b) in from.iter().zip(to.iter()) {
                let key = Value::fixnum(*a as i32).ok_or(Trap::Overflow)?;
                let val = Value::fixnum(*b as i32).ok_or(Trap::Overflow)?;
                entries.push((key, val));
            }
            if let Some(drop) = drop {
                for c in drop.ok_or(Trap::TypeError)? {
                    let key = Value::fixnum(c as i32).ok_or(Trap::Overflow)?;
                    entries.push((key, Value::NONE));
                }
            }
            model.new_dict(entries)
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
                    let raw = String::from(model.str_text(*x)?);
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
                } else if let Some(method) = model.find_dunder(*x, "__int__") {
                    let result = call_value(method, &[], functions, model, depth)?;
                    if model.is_int(result) {
                        Ok(result)
                    } else {
                        Err(Trap::TypeError)
                    }
                } else if let Some(method) = model.find_dunder(*x, "__index__") {
                    let result = call_value(method, &[], functions, model, depth)?;
                    if model.is_int(result) {
                        Ok(result)
                    } else {
                        Err(Trap::TypeError)
                    }
                } else {
                    Err(Trap::TypeError)
                }
            }
            [x, base] => {
                let base = base.as_int().ok_or(Trap::TypeError)?;
                if base != 0 && !(2..=36).contains(&base) {
                    return Err(model
                        .with_message(Trap::ValueError, "int() base must be >= 2 and <= 36, or 0"));
                }
                if !model.is_str(*x) {
                    return Err(Trap::TypeError);
                }
                let raw = String::from(model.str_text(*x)?);
                parse_int_radix(&raw, base, *x, model)
            }
            _ => Err(Trap::TypeError),
        },
        #[cfg(not(feature = "float"))]
        Builtin::Float => Err(Trap::FloatUnavailable),
        #[cfg(feature = "float")]
        Builtin::Float => match args {
            [] => model.new_float(0.0),
            [x] => {
                if let Some(f) = model.as_f64(*x) {
                    model.new_float(f)
                } else if model.is_str(*x) {
                    let text = String::from(model.str_text(*x)?);
                    #[cfg(not(feature = "float"))]
                    {
                        let _ = text;
                        return Err(Trap::FloatUnavailable);
                    }
                    #[cfg(feature = "float")]
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
                } else if let Some(method) = model.find_dunder(*x, "__float__") {
                    let result = call_value(method, &[], functions, model, depth)?;
                    match model.as_f64(result) {
                        Some(f) => model.new_float(f),
                        None => Err(Trap::TypeError),
                    }
                } else if let Some(method) = model.find_dunder(*x, "__index__") {
                    let result = call_value(method, &[], functions, model, depth)?;
                    match model.as_f64(result) {
                        Some(f) => model.new_float(f),
                        None => Err(Trap::TypeError),
                    }
                } else {
                    Err(Trap::TypeError)
                }
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::FloatFromhex => {
            let [x] = args else {
                return Err(Trap::TypeError);
            };
            let text = model.str_text(*x).map(String::from)?;
            #[cfg(not(feature = "float"))]
            {
                let _ = text;
                return Err(Trap::FloatUnavailable);
            }
            #[cfg(feature = "float")]
            match parse_hex_float(&text) {
                Some(f) => model.new_float(f),
                None => Err(model
                    .with_message(Trap::ValueError, "invalid hexadecimal floating-point string")),
            }
        }
        #[cfg(feature = "complex")]
        Builtin::Complex => match args {
            [] => model.new_complex(0.0, 0.0),
            [x] => {
                if let Some((re, im)) = model.as_complex(*x) {
                    model.new_complex(re, im)
                } else if model.is_str(*x) {
                    let text = String::from(model.str_text(*x)?);
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
        Builtin::Iter => match args {
            [iterable] => iterator_for(*iterable, functions, model, depth),
            [callable, sentinel] => {
                let sources = model.new_tuple(alloc::vec![*sentinel])?;
                model.new_lazy_iter(LAZY_CALLABLE, *callable, sources)
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::Set => {
            let elems = collect_iterable(model, args, functions, depth)?;
            let deduped = crate::interp::dedup_elems(elems, functions, model, depth)?;
            model.new_set(deduped)
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
                    model.new_dict_dyn(kv, functions, depth)
                }
            }
            _ => Err(Trap::TypeError),
        },
        Builtin::Reversed => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let arg = args[0];
            if let Some(method) = model.find_dunder(arg, "__reversed__") {
                return call_value(method, &[], functions, model, depth);
            }
            let reversible = model.str_bytes(arg).is_some()
                || model.is_list(arg)
                || model.is_tuple(arg)
                || model.is_range(arg)
                || model.is_dict(arg)
                || model.is_dict_view(arg)
                || model.is_deque(arg);
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
            let arg = coerce_index(args[0], functions, model, depth)?;
            let code = arg.as_int().ok_or(Trap::TypeError)?;
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
            let s = model.str_bytes(args[0]).ok_or(Trap::TypeError)?;
            let mut points = crate::object::code_points(s);
            match (points.next(), points.next()) {
                (Some(c), None) => Value::fixnum(c as i32).ok_or(Trap::Overflow),
                _ => Err(Trap::TypeError),
            }
        }
        Builtin::Divmod => {
            let [a, b] = args else {
                return Err(Trap::TypeError);
            };
            if let Some(method) = model.find_dunder(*a, "__divmod__") {
                return call_value(method, &[*b], functions, model, depth);
            }
            if let Some(method) = model.find_dunder(*b, "__rdivmod__") {
                return call_value(method, &[*a], functions, model, depth);
            }
            let quotient = binary(BinOp::FloorDiv, *a, *b, model)?;
            let remainder = binary(BinOp::Mod, *a, *b, model)?;
            model.new_tuple(alloc::vec![quotient, remainder])
        }
        Builtin::Pow => match args {
            [base, exp] => {
                #[cfg(feature = "float")]
                if model.is_float(*base) || model.is_float(*exp) {
                    let b = model.as_f64(*base).ok_or(Trap::TypeError)?;
                    let e = model.as_f64(*exp).ok_or(Trap::TypeError)?;
                    return crate::interp::float_pow(b, e, model);
                }
                let e = exp.as_int().ok_or(Trap::TypeError)?;
                if e < 0 {
                    #[cfg(feature = "float")]
                    {
                        let b = model.as_f64(*base).ok_or(Trap::TypeError)?;
                        return crate::interp::float_pow(b, e as f64, model);
                    }
                    #[cfg(not(feature = "float"))]
                    return Err(Trap::FloatUnavailable);
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
                if modulus.is_zero() {
                    return Err(Trap::ValueError);
                }
                let (base, magnitude) = if exponent < 0 {
                    let inverse = base.mod_inverse(&modulus).ok_or_else(|| {
                        model.raise_named_exception(
                            "ValueError",
                            "base is not invertible for the given modulus",
                        )
                    })?;
                    (inverse, exponent.unsigned_abs())
                } else {
                    (base, exponent as u64)
                };
                let reduce = |x: &BigInt| x.divmod(&modulus).map(|(_, r)| r).ok_or(Trap::ValueError);
                let mut acc = reduce(&BigInt::from_i128(1))?;
                let mut base_mod = reduce(&base)?;
                let mut bits = magnitude;
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
        Builtin::Hex => format_radix(model, args, functions, depth, "0x", 16),
        Builtin::Bin => format_radix(model, args, functions, depth, "0b", 2),
        Builtin::Oct => format_radix(model, args, functions, depth, "0o", 8),
        Builtin::Frozenset => {
            let elems = collect_iterable(model, args, functions, depth)?;
            let deduped = crate::interp::dedup_elems(elems, functions, model, depth)?;
            model.new_frozenset(deduped)
        }
        Builtin::Callable => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            Ok(Value::from_bool(value_is_callable(args[0], model)))
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
                        let value = if model.is_generator(iterator) {
                            model.take_generator_return().unwrap_or(Value::NONE)
                        } else {
                            Value::NONE
                        };
                        Err(model.raise_named_exception_with_value("StopIteration", value))
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
                [x, n] => (*x, Some(*n)),
                _ => return Err(Trap::TypeError),
            };
            #[cfg(feature = "float")]
            if let Some(f) = model.float_value(value) {
                let nd = ndigits.map(|n| n.as_int().ok_or(Trap::TypeError)).transpose()?;
                return round_float(f, nd, model);
            }
            if let Some(x) = model.as_i128(value) {
                let nd = ndigits.map_or(Ok(0), |n| n.as_int().ok_or(Trap::TypeError))?;
                model.new_long(round_half_even(x, nd))
            } else if let Some(method) = model.find_dunder(value, "__round__") {
                match ndigits {
                    Some(n) => call_value(method, &[n], functions, model, depth),
                    None => call_value(method, &[], functions, model, depth),
                }
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
        Builtin::MatchClassPositional => {
            let [subject, cls, count] = args else {
                return Err(Trap::TypeError);
            };
            let (subject, cls) = (*subject, *cls);
            let count =
                usize::try_from(count.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::TypeError)?;
            if !isinstance_of(subject, cls, model)? {
                return Ok(Value::NONE);
            }
            let self_match = is_self_match_type(cls);
            let match_args = if self_match {
                None
            } else {
                let mut cache = InlineCache::empty();
                match model.getattr(cls, "__match_args__", &mut cache) {
                    Ok(ma) => model.seq_value(ma).cloned(),
                    Err(Trap::AttributeError) => None,
                    Err(other) => return Err(other),
                }
            };
            let accepted = if self_match { 1 } else { match_args.as_ref().map_or(0, Vec::len) };
            if count > accepted {
                let name = model.class_display_name(cls);
                let noun = if accepted == 1 { "sub-pattern" } else { "sub-patterns" };
                let message =
                    alloc::format!("{name}() accepts {accepted} positional {noun} ({count} given)");
                return Err(model.raise_named_exception("TypeError", &message));
            }
            let mut values = Vec::with_capacity(count);
            if self_match {
                if count == 1 {
                    values.push(subject);
                }
            } else if let Some(names) = match_args {
                let mut cache = InlineCache::empty();
                for name_val in names.iter().take(count) {
                    if model.str_bytes(*name_val).is_none() {
                        return Err(model.raise_named_exception(
                            "TypeError",
                            "__match_args__ elements must be strings",
                        ));
                    }
                    let attr = String::from(model.str_text(*name_val)?);
                    match model.getattr(subject, &attr, &mut cache) {
                        Ok(v) => values.push(v),
                        Err(Trap::AttributeError) => return Ok(Value::NONE),
                        Err(other) => return Err(other),
                    }
                }
            }
            model.new_tuple(values)
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
                [v, s] => (*v, model.str_text(*s).map(String::from)?),
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
                [b] => (
                    model.bytes_value(*b).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?,
                    String::from("big"),
                ),
                [b, order] => (
                    model.bytes_value(*b).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?,
                    model.str_text(*order).map(String::from)?,
                ),
                _ => return Err(Trap::TypeError),
            };
            model.int_from_bytes(&bytes, &byteorder, false)
        }
        Builtin::BytesFromhex => {
            let [s] = args else { return Err(Trap::TypeError); };
            let hex = model.str_text(*s).map(String::from)?;
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
        Builtin::Property => {
            let (fget, fset, fdel) = match args {
                [] => (Value::NONE, Value::NONE, Value::NONE),
                [g] => (*g, Value::NONE, Value::NONE),
                [g, s] => (*g, *s, Value::NONE),
                [g, s, d] => (*g, *s, *d),
                _ => return Err(Trap::TypeError),
            };
            model.new_property(fget, fset, fdel)
        }
        Builtin::Type => {
            let [value] = args else {
                return Err(Trap::TypeError);
            };
            match type_of(*value, model) {
                Some(class) => Ok(class),
                None => {
                    let message = "type() is not supported for this object";
                    Err(model.raise_named_exception("TypeError", message))
                }
            }
        }
        Builtin::NoneType => match args {
            [] => Ok(Value::NONE),
            _ => Err(Trap::TypeError),
        },
        Builtin::EllipsisType => match args {
            [] => Ok(Value::ELLIPSIS),
            _ => Err(Trap::TypeError),
        },
        Builtin::NotImplementedType => match args {
            [] => Ok(Value::NOT_IMPLEMENTED),
            _ => Err(Trap::TypeError),
        },
        view_type @ (Builtin::DictKeysType | Builtin::DictValuesType | Builtin::DictItemsType) => {
            let message = alloc::format!("cannot create '{}' instances", view_type.python_name());
            Err(model.raise_named_exception("TypeError", &message))
        }
        only_a_type @ (Builtin::FunctionType
        | Builtin::BuiltinFunctionType
        | Builtin::MethodType
        | Builtin::GeneratorType
        | Builtin::CoroutineType
        | Builtin::ModuleType) => {
            let message = alloc::format!("cannot create '{}' instances", only_a_type.python_name());
            Err(model.raise_named_exception("TypeError", &message))
        }
        Builtin::Getattr => {
            let (obj, name, default) = match args {
                [obj, name] => (*obj, *name, None),
                [obj, name, default] => (*obj, *name, Some(*default)),
                _ => return Err(Trap::TypeError),
            };
            let attr = String::from(model.str_text(name)?);
            let mut cache = InlineCache::empty();
            match getattr_hooked(obj, &attr, &mut cache, functions, model, depth) {
                Ok(value) => Ok(value),
                Err(Trap::AttributeError) => match default {
                    Some(fallback) => Ok(fallback),
                    None => Err(model.attribute_error(obj, &attr)),
                },
                Err(Trap::Raised) if default.is_some() && model.pending_exception_is("AttributeError") => {
                    model.take_pending_exception();
                    Ok(default.expect("default present"))
                }
                Err(other) => Err(other),
            }
        }
        Builtin::IntIsInteger => {
            if !args.is_empty() {
                return Err(Trap::TypeError);
            }
            Ok(Value::TRUE)
        }
        Builtin::Dir => {
            let [value] = args else {
                let message = "dir() with no arguments is only supported at a call site";
                return Err(model.raise_named_exception("TypeError", message));
            };
            #[cfg(not(feature = "introspection"))]
            {
                let _ = value;
                let message = "dir() is not available in this build (introspection is off)";
                return Err(model.raise_named_exception("NotImplementedError", message));
            }
            #[cfg(feature = "introspection")]
            {
                let names = model.dir_names(*value);
                let mut entries = Vec::with_capacity(names.len());
                for name in names {
                    entries.push(model.new_str(&name)?);
                }
                model.new_list(entries)
            }
        }
        Builtin::Open => {
            let (path, mode) = match args {
                [path] => (*path, "r"),
                [path, mode] => {
                    let text = model.str_text(*mode)?;
                    (*path, text)
                }
                _ => return Err(Trap::TypeError),
            };
            let mode = String::from(mode);
            let parsed = match crate::fileio::FileMode::parse(&mode) {
                Ok(parsed) => parsed,
                Err(message) => return Err(model.raise_named_exception("ValueError", &message)),
            };
            let path = match model.str_bytes(path) {
                Some(_) => String::from(model.str_text(path)?),
                None => {
                    let kind = model.type_name_of(path);
                    let message = alloc::format!(
                        "expected str, bytes or os.PathLike object, not {kind}"
                    );
                    return Err(model.raise_named_exception("TypeError", &message));
                }
            };
            model.open_file(&path, parsed)
        }
        Builtin::Vars => {
            let [obj] = args else {
                let message = "vars() with no arguments is not supported";
                return Err(model.raise_named_exception("TypeError", message));
            };
            let mut cache = InlineCache::empty();
            match getattr_hooked(*obj, "__dict__", &mut cache, functions, model, depth) {
                Ok(dict) => Ok(dict),
                Err(Trap::AttributeError) => {
                    let message = "vars() argument must have __dict__ attribute";
                    Err(model.raise_named_exception("TypeError", message))
                }
                Err(other) => Err(other),
            }
        }
        Builtin::Object => {
            if !args.is_empty() {
                return Err(model.raise_named_exception("TypeError", "object() takes no arguments"));
            }
            model.new_object_base()
        }
        Builtin::Globals => {
            if !args.is_empty() {
                return Err(model.raise_named_exception("TypeError", "globals() takes no arguments"));
            }
            let pairs = model.current_module_globals();
            model.namespace_from_globals(pairs)
        }
        Builtin::Locals => {
            if !args.is_empty() {
                return Err(model.raise_named_exception("TypeError", "locals() takes no arguments"));
            }
            let pairs = model.current_module_globals();
            model.namespace_from_globals(pairs)
        }
        Builtin::Hasattr => {
            let [obj, name] = args else {
                return Err(Trap::TypeError);
            };
            let attr = String::from(model.str_text(*name)?);
            let mut cache = InlineCache::empty();
            match getattr_hooked(*obj, &attr, &mut cache, functions, model, depth) {
                Ok(_) => Ok(Value::TRUE),
                Err(Trap::AttributeError) => Ok(Value::FALSE),
                Err(Trap::Raised) if model.pending_exception_is("AttributeError") => {
                    model.take_pending_exception();
                    Ok(Value::FALSE)
                }
                Err(other) => Err(other),
            }
        }
        Builtin::Setattr => {
            let [obj, name, value] = args else {
                return Err(Trap::TypeError);
            };
            let attr = String::from(model.str_text(*name)?);
            set_attr(*obj, &attr, *value, functions, model, depth)?;
            Ok(Value::NONE)
        }
        Builtin::Delattr => {
            let [obj, name] = args else {
                return Err(Trap::TypeError);
            };
            let attr = String::from(model.str_text(*name)?);
            model.py_delattr(*obj, &attr)?;
            Ok(Value::NONE)
        }
        Builtin::Hash => {
            let [x] = args else {
                return Err(Trap::TypeError);
            };
            if let Some(method) = model.find_dunder(*x, "__hash__") {
                return call_value(method, &[], functions, model, depth + 1);
            }
            match model.py_hash(*x) {
                Err(Trap::TypeError) => {
                    let message = alloc::format!("unhashable type: '{}'", model.tp_name_of(*x));
                    Err(model.raise_named_exception("TypeError", &message))
                }
                other => other,
            }
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
            model.new_dict_fromkeys(iterable, value, functions, depth)
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
        if let Some(matches) = crate::stdlib::stdlib_type_matches(id, value, model) {
            return Ok(matches);
        }
        let matches = match Builtin::from_id(id) {
            Some(Builtin::Object) => true,
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
            Some(Builtin::Range) => model.is_range(value),
            Some(Builtin::Slice) => model.is_slice(value),
            Some(Builtin::Type) => {
                model.is_class(value)
                    || value.as_builtin_id().and_then(Builtin::from_id).is_some_and(Builtin::is_type)
                    || value.as_builtin_id().is_some_and(crate::stdlib::stdlib_is_type)
            }
            Some(Builtin::NoneType) => value == Value::NONE,
            Some(Builtin::EllipsisType) => value.is_ellipsis(),
            Some(Builtin::NotImplementedType) => value.is_not_implemented(),
            Some(Builtin::DictKeysType) => model.dict_view_kind(value) == Some(DictViewKind::Keys),
            Some(Builtin::DictValuesType) => {
                model.dict_view_kind(value) == Some(DictViewKind::Values)
            }
            Some(Builtin::DictItemsType) => model.dict_view_kind(value) == Some(DictViewKind::Items),
            Some(Builtin::FunctionType) => {
                value.as_function_index().is_some() || model.is_py_function(value)
            }
            Some(Builtin::MethodType) => model.is_py_bound(value),
            Some(Builtin::BuiltinFunctionType) => {
                value.as_builtin_id().is_some()
                    || model.is_bound_method(value)
                    || model.is_unbound_method(value)
            }
            Some(Builtin::GeneratorType) => model.is_generator(value),
            Some(Builtin::CoroutineType) => model.is_coroutine(value),
            Some(Builtin::ModuleType) => model.is_module_object(value),
            _ => return Err(Trap::TypeError),
        };
        return Ok(matches);
    }
    if model.is_ntclass(classinfo) {
        return Ok(model.ntinstance_class(value) == Some(classinfo));
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
        model.is_class(v)
            || model.is_ntclass(v)
            || v.as_builtin_id().and_then(Builtin::from_id).is_some_and(Builtin::is_type)
            || v.as_builtin_id().is_some_and(crate::stdlib::stdlib_is_type)
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
    if classinfo.as_builtin_id() == Some(Builtin::Object.id()) {
        return Ok(true);
    }
    if model.is_class(cls) && model.is_class(classinfo) {
        return Ok(model.is_subclass_of(cls, classinfo));
    }
    if model.is_ntclass(cls) {
        return Ok(cls == classinfo || classinfo.as_builtin_id() == Some(Builtin::Tuple.id()));
    }
    if let (Some(a), Some(b)) = (cls.as_builtin_id(), classinfo.as_builtin_id()) {
        if let Some(result) = crate::stdlib::stdlib_issubclass(a, b) {
            return Ok(result);
        }
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
    if id >= crate::stdlib::STDLIB_BASE {
        return crate::stdlib::call_stdlib_kw(id, posargs, kwargs, functions, model, depth);
    }
    match Builtin::from_id(id).ok_or(Trap::Malformed)? {
        Builtin::Sorted => sorted_kw(posargs, kwargs, functions, model, depth),
        Builtin::Dict => dict_kw(posargs, kwargs, functions, model, depth),
        Builtin::Min => min_max_kw(posargs, kwargs, Ordering::Less, functions, model, depth),
        Builtin::Max => min_max_kw(posargs, kwargs, Ordering::Greater, functions, model, depth),
        Builtin::Print => print_kw(posargs, kwargs, functions, model, depth),
        Builtin::Enumerate => enumerate_kw(posargs, kwargs, functions, model, depth),
        Builtin::Zip => zip_kw(posargs, kwargs, functions, model, depth),
        Builtin::IntFromBytes => {
            let mut byteorder = String::from("big");
            let bytes = match posargs {
                [b] => model.bytes_value(*b).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?,
                [b, order] => {
                    byteorder = model.str_text(*order).map(String::from)?;
                    model.bytes_value(*b).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?
                }
                _ => return Err(Trap::TypeError),
            };
            let mut signed = false;
            for &(name, value) in kwargs {
                match name {
                    "signed" => signed = model.py_truthy(value)?.unwrap_or(false),
                    "byteorder" if posargs.len() == 1 => {
                        byteorder =
                            model.str_text(value).map(String::from)?;
                    }
                    other => {
                        let message = alloc::format!(
                            "from_bytes() got an unexpected keyword argument '{other}'"
                        );
                        return Err(model.raise_named_exception("TypeError", &message));
                    }
                }
            }
            model.int_from_bytes(&bytes, &byteorder, signed)
        }
        _ if kwargs.is_empty() => call_builtin(id, posargs, functions, model, depth),
        _ => Err(Trap::TypeError),
    }
}

/// `enumerate(iterable, start=0)`: accepts `start` as a keyword. A `start` given both positionally
/// and by keyword, or any other keyword, is a `TypeError`.
fn enumerate_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let iterable = *posargs.first().ok_or(Trap::TypeError)?;
    if posargs.len() > 2 {
        return Err(Trap::TypeError);
    }
    let mut start = posargs.get(1).copied();
    for &(name, value) in kwargs {
        if name == "start" && start.is_none() {
            start = Some(value);
        } else {
            return Err(Trap::TypeError);
        }
    }
    let mut call_args = alloc::vec![iterable];
    if let Some(start) = start {
        call_args.push(start);
    }
    call_builtin(Builtin::Enumerate.id(), &call_args, functions, model, depth)
}

/// `zip(*iterables, strict=False)`: accepts the keyword-only `strict`. When truthy, the lazy iterator
/// raises `ValueError` if the sources differ in length (enforced during iteration); the flag rides the
/// iterator's state slot. Any other keyword is a `TypeError`.
fn zip_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let mut strict = false;
    for &(name, value) in kwargs {
        if name == "strict" {
            strict = model.py_truthy(value)?.unwrap_or_else(|| value.is_truthy());
        } else {
            return Err(Trap::TypeError);
        }
    }
    let sources = source_iters(model, posargs, functions, depth)?;
    let flag = if strict { Value::TRUE } else { Value::NONE };
    model.new_lazy_iter(LAZY_ZIP, flag, sources)
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
            model.str_text(value).map(String::from)
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
        match entries.iter().position(|(k, _)| model.str_bytes(*k) == Some(name.as_bytes())) {
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
    functions: &[CodeObject],
    depth: usize,
    prefix: &str,
    radix: u8,
) -> Result<Value, Trap> {
    let [arg] = args else {
        return Err(Trap::TypeError);
    };
    let arg = coerce_index(*arg, functions, model, depth)?;
    let value = model.as_bigint(arg).ok_or(Trap::TypeError)?;
    let bits_per_digit = match radix {
        16 => 4,
        8 => 3,
        _ => 1,
    };
    let body = value.to_power_of_two_radix_string(bits_per_digit);
    let rendered = match body.strip_prefix('-') {
        Some(magnitude) => alloc::format!("-{prefix}{magnitude}"),
        None => alloc::format!("{prefix}{body}"),
    };
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
    match model.str_bytes(result) {
        Some(_) => Ok(String::from(model.str_text(result)?)),
        None => model.display(result),
    }
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
        || model.is_dict_view(value)
    {
        return repr_arg(value, functions, model, depth);
    }
    model.display(value)
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
    if let Some(class) = model.ntinstance_class(value) {
        let fields = model.ntclass_fields(class);
        let elems = model.seq_value(value).cloned().unwrap_or_default();
        let mut parts = Vec::with_capacity(fields.len());
        for (field, element) in fields.iter().zip(&elems) {
            let rendered = repr_arg(*element, functions, model, depth + 1)?;
            parts.push(alloc::format!("{field}={rendered}"));
        }
        return Ok(alloc::format!("{}({})", model.ntclass_name(class), parts.join(", ")));
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
        if model.is_counter(value) {
            if entries.is_empty() {
                return Ok(String::from("Counter()"));
            }
            let entries = model.counter_display_entries(entries);
            let mut parts = Vec::with_capacity(entries.len());
            for (key, val) in &entries {
                let key = repr_arg(*key, functions, model, depth + 1)?;
                let val = repr_arg(*val, functions, model, depth + 1)?;
                parts.push(alloc::format!("{key}: {val}"));
            }
            return Ok(alloc::format!("Counter({{{}}})", parts.join(", ")));
        }
        let mut parts = Vec::with_capacity(entries.len());
        for (key, val) in &entries {
            let key = repr_arg(*key, functions, model, depth + 1)?;
            let val = repr_arg(*val, functions, model, depth + 1)?;
            parts.push(alloc::format!("{key}: {val}"));
        }
        if let Some(factory) = model.defaultdict_factory(value) {
            let factory = repr_arg(factory, functions, model, depth + 1)?;
            return Ok(alloc::format!("defaultdict({factory}, {{{}}})", parts.join(", ")));
        }
        if model.is_ordereddict(value) {
            return Ok(if parts.is_empty() {
                String::from("OrderedDict()")
            } else {
                alloc::format!("OrderedDict({{{}}})", parts.join(", "))
            });
        }
        return Ok(alloc::format!("{{{}}}", parts.join(", ")));
    }
    if let Some(elems) = model.deque_elems(value).cloned() {
        let mut parts = Vec::with_capacity(elems.len());
        for element in &elems {
            parts.push(repr_arg(*element, functions, model, depth + 1)?);
        }
        let inner = parts.join(", ");
        return Ok(match model.deque_maxlen(value).unwrap_or(None) {
            Some(m) => alloc::format!("deque([{inner}], maxlen={m})"),
            None => alloc::format!("deque([{inner}])"),
        });
    }
    if let Some(kind) = model.dict_view_kind(value) {
        let entries = model.dict_view_dict(value);
        let entries = model.dict_value(entries).cloned().unwrap_or_default();
        let mut parts = Vec::with_capacity(entries.len());
        for (key, val) in &entries {
            parts.push(match kind {
                DictViewKind::Keys => repr_arg(*key, functions, model, depth + 1)?,
                DictViewKind::Values => repr_arg(*val, functions, model, depth + 1)?,
                DictViewKind::Items => alloc::format!(
                    "({}, {})",
                    repr_arg(*key, functions, model, depth + 1)?,
                    repr_arg(*val, functions, model, depth + 1)?
                ),
            });
        }
        let name = match kind {
            DictViewKind::Keys => "dict_keys",
            DictViewKind::Values => "dict_values",
            DictViewKind::Items => "dict_items",
        };
        return Ok(alloc::format!("{name}([{}])", parts.join(", ")));
    }
    Ok(model.repr(value))
}

/// Escapes every non-ASCII code point of `s` as `\xNN` / `\uNNNN` / `\UNNNNNNNN` (Python's `ascii()`).
fn ascii_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let cp = c as u32;
        if cp < 0x80 {
            out.push(c);
        } else if cp <= 0xff {
            out.push_str(&alloc::format!("\\x{cp:02x}"));
        } else if cp <= 0xffff {
            out.push_str(&alloc::format!("\\u{cp:04x}"));
        } else {
            out.push_str(&alloc::format!("\\U{cp:08x}"));
        }
    }
    out
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
            let text = model.str_text(*source).map(String::from)?;
            let name = model.str_text(*encoding).map(String::from)?;
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
#[cfg(feature = "float")]
fn parse_python_float(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    strip_underscores(trimmed)?.parse::<f64>().ok()
}

/// Parses CPython's `float.fromhex` grammar: an optional sign, then `inf`/`infinity`/`nan`
/// (case-insensitive) or a hexadecimal float `['0x'] H ['.' H] ['p' ['+'|'-'] D]` -- `H` hex digits,
/// the `p` exponent a power of TWO in decimal. `None` for anything malformed (the caller raises the
/// ValueError). The significand accumulates in `f64`, exact for a normalized value (<= 13 hex
/// fraction digits = 53 bits), so every representable double round-trips.
#[cfg(feature = "float")]
fn parse_hex_float(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    let (negative, rest) = match trimmed.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if rest.is_empty() {
        return None;
    }
    let lower = rest.to_ascii_lowercase();
    match lower.as_str() {
        "inf" | "infinity" => return Some(if negative { f64::NEG_INFINITY } else { f64::INFINITY }),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    let body = lower.strip_prefix("0x").unwrap_or(&lower);
    let (mantissa, exp) = match body.split_once('p') {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (body, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let mut significand = 0.0f64;
    for ch in int_part.chars() {
        significand = significand * 16.0 + f64::from(ch.to_digit(16)?);
    }
    let mut scale = 1.0f64;
    for ch in frac_part.chars() {
        scale /= 16.0;
        significand += f64::from(ch.to_digit(16)?) * scale;
    }
    let value = significand * libm::exp2(f64::from(exp));
    Some(if negative { -value } else { value })
}

/// Removes Python's numeric `_` separators, which must sit BETWEEN two digits (`1_000` is valid,
/// `_1`/`1_`/`1__0`/`1_.0` are not). `None` for a misplaced underscore; the original string when
/// there is none.
/// Neumaier compensated summation (an improved Kahan): tracks the lost low-order bits in `c` so a
/// large-then-small run (`[1e20, 1, -1e20]`) keeps the small term. Backs `sum()` over floats.
#[cfg(feature = "float")]
fn neumaier_sum(start: f64, values: impl Iterator<Item = f64>) -> f64 {
    let mut sum = start;
    let mut compensation = 0.0;
    for x in values {
        let t = sum + x;
        if sum.abs() >= x.abs() {
            compensation += (sum - t) + x;
        } else {
            compensation += (x - t) + sum;
        }
        sum = t;
    }
    sum + compensation
}

/// `int(str, base)`: parse `raw` in `base` (2..=36, or 0 to auto-detect from a `0x`/`0o`/`0b`
/// prefix), allowing a leading sign, the matching prefix, and `_` digit separators. `original` is
/// the source string (for the error message).
fn parse_int_radix(
    raw: &str,
    base: i64,
    original: Value,
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    let trimmed = raw.trim();
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let lowered = body.to_ascii_lowercase();
    let (base, body) = if base == 0 {
        if lowered.starts_with("0x") {
            (16, &body[2..])
        } else if lowered.starts_with("0o") {
            (8, &body[2..])
        } else if lowered.starts_with("0b") {
            (2, &body[2..])
        } else {
            (10, body)
        }
    } else {
        let prefix = match base {
            16 => "0x",
            8 => "0o",
            2 => "0b",
            _ => "",
        };
        if !prefix.is_empty() && lowered.starts_with(prefix) {
            (base, &body[2..])
        } else {
            (base, body)
        }
    };
    let parsed = strip_underscores(body)
        .and_then(|cleaned| BigInt::from_str_radix(&cleaned, base as u32, negative));
    match parsed {
        Some(big) => model.new_bigint(big),
        None => {
            let message = alloc::format!(
                "invalid literal for int() with base {base}: {}",
                model.repr(original)
            );
            Err(model.with_message(Trap::ValueError, &message))
        }
    }
}

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
#[cfg(feature = "float")]
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
#[cfg(feature = "float")]
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
#[cfg(feature = "float")]
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
        None => {
            return match default {
                Some(value) => Ok(value),
                None => {
                    let name = if keep == Ordering::Less { "min" } else { "max" };
                    let message = alloc::format!("{name}() iterable argument is empty");
                    Err(model.raise_named_exception("ValueError", &message))
                }
            };
        }
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
    fn match_class_positional_self_match_arity_and_nonmatch() {
        let mut model = ObjectModel::new(Vec::new(), 8192);
        let mc = Builtin::MatchClassPositional.id();
        let int_ty = Value::builtin_ref(Builtin::Int.id());
        let five = Value::fixnum(5).unwrap();
        let text = model.new_str("hi").unwrap();
        let (zero, one, two) =
            (Value::fixnum(0).unwrap(), Value::fixnum(1).unwrap(), Value::fixnum(2).unwrap());
        let bound = call_builtin(mc, &[five, int_ty, one], &[], &mut model, 0).unwrap();
        assert_eq!(model.seq_value(bound).cloned(), Some(alloc::vec![five]));
        let none_bound = call_builtin(mc, &[five, int_ty, zero], &[], &mut model, 0).unwrap();
        assert_eq!(model.seq_value(none_bound).map(alloc::vec::Vec::len), Some(0));
        assert_eq!(call_builtin(mc, &[five, int_ty, two], &[], &mut model, 0), Err(Trap::Raised));
        let exc = model.take_pending_exception().unwrap();
        assert!(
            model.repr(exc).contains("int() accepts 1 positional sub-pattern (2 given)"),
            "got: {}",
            model.repr(exc)
        );
        assert_eq!(call_builtin(mc, &[text, int_ty, one], &[], &mut model, 0), Ok(Value::NONE));
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
        assert_eq!(type_of(Value::NONE, &model), Some(Value::builtin_ref(Builtin::NoneType.id())));
        assert_eq!(type_of(Value::ELLIPSIS, &model), Some(Value::builtin_ref(Builtin::EllipsisType.id())));
        assert_eq!(Builtin::Int.python_name(), "int");
        assert_eq!(Builtin::Abs.python_name(), "abs");
        assert_eq!(Builtin::NoneType.python_name(), "NoneType");
        assert_eq!(Builtin::EllipsisType.python_name(), "ellipsis");
        assert!(Builtin::Int.is_type() && !Builtin::Abs.is_type());
        assert!(Builtin::NoneType.is_type() && Builtin::EllipsisType.is_type());
    }
}
