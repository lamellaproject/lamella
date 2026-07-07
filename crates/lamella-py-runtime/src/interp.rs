//! The bytecode interpreter.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use lamella_gc::Ref;
use lamella_py_bytecode::{BinOp, CmpOp, CodeObject, Const, ExcEntry, Op, UnaryOp};

use crate::bigint::BigInt;
use crate::object::{InlineCache, ObjectModel};
use crate::trap::Trap;
use crate::value::{Value, FIXNUM_MAX, FIXNUM_MIN};

/// One activation record: the instruction pointer, the local-variable slots, the evaluation
/// stack, this code's inline caches, and any exception currently being handled.
///
/// A frame holds everything an in-progress call needs, so a driver over an explicit frame
/// stack can suspend it (a generator, step 4) or hold it without a native Rust activation
/// behind it (step 3). The `ip` and `caches` live here rather than as `exec` locals for the
/// same reason: a suspended frame carries its own resume point and cache state.
///
/// Every [`Value`] it holds is a GC root traced by tag ([`Frame::trace`]); `ip` and `caches`
/// carry no managed pointers, so they are not traced.
#[derive(Debug)]
pub struct Frame {
    /// Which code this frame runs (an index, not a borrow -- see [`CodeId`]). The driver
    /// resolves it to a [`CodeObject`] each step.
    code: CodeId,
    /// Whether this is the module body (its `StoreFast`s mirror into the module globals). Only
    /// the entry frame of [`run_module`] sets it; a called function never does.
    is_module: bool,
    /// The instruction pointer: the index into the code's `ops` of the next op to run.
    ip: usize,
    locals: Vec<Value>,
    stack: Vec<Value>,
    /// This activation's inline-cache slots -- one per cacheable site, sized by the code's
    /// `cache_count` and indexed by each [`Op::LoadAttr`]'s `cache` field. The bytecode is
    /// immutable (flash-resident under XIP); the caches are the per-activation RAM side array
    /// (`lamella_py_bytecode` module note), so they belong to the frame, not the code.
    caches: Vec<InlineCache>,
    /// The exception currently being handled in this frame -- set on entry to an `except` block,
    /// cleared by `PopExcept` -- or `None`. A GC root while set (traced by [`Frame::trace`]), so an
    /// allocation inside a handler body cannot free the in-flight exception out from under it.
    active_exception: Option<Value>,
    /// The deref array for closures: `[0 .. cellvars.len())` are this frame's OWN cells (locals a
    /// nested function captures), then the captured cells the closure carried in. `LoadDeref` /
    /// `StoreDeref` / `LoadClosure` index it; empty for a function with no cell/free variables. Each
    /// slot is a `Cell` (a heap object), so all are GC roots (traced by [`Frame::trace`]).
    derefs: Vec<Value>,
    /// The active class-body namespace dict while executing a `class` body (`SetupClassNamespace`
    /// sets it, `StoreName`/`LoadName` target it, `BuildClass` consumes it), else `None`. A GC root
    /// while set (traced by [`Frame::trace`]) so building the namespace cannot free it.
    class_namespace: Option<Value>,
}

impl Frame {
    /// A frame with `num_locals` slots (every local initially [`Value::UNBOUND`] so a read
    /// before assignment traps rather than reading garbage) and `cache_count` cold inline
    /// caches, its instruction pointer at the first op.
    #[must_use]
    pub fn new(num_locals: usize, cache_count: usize) -> Frame {
        let mut locals = Vec::with_capacity(num_locals);
        locals.resize(num_locals, Value::UNBOUND);
        let mut caches = Vec::with_capacity(cache_count);
        caches.resize(cache_count, InlineCache::empty());
        Frame {
            code: CodeId::Entry,
            is_module: false,
            ip: 0,
            locals,
            stack: Vec::new(),
            caches,
            active_exception: None,
            derefs: Vec::new(),
            class_namespace: None,
        }
    }

    /// Empties the frame for return to the pool: drops every held `Value` (locals, eval stack,
    /// caches, in-flight exception) so nothing stale is retained, KEEPING the Vec allocations for
    /// [`new_frame`] to reuse. Clearing (not just draining on reuse) also keeps the pool safe for a
    /// future safe-point collector: a pooled frame holds no reference to trace or dangle.
    pub(crate) fn clear_for_reuse(&mut self) {
        self.locals.clear();
        self.stack.clear();
        self.caches.clear();
        self.active_exception = None;
        self.derefs.clear();
        self.class_namespace = None;
    }

    /// Pushes a value onto the evaluation stack.
    fn push(&mut self, value: Value) {
        self.stack.push(value);
    }

    /// Pops a value, or [`Trap::StackUnderflow`] on an empty stack.
    fn pop(&mut self) -> Result<Value, Trap> {
        self.stack.pop().ok_or(Trap::StackUnderflow)
    }

    /// Reads the top of the evaluation stack WITHOUT removing it, or [`Trap::StackUnderflow`] if
    /// empty. For an op that inspects the top and leaves it (e.g. `ImportFrom` reads a member off
    /// the module it keeps on the stack for the next one).
    fn peek(&self) -> Result<Value, Trap> {
        self.stack.last().copied().ok_or(Trap::StackUnderflow)
    }

    /// Reads local slot `idx`, trapping on an out-of-range slot or an unbound local.
    fn load_local(&self, idx: usize) -> Result<Value, Trap> {
        let value = *self.locals.get(idx).ok_or(Trap::Malformed)?;
        if value.is_unbound() {
            return Err(Trap::UnboundLocal);
        }
        Ok(value)
    }

    /// Writes local slot `idx`, trapping on an out-of-range slot.
    fn store_local(&mut self, idx: usize, value: Value) -> Result<(), Trap> {
        *self.locals.get_mut(idx).ok_or(Trap::Malformed)? = value;
        Ok(())
    }

    /// Reports every slot the frame holds (locals and the evaluation stack) to the
    /// collector, tracing each *by tag*: pointer slots relocate in place, immediates
    /// are skipped. Drive it from [`lamella_gc::Heap::collect`]'s root closure.
    pub fn trace(&mut self, visit: &mut dyn FnMut(&mut Ref)) {
        for slot in self.locals.iter_mut() {
            Value::trace_slot(slot, visit);
        }
        for slot in self.stack.iter_mut() {
            Value::trace_slot(slot, visit);
        }
        for slot in self.derefs.iter_mut() {
            Value::trace_slot(slot, visit);
        }
        if let Some(exc) = self.active_exception.as_mut() {
            Value::trace_slot(exc, visit);
        }
        if let Some(namespace) = self.class_namespace.as_mut() {
            Value::trace_slot(namespace, visit);
        }
    }
}

/// Materializes a constant-pool entry (other than a string) as a runtime value.
///
/// An integer outside the fixnum range overflows (Python's `int` is unbounded -- data
/// model, Numbers -- so larger values would be bignums, which this representation does
/// not hold). A string constant is materialized by the `LoadConst` handler, not here.
fn const_value(c: &Const) -> Result<Value, Trap> {
    match c {
        Const::None => Ok(Value::NONE),
        Const::Bool(b) => Ok(Value::from_bool(*b)),
        Const::Int(n) => {
            if *n >= i64::from(FIXNUM_MIN) && *n <= i64::from(FIXNUM_MAX) {
                Value::fixnum(*n as i32).ok_or(Trap::Overflow)
            } else {
                Err(Trap::Overflow)
            }
        }
        Const::Str(_)
        | Const::KwNames(_)
        | Const::ArgKinds(_)
        | Const::Float(_)
        | Const::Imaginary(_)
        | Const::BigInt(_)
        | Const::Bytes(_) => Err(Trap::Unsupported),
    }
}

/// Evaluates a binary arithmetic / bitwise operator over two `int`/`bool` operands
/// (`bool` is an int subtype -- see `value.rs::as_int`), computed in `i128` so a fixnum
/// overflow is detected exactly (no wrap). Operands of inappropriate type are a
/// `TypeError` (Python 3.14.6 "Built-in Exceptions"); the dynamic `py_binop` over
/// arbitrary types (the reflected `__add__`/`__radd__` protocol) composes with the broader
/// object model.
///
/// Semantics follow Python's signed/arbitrary-precision `int`: `& | ^` are exact bitwise
/// over the (infinite) two's-complement value; `<<` is a left shift; `>>` is an
/// ARITHMETIC (sign-propagating) right shift (`-8 >> 1 == -4`); a negative shift count is
/// a `ValueError`. A result outside the 31-bit fixnum range promotes to a heap `long` (an i128;
/// the i128-range first increment, a result beyond i128 is `Trap::Overflow`). `//` floors toward
/// negative infinity
/// and `%` takes the divisor's sign (with `x == (x // y) * y + (x % y)`, Python 3.14.6
/// "Binary arithmetic operations"); a zero divisor raises `ZeroDivisionError`.
pub(crate) fn binary(op: BinOp, a: Value, b: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
    #[cfg(feature = "complex")]
    if model.is_complex(a) || model.is_complex(b) {
        return complex_binary(op, a, b, model);
    }
    if op == BinOp::TrueDiv || model.is_float(a) || model.is_float(b) {
        let x = model.as_f64(a).ok_or(Trap::TypeError)?;
        let y = model.as_f64(b).ok_or(Trap::TypeError)?;
        return float_binary(op, x, y, model);
    }
    let (x, y) = match (model.as_i128(a), model.as_i128(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return bigint_binary(op, a, b, model),
    };
    let result: i128 = match op {
        BinOp::Add => match x.checked_add(y) {
            Some(r) => r,
            None => return bigint_binary(op, a, b, model),
        },
        BinOp::Sub => match x.checked_sub(y) {
            Some(r) => r,
            None => return bigint_binary(op, a, b, model),
        },
        BinOp::Mul => match x.checked_mul(y) {
            Some(r) => r,
            None => return bigint_binary(op, a, b, model),
        },
        BinOp::TrueDiv => unreachable!("true division took the float path"),
        BinOp::Pow => {
            if y < 0 {
                return float_pow(x as f64, y as f64, model);
            }
            let exp = u32::try_from(y).map_err(|_| Trap::Overflow)?;
            match x.checked_pow(exp) {
                Some(r) => r,
                None => return model.new_bigint(bigint_pow(&BigInt::from_i128(x), exp)),
            }
        }
        BinOp::BitAnd => x & y,
        BinOp::BitOr => x | y,
        BinOp::BitXor => x ^ y,
        BinOp::LShift => {
            if y < 0 {
                return Err(Trap::ValueError);
            } else if x == 0 {
                0
            } else if y >= 128 {
                return bigint_binary(op, a, b, model);
            } else {
                let shifted = x.wrapping_shl(y as u32);
                if shifted >> (y as u32) != x {
                    return bigint_binary(op, a, b, model);
                }
                shifted
            }
        }
        BinOp::RShift => {
            if y < 0 {
                return Err(Trap::ValueError);
            }
            x >> y.min(127)
        }
        BinOp::FloorDiv => {
            if y == 0 {
                return Err(Trap::ZeroDivisionError);
            }
            let q = x.checked_div(y).ok_or(Trap::Overflow)?;
            let r = x.checked_rem(y).ok_or(Trap::Overflow)?;
            if r != 0 && (r < 0) != (y < 0) { q - 1 } else { q }
        }
        BinOp::Mod => {
            if y == 0 {
                return Err(Trap::ZeroDivisionError);
            }
            let r = x.checked_rem(y).ok_or(Trap::Overflow)?;
            if r != 0 && (r < 0) != (y < 0) { r + y } else { r }
        }
        BinOp::MatMul => return Err(Trap::TypeError),
    };
    model.new_long(result)
}

/// Evaluates a binary operator with an arbitrary-precision `int` operand (or an i128 op that
/// overflowed): `+ - *`, floor division / modulo, shifts, and the bitwise operators in full
/// `BigInt` precision, normalized back down to the smallest int tier.
fn bigint_binary(op: BinOp, a: Value, b: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
    let x = model.as_bigint(a).ok_or(Trap::TypeError)?;
    let y = model.as_bigint(b).ok_or(Trap::TypeError)?;
    match op {
        BinOp::Add => model.new_bigint(x.add(&y)),
        BinOp::Sub => model.new_bigint(x.sub(&y)),
        BinOp::Mul => model.new_bigint(x.mul(&y)),
        BinOp::FloorDiv => {
            let (quotient, _) = x.divmod(&y).ok_or(Trap::ZeroDivisionError)?;
            model.new_bigint(quotient)
        }
        BinOp::Mod => {
            let (_, remainder) = x.divmod(&y).ok_or(Trap::ZeroDivisionError)?;
            model.new_bigint(remainder)
        }
        BinOp::LShift => {
            let shift = shift_count(b, model)?;
            model.new_bigint(x.shl(shift))
        }
        BinOp::RShift => {
            let shift = shift_count(b, model)?;
            model.new_bigint(x.shr(shift))
        }
        BinOp::BitAnd => model.new_bigint(x.bitand(&y)),
        BinOp::BitOr => model.new_bigint(x.bitor(&y)),
        BinOp::BitXor => model.new_bigint(x.bitxor(&y)),
        BinOp::TrueDiv | BinOp::Pow => Err(Trap::Overflow),
        BinOp::MatMul => Err(Trap::TypeError),
    }
}

/// The shift amount for a `<<`/`>>` with a big operand: a non-negative integer as a `u64`. A
/// negative shift count is a `ValueError` (Python's rule); an absurdly large one is an
/// `OverflowError`.
fn shift_count(value: Value, model: &ObjectModel) -> Result<u64, Trap> {
    let n = model.as_i128(value).ok_or(Trap::TypeError)?;
    if n < 0 {
        return Err(Trap::ValueError);
    }
    u64::try_from(n).map_err(|_| Trap::Overflow)
}

/// `base ** exp` for a non-negative integer exponent, in arbitrary precision -- binary exponentiation
/// over `BigInt` multiplication (so `2 ** 200` is exact). Backs the `**` operator and `pow()` once the
/// i128 power overflows.
pub(crate) fn bigint_pow(base: &BigInt, exp: u32) -> BigInt {
    let mut result = BigInt::from_i128(1);
    let mut squared = base.clone();
    let mut bits = exp;
    while bits > 0 {
        if bits & 1 == 1 {
            result = result.mul(&squared);
        }
        bits >>= 1;
        if bits > 0 {
            squared = squared.mul(&squared);
        }
    }
    result
}

/// Evaluates a binary operator over two doubles (the float path of [`binary`]). `+ - * /` are the
/// IEEE-754 operations; `//` and `%` follow Python's floor semantics (result has the divisor's
/// sign) exactly as CPython's `float_divmod`. A zero divisor for `/`, `//`, or `%` raises
/// `ZeroDivisionError` (Python NEVER produces an IEEE infinity/NaN from float division by zero).
/// The bitwise/shift operators do not apply to floats -- a `TypeError` (`1.0 & 2` in CPython).
fn float_binary(op: BinOp, x: f64, y: f64, model: &mut ObjectModel) -> Result<Value, Trap> {
    let result: f64 = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::TrueDiv => {
            if y == 0.0 {
                return Err(Trap::ZeroDivisionError);
            }
            x / y
        }
        BinOp::FloorDiv => {
            if y == 0.0 {
                return Err(Trap::ZeroDivisionError);
            }
            float_floordiv(x, y)
        }
        BinOp::Mod => {
            if y == 0.0 {
                return Err(Trap::ZeroDivisionError);
            }
            float_mod(x, y)
        }
        BinOp::Pow => return float_pow(x, y, model),
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::LShift | BinOp::RShift
        | BinOp::MatMul => {
            return Err(Trap::TypeError);
        }
    };
    model.new_float(result)
}

/// Python's `**` on doubles (the shared path for the `**` operator and the `pow()` builtin): IEEE
/// `pow`, EXCEPT a ZERO base to a NEGATIVE power raises `ZeroDivisionError` ("zero to a negative
/// power" -- Python never yields an infinity there, `0.0 ** -1` is an error, not `inf`). A NEGATIVE
/// base to a NON-INTEGER power is a COMPLEX number in CPython (`float.__pow__` delegates to complex
/// power); under the `complex` knob the interpreter returns that complex, else it yields `NaN`
/// (libm::pow's result there) -- the documented divergence on a tier without complex.
pub(crate) fn float_pow(base: f64, exp: f64, model: &mut ObjectModel) -> Result<Value, Trap> {
    if base == 0.0 && exp < 0.0 {
        return Err(model.with_message(Trap::ZeroDivisionError, "zero to a negative power"));
    }
    #[cfg(feature = "complex")]
    if base < 0.0 && exp != libm::floor(exp) {
        return complex_pow((base, 0.0), (exp, 0.0), model);
    }
    model.new_float(libm::pow(base, exp))
}

/// Evaluates a binary operator with at least one `complex` operand (the complex path of [`binary`]).
/// `+ - *` are componentwise / Gaussian; `/` uses Smith's algorithm (CPython's `_Py_c_quot`); `**`
/// uses exact repeated multiplication for a small integer exponent and the polar form otherwise. An
/// int/float operand promotes to `(x, 0)`. Floor division, modulo, and the bitwise/shift operators
/// are not defined for complex -- a `TypeError` (as in CPython).
#[cfg(feature = "complex")]
fn complex_binary(op: BinOp, a: Value, b: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
    let lhs = model.as_complex(a).ok_or(Trap::TypeError)?;
    let rhs = model.as_complex(b).ok_or(Trap::TypeError)?;
    let (re, im) = match op {
        BinOp::Add => (lhs.0 + rhs.0, lhs.1 + rhs.1),
        BinOp::Sub => (lhs.0 - rhs.0, lhs.1 - rhs.1),
        BinOp::Mul => complex_prod(lhs, rhs),
        BinOp::TrueDiv => complex_quot(lhs, rhs)?,
        BinOp::Pow => return complex_pow(lhs, rhs, model),
        _ => return Err(Trap::TypeError),
    };
    model.new_complex(re, im)
}

/// Complex multiplication `(a+bi)(c+di) = (ac-bd) + (ad+bc)i` (CPython's `_Py_c_prod`).
#[cfg(feature = "complex")]
fn complex_prod((ar, ai): (f64, f64), (br, bi): (f64, f64)) -> (f64, f64) {
    (ar * br - ai * bi, ar * bi + ai * br)
}

/// Complex division via Smith's algorithm (CPython's `_Py_c_quot`): scale by the larger-magnitude
/// denominator component to avoid overflow. A zero divisor raises `ZeroDivisionError`.
#[cfg(feature = "complex")]
fn complex_quot((ar, ai): (f64, f64), (br, bi): (f64, f64)) -> Result<(f64, f64), Trap> {
    let (abs_br, abs_bi) = (libm::fabs(br), libm::fabs(bi));
    if abs_br >= abs_bi {
        if abs_br == 0.0 {
            return Err(Trap::ZeroDivisionError);
        }
        let ratio = bi / br;
        let denom = br + bi * ratio;
        Ok(((ar + ai * ratio) / denom, (ai - ar * ratio) / denom))
    } else if abs_bi >= abs_br {
        let ratio = br / bi;
        let denom = br * ratio + bi;
        Ok(((ar * ratio + ai) / denom, (ai * ratio - ar) / denom))
    } else {
        Ok((f64::NAN, f64::NAN))
    }
}

/// Complex exponentiation (CPython's `complex_pow`): an integer real exponent with magnitude `<= 100`
/// uses EXACT repeated multiplication (so `(1+2j) ** 2 == (-3+4j)` bit-for-bit), everything else the
/// polar form. A zero base to a negative or complex power raises `ZeroDivisionError`.
#[cfg(feature = "complex")]
fn complex_pow(base: (f64, f64), exp: (f64, f64), model: &mut ObjectModel) -> Result<Value, Trap> {
    let (re, im) = if exp.1 == 0.0 && exp.0 == libm::floor(exp.0) && libm::fabs(exp.0) <= 100.0 {
        complex_powi(base, exp.0 as i64)?
    } else {
        complex_pow_polar(base, exp)?
    };
    model.new_complex(re, im)
}

/// `base ** n` for an integer `n` by binary exponentiation over complex multiplication (CPython's
/// `c_powi`/`c_powu`); a negative `n` inverts the positive power.
#[cfg(feature = "complex")]
fn complex_powi(base: (f64, f64), n: i64) -> Result<(f64, f64), Trap> {
    fn powu(x: (f64, f64), n: i64) -> (f64, f64) {
        let mut result = (1.0, 0.0);
        let mut squared = x;
        let mut mask = 1i64;
        while mask > 0 && n >= mask {
            if n & mask != 0 {
                result = complex_prod(result, squared);
            }
            mask <<= 1;
            squared = complex_prod(squared, squared);
        }
        result
    }
    match n.cmp(&0) {
        core::cmp::Ordering::Greater => Ok(powu(base, n)),
        core::cmp::Ordering::Less => complex_quot((1.0, 0.0), powu(base, -n)),
        core::cmp::Ordering::Equal => Ok((1.0, 0.0)),
    }
}

/// `base ** exp` via the polar form `r^exp` with the angle (CPython's `_Py_c_pow`), for a
/// non-integer or complex exponent. A zero base to a negative/complex power raises.
#[cfg(feature = "complex")]
fn complex_pow_polar((ar, ai): (f64, f64), (br, bi): (f64, f64)) -> Result<(f64, f64), Trap> {
    if br == 0.0 && bi == 0.0 {
        return Ok((1.0, 0.0));
    }
    if ar == 0.0 && ai == 0.0 {
        if bi != 0.0 || br < 0.0 {
            return Err(Trap::ZeroDivisionError);
        }
        return Ok((0.0, 0.0));
    }
    let vabs = libm::hypot(ar, ai);
    let mut len = libm::pow(vabs, br);
    let angle = libm::atan2(ai, ar);
    let mut phase = angle * br;
    if bi != 0.0 {
        len /= libm::exp(angle * bi);
        phase += bi * libm::log(vabs);
    }
    Ok((len * libm::cos(phase), len * libm::sin(phase)))
}

/// Python's float floor division `x // y` (the quotient of CPython's `float_divmod`): the exact
/// floor toward negative infinity, computed through `fmod` so the result matches CPython bit for
/// bit (naively flooring `x / y` can round the wrong way near an integer). The precondition is
/// `y != 0`.
fn float_floordiv(x: f64, y: f64) -> f64 {
    let modulus = libm::fmod(x, y);
    let mut div = (x - modulus) / y;
    if modulus != 0.0 && (y < 0.0) != (modulus < 0.0) {
        div -= 1.0;
    }
    if div != 0.0 {
        let floor = libm::floor(div);
        if div - floor > 0.5 { floor + 1.0 } else { floor }
    } else {
        libm::copysign(0.0, x / y)
    }
}

/// Python's float modulo `x % y` (the remainder of CPython's `float_divmod`): `fmod` adjusted so
/// the result takes the DIVISOR's sign (`-7.5 % 2 == 0.5`), and a zero remainder carries the
/// divisor's sign (`-1.0 % 1.0 == 0.0`). The precondition is `y != 0`.
fn float_mod(x: f64, y: f64) -> f64 {
    let mut modulus = libm::fmod(x, y);
    if modulus != 0.0 {
        if (y < 0.0) != (modulus < 0.0) {
            modulus += y;
        }
    } else {
        modulus = libm::copysign(0.0, y);
    }
    modulus
}

/// The (method, reflected-method) dunder names for a binary operator (`__add__`/`__radd__`, ...).
fn binop_dunder_names(op: BinOp) -> (&'static str, &'static str) {
    match op {
        BinOp::Add => ("__add__", "__radd__"),
        BinOp::Sub => ("__sub__", "__rsub__"),
        BinOp::Mul => ("__mul__", "__rmul__"),
        BinOp::Mod => ("__mod__", "__rmod__"),
        BinOp::FloorDiv => ("__floordiv__", "__rfloordiv__"),
        BinOp::BitAnd => ("__and__", "__rand__"),
        BinOp::BitOr => ("__or__", "__ror__"),
        BinOp::BitXor => ("__xor__", "__rxor__"),
        BinOp::LShift => ("__lshift__", "__rlshift__"),
        BinOp::RShift => ("__rshift__", "__rrshift__"),
        BinOp::TrueDiv => ("__truediv__", "__rtruediv__"),
        BinOp::Pow => ("__pow__", "__rpow__"),
        BinOp::MatMul => ("__matmul__", "__rmatmul__"),
    }
}

/// Tries a binary operator's dunder protocol on user-class operands: `lhs.__op__(rhs)`, else the
/// reflected `rhs.__rop__(lhs)`. `None` if neither operand's class defines the relevant method (the
/// str/seq and numeric paths then apply). NotImplemented is not modeled -- a defined method is used.
fn try_binop_dunder(
    op: BinOp,
    lhs: Value,
    rhs: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    let (name, reflected) = binop_dunder_names(op);
    if let Some(method) = model.find_dunder(lhs, name) {
        return Ok(Some(call_value(method, &[rhs], functions, model, depth + 1)?));
    }
    if let Some(method) = model.find_dunder(rhs, reflected) {
        return Ok(Some(call_value(method, &[lhs], functions, model, depth + 1)?));
    }
    Ok(None)
}

/// The dunder method name for a unary operator (`__neg__`/`__pos__`/`__invert__`).
fn unary_dunder_name(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "__neg__",
        UnaryOp::Pos => "__pos__",
        UnaryOp::Invert => "__invert__",
    }
}

/// The dunder method name for a comparison operator.
fn compare_dunder_name(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "__eq__",
        CmpOp::Ne => "__ne__",
        CmpOp::Lt => "__lt__",
        CmpOp::Le => "__le__",
        CmpOp::Gt => "__gt__",
        CmpOp::Ge => "__ge__",
        CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not have no comparison dunder"),
    }
}

/// The comparison to try on the RIGHT operand when the left lacks its own method: reflection swaps
/// `<`/`>` and `<=`/`>=`; `==`/`!=` are symmetric.
fn reflected_compare(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not are handled without reflection"),
    }
}

/// Calls `receiver.<op-method>(other)` if `receiver`'s class defines it (or, for `!=` with no
/// `__ne__`, derives it by negating `__eq__`), returning the boolean result.
fn compare_dunder_call(
    op: CmpOp,
    receiver: Value,
    other: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if let Some(method) = model.find_dunder(receiver, compare_dunder_name(op)) {
        let outcome = call_value(method, &[other], functions, model, depth + 1)?;
        return Ok(Some(Value::from_bool(model.py_truthy(outcome)?.unwrap_or(false))));
    }
    if matches!(op, CmpOp::Ne) {
        if let Some(method) = model.find_dunder(receiver, "__eq__") {
            let outcome = call_value(method, &[other], functions, model, depth + 1)?;
            return Ok(Some(Value::from_bool(!model.py_truthy(outcome)?.unwrap_or(false))));
        }
    }
    Ok(None)
}

/// Tries a comparison's dunder protocol on user-class operands: `lhs.__op__(rhs)`, else the
/// reflected `rhs.<reflected op>(lhs)`. `None` if neither class defines the relevant method. Shared
/// with the sorting/min/max builtins so `sorted(objects)` honors `__lt__`.
pub(crate) fn try_compare_dunder(
    op: CmpOp,
    lhs: Value,
    rhs: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if let Some(value) = compare_dunder_call(op, lhs, rhs, functions, model, depth)? {
        return Ok(Some(value));
    }
    compare_dunder_call(reflected_compare(op), rhs, lhs, functions, model, depth)
}

/// Truthiness honoring a user class's `__bool__` (then `__len__ != 0`), else the built-in tag-based
/// truthiness. The interpreter-aware form for `if`/`while`/`bool()` on instances.
pub(crate) fn py_truthy_dyn(
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<bool, Trap> {
    if model.is_instance(value) {
        if let Some(method) = model.find_dunder(value, "__bool__") {
            let result = call_value(method, &[], functions, model, depth + 1)?;
            return Ok(result == Value::TRUE);
        }
        if let Some(method) = model.find_dunder(value, "__len__") {
            let result = call_value(method, &[], functions, model, depth + 1)?;
            return Ok(model.as_i128(result).unwrap_or(0) != 0);
        }
    }
    Ok(model.py_truthy(value)?.unwrap_or_else(|| value.is_truthy()))
}

/// Interp-aware element equality for set membership and dedup: when either operand is a user
/// instance, its `__eq__` (with the reflected fallback) decides; otherwise the model's value
/// equality (`key_eq`). This is to sets what the interp-aware repr is to `print` -- the model's
/// `key_eq` is identity for a user object (it cannot call a dunder), so the equality a set uses to
/// dedup and test membership is threaded through the interpreter here.
pub(crate) fn elem_eq(
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<bool, Trap> {
    if model.is_instance(a) || model.is_instance(b) {
        if let Some(result) = try_compare_dunder(CmpOp::Eq, a, b, functions, model, depth)? {
            return Ok(result == Value::TRUE);
        }
    }
    Ok(model.key_eq(a, b))
}

/// Whether `elements` (a detached snapshot -- NOT a borrow of model state, since [`elem_eq`] may
/// re-enter the interpreter) holds a value equal to `needle`.
pub(crate) fn elems_contain(
    needle: Value,
    elements: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<bool, Trap> {
    model.require_hashable(needle)?;
    for &e in elements {
        if elem_eq(needle, e, functions, model, depth)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Dedups `elements` interp-aware, first-seen order -- a set literal / comprehension / `set(iter)`.
pub(crate) fn dedup_elems(
    elements: Vec<Value>,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Vec<Value>, Trap> {
    let mut out: Vec<Value> = Vec::new();
    for e in elements {
        if !elems_contain(e, &out, functions, model, depth)? {
            out.push(e);
        }
    }
    Ok(out)
}

/// The union of set snapshots `a` and `b` (`a`'s elements, then `b`'s new ones), interp-aware.
pub(crate) fn union_elems_dyn(
    a: &[Value],
    b: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Vec<Value>, Trap> {
    let mut result = a.to_vec();
    for &e in b {
        if !elems_contain(e, &result, functions, model, depth)? {
            result.push(e);
        }
    }
    Ok(result)
}

/// The elements of set snapshot `a` that are (intersection, `keep_common == true`) / are not
/// (difference) also in `b`, interp-aware.
pub(crate) fn filter_elems_dyn(
    a: &[Value],
    b: &[Value],
    keep_common: bool,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Vec<Value>, Trap> {
    let mut result = Vec::new();
    for &x in a {
        if elems_contain(x, b, functions, model, depth)? == keep_common {
            result.push(x);
        }
    }
    Ok(result)
}

/// Whether every element of set snapshot `a` is in `b` (`a` is a subset of `b`), interp-aware.
pub(crate) fn subset_dyn(
    a: &[Value],
    b: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<bool, Trap> {
    for &x in a {
        if !elems_contain(x, b, functions, model, depth)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether set snapshots `a` and `b` share no element (they are disjoint), interp-aware.
pub(crate) fn disjoint_dyn(
    a: &[Value],
    b: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<bool, Trap> {
    for &x in a {
        if elems_contain(x, b, functions, model, depth)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The interp-aware `set <op> set` for `| & - ^`, or `None` if this is not a two-set operation (so
/// the model's binary dispatch raises the type error). Honors the elements' `__eq__`.
fn try_set_binop_dyn(
    op: BinOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    let both_sets = (model.is_set(a) || model.is_frozenset(a))
        && (model.is_set(b) || model.is_frozenset(b));
    if !both_sets || !matches!(op, BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor) {
        return Ok(None);
    }
    let a_elems = model.set_value(a).ok_or(Trap::TypeError)?.clone();
    let b_elems = model.set_value(b).ok_or(Trap::TypeError)?.clone();
    let result = match op {
        BinOp::BitOr => union_elems_dyn(&a_elems, &b_elems, functions, model, depth)?,
        BinOp::BitAnd => filter_elems_dyn(&a_elems, &b_elems, true, functions, model, depth)?,
        BinOp::Sub => filter_elems_dyn(&a_elems, &b_elems, false, functions, model, depth)?,
        BinOp::BitXor => {
            let mut r = filter_elems_dyn(&a_elems, &b_elems, false, functions, model, depth)?;
            r.extend(filter_elems_dyn(&b_elems, &a_elems, false, functions, model, depth)?);
            r
        }
        _ => unreachable!("guarded by the matches! above"),
    };
    let set = if model.is_frozenset(a) {
        model.new_frozenset(result)?
    } else {
        model.new_set(result)?
    };
    Ok(Some(set))
}

/// The interp-aware set comparison (the left operand is a set/frozenset), or `None` if the left is
/// not a set (so the model handles it). `==`/`!=` test set equality (a set never equals a non-set);
/// `< <= > >=` are (proper) subset/superset and require the right to be a set (else `TypeError`).
/// Membership throughout is by the elements' `__eq__`, so a set of user objects compares correctly.
fn try_set_compare_dyn(
    op: CmpOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if !(model.is_set(a) || model.is_frozenset(a)) {
        return Ok(None);
    }
    let b_is_set = model.is_set(b) || model.is_frozenset(b);
    let value = match op {
        CmpOp::Eq | CmpOp::Ne => {
            let equal = if b_is_set {
                let a_elems = model.set_value(a).ok_or(Trap::TypeError)?.clone();
                let b_elems = model.set_value(b).ok_or(Trap::TypeError)?.clone();
                a_elems.len() == b_elems.len()
                    && subset_dyn(&a_elems, &b_elems, functions, model, depth)?
            } else {
                false
            };
            if matches!(op, CmpOp::Ne) { !equal } else { equal }
        }
        CmpOp::Le | CmpOp::Ge | CmpOp::Lt | CmpOp::Gt => {
            if !b_is_set {
                return Err(Trap::TypeError);
            }
            let a_elems = model.set_value(a).ok_or(Trap::TypeError)?.clone();
            let b_elems = model.set_value(b).ok_or(Trap::TypeError)?.clone();
            match op {
                CmpOp::Le => subset_dyn(&a_elems, &b_elems, functions, model, depth)?,
                CmpOp::Ge => subset_dyn(&b_elems, &a_elems, functions, model, depth)?,
                CmpOp::Lt => {
                    a_elems.len() < b_elems.len()
                        && subset_dyn(&a_elems, &b_elems, functions, model, depth)?
                }
                CmpOp::Gt => {
                    b_elems.len() < a_elems.len()
                        && subset_dyn(&b_elems, &a_elems, functions, model, depth)?
                }
                _ => unreachable!("guarded by the outer match arm"),
            }
        }
        CmpOp::Is | CmpOp::IsNot => return Ok(None),
    };
    Ok(Some(Value::from_bool(value)))
}

/// Dedups `pairs` into dict entries interp-aware -- first-seen key position, last value winning (a
/// dict literal `{...}`, `BuildDict`). A key with a user `__eq__` collapses with an equal one. This
/// is to dicts what [`dedup_elems`] is to sets.
pub(crate) fn dedup_pairs(
    pairs: Vec<(Value, Value)>,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Vec<(Value, Value)>, Trap> {
    let mut out: Vec<(Value, Value)> = Vec::new();
    for (key, value) in pairs {
        model.require_hashable(key)?;
        let mut found = None;
        for (idx, (k, _)) in out.iter().enumerate() {
            if elem_eq(key, *k, functions, model, depth)? {
                found = Some(idx);
                break;
            }
        }
        match found {
            Some(idx) => out[idx].1 = value,
            None => out.push((key, value)),
        }
    }
    Ok(out)
}

/// The interp-aware `dict == dict` / `dict != dict`, or `None` when this is not two dicts, not an
/// equality op (dict ordering is a `TypeError`, left to the model), or neither dict holds a user
/// instance (the model's fast [`ObjectModel::dict_equal`] is then exact). Honors a user `__eq__` on
/// the dicts' keys and values.
fn try_dict_compare_dyn(
    op: CmpOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if !(model.is_dict(a) && model.is_dict(b)) {
        return Ok(None);
    }
    if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
        return Ok(None);
    }
    if !model.dict_has_instance(a) && !model.dict_has_instance(b) {
        return Ok(None);
    }
    let equal = model.dict_equal_dyn(a, b, functions, depth)?;
    Ok(Some(Value::from_bool(if matches!(op, CmpOp::Ne) { !equal } else { equal })))
}

/// The interp-aware `dict | dict` merge (PEP 584; the right dict wins a key conflict, the key keeps
/// its first position), or `None` when this is not a `|` of two dicts, or neither dict holds a user
/// instance (the model's fast [`ObjectModel::py_binary`] merge is then exact). Collapsing equal keys
/// by their `__eq__` keeps `|` consistent with `dict.update`.
fn try_dict_binop_dyn(
    op: BinOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if op != BinOp::BitOr || !(model.is_dict(a) && model.is_dict(b)) {
        return Ok(None);
    }
    if !model.dict_has_instance(a) && !model.dict_has_instance(b) {
        return Ok(None);
    }
    let mut pairs = model.dict_entries(a).unwrap_or_default();
    pairs.extend(model.dict_entries(b).unwrap_or_default());
    Ok(Some(model.new_dict_dyn(pairs, functions, depth)?))
}

/// Evaluates a unary `-`/`+`/`~` over an `int`/`bool` operand (Python int semantics:
/// `+x == x`, `-x`, `~x == -x - 1`); other types are a `TypeError`. The customizable
/// `__neg__`/`__pos__`/`__invert__` protocol composes with the broader object model.
fn unary(op: UnaryOp, v: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
    #[cfg(feature = "complex")]
    if let Some((re, im)) = model.complex_value(v) {
        return match op {
            UnaryOp::Neg => model.new_complex(-re, -im),
            UnaryOp::Pos => model.new_complex(re, im),
            UnaryOp::Invert => Err(Trap::TypeError),
        };
    }
    if let Some(f) = model.float_value(v) {
        let result = match op {
            UnaryOp::Neg => -f,
            UnaryOp::Pos => f,
            UnaryOp::Invert => return Err(Trap::TypeError),
        };
        return model.new_float(result);
    }
    if let Some(big) = model.bigint_value(v).cloned() {
        let result = match op {
            UnaryOp::Neg => big.neg(),
            UnaryOp::Pos => big,
            UnaryOp::Invert => big.neg().sub(&BigInt::from_i128(1)),
        };
        return model.new_bigint(result);
    }
    let x = model.as_i128(v).ok_or(Trap::TypeError)?;
    let result: i128 = match op {
        UnaryOp::Neg => x.checked_neg().ok_or(Trap::Overflow)?,
        UnaryOp::Pos => x,
        UnaryOp::Invert => !x,
    };
    model.new_long(result)
}

/// Evaluates a comparison (Python 3.14.6 Language Reference, "Comparisons", 6.10).
///
/// `int`/`bool` operands compare numerically (numbers compare mathematically correct).
/// For any other operands the default applies: `==`/`!=` are based on object identity
/// (so `None == None` is true, and two distinct objects are unequal), and the ordering
/// operators `<`/`<=`/`>`/`>=` have no default and raise `TypeError`. The customizable
/// `__eq__`/`__lt__`/... protocol (the `py_compare` intrinsic) composes with the broader
/// object model.
fn compare(op: CmpOp, a: Value, b: Value, model: &ObjectModel) -> Result<Value, Trap> {
    #[cfg(feature = "complex")]
    if model.is_complex(a) || model.is_complex(b) {
        let equal = match (model.as_complex(a), model.as_complex(b)) {
            (Some(lhs), Some(rhs)) => lhs == rhs,
            _ => false,
        };
        return match op {
            CmpOp::Eq => Ok(Value::from_bool(equal)),
            CmpOp::Ne => Ok(Value::from_bool(!equal)),
            _ => Err(Trap::TypeError),
        };
    }
    if let (Some(x), Some(y)) = (model.as_i128(a), model.as_i128(b)) {
        let result = match op {
            CmpOp::Lt => x < y,
            CmpOp::Le => x <= y,
            CmpOp::Eq => x == y,
            CmpOp::Ne => x != y,
            CmpOp::Gt => x > y,
            CmpOp::Ge => x >= y,
            CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not handled in the Op::Compare path"),
        };
        Ok(Value::from_bool(result))
    } else if model.is_int(a) && model.is_int(b) {
        use core::cmp::Ordering;
        let ord = model.as_bigint(a).unwrap_or_default().cmp(&model.as_bigint(b).unwrap_or_default());
        let result = match op {
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
            CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not handled in the Op::Compare path"),
        };
        Ok(Value::from_bool(result))
    } else if (model.is_float(a) || model.is_float(b)) && model.as_f64(a).is_some() && model.as_f64(b).is_some() {
        let x = model.as_f64(a).unwrap_or(f64::NAN);
        let y = model.as_f64(b).unwrap_or(f64::NAN);
        let result = match op {
            CmpOp::Lt => x < y,
            CmpOp::Le => x <= y,
            CmpOp::Eq => x == y,
            CmpOp::Ne => x != y,
            CmpOp::Gt => x > y,
            CmpOp::Ge => x >= y,
            CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not handled in the Op::Compare path"),
        };
        Ok(Value::from_bool(result))
    } else {
        match op {
            CmpOp::Eq => Ok(Value::from_bool(a == b)),
            CmpOp::Ne => Ok(Value::from_bool(a != b)),
            _ => Err(Trap::TypeError),
        }
    }
}

/// The maximum nesting of intra-module calls before the interpreter reports
/// `RecursionError` -- a guard so a runaway recursion is bounded rather than overflowing
/// the native stack.
const MAX_CALL_DEPTH: usize = 256;

/// Runs `code` (one function of `functions`) with `args`, returning the value it returns.
///
/// `functions` is the module's function table: `LoadGlobal` resolves a name to one of
/// them and `Call` invokes it (a program with no calls passes an empty slice). `args`
/// must match `code`'s parameter count. `model` resolves attribute access (and owns the
/// heap any objects live on); code that never touches an object leaves it unused, so the
/// caller may pass an empty model.
pub fn run(
    code: &CodeObject,
    functions: &[CodeObject],
    args: &[Value],
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    run_frames(code, functions, args, &[], model, false, 0)
}

/// Runs the module body (the top-level statements). Its local bindings mirror into the module
/// globals as they happen, so a function reaches a top-level name (a class, a global) by
/// `LoadGlobal`. Run this before invoking module functions.
pub fn run_module(
    body: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    run_frames(body, functions, &[], &[], model, true, 0)
}

/// One op's control-flow outcome, returned from the per-op block to the [`run_frames`]
/// driver: fall through to the next op ([`Flow::Next`]), return `value` from the current
/// function ([`Flow::Return`]), or invoke a direct Python function ([`Flow::Call`]). A jump
/// just mutates the frame's `ip` and falls through with `Next`.
enum Flow {
    Next,
    Return(Value),
    /// Call module function `index` with `args` already bound to its parameters. The driver
    /// pushes a new [`Frame`] onto the explicit frame stack, so a deep Python call chain never
    /// grows the native stack. Only a DIRECT call of a plain or defaulted Python function
    /// reaches here; builtins, bound methods, class init, and dunders stay on [`call_value`]
    /// (bounded native recursion). `cells` are the captured cells if the callee is a closure
    /// (empty otherwise); the driver seeds the freevar half of the new frame's deref array with them.
    Call { index: u32, args: Vec<Value>, cells: Vec<Value> },
    /// The current (generator) frame yielded `value`: [`drive`] stops and returns it, leaving the
    /// yielding frame on top of the stack for the resumer to re-suspend. Reached only during a
    /// generator resume -- a generator function's body runs nowhere else.
    Yield(Value),
}

/// The outcome of running [`drive`] over a frame stack: the bottom frame returned `value`, or a
/// generator frame yielded `value` (and was left on top of the stack, to be re-suspended).
enum DriveOutcome {
    Returned(Value),
    Yielded(Value),
}

/// Which code a [`Frame`] runs, held as an index rather than a borrow so a suspended frame (a
/// generator, step 4) can outlive the call that created it, and so the code can live in flash
/// (XIP) independent of the frame's RAM. Resolved against the driver's `entry` + `functions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeId {
    /// The top-level code passed to [`run`] / [`run_module`] (or the callee of a nested
    /// [`call_value`] driver): the entry frame's code, which is not in the `functions` table.
    Entry,
    /// Module function `functions[index]` -- every pushed frame, and every generator.
    Func(u32),
}

/// The innermost `exc_table` entry whose protected range covers op `ip`, or `None`. Entries
/// are emitted innermost-first, so the first cover is the tightest handler.
fn find_handler(exc_table: &[ExcEntry], ip: u32) -> Option<ExcEntry> {
    exc_table.iter().copied().find(|e| e.start <= ip && ip < e.end)
}

/// Invokes module function `index` with `args` already bound to its parameters: runs it to
/// completion through a nested driver, OR -- if it is a generator function -- returns a fresh
/// generator object (its body runs only when resumed). The shared tail of the [`call_value`]
/// Python-callee branches (the callback path); the DIRECT `Op::Call` path instead pushes a frame
/// onto the explicit stack.
/// Resolves `name` in the module GLOBAL namespace: a defaulted function's PyFunction (which shadows
/// its bare function-table ref, since the ref carries no defaults), else an intra-module function,
/// else a module-level global (a class or other top-level binding), else a built-in, else a built-in
/// exception class (so `except IndexError` / `raise ValueError` find the type). `None` if unbound (a
/// `NameError` at the use site). Shared by `LoadGlobal` and the outer fallback of `LoadName`.
fn resolve_global(name: &str, functions: &[CodeObject], model: &mut ObjectModel) -> Option<Value> {
    if let Some(pyfunc) = model.get_global(name).filter(|&g| model.is_py_function(g)) {
        Some(pyfunc)
    } else if let Some(index) = functions.iter().position(|f| f.name == *name) {
        Some(Value::function_ref(index as u32))
    } else if let Some(global) = model.get_global(name) {
        Some(global)
    } else if let Some(id) = crate::builtins::builtin_id(name) {
        Some(Value::builtin_ref(id))
    } else if name == "Ellipsis" {
        Some(Value::ELLIPSIS)
    } else {
        model.exception_class(name)
    }
}

fn invoke_function(
    index: u32,
    args: &[Value],
    cells: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
    if code.is_generator {
        let generator = new_frame(code, CodeId::Func(index), args, false, cells, model)?;
        model.new_generator(generator)
    } else {
        run_frames(code, functions, args, cells, model, false, depth)
    }
}

/// Dispatches a call of `callee` with `args` -- the unified callable protocol shared by the
/// `Call` op, builtins that invoke dunders, and dunder dispatch. `depth` is the callee's call
/// depth. Handles module functions, builtins, bound str/Python methods, and instantiating a class.
pub(crate) fn call_value(
    callee: Value,
    args: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if let Some(index) = callee.as_function_index() {
        invoke_function(index, args, &[], functions, model, depth)
    } else if model.is_py_function(callee) {
        let index = model.py_function_index(callee);
        let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
        let defaults = model.py_function_defaults(callee);
        let kwdefaults = model.py_function_kwdefaults(callee);
        let bound = bind_arguments(code, args, &[], &defaults, kwdefaults, model)?;
        let cells = model.py_function_cells(callee);
        invoke_function(index, &bound, &cells, functions, model, depth)
    } else if let Some(id) = callee.as_builtin_id() {
        crate::builtins::call_builtin(id, args, functions, model, depth)
    } else if model.is_str_join(callee) {
        let [iterable] = args else {
            return Err(Trap::TypeError);
        };
        let items = crate::builtins::collect_iterable(model, &[*iterable], functions, depth)?;
        let list = model.new_list(items)?;
        model.call_bound_method(callee, &[list])
    } else if model.is_bound_method(callee) {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        if model.is_generator(receiver) {
            call_generator_method(receiver, method_id, args, functions, model, depth)
        } else if model.is_set(receiver) || model.is_frozenset(receiver) {
            model.call_set_method_dyn(receiver, method_id, args, functions, depth)
        } else if model.is_dict(receiver) {
            model.call_dict_method_dyn(receiver, method_id, args, functions, depth)
        } else {
            model.call_bound_method(callee, args)
        }
    } else if model.is_py_bound(callee) {
        let func = model.bound_func(callee);
        let (index, defaults, kwdefaults, cells) = if let Some(index) = func.as_function_index() {
            (index, Vec::new(), Value::NONE, Vec::new())
        } else if model.is_py_function(func) {
            (
                model.py_function_index(func),
                model.py_function_defaults(func),
                model.py_function_kwdefaults(func),
                model.py_function_cells(func),
            )
        } else {
            return Err(Trap::TypeError);
        };
        let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
        let mut method_args = Vec::with_capacity(args.len() + 1);
        method_args.push(model.bound_self(callee));
        method_args.extend_from_slice(args);
        let bound = bind_arguments(code, &method_args, &[], &defaults, kwdefaults, model)?;
        invoke_function(index, &bound, &cells, functions, model, depth)
    } else if model.is_unbound_method(callee) {
        let (receiver, rest) = args.split_first().ok_or(Trap::TypeError)?;
        let name_value = model.unbound_method_name(callee);
        let name = model.str_value(name_value).ok_or(Trap::TypeError)?.to_string();
        let bound = model.getattr(*receiver, &name, &mut crate::object::InlineCache::empty())?;
        call_value(bound, rest, functions, model, depth)
    } else if model.is_class(callee) {
        let instance = model.new_object(callee)?;
        if let Some(init) = model.find_init(callee) {
            let mut init_args = Vec::with_capacity(args.len() + 1);
            init_args.push(instance);
            init_args.extend_from_slice(args);
            call_value(init, &init_args, functions, model, depth)?;
        } else if !args.is_empty() {
            model.init_default_args(instance, args)?;
        }
        Ok(instance)
    } else if model.is_pin_factory(callee) {
        model.call_pin_factory(args)
    } else if model.is_dio_factory(callee) {
        model.call_dio_factory(args)
    } else if let Some(call_method) = model.find_dunder(callee, "__call__") {
        call_value(call_method, args, functions, model, depth + 1)
    } else {
        Err(Trap::TypeError)
    }
}

/// Dispatches a call carrying KEYWORD arguments (`Op::CallKw`). Like [`call_value`] but binds
/// `posargs` + `kwargs` (+ any defaults) to the callee's parameters via [`bind_arguments`]. Handles
/// Python functions (plain + defaulted), bound methods, and class instantiation. A keyword call to a
/// BUILT-IN is not supported (built-ins take positional arguments via `Op::Call`).
fn call_value_kw(
    callee: Value,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if let Some(index) = callee.as_function_index() {
        let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
        let bound = bind_arguments(code, posargs, kwargs, &[], Value::NONE, model)?;
        invoke_function(index, &bound, &[], functions, model, depth)
    } else if model.is_py_function(callee) {
        let index = model.py_function_index(callee);
        let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
        let defaults = model.py_function_defaults(callee);
        let kwdefaults = model.py_function_kwdefaults(callee);
        let bound = bind_arguments(code, posargs, kwargs, &defaults, kwdefaults, model)?;
        let cells = model.py_function_cells(callee);
        invoke_function(index, &bound, &cells, functions, model, depth)
    } else if model.is_py_bound(callee) {
        let func = model.bound_func(callee);
        let mut all_pos = Vec::with_capacity(posargs.len() + 1);
        all_pos.push(model.bound_self(callee));
        all_pos.extend_from_slice(posargs);
        call_value_kw(func, &all_pos, kwargs, functions, model, depth)
    } else if model.is_class(callee) {
        let instance = model.new_object(callee)?;
        if let Some(init) = model.find_init(callee) {
            let mut all_pos = Vec::with_capacity(posargs.len() + 1);
            all_pos.push(instance);
            all_pos.extend_from_slice(posargs);
            call_value_kw(init, &all_pos, kwargs, functions, model, depth)?;
        } else if !posargs.is_empty() || !kwargs.is_empty() {
            return Err(Trap::TypeError);
        }
        Ok(instance)
    } else if model.is_list_sort_bound(callee) {
        let receiver = model.bound_receiver(callee);
        list_sort_kw(posargs, kwargs, receiver, functions, model, depth)
    } else if model.is_str_format_bound(callee) {
        let receiver = model.bound_receiver(callee);
        let template = model.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
        let rendered = model.format_template(&template, posargs, kwargs)?;
        model.new_str(&rendered)
    } else if model.is_dict_update_bound(callee) {
        let receiver = model.bound_receiver(callee);
        match posargs {
            [] => {}
            [other] => {
                model.call_bound_method(callee, &[*other])?;
            }
            _ => return Err(Trap::TypeError),
        }
        for &(name, value) in kwargs {
            let key = model.new_str(name)?;
            model.py_setitem(receiver, key, value)?;
        }
        Ok(Value::NONE)
    } else if model.is_bound_method(callee) {
        Err(Trap::TypeError)
    } else if let Some(id) = callee.as_builtin_id() {
        crate::builtins::call_builtin_kw(id, posargs, kwargs, functions, model, depth)
    } else {
        Err(Trap::TypeError)
    }
}

/// `list.sort(*, key=None, reverse=False)`: sort `receiver` in place. With `key`, each element's
/// sort key is `key(element)`; `reverse` sorts descending (ties keep their original order). Returns
/// `None`. The only built-in method with keyword arguments (its `CallKw` routes here).
fn list_sort_kw(
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    receiver: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if !posargs.is_empty() {
        return Err(Trap::TypeError);
    }
    let mut key = Value::NONE;
    let mut reverse = false;
    for &(name, value) in kwargs {
        match name {
            "key" => key = value,
            "reverse" => reverse = model.py_truthy(value)?.unwrap_or(true),
            _ => return Err(Trap::TypeError),
        }
    }
    let keys = if key.is_none() {
        None
    } else {
        let elements = model.seq_elements(receiver).ok_or(Trap::TypeError)?;
        let mut computed = Vec::with_capacity(elements.len());
        for element in elements {
            computed.push(call_value(key, &[element], functions, model, depth)?);
        }
        Some(computed)
    };
    model.list_sort_in_place(receiver, keys, reverse)
}

/// Binds positional + keyword arguments (+ positional defaults) to a function's parameters,
/// producing exactly `code.params.len()` locals -- CPython's call binding (Language Reference
/// 6.3.4). `defaults` fill the TRAILING positional parameters; a keyword binds by name to a
/// `pos_or_keyword` parameter. Pure over `Value`s + the code object (the caller pre-extracts the
/// defaults tuple). The slot layout is `[regular.., *args?, keyword-only.., **kwargs?]`.
fn bind_arguments(
    code: &CodeObject,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    defaults: &[Value],
    kwdefaults: Value,
    model: &mut ObjectModel,
) -> Result<Vec<Value>, Trap> {
    let nparams = code.params.len();
    let posonly = code.posonly_count as usize;
    let kwonly = code.kwonly_count as usize;
    let varkwargs_idx = code.has_varkwargs.then(|| nparams - 1);
    let n_regular = nparams - code.has_varargs as usize - kwonly - code.has_varkwargs as usize;
    let varargs_idx = code.has_varargs.then_some(n_regular);

    let mut slots: Vec<Option<Value>> = alloc::vec![None; nparams];

    if posargs.len() > n_regular {
        let Some(va) = varargs_idx else {
            return Err(Trap::TypeError);
        };
        for (i, &value) in posargs.iter().take(n_regular).enumerate() {
            slots[i] = Some(value);
        }
        slots[va] = Some(model.new_tuple(posargs[n_regular..].to_vec())?);
    } else {
        for (i, &value) in posargs.iter().enumerate() {
            slots[i] = Some(value);
        }
        if let Some(va) = varargs_idx {
            slots[va] = Some(model.new_tuple(Vec::new())?);
        }
    }

    let mut extra: Vec<(Value, Value)> = Vec::new();
    for &(name, value) in kwargs {
        let target = code
            .params
            .iter()
            .position(|p| p.name == name)
            .filter(|&idx| idx >= posonly && Some(idx) != varargs_idx && Some(idx) != varkwargs_idx);
        match target {
            Some(idx) => {
                if slots[idx].is_some() {
                    return Err(Trap::TypeError);
                }
                slots[idx] = Some(value);
            }
            None if code.has_varkwargs => extra.push((model.new_str(name)?, value)),
            None => return Err(Trap::TypeError),
        }
    }
    if let Some(vk) = varkwargs_idx {
        slots[vk] = Some(model.new_dict(extra)?);
    }

    if kwonly > 0 && !kwdefaults.is_none() {
        let kwonly_start = n_regular + code.has_varargs as usize;
        let kwonly_range = kwonly_start..kwonly_start + kwonly;
        let params = &code.params[kwonly_range.clone()];
        for (slot, param) in slots[kwonly_range].iter_mut().zip(params) {
            if slot.is_none() {
                if let Some(default) = model.dict_get_str(kwdefaults, &param.name) {
                    *slot = Some(default);
                }
            }
        }
    }

    let first_default = n_regular.saturating_sub(defaults.len());
    for (j, &default) in defaults.iter().enumerate() {
        if slots[first_default + j].is_none() {
            slots[first_default + j] = Some(default);
        }
    }

    let mut locals = Vec::with_capacity(nparams);
    for slot in slots {
        locals.push(slot.ok_or(Trap::TypeError)?);
    }
    Ok(locals)
}

/// Resumes generator `gen` by one step: runs its suspended frame through [`drive`] until the next
/// `yield` (returns the yielded value) or the body returns / falls through (returns [`Value::STOP`],
/// the exhaustion sentinel a `for` loop reads as StopIteration). `next()` sends `None`, pushed onto
/// the frame's eval stack as the `yield` expression's result -- except on the FIRST resume, where
/// the body has not reached a `yield` yet (a fresh frame, ip 0). The frame is taken OUT of the
/// generator while running, so a re-entrant resume (or an exhausted generator) yields STOP.
/// How a suspended generator is resumed: a value sent into the `yield` (`Send(NONE)` == `next()`),
/// an exception thrown in at the suspension point, or a close (throw `GeneratorExit`, expecting the
/// body to finish).
enum Resume {
    Send(Value),
    Throw(Value),
    Close,
}

pub(crate) const GEN_SEND: u32 = 0;
pub(crate) const GEN_THROW: u32 = 1;
pub(crate) const GEN_CLOSE: u32 = 2;
pub(crate) const GEN_NEXT: u32 = 3;

/// The method id for a generator method `name` (`gen.send`/`.throw`/`.close`/`.__next__`), or
/// `None`. Getattr binds these to the generator; [`call_generator_method`] dispatches them.
pub(crate) fn generator_method_id(name: &str) -> Option<u32> {
    match name {
        "send" => Some(GEN_SEND),
        "throw" => Some(GEN_THROW),
        "close" => Some(GEN_CLOSE),
        "__next__" => Some(GEN_NEXT),
        _ => None,
    }
}

fn resume_generator(
    generator: Value,
    resume: Resume,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if depth > MAX_CALL_DEPTH {
        return Err(Trap::RecursionError);
    }
    let close_mode = matches!(resume, Resume::Close);
    let mut frame = match model.take_generator_frame(generator) {
        Some(frame) => frame,
        None => {
            return match resume {
                Resume::Send(_) => Ok(Value::STOP),
                Resume::Throw(exc) => {
                    model.set_pending_exception(exc);
                    Err(Trap::Raised)
                }
                Resume::Close => Ok(Value::NONE),
            };
        }
    };
    let entry = match frame.code {
        CodeId::Func(index) => functions.get(index as usize).ok_or(Trap::Malformed)?,
        CodeId::Entry => return Err(Trap::Malformed),
    };
    match resume {
        Resume::Send(value) => {
            if frame.ip == 0 && value != Value::NONE {
                model.put_generator_frame(generator, frame);
                return Err(Trap::TypeError);
            }
            if frame.ip > 0 {
                frame.stack.push(value);
            }
        }
        Resume::Throw(exc) => match find_handler(&entry.exc_table, frame.ip as u32) {
            Some(handler) => {
                frame.stack.truncate(handler.depth as usize);
                frame.active_exception = Some(exc);
                frame.ip = handler.target as usize;
            }
            None => {
                model.set_pending_exception(exc);
                return Err(Trap::Raised);
            }
        },
        Resume::Close => {
            let exc = model.new_exception("GeneratorExit")?;
            match find_handler(&entry.exc_table, frame.ip as u32) {
                Some(handler) => {
                    frame.stack.truncate(handler.depth as usize);
                    frame.active_exception = Some(exc);
                    frame.ip = handler.target as usize;
                }
                None => return Ok(Value::NONE),
            }
        }
    }
    let mut frames: Vec<Frame> = Vec::new();
    frames.push(frame);
    match drive(&mut frames, entry, functions, model, depth + 1) {
        Ok(DriveOutcome::Yielded(value)) => {
            let suspended = frames.pop().ok_or(Trap::Malformed)?;
            if close_mode {
                return Err(
                    model.raise_named_exception("RuntimeError", "generator ignored GeneratorExit")
                );
            }
            model.put_generator_frame(generator, suspended);
            Ok(value)
        }
        Ok(DriveOutcome::Returned(_)) => Ok(if close_mode { Value::NONE } else { Value::STOP }),
        Err(Trap::Raised) if close_mode => {
            if model.pending_exception_is("GeneratorExit") {
                model.take_pending_exception();
                Ok(Value::NONE)
            } else {
                Err(Trap::Raised)
            }
        }
        Err(trap) => Err(trap),
    }
}

/// Dispatches a generator method (`gen.send`/`.throw`/`.close`/`.__next__`): a bound method whose
/// receiver is a generator, routed here from [`call_value`] with the driver context. send/throw/next
/// return the next yielded value or raise `StopIteration` on exhaustion; close returns None.
fn call_generator_method(
    generator: Value,
    method_id: u32,
    args: &[Value],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    match method_id {
        GEN_NEXT => {
            let value = resume_generator(generator, Resume::Send(Value::NONE), functions, model, depth)?;
            stop_or_value(value, model)
        }
        GEN_SEND => {
            let sent = match args {
                [value] => *value,
                _ => return Err(Trap::TypeError),
            };
            let value = resume_generator(generator, Resume::Send(sent), functions, model, depth)?;
            stop_or_value(value, model)
        }
        GEN_THROW => {
            let raised = match args {
                [exc] => *exc,
                _ => return Err(Trap::TypeError),
            };
            let exc = model.raise_value(raised)?;
            let value = resume_generator(generator, Resume::Throw(exc), functions, model, depth)?;
            stop_or_value(value, model)
        }
        GEN_CLOSE => {
            if !args.is_empty() {
                return Err(Trap::TypeError);
            }
            resume_generator(generator, Resume::Close, functions, model, depth)
        }
        _ => Err(Trap::AttributeError),
    }
}

/// Maps a resume result to an explicit-method result: a `STOP` sentinel becomes a raised
/// `StopIteration` (unlike the for-loop path, which stops silently); any value passes through.
fn stop_or_value(value: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
    if value.is_stop() {
        Err(model.raise_named_exception("StopIteration", ""))
    } else {
        Ok(value)
    }
}

/// The iterator over `value`: a class instance's `__iter__` (the result iterated if it is not
/// already an iterator), else the built-in iterator. Shared by `GetIter`, `iter()`, and the
/// builtins that collect an iterable.
pub(crate) fn iterator_for(
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if model.is_generator(value) || model.is_lazy_iter(value) {
        return Ok(value);
    }
    if let Some(iter_method) = model.find_dunder(value, "__iter__") {
        let result = call_value(iter_method, &[], functions, model, depth)?;
        if model.is_iter(result)
            || model.is_generator(result)
            || model.find_dunder(result, "__next__").is_some()
        {
            Ok(result)
        } else {
            model.new_iter(result)
        }
    } else {
        model.new_iter(value)
    }
}

/// Advances `iterator` one step: resumes a generator through the driver, or delegates to the
/// object model's `py_next` for a built-in iterator (str/list/dict/range/...). `Some(value)` is the
/// next item, `None` exhaustion. The ONE iteration primitive `ForIter` and every iterable-consuming
/// built-in (`list`/`sum`/`sorted`/... via `collect_iterable`, and `next`) share -- so a generator
/// works everywhere a built-in iterator does, not only in a `for` loop.
pub(crate) fn py_next_value(
    iterator: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if model.is_generator(iterator) {
        let value = resume_generator(iterator, Resume::Send(Value::NONE), functions, model, depth)?;
        Ok(if value.is_stop() { None } else { Some(value) })
    } else if model.is_lazy_iter(iterator) {
        lazy_iter_next(iterator, functions, model, depth)
    } else if let Some(next_method) = model.find_dunder(iterator, "__next__") {
        match call_value(next_method, &[], functions, model, depth) {
            Ok(value) => Ok(Some(value)),
            Err(Trap::Raised) if model.pending_exception_is("StopIteration") => {
                model.take_pending_exception();
                Ok(None)
            }
            Err(other) => Err(other),
        }
    } else {
        model.py_next(iterator)
    }
}

/// Advances a lazy `map`/`filter`/`zip`/`enumerate` one step, pulling from its source iterator(s)
/// and applying its function. `None` when a source is exhausted.
fn lazy_iter_next(
    iterator: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    let sources = model.lazy_iter_sources(iterator);
    match model.lazy_iter_kind(iterator) {
        crate::object::LAZY_MAP => {
            let mut row = Vec::with_capacity(sources.len());
            for source in &sources {
                match py_next_value(*source, functions, model, depth)? {
                    Some(value) => row.push(value),
                    None => return Ok(None),
                }
            }
            let func = model.lazy_iter_state(iterator);
            Ok(Some(call_value(func, &row, functions, model, depth)?))
        }
        crate::object::LAZY_FILTER => {
            let source = sources[0];
            let func = model.lazy_iter_state(iterator);
            loop {
                let Some(value) = py_next_value(source, functions, model, depth)? else {
                    return Ok(None);
                };
                let keep = if func.is_none() {
                    py_truthy_dyn(value, functions, model, depth)?
                } else {
                    let result = call_value(func, &[value], functions, model, depth)?;
                    py_truthy_dyn(result, functions, model, depth)?
                };
                if keep {
                    return Ok(Some(value));
                }
            }
        }
        crate::object::LAZY_ZIP => {
            if sources.is_empty() {
                return Ok(None);
            }
            let mut row = Vec::with_capacity(sources.len());
            for source in &sources {
                match py_next_value(*source, functions, model, depth)? {
                    Some(value) => row.push(value),
                    None => return Ok(None),
                }
            }
            Ok(Some(model.new_tuple(row)?))
        }
        crate::object::LAZY_ENUMERATE => {
            let source = sources[0];
            let Some(value) = py_next_value(source, functions, model, depth)? else {
                return Ok(None);
            };
            let count = model.lazy_iter_state(iterator);
            let pair = model.new_tuple(alloc::vec![count, value])?;
            let next = model.new_long(model.as_i128(count).unwrap_or(0) + 1)?;
            model.lazy_iter_set_state(iterator, next);
            Ok(Some(pair))
        }
        crate::object::LAZY_CALLABLE => {
            let callable = model.lazy_iter_state(iterator);
            let sentinel = sources.first().copied().unwrap_or(Value::NONE);
            let result = call_value(callable, &[], functions, model, depth)?;
            Ok((!model.key_eq(result, sentinel)).then_some(result))
        }
        _ => Err(Trap::TypeError),
    }
}

/// Resolves a [`CodeId`] to its [`CodeObject`] against the driver's entry + function table.
/// Every stored `CodeId` is validated when its frame is created, so the index is in range.
fn resolve_code<'a>(
    id: CodeId,
    entry: &'a CodeObject,
    functions: &'a [CodeObject],
) -> &'a CodeObject {
    match id {
        CodeId::Entry => entry,
        CodeId::Func(index) => &functions[index as usize],
    }
}

/// Builds a fresh frame for `code`, binding `args` to its leading local slots. A wrong argument
/// count is a `TypeError` (CPython call binding), not malformed bytecode. `id` records which
/// code the frame runs, and `is_module` whether it is the module body.
fn new_frame(
    code: &CodeObject,
    id: CodeId,
    args: &[Value],
    is_module: bool,
    captured_cells: &[Value],
    model: &mut ObjectModel,
) -> Result<Frame, Trap> {
    if args.len() != code.params.len() {
        return Err(Trap::TypeError);
    }
    let mut frame = match model.take_pooled_frame() {
        Some(mut pooled) => {
            pooled.locals.clear();
            pooled.locals.resize(code.n_locals, Value::UNBOUND);
            pooled.caches.clear();
            pooled.caches.resize(code.cache_count, InlineCache::empty());
            pooled.stack.clear();
            pooled.active_exception = None;
            pooled.ip = 0;
            pooled
        }
        None => Frame::new(code.n_locals, code.cache_count),
    };
    frame.code = id;
    frame.is_module = is_module;
    for (i, arg) in args.iter().enumerate() {
        frame.locals[i] = *arg;
    }
    frame.derefs.clear();
    for cellvar in &code.cellvars {
        let init = code
            .local_names
            .iter()
            .position(|name| name == cellvar)
            .filter(|&slot| slot < code.params.len())
            .map_or(Value::UNBOUND, |slot| frame.locals[slot]);
        let cell = model.new_cell(init)?;
        frame.derefs.push(cell);
    }
    frame.derefs.extend_from_slice(captured_cells);
    Ok(frame)
}

/// The interpreter driver: runs `entry` (with `args`) over an EXPLICIT stack of [`Frame`]s, so
/// a deep chain of direct Python calls grows a heap `Vec`, never the native stack. A direct
/// `Op::Call` / `Op::CallKw` of a plain or defaulted Python function pushes a frame; `Op::Return`
/// pops one and hands its value to the caller; a raise/trap unwinds across the frames, each
/// consulting its own `exc_table`. Builtins, bound methods, class init, and dunders run through
/// [`call_value`], which re-enters this driver -- bounded native recursion, guarded by `depth`.
///
/// GC: the collector traces EVERY frame on the stack, so a mid-call collection sees all roots
/// (the fix for the old one-frame-at-a-time root gap). `is_module` marks the entry frame as the
/// module body (its `StoreFast`s mirror into the globals). The explicit stack is bounded by
/// [`MAX_CALL_DEPTH`] frames -> `RecursionError`; `depth` bounds the native callback recursion.
fn run_frames(
    entry: &CodeObject,
    functions: &[CodeObject],
    args: &[Value],
    cells: &[Value],
    model: &mut ObjectModel,
    is_module: bool,
    depth: usize,
) -> Result<Value, Trap> {
    if depth > MAX_CALL_DEPTH {
        return Err(Trap::RecursionError);
    }
    let mut frames: Vec<Frame> = Vec::new();
    frames.push(new_frame(entry, CodeId::Entry, args, is_module, cells, model)?);
    match drive(&mut frames, entry, functions, model, depth)? {
        DriveOutcome::Returned(value) => Ok(value),
        DriveOutcome::Yielded(_) => Err(Trap::Malformed),
    }
}

/// Runs the explicit frame stack `frames` (seeded by the caller) until its bottom frame returns
/// ([`DriveOutcome::Returned`]) or a generator frame yields ([`DriveOutcome::Yielded`], leaving
/// the yielding frame on top of `frames` for the resumer to re-suspend). Shared by [`run_frames`]
/// (seeded with the entry frame) and [`resume_generator`] (seeded with a generator's suspended
/// frame). `entry` resolves the bottom frame's [`CodeId::Entry`]; a resume's frames are all
/// [`CodeId::Func`], so it passes the generator's own code and `entry` is never consulted.
fn drive(
    frames: &mut Vec<Frame>,
    entry: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<DriveOutcome, Trap> {
    loop {
        let top = frames.len() - 1;
        let code = resolve_code(frames[top].code, entry, functions);
        let faulting_ip = frames[top].ip as u32;
        let op = *code.ops.get(frames[top].ip).ok_or(Trap::Malformed)?;
        frames[top].ip += 1;
        let frame = &mut frames[top];
        let flow = (|| -> Result<Flow, Trap> {
            match op {
            Op::LoadConst(idx) => {
                let c = code.consts.get(idx as usize).ok_or(Trap::Malformed)?;
                let value = match c {
                    Const::Str(s) => model.new_str(s)?,
                    Const::Int(n) => model.new_long(i128::from(*n))?,
                    Const::Float(bits) => model.new_float(f64::from_bits(*bits))?,
                    #[cfg(feature = "complex")]
                    Const::Imaginary(bits) => model.new_complex(0.0, f64::from_bits(*bits))?,
                    #[cfg(not(feature = "complex"))]
                    Const::Imaginary(_) => return Err(Trap::Unsupported),
                    Const::BigInt(digits) => {
                        let big = crate::bigint::BigInt::from_decimal_str(digits)
                            .ok_or(Trap::Malformed)?;
                        model.new_bigint(big)?
                    }
                    Const::Bytes(data) => model.new_bytes(data.clone())?,
                    other => const_value(other)?,
                };
                frame.push(value);
            }
            Op::LoadFast(idx) => {
                let value = frame.load_local(idx as usize)?;
                frame.push(value);
            }
            Op::StoreFast(idx) => {
                let value = frame.pop()?;
                frame.store_local(idx as usize, value)?;
                if frame.is_module {
                    let name = code.local_names.get(idx as usize).ok_or(Trap::Malformed)?;
                    model.set_global(name, value);
                }
            }
            Op::LoadGlobal(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let value = resolve_global(name, functions, model).ok_or(Trap::NameError)?;
                frame.push(value);
            }
            Op::LoadAttr { name, cache } => {
                let receiver = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                let slot = frame.caches.get_mut(cache as usize).ok_or(Trap::Malformed)?;
                let value = model.getattr(receiver, attr, slot)?;
                if model.is_property(value) && model.is_instance(receiver) {
                    let (fget, _, _) = model.property_accessors(value);
                    if fget.is_none() {
                        return Err(Trap::AttributeError);
                    }
                    let result = call_value(fget, &[receiver], functions, model, depth + 1)?;
                    frame.push(result);
                } else {
                    frame.push(value);
                }
            }
            Op::Binary(binop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let result = if let Some(value) =
                    try_binop_dunder(binop, lhs, rhs, functions, model, depth)?
                {
                    value
                } else if let Some(value) = try_set_binop_dyn(binop, lhs, rhs, functions, model, depth)? {
                    value
                } else if let Some(value) = try_dict_binop_dyn(binop, lhs, rhs, functions, model, depth)? {
                    value
                } else if let Some(value) = model.py_binary(binop, lhs, rhs)? {
                    value
                } else {
                    binary(binop, lhs, rhs, model)?
                };
                frame.push(result);
            }
            Op::Unary(unop) => {
                let value = frame.pop()?;
                let result = if let Some(method) = model.find_dunder(value, unary_dunder_name(unop)) {
                    call_value(method, &[], functions, model, depth + 1)?
                } else {
                    unary(unop, value, model)?
                };
                frame.push(result);
            }
            Op::Compare(cmpop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let result = if matches!(cmpop, CmpOp::Is | CmpOp::IsNot) {
                    let same = lhs == rhs;
                    Value::from_bool(if matches!(cmpop, CmpOp::IsNot) { !same } else { same })
                } else if let Some(value) =
                    try_compare_dunder(cmpop, lhs, rhs, functions, model, depth)?
                {
                    value
                } else if let Some(value) = try_set_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
                    value
                } else if let Some(value) = try_dict_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
                    value
                } else {
                    match model.py_compare(cmpop, lhs, rhs)? {
                        Some(value) => value,
                        None => compare(cmpop, lhs, rhs, model)?,
                    }
                };
                frame.push(result);
            }
            Op::Subscript { cache: _ } => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                let result = if let Some(method) = model.find_dunder(container, "__getitem__") {
                    call_value(method, &[index], functions, model, depth + 1)?
                } else {
                    model.py_getitem_dyn(container, index, functions, depth)?
                };
                frame.push(result);
            }
            Op::BuildSlice => {
                let step = frame.pop()?;
                let upper = frame.pop()?;
                let lower = frame.pop()?;
                frame.push(model.new_slice(lower, upper, step)?);
            }
            Op::BuildList(count) => {
                let mut elems = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    elems.push(frame.pop()?);
                }
                elems.reverse();
                frame.push(model.new_list(elems)?);
            }
            Op::BuildTuple(count) => {
                let mut elems = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    elems.push(frame.pop()?);
                }
                elems.reverse();
                frame.push(model.new_tuple(elems)?);
            }
            Op::BuildDict(count) => {
                let mut pairs = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let value = frame.pop()?;
                    let key = frame.pop()?;
                    pairs.push((key, value));
                }
                pairs.reverse();
                frame.push(model.new_dict_dyn(pairs, functions, depth)?);
            }
            Op::Setitem => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                let value = frame.pop()?;
                if let Some(method) = model.find_dunder(container, "__setitem__") {
                    call_value(method, &[index, value], functions, model, depth + 1)?;
                } else if model.is_slice(index) && model.is_list(container) {
                    let elements = crate::builtins::collect_iterable(model, &[value], functions, depth)?;
                    model.seq_setitem_slice(container, index, elements)?;
                } else {
                    model.py_setitem_dyn(container, index, value, functions, depth)?;
                }
            }
            Op::Contains { negate } => {
                let container = frame.pop()?;
                let element = frame.pop()?;
                let contained = if let Some(method) = model.find_dunder(container, "__contains__") {
                    let result = call_value(method, &[element], functions, model, depth + 1)?;
                    model.py_truthy(result)?.unwrap_or(false)
                } else if model.is_set(container) || model.is_frozenset(container) {
                    let elems = model.set_value(container).ok_or(Trap::TypeError)?.clone();
                    elems_contain(element, &elems, functions, model, depth)?
                } else {
                    model.py_contains_dyn(container, element, functions, depth)?
                };
                frame.push(Value::from_bool(contained ^ negate));
            }
            Op::GetIter => {
                let iterable = frame.pop()?;
                let iterator = iterator_for(iterable, functions, model, depth + 1)?;
                frame.push(iterator);
            }
            Op::ForIter(target) => {
                let iterator = frame.pop()?;
                match py_next_value(iterator, functions, model, depth)? {
                    Some(value) => frame.push(value),
                    None => frame.ip = target as usize,
                }
            }
            Op::PopTop => {
                frame.pop()?;
            }
            Op::Jump(target) => {
                frame.ip = target as usize;
            }
            Op::PopJumpIfFalse(target) => {
                let value = frame.pop()?;
                if !py_truthy_dyn(value, functions, model, depth)? {
                    frame.ip = target as usize;
                }
            }
            Op::Call(argc) => {
                let argc = argc as usize;
                let mut call_args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    call_args.push(frame.pop()?);
                }
                call_args.reverse();
                let callee = frame.pop()?;
                if let Some(index) = callee.as_function_index() {
                    let callee_code = functions.get(index as usize).ok_or(Trap::Malformed)?;
                    let bound = if callee_code.has_varargs
                        || callee_code.has_varkwargs
                        || callee_code.kwonly_count > 0
                    {
                        bind_arguments(callee_code, &call_args, &[], &[], Value::NONE, model)?
                    } else {
                        call_args
                    };
                    if callee_code.is_generator {
                        let generator = new_frame(callee_code, CodeId::Func(index), &bound, false, &[], model)?;
                        frame.push(model.new_generator(generator)?);
                    } else {
                        return Ok(Flow::Call { index, args: bound, cells: Vec::new() });
                    }
                } else if model.is_py_function(callee) {
                    let index = model.py_function_index(callee);
                    let callee_code = functions.get(index as usize).ok_or(Trap::Malformed)?;
                    let defaults = model.py_function_defaults(callee);
                    let kwdefaults = model.py_function_kwdefaults(callee);
                    let bound = bind_arguments(callee_code, &call_args, &[], &defaults, kwdefaults, model)?;
                    let cells = model.py_function_cells(callee);
                    if callee_code.is_generator {
                        let generator = new_frame(callee_code, CodeId::Func(index), &bound, false, &cells, model)?;
                        frame.push(model.new_generator(generator)?);
                    } else {
                        return Ok(Flow::Call { index, args: bound, cells });
                    }
                } else {
                    let result = call_value(callee, &call_args, functions, model, depth + 1)?;
                    frame.push(result);
                }
            }
            Op::Return => return Ok(Flow::Return(frame.pop()?)),
            Op::Raise(argc) => {
                let exception = if argc == 2 {
                    let cause = frame.pop()?;
                    let value = frame.pop()?;
                    let exception = model.raise_value(value)?;
                    let cause = if model.is_class(cause) { model.new_object(cause)? } else { cause };
                    model.py_setattr_instance(exception, "__cause__", cause)?;
                    exception
                } else if argc == 1 {
                    let value = frame.pop()?;
                    model.raise_value(value)?
                } else {
                    match frame.active_exception {
                        Some(active) => active,
                        None => {
                            let class =
                                model.exception_class("RuntimeError").ok_or(Trap::Malformed)?;
                            model.new_object(class)?
                        }
                    }
                };
                model.set_pending_exception(exception);
                return Err(Trap::Raised);
            }
            Op::MatchExc => {
                let exc_type = frame.pop()?;
                let active = frame.active_exception.ok_or(Trap::Malformed)?;
                let matched = if let Some(types) = model.seq_value(exc_type).cloned() {
                    types.iter().any(|&ty| model.exception_isinstance(active, ty))
                } else {
                    model.exception_isinstance(active, exc_type)
                };
                frame.push(Value::from_bool(matched));
            }
            Op::LoadExc => {
                let active = frame.active_exception.ok_or(Trap::Malformed)?;
                frame.push(active);
            }
            Op::PopExcept => {
                frame.active_exception = None;
            }
            Op::Reraise => {
                let active = frame.active_exception.ok_or(Trap::Malformed)?;
                model.set_pending_exception(active);
                return Err(Trap::Raised);
            }
            Op::MakeFunction { func, flags } => {
                let name = code.names.get(func as usize).ok_or(Trap::Malformed)?;
                let index = functions
                    .iter()
                    .position(|f| f.name == *name)
                    .ok_or(Trap::NameError)? as u32;
                if flags == 0 {
                    frame.push(Value::function_ref(index));
                } else {
                    let cells = if flags & 0x04 != 0 {
                        let ncells =
                            functions.get(index as usize).ok_or(Trap::Malformed)?.freevars.len();
                        let mut cells = Vec::with_capacity(ncells);
                        for _ in 0..ncells {
                            cells.push(frame.pop()?);
                        }
                        cells.reverse();
                        Some(model.new_tuple(cells)?)
                    } else {
                        None
                    };
                    let kwdefaults = if flags & 0x02 != 0 { frame.pop()? } else { Value::NONE };
                    let defaults = if flags & 0x01 != 0 { frame.pop()? } else { Value::NONE };
                    let function = match cells {
                        Some(cells) => model.new_closure(index, defaults, kwdefaults, cells)?,
                        None => model.new_py_function(index, defaults, kwdefaults)?,
                    };
                    frame.push(function);
                }
            }
            Op::CallKw { argc, kwnames } => {
                let names = match code.consts.get(kwnames as usize) {
                    Some(Const::KwNames(names)) => names,
                    _ => return Err(Trap::Malformed),
                };
                let mut kwvals = Vec::with_capacity(names.len());
                for _ in 0..names.len() {
                    kwvals.push(frame.pop()?);
                }
                kwvals.reverse();
                let mut call_args = Vec::with_capacity(argc as usize);
                for _ in 0..argc {
                    call_args.push(frame.pop()?);
                }
                call_args.reverse();
                let callee = frame.pop()?;
                let kwargs: Vec<(&str, Value)> =
                    names.iter().map(|s| s.as_str()).zip(kwvals).collect();
                if let Some(index) = callee.as_function_index() {
                    let callee_code = functions.get(index as usize).ok_or(Trap::Malformed)?;
                    let bound = bind_arguments(callee_code, &call_args, &kwargs, &[], Value::NONE, model)?;
                    if callee_code.is_generator {
                        let generator = new_frame(callee_code, CodeId::Func(index), &bound, false, &[], model)?;
                        frame.push(model.new_generator(generator)?);
                    } else {
                        return Ok(Flow::Call { index, args: bound, cells: Vec::new() });
                    }
                } else if model.is_py_function(callee) {
                    let index = model.py_function_index(callee);
                    let callee_code = functions.get(index as usize).ok_or(Trap::Malformed)?;
                    let defaults = model.py_function_defaults(callee);
                    let kwdefaults = model.py_function_kwdefaults(callee);
                    let bound = bind_arguments(callee_code, &call_args, &kwargs, &defaults, kwdefaults, model)?;
                    let cells = model.py_function_cells(callee);
                    if callee_code.is_generator {
                        let generator = new_frame(callee_code, CodeId::Func(index), &bound, false, &cells, model)?;
                        frame.push(model.new_generator(generator)?);
                    } else {
                        return Ok(Flow::Call { index, args: bound, cells });
                    }
                } else {
                    let result =
                        call_value_kw(callee, &call_args, &kwargs, functions, model, depth + 1)?;
                    frame.push(result);
                }
            }
            Op::CallEx {
                argc,
                kinds,
                kwnames,
            } => {
                let kinds = match code.consts.get(kinds as usize) {
                    Some(Const::ArgKinds(k)) => k.clone(),
                    _ => return Err(Trap::Malformed),
                };
                let names = match code.consts.get(kwnames as usize) {
                    Some(Const::KwNames(n)) => n.clone(),
                    _ => return Err(Trap::Malformed),
                };
                if kinds.len() != argc as usize {
                    return Err(Trap::Malformed);
                }
                let mut vals = Vec::with_capacity(argc as usize);
                for _ in 0..argc {
                    vals.push(frame.pop()?);
                }
                vals.reverse();
                let callee = frame.pop()?;
                let mut posargs: Vec<Value> = Vec::new();
                let mut kw_owned: Vec<(String, Value)> = Vec::new();
                let mut names = names.into_iter();
                for (kind, val) in kinds.iter().zip(vals) {
                    match kind {
                        0 => posargs.push(val),
                        1 => {
                            let items = model.seq_elements(val).ok_or(Trap::TypeError)?;
                            posargs.extend(items);
                        }
                        2 => {
                            let name = names.next().ok_or(Trap::Malformed)?;
                            kw_owned.push((name, val));
                        }
                        3 => {
                            let entries = model.dict_entries(val).ok_or(Trap::TypeError)?;
                            for (key, value) in entries {
                                let name = model.str_value(key).ok_or(Trap::TypeError)?.to_string();
                                kw_owned.push((name, value));
                            }
                        }
                        _ => return Err(Trap::Malformed),
                    }
                }
                let kwargs: Vec<(&str, Value)> =
                    kw_owned.iter().map(|(k, v)| (k.as_str(), *v)).collect();
                let result = call_value_kw(callee, &posargs, &kwargs, functions, model, depth + 1)?;
                frame.push(result);
            }
            Op::Yield => {
                return Ok(Flow::Yield(frame.pop()?));
            }
            Op::SetupClassNamespace => {
                let namespace = model.new_dict(Vec::new())?;
                frame.class_namespace = Some(namespace);
            }
            Op::StoreName(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let value = frame.pop()?;
                let namespace = frame.class_namespace.ok_or(Trap::Malformed)?;
                let key = model.new_str(name)?;
                model.py_setitem(namespace, key, value)?;
            }
            Op::LoadName(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let from_namespace = frame
                    .class_namespace
                    .and_then(|namespace| model.dict_get_str(namespace, name));
                let value = match from_namespace {
                    Some(value) => value,
                    None => resolve_global(name, functions, model).ok_or(Trap::NameError)?,
                };
                frame.push(value);
            }
            Op::ImportName(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let module = model.import_module(name)?;
                frame.push(module);
            }
            Op::ImportFrom(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let module = frame.peek()?;
                let value = model.import_from(module, name)?;
                frame.push(value);
            }
            Op::BuildClass => {
                let namespace = match frame.class_namespace.take() {
                    Some(namespace) => namespace,
                    None => frame.pop()?,
                };
                let base = frame.pop()?;
                let name = frame.pop()?;
                frame.push(model.new_class(name, base, namespace)?);
            }
            Op::SetAttr { name, cache: _ } => {
                let object = frame.pop()?;
                let value = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                if model.is_instance(object) {
                    if let Some(property) = model.class_property(object, attr) {
                        let (_, fset, _) = model.property_accessors(property);
                        if fset.is_none() {
                            return Err(Trap::AttributeError);
                        }
                        call_value(fset, &[object, value], functions, model, depth + 1)?;
                    } else {
                        model.py_setattr_instance(object, attr, value)?;
                    }
                } else if model.is_class(object) {
                    model.py_setattr_class(object, attr, value)?;
                } else if model.is_user_function(object) {
                    model.py_setattr_function(object, attr, value)?;
                } else {
                    model.py_setattr_native(object, attr, value)?;
                }
            }
            Op::DeleteItem => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                if let Some(method) = model.find_dunder(container, "__delitem__") {
                    call_value(method, &[index], functions, model, depth + 1)?;
                } else {
                    model.py_delitem_dyn(container, index, functions, depth)?;
                }
            }
            Op::DeleteAttr { name } => {
                let object = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                if model.is_instance(object) {
                    model.py_delattr_instance(object, attr)?;
                } else {
                    return Err(Trap::AttributeError);
                }
            }
            Op::DeleteFast(idx) => {
                frame.store_local(idx as usize, Value::UNBOUND)?;
            }
            Op::UnpackSequence(count) => {
                let value = frame.pop()?;
                let elements = crate::builtins::collect_iterable(model, &[value], functions, depth)?;
                if elements.len() != count as usize {
                    return Err(Trap::ValueError);
                }
                for &element in elements.iter().rev() {
                    frame.push(element);
                }
            }
            Op::UnpackEx { before, after } => {
                let value = frame.pop()?;
                let targets = model.unpack_ex(value, before as usize, after as usize)?;
                for &target in targets.iter().rev() {
                    frame.push(target);
                }
            }
            Op::ListAppend => {
                let value = frame.pop()?;
                let list = frame.pop()?;
                model.list_append(list, value)?;
            }
            Op::SetAdd => {
                let value = frame.pop()?;
                let set = frame.pop()?;
                let elems = model.set_value(set).ok_or(Trap::TypeError)?.clone();
                if !elems_contain(value, &elems, functions, model, depth)? {
                    model.set_push(set, value)?;
                }
            }
            Op::DictInsert => {
                let value = frame.pop()?;
                let key = frame.pop()?;
                let dict = frame.pop()?;
                model.py_setitem_dyn(dict, key, value, functions, depth)?;
            }
            Op::LoadSuper(name_idx) => {
                let class_name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let class = model.get_global(class_name).ok_or(Trap::NameError)?;
                let self_value = frame.load_local(0)?;
                frame.push(model.new_super(class, self_value)?);
            }
            Op::BuildSet(count) => {
                let mut elements = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    elements.push(frame.pop()?);
                }
                elements.reverse();
                let deduped = dedup_elems(elements, functions, model, depth)?;
                frame.push(model.new_set(deduped)?);
            }
            Op::LoadDeref(idx) => {
                let cell = *frame.derefs.get(idx as usize).ok_or(Trap::Malformed)?;
                frame.push(model.cell_get(cell)?);
            }
            Op::StoreDeref(idx) => {
                let value = frame.pop()?;
                let cell = *frame.derefs.get(idx as usize).ok_or(Trap::Malformed)?;
                model.cell_set(cell, value)?;
            }
            Op::LoadClosure(idx) => {
                let cell = *frame.derefs.get(idx as usize).ok_or(Trap::Malformed)?;
                frame.push(cell);
            }
            }
            Ok(Flow::Next)
        })();
        match flow {
            Ok(Flow::Next) => {}
            Ok(Flow::Return(value)) => {
                if let Some(done) = frames.pop() {
                    model.recycle_frame(done);
                }
                match frames.last_mut() {
                    Some(caller) => caller.push(value),
                    None => return Ok(DriveOutcome::Returned(value)),
                }
            }
            Ok(Flow::Yield(value)) => return Ok(DriveOutcome::Yielded(value)),
            Ok(Flow::Call { index, args, cells }) => {
                let callee = functions.get(index as usize).ok_or(Trap::Malformed)?;
                if frames.len() >= MAX_CALL_DEPTH {
                    return Err(Trap::RecursionError);
                }
                frames.push(new_frame(callee, CodeId::Func(index), &args, false, &cells, model)?);
            }
            Err(trap) => {
                let exception = match model.take_pending_exception() {
                    Some(exception) => exception,
                    None => match model.trap_to_exception(trap) {
                        Some(exception) => exception,
                        None => return Err(trap),
                    },
                };
                let mut search_ip = faulting_ip;
                loop {
                    let top = frames.len() - 1;
                    let code = resolve_code(frames[top].code, entry, functions);
                    if let Some(handler) = find_handler(&code.exc_table, search_ip) {
                        let frame = &mut frames[top];
                        frame.stack.truncate(handler.depth as usize);
                        frame.active_exception = Some(exception);
                        frame.ip = handler.target as usize;
                        break;
                    }
                    frames.pop();
                    match frames.last() {
                        Some(caller) => search_ip = (caller.ip as u32).saturating_sub(1),
                        None => {
                            model.set_pending_exception(exception);
                            return Err(trap);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::PyType;
    use lamella_py_bytecode::{Param, StaticType};

    #[test]
    fn frame_pool_recycles_buffers() {
        let mut model = no_objects();
        let c = code(2, 0, vec![], vec![], 0, vec![]);
        let f1 = new_frame(&c, CodeId::Entry, &[], false, &[], &mut model).unwrap();
        let cap = f1.locals.capacity();
        model.recycle_frame(f1);
        let f2 = new_frame(&c, CodeId::Func(0), &[], false, &[], &mut model).unwrap();
        assert_eq!(f2.locals.len(), 2);
        assert!(f2.locals.iter().all(|v| v.is_unbound()));
        assert_eq!(f2.locals.capacity(), cap);
        assert!(model.take_pooled_frame().is_none());
    }

    #[test]
    fn generator_method_ids_map_the_four_methods() {
        assert_eq!(generator_method_id("send"), Some(GEN_SEND));
        assert_eq!(generator_method_id("throw"), Some(GEN_THROW));
        assert_eq!(generator_method_id("close"), Some(GEN_CLOSE));
        assert_eq!(generator_method_id("__next__"), Some(GEN_NEXT));
        assert_eq!(generator_method_id("nope"), None);
    }

    /// An empty object space, for code that never touches an object.
    fn no_objects() -> ObjectModel {
        ObjectModel::new(Vec::new(), 64)
    }

    fn bin(op: BinOp, a: Value, b: Value) -> Result<Value, Trap> {
        binary(op, a, b, &mut ObjectModel::new(Vec::new(), 4096))
    }
    fn cmp(op: CmpOp, a: Value, b: Value) -> Result<Value, Trap> {
        compare(op, a, b, &ObjectModel::new(Vec::new(), 4096))
    }
    fn un(op: UnaryOp, v: Value) -> Result<Value, Trap> {
        unary(op, v, &mut ObjectModel::new(Vec::new(), 4096))
    }

    /// Builds a code object from the minimal fields the interpreter reads,
    /// defaulting the typing fields the lowering (not the interpreter) consumes.
    fn code(
        n_locals: usize,
        n_args: usize,
        consts: Vec<Const>,
        names: Vec<String>,
        cache_count: usize,
        ops: Vec<Op>,
    ) -> CodeObject {
        CodeObject {
            name: String::from("<test>"),
            params: (0..n_args)
                .map(|i| Param {
                    name: format!("a{i}"),
                    ty: StaticType::Dynamic,
                })
                .collect(),
            posonly_count: 0,
            kwonly_count: 0,
            is_generator: false,
            has_varargs: false,
            has_varkwargs: false,
            ret_ty: StaticType::Dynamic,
            n_locals,
            local_names: (0..n_locals).map(|i| format!("v{i}")).collect(),
            cellvars: Vec::new(),
            freevars: Vec::new(),
            local_types: vec![StaticType::Dynamic; n_locals],
            consts,
            names,
            ops,
            cache_count,
            exc_table: Vec::new(),
        }
    }

    /// An iterative `fib`:
    /// ```python
    /// def fib(n: int) -> int:
    ///     a = 0
    ///     b = 1
    ///     i = 0
    ///     while i < n:
    ///         t = a + b
    ///         a = b
    ///         b = t
    ///         i = i + 1
    ///     return a
    /// ```
    /// Locals: n=0 (arg), a=1, b=2, i=3, t=4. Consts: [0, 1].
    fn fib_code() -> CodeObject {
        use Op::*;
        let ops = vec![
            LoadConst(0),
            StoreFast(1),
            LoadConst(1),
            StoreFast(2),
            LoadConst(0),
            StoreFast(3),
            LoadFast(3),
            LoadFast(0),
            Compare(CmpOp::Lt),
            PopJumpIfFalse(23),
            LoadFast(1),
            LoadFast(2),
            Binary(BinOp::Add),
            StoreFast(4),
            LoadFast(2),
            StoreFast(1),
            LoadFast(4),
            StoreFast(2),
            LoadFast(3),
            LoadConst(1),
            Binary(BinOp::Add),
            StoreFast(3),
            Jump(6),
            LoadFast(1),
            Return,
        ];
        code(5, 1, vec![Const::Int(0), Const::Int(1)], Vec::new(), 0, ops)
    }

    #[test]
    fn fib_ten_is_fifty_five() {
        let code = fib_code();
        let mut model = no_objects();
        let result = run(&code, &[], &[Value::fixnum(10).unwrap()], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(55));
    }

    #[test]
    fn bind_arguments_positionals_keywords_and_defaults() {
        let c = code(2, 2, Vec::new(), Vec::new(), 0, Vec::new());
        let f = |n: i32| Value::fixnum(n).unwrap();
        let mut m = no_objects();
        assert_eq!(bind_arguments(&c, &[f(5)], &[], &[f(10)], Value::NONE, &mut m).unwrap(), vec![f(5), f(10)]);
        assert_eq!(
            bind_arguments(&c, &[f(5)], &[("a1", f(20))], &[], Value::NONE, &mut m).unwrap(),
            vec![f(5), f(20)]
        );
        assert_eq!(
            bind_arguments(&c, &[f(5), f(6)], &[], &[f(10)], Value::NONE, &mut m).unwrap(),
            vec![f(5), f(6)]
        );
        let mut va = code(2, 2, Vec::new(), Vec::new(), 0, Vec::new());
        va.has_varargs = true;
        let bound = bind_arguments(&va, &[f(5), f(6), f(7)], &[], &[], Value::NONE, &mut m).unwrap();
        assert_eq!(bound.len(), 2);
        assert_eq!(bound[0], f(5));
    }

    #[test]
    fn bind_arguments_rejects_bad_calls() {
        let c = code(2, 2, Vec::new(), Vec::new(), 0, Vec::new());
        let f = |n: i32| Value::fixnum(n).unwrap();
        let mut m = no_objects();
        assert_eq!(bind_arguments(&c, &[f(1), f(2), f(3)], &[], &[], Value::NONE, &mut m), Err(Trap::TypeError));
        assert_eq!(bind_arguments(&c, &[f(1)], &[("nope", f(2))], &[], Value::NONE, &mut m), Err(Trap::TypeError));
        assert_eq!(bind_arguments(&c, &[f(1)], &[("a0", f(2))], &[], Value::NONE, &mut m), Err(Trap::TypeError));
        assert_eq!(bind_arguments(&c, &[f(1)], &[], &[], Value::NONE, &mut m), Err(Trap::TypeError));
    }

    #[test]
    fn make_function_with_flags_builds_a_defaulted_py_function() {
        use Op::*;
        let mut add = code(2, 2, Vec::new(), Vec::new(), 0,
            vec![LoadFast(0), LoadFast(1), Binary(BinOp::Add), Return]);
        add.name = String::from("add");
        let body = code(0, 0, vec![Const::Int(10)], vec![String::from("add")], 0,
            vec![LoadConst(0), BuildTuple(1), MakeFunction { func: 0, flags: 1 }, Return]);
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let pyfunc = run(&body, core::slice::from_ref(&add), &[], &mut model).unwrap();
        assert!(model.is_py_function(pyfunc));
        assert_eq!(model.py_function_index(pyfunc), 0);
        assert_eq!(model.py_function_defaults(pyfunc), vec![Value::fixnum(10).unwrap()]);
    }

    #[test]
    fn bound_method_binds_positional_defaults() {
        use Op::*;
        let m = code(2, 2, Vec::new(), Vec::new(), 0, vec![LoadFast(1), Return]);
        let functions = [m];
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let f = |n: i32| Value::fixnum(n).unwrap();
        let defaults = model.new_tuple(vec![f(10)]).unwrap();
        let pyfunc = model.new_py_function(0, defaults, Value::NONE).unwrap();
        let bound = model.new_py_bound(f(0), pyfunc).unwrap();
        assert_eq!(call_value(bound, &[], &functions, &mut model, 0).unwrap(), f(10));
        assert_eq!(call_value(bound, &[f(5)], &functions, &mut model, 0).unwrap(), f(5));
    }

    #[test]
    fn bind_arguments_applies_keyword_only_defaults() {
        let mut c = code(2, 2, Vec::new(), Vec::new(), 0, Vec::new());
        c.kwonly_count = 1;
        c.params[1].name = String::from("b");
        let f = |n: i32| Value::fixnum(n).unwrap();
        let mut m = ObjectModel::new(Vec::new(), 16 * 1024);
        let b_key = m.new_str("b").unwrap();
        let kwdefaults = m.new_dict(vec![(b_key, f(1))]).unwrap();
        assert_eq!(bind_arguments(&c, &[f(5)], &[], &[], kwdefaults, &mut m).unwrap(), vec![f(5), f(1)]);
        assert_eq!(
            bind_arguments(&c, &[f(5)], &[("b", f(9))], &[], kwdefaults, &mut m).unwrap(),
            vec![f(5), f(9)]
        );
        assert_eq!(bind_arguments(&c, &[f(5)], &[], &[], Value::NONE, &mut m), Err(Trap::TypeError));
    }

    #[test]
    fn instantiating_a_class_binds_a_defaulted_init() {
        use Op::*;
        let init = code(2, 2, vec![Const::None], vec![String::from("val")], 1,
            vec![LoadFast(1), LoadFast(0), SetAttr { name: 0, cache: 0 }, LoadConst(0), Return]);
        let functions = [init];
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let f = |n: i32| Value::fixnum(n).unwrap();
        let name = model.new_str("C").unwrap();
        let key_init = model.new_str("__init__").unwrap();
        let defaults = model.new_tuple(vec![f(7)]).unwrap();
        let init_fn = model.new_py_function(0, defaults, Value::NONE).unwrap();
        let ns = model.new_dict(vec![(key_init, init_fn)]).unwrap();
        let class = model.new_class(name, Value::NONE, ns).unwrap();
        let obj = call_value(class, &[], &functions, &mut model, 0).unwrap();
        assert!(model.is_instance(obj));
        assert_eq!(model.py_getattr_instance(obj, "val").unwrap(), f(7));
        let obj2 = call_value(class, &[f(9)], &functions, &mut model, 0).unwrap();
        assert_eq!(model.py_getattr_instance(obj2, "val").unwrap(), f(9));
    }

    #[test]
    fn closure_reads_a_captured_enclosing_local() {
        use Op::*;
        let mut inner = code(0, 0, vec![Const::Int(1)], Vec::new(), 0,
            vec![LoadDeref(0), LoadConst(0), Binary(BinOp::Add), Return]);
        inner.name = String::from("inner");
        inner.freevars = vec![String::from("v0")];
        let mut outer = code(2, 1, Vec::new(), vec![String::from("inner")], 0,
            vec![LoadClosure(0), MakeFunction { func: 0, flags: 0x04 }, StoreFast(1), LoadFast(1), Return]);
        outer.name = String::from("outer");
        outer.cellvars = vec![String::from("v0")];
        let entry = code(0, 0, vec![Const::Int(10)], vec![String::from("outer")], 0,
            vec![MakeFunction { func: 0, flags: 0 }, LoadConst(0), Call(1), Call(0), Return]);
        let functions = [outer, inner];
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let result = run(&entry, &functions, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(11));
    }

    #[test]
    fn closure_shares_a_mutable_cell_across_calls() {
        use Op::*;
        let mut inc = code(0, 0, vec![Const::Int(1)], Vec::new(), 0,
            vec![LoadDeref(0), LoadConst(0), Binary(BinOp::Add), StoreDeref(0), LoadDeref(0), Return]);
        inc.name = String::from("inc");
        inc.freevars = vec![String::from("v0")];
        let mut make_counter = code(2, 0, vec![Const::Int(0)], vec![String::from("inc")], 0,
            vec![LoadConst(0), StoreDeref(0), LoadClosure(0),
                 MakeFunction { func: 0, flags: 0x04 }, StoreFast(1), LoadFast(1), Return]);
        make_counter.name = String::from("make_counter");
        make_counter.cellvars = vec![String::from("v0")];
        let entry = code(1, 0, Vec::new(), vec![String::from("make_counter")], 0,
            vec![MakeFunction { func: 0, flags: 0 }, Call(0), StoreFast(0),
                 LoadFast(0), Call(0), LoadFast(0), Call(0), Binary(BinOp::Add), Return]);
        let functions = [make_counter, inc];
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let result = run(&entry, &functions, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(3));
    }

    #[test]
    fn class_body_reads_a_name_it_just_bound() {
        use Op::*;
        let entry = code(
            1,
            0,
            vec![Const::Str(String::from("C")), Const::None, Const::Int(5), Const::Int(1)],
            vec![String::from("a"), String::from("b")],
            1,
            vec![
                LoadConst(0),
                LoadConst(1),
                SetupClassNamespace,
                LoadConst(2),
                StoreName(0),
                LoadName(0),
                LoadConst(3),
                Binary(BinOp::Add),
                StoreName(1),
                BuildClass,
                StoreFast(0),
                LoadFast(0),
                LoadAttr { name: 1, cache: 0 },
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let result = run(&entry, &[], &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(6));
    }

    #[test]
    fn class_body_name_read_falls_back_to_a_builtin() {
        use Op::*;
        let entry = code(
            1,
            0,
            vec![Const::Str(String::from("C")), Const::None],
            vec![String::from("len"), String::from("n")],
            1,
            vec![
                LoadConst(0),
                LoadConst(1),
                SetupClassNamespace,
                LoadName(0),
                StoreName(1),
                BuildClass,
                StoreFast(0),
                LoadFast(0),
                LoadAttr { name: 1, cache: 0 },
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let result = run(&entry, &[], &[], &mut model).unwrap();
        assert!(result.as_builtin_id().is_some(), "C.n resolved to the len built-in");
    }

    #[test]
    fn import_math_and_call_a_function() {
        use Op::*;
        let entry = code(
            1,
            0,
            vec![Const::Int(4)],
            vec![String::from("math"), String::from("sqrt")],
            1,
            vec![
                ImportName(0),
                StoreFast(0),
                LoadFast(0),
                LoadAttr { name: 1, cache: 0 },
                LoadConst(0),
                Call(1),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run(&entry, &[], &[], &mut model).unwrap();
        assert_eq!(model.as_f64(result), Some(2.0));
    }

    #[test]
    fn from_import_binds_a_member() {
        use Op::*;
        let entry = code(
            1,
            0,
            vec![Const::Int(9)],
            vec![String::from("math"), String::from("sqrt")],
            0,
            vec![
                ImportName(0),
                ImportFrom(1),
                StoreFast(0),
                PopTop,
                LoadFast(0),
                LoadConst(0),
                Call(1),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run(&entry, &[], &[], &mut model).unwrap();
        assert_eq!(model.as_f64(result), Some(3.0));
    }

    #[test]
    fn from_import_reads_a_constant() {
        use Op::*;
        let entry = code(
            1,
            0,
            Vec::new(),
            vec![String::from("math"), String::from("pi")],
            0,
            vec![ImportName(0), ImportFrom(1), StoreFast(0), PopTop, LoadFast(0), Return],
        );
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run(&entry, &[], &[], &mut model).unwrap();
        assert_eq!(model.as_f64(result), Some(core::f64::consts::PI));
    }

    #[test]
    fn import_is_idempotent_and_cached() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let first = model.import_module("math").unwrap();
        let second = model.import_module("math").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn import_unknown_module_raises_module_not_found() {
        use Op::*;
        let entry = code(
            1,
            0,
            Vec::new(),
            vec![String::from("nonexistent_xyz")],
            0,
            vec![ImportName(0), StoreFast(0), Return],
        );
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let err = run(&entry, &[], &[], &mut model).unwrap_err();
        assert_eq!(err, Trap::Raised);
        let exc = model.take_pending_exception().unwrap();
        assert_eq!(model.exception_type_name(exc), Some("ModuleNotFoundError"));
    }

    #[test]
    fn from_import_of_a_missing_member_raises_import_error() {
        use Op::*;
        let entry = code(
            1,
            0,
            Vec::new(),
            vec![String::from("math"), String::from("nope")],
            0,
            vec![ImportName(0), ImportFrom(1), StoreFast(0), PopTop, Return],
        );
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let err = run(&entry, &[], &[], &mut model).unwrap_err();
        assert_eq!(err, Trap::Raised);
        let exc = model.take_pending_exception().unwrap();
        assert_eq!(model.exception_type_name(exc), Some("ImportError"));
    }

    #[test]
    fn a_defaulted_function_called_positionally_fills_the_default() {
        use Op::*;
        let mut add = code(2, 2, Vec::new(), Vec::new(), 0,
            vec![LoadFast(0), LoadFast(1), Binary(BinOp::Add), Return]);
        add.name = String::from("add");
        let functions = [add];
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let defaults = model.new_tuple(vec![Value::fixnum(10).unwrap()]).unwrap();
        let pyfunc = model.new_py_function(0, defaults, Value::NONE).unwrap();
        let result = call_value(pyfunc, &[Value::fixnum(5).unwrap()], &functions, &mut model, 1).unwrap();
        assert_eq!(result.as_fixnum(), Some(15));
    }

    #[test]
    fn callkw_binds_a_keyword_argument() {
        use Op::*;
        let mut sub = code(2, 2, Vec::new(), Vec::new(), 0,
            vec![LoadFast(0), LoadFast(1), Binary(BinOp::Sub), Return]);
        sub.name = String::from("sub");
        let body = code(0, 0,
            vec![Const::Int(10), Const::Int(3), Const::KwNames(vec![String::from("a1")])],
            vec![String::from("sub")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), CallKw { argc: 1, kwnames: 2 }, Return]);
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&body, core::slice::from_ref(&sub), &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(7));
    }

    #[test]
    fn fib_matches_the_reference_sequence() {
        let expected = [0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144];
        let code = fib_code();
        let mut model = no_objects();
        for (n, want) in expected.iter().enumerate() {
            let got = run(&code, &[], &[Value::fixnum(n as i32).unwrap()], &mut model).unwrap();
            assert_eq!(got.as_fixnum(), Some(*want), "fib({n})");
        }
    }

    #[test]
    fn arithmetic_promotes_past_the_fixnum_range_to_a_long() {
        use Op::*;
        let code = code(
            0,
            0,
            vec![Const::Int(i64::from(FIXNUM_MAX)), Const::Int(2)],
            Vec::new(),
            0,
            vec![LoadConst(0), LoadConst(1), Binary(BinOp::Mul), Return],
        );
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let got = run(&code, &[], &[], &mut model).unwrap();
        assert_eq!(model.long_value(got), Some(i128::from(FIXNUM_MAX) * 2));
    }

    #[test]
    fn try_except_catches_a_raised_exception() {
        use Op::*;
        let mut code = code(
            0,
            0,
            vec![Const::Int(99)],
            vec![String::from("IndexError")],
            0,
            vec![LoadGlobal(0), Raise(1), PopExcept, LoadConst(0), Return],
        );
        code.exc_table = vec![ExcEntry {
            start: 0,
            end: 2,
            target: 2,
            depth: 0,
        }];
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run(&code, &[], &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(99));
    }

    #[test]
    fn uncaught_exception_escapes_with_its_type() {
        use Op::*;
        let code = code(
            0,
            0,
            Vec::new(),
            vec![String::from("ValueError")],
            0,
            vec![LoadGlobal(0), Raise(1)],
        );
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().unwrap();
        assert_eq!(model.exception_type_name(exc), Some("ValueError"));
    }

    #[test]
    fn an_exception_unwinds_from_a_callee_to_the_callers_handler() {
        use Op::*;
        let mut boom = code(0, 0, Vec::new(), vec![String::from("ValueError")], 0,
            vec![LoadGlobal(0), Raise(1)]);
        boom.name = String::from("boom");
        let mut main = code(0, 0, vec![Const::Int(7)], vec![String::from("boom")], 0,
            vec![LoadGlobal(0), Call(0), PopTop, PopExcept, LoadConst(0), Return]);
        main.exc_table = vec![ExcEntry { start: 0, end: 2, target: 3, depth: 0 }];
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run(&main, core::slice::from_ref(&boom), &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(7));
    }

    #[test]
    fn a_generator_yields_its_values_through_a_for_loop() {
        use Op::*;
        let mut generator = code(0, 0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3), Const::None], Vec::new(), 0,
            vec![
                LoadConst(0), Yield, PopTop,
                LoadConst(1), Yield, PopTop,
                LoadConst(2), Yield, PopTop,
                LoadConst(3), Return,
            ]);
        generator.name = String::from("gen");
        generator.is_generator = true;
        let main = code(3, 0, vec![Const::Int(0)], vec![String::from("gen")], 0, vec![
            LoadConst(0), StoreFast(0),
            LoadGlobal(0), Call(0),
            GetIter, StoreFast(1),
            LoadFast(1), ForIter(14), StoreFast(2),
            LoadFast(0), LoadFast(2), Binary(BinOp::Add), StoreFast(0),
            Jump(6),
            LoadFast(0), Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&main, core::slice::from_ref(&generator), &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(6));
    }

    #[test]
    fn a_stateful_generator_persists_locals_across_yields() {
        use Op::*;
        let mut countup = code(1, 0,
            vec![Const::Int(0), Const::Int(3), Const::Int(1), Const::None], Vec::new(), 0,
            vec![
                LoadConst(0), StoreFast(0),
                LoadFast(0), LoadConst(1), Compare(CmpOp::Lt),
                PopJumpIfFalse(14),
                LoadFast(0), Yield, PopTop,
                LoadFast(0), LoadConst(2), Binary(BinOp::Add), StoreFast(0),
                Jump(2),
                LoadConst(3), Return,
            ]);
        countup.name = String::from("countup");
        countup.is_generator = true;
        let main = code(3, 0, vec![Const::Int(0)], vec![String::from("countup")], 0, vec![
            LoadConst(0), StoreFast(0),
            LoadGlobal(0), Call(0),
            GetIter, StoreFast(1),
            LoadFast(1), ForIter(14), StoreFast(2),
            LoadFast(0), LoadFast(2), Binary(BinOp::Add), StoreFast(0),
            Jump(6),
            LoadFast(0), Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&main, core::slice::from_ref(&countup), &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(3));
    }

    #[test]
    fn a_generator_feeds_iterable_consuming_builtins() {
        use Op::*;
        let mut generator = code(0, 0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3), Const::None], Vec::new(), 0,
            vec![LoadConst(0), Yield, PopTop, LoadConst(1), Yield, PopTop,
                 LoadConst(2), Yield, PopTop, LoadConst(3), Return]);
        generator.name = String::from("gen");
        generator.is_generator = true;
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let sum_prog = code(0, 0, Vec::new(), vec![String::from("sum"), String::from("gen")], 0,
            vec![LoadGlobal(0), LoadGlobal(1), Call(0), Call(1), Return]);
        let total = run(&sum_prog, core::slice::from_ref(&generator), &[], &mut model).unwrap();
        assert_eq!(total.as_fixnum(), Some(6));
        let list_prog = code(0, 0, Vec::new(), vec![String::from("list"), String::from("gen")], 0,
            vec![LoadGlobal(0), LoadGlobal(1), Call(0), Call(1), Return]);
        let list = run(&list_prog, core::slice::from_ref(&generator), &[], &mut model).unwrap();
        assert_eq!(model.repr(list), "[1, 2, 3]");
    }

    #[test]
    fn min_max_take_a_single_iterable_or_varargs() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let list_call = |name: &str| {
            code(0, 0, vec![Const::Int(3), Const::Int(1), Const::Int(4), Const::Int(5)],
                vec![String::from(name)], 0,
                vec![LoadGlobal(0), LoadConst(0), LoadConst(1), LoadConst(2), LoadConst(1),
                     LoadConst(3), BuildList(5), Call(1), Return])
        };
        assert_eq!(run(&list_call("max"), &[], &[], &mut model).unwrap().as_fixnum(), Some(5));
        assert_eq!(run(&list_call("min"), &[], &[], &mut model).unwrap().as_fixnum(), Some(1));
        let varargs = code(0, 0, vec![Const::Int(3), Const::Int(5), Const::Int(1)],
            vec![String::from("max")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), LoadConst(2), Call(3), Return]);
        assert_eq!(run(&varargs, &[], &[], &mut model).unwrap().as_fixnum(), Some(5));
    }

    #[test]
    fn builtin_keyword_args_sorted_and_dict() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let sorted_rev = code(0, 0,
            vec![Const::Int(3), Const::Int(1), Const::Int(2),
                 Const::KwNames(vec![String::from("reverse")]), Const::Bool(true)],
            vec![String::from("sorted")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), LoadConst(2), BuildList(3),
                 LoadConst(4), CallKw { argc: 1, kwnames: 3 }, Return]);
        let result = run(&sorted_rev, &[], &[], &mut model).unwrap();
        assert_eq!(model.repr(result), "[3, 2, 1]");
        let dict_kw = code(0, 0,
            vec![Const::Int(1), Const::Int(2),
                 Const::KwNames(vec![String::from("a"), String::from("b")])],
            vec![String::from("dict")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), CallKw { argc: 0, kwnames: 2 }, Return]);
        let d = run(&dict_kw, &[], &[], &mut model).unwrap();
        assert_eq!(model.repr(d), "{'a': 1, 'b': 2}");
    }

    #[test]
    fn unpack_sequence_assigns_in_order() {
        use Op::*;
        let code = code(
            2,
            0,
            vec![Const::Int(10), Const::Int(20)],
            Vec::new(),
            0,
            vec![
                LoadConst(0),
                LoadConst(1),
                BuildTuple(2),
                UnpackSequence(2),
                StoreFast(0),
                StoreFast(1),
                LoadFast(0),
                LoadFast(1),
                Binary(BinOp::Sub),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model).unwrap().as_fixnum(), Some(-10));
    }

    #[test]
    fn unpack_sequence_length_mismatch_is_value_error() {
        use Op::*;
        let code = code(
            2,
            0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3)],
            Vec::new(),
            0,
            vec![
                LoadConst(0),
                LoadConst(1),
                LoadConst(2),
                BuildTuple(3),
                UnpackSequence(2),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::ValueError));
    }

    #[test]
    fn list_comprehension_ops() {
        use Op::*;
        let code = code(
            3,
            0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3), Const::Int(2)],
            Vec::new(),
            0,
            vec![
                BuildList(0), StoreFast(0),
                LoadConst(0), LoadConst(1), LoadConst(2), BuildList(3), GetIter, StoreFast(2),
                LoadFast(2), ForIter(17), StoreFast(1),
                LoadFast(0), LoadFast(1), LoadConst(3), Binary(BinOp::Mul), ListAppend,
                Jump(8),
                LoadFast(0), Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&code, &[], &[], &mut model).unwrap();
        assert_eq!(model.repr(result), "[2, 4, 6]");
    }

    #[test]
    fn set_comprehension_ops() {
        use Op::*;
        let code = code(
            3,
            0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3)],
            Vec::new(),
            0,
            vec![
                BuildSet(0), StoreFast(0),
                LoadConst(0), LoadConst(0), LoadConst(1), LoadConst(2), LoadConst(2), BuildList(5),
                GetIter, StoreFast(2),
                LoadFast(2), ForIter(17), StoreFast(1),
                LoadFast(0), LoadFast(1), SetAdd,
                Jump(10),
                LoadFast(0), Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&code, &[], &[], &mut model).unwrap();
        assert_eq!(model.repr(result), "{1, 2, 3}");
    }

    #[test]
    fn dict_comprehension_ops() {
        use Op::*;
        let code = code(
            3,
            0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3)],
            Vec::new(),
            0,
            vec![
                BuildDict(0), StoreFast(0),
                LoadConst(0), LoadConst(1), LoadConst(2), BuildList(3), GetIter, StoreFast(2),
                LoadFast(2), ForIter(18), StoreFast(1),
                LoadFast(0), LoadFast(1), LoadFast(1), LoadFast(1), Binary(BinOp::Mul), DictInsert,
                Jump(8),
                LoadFast(0), Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&code, &[], &[], &mut model).unwrap();
        assert_eq!(model.repr(result), "{1: 1, 2: 4, 3: 9}");
    }

    #[test]
    fn bool_is_an_int_subtype_in_arithmetic_and_comparison() {
        assert_eq!(
            bin(BinOp::Add, Value::TRUE, Value::fixnum(1).unwrap()).unwrap().as_fixnum(),
            Some(2)
        );
        assert_eq!(cmp(CmpOp::Eq, Value::fixnum(0).unwrap(), Value::FALSE), Ok(Value::TRUE));
        assert_eq!(cmp(CmpOp::Eq, Value::fixnum(1).unwrap(), Value::TRUE), Ok(Value::TRUE));
        assert_eq!(cmp(CmpOp::Eq, Value::NONE, Value::NONE), Ok(Value::TRUE));
        assert_eq!(cmp(CmpOp::Eq, Value::NONE, Value::fixnum(1).unwrap()), Ok(Value::FALSE));
        assert_eq!(bin(BinOp::Add, Value::NONE, Value::fixnum(1).unwrap()), Err(Trap::TypeError));
        assert_eq!(cmp(CmpOp::Lt, Value::NONE, Value::NONE), Err(Trap::TypeError));
    }

    #[test]
    fn floor_div_and_mod_match_python_signs() {
        let f = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(bin(BinOp::FloorDiv, f(7), f(2)).unwrap().as_fixnum(), Some(3));
        assert_eq!(bin(BinOp::FloorDiv, f(-7), f(2)).unwrap().as_fixnum(), Some(-4));
        assert_eq!(bin(BinOp::FloorDiv, f(7), f(-2)).unwrap().as_fixnum(), Some(-4));
        assert_eq!(bin(BinOp::FloorDiv, f(-7), f(-2)).unwrap().as_fixnum(), Some(3));
        assert_eq!(bin(BinOp::Mod, f(7), f(2)).unwrap().as_fixnum(), Some(1));
        assert_eq!(bin(BinOp::Mod, f(-7), f(2)).unwrap().as_fixnum(), Some(1));
        assert_eq!(bin(BinOp::Mod, f(7), f(-2)).unwrap().as_fixnum(), Some(-1));
        assert_eq!(bin(BinOp::Mod, f(-7), f(-2)).unwrap().as_fixnum(), Some(-1));
        assert_eq!(bin(BinOp::FloorDiv, f(5), f(0)), Err(Trap::ZeroDivisionError));
        assert_eq!(bin(BinOp::Mod, f(5), f(0)), Err(Trap::ZeroDivisionError));
    }

    #[test]
    fn float_arithmetic_matches_python() {
        let mut m = ObjectModel::new(Vec::new(), 16 * 1024);
        let fx = |n: i32| Value::fixnum(n).unwrap();
        let flt = |m: &mut ObjectModel, x: f64| m.new_float(x).unwrap();
        let six_over_two = binary(BinOp::TrueDiv, fx(6), fx(2), &mut m).unwrap();
        assert_eq!(m.float_value(six_over_two), Some(3.0));
        let three_five = flt(&mut m, 3.5);
        let mixed = binary(BinOp::Add, fx(2), three_five, &mut m).unwrap();
        assert_eq!(m.float_value(mixed), Some(5.5));
        let a = flt(&mut m, -7.5);
        let b = flt(&mut m, 2.0);
        let floordiv = binary(BinOp::FloorDiv, a, b, &mut m).unwrap();
        assert_eq!(m.float_value(floordiv), Some(-4.0));
        let modulo = binary(BinOp::Mod, a, b, &mut m).unwrap();
        assert_eq!(m.float_value(modulo), Some(0.5));
        let zero = flt(&mut m, 0.0);
        let one = flt(&mut m, 1.0);
        assert_eq!(binary(BinOp::TrueDiv, one, zero, &mut m), Err(Trap::ZeroDivisionError));
        assert_eq!(binary(BinOp::Mod, one, zero, &mut m), Err(Trap::ZeroDivisionError));
        assert_eq!(binary(BinOp::BitAnd, one, fx(2), &mut m), Err(Trap::TypeError));
        let neg = unary(UnaryOp::Neg, three_five, &mut m).unwrap();
        assert_eq!(m.float_value(neg), Some(-3.5));
        assert_eq!(unary(UnaryOp::Invert, three_five, &mut m), Err(Trap::TypeError));
    }

    #[test]
    fn exponentiation_matches_python() {
        let mut m = ObjectModel::new(Vec::new(), 16 * 1024);
        let fx = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(binary(BinOp::Pow, fx(2), fx(10), &mut m).unwrap().as_fixnum(), Some(1024));
        assert_eq!(binary(BinOp::Pow, fx(2), fx(0), &mut m).unwrap().as_fixnum(), Some(1));
        let half = binary(BinOp::Pow, fx(2), fx(-1), &mut m).unwrap();
        assert_eq!(m.float_value(half), Some(0.5));
        let nine = m.new_float(9.0).unwrap();
        let root = binary(BinOp::Pow, nine, m.new_float(0.5).unwrap(), &mut m).unwrap();
        assert_eq!(m.float_value(root), Some(3.0));
        assert_eq!(binary(BinOp::Pow, fx(0), fx(-1), &mut m), Err(Trap::ZeroDivisionError));
        let zero = m.new_float(0.0).unwrap();
        assert_eq!(binary(BinOp::Pow, zero, fx(-2), &mut m), Err(Trap::ZeroDivisionError));
    }

    #[cfg(feature = "complex")]
    #[test]
    fn complex_arithmetic_and_pow_divergence() {
        let mut m = ObjectModel::new(Vec::new(), 16 * 1024);
        let fx = |n: i32| Value::fixnum(n).unwrap();
        let a = m.new_complex(1.0, 2.0).unwrap();
        let b = m.new_complex(3.0, 4.0).unwrap();
        let prod = binary(BinOp::Mul, a, b, &mut m).unwrap();
        assert_eq!(m.complex_value(prod), Some((-5.0, 10.0)));
        let plus1 = binary(BinOp::Add, a, fx(1), &mut m).unwrap();
        assert_eq!(m.complex_value(plus1), Some((2.0, 2.0)));
        let squared = binary(BinOp::Pow, a, fx(2), &mut m).unwrap();
        assert_eq!(m.complex_value(squared), Some((-3.0, 4.0)));
        assert_eq!(binary(BinOp::FloorDiv, a, b, &mut m), Err(Trap::TypeError));
        assert_eq!(binary(BinOp::Mod, a, b, &mut m), Err(Trap::TypeError));
        assert_eq!(compare(CmpOp::Lt, a, b, &m), Err(Trap::TypeError));
        let zero = m.new_complex(0.0, 0.0).unwrap();
        assert_eq!(binary(BinOp::TrueDiv, a, zero, &mut m), Err(Trap::ZeroDivisionError));
        let root = float_pow(-4.0, 0.5, &mut m).unwrap();
        let (re, im) = m.complex_value(root).unwrap();
        assert!(re.abs() < 1e-9 && (im - 2.0).abs() < 1e-9, "sqrt(-4) ~= 2j, got ({re}+{im}j)");
        let three = m.new_complex(3.0, 0.0).unwrap();
        assert_eq!(compare(CmpOp::Eq, three, fx(3), &m), Ok(Value::TRUE));
    }

    #[test]
    fn integer_arithmetic_promotes_past_i128_to_bigint() {
        let mut m = ObjectModel::new(Vec::new(), 64 * 1024);
        let ten_40 = m.new_bigint(crate::bigint::BigInt::from_decimal_str(&("1".to_string() + &"0".repeat(40))).unwrap()).unwrap();
        assert!(m.is_bigint(ten_40));
        let squared = binary(BinOp::Mul, ten_40, ten_40, &mut m).unwrap();
        assert_eq!(m.display(squared), "1".to_string() + &"0".repeat(80));
        let zero = binary(BinOp::Sub, ten_40, ten_40, &mut m).unwrap();
        assert_eq!(zero.as_fixnum(), Some(0));
        let two = Value::fixnum(2).unwrap();
        let big_pow = binary(BinOp::Pow, two, Value::fixnum(130).unwrap(), &mut m).unwrap();
        assert_eq!(m.display(big_pow), "1361129467683753853853498429727072845824");
        assert_eq!(compare(CmpOp::Gt, squared, ten_40, &m), Ok(Value::TRUE));
        let neg = unary(UnaryOp::Neg, ten_40, &mut m).unwrap();
        assert_eq!(m.display(neg), "-1".to_string() + &"0".repeat(40));
    }

    #[test]
    fn float_comparison_matches_python_including_nan() {
        let m = &mut ObjectModel::new(Vec::new(), 16 * 1024);
        let fx = |n: i32| Value::fixnum(n).unwrap();
        let one_five = m.new_float(1.5).unwrap();
        let two_f = m.new_float(2.0).unwrap();
        assert_eq!(compare(CmpOp::Lt, fx(1), one_five, m), Ok(Value::TRUE));
        assert_eq!(compare(CmpOp::Eq, two_f, fx(2), m), Ok(Value::TRUE));
        assert_eq!(compare(CmpOp::Lt, two_f, fx(2), m), Ok(Value::FALSE));
        let nan = m.new_float(f64::NAN).unwrap();
        assert_eq!(compare(CmpOp::Eq, nan, nan, m), Ok(Value::FALSE));
        assert_eq!(compare(CmpOp::Ne, nan, nan, m), Ok(Value::TRUE));
        assert_eq!(compare(CmpOp::Lt, nan, fx(1), m), Ok(Value::FALSE));
    }

    #[test]
    fn reading_an_unbound_local_traps() {
        use Op::*;
        let code = code(1, 0, Vec::new(), Vec::new(), 0, vec![LoadFast(0), Return]);
        let mut model = no_objects();
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::UnboundLocal));
    }

    #[test]
    fn obj_attr_runs_through_the_interpreter() {
        use Op::*;
        let mut model = ObjectModel::new(vec![PyType::with_slots("Point", &["x", "y"])], 4096);
        let obj = model
            .new_instance(0, &[Value::fixnum(7).unwrap(), Value::fixnum(9).unwrap()])
            .unwrap();
        let code = code(
            1,
            1,
            Vec::new(),
            vec![String::from("x")],
            1,
            vec![LoadFast(0), LoadAttr { name: 0, cache: 0 }, Return],
        );
        let result = run(&code, &[], &[obj], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(7));
    }

    #[test]
    fn attr_access_in_a_loop_exercises_the_inline_cache() {
        use Op::*;
        let mut model = ObjectModel::new(vec![PyType::with_slots("Point", &["x"])], 4096);
        let obj = model.new_instance(0, &[Value::fixnum(7).unwrap()]).unwrap();
        let ops = vec![
            LoadConst(0),
            StoreFast(1),
            LoadConst(0),
            StoreFast(2),
            LoadFast(2),
            LoadConst(2),
            Compare(CmpOp::Lt),
            PopJumpIfFalse(18),
            LoadFast(1),
            LoadFast(0),
            LoadAttr { name: 0, cache: 0 },
            Binary(BinOp::Add),
            StoreFast(1),
            LoadFast(2),
            LoadConst(1),
            Binary(BinOp::Add),
            StoreFast(2),
            Jump(4),
            LoadFast(1),
            Return,
        ];
        let code = code(
            3,
            1,
            vec![Const::Int(0), Const::Int(1), Const::Int(3)],
            vec![String::from("x")],
            1,
            ops,
        );
        let result = run(&code, &[], &[obj], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(21));
    }

    #[test]
    fn bitwise_and_shift_ops() {
        let f = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(bin(BinOp::BitAnd, f(12), f(10)).unwrap().as_fixnum(), Some(8));
        assert_eq!(bin(BinOp::BitOr, f(12), f(10)).unwrap().as_fixnum(), Some(14));
        assert_eq!(bin(BinOp::BitXor, f(12), f(10)).unwrap().as_fixnum(), Some(6));
        assert_eq!(bin(BinOp::LShift, f(1), f(10)).unwrap().as_fixnum(), Some(1024));
        assert_eq!(bin(BinOp::RShift, f(-8), f(1)).unwrap().as_fixnum(), Some(-4));
        assert_eq!(bin(BinOp::RShift, f(7), f(1)).unwrap().as_fixnum(), Some(3));
        assert_eq!(bin(BinOp::BitOr, Value::TRUE, f(2)).unwrap().as_fixnum(), Some(3));
        assert_eq!(bin(BinOp::LShift, f(1), f(-1)), Err(Trap::ValueError));
        assert_eq!(bin(BinOp::RShift, f(1), f(-1)), Err(Trap::ValueError));
        assert_eq!(bin(BinOp::LShift, f(1), f(40)).unwrap().as_fixnum(), None);
        let mut m = ObjectModel::new(Vec::new(), 16 * 1024);
        let big_shift = binary(BinOp::LShift, f(1), f(200), &mut m).unwrap();
        assert_eq!(m.display(big_shift), "1606938044258990275541962092341162602522202993782792835301376");
    }

    #[test]
    fn unary_ops() {
        let f = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(un(UnaryOp::Neg, f(5)).unwrap().as_fixnum(), Some(-5));
        assert_eq!(un(UnaryOp::Pos, f(5)).unwrap().as_fixnum(), Some(5));
        assert_eq!(un(UnaryOp::Invert, f(5)).unwrap().as_fixnum(), Some(-6));
        assert_eq!(un(UnaryOp::Invert, f(0)).unwrap().as_fixnum(), Some(-1));
        assert_eq!(un(UnaryOp::Neg, Value::TRUE).unwrap().as_fixnum(), Some(-1));
        assert_eq!(un(UnaryOp::Neg, Value::NONE), Err(Trap::TypeError));
    }

    #[test]
    fn intra_module_calls_and_recursion() {
        use Op::*;
        let mut model = no_objects();

        let mut square = code(1, 1, Vec::new(), Vec::new(), 0,
            vec![LoadFast(0), LoadFast(0), Binary(BinOp::Mul), Return]);
        square.name = String::from("square");
        let main = code(0, 0, vec![Const::Int(7)], vec![String::from("square")], 0,
            vec![LoadGlobal(0), LoadConst(0), Call(1), Return]);
        let result = run(&main, &[square], &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(49));

        let mut fact = code(1, 1, vec![Const::Int(1)], vec![String::from("fact")], 0, vec![
            LoadFast(0),
            LoadConst(0),
            Compare(CmpOp::Le),
            PopJumpIfFalse(6),
            LoadConst(0),
            Return,
            LoadFast(0),
            LoadGlobal(0),
            LoadFast(0),
            LoadConst(0),
            Binary(BinOp::Sub),
            Call(1),
            Binary(BinOp::Mul),
            Return,
        ]);
        fact.name = String::from("fact");
        let r = run(&fact, core::slice::from_ref(&fact), &[Value::fixnum(5).unwrap()], &mut model).unwrap();
        assert_eq!(r.as_fixnum(), Some(120));

        let mut loop_fn = code(0, 0, Vec::new(), vec![String::from("loop_fn")], 0,
            vec![LoadGlobal(0), Call(0), Return]);
        loop_fn.name = String::from("loop_fn");
        assert_eq!(
            run(&loop_fn, core::slice::from_ref(&loop_fn), &[], &mut model),
            Err(Trap::RecursionError)
        );
    }

    #[test]
    fn builtins_and_str() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 4096);

        let abs_prog = code(0, 0, vec![Const::Int(-5)], vec![String::from("abs")], 0,
            vec![LoadGlobal(0), LoadConst(0), Call(1), Return]);
        assert_eq!(run(&abs_prog, &[], &[], &mut model).unwrap().as_fixnum(), Some(5));

        let consts = vec![Const::Int(3), Const::Int(5), Const::Int(1)];
        let min_prog = code(0, 0, consts.clone(), vec![String::from("min")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), LoadConst(2), Call(3), Return]);
        assert_eq!(run(&min_prog, &[], &[], &mut model).unwrap().as_fixnum(), Some(1));
        let max_prog = code(0, 0, consts, vec![String::from("max")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), LoadConst(2), Call(3), Return]);
        assert_eq!(run(&max_prog, &[], &[], &mut model).unwrap().as_fixnum(), Some(5));

        let len_prog = code(0, 0, vec![Const::Str(String::from("hello"))],
            vec![String::from("len")], 0, vec![LoadGlobal(0), LoadConst(0), Call(1), Return]);
        assert_eq!(run(&len_prog, &[], &[], &mut model).unwrap().as_fixnum(), Some(5));

        let bad = code(0, 0, Vec::new(), vec![String::from("nope")], 0, vec![LoadGlobal(0), Return]);
        assert_eq!(run(&bad, &[], &[], &mut model), Err(Trap::NameError));
    }

    #[test]
    fn strings_through_the_interpreter() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 4096);

        let cat = code(0, 0, vec![Const::Str(String::from("ab")), Const::Str(String::from("cd"))],
            vec![String::from("len")], 0,
            vec![LoadGlobal(0), LoadConst(0), LoadConst(1), Binary(BinOp::Add), Call(1), Return]);
        assert_eq!(run(&cat, &[], &[], &mut model).unwrap().as_fixnum(), Some(4));

        let cmp = code(0, 0,
            vec![Const::Str(String::from("a")), Const::Str(String::from("b")), Const::Int(1), Const::Int(0)],
            Vec::new(), 0,
            vec![
                LoadConst(0), LoadConst(1), Compare(CmpOp::Lt), PopJumpIfFalse(6),
                LoadConst(2), Return,
                LoadConst(3), Return,
            ]);
        assert_eq!(run(&cmp, &[], &[], &mut model).unwrap().as_fixnum(), Some(1));

        let truthy = code(0, 0,
            vec![Const::Str(String::from("")), Const::Int(1), Const::Int(0)],
            Vec::new(), 0,
            vec![
                LoadConst(0), PopJumpIfFalse(4),
                LoadConst(1), Return,
                LoadConst(2), Return,
            ]);
        assert_eq!(run(&truthy, &[], &[], &mut model).unwrap().as_fixnum(), Some(0));
    }

    #[test]
    fn str_subscript_through_the_interpreter() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let index_prog = |i: i64| {
            code(
                0,
                0,
                vec![Const::Str(String::from("abc")), Const::Int(i)],
                Vec::new(),
                1,
                vec![LoadConst(0), LoadConst(1), Subscript { cache: 0 }, Return],
            )
        };
        let b = run(&index_prog(1), &[], &[], &mut model).unwrap();
        assert_eq!(model.str_value(b), Some("b"));
        let c = run(&index_prog(-1), &[], &[], &mut model).unwrap();
        assert_eq!(model.str_value(c), Some("c"));
        assert_eq!(run(&index_prog(5), &[], &[], &mut model), Err(Trap::IndexError));
    }

    #[test]
    fn str_method_call_through_the_interpreter() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let prog = code(
            0,
            0,
            vec![Const::Str(String::from("abc"))],
            vec![String::from("upper")],
            1,
            vec![LoadConst(0), LoadAttr { name: 0, cache: 0 }, Call(0), Return],
        );
        let r = run(&prog, &[], &[], &mut model).unwrap();
        assert_eq!(model.str_value(r), Some("ABC"));
    }

    #[test]
    fn str_slice_through_the_interpreter() {
        use Op::*;
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let prog = code(
            0,
            0,
            vec![
                Const::Str(String::from("hello")),
                Const::Int(1),
                Const::Int(4),
                Const::None,
            ],
            Vec::new(),
            1,
            vec![
                LoadConst(0),
                LoadConst(1),
                LoadConst(2),
                LoadConst(3),
                BuildSlice,
                Subscript { cache: 0 },
                Return,
            ],
        );
        let r = run(&prog, &[], &[], &mut model).unwrap();
        assert_eq!(model.str_value(r), Some("ell"));
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn the_shared_gc_scans_a_frame_by_tag() {
        let mut model = ObjectModel::new(vec![PyType::with_slots("Point", &["x"])], 4096);

        let _garbage = model.new_instance(0, &[Value::fixnum(111).unwrap()]).unwrap();
        let live = model.new_instance(0, &[Value::fixnum(7).unwrap()]).unwrap();
        let live_addr_before = live.as_ref().unwrap();

        let mut frame = Frame::new(2, 0);
        frame.locals[0] = live;
        frame.locals[1] = Value::fixnum(42).unwrap();

        model.heap_mut().collect(|visit| frame.trace(visit));

        assert_eq!(frame.locals[1], Value::fixnum(42).unwrap());

        let relocated = frame.locals[0];
        assert!(relocated.is_pointer());
        let new_addr = relocated.as_ref().unwrap();
        assert_ne!(new_addr, live_addr_before, "the live object was compacted down");

        let mut cache = InlineCache::empty();
        assert_eq!(
            model.getattr(relocated, "x", &mut cache).unwrap().as_fixnum(),
            Some(7)
        );
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn the_gc_traces_a_frames_in_flight_exception() {
        let mut model = ObjectModel::new(vec![PyType::with_slots("Err", &["code"])], 4096);
        let _garbage = model.new_instance(0, &[Value::fixnum(1).unwrap()]).unwrap();
        let exc = model.new_instance(0, &[Value::fixnum(9).unwrap()]).unwrap();
        let exc_addr_before = exc.as_ref().unwrap();

        let mut frame = Frame::new(0, 0);
        frame.active_exception = Some(exc);

        model.heap_mut().collect(|visit| frame.trace(visit));

        let relocated = frame
            .active_exception
            .expect("the in-flight exception survives the collection");
        assert!(relocated.is_pointer());
        assert_ne!(
            relocated.as_ref().unwrap(),
            exc_addr_before,
            "the exception was compacted down"
        );
        let mut cache = InlineCache::empty();
        assert_eq!(
            model.getattr(relocated, "code", &mut cache).unwrap().as_fixnum(),
            Some(9)
        );
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn the_gc_traces_every_frame_on_an_explicit_call_chain() {
        let mut model = ObjectModel::new(vec![PyType::with_slots("Box", &["v"])], 4096);
        let mut frames: Vec<Frame> = Vec::new();
        let mut before: Vec<lamella_gc::Ref> = Vec::new();
        for i in 0..3i32 {
            let _garbage = model.new_instance(0, &[Value::fixnum(100 + i).unwrap()]).unwrap();
            let live = model.new_instance(0, &[Value::fixnum(i).unwrap()]).unwrap();
            before.push(live.as_ref().unwrap());
            let mut frame = Frame::new(1, 0);
            frame.locals[0] = live;
            frames.push(frame);
        }
        model.heap_mut().collect(|visit| {
            for frame in frames.iter_mut() {
                frame.trace(visit);
            }
        });
        let mut cache = InlineCache::empty();
        for (i, frame) in frames.iter().enumerate() {
            let obj = frame.locals[0];
            assert!(obj.is_pointer(), "frame {i}'s object survived");
            assert_ne!(obj.as_ref().unwrap(), before[i], "frame {i}'s object was compacted down");
            assert_eq!(model.getattr(obj, "v", &mut cache).unwrap().as_fixnum(), Some(i as i32));
        }
    }

    #[test]
    fn a_no_alloc_driver_runs_with_no_managed_heap() {
        let code = fib_code();
        let mut model = ObjectModel::new(Vec::new(), 0);
        let result = run(&code, &[], &[Value::fixnum(10).unwrap()], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(55));
    }

    #[test]
    fn allocating_on_the_no_heap_tier_fails_loud() {
        use Op::*;
        let code = code(0, 0, vec![Const::Str(String::from("x"))], Vec::new(), 0,
            vec![LoadConst(0), Return]);
        let mut model = ObjectModel::new(Vec::new(), 0);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::OutOfMemory));
    }
}
