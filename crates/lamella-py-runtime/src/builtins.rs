//! The runtime's built-in functions -- the first slice of the `builtins` namespace.

use core::cmp::Ordering;

use alloc::string::String;
use alloc::vec::Vec;

use lamella_py_bytecode::CodeObject;

use crate::interp::{call_value, iterator_for};
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
    }
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
    if elements.iter().all(|e| e.as_int().is_some()) {
        elements.sort_by_key(|e| e.as_int().unwrap_or(0));
    } else if elements.iter().all(|e| model.str_value(*e).is_some()) {
        let mut keyed: Vec<(String, Value)> = elements
            .iter()
            .map(|e| (String::from(model.str_value(*e).unwrap_or("")), *e))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        elements = keyed.into_iter().map(|(_, v)| v).collect();
    } else {
        return Err(Trap::TypeError);
    }
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
