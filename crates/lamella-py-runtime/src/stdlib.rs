//! The standard-library modules the interpreter provides natively.

use alloc::string::String;
#[cfg(feature = "float")]
use alloc::vec;
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
    /// `math.frexp(x)` -- the (mantissa, exponent) pair with `x == m * 2**e` and `0.5 <= |m| < 1`.
    MathFrexp,
    /// `math.ldexp(x, i)` -- `x * 2**i`, the inverse of `frexp`.
    MathLdexp,
    /// `math.modf(x)` -- the (fractional, integer) parts, both floats, both carrying `x`'s sign.
    MathModf,
    /// `_time.time_ns()` -- nanoseconds since the Unix epoch, from the host's wall clock.
    TimeTimeNs,
    /// `_time.monotonic_ns()` -- nanoseconds from an arbitrary origin that never goes backwards.
    TimeMonotonicNs,
    /// `_time.sleep_ns(n)` -- block for `n` nanoseconds.
    TimeSleepNs,
    /// `_struct.pack_float(value, size, big_endian)` -- the exact IEEE-754 bytes of a float.
    StructPackFloat,
    /// `_struct.unpack_float(data, big_endian)` -- the float those bytes are.
    StructUnpackFloat,
    /// `_fs.listdir(path)` -- the names directly inside a directory.
    FsListdir,
    /// `_fs.remove(path)` -- delete a file.
    FsRemove,
    /// `_fs.mkdir(path)` -- create a directory.
    FsMkdir,
    /// `_fs.rmdir(path)` -- remove an empty directory.
    FsRmdir,
    /// `_fs.rename(src, dst)` -- rename a file or directory.
    FsRename,
    /// `_fs.kind(path)` -- `(is_directory, size)`; raises if the path does not exist.
    FsKind,
    /// `weakref.ref(object)` -- a reference that does not keep its target alive.
    WeakrefRef,
    /// `_reactor.park(id, deadline_ms)` -- park an opaque waiter id on a timer deadline.
    ReactorPark,
    /// `_reactor.unpark(id)` -- drop a waiter's park (its timer was cancelled).
    ReactorUnpark,
    /// `_reactor.block_point()` -- the ONE blocking wait; the woken ids, or `None` for "nothing to
    /// wait for".
    ReactorBlockPoint,
    /// `_reactor.now_ms()` -- the monotonic clock in the same unit deadlines are given in.
    ReactorNowMs,
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
            34 => MathFrexp,
            35 => MathLdexp,
            36 => MathModf,
            37 => TimeTimeNs,
            38 => TimeMonotonicNs,
            39 => TimeSleepNs,
            40 => StructPackFloat,
            41 => StructUnpackFloat,
            42 => FsListdir,
            43 => FsRemove,
            44 => FsMkdir,
            45 => FsRmdir,
            46 => FsRename,
            47 => FsKind,
            48 => WeakrefRef,
            49 => ReactorPark,
            50 => ReactorUnpark,
            51 => ReactorBlockPoint,
            52 => ReactorNowMs,
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
            MathFrexp => "frexp",
            MathLdexp => "ldexp",
            MathModf => "modf",
            TimeTimeNs => "time_ns",
            TimeMonotonicNs => "monotonic_ns",
            TimeSleepNs => "sleep_ns",
            StructPackFloat => "pack_float",
            StructUnpackFloat => "unpack_float",
            FsListdir => "listdir",
            FsRemove => "remove",
            FsMkdir => "mkdir",
            FsRmdir => "rmdir",
            FsRename => "rename",
            FsKind => "kind",
            WeakrefRef => "ref",
            ReactorPark => "park",
            ReactorUnpark => "unpark",
            ReactorBlockPoint => "block_point",
            ReactorNowMs => "now_ms",
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
                | StdlibFn::WeakrefRef
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
        StdlibFn::WeakrefRef => "weakref",
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
        StdlibFn::WeakrefRef => Some(model.is_weakref(value)),
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
        "_platform" => Some(build_platform_module(model)),
        "_time" => Some(build_time_module(model)),
        "_fs" => Some(build_fs_module(model)),
        "_struct" => Some(build_struct_seam_module(model)),
        "weakref" => Some(build_weakref_module(model)),
        "_reactor" => Some(build_reactor_module(model)),
        _ => None,
    }
}

/// Builds `_reactor`: the block point an event loop waits on, and nothing above it.
///
/// Native because the wait itself is -- parking an OS thread on the nearest deadline is not something
/// Python can express -- and DELIBERATELY no more than that. The park store and the idle algorithm are
/// `lamella-reactor`'s, shared with the C# tier's scheduler and the AOT one so a sleep lands in the
/// SAME wait on every tier; the ready queue, the futures and the tasks are `asyncio`'s, in Python,
/// where a program can read them.
///
/// A waiter is an opaque `u32` on both sides of this seam: nothing below knows what a coroutine is.
fn build_reactor_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    use StdlibFn::*;
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for f in [ReactorPark, ReactorUnpark, ReactorBlockPoint, ReactorNowMs] {
        let key = model.new_str(f.python_name())?;
        entries.push((key, Value::builtin_ref(f.id())));
    }
    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// Builds `weakref`: a reference that names an object without keeping it alive.
///
/// Native rather than managed Python because the whole of it is a collector property -- the target
/// slot has to be one the collector neither traces nor leaves stale, and nothing written in Python
/// can ask for that. What a program gets is CPython's shape: `weakref.ref(obj)` is callable and
/// answers the target, or `None` once the target has been reclaimed.
///
/// `ref` and `ReferenceType` are the SAME object here as in CPython (`weakref.ref is
/// weakref.ReferenceType`), so `isinstance(r, weakref.ref)` and the `ReferenceType` spelling both
/// work off one type.
fn build_weakref_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for name in ["ref", "ReferenceType"] {
        let key = model.new_str(name)?;
        entries.push((key, Value::builtin_ref(StdlibFn::WeakrefRef.id())));
    }
    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// Builds `_struct`: the float half of `struct`, which is the only part that cannot be written in
/// Python. Reading a float's bits requires seeing the representation; everything else `struct` does
/// is integer arithmetic the language already performs exactly.
fn build_struct_seam_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    use StdlibFn::*;
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for f in [StructPackFloat, StructUnpackFloat] {
        let key = model.new_str(f.python_name())?;
        entries.push((key, Value::builtin_ref(f.id())));
    }
    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// Builds `_time`: the raw clock seam, in nanoseconds and nothing else. The `time` module builds
/// CPython's surface (seconds as floats, the sleep argument, the aliases) on top of it in Python --
/// so this side stays the smallest thing that must be native, which is reading the host's clock.
fn build_time_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    use StdlibFn::*;
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for f in [TimeTimeNs, TimeMonotonicNs, TimeSleepNs] {
        let key = model.new_str(f.python_name())?;
        entries.push((key, Value::builtin_ref(f.id())));
    }
    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// Builds `_fs`: the directory half of the filesystem seam. `open()` is a built-in (a file is an
/// object, not a module function), so what is left here is what `os` is made of -- and `os` itself is
/// managed Python, because joining path strings needs no host underneath it.
fn build_fs_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    use StdlibFn::*;
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for f in [FsListdir, FsRemove, FsMkdir, FsRmdir, FsRename, FsKind] {
        let key = model.new_str(f.python_name())?;
        entries.push((key, Value::builtin_ref(f.id())));
    }
    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
}

/// The C ABI of the target this runtime was compiled for, DERIVED from the compiler rather than
/// written down: each entry is what `size_of`/`align_of` report for that C type, so the table
/// cannot disagree with the code the same toolchain emitted.
///
/// This is why it is derived. `long` is 4 bytes under Windows' LLP64 and on a 32-bit device, and 8
/// under LP64 -- so a hand-written table would be right on one machine and silently wrong on
/// another, and the symptom would be a packed byte landing in the wrong place on silicon rather
/// than a failing build.
const C_TYPES: &[(&str, usize, usize)] = &[
    ("char", size_of::<core::ffi::c_char>(), align_of::<core::ffi::c_char>()),
    ("bool", size_of::<bool>(), align_of::<bool>()),
    ("short", size_of::<core::ffi::c_short>(), align_of::<core::ffi::c_short>()),
    ("int", size_of::<core::ffi::c_int>(), align_of::<core::ffi::c_int>()),
    ("long", size_of::<core::ffi::c_long>(), align_of::<core::ffi::c_long>()),
    (
        "long long",
        size_of::<core::ffi::c_longlong>(),
        align_of::<core::ffi::c_longlong>(),
    ),
    ("float", size_of::<f32>(), align_of::<f32>()),
    ("double", size_of::<f64>(), align_of::<f64>()),
    ("size_t", size_of::<usize>(), align_of::<usize>()),
    ("void*", size_of::<*const ()>(), align_of::<*const ()>()),
];

/// Builds `_platform`: the target's ABI facts, in one place because more than one caller needs
/// them and none of them should guess. `struct`'s native mode reads the sizes and alignments to lay
/// a record out the way the platform's C compiler would; `sys` reads `byteorder` and `maxsize`.
///
/// These are properly platform-DEPENDENT: the answer on a host and the answer on a device differ,
/// and both are correct. That is why they are answered by the runtime a program is actually running
/// on rather than baked in at any earlier stage.
fn build_platform_module(model: &mut ObjectModel) -> Result<Value, Trap> {
    let mut entries: Vec<(Value, Value)> = Vec::new();

    let key = model.new_str("byteorder")?;
    let order = if cfg!(target_endian = "big") { "big" } else { "little" };
    let value = model.new_str(order)?;
    entries.push((key, value));

    let key = model.new_str("maxsize")?;
    let value = model.new_bigint(BigInt::from_i128(isize::MAX as i128))?;
    entries.push((key, value));

    let key = model.new_str("version")?;
    let value = model.new_str(env!("CARGO_PKG_VERSION"))?;
    entries.push((key, value));

    let mut sizes: Vec<(Value, Value)> = Vec::new();
    let mut aligns: Vec<(Value, Value)> = Vec::new();
    for &(name, size, align) in C_TYPES {
        let size_key = model.new_str(name)?;
        let align_key = model.new_str(name)?;
        sizes.push((size_key, Value::fixnum(size as i32).ok_or(Trap::Overflow)?));
        aligns.push((align_key, Value::fixnum(align as i32).ok_or(Trap::Overflow)?));
    }
    let key = model.new_str("sizes")?;
    let value = model.new_dict(sizes)?;
    entries.push((key, value));
    let key = model.new_str("aligns")?;
    let value = model.new_dict(aligns)?;
    entries.push((key, value));

    let namespace = model.new_dict(entries)?;
    model.new_module(namespace)
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

    #[cfg(feature = "float")]
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
        MathFmod, MathIsnan, MathIsinf, MathIsfinite, MathFrexp, MathLdexp, MathModf,
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
        TimeTimeNs | TimeMonotonicNs => {
            if !args.is_empty() {
                return Err(Trap::TypeError);
            }
            let nanos = if StdlibFn::from_id(id) == Some(TimeTimeNs) {
                model.now_ns()?
            } else {
                model.monotonic_ns()?
            };
            model.new_bigint(BigInt::from_i128(i128::from(nanos)))
        }
        #[cfg(not(feature = "float"))]
        StructPackFloat => Err(Trap::FloatUnavailable),
        #[cfg(feature = "float")]
        StructPackFloat => {
            let [value, size, big_endian] = args else {
                return Err(Trap::TypeError);
            };
            let Some(value) = model.as_f64(*value) else {
                let message = "required argument is not a float";
                return Err(model.raise_named_exception("TypeError", message));
            };
            let big = model.as_i128(*big_endian).unwrap_or(0) != 0;
            let bytes: Vec<u8> = match model.as_i128(*size) {
                Some(4) => {
                    let narrowed = value as f32;
                    if value.is_finite() && narrowed.is_infinite() {
                        let message = "float too large to pack with f format";
                        return Err(model.raise_named_exception("OverflowError", message));
                    }
                    if big {
                        narrowed.to_be_bytes().to_vec()
                    } else {
                        narrowed.to_le_bytes().to_vec()
                    }
                }
                Some(8) => {
                    if big {
                        value.to_be_bytes().to_vec()
                    } else {
                        value.to_le_bytes().to_vec()
                    }
                }
                _ => return Err(Trap::TypeError),
            };
            model.new_bytes(bytes)
        }
        FsListdir | FsRemove | FsMkdir | FsRmdir | FsRename | FsKind => {
            let path = match args.first().and_then(|value| model.str_bytes(*value)) {
                Some(_) => String::from(model.str_text(args[0])?),
                None => {
                    let kind = args.first().map_or_else(
                        || String::from("nothing"),
                        |value| model.type_name_of(*value),
                    );
                    let message =
                        alloc::format!("expected str, bytes or os.PathLike object, not {kind}");
                    return Err(model.raise_named_exception("TypeError", &message));
                }
            };
            let which = StdlibFn::from_id(id).ok_or(Trap::Malformed)?;
            let expected = if which == FsRename { 2 } else { 1 };
            if args.len() != expected {
                return Err(Trap::TypeError);
            }
            match which {
                FsListdir => model.fs_listdir(&path),
                FsRemove => model.fs_remove(&path),
                FsMkdir => model.fs_mkdir(&path),
                FsRmdir => model.fs_rmdir(&path),
                FsKind => model.fs_kind(&path),
                _ => {
                    let to = String::from(model.str_text(args[1])?);
                    model.fs_rename(&path, &to)
                }
            }
        }
        WeakrefRef => {
            let [target] = args else {
                return Err(Trap::TypeError);
            };
            if !model.supports_weak_reference(*target) {
                let name = model.type_name_of(*target);
                let message = alloc::format!("cannot create weak reference to '{name}' object");
                return Err(model.raise_named_exception("TypeError", &message));
            }
            model.new_weakref(*target)
        }
        StructUnpackFloat => {
            let [data, big_endian] = args else {
                return Err(Trap::TypeError);
            };
            let Some(data) = model.bytes_value(*data).map(<[u8]>::to_vec) else {
                return Err(Trap::TypeError);
            };
            let big = model.as_i128(*big_endian).unwrap_or(0) != 0;
            let value = match data.len() {
                4 => {
                    let mut word = [0u8; 4];
                    word.copy_from_slice(&data);
                    let narrow = if big {
                        f32::from_be_bytes(word)
                    } else {
                        f32::from_le_bytes(word)
                    };
                    f64::from(narrow)
                }
                8 => {
                    let mut word = [0u8; 8];
                    word.copy_from_slice(&data);
                    if big {
                        f64::from_be_bytes(word)
                    } else {
                        f64::from_le_bytes(word)
                    }
                }
                _ => return Err(Trap::TypeError),
            };
            model.new_float(value)
        }
        TimeSleepNs => {
            let [nanos] = args else {
                return Err(Trap::TypeError);
            };
            let Some(nanos) = model.as_i128(*nanos) else {
                let message = "sleep_ns() takes a whole number of nanoseconds";
                return Err(model.raise_named_exception("TypeError", message));
            };
            let nanos = i64::try_from(nanos).unwrap_or(i64::MAX);
            model.sleep_ns(nanos)?;
            Ok(Value::NONE)
        }
        ReactorPark => {
            let [id, deadline] = args else {
                return Err(Trap::TypeError);
            };
            let (Some(id), Some(deadline)) = (model.as_i128(*id), model.as_i128(*deadline)) else {
                let message = "park() takes a waiter id and a deadline in milliseconds";
                return Err(model.raise_named_exception("TypeError", message));
            };
            let id = u32::try_from(id).map_err(|_| Trap::TypeError)?;
            model.park_waiter(id, u64::try_from(deadline).unwrap_or(0));
            Ok(Value::NONE)
        }
        ReactorUnpark => {
            let [id] = args else {
                return Err(Trap::TypeError);
            };
            let Some(id) = model.as_i128(*id) else {
                let message = "unpark() takes a waiter id";
                return Err(model.raise_named_exception("TypeError", message));
            };
            model.unpark_waiter(u32::try_from(id).map_err(|_| Trap::TypeError)?);
            Ok(Value::NONE)
        }
        ReactorBlockPoint => {
            if !args.is_empty() {
                return Err(Trap::TypeError);
            }
            let Some(woken) = model.reactor_block_point() else {
                return Ok(Value::NONE);
            };
            let ids = woken
                .into_iter()
                .map(|id| Value::fixnum(id as i32).ok_or(Trap::Overflow))
                .collect::<Result<Vec<Value>, Trap>>()?;
            model.new_list(ids)
        }
        ReactorNowMs => {
            if !args.is_empty() {
                return Err(Trap::TypeError);
            }
            model.new_bigint(BigInt::from_i128(i128::from(model.reactor_now_millis())))
        }
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
            let name = model.str_text(*name_arg).map(String::from)?;
            let fields: Vec<String> = if model.str_bytes(*fields_arg).is_some() {
                let spec = model.str_text(*fields_arg)?;
                spec.replace(',', " ").split_whitespace().map(String::from).collect()
            } else {
                let items = model.seq_value(*fields_arg).cloned().ok_or(Trap::TypeError)?;
                let mut fields = Vec::with_capacity(items.len());
                for item in items {
                    fields.push(model.str_text(item).map(String::from)?);
                }
                fields
            };
            model.new_ntclass(&name, &fields)
        }
        #[cfg(feature = "float")]
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
        #[cfg(feature = "float")]
        MathFabs => {
            let x = one_real(args, model)?;
            model.new_float(libm::fabs(x))
        }
        MathFactorial => factorial(args, model),
        MathGcd => gcd(args, model),
        MathLcm => lcm(args, model),
        MathIsqrt => isqrt(args, model),
        #[cfg(feature = "float")]
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
        #[cfg(feature = "float")]
        MathExp => {
            let x = one_real(args, model)?;
            let r = libm::exp(x);
            if r.is_infinite() && x.is_finite() {
                return Err(model.with_message(Trap::Overflow, "math range error"));
            }
            model.new_float(r)
        }
        #[cfg(feature = "float")]
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
        #[cfg(feature = "float")]
        MathLog2 => {
            let x = one_real(args, model)?;
            positive(x, model)?;
            model.new_float(libm::log2(x))
        }
        #[cfg(feature = "float")]
        MathLog10 => {
            let x = one_real(args, model)?;
            positive(x, model)?;
            model.new_float(libm::log10(x))
        }
        #[cfg(feature = "float")]
        MathSin => {
            let x = one_real(args, model)?;
            model.new_float(libm::sin(x))
        }
        #[cfg(feature = "float")]
        MathCos => {
            let x = one_real(args, model)?;
            model.new_float(libm::cos(x))
        }
        #[cfg(feature = "float")]
        MathTan => {
            let x = one_real(args, model)?;
            model.new_float(libm::tan(x))
        }
        #[cfg(feature = "float")]
        MathAsin => {
            let x = one_real(args, model)?;
            unit_range(x, model)?;
            model.new_float(libm::asin(x))
        }
        #[cfg(feature = "float")]
        MathAcos => {
            let x = one_real(args, model)?;
            unit_range(x, model)?;
            model.new_float(libm::acos(x))
        }
        #[cfg(feature = "float")]
        MathAtan => {
            let x = one_real(args, model)?;
            model.new_float(libm::atan(x))
        }
        #[cfg(feature = "float")]
        MathAtan2 => {
            let (y, x) = two_reals(args, model)?;
            model.new_float(libm::atan2(y, x))
        }
        #[cfg(feature = "float")]
        MathHypot => {
            let coords: Vec<f64> = args.iter().map(|&a| real(a, model)).collect::<Result<_, _>>()?;
            let r = match coords.as_slice() {
                [] => 0.0,
                [a, b] => libm::hypot(*a, *b),
                many => libm::sqrt(many.iter().map(|v| v * v).sum()),
            };
            model.new_float(r)
        }
        #[cfg(feature = "float")]
        MathDegrees => {
            let x = one_real(args, model)?;
            model.new_float(x * 180.0 / core::f64::consts::PI)
        }
        #[cfg(feature = "float")]
        MathRadians => {
            let x = one_real(args, model)?;
            model.new_float(x * core::f64::consts::PI / 180.0)
        }
        #[cfg(feature = "float")]
        MathCopysign => {
            let (x, y) = two_reals(args, model)?;
            model.new_float(libm::copysign(x, y))
        }
        #[cfg(feature = "float")]
        MathFmod => {
            let (x, y) = two_reals(args, model)?;
            model.new_float(libm::fmod(x, y))
        }
        #[cfg(feature = "float")]
        MathIsnan => Ok(Value::from_bool(one_real(args, model)?.is_nan())),
        #[cfg(feature = "float")]
        MathIsinf => Ok(Value::from_bool(one_real(args, model)?.is_infinite())),
        #[cfg(feature = "float")]
        MathIsfinite => Ok(Value::from_bool(one_real(args, model)?.is_finite())),
        #[cfg(feature = "float")]
        MathFrexp => {
            let (mantissa, exponent) = libm::frexp(one_real(args, model)?);
            let mantissa = model.new_float(mantissa)?;
            let exponent = float_to_int(f64::from(exponent), model)?;
            model.new_tuple(vec![mantissa, exponent])
        }
        #[cfg(feature = "float")]
        MathLdexp => {
            let [x, i] = args else { return Err(Trap::TypeError) };
            let x = real(*x, model)?;
            if !model.is_int(*i) {
                return Err(model.with_message(
                    Trap::TypeError,
                    "Expected an int as second argument to ldexp.",
                ));
            }
            let exponent = model.as_f64(*i).ok_or(Trap::TypeError)?;
            let exponent = if exponent > f64::from(i32::MAX) {
                i32::MAX
            } else if exponent < f64::from(i32::MIN) {
                i32::MIN
            } else {
                exponent as i32
            };
            let scaled = libm::ldexp(x, exponent);
            if scaled.is_infinite() && x.is_finite() {
                return Err(model.with_message(Trap::Overflow, "math range error"));
            }
            model.new_float(scaled)
        }
        #[cfg(feature = "float")]
        MathModf => {
            let (fractional, integral) = libm::modf(one_real(args, model)?);
            let fractional = model.new_float(fractional)?;
            let integral = model.new_float(integral)?;
            model.new_tuple(vec![fractional, integral])
        }
        #[cfg(not(feature = "float"))]
        _ => Err(Trap::FloatUnavailable),
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
    #[cfg(feature = "float")]
    if let Some(f) = model.float_value(*x) {
        let rounded = match how {
            Rounding::Floor => libm::floor(f),
            Rounding::Ceil => libm::ceil(f),
            Rounding::Trunc => libm::trunc(f),
        };
        return float_to_int(rounded, model);
    }
    #[cfg(not(feature = "float"))]
    let _ = how;
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
    let mut root = if n == 0 { 0 } else { 1i128 << (128 - n.leading_zeros()).div_ceil(2) };
    while root > 0 {
        let next = (root + n / root) / 2;
        if next >= root {
            break;
        }
        root = next;
    }
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
#[cfg(feature = "float")]
fn real(value: Value, model: &mut ObjectModel) -> Result<f64, Trap> {
    model.as_f64(value).ok_or_else(|| real_type_error(model, value))
}

/// Reads exactly one real argument (arity + coercion).
#[cfg(feature = "float")]
fn one_real(args: &[Value], model: &mut ObjectModel) -> Result<f64, Trap> {
    let [x] = args else { return Err(Trap::TypeError) };
    real(*x, model)
}

/// Reads exactly two real arguments (arity + coercion).
#[cfg(feature = "float")]
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
#[cfg(feature = "float")]
fn positive(x: f64, model: &mut ObjectModel) -> Result<(), Trap> {
    if x <= 0.0 {
        return Err(model.with_message(Trap::ValueError, "expected a positive input"));
    }
    Ok(())
}

/// The `ValueError` for an out-of-`[-1, 1]` domain violation (`asin`/`acos`). A NaN passes
/// (CPython returns NaN rather than raising). `asin`/`acos` are float-only, so this is too.
#[cfg(feature = "float")]
fn unit_range(x: f64, model: &mut ObjectModel) -> Result<(), Trap> {
    #[allow(clippy::manual_range_contains)]
    if x < -1.0 || x > 1.0 {
        return Err(nonnegative_error(model, "expected a number in range from -1 up to 1", x));
    }
    Ok(())
}

/// A `ValueError` of the form `"{prefix}, got {x}"` where `x` renders as CPython's float repr
/// (the input coerced to float). Used by the domain checks that name the offending value -- all of
/// which are float entry points, so this has no callers on the no-float tier.
#[cfg(feature = "float")]
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
#[cfg(feature = "float")]
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

    #[cfg(feature = "float")]
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

    #[cfg(feature = "float")]
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

    /// frexp splits a double into a mantissa in [0.5, 1) and an exponent that ldexp puts back --
    /// exactly, including the subnormal that needs frexp's internal rescale.
    #[cfg(feature = "float")]
    #[test]
    fn frexp_and_ldexp_round_trip() {
        let mut m = model();
        for value in [3.5_f64, -3.5, 1e300, 5e-324, 0.5] {
            let x = m.new_float(value).unwrap();
            let pair = call_stdlib(StdlibFn::MathFrexp.id(), &[x], &[], &mut m, 0).unwrap();
            let parts = m.seq_value(pair).cloned().unwrap();
            let mantissa = m.as_f64(parts[0]).unwrap();
            assert!((0.5..1.0).contains(&libm::fabs(mantissa)), "mantissa {mantissa} of {value}");
            let back =
                call_stdlib(StdlibFn::MathLdexp.id(), &[parts[0], parts[1]], &[], &mut m, 0).unwrap();
            assert_eq!(m.as_f64(back), Some(value));
        }
    }

    /// ldexp's exponent reads through the int lane, so every integer tier works -- notably `bool`,
    /// which is an int in Python but is not a fixnum here.
    #[cfg(feature = "float")]
    #[test]
    fn ldexp_accepts_every_integer_tier_and_refuses_a_float() {
        let mut m = model();
        let one = m.new_float(1.0).unwrap();
        let r = call_stdlib(StdlibFn::MathLdexp.id(), &[one, Value::TRUE], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_f64(r), Some(2.0));
        let r = call_stdlib(StdlibFn::MathLdexp.id(), &[one, Value::FALSE], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_f64(r), Some(1.0));
        let half = m.new_float(1.5).unwrap();
        assert!(matches!(
            call_stdlib(StdlibFn::MathLdexp.id(), &[one, half], &[], &mut m, 0),
            Err(Trap::TypeError)
        ));
    }

    /// An exponent past the double range raises, but only for a finite input: inf/nan pass through,
    /// and scaling toward zero underflows silently.
    #[cfg(feature = "float")]
    #[test]
    fn ldexp_overflows_only_a_finite_input() {
        let mut m = model();
        let one = m.new_float(1.0).unwrap();
        assert!(matches!(
            call_stdlib(StdlibFn::MathLdexp.id(), &[one, fixnum(2000)], &[], &mut m, 0),
            Err(Trap::Overflow)
        ));
        let r = call_stdlib(StdlibFn::MathLdexp.id(), &[one, fixnum(-2000)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_f64(r), Some(0.0));
        let inf = m.new_float(f64::INFINITY).unwrap();
        let r = call_stdlib(StdlibFn::MathLdexp.id(), &[inf, fixnum(2000)], &[], &mut m, 0).unwrap();
        assert_eq!(m.as_f64(r), Some(f64::INFINITY));
    }

    /// modf yields (fractional, integer) -- Python's order, and both parts carry the sign.
    #[cfg(feature = "float")]
    #[test]
    fn modf_splits_fraction_then_integer() {
        let mut m = model();
        let x = m.new_float(-3.5).unwrap();
        let pair = call_stdlib(StdlibFn::MathModf.id(), &[x], &[], &mut m, 0).unwrap();
        let parts = m.seq_value(pair).cloned().unwrap();
        assert_eq!(m.as_f64(parts[0]), Some(-0.5));
        assert_eq!(m.as_f64(parts[1]), Some(-3.0));
    }

    #[cfg(feature = "float")]
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

    #[cfg(feature = "float")]
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

    #[cfg(feature = "float")]
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
