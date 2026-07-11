//! The standard-library modules the interpreter provides natively.

use alloc::string::String;
use alloc::vec::Vec;

use lamella_py_bytecode::CodeObject;

use crate::bigint::BigInt;
use crate::object::ObjectModel;
use crate::trap::Trap;
use crate::value::Value;

/// The first built-in-reference id reserved for a native stdlib function. The core built-ins
/// ([`crate::builtins::Builtin`]) occupy the id space below it; a `builtin_ref` id at or above
/// this belongs to a stdlib module and is dispatched by [`call_stdlib`]. These ids are purely
/// runtime-internal -- a module's namespace is rebuilt at import time, so they are never
/// serialized and may be reordered freely (unlike the wire-stable [`crate::builtins::Builtin`]).
pub const STDLIB_BASE: u32 = 0x1000;

/// A native stdlib function, identified by its offset above [`STDLIB_BASE`]. The `math` block
/// comes first; the set widens as further modules are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum StdlibFn {
    MathSqrt = 0,
    MathFloor,
    MathCeil,
    MathTrunc,
    MathFabs,
    MathFactorial,
    MathGcd,
    MathLcm,
    MathIsqrt,
    MathPow,
    MathExp,
    MathLog,
    MathLog2,
    MathLog10,
    MathSin,
    MathCos,
    MathTan,
    MathAsin,
    MathAcos,
    MathAtan,
    MathAtan2,
    MathHypot,
    MathDegrees,
    MathRadians,
    MathCopysign,
    MathFmod,
    MathIsnan,
    MathIsinf,
    MathIsfinite,
    /// `collections.defaultdict` -- BOTH the constructor and the type object (`type(dd)` returns
    /// this id, so `type(dd) is defaultdict` holds and `isinstance` accepts it).
    CollectionsDefaultdict,
    /// `collections.Counter` -- constructor + type object, like defaultdict.
    CollectionsCounter,
    /// `collections.OrderedDict` -- constructor + type object, like defaultdict.
    CollectionsOrderedDict,
    /// `collections.deque` -- constructor + type object, like defaultdict.
    CollectionsDeque,
    /// `collections.namedtuple` -- the class FACTORY (a plain function, not a type object).
    CollectionsNamedtuple,
}

impl StdlibFn {
    /// This function's built-in-reference id (its offset above [`STDLIB_BASE`]).
    fn id(self) -> u32 {
        STDLIB_BASE + self as u32
    }

    /// The function for a built-in-reference `id`, or `None` if `id` names no stdlib function.
    fn from_id(id: u32) -> Option<StdlibFn> {
        use StdlibFn::*;
        let offset = id.checked_sub(STDLIB_BASE)?;
        Some(match offset {
            0 => MathSqrt,
            1 => MathFloor,
            2 => MathCeil,
            3 => MathTrunc,
            4 => MathFabs,
            5 => MathFactorial,
            6 => MathGcd,
            7 => MathLcm,
            8 => MathIsqrt,
            9 => MathPow,
            10 => MathExp,
            11 => MathLog,
            12 => MathLog2,
            13 => MathLog10,
            14 => MathSin,
            15 => MathCos,
            16 => MathTan,
            17 => MathAsin,
            18 => MathAcos,
            19 => MathAtan,
            20 => MathAtan2,
            21 => MathHypot,
            22 => MathDegrees,
            23 => MathRadians,
            24 => MathCopysign,
            25 => MathFmod,
            26 => MathIsnan,
            27 => MathIsinf,
            28 => MathIsfinite,
            29 => CollectionsDefaultdict,
            30 => CollectionsCounter,
            31 => CollectionsOrderedDict,
            32 => CollectionsDeque,
            33 => CollectionsNamedtuple,
            _ => return None,
        })
    }

    /// The function's Python name (its key in the module namespace and its `__name__`).
    fn python_name(self) -> &'static str {
        use StdlibFn::*;
        match self {
            MathSqrt => "sqrt",
            MathFloor => "floor",
            MathCeil => "ceil",
            MathTrunc => "trunc",
            MathFabs => "fabs",
            MathFactorial => "factorial",
            MathGcd => "gcd",
            MathLcm => "lcm",
            MathIsqrt => "isqrt",
            MathPow => "pow",
            MathExp => "exp",
            MathLog => "log",
            MathLog2 => "log2",
            MathLog10 => "log10",
            MathSin => "sin",
            MathCos => "cos",
            MathTan => "tan",
            MathAsin => "asin",
            MathAcos => "acos",
            MathAtan => "atan",
            MathAtan2 => "atan2",
            MathHypot => "hypot",
            MathDegrees => "degrees",
            MathRadians => "radians",
            MathCopysign => "copysign",
            MathFmod => "fmod",
            MathIsnan => "isnan",
            MathIsinf => "isinf",
            MathIsfinite => "isfinite",
            CollectionsDefaultdict => "defaultdict",
            CollectionsCounter => "Counter",
            CollectionsOrderedDict => "OrderedDict",
            CollectionsDeque => "deque",
            CollectionsNamedtuple => "namedtuple",
        }
    }
}

/// Dispatches a KEYWORD call of stdlib function `id`. The only stdlib keyword surface is
/// `deque(iterable, maxlen=N)`; any other stdlib function with no keywords falls back to the
/// positional dispatch (a `*`-unpacking call), and a genuine keyword elsewhere is a TypeError.
pub fn call_stdlib_kw(
    id: u32,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if StdlibFn::from_id(id) == Some(StdlibFn::CollectionsDeque) && !kwargs.is_empty() {
        let mut maxlen = Value::NONE;
        for &(name, value) in kwargs {
            if name == "maxlen" {
                maxlen = value;
            } else {
                return Err(Trap::TypeError);
            }
        }
        let mut args: Vec<Value> = posargs.to_vec();
        match args.len() {
            0 => {
                let empty = model.new_list(Vec::new())?;
                args.push(empty);
            }
            1 => {}
            _ => return Err(Trap::TypeError),
        }
        args.push(maxlen);
        return call_stdlib(id, &args, functions, model, depth);
    }
    if kwargs.is_empty() {
        return call_stdlib(id, posargs, functions, model, depth);
    }
    Err(Trap::TypeError)
}

/// The Python name of the stdlib function `id` names (for `repr` / `__name__`), or `None` if
/// `id` is not a stdlib function id. Consumed by the built-in `repr`/`__name__` paths, which
/// fall here when [`crate::builtins::Builtin::from_id`] does not recognize the id.
#[must_use]
pub fn stdlib_name(id: u32) -> Option<&'static str> {
    StdlibFn::from_id(id).map(StdlibFn::python_name)
}

/// Whether stdlib id `id` is a TYPE object (usable as `isinstance`'s second argument; repr'd as
/// `<class 'module.Name'>`), versus a plain module function like `math.sqrt`.
#[must_use]
pub fn stdlib_is_type(id: u32) -> bool {
    matches!(
        StdlibFn::from_id(id),
        Some(
            StdlibFn::CollectionsDefaultdict
                | StdlibFn::CollectionsCounter
                | StdlibFn::CollectionsOrderedDict
                | StdlibFn::CollectionsDeque
        )
    )
}

/// The defining module of stdlib id `id` (for the `<class 'collections.defaultdict'>` repr).
#[must_use]
pub fn stdlib_module_of(id: u32) -> Option<&'static str> {
    StdlibFn::from_id(id).map(|f| match f {
        StdlibFn::CollectionsDefaultdict
        | StdlibFn::CollectionsCounter
        | StdlibFn::CollectionsOrderedDict
        | StdlibFn::CollectionsDeque => "collections",
        _ => "math",
    })
}

/// Whether stdlib type `id` spells its `tp_name` DOTTED in CPython's hash/attribute error
/// messages: the C-implemented collections types do (`collections.deque`); the pure-Python
/// `Counter` does not. Probed CPython 3.14 behavior, consumed by the error-message sites only.
#[must_use]
pub fn stdlib_tp_name_dotted(id: u32) -> bool {
    matches!(
        StdlibFn::from_id(id),
        Some(
            StdlibFn::CollectionsDefaultdict
                | StdlibFn::CollectionsOrderedDict
                | StdlibFn::CollectionsDeque
        )
    )
}

/// The `isinstance(value, <stdlib type>)` test for a stdlib TYPE id, or `None` when `id` is not a
/// stdlib type (the caller then rejects it as a non-type).
#[must_use]
pub fn stdlib_type_matches(id: u32, value: Value, model: &ObjectModel) -> Option<bool> {
    match StdlibFn::from_id(id)? {
        StdlibFn::CollectionsDefaultdict => Some(model.is_defaultdict(value)),
        StdlibFn::CollectionsCounter => Some(model.is_counter(value)),
        StdlibFn::CollectionsOrderedDict => Some(model.is_ordereddict(value)),
        StdlibFn::CollectionsDeque => Some(model.is_deque(value)),
        _ => None,
    }
}

/// `type(value)` for a value whose type is a STDLIB type object (`type(dd) is defaultdict`), or
/// `None` when the value is not a stdlib-typed one (the core `type_of` chain then applies).
#[must_use]
pub fn stdlib_type_of(value: Value, model: &ObjectModel) -> Option<Value> {
    if model.is_defaultdict(value) {
        return Some(Value::builtin_ref(StdlibFn::CollectionsDefaultdict.id()));
    }
    if model.is_counter(value) {
        return Some(Value::builtin_ref(StdlibFn::CollectionsCounter.id()));
    }
    if model.is_ordereddict(value) {
        return Some(Value::builtin_ref(StdlibFn::CollectionsOrderedDict.id()));
    }
    if model.is_deque(value) {
        return Some(Value::builtin_ref(StdlibFn::CollectionsDeque.id()));
    }
    None
}

/// `issubclass` between two stdlib/builtin ids where at least one is a stdlib type: a stdlib type
/// is a subclass of itself, and the dict subtypes (defaultdict) are subclasses of `dict`. `None`
/// when neither id is a stdlib type (the core builtin-vs-builtin rule then applies).
#[must_use]
pub fn stdlib_issubclass(cls_id: u32, base_id: u32) -> Option<bool> {
    let cls = StdlibFn::from_id(cls_id);
    let base = StdlibFn::from_id(base_id);
    if cls.is_none() && base.is_none() {
        return None;
    }
    if cls == base {
        return Some(true);
    }
    if matches!(
        cls,
        Some(
            StdlibFn::CollectionsDefaultdict
                | StdlibFn::CollectionsCounter
                | StdlibFn::CollectionsOrderedDict
        )
    ) {
        return Some(base_id == crate::builtins::Builtin::Dict.id());
    }
    Some(false)
}

/// Builds a native stdlib module `name`, or `None` if there is no native module by that name.
/// The import machinery calls this on a `sys.modules` miss; a `None` result is a
/// `ModuleNotFoundError` at the import site.
pub fn build_module(name: &str, model: &mut ObjectModel) -> Option<Result<Value, Trap>> {
    match name {
        "math" => Some(build_math_module(model)),
        "collections" => Some(build_collections_module(model)),
        _ => None,
    }
}

/// Builds the `collections` module: the container types, each member both the constructor and the
/// type object. Semantics follow Python 3.14.6 "collections -- Container datatypes".
fn build_collections_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for f in [
        StdlibFn::CollectionsDefaultdict,
        StdlibFn::CollectionsCounter,
        StdlibFn::CollectionsOrderedDict,
        StdlibFn::CollectionsDeque,
        StdlibFn::CollectionsNamedtuple,
    ] {
        let key = model.new_str(f.python_name())?;
        entries.push((key, Value::builtin_ref(f.id())));
    }
    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// Builds the `math` module: a namespace dict of its constants + functions, wrapped in a module
/// object. Constants follow Python 3.14.6 "math".
fn build_math_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    use StdlibFn::*;
    let mut entries: Vec<(Value, Value)> = Vec::new();

    for (name, value) in [
        ("pi", core::f64::consts::PI),
        ("e", core::f64::consts::E),
        ("tau", core::f64::consts::TAU),
        ("inf", f64::INFINITY),
        ("nan", f64::NAN),
    ] {
        let key = model.new_str(name)?;
        let val = model.new_float(value)?;
        entries.push((key, val));
    }

    const FUNCTIONS: &[StdlibFn] = &[
        MathSqrt, MathFloor, MathCeil, MathTrunc, MathFabs, MathFactorial, MathGcd, MathLcm,
        MathIsqrt, MathPow, MathExp, MathLog, MathLog2, MathLog10, MathSin, MathCos, MathTan,
        MathAsin, MathAcos, MathAtan, MathAtan2, MathHypot, MathDegrees, MathRadians, MathCopysign,
        MathFmod, MathIsnan, MathIsinf, MathIsfinite,
    ];
    for &f in FUNCTIONS {
        let key = model.new_str(f.python_name())?;
        entries.push((key, Value::builtin_ref(f.id())));
    }

    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// Dispatches a call of the stdlib function `id` with `args`. Precondition: `id >= STDLIB_BASE`
/// (a core built-in never reaches here). An unknown id is `Malformed`.
pub fn call_stdlib(
    id: u32,
    args: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    use StdlibFn::*;
    match StdlibFn::from_id(id).ok_or(Trap::Malformed)? {
        CollectionsDefaultdict => {
            let (factory, init) = match args {
                [] => (Value::NONE, None),
                [f] => (*f, None),
                [f, init] => (*f, Some(*init)),
                _ => return Err(Trap::TypeError),
            };
            if factory != Value::NONE && !crate::builtins::value_is_callable(factory, model) {
                let message = "first argument must be callable or None";
                return Err(model.raise_named_exception("TypeError", message));
            }
            let entries = match init {
                None => Vec::new(),
                Some(mapping) => model.dict_entries(mapping).ok_or(Trap::TypeError)?,
            };
            model.new_defaultdict(factory, entries)
        }
        CollectionsCounter => {
            let entries = match args {
                [] => Vec::new(),
                [mapping] if model.dict_entries(*mapping).is_some() => {
                    model.dict_entries(*mapping).unwrap_or_default()
                }
                [iterable] => {
                    let items =
                        crate::builtins::collect_iterable(model, &[*iterable], functions, depth)?;
                    let mut counts: Vec<(Value, i128)> = Vec::new();
                    for item in items {
                        let mut found = false;
                        for entry in &mut counts {
                            if crate::interp::elem_eq(item, entry.0, functions, model, depth)? {
                                entry.1 += 1;
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            counts.push((item, 1));
                        }
                    }
                    let mut entries = Vec::with_capacity(counts.len());
                    for (key, n) in counts {
                        let count = model.int_from_i128(n)?;
                        entries.push((key, count));
                    }
                    entries
                }
                _ => return Err(Trap::TypeError),
            };
            model.new_counter(entries)
        }
        CollectionsOrderedDict => {
            let pairs = match args {
                [] => Vec::new(),
                [mapping] if model.dict_entries(*mapping).is_some() => {
                    model.dict_entries(*mapping).unwrap_or_default()
                }
                [iterable] => {
                    let items =
                        crate::builtins::collect_iterable(model, &[*iterable], functions, depth)?;
                    let mut kv = Vec::with_capacity(items.len());
                    for item in items {
                        let parts = model.unpack_sequence(item, 2)?;
                        kv.push((parts[0], parts[1]));
                    }
                    kv
                }
                _ => return Err(Trap::TypeError),
            };
            model.new_ordereddict(pairs)
        }
        CollectionsDeque => {
            let (iterable, maxlen_arg) = match args {
                [] => (None, Value::NONE),
                [iterable] => (Some(*iterable), Value::NONE),
                [iterable, maxlen] => (Some(*iterable), *maxlen),
                _ => return Err(Trap::TypeError),
            };
            let maxlen = if maxlen_arg == Value::NONE {
                None
            } else {
                let m = maxlen_arg.as_int().ok_or(Trap::TypeError)?;
                if m < 0 {
                    let message = "maxlen must be non-negative";
                    return Err(model.with_message(Trap::ValueError, message));
                }
                Some(m as usize)
            };
            let elements = match iterable {
                None => Vec::new(),
                Some(iterable) => {
                    crate::builtins::collect_iterable(model, &[iterable], functions, depth)?
                }
            };
            model.new_deque(elements, maxlen)
        }
        CollectionsNamedtuple => {
            let [name_arg, fields_arg] = args else {
                return Err(Trap::TypeError);
            };
            let name = model.str_value(*name_arg).map(String::from).ok_or(Trap::TypeError)?;
            let fields: Vec<String> = if let Some(spec) = model.str_value(*fields_arg) {
                spec.replace(',', " ").split_whitespace().map(String::from).collect()
            } else {
                let items = model.seq_value(*fields_arg).cloned().ok_or(Trap::TypeError)?;
                let mut fields = Vec::with_capacity(items.len());
                for item in items {
                    fields.push(model.str_value(item).map(String::from).ok_or(Trap::TypeError)?);
                }
                fields
            };
            model.new_ntclass(&name, &fields)
        }
        MathSqrt => {
            let x = one_real(args, model)?;
            if x < 0.0 {
                return Err(nonnegative_error(model, "expected a nonnegative input", x));
            }
            model.new_float(libm::sqrt(x))
        }
        MathFloor => floor_ceil_trunc(args, model, Rounding::Floor),
        MathCeil => floor_ceil_trunc(args, model, Rounding::Ceil),
        MathTrunc => floor_ceil_trunc(args, model, Rounding::Trunc),
        MathFabs => {
            let x = one_real(args, model)?;
            model.new_float(libm::fabs(x))
        }
        MathFactorial => factorial(args, model),
        MathGcd => gcd(args, model),
        MathLcm => lcm(args, model),
        MathIsqrt => isqrt(args, model),
        MathPow => {
            let (x, y) = two_reals(args, model)?;
            let r = libm::pow(x, y);
            if r.is_nan() && x.is_finite() && y.is_finite() {
                return Err(model.with_message(Trap::ValueError, "math domain error"));
            }
            if r.is_infinite() && x.is_finite() && y.is_finite() {
                return Err(model.with_message(Trap::Overflow, "math range error"));
            }
            model.new_float(r)
        }
        MathExp => {
            let x = one_real(args, model)?;
            let r = libm::exp(x);
            if r.is_infinite() && x.is_finite() {
                return Err(model.with_message(Trap::Overflow, "math range error"));
            }
            model.new_float(r)
        }
        MathLog => match args {
            [x] => {
                let x = real(*x, model)?;
                positive(x, model)?;
                model.new_float(libm::log(x))
            }
            [x, base] => {
                let x = real(*x, model)?;
                let base = real(*base, model)?;
                positive(x, model)?;
                positive(base, model)?;
                model.new_float(libm::log(x) / libm::log(base))
            }
            _ => Err(Trap::TypeError),
        },
        MathLog2 => {
            let x = one_real(args, model)?;
            positive(x, model)?;
            model.new_float(libm::log2(x))
        }
        MathLog10 => {
            let x = one_real(args, model)?;
            positive(x, model)?;
            model.new_float(libm::log10(x))
        }
        MathSin => {
            let x = one_real(args, model)?;
            model.new_float(libm::sin(x))
        }
        MathCos => {
            let x = one_real(args, model)?;
            model.new_float(libm::cos(x))
        }
        MathTan => {
            let x = one_real(args, model)?;
            model.new_float(libm::tan(x))
        }
        MathAsin => {
            let x = one_real(args, model)?;
            unit_range(x, model)?;
            model.new_float(libm::asin(x))
        }
        MathAcos => {
            let x = one_real(args, model)?;
            unit_range(x, model)?;
            model.new_float(libm::acos(x))
        }
        MathAtan => {
            let x = one_real(args, model)?;
            model.new_float(libm::atan(x))
        }
        MathAtan2 => {
            let (y, x) = two_reals(args, model)?;
            model.new_float(libm::atan2(y, x))
        }
        MathHypot => {
            let coords: Vec<f64> = args.iter().map(|&a| real(a, model)).collect::<Result<_, _>>()?;
            let r = match coords.as_slice() {
                [] => 0.0,
                [a, b] => libm::hypot(*a, *b),
                many => libm::sqrt(many.iter().map(|v| v * v).sum()),
            };
            model.new_float(r)
        }
        MathDegrees => {
            let x = one_real(args, model)?;
            model.new_float(x * 180.0 / core::f64::consts::PI)
        }
        MathRadians => {
            let x = one_real(args, model)?;
            model.new_float(x * core::f64::consts::PI / 180.0)
        }
        MathCopysign => {
            let (x, y) = two_reals(args, model)?;
            model.new_float(libm::copysign(x, y))
        }
        MathFmod => {
            let (x, y) = two_reals(args, model)?;
            model.new_float(libm::fmod(x, y))
        }
        MathIsnan => Ok(Value::from_bool(one_real(args, model)?.is_nan())),
        MathIsinf => Ok(Value::from_bool(one_real(args, model)?.is_infinite())),
        MathIsfinite => Ok(Value::from_bool(one_real(args, model)?.is_finite())),
    }
}

/// How [`floor_ceil_trunc`] rounds a float to an integer.
#[derive(Clone, Copy)]
enum Rounding {
    Floor,
    Ceil,
    Trunc,
}

/// `math.floor` / `math.ceil` / `math.trunc`: each returns an `int`. An integer argument is
/// returned unchanged (exact, no float round-trip); a float is rounded then converted to an int.
fn floor_ceil_trunc(args: &[Value], model: &mut ObjectModel, how: Rounding) -> Result<Value, Trap> {
    let [x] = args else { return Err(Trap::TypeError) };
    if let Some(f) = model.float_value(*x) {
        let rounded = match how {
            Rounding::Floor => libm::floor(f),
            Rounding::Ceil => libm::ceil(f),
            Rounding::Trunc => libm::trunc(f),
        };
        return float_to_int(rounded, model);
    }
    if model.is_bigint(*x) {
        return Ok(*x);
    }
    if let Some(n) = model.as_i128(*x) {
        return model.new_long(n);
    }
    Err(real_type_error(model, *x))
}

/// `math.factorial(n)`: `n` must be a non-negative integer (a float -- even integral -- is a
/// `TypeError`). Arbitrary precision, so a large factorial promotes past `long` to a `bigint`.
fn factorial(args: &[Value], model: &mut ObjectModel) -> Result<Value, Trap> {
    let [x] = args else { return Err(Trap::TypeError) };
    let n = integer_arg(*x, model)?;
    if n < 0 {
        return Err(model.with_message(Trap::ValueError, "factorial() not defined for negative values"));
    }
    let mut acc = BigInt::from_i128(1);
    let mut i: i128 = 2;
    while i <= n {
        acc = acc.mul(&BigInt::from_i128(i));
        i += 1;
    }
    model.new_bigint(acc)
}

/// `math.gcd(*integers)`: the greatest common divisor (0 with no arguments or all-zero inputs).
fn gcd(args: &[Value], model: &mut ObjectModel) -> Result<Value, Trap> {
    let mut acc: i128 = 0;
    for &a in args {
        acc = gcd_i128(acc, integer_arg(a, model)?);
    }
    model.new_long(acc)
}

/// `math.lcm(*integers)`: the least common multiple (1 with no arguments, 0 if any input is 0).
fn lcm(args: &[Value], model: &mut ObjectModel) -> Result<Value, Trap> {
    let mut acc: i128 = 1;
    for &a in args {
        let v = integer_arg(a, model)?;
        if v == 0 {
            return model.new_long(0);
        }
        let g = gcd_i128(acc, v);
        acc = (acc / g).checked_mul(v).ok_or(Trap::Overflow)?.abs();
    }
    model.new_long(acc)
}

/// `math.isqrt(n)`: the integer square root (floor of the exact root) of a non-negative integer.
fn isqrt(args: &[Value], model: &mut ObjectModel) -> Result<Value, Trap> {
    let [x] = args else { return Err(Trap::TypeError) };
    let n = integer_arg(*x, model)?;
    if n < 0 {
        return Err(model.with_message(Trap::ValueError, "isqrt() argument must be nonnegative"));
    }
    let mut root = (libm::sqrt(n as f64)) as i128;
    while root > 0 && root.saturating_mul(root) > n {
        root -= 1;
    }
    while (root + 1).saturating_mul(root + 1) <= n {
        root += 1;
    }
    model.new_long(root)
}

/// The greatest common divisor of two `i128`s (Euclid), non-negative.
fn gcd_i128(mut a: i128, mut b: i128) -> i128 {
    a = a.abs();
    b = b.abs();
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}


/// Coerces `value` to an `f64` (int/bool/long/bigint/float), or a `TypeError` ("must be real
/// number, not X") for a non-real argument -- the coercion CPython's math functions apply.
fn real(value: Value, model: &mut ObjectModel) -> Result<f64, Trap> {
    model.as_f64(value).ok_or_else(|| real_type_error(model, value))
}

/// Reads exactly one real argument (arity + coercion).
fn one_real(args: &[Value], model: &mut ObjectModel) -> Result<f64, Trap> {
    let [x] = args else { return Err(Trap::TypeError) };
    real(*x, model)
}

/// Reads exactly two real arguments (arity + coercion).
fn two_reals(args: &[Value], model: &mut ObjectModel) -> Result<(f64, f64), Trap> {
    let [x, y] = args else { return Err(Trap::TypeError) };
    Ok((real(*x, model)?, real(*y, model)?))
}

/// Reads an argument that must be an integer (fixnum / bool / long), for `factorial` / `gcd` /
/// `isqrt`. A float (even integral) or other non-integer is a `TypeError`; a value beyond the
/// `i128` range (a bigint) is an `OverflowError` (these functions work in `i128`).
fn integer_arg(value: Value, model: &mut ObjectModel) -> Result<i128, Trap> {
    if let Some(n) = model.as_i128(value) {
        return Ok(n);
    }
    if model.is_bigint(value) {
        return Err(model.with_message(Trap::Overflow, "int too large to convert"));
    }
    let message = alloc::format!(
        "'{}' object cannot be interpreted as an integer",
        type_name(model, value)
    );
    Err(model.with_message(Trap::TypeError, &message))
}

/// The `ValueError` for a positive-input domain violation (`log`/`log2`/`log10`).
fn positive(x: f64, model: &mut ObjectModel) -> Result<(), Trap> {
    if x <= 0.0 {
        return Err(model.with_message(Trap::ValueError, "expected a positive input"));
    }
    Ok(())
}

/// The `ValueError` for an out-of-`[-1, 1]` domain violation (`asin`/`acos`). A NaN passes
/// (CPython returns NaN rather than raising).
fn unit_range(x: f64, model: &mut ObjectModel) -> Result<(), Trap> {
    #[allow(clippy::manual_range_contains)]
    if x < -1.0 || x > 1.0 {
        return Err(nonnegative_error(model, "expected a number in range from -1 up to 1", x));
    }
    Ok(())
}

/// A `ValueError` of the form `"{prefix}, got {x}"` where `x` renders as CPython's float repr
/// (the input coerced to float). Used by the domain checks that name the offending value.
fn nonnegative_error(model: &mut ObjectModel, prefix: &str, x: f64) -> Trap {
    let got = match model.new_float(x) {
        Ok(v) => model.repr(v),
        Err(_) => String::new(),
    };
    let message = alloc::format!("{prefix}, got {got}");
    model.with_message(Trap::ValueError, &message)
}

/// The `TypeError` for a non-real argument to a math function: `"must be real number, not X"`.
fn real_type_error(model: &mut ObjectModel, value: Value) -> Trap {
    let message = alloc::format!("must be real number, not {}", type_name(model, value));
    model.with_message(Trap::TypeError, &message)
}

/// Converts an integral `f64` (a floor/ceil/trunc result) to an `int` value. NaN/infinity and a
/// magnitude past the `i128` range fault exactly as `int(float)` does (float-to-int is capped at
/// `i128` here -- the same documented bound as the `int()` built-in).
fn float_to_int(f: f64, model: &mut ObjectModel) -> Result<Value, Trap> {
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
}

/// The Python type name of `value` for a diagnostic message (the common built-in types; anything
/// else is "object").
fn type_name(model: &ObjectModel, value: Value) -> &'static str {
    if value == Value::NONE {
        "NoneType"
    } else if value == Value::TRUE || value == Value::FALSE {
        "bool"
    } else if value.is_fixnum() || model.is_long(value) || model.is_bigint(value) {
        "int"
    } else if model.is_float(value) {
        "float"
    } else if model.is_str(value) {
        "str"
    } else if model.is_bytes(value) {
        "bytes"
    } else if model.is_bytearray(value) {
        "bytearray"
    } else if model.is_list(value) {
        "list"
    } else if model.is_tuple(value) {
        "tuple"
    } else if model.is_dict(value) {
        "dict"
    } else if model.is_set(value) {
        "set"
    } else if model.is_frozenset(value) {
        "frozenset"
    } else {
        "object"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ObjectModel {
        ObjectModel::new(Vec::new(), 64 * 1024)
    }

    fn fixnum(n: i32) -> Value {
        Value::fixnum(n).unwrap()
    }

    /// Calls math function `f` with real `args`, returning the result's f64.
    fn call_f(f: StdlibFn, args: &[f64], model: &mut ObjectModel) -> f64 {
        let vals: Vec<Value> = args.iter().map(|&a| model.new_float(a).unwrap()).collect();
        let r = call_stdlib(f.id(), &vals, &[], model, 0).unwrap();
        model.as_f64(r).unwrap()
    }

    #[test]
    fn math_float_functions_match_cpython() {
        let mut m = model();
        assert_eq!(call_f(StdlibFn::MathSqrt, &[2.0], &mut m), core::f64::consts::SQRT_2);
        assert_eq!(call_f(StdlibFn::MathSqrt, &[0.0], &mut m), 0.0);
        assert_eq!(call_f(StdlibFn::MathFabs, &[-3.5], &mut m), 3.5);
        assert_eq!(call_f(StdlibFn::MathPow, &[2.0, 10.0], &mut m), 1024.0);
        assert_eq!(call_f(StdlibFn::MathPow, &[2.0, -1.0], &mut m), 0.5);
        assert_eq!(call_f(StdlibFn::MathExp, &[0.0], &mut m), 1.0);
        assert_eq!(call_f(StdlibFn::MathLog, &[core::f64::consts::E], &mut m), 1.0);
        assert_eq!(call_f(StdlibFn::MathLog, &[8.0, 2.0], &mut m), 3.0);
        assert_eq!(call_f(StdlibFn::MathLog2, &[8.0], &mut m), 3.0);
        assert_eq!(call_f(StdlibFn::MathLog10, &[1000.0], &mut m), 3.0);
        assert_eq!(call_f(StdlibFn::MathSin, &[0.0], &mut m), 0.0);
        assert_eq!(call_f(StdlibFn::MathCos, &[0.0], &mut m), 1.0);
        assert_eq!(call_f(StdlibFn::MathDegrees, &[core::f64::consts::PI], &mut m), 180.0);
        assert_eq!(call_f(StdlibFn::MathRadians, &[180.0], &mut m), core::f64::consts::PI);
        assert_eq!(call_f(StdlibFn::MathAtan2, &[1.0, 1.0], &mut m), core::f64::consts::FRAC_PI_4);
        assert_eq!(call_f(StdlibFn::MathHypot, &[3.0, 4.0], &mut m), 5.0);
        assert_eq!(call_f(StdlibFn::MathCopysign, &[3.0, -1.0], &mut m), -3.0);
        assert_eq!(call_f(StdlibFn::MathFmod, &[10.0, 3.0], &mut m), 1.0);
    }

    #[test]
    fn math_int_returning_functions() {
        let mut m = model();
        let three_seven = m.new_float(3.7).unwrap();
        let r = call_stdlib(StdlibFn::MathFloor.id(), &[three_seven], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(3));
        let three_two = m.new_float(3.2).unwrap();
        let r = call_stdlib(StdlibFn::MathCeil.id(), &[three_two], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(4));
        let neg = m.new_float(-3.7).unwrap();
        let r = call_stdlib(StdlibFn::MathTrunc.id(), &[neg], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(-3));
        let r = call_stdlib(StdlibFn::MathFloor.id(), &[fixnum(5)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(5));
        let r = call_stdlib(StdlibFn::MathFactorial.id(), &[fixnum(5)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(120));
        let r = call_stdlib(StdlibFn::MathGcd.id(), &[fixnum(12), fixnum(18)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(6));
        let r = call_stdlib(StdlibFn::MathLcm.id(), &[fixnum(4), fixnum(6)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(12));
        let r = call_stdlib(StdlibFn::MathIsqrt.id(), &[fixnum(17)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_i128(r), Some(4));
    }

    #[test]
    fn factorial_promotes_to_bigint() {
        let mut m = model();
        let r = call_stdlib(StdlibFn::MathFactorial.id(), &[fixnum(25)], &[], &mut m, 0).unwrap();
        assert_eq!(m.repr(r), "15511210043330985984000000");
    }

    #[test]
    fn math_predicates_are_bools() {
        let mut m = model();
        let inf = m.new_float(f64::INFINITY).unwrap();
        let nan = m.new_float(f64::NAN).unwrap();
        let one = m.new_float(1.0).unwrap();
        assert_eq!(call_stdlib(StdlibFn::MathIsinf.id(), &[inf], &[], &mut m, 0).unwrap(), Value::TRUE);
        assert_eq!(call_stdlib(StdlibFn::MathIsnan.id(), &[nan], &[], &mut m, 0).unwrap(), Value::TRUE);
        assert_eq!(call_stdlib(StdlibFn::MathIsfinite.id(), &[one], &[], &mut m, 0).unwrap(), Value::TRUE);
        assert_eq!(call_stdlib(StdlibFn::MathIsfinite.id(), &[inf], &[], &mut m, 0).unwrap(), Value::FALSE);
    }

    #[test]
    fn math_domain_errors() {
        let mut m = model();
        let neg = m.new_float(-1.0).unwrap();
        let err = call_stdlib(StdlibFn::MathSqrt.id(), &[neg], &[], &mut m, 0).unwrap_err();
        assert_eq!(err, Trap::ValueError);
        let zero = m.new_float(0.0).unwrap();
        assert_eq!(call_stdlib(StdlibFn::MathLog.id(), &[zero], &[], &mut m, 0).unwrap_err(), Trap::ValueError);
        let r = call_stdlib(StdlibFn::MathFactorial.id(), &[fixnum(-1)], &[], &mut m, 0).unwrap_err();
        assert_eq!(r, Trap::ValueError);
        let half = m.new_float(3.5).unwrap();
        assert_eq!(call_stdlib(StdlibFn::MathFactorial.id(), &[half], &[], &mut m, 0).unwrap_err(), Trap::TypeError);
        let two = m.new_float(2.0).unwrap();
        assert_eq!(call_stdlib(StdlibFn::MathAcos.id(), &[two], &[], &mut m, 0).unwrap_err(), Trap::ValueError);
        let big = m.new_float(10000.0).unwrap();
        assert_eq!(call_stdlib(StdlibFn::MathExp.id(), &[big], &[], &mut m, 0).unwrap_err(), Trap::Overflow);
    }

    #[test]
    fn build_math_module_exposes_members() {
        let mut m = model();
        let module = build_math_module(&mut m).unwrap();
        assert!(m.is_module_object(module));
        let ns = m.module_namespace(module);
        assert_eq!(m.dict_get_str(ns, "pi").and_then(|v| m.as_f64(v)), Some(core::f64::consts::PI));
        let sqrt = m.dict_get_str(ns, "sqrt").unwrap();
        assert_eq!(sqrt.as_builtin_id(), Some(StdlibFn::MathSqrt.id()));
        assert_eq!(stdlib_name(StdlibFn::MathSqrt.id()), Some("sqrt"));
    }

    #[test]
    fn stdlib_name_rejects_core_builtin_ids() {
        assert_eq!(stdlib_name(0), None);
        assert_eq!(stdlib_name(STDLIB_BASE - 1), None);
    }
}
