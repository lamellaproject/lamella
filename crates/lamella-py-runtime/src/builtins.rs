//! The runtime's built-in functions -- the first slice of the `builtins` namespace.

use core::cmp::Ordering;

use alloc::string::String;
use alloc::vec::Vec;

use lamella_py_bytecode::{BinOp, CodeObject};

use crate::interp::{binary, call_value, iterator_for};
use crate::object::ObjectModel;
use crate::trap::Trap;
use crate::value::{Value, FIXNUM_MAX, FIXNUM_MIN};

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
            _ => None,
        }
    }

    /// The built-in's stable id.
    #[must_use]
    pub fn id(self) -> u32 {
        self as u32
    }
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
            let n = i128::from(args[0].as_int().ok_or(Trap::TypeError)?);
            fixnum_from_i128(n.abs())
        }
        Builtin::Min => fold_min_max(args, Ordering::Less),
        Builtin::Max => fold_min_max(args, Ordering::Greater),
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
            if let Some(str_method) = model.find_dunder(args[0], "__str__") {
                return call_value(str_method, &[], functions, model, depth);
            }
            model.py_str(args[0])
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
                [it] => (*it, 0),
                [it, s] => (*it, s.as_int().ok_or(Trap::TypeError)?),
                _ => return Err(Trap::TypeError),
            };
            let elements = collect_iterable(model, &[iterable], functions, depth)?;
            let mut pairs = Vec::with_capacity(elements.len());
            for (i, element) in elements.into_iter().enumerate() {
                let index =
                    Value::fixnum(i32::try_from(start + i as i64).map_err(|_| Trap::Overflow)?)
                        .ok_or(Trap::Overflow)?;
                pairs.push(model.new_tuple(alloc::vec![index, element])?);
            }
            model.new_list(pairs)
        }
        Builtin::Sum => {
            let (iterable, start) = match args {
                [it] => (*it, 0i128),
                [it, s] => (*it, i128::from(s.as_int().ok_or(Trap::TypeError)?)),
                _ => return Err(Trap::TypeError),
            };
            let elements = collect_iterable(model, &[iterable], functions, depth)?;
            let mut acc = start;
            for element in elements {
                acc += i128::from(element.as_int().ok_or(Trap::TypeError)?);
            }
            fixnum_from_i128(acc)
        }
        Builtin::Sorted => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let elements = collect_iterable(model, &[args[0]], functions, depth)?;
            sorted_list(model, elements)
        }
        Builtin::Bool => match args {
            [] => Ok(Value::FALSE),
            [x] => Ok(Value::from_bool(model.py_truthy(*x)?.unwrap_or(true))),
            _ => Err(Trap::TypeError),
        },
        Builtin::Repr => {
            if args.len() != 1 {
                return Err(Trap::TypeError);
            }
            let rendered = model.repr(args[0]);
            model.new_str(&rendered)
        }
        Builtin::Int => match args {
            [] => Value::fixnum(0).ok_or(Trap::Overflow),
            [x] => {
                if let Some(n) = x.as_int() {
                    Value::fixnum(i32::try_from(n).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)
                } else if let Some(s) = model.str_value(*x) {
                    let parsed: i64 = s.trim().parse().map_err(|_| Trap::ValueError)?;
                    Value::fixnum(i32::try_from(parsed).map_err(|_| Trap::Overflow)?)
                        .ok_or(Trap::Overflow)
                } else {
                    Err(Trap::TypeError)
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
            let func = args[0];
            let mut columns: Vec<Vec<Value>> = Vec::with_capacity(args.len() - 1);
            for arg in &args[1..] {
                columns.push(collect_iterable(model, &[*arg], functions, depth)?);
            }
            let rows = columns.iter().map(|c| c.len()).min().unwrap_or(0);
            let mut result = Vec::with_capacity(rows);
            for r in 0..rows {
                let call_args: Vec<Value> = columns.iter().map(|c| c[r]).collect();
                result.push(call_value(func, &call_args, functions, model, depth)?);
            }
            model.new_list(result)
        }
        Builtin::Filter => {
            if args.len() != 2 {
                return Err(Trap::TypeError);
            }
            let func = args[0];
            let elements = collect_iterable(model, &[args[1]], functions, depth)?;
            let mut result = Vec::new();
            for element in elements {
                let keep = if func.is_none() {
                    model.py_truthy(element)?.unwrap_or(true)
                } else {
                    let outcome = call_value(func, &[element], functions, model, depth)?;
                    model.py_truthy(outcome)?.unwrap_or(true)
                };
                if keep {
                    result.push(element);
                }
            }
            model.new_list(result)
        }
        Builtin::Zip => {
            let mut columns: Vec<Vec<Value>> = Vec::with_capacity(args.len());
            for arg in args {
                columns.push(collect_iterable(model, &[*arg], functions, depth)?);
            }
            let rows = columns.iter().map(|c| c.len()).min().unwrap_or(0);
            let mut result = Vec::with_capacity(rows);
            for r in 0..rows {
                let row: Vec<Value> = columns.iter().map(|c| c[r]).collect();
                result.push(model.new_tuple(row)?);
            }
            model.new_list(result)
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
            let quotient = binary(BinOp::FloorDiv, *a, *b)?;
            let remainder = binary(BinOp::Mod, *a, *b)?;
            model.new_tuple(alloc::vec![quotient, remainder])
        }
        Builtin::Pow => match args {
            [base, exp] => {
                let b = i128::from(base.as_int().ok_or(Trap::TypeError)?);
                let e = exp.as_int().ok_or(Trap::TypeError)?;
                if e < 0 {
                    return Err(Trap::ValueError);
                }
                let mut acc: i128 = 1;
                for _ in 0..e {
                    acc = acc.checked_mul(b).ok_or(Trap::Overflow)?;
                }
                fixnum_from_i128(acc)
            }
            [base, exp, modulus] => {
                let b = i128::from(base.as_int().ok_or(Trap::TypeError)?);
                let e = exp.as_int().ok_or(Trap::TypeError)?;
                let m = i128::from(modulus.as_int().ok_or(Trap::TypeError)?);
                if e < 0 || m == 0 {
                    return Err(Trap::ValueError);
                }
                let mut acc: i128 = 1i128.rem_euclid(m);
                let mut base_mod = b.rem_euclid(m);
                let mut bits = e;
                while bits > 0 {
                    if bits & 1 == 1 {
                        acc = (acc * base_mod).rem_euclid(m);
                    }
                    base_mod = (base_mod * base_mod).rem_euclid(m);
                    bits >>= 1;
                }
                fixnum_from_i128(acc)
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
                || model.is_py_bound(x);
            Ok(Value::from_bool(callable))
        }
        Builtin::Next => {
            let (iterator, default) = match args {
                [it] => (*it, None),
                [it, d] => (*it, Some(*d)),
                _ => return Err(Trap::TypeError),
            };
            if !model.is_iter(iterator) {
                return Err(Trap::TypeError);
            }
            match model.py_next(iterator)? {
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
    }
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

/// The display form of `value` for `print`/`str`: an instance's `__str__` if its class defines
/// one, else the default rendering (int/str/container/...).
fn display_arg(
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<String, Trap> {
    if let Some(str_method) = model.find_dunder(value, "__str__") {
        let result = call_value(str_method, &[], functions, model, depth)?;
        if let Some(s) = model.str_value(result) {
            return Ok(String::from(s));
        }
        return Ok(model.display(result));
    }
    Ok(model.display(value))
}

/// Sorts the collected elements into a new list: int elements numerically, str lexicographically;
/// a mixed or otherwise unorderable set is a `TypeError`.
fn sorted_list(model: &mut ObjectModel, mut elements: Vec<Value>) -> Result<Value, Trap> {
    model.sort_values(&mut elements)?;
    model.new_list(elements)
}

/// Drains `list(...)`/`tuple(...)`'s optional single iterable argument into a vector (empty for
/// the no-argument form), via the iterator protocol so any iterable works.
fn collect_iterable(
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
    while let Some(item) = model.py_next(iterator)? {
        elems.push(item);
    }
    Ok(elems)
}

/// Materializes an `i128` as a fixnum, trapping `Overflow` outside the fixnum range.
fn fixnum_from_i128(v: i128) -> Result<Value, Trap> {
    if v < i128::from(FIXNUM_MIN) || v > i128::from(FIXNUM_MAX) {
        return Err(Trap::Overflow);
    }
    Value::fixnum(v as i32).ok_or(Trap::Overflow)
}

/// Folds `min`/`max` over the positional arguments (`keep` = `Less` for min, `Greater`
/// for max), returning the first extreme argument -- matching Python's tie behavior.
///
/// Compares int/bool arguments numerically and needs at least two of them; the
/// single-iterable form (`min([..])`) and the `__lt__` protocol compose with containers
/// and the broader object model.
fn fold_min_max(args: &[Value], keep: Ordering) -> Result<Value, Trap> {
    if args.len() < 2 {
        return Err(Trap::TypeError);
    }
    let mut best = args[0];
    let mut best_n = best.as_int().ok_or(Trap::TypeError)?;
    for &arg in &args[1..] {
        let n = arg.as_int().ok_or(Trap::TypeError)?;
        if n.cmp(&best_n) == keep {
            best = arg;
            best_n = n;
        }
    }
    Ok(best)
}
