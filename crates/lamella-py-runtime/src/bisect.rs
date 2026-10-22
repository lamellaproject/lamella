//! The `_bisect` module: `bisect_left`, `bisect_right`, `insort_left` and `insort_right`, which
//! `bisect.py` imports and re-exports beside its two aliases, `bisect` and `insort`.
//!
//! Each function finds where `x` belongs in the sorted sequence `a` by halving the range `[lo, hi)`.
//! The probe is `a[(lo + hi) // 2]`, read afresh at every step, through the container's own
//! `__getitem__` when it has one. The left variants keep `x` before the entries equal to it, by
//! moving past an entry less than `x`; the right variants keep it after them, by stopping at an entry
//! greater than `x`. The only operation applied to elements is `<`, evaluated as the `<` operator
//! evaluates it, and its result is taken for its truth. With `key`, `key(entry)` takes the entry's
//! place in every comparison, while `x` is compared as given; `insort` compares `key(x)`, computed
//! once before the search, and inserts `x` itself through the container's `insert` method.
//!
//! The steps run in a fixed order, each before the next: the arguments are bound and `lo` and `hi`
//! converted, `insort` computes `key(x)`, a negative `lo` is refused, an absent `hi` (or `-1`) becomes
//! `len(a)`, and a container that cannot be indexed by position is refused, even for an empty range.
//! Only then does the search begin.

use alloc::format;
use alloc::vec::Vec;

use lamella_py_bytecode::{CmpOp, Functions};

use crate::interp::{call_value, compare_values, getattr_hooked, py_truthy_dyn};
use crate::object::{InlineCache, ObjectModel};
use crate::trap::Trap;
use crate::value::Value;

/// The four functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Function {
    BisectLeft,
    BisectRight,
    InsortLeft,
    InsortRight,
}

impl Function {
    /// Whether the function places `x` after the entries equal to it rather than before them.
    fn right(self) -> bool {
        matches!(self, Function::BisectRight | Function::InsortRight)
    }

    /// Whether the function inserts `x` rather than answering where it goes.
    fn inserts(self) -> bool {
        matches!(self, Function::InsortLeft | Function::InsortRight)
    }
}

/// The five parameters, in their positional order; `key` is accepted by name only.
const PARAMETERS: [&str; 5] = ["a", "x", "lo", "hi", "key"];

/// One call's arguments once bound: `lo` and `hi` converted to indexes, and `hi` `None` when it was
/// absent or `None`.
struct Bound {
    a: Value,
    x: Value,
    lo: i64,
    hi: Option<i64>,
    key: Value,
}

/// Calls `_bisect.<name>(*args, **kwargs)`: binds the arguments, then searches, then inserts for an
/// `insort` function. A search answers the index; an insertion answers `None`.
pub(crate) fn call(
    function: Function,
    name: &str,
    args: &[Value],
    kwargs: &[(&str, Value)],
    functions: &Functions,
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let Bound { a, x, lo, hi, key } = bind(name, args, kwargs, functions, model, depth)?;
    let insort = function.inserts();
    let wanted = if insort && !key.is_none() {
        call_value(key, &[x], functions, model, depth + 1)?
    } else {
        x
    };
    if lo < 0 {
        return Err(model.raise_named_exception("ValueError", "lo must be non-negative"));
    }
    let hi = match hi {
        None | Some(-1) => length(a, functions, model, depth)?,
        Some(hi) => hi,
    };
    check_indexable(a, model)?;
    let search = Search { a, key, functions, depth };
    let index = search.run(function.right(), wanted, lo, hi, model)?;
    if insort {
        insert(a, index, x, functions, model, depth)?;
        return Ok(Value::NONE);
    }
    model.int_from_i128(i128::from(index))
}

/// Binds one call to the five parameters, raising the first error in it, then converts `lo` and
/// `hi`, in that order.
fn bind(
    name: &str,
    args: &[Value],
    kwargs: &[(&str, Value)],
    functions: &Functions,
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Bound, Trap> {
    check_call(name, args.len(), kwargs, model)?;
    let named = |wanted: &str| kwargs.iter().find(|(keyword, _)| *keyword == wanted).map(|&(_, value)| value);
    let a = args.first().copied().or_else(|| named("a")).ok_or(Trap::Malformed)?;
    let x = args.get(1).copied().or_else(|| named("x")).ok_or(Trap::Malformed)?;
    let lo = args.get(2).copied().or_else(|| named("lo"));
    let hi = args.get(3).copied().or_else(|| named("hi"));
    let key = named("key").unwrap_or(Value::NONE);
    let lo = match lo {
        Some(lo) => lo_index(lo, functions, model, depth)?,
        None => 0,
    };
    let hi = match hi {
        Some(hi) if !hi.is_none() => Some(hi_index(hi, functions, model, depth)?),
        _ => None,
    };
    Ok(Bound { a, x, lo, hi, key })
}

/// Raises the first error in a call with `given` positional arguments and these keywords, in the
/// order a built-in function's argument parser finds them: too many arguments in all, too many by
/// position, a required one missing, one given both by position and by name, then an unknown
/// keyword. Returns when the call binds.
fn check_call(name: &str, given: usize, kwargs: &[(&str, Value)], model: &mut ObjectModel) -> Result<(), Trap> {
    let named = |parameter: &str| kwargs.iter().any(|(keyword, _)| *keyword == parameter);
    let total = given + kwargs.len();
    let message = if total > PARAMETERS.len() {
        if given > 0 {
            format!("{name}() takes at most 5 arguments ({total} given)")
        } else {
            format!("{name}() takes at most 5 keyword arguments ({total} given)")
        }
    } else if given > 4 {
        format!("{name}() takes at most 4 positional arguments ({given} given)")
    } else if let Some(missing) = (given..2).find(|&position| !named(PARAMETERS[position])) {
        format!("{name}() missing required argument '{}' (pos {})", PARAMETERS[missing], missing + 1)
    } else if let Some(twice) = (0..given).find(|&position| named(PARAMETERS[position])) {
        format!("argument for {name}() given by name ('{}') and position ({})", PARAMETERS[twice], twice + 1)
    } else if let Some(&(unknown, _)) = kwargs.iter().find(|(keyword, _)| !PARAMETERS.contains(keyword)) {
        let mut message = format!("{name}() got an unexpected keyword argument '{unknown}'");
        if let Some(meant) = suggestion(unknown) {
            message.push_str(&format!(". Did you mean '{meant}'?"));
        }
        message
    } else {
        return Ok(());
    };
    Err(model.raise_named_exception("TypeError", &message))
}

/// The parameter an unknown keyword most likely meant, if one is close enough.
///
/// The cost of turning the keyword into a parameter's name is counted over their UTF-8 bytes: 2 to
/// insert, delete or replace a byte, and 1 to replace an ASCII letter by the same letter in the other
/// case. A name qualifies when the cost is at most a third of the two lengths plus 3, rounded down;
/// the cheapest qualifying name wins, the earliest parameter on a tie.
fn suggestion(word: &str) -> Option<&'static str> {
    let mut best: Option<(&'static str, usize)> = None;
    for name in PARAMETERS {
        let cost = edit_cost(word.as_bytes(), name.as_bytes());
        let qualifies = cost <= (word.len() + name.len() + 3) / 3;
        if qualifies && best.is_none_or(|(_, cheapest)| cost < cheapest) {
            best = Some((name, cost));
        }
    }
    best.map(|(name, _)| name)
}

/// The cheapest edit from the bytes of `word` to the ASCII `name`, at [`suggestion`]'s costs.
fn edit_cost(word: &[u8], name: &[u8]) -> usize {
    let mut previous: Vec<usize> = (0..=name.len()).map(|column| 2 * column).collect();
    for &unit in word {
        let mut current = Vec::with_capacity(name.len() + 1);
        current.push(previous[0] + 2);
        for (column, &code) in name.iter().enumerate() {
            let step = if unit == code {
                0
            } else if unit.eq_ignore_ascii_case(&code) {
                1
            } else {
                2
            };
            let cheapest = (previous[column] + step).min(previous[column + 1] + 2).min(current[column] + 2);
            current.push(cheapest);
        }
        previous = current;
    }
    previous[name.len()]
}

/// The integer `value` stands for as an index: an int (a bool included) at its own value, and an
/// instance through its type's `__index__`, which must return an int. `None` when `value` is none of
/// these. An int too large for an `i128` reads as the `i128` bound on its side, which no index fits
/// either.
fn index_value(value: Value, functions: &Functions, model: &mut ObjectModel, depth: usize) -> Result<Option<i128>, Trap> {
    let value = if model.is_instance(value) {
        let Some(hook) = model.find_dunder(value, "__index__") else { return Ok(None) };
        let result = call_value(hook, &[], functions, model, depth + 1)?;
        if !model.is_int(result) {
            let type_name = model.type_name_of(result);
            let message = format!("__index__ returned non-int (type {type_name})");
            return Err(model.raise_named_exception("TypeError", &message));
        }
        result
    } else if model.is_int(value) {
        value
    } else {
        return Ok(None);
    };
    if let Some(n) = model.as_i128(value) {
        return Ok(Some(n));
    }
    let negative = model.bigint_value(value).is_some_and(|n| n.is_negative());
    Ok(Some(if negative { i128::MIN } else { i128::MAX }))
}

/// `lo` as an index: an integer or index-like value that fits 64 bits.
fn lo_index(value: Value, functions: &Functions, model: &mut ObjectModel, depth: usize) -> Result<i64, Trap> {
    let Some(n) = index_value(value, functions, model, depth)? else {
        let type_name = model.type_name_of(value);
        let message = format!("'{type_name}' object cannot be interpreted as an integer");
        return Err(model.raise_named_exception("TypeError", &message));
    };
    i64::try_from(n)
        .map_err(|_| model.raise_named_exception("OverflowError", "Python int too large to convert to C ssize_t"))
}

/// `hi`, when it is not `None`, as an index: an integer or index-like value that fits 64 bits.
fn hi_index(value: Value, functions: &Functions, model: &mut ObjectModel, depth: usize) -> Result<i64, Trap> {
    let type_name = model.type_name_of(value);
    let Some(n) = index_value(value, functions, model, depth)? else {
        let message = format!("argument should be integer or None, not '{type_name}'");
        return Err(model.raise_named_exception("TypeError", &message));
    };
    i64::try_from(n).map_err(|_| {
        let message = format!("cannot fit '{type_name}' into an index-sized integer");
        model.raise_named_exception("OverflowError", &message)
    })
}

/// `len(a)` as an index. A length from `__len__` must be an integer or index-like, then not
/// negative, then fit 64 bits, each refused in `len`'s own words.
fn length(a: Value, functions: &Functions, model: &mut ObjectModel, depth: usize) -> Result<i64, Trap> {
    let Some(method) = model.find_dunder(a, "__len__") else {
        let n = match model.py_len(a) {
            Err(Trap::TypeError) => return Err(model.len_type_error(a)),
            other => other?,
        };
        return model.as_i128(n).and_then(|n| i64::try_from(n).ok()).ok_or(Trap::Malformed);
    };
    let result = call_value(method, &[], functions, model, depth + 1)?;
    let Some(n) = index_value(result, functions, model, depth)? else {
        let type_name = model.type_name_of(result);
        let message = format!("'{type_name}' object cannot be interpreted as an integer");
        return Err(model.raise_named_exception("TypeError", &message));
    };
    if n < 0 {
        return Err(model.raise_named_exception("ValueError", "__len__() should return >= 0"));
    }
    i64::try_from(n)
        .map_err(|_| model.raise_named_exception("OverflowError", "cannot fit 'int' into an index-sized integer"))
}

/// Refuses a container that cannot be indexed by position. A mapping is not a sequence, with one
/// exception: a `Counter`, whose missing keys read as 0, is indexed by its integer keys. Any other
/// value must have `__getitem__` on its type: a user class's own, or a built-in sequence's.
fn check_indexable(a: Value, model: &mut ObjectModel) -> Result<(), Trap> {
    let indexable = if model.is_instance(a) {
        model.find_dunder(a, "__getitem__").is_some()
    } else if model.is_dict(a) && !model.is_counter(a) {
        let message = format!("{} is not a sequence", model.tp_name_of(a));
        return Err(model.raise_named_exception("TypeError", &message));
    } else {
        model.is_counter(a)
            || model.seq_value(a).is_some()
            || model.is_str(a)
            || model.is_bytes(a)
            || model.is_bytearray(a)
            || model.is_memoryview(a)
            || model.is_range(a)
            || model.is_deque(a)
    };
    if indexable {
        return Ok(());
    }
    let message = format!("'{}' object does not support indexing", model.type_name_of(a));
    Err(model.raise_named_exception("TypeError", &message))
}

/// Inserts `x` at `index` through `a.insert`, looked up the way `a.insert` is looked up.
fn insert(a: Value, index: i64, x: Value, functions: &Functions, model: &mut ObjectModel, depth: usize) -> Result<(), Trap> {
    let mut cache = InlineCache::empty();
    let method = match getattr_hooked(a, "insert", &mut cache, functions, model, depth) {
        Err(Trap::AttributeError) => return Err(model.attribute_error(a, "insert")),
        other => other?,
    };
    let index = model.int_from_i128(i128::from(index))?;
    call_value(method, &[index, x], functions, model, depth + 1)?;
    Ok(())
}

/// One search over the container `a`, with its key function (`None` for none).
struct Search<'f> {
    a: Value,
    key: Value,
    functions: &'f Functions,
    depth: usize,
}

impl Search<'_> {
    /// The insertion point for `x` within `[lo, hi)`, where `lo` is not negative: after the entries
    /// equal to `x` when `right` is set, before them otherwise.
    fn run(&self, right: bool, x: Value, mut lo: i64, mut hi: i64, model: &mut ObjectModel) -> Result<i64, Trap> {
        while lo < hi {
            // `lo` is not negative, so `hi - lo` cannot overflow and the midpoint rounds down.
            let mid = lo + (hi - lo) / 2;
            let entry = self.entry(mid, model)?;
            let goes_before = if right { self.less(x, entry, model)? } else { !self.less(entry, x, model)? };
            if goes_before {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok(lo)
    }

    /// `a[index]`, passed through `key` when there is one. A list's element is read from its storage,
    /// since nothing can change how a list indexes; any other container is subscripted as the `[]`
    /// operator subscripts it.
    fn entry(&self, index: i64, model: &mut ObjectModel) -> Result<Value, Trap> {
        let stored = usize::try_from(index)
            .ok()
            .filter(|_| model.is_list(self.a))
            .and_then(|position| model.seq_value(self.a)?.get(position).copied());
        let entry = match stored {
            Some(entry) => entry,
            None => {
                let index = model.int_from_i128(i128::from(index))?;
                match model.find_dunder(self.a, "__getitem__") {
                    Some(method) => call_value(method, &[index], self.functions, model, self.depth + 1)?,
                    None => model.py_getitem_dyn(self.a, index, self.functions, self.depth)?,
                }
            }
        };
        if self.key.is_none() {
            return Ok(entry);
        }
        call_value(self.key, &[entry], self.functions, model, self.depth + 1)
    }

    /// Whether `left < right`, as the `<` operator evaluates it, by the truth of its result.
    fn less(&self, left: Value, right: Value, model: &mut ObjectModel) -> Result<bool, Trap> {
        let outcome = compare_values(CmpOp::Lt, left, right, self.functions, model, self.depth)?;
        py_truthy_dyn(outcome, self.functions, model, self.depth)
    }
}
