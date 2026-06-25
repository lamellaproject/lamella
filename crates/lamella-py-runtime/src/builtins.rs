//! The runtime's built-in functions -- the first slice of the `builtins` namespace.

use core::cmp::Ordering;

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
        _ => return None,
    };
    Some(builtin.id())
}

/// Calls built-in `id` with `args` (Python 3.14.6 "Built-in Functions").
pub fn call_builtin(id: u32, args: &[Value], model: &ObjectModel) -> Result<Value, Trap> {
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
            model.py_len(args[0])
        }
    }
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
