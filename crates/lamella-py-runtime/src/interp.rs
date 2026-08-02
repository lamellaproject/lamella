//! The bytecode interpreter.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use lamella_gc::Ref;
use lamella_py_bytecode::{BinOp, Bundle, CmpOp, CodeObject, Const, ExcEntry, Op, UnaryOp};

use crate::bigint::BigInt;
use crate::object::{DescriptorRead, DictViewKind, InlineCache, ObjectModel};
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
    /// Which module this frame's code, globals and sibling-function refs resolve against -- the
    /// callee's HOME module, not the caller's.
    ///
    /// It rides the FRAME rather than the driver loop, which is what lets one frame stack hold
    /// frames from several modules: the driver installs the top frame's module before each op, so a
    /// call into an imported module is a frame push instead of a nested driver loop the collector's
    /// safe point cannot see past.
    module: u16,
    /// The instruction pointer: the index into the code's `ops` of the next op to run.
    ip: usize,
    locals: Vec<Value>,
    stack: Vec<Value>,
    /// This activation's inline-cache slots -- one per cacheable site, sized by the code's
    /// `cache_count` and indexed by each [`Op::LoadAttr`]'s `cache` field. The bytecode is
    /// immutable (flash-resident under XIP); the caches are the per-activation RAM side array
    /// (`lamella_py_bytecode` module note), so they belong to the frame, not the code.
    caches: Vec<InlineCache>,
    /// The exceptions being handled in this frame, innermost LAST -- pushed on entry to a handler,
    /// popped by `PopExcept`. A STACK rather than one slot because a handler can raise and catch
    /// another exception, and when that inner handling finishes the OUTER exception is being handled
    /// again: it is what a bare `raise` re-raises and what the next raise records as its
    /// `__context__`. Every entry is a GC root (traced by [`Frame::trace`]), so an allocation inside
    /// a handler body cannot free an in-flight exception out from under it.
    ///
    /// Each entry remembers the PROTECTED RANGE of the try block whose handler pushed it, because a
    /// handler body an exception escapes never reaches its own `PopExcept` -- see
    /// [`Frame::enter_handler`], the only thing that pushes.
    handled: Vec<Handled>,
    /// The deref array for closures: `[0 .. cellvars.len())` are this frame's OWN cells (locals a
    /// nested function captures), then the captured cells the closure carried in. `LoadDeref` /
    /// `StoreDeref` / `LoadClosure` index it; empty for a function with no cell/free variables. Each
    /// slot is a `Cell` (a heap object), so all are GC roots (traced by [`Frame::trace`]).
    derefs: Vec<Value>,
    /// The active class-body namespace dict while executing a `class` body (`SetupClassNamespace`
    /// sets it, `StoreName`/`LoadName` target it, `BuildClass` consumes it), else `None`. A GC root
    /// while set (traced by [`Frame::trace`]) so building the namespace cannot free it.
    class_namespace: Option<Value>,
    /// Set only on an imported module's BODY frame: the rest of the `import` that pushed it, held
    /// until the body finishes -- the module object to hand the importer, and the name to un-cache if
    /// the body raises instead. A GC root while set (traced by [`Frame::trace`]): the module object
    /// is reachable from nowhere else until the body completes and the importer binds it.
    finishes_import: Option<ImportCompletion>,
    /// Whether this frame is mid `yield from` delegation (an `Op::YieldFrom` episode). While set, a
    /// resume of this generator re-runs YieldFrom (its ip was rewound to the op) with the sent value
    /// on TOS above the sub-iterator; false marks the first entry (send `None`) and each completed
    /// episode. Reset on reuse. Not a GC root -- the sub it refers to is on the eval stack.
    yield_from_active: bool,
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
            module: 0,
            ip: 0,
            locals,
            stack: Vec::new(),
            caches,
            handled: Vec::new(),
            derefs: Vec::new(),
            class_namespace: None,
            finishes_import: None,
            yield_from_active: false,
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
        self.handled.clear();
        self.derefs.clear();
        self.class_namespace = None;
        self.finishes_import = None;
        self.yield_from_active = false;
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

    /// Enters `handler` for `exception`: truncates the operand stack, records the exception as the
    /// one this frame is handling, and jumps to the handler.
    ///
    /// The recording is where the block structure matters. A handler body is left in one of two ways.
    /// Normally it runs to its `PopExcept`. But when an exception ESCAPES the body, that `PopExcept`
    /// is never reached, and the entry it would have popped has to go somewhere. It goes here: an
    /// entry whose try block is INSIDE the one now handling belongs to a handler body being left, so
    /// it stops being handled. Nesting of protected ranges IS the lexical nesting of try blocks, so
    /// the exception table already carries what is needed.
    ///
    /// The distinction this draws is the one a single slot could not: an exception escaping a handler
    /// body outward (drop the inner entry) versus a NEW try block written inside a handler body (keep
    /// the outer entry -- its exception is still being handled, and is what a bare raise re-raises).
    fn enter_handler(&mut self, exception: Value, handler: ExcEntry) {
        self.stack.truncate(handler.depth as usize);
        self.handled.retain(|h| !(h.start >= handler.start && h.end <= handler.end));
        self.handled.push(Handled { exception, start: handler.start, end: handler.end });
        self.ip = handler.target as usize;
    }

    /// The exception this frame is handling -- the innermost still-open handler's -- or `None`.
    fn handling(&self) -> Option<Value> {
        self.handled.last().map(|h| h.exception)
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
        for entry in self.handled.iter_mut() {
            Value::trace_slot(&mut entry.exception, visit);
        }
        if let Some(namespace) = self.class_namespace.as_mut() {
            Value::trace_slot(namespace, visit);
        }
        if let Some(pending) = self.finishes_import.as_mut() {
            Value::trace_slot(&mut pending.module_obj, visit);
        }
    }

    /// The bytes this frame's buffers hold -- its locals, evaluation stack, inline caches, open
    /// handlers and closure cells. CAPACITY rather than length, because a buffer that has grown and
    /// shrunk still owns what it reserved, and the arena it came from does not know the difference.
    ///
    /// Part of [`crate::object::ObjectModel::footprint`]; a suspended generator's frame and a pooled
    /// one both cost this much whether or not anything is stored in them.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.locals.capacity() * size_of::<Value>()
            + self.stack.capacity() * size_of::<Value>()
            + self.caches.capacity() * size_of::<InlineCache>()
            + self.handled.capacity() * size_of::<Handled>()
            + self.derefs.capacity() * size_of::<Value>()
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
        let outcome = call_value(method, &[rhs], functions, model, depth + 1)?;
        if !outcome.is_not_implemented() {
            return Ok(Some(outcome));
        }
    }
    if let Some(method) = model.find_dunder(rhs, reflected) {
        let outcome = call_value(method, &[lhs], functions, model, depth + 1)?;
        if !outcome.is_not_implemented() {
            return Ok(Some(outcome));
        }
    }
    Ok(None)
}

/// The full binary-operator dispatch: a user operator dunder (`__add__`/`__radd__`/...) wins, then the
/// interp-aware set / dict operators, then `str`/sequence operands, else numeric. Extracted from the
/// [`Op::Binary`] handler so a bare `Trap::TypeError` (no operation applies to these operand types) can
/// be turned into a descriptive `TypeError` at the call site, while a dunder that itself raised
/// (`Trap::Raised`, carrying its own exception) or any other trap propagates unchanged.
pub(crate) fn dispatch_binary(
    binop: BinOp,
    lhs: Value,
    rhs: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if let Some(value) = try_binop_dunder(binop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_set_binop_dyn(binop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_counter_binop_dyn(binop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_dict_binop_dyn(binop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_view_binop_dyn(binop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = model.py_binary(binop, lhs, rhs)? {
        Ok(value)
    } else {
        binary(binop, lhs, rhs, model)
    }
}

/// The dunder method name for an augmented assignment's in-place operator (`__iadd__`/...).
/// Unlike the plain operator there is no reflected form -- with the method missing, the whole
/// dispatch falls back to the plain binary protocol.
fn inplace_binop_dunder_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "__iadd__",
        BinOp::Sub => "__isub__",
        BinOp::Mul => "__imul__",
        BinOp::Mod => "__imod__",
        BinOp::FloorDiv => "__ifloordiv__",
        BinOp::BitAnd => "__iand__",
        BinOp::BitOr => "__ior__",
        BinOp::BitXor => "__ixor__",
        BinOp::LShift => "__ilshift__",
        BinOp::RShift => "__irshift__",
        BinOp::TrueDiv => "__itruediv__",
        BinOp::Pow => "__ipow__",
        BinOp::MatMul => "__imatmul__",
    }
}

/// The in-place dispatch for augmented assignment ([`Op::InplaceBinOp`]): a user class's in-place
/// dunder (`__iadd__`/`__ior__`/...) wins (a NotImplemented return falls back to the plain binary
/// protocol, like [`try_binop_dunder`]); then a built-in MUTABLE left operand applies the
/// operation in place and the result IS that same object, so aliases observe the mutation:
/// `list += any-iterable` extends (a generator source too), `list *= int` repeats,
/// `dict |= dict-or-pairs` updates, `set OP= set|frozenset` rewrites the contents for `| & - ^`,
/// `bytearray += bytes-like` appends, `bytearray *= int` repeats. Everything else falls back to
/// [`dispatch_binary`] -- for immutables the plain operator result rebinds, which is CPython's
/// augmented semantics when no in-place slot applies.
fn dispatch_inplace_binary(
    binop: BinOp,
    lhs: Value,
    rhs: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if let Some(method) = model.find_dunder(lhs, inplace_binop_dunder_name(binop)) {
        let outcome = call_value(method, &[rhs], functions, model, depth + 1)?;
        if !outcome.is_not_implemented() {
            return Ok(outcome);
        }
    }
    if model.is_list(lhs) {
        match binop {
            BinOp::Add => {
                let items = crate::builtins::collect_iterable(model, &[rhs], functions, depth)?;
                model.list_extend_in_place(lhs, items)?;
                return Ok(lhs);
            }
            BinOp::Mul => {
                if let Some(count) = rhs.as_int() {
                    model.list_repeat_in_place(lhs, count)?;
                    return Ok(lhs);
                }
            }
            _ => {}
        }
    }
    if model.is_counter(lhs)
        && model.dict_value(rhs).is_some()
        && matches!(binop, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr)
    {
        let entries = counter_op_entries(binop, lhs, rhs, functions, model, depth)?;
        model.dict_replace_entries(lhs, entries)?;
        return Ok(lhs);
    }
    if model.is_dict(lhs) && binop == BinOp::BitOr {
        let pairs = if let Some(entries) = model.dict_entries(rhs) {
            entries
        } else {
            let items = crate::builtins::collect_iterable(model, &[rhs], functions, depth)?;
            let mut kv = Vec::with_capacity(items.len());
            for item in items {
                let parts = model.unpack_sequence(item, 2)?;
                kv.push((parts[0], parts[1]));
            }
            kv
        };
        for (key, value) in pairs {
            model.py_setitem_dyn(lhs, key, value, functions, depth)?;
        }
        return Ok(lhs);
    }
    if model.is_set(lhs)
        && (model.is_set(rhs) || model.is_frozenset(rhs))
        && matches!(binop, BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor)
    {
        let a_elems = model.set_value(lhs).ok_or(Trap::TypeError)?.clone();
        let b_elems = model.set_value(rhs).ok_or(Trap::TypeError)?.clone();
        let result = match binop {
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
        model.set_replace_elems(lhs, result)?;
        return Ok(lhs);
    }
    if model.is_bytearray(lhs) {
        match binop {
            BinOp::Add => {
                if let Some(data) = model.bytes_value(rhs).map(<[u8]>::to_vec) {
                    model.bytearray_extend_in_place(lhs, data)?;
                    return Ok(lhs);
                }
            }
            BinOp::Mul => {
                if let Some(count) = rhs.as_int() {
                    model.bytearray_repeat_in_place(lhs, count)?;
                    return Ok(lhs);
                }
            }
            _ => {}
        }
    }
    dispatch_binary(binop, lhs, rhs, functions, model, depth)
}

/// The full comparison dispatch for a non-identity comparison: a user comparison dunder
/// (`__eq__`/`__lt__`/... + reflected) wins, then the interp-aware set/dict `==`, else the built-in
/// value comparison. Extracted from the [`Op::Compare`] handler so a bare `Trap::TypeError` (an
/// ordering comparison on values that do not support it) can become a descriptive `TypeError`, while a
/// dunder that itself raised (`Trap::Raised`) or any other trap propagates unchanged. `is`/`is not` are
/// object identity and never reach here (the handler resolves them first).
fn dispatch_compare(
    cmpop: CmpOp,
    lhs: Value,
    rhs: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if let Some(value) = try_compare_dunder(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_view_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_set_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_odict_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_deque_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_seq_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else if let Some(value) = try_dict_compare_dyn(cmpop, lhs, rhs, functions, model, depth)? {
        Ok(value)
    } else {
        match model.py_compare(cmpop, lhs, rhs)? {
            Some(value) => Ok(value),
            None => compare(cmpop, lhs, rhs, model),
        }
    }
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
        if !outcome.is_not_implemented() {
            return Ok(Some(Value::from_bool(model.py_truthy(outcome)?.unwrap_or(false))));
        }
    }
    if matches!(op, CmpOp::Ne) {
        if let Some(method) = model.find_dunder(receiver, "__eq__") {
            let outcome = call_value(method, &[other], functions, model, depth + 1)?;
            if !outcome.is_not_implemented() {
                return Ok(Some(Value::from_bool(!model.py_truthy(outcome)?.unwrap_or(false))));
            }
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
    if value.is_not_implemented() {
        let message = "NotImplemented should not be used in a boolean context";
        return Err(model.with_message(Trap::TypeError, message));
    }
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

/// The entries of `lhs OP rhs` for the Counter operators (`+ - & |`): lhs keys in order, then
/// rhs-only keys; counts combine per the op (sum / difference / min / max) and results <= 0 are
/// DROPPED (CPython's keep-positive rule -- note `subtract()` the METHOD keeps them, only the
/// operators strip). Counts must be ints; a non-int is the bare TypeError. Key matching is
/// interp-aware (`elem_eq`), so a user-`__eq__` key folds correctly.
fn counter_op_entries(
    op: BinOp,
    lhs: Value,
    rhs: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Vec<(Value, Value)>, Trap> {
    let a = model.dict_entries(lhs).unwrap_or_default();
    let b = model.dict_entries(rhs).unwrap_or_default();
    let mut out: Vec<(Value, i128)> = Vec::new();
    for &(key, count) in &a {
        let x = model.as_i128(count).ok_or(Trap::TypeError)?;
        let mut y = 0i128;
        for &(other_key, other_count) in &b {
            if elem_eq(key, other_key, functions, model, depth)? {
                y = model.as_i128(other_count).ok_or(Trap::TypeError)?;
                break;
            }
        }
        let n = match op {
            BinOp::Add => x + y,
            BinOp::Sub => x - y,
            BinOp::BitAnd => x.min(y),
            BinOp::BitOr => x.max(y),
            _ => return Err(Trap::TypeError),
        };
        if n > 0 {
            out.push((key, n));
        }
    }
    for &(key, count) in &b {
        let mut in_lhs = false;
        for &(lhs_key, _) in &a {
            if elem_eq(key, lhs_key, functions, model, depth)? {
                in_lhs = true;
                break;
            }
        }
        if in_lhs {
            continue;
        }
        let y = model.as_i128(count).ok_or(Trap::TypeError)?;
        let n = match op {
            BinOp::Add | BinOp::BitOr => y,
            BinOp::Sub => -y,
            BinOp::BitAnd => 0,
            _ => return Err(Trap::TypeError),
        };
        if n > 0 {
            out.push((key, n));
        }
    }
    let mut entries = Vec::with_capacity(out.len());
    for (key, n) in out {
        let count = model.int_from_i128(n)?;
        entries.push((key, count));
    }
    Ok(entries)
}

/// The Counter operators (`c1 + c2`, `- & |`) -- BOTH operands must be Counters (CPython's
/// isinstance check: `c + dict` is the unsupported-operand TypeError, while `c | dict` falls
/// through here to the plain dict merge, both matching CPython). The result is a NEW Counter.
fn try_counter_binop_dyn(
    op: BinOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::BitAnd | BinOp::BitOr)
        || !model.is_counter(a)
        || !model.is_counter(b)
    {
        return Ok(None);
    }
    let entries = counter_op_entries(op, a, b, functions, model, depth)?;
    Ok(Some(model.new_counter(entries)?))
}

/// The deque comparisons: two deques compare like lists -- `==`/`!=` elementwise, `< <= > >=`
/// lexicographic (the first unequal pair decides via the full comparison dispatch; a strict
/// prefix is less). `None` unless BOTH operands are deques: a deque never equals a list
/// (CPython), and an ordering against a non-deque is the default TypeError.
fn try_deque_compare_dyn(
    op: CmpOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if !model.is_deque(a) || !model.is_deque(b) {
        return Ok(None);
    }
    if matches!(op, CmpOp::Is | CmpOp::IsNot) {
        return Ok(None);
    }
    let a_elems = model.deque_elems(a).cloned().unwrap_or_default();
    let b_elems = model.deque_elems(b).cloned().unwrap_or_default();
    let mut split = None;
    for (i, (&x, &y)) in a_elems.iter().zip(&b_elems).enumerate() {
        if !elem_eq(x, y, functions, model, depth)? {
            split = Some(i);
            break;
        }
    }
    let value = match op {
        CmpOp::Eq => split.is_none() && a_elems.len() == b_elems.len(),
        CmpOp::Ne => split.is_some() || a_elems.len() != b_elems.len(),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => match split {
            Some(i) => {
                let decided = dispatch_compare(op, a_elems[i], b_elems[i], functions, model, depth)?;
                return Ok(Some(decided));
            }
            None => match op {
                CmpOp::Lt => a_elems.len() < b_elems.len(),
                CmpOp::Le => a_elems.len() <= b_elems.len(),
                CmpOp::Gt => a_elems.len() > b_elems.len(),
                CmpOp::Ge => a_elems.len() >= b_elems.len(),
                _ => unreachable!("guarded by the outer match arm"),
            },
        },
        CmpOp::Is | CmpOp::IsNot => unreachable!("returned None above"),
    };
    Ok(Some(Value::from_bool(value)))
}

/// `list == list` / `tuple == tuple` (and ordering) comparing ELEMENT BY ELEMENT through the full
/// dispatch, so an element's own `__eq__` decides. Without this the comparison falls back to a
/// native value check that cannot call a user dunder, and `[obj] == [obj2]` is False for objects
/// their own `__eq__` calls equal.
///
/// A list never equals a tuple, so both operands must be the same kind -- but a namedtuple IS a
/// tuple, which is why the tuple side tests "not a list, and sequence-shaped" rather than a
/// concrete type.
fn try_seq_compare_dyn(
    op: CmpOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if matches!(op, CmpOp::Is | CmpOp::IsNot) {
        return Ok(None);
    }
    let both_lists = model.is_list(a) && model.is_list(b);
    let both_tuples = !model.is_list(a)
        && !model.is_list(b)
        && model.seq_value(a).is_some()
        && model.seq_value(b).is_some();
    if !both_lists && !both_tuples {
        return Ok(None);
    }
    let a_elems = model.seq_value(a).cloned().unwrap_or_default();
    let b_elems = model.seq_value(b).cloned().unwrap_or_default();
    let mut split = None;
    for (i, (&x, &y)) in a_elems.iter().zip(&b_elems).enumerate() {
        if !elem_eq(x, y, functions, model, depth)? {
            split = Some(i);
            break;
        }
    }
    let value = match op {
        CmpOp::Eq => split.is_none() && a_elems.len() == b_elems.len(),
        CmpOp::Ne => split.is_some() || a_elems.len() != b_elems.len(),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => match split {
            Some(i) => {
                let decided = dispatch_compare(op, a_elems[i], b_elems[i], functions, model, depth)?;
                return Ok(Some(decided));
            }
            None => match op {
                CmpOp::Lt => a_elems.len() < b_elems.len(),
                CmpOp::Le => a_elems.len() <= b_elems.len(),
                CmpOp::Gt => a_elems.len() > b_elems.len(),
                CmpOp::Ge => a_elems.len() >= b_elems.len(),
                _ => unreachable!("guarded by the outer match arm"),
            },
        },
        CmpOp::Is | CmpOp::IsNot => unreachable!("returned None above"),
    };
    Ok(Some(Value::from_bool(value)))
}

/// The ORDER-SENSITIVE OrderedDict equality: `od1 == od2` compares entries pairwise IN ORDER --
/// only when BOTH operands are OrderedDicts (CPython); an OrderedDict against a plain dict (or
/// another subtype) falls through to the order-insensitive dict equality. Orderings stay
/// TypeErrors via the default machinery.
fn try_odict_compare_dyn(
    op: CmpOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if !matches!(op, CmpOp::Eq | CmpOp::Ne) || !model.is_ordereddict(a) || !model.is_ordereddict(b)
    {
        return Ok(None);
    }
    let a_entries = model.dict_entries(a).unwrap_or_default();
    let b_entries = model.dict_entries(b).unwrap_or_default();
    let mut equal = a_entries.len() == b_entries.len();
    if equal {
        for (&(a_key, a_value), &(b_key, b_value)) in a_entries.iter().zip(&b_entries) {
            if !elem_eq(a_key, b_key, functions, model, depth)?
                || !elem_eq(a_value, b_value, functions, model, depth)?
            {
                equal = false;
                break;
            }
        }
    }
    Ok(Some(Value::from_bool(if matches!(op, CmpOp::Ne) { !equal } else { equal })))
}

/// The elements of a SET-LIKE operand for the dict-view operators: a keys/items view materializes
/// its current projection, a set/frozenset gives its elements. `None` for anything else -- a
/// VALUES view included, since it has no set protocol.
fn set_like_elems(value: Value, model: &mut ObjectModel) -> Result<Option<Vec<Value>>, Trap> {
    match model.dict_view_kind(value) {
        Some(DictViewKind::Keys | DictViewKind::Items) => model.dict_view_elems(value),
        Some(DictViewKind::Values) => Ok(None),
        None => Ok(model.set_value(value).cloned()),
    }
}

/// The interp-aware dict-view set operators (`d.keys() | & - ^ other`, view on either side), or
/// `None` when neither operand is a keys/items view (a VALUES view has no set protocol and falls
/// through to the plain TypeError). The result is a plain `set`, as CPython's. The other operand
/// may be ANY iterable (CPython: `d.keys() & "ab"` works; a non-iterable raises its
/// `'X' object is not iterable`), deduped up front so a repeating iterable behaves as its set.
fn try_view_binop_dyn(
    op: BinOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if !matches!(op, BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor) {
        return Ok(None);
    }
    let a_view = matches!(model.dict_view_kind(a), Some(DictViewKind::Keys | DictViewKind::Items));
    let b_view = matches!(model.dict_view_kind(b), Some(DictViewKind::Keys | DictViewKind::Items));
    if !a_view && !b_view {
        return Ok(None);
    }
    let a_elems = match set_like_elems(a, model)? {
        Some(elems) => elems,
        None => {
            let items = crate::builtins::collect_iterable(model, &[a], functions, depth)?;
            dedup_elems(items, functions, model, depth)?
        }
    };
    let b_elems = match set_like_elems(b, model)? {
        Some(elems) => elems,
        None => {
            let items = crate::builtins::collect_iterable(model, &[b], functions, depth)?;
            dedup_elems(items, functions, model, depth)?
        }
    };
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
    Ok(Some(model.new_set(result)?))
}

/// The interp-aware dict-view comparisons: a keys/items view compares AS A SET with another
/// keys/items view or a set/frozenset -- `==`/`!=` set equality, `< <= > >=` (proper)
/// subset/superset. `==`/`!=` against a NON-set-like is not handled here (`None`), falling through
/// to identity, so `d.keys() == [..]` is False as in CPython; an ORDERING against a non-set-like
/// is a TypeError (the dispatch chokepoint renders CPython's message). A VALUES view has no set
/// protocol and is not handled at all (identity `==`, ordering TypeError).
fn try_view_compare_dyn(
    op: CmpOp,
    a: Value,
    b: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    let a_view = matches!(model.dict_view_kind(a), Some(DictViewKind::Keys | DictViewKind::Items));
    let b_view = matches!(model.dict_view_kind(b), Some(DictViewKind::Keys | DictViewKind::Items));
    if !a_view && !b_view {
        return Ok(None);
    }
    let value = match op {
        CmpOp::Eq | CmpOp::Ne => {
            let equal = match (set_like_elems(a, model)?, set_like_elems(b, model)?) {
                (Some(a_elems), Some(b_elems)) => {
                    a_elems.len() == b_elems.len()
                        && subset_dyn(&a_elems, &b_elems, functions, model, depth)?
                }
                _ => false,
            };
            if matches!(op, CmpOp::Ne) { !equal } else { equal }
        }
        CmpOp::Le | CmpOp::Ge | CmpOp::Lt | CmpOp::Gt => {
            let (Some(a_elems), Some(b_elems)) =
                (set_like_elems(a, model)?, set_like_elems(b, model)?)
            else {
                return Err(Trap::TypeError);
            };
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

/// Evaluates a unary `-`/`+`/`~` over an `int`/`bool` operand (Python int semantics:
/// `+x == x`, `-x`, `~x == -x - 1`); other types are a `TypeError`. The customizable
/// `__neg__`/`__pos__`/`__invert__` protocol composes with the broader object model.
pub(crate) fn unary(op: UnaryOp, v: Value, model: &mut ObjectModel) -> Result<Value, Trap> {
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
pub(crate) fn compare(op: CmpOp, a: Value, b: Value, model: &ObjectModel) -> Result<Value, Trap> {
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
    run_frames(code, functions, args, &[], model, false, 0, 0)
}

/// Runs the module body (the top-level statements). Its local bindings mirror into the module
/// globals as they happen, so a function reaches a top-level name (a class, a global) by
/// `LoadGlobal`. Run this before invoking module functions. This is the ENTRY module (module 0); a
/// managed module's body runs via `run_managed_module` with its own module id.
pub fn run_module(
    body: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    run_frames(body, functions, &[], &[], model, true, 0, 0)
}

/// Runs a compiled multi-module program (a [`Bundle`]): installs the bundle's importable managed
/// modules as the registry, then runs the entry module as the program (module 0). An `import name`
/// in ANY module then resolves against these bundled modules (after the native/host modules, per the
/// sealed-bundle precedence), so `import helpers` / `from helpers import x` / `helpers.fn()` reach
/// the bundled `helpers` -- with each module's globals and sibling functions bound to its OWN
/// namespace (a module function's home-module id rides its value, so it resolves globals against
/// its home even when called from another module), not the importer's.
///
/// This is the multi-module analog of [`run_module`]: a single-file program is a bundle with no
/// managed modules, for which `run_bundle` behaves exactly like `run_module` of its entry. Consumes
/// the bundle -- its modules move into the registry (no clone) and its entry runs in place.
pub fn run_bundle(bundle: Bundle, model: &mut ObjectModel) -> Result<Value, Trap> {
    let Bundle { entry, modules } = bundle;
    model.set_managed_modules(modules);
    let entry_functions: Rc<[CodeObject]> = Rc::from(entry.functions);
    model.set_entry_functions(Rc::clone(&entry_functions));
    run_module(&entry.body, &entry_functions, model)
}

/// Resolves an `import name` the interpreter-aware way, backing [`Op::ImportName`]: a cached / host /
/// native module FIRST (native wins, the sealed-bundle precedence -- a bundled `math.py` does not
/// shadow the stdlib `math`), else a MANAGED Python-authored module whose body has to RUN, else
/// `ModuleNotFoundError`.
///
/// It does not run the body itself. A managed module's body runs on the IMPORTER's frame stack --
/// pushed by the driver like a call, finished by [`finish_import`] when it returns -- because a body
/// run on a nested driver loop reaches no safe point, so nothing it allocates can be reclaimed until
/// the import completes. Everything the import machinery still has to do after the body runs rides an
/// [`ImportCompletion`] on the body's frame.
///
/// The module object is cached BEFORE its body runs, so a circular import (`a` imports `b`, `b`
/// imports `a`) sees the in-progress module and terminates rather than re-running the body forever.
/// During the body that cached module's namespace is empty; it is filled once the body completes (a
/// re-entrant import mid-cycle thus sees an empty module, which is enough to break the cycle --
/// exposing the partial-as-of-that-point state is a later refinement).
fn begin_import(name: &str, model: &mut ObjectModel) -> Result<ImportOutcome, Trap> {
    if let Some(result) = model.import_builtin_module(name) {
        return result.map(ImportOutcome::Ready);
    }
    let Some(module_id) = model.managed_module_id(name) else {
        return Err(model.module_not_found(name));
    };
    let empty = model.new_dict(Vec::new())?;
    let module_obj = model.new_module(empty)?;
    model.cache_module(name, module_obj);
    let completion = ImportCompletion { name: String::from(name), module_obj };
    Ok(ImportOutcome::RunBody { module_id, completion })
}

/// The other half of an `import`, run when the module BODY's frame returns: builds the module's dict
/// from what the body bound and hands back the MODULE object, which is what the importer receives.
///
/// The dict = the names the body BOUND (assignments, classes, and defaulted defs -- all mirrored into
/// module `module_id`'s globals by `StoreFast`) UNIONed with its top-level `def`s. A plain top-level
/// def is NOT stored as a global -- it lives in the function table, resolved by name via
/// [`resolve_global`] -- so without this union it is absent from the namespace and `from m import fn`
/// / `m.fn` fail. A top-level def has a SIMPLE name; nested defs (`outer.inner`), lambdas
/// (`<module>.<lambda.0>`) and class methods (`C.m`) carry a dotted qualified name and are not
/// module-level bindings, so a dot excludes them. Each exported def is stamped with THIS module as its
/// home (`function_ref_in_module`), so a call through it resolves the callee's own globals. A def the
/// body already bound (a defaulted def, stored as a PyFunction carrying its defaults) is skipped so
/// its defaults are not lost.
fn finish_import(
    module_id: u16,
    completion: ImportCompletion,
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    match build_module_namespace(module_id, completion.module_obj, model) {
        Ok(()) => Ok(completion.module_obj),
        Err(trap) => {
            model.uncache_module(&completion.name);
            Err(trap)
        }
    }
}

/// Fills `module_obj`'s namespace from module `module_id`'s bound globals and top-level defs.
fn build_module_namespace(
    module_id: u16,
    module_obj: Value,
    model: &mut ObjectModel,
) -> Result<(), Trap> {
    let functions = model.managed_functions_rc(module_id).ok_or(Trap::Malformed)?;
    let mut pairs = model.managed_module_globals(module_id);
    for (index, func) in functions.iter().enumerate() {
        if !func.name.contains('.') && !pairs.iter().any(|(n, _)| *n == func.name) {
            pairs.push((func.name.clone(), Value::function_ref_in_module(module_id, index as u32)));
        }
    }
    let namespace = model.namespace_from_globals(pairs)?;
    model.set_module_namespace(module_obj, namespace);
    Ok(())
}

/// One op's control-flow outcome, returned from the per-op block to the [`run_frames`]
/// driver: fall through to the next op ([`Flow::Next`]), return `value` from the current
/// function ([`Flow::Return`]), or invoke a direct Python function ([`Flow::Call`]). A jump
/// just mutates the frame's `ip` and falls through with `Next`.
enum Flow {
    Next,
    Return(Value),
    /// Call the planned Python function. The driver pushes a new [`Frame`] onto the explicit frame
    /// stack, so a deep Python call chain never grows the native stack -- and, since the frame
    /// carries its own module, so does a call into an IMPORTED module or a method of a class defined
    /// in one. Builtins, class init, dunders and generator resumes still stay on [`call_value`]
    /// (bounded native recursion).
    Call(PendingPyCall),
    /// Run an imported module's BODY on this frame stack, then hand the caller the MODULE object
    /// (not the body's return value) -- see [`ImportCompletion`]. The same reason as [`Flow::Call`]:
    /// a body run on a nested driver loop reaches no safe point.
    ImportBody { module_id: u16, completion: ImportCompletion },
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
    /// An imported module's top-level BODY, resolved from the registry by the frame's own module id.
    /// Distinct from [`CodeId::Entry`] because a body frame is pushed onto the IMPORTER's stack,
    /// where `Entry` means the importer's own code rather than this frame's.
    ModuleBody,
}

/// The half of an `import` that cannot run until the module BODY has: carried on the body's frame
/// from the moment it is pushed onto the caller's stack until it returns or raises.
#[derive(Debug)]
struct ImportCompletion {
    /// The module's registry name, so a body that RAISES can be un-cached and a later import retries
    /// (CPython drops a failed import from `sys.modules`).
    name: String,
    /// The module object, cached before the body ran so a circular import terminates against it. Its
    /// namespace is empty until the body completes; the importer receives THIS, not what the body
    /// returned.
    module_obj: Value,
}

/// What resolving an `import` produced: the module itself, or a body the driver has to run first.
enum ImportOutcome {
    /// A cached, host or native module -- nothing to run.
    Ready(Value),
    /// A Python-authored module being imported for the first time: its body has to run, and it runs
    /// on the importer's own frame stack so the collector's safe point reaches it.
    RunBody { module_id: u16, completion: ImportCompletion },
}

/// One exception this frame is handling, with the protected range of the try block whose handler
/// recorded it -- the range is how [`Frame::enter_handler`] tells a handler body being LEFT from a try
/// block written INSIDE one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Handled {
    /// The exception being handled.
    exception: Value,
    /// First op index of the try body its handler protects.
    start: u32,
    /// One past the last op index of that try body.
    end: u32,
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
    if let Some(pyfunc) = model.current_module_global(name).filter(|&g| model.is_py_function(g)) {
        Some(pyfunc)
    } else if let Some(index) = functions.iter().position(|f| f.name == *name) {
        Some(Value::function_ref_in_module(model.current_module(), index as u32))
    } else if let Some(global) = model.current_module_global(name) {
        Some(global)
    } else if let Some(id) = crate::builtins::builtin_id(name) {
        Some(Value::builtin_ref(id))
    } else if name == "Ellipsis" {
        Some(Value::ELLIPSIS)
    } else if name == "NotImplemented" {
        Some(Value::NOT_IMPLEMENTED)
    } else {
        model.exception_class(name)
    }
}

/// A call of Python-authored code, resolved to everything needed to RUN it -- which function of
/// which module, its arguments already bound to the parameters, and its captured cells -- but not
/// yet run.
///
/// Separating the resolution from the running is what lets ONE frame stack serve a call into an
/// imported module: the driver pushes the frame itself ([`Flow::Call`]) instead of re-entering a
/// nested driver loop, and a nested driver loop is precisely what the collector's safe point cannot
/// see past. Both callers of [`plan_py_call`] use this, so a call binds its arguments the same way
/// whichever of the two runs it.
struct PendingPyCall {
    /// Index into `module`'s function table.
    index: u32,
    /// The arguments, already placed in the callee's parameter slots.
    args: Vec<Value>,
    /// The captured cells if the callee is a closure, else empty; they seed the freevar half of the
    /// new frame's deref array.
    cells: Vec<Value>,
    /// The callee's HOME module: which function table its code comes from and which globals it
    /// resolves against, regardless of who called it.
    module: u16,
    /// Whether the callee is a generator function -- calling one RETURNS a generator object and
    /// runs no code, so the frame built here is suspended rather than pushed.
    is_generator: bool,
}

/// What [`plan_py_call`] made of a callee.
enum PyCallPlan {
    /// It runs Python code in a frame.
    Frame(PendingPyCall),
    /// It does not (a builtin, a class, a native shim, a callable instance), and here are the
    /// positional arguments back, so the caller dispatches them without copying them twice.
    NotAFrame(Vec<Value>),
}

/// Resolves `callee` to the frame it would run, or hands the arguments back if it is not a Python
/// function at all.
///
/// The three shapes that run Python code in a frame are a bare `function_ref`, a `PyFunction`
/// (defaults / kwdefaults / closure cells) and a bound method of a user class (`self` prepended).
/// Each resolves against the callee's OWN home module -- its code, its globals and its sibling
/// functions -- so an imported function behaves the same called from anywhere.
///
/// **Whether the arguments are BOUND here is not a tuning choice, it is which error a bad call
/// reports.** A bare function called positionally takes its arguments as they stand and lets frame
/// creation check the arity; everything that has to PLACE a value -- a default, a keyword, `*args`,
/// `**kwargs`, a keyword-only parameter, or the `self` of a method -- goes through
/// [`bind_arguments`] first, which is where the per-shape CPython wording lives.
fn plan_py_call(
    callee: Value,
    posargs: Vec<Value>,
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
) -> Result<PyCallPlan, Trap> {
    let (func, receiver) = if model.is_py_bound(callee) {
        let func = model.bound_func(callee);
        if func.as_function_index().is_none() && !model.is_py_function(func) {
            return Err(Trap::TypeError);
        }
        (func, Some(model.bound_self(callee)))
    } else if callee.as_function_index().is_some() || model.is_py_function(callee) {
        (callee, None)
    } else {
        return Ok(PyCallPlan::NotAFrame(posargs));
    };
    let module = model.function_home(func);
    let (index, defaults, kwdefaults, cells) = match func.as_function_index() {
        Some(index) => (index, Vec::new(), Value::NONE, Vec::new()),
        None => (
            model.py_function_index(func),
            model.py_function_defaults(func),
            model.py_function_kwdefaults(func),
            model.py_function_cells(func),
        ),
    };
    let home_funcs = model.managed_functions_rc(module);
    let code_funcs: &[CodeObject] = home_funcs.as_deref().unwrap_or(functions);
    let code = code_funcs.get(index as usize).ok_or(Trap::Malformed)?;
    let is_generator = code.is_generator;
    let must_bind = receiver.is_some()
        || func.as_function_index().is_none()
        || !kwargs.is_empty()
        || code.has_varargs
        || code.has_varkwargs
        || code.kwonly_count > 0;
    let args = if must_bind {
        let mut positional = Vec::with_capacity(posargs.len() + usize::from(receiver.is_some()));
        if let Some(self_value) = receiver {
            positional.push(self_value);
        }
        positional.extend_from_slice(&posargs);
        bind_arguments(code, &positional, kwargs, &defaults, kwdefaults, model)?
    } else {
        posargs
    };
    Ok(PyCallPlan::Frame(PendingPyCall { index, args, cells, module, is_generator }))
}

/// Builds the frame a planned call runs in, resolving the callee's code against its HOME module's
/// table. Shared by the driver's same-stack push and the generator-object case, so a generator's
/// suspended frame and a called frame are built identically.
fn new_planned_frame(
    pending: &PendingPyCall,
    functions: &[CodeObject],
    model: &mut ObjectModel,
) -> Result<Frame, Trap> {
    let home_funcs = model.managed_functions_rc(pending.module);
    let code_funcs: &[CodeObject] = home_funcs.as_deref().unwrap_or(functions);
    let code = code_funcs.get(pending.index as usize).ok_or(Trap::Malformed)?;
    new_frame(
        code,
        CodeId::Func(pending.index),
        &pending.args,
        false,
        &pending.cells,
        pending.module,
        model,
    )
}

/// Runs a planned call on a NESTED driver loop and returns its value -- the callback path, for a
/// callee reached from somewhere that has no frame stack to push onto (a dunder, a builtin's `key=`,
/// a class initializer). The same plan pushed by [`Flow::Call`] instead runs on the caller's own
/// stack, which is the one the safe point can see.
fn run_planned_call(
    pending: PendingPyCall,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if pending.is_generator {
        let generator = new_planned_frame(&pending, functions, model)?;
        return model.new_generator(generator, pending.module);
    }
    let home_funcs = model.managed_functions_rc(pending.module);
    let code_funcs: &[CodeObject] = home_funcs.as_deref().unwrap_or(functions);
    let code = code_funcs.get(pending.index as usize).ok_or(Trap::Malformed)?;
    run_frames(
        code,
        code_funcs,
        &pending.args,
        &pending.cells,
        model,
        false,
        depth,
        pending.module,
    )
}

/// `object.attr = value` -- the one attribute-store dispatch shared by the `SetAttr` op and the
/// `setattr` builtin, so `obj.x = v` and `setattr(obj, 'x', v)` cannot diverge. An instance routes
/// through a user `__setattr__` (which owns every write) or a property setter before the plain
/// instance-dict store; a class writes its own namespace (decorator mutation / class-level rebinding);
/// a user function stores in the attribute side-table; anything else is a native property set.
pub(crate) fn set_attr(
    object: Value,
    attr: &str,
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<(), Trap> {
    if model.is_instance(object) {
        if let Some(hook) = model.find_dunder(object, "__setattr__") {
            let name_value = model.new_str(attr)?;
            call_value(hook, &[name_value, value], functions, model, depth + 1)?;
        } else if let Some(set) = model.instance_set_descriptor(object, attr) {
            call_value(set, &[object, value], functions, model, depth + 1)?;
        } else if let Some(property) = model.class_property(object, attr) {
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
    Ok(())
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
    if callee.as_function_index().is_some()
        || model.is_py_function(callee)
        || model.is_py_bound(callee)
    {
        return match plan_py_call(callee, args.to_vec(), &[], functions, model)? {
            PyCallPlan::Frame(pending) => run_planned_call(pending, functions, model, depth),
            PyCallPlan::NotAFrame(_) => Err(Trap::Malformed),
        };
    }
    if let Some(id) = callee.as_builtin_id() {
        crate::builtins::call_builtin(id, args, functions, model, depth)
    } else if model.is_str_join(callee) {
        let [iterable] = args else {
            return Err(Trap::TypeError);
        };
        let items = crate::builtins::collect_iterable(model, &[*iterable], functions, depth)?;
        let list = model.new_list(items)?;
        model.call_bound_method(callee, &[list])
    } else if model.is_str_format_bound(callee) {
        let receiver = model.bound_receiver(callee);
        let template = model.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
        let rendered = model.format_template(&template, args, &[], functions, depth)?;
        model.new_str(&rendered)
    } else if model.is_bound_method(callee) {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        if method_id == crate::object::CALL_DUNDER {
            call_value(receiver, args, functions, model, depth)
        } else if method_id == crate::object::NEXT_DUNDER {
            match py_next_value(receiver, functions, model, depth)? {
                Some(value) => Ok(value),
                None => {
                    let value = if model.is_generator(receiver) {
                        model.take_generator_return().unwrap_or(Value::NONE)
                    } else {
                        Value::NONE
                    };
                    Err(model.raise_named_exception_with_value("StopIteration", value))
                }
            }
        } else if model.is_builtin_dunder_method(method_id)
            && model.builtin_dunder_comparison(method_id).is_some()
            && !model.is_instance(match args {
                [subject, _] => *subject,
                _ => receiver,
            })
        {
            let op = model.builtin_dunder_comparison(method_id).unwrap_or(CmpOp::Eq);
            let (receiver, other) = match args {
                [other] => (receiver, *other),
                [subject, other] => (*subject, *other),
                _ => return Err(Trap::TypeError),
            };
            if !model.comparison_dunder_accepts(receiver, other) {
                return Ok(Value::NOT_IMPLEMENTED);
            }
            if model.is_dict(receiver) && !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                return Ok(Value::NOT_IMPLEMENTED);
            }
            match dispatch_compare(op, receiver, other, functions, model, depth) {
                Ok(value) => Ok(value),
                Err(Trap::TypeError) => Ok(Value::NOT_IMPLEMENTED),
                Err(error) => Err(error),
            }
        } else if model.is_builtin_dunder_method(method_id) {
            model.call_bound_method(callee, args)
        } else if model.is_generator(receiver) {
            call_generator_method(receiver, method_id, args, functions, model, depth)
        } else if model.is_set(receiver) || model.is_frozenset(receiver) {
            model.call_set_method_dyn(receiver, method_id, args, functions, depth)
        } else if model.is_dict(receiver) {
            model.call_dict_method_dyn(receiver, method_id, args, functions, depth)
        } else if model.is_deque(receiver) {
            model.call_deque_method_dyn(receiver, method_id, args, functions, depth)
        } else if model.is_list(receiver) {
            model.call_list_method_dyn(callee, receiver, method_id, args, functions, depth)
        } else {
            model.call_bound_method(callee, args)
        }
    } else if model.is_unbound_method(callee) {
        let (receiver, rest) = args.split_first().ok_or(Trap::TypeError)?;
        let name_value = model.unbound_method_name(callee);
        let name = model.str_value(name_value).ok_or(Trap::TypeError)?.to_string();
        let bound = model.getattr(*receiver, &name, &mut crate::object::InlineCache::empty())?;
        call_value(bound, rest, functions, model, depth)
    } else if model.is_ntclass(callee) {
        construct_namedtuple(callee, args, &[], model)
    } else if model.is_class(callee) {
        instantiate(callee, args, &[], functions, model, depth)
    } else if model.is_pin_factory(callee) {
        model.call_pin_factory(args)
    } else if model.is_uart_shim_factory(callee) {
        model.call_uart_shim_factory(callee, args, &[])
    } else if model.is_spi_shim_factory(callee) {
        model.call_spi_shim_factory(callee, args, &[])
    } else if model.is_i2c_shim_factory(callee) {
        model.call_i2c_shim_factory(callee, args, &[])
    } else if model.is_adc_shim_factory(callee) {
        model.call_adc_shim_factory(callee, args)
    } else if model.is_dio_factory(callee) {
        model.call_dio_factory(args)
    } else if let Some(call_method) = model.find_dunder(callee, "__call__") {
        call_value(call_method, args, functions, model, depth + 1)
    } else {
        Err(model.object_is_not(callee, "callable"))
    }
}

/// Binds a namedtuple class's call arguments (positional + keyword) to its fields and allocates
/// the instance. The arity/keyword errors carry CPython's `Name.__new__()` spellings.
fn construct_namedtuple(
    class: Value,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    let fields = model.ntclass_fields(class);
    let name = model.ntclass_name(class);
    if posargs.len() > fields.len() {
        let message = alloc::format!(
            "{name}.__new__() takes {} positional arguments but {} were given",
            fields.len() + 1,
            posargs.len() + 1
        );
        return Err(model.raise_named_exception("TypeError", &message));
    }
    let mut slots: Vec<Option<Value>> = posargs.iter().map(|&v| Some(v)).collect();
    slots.resize(fields.len(), None);
    for &(kw_name, value) in kwargs {
        let Some(at) = fields.iter().position(|f| f == kw_name) else {
            let message =
                alloc::format!("{name}.__new__() got an unexpected keyword argument '{kw_name}'");
            return Err(model.raise_named_exception("TypeError", &message));
        };
        if slots[at].is_some() {
            let message =
                alloc::format!("{name}.__new__() got multiple values for argument '{kw_name}'");
            return Err(model.raise_named_exception("TypeError", &message));
        }
        slots[at] = Some(value);
    }
    let missing: Vec<&str> = fields
        .iter()
        .zip(&slots)
        .filter(|(_, slot)| slot.is_none())
        .map(|(field, _)| field.as_str())
        .collect();
    if !missing.is_empty() {
        let plural = if missing.len() == 1 { "" } else { "s" };
        let message = alloc::format!(
            "{name}.__new__() missing {} required positional argument{plural}: {}",
            missing.len(),
            join_arg_names(&missing)
        );
        return Err(model.raise_named_exception("TypeError", &message));
    }
    let elements = slots.into_iter().flatten().collect();
    model.new_ntinstance(class, elements)
}

/// `C(*posargs, **kwargs)`: constructs an instance of a user class. Allocation goes through the
/// class's `__new__` and initialization through the result's `__init__` -- CPython's two-step, in
/// that order, and the ONE path both the positional and the keyword call arms use, so a class
/// cannot be constructed two different ways depending on how its arguments were spelled.
///
/// Two consequences of the two-step worth naming, because they are what `__new__` is FOR:
/// a `__new__` returning something that is not an instance of the class skips initialization
/// entirely (a caching or interning factory hands back an object that is already initialized), and
/// `__init__` is resolved on the RESULT's class rather than on the class that was called (a `__new__`
/// returning a subclass instance initializes as that subclass).
fn instantiate(
    class: Value,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let instance = match model.find_new(class)? {
        Some(new) => {
            let mut new_args = Vec::with_capacity(posargs.len() + 1);
            new_args.push(class);
            new_args.extend_from_slice(posargs);
            let made = call_maybe_kw(new, &new_args, kwargs, functions, model, depth + 1)?;
            if !model.is_instance_of(made, class) {
                return Ok(made);
            }
            made
        }
        None => model.object_new(class, posargs, kwargs)?,
    };
    let actual = crate::builtins::type_of(instance, model).unwrap_or(class);
    if let Some(init) = model.find_init(actual) {
        let mut init_args = Vec::with_capacity(posargs.len() + 1);
        init_args.push(instance);
        init_args.extend_from_slice(posargs);
        call_maybe_kw(init, &init_args, kwargs, functions, model, depth)?;
    } else {
        model.init_default_args(instance, posargs)?;
    }
    Ok(instance)
}

/// Calls `callee` with positional arguments and, when there are any, keywords -- the keyword path
/// only when it is needed, so a purely positional call behaves exactly as [`call_value`] does.
fn call_maybe_kw(
    callee: Value,
    posargs: &[Value],
    kwargs: &[(&str, Value)],
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if kwargs.is_empty() {
        call_value(callee, posargs, functions, model, depth)
    } else {
        call_value_kw(callee, posargs, kwargs, functions, model, depth)
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
    if callee.as_function_index().is_some()
        || model.is_py_function(callee)
        || model.is_py_bound(callee)
    {
        return match plan_py_call(callee, posargs.to_vec(), kwargs, functions, model)? {
            PyCallPlan::Frame(pending) => run_planned_call(pending, functions, model, depth),
            PyCallPlan::NotAFrame(_) => Err(Trap::Malformed),
        };
    }
    if model.is_ntclass(callee) {
        construct_namedtuple(callee, posargs, kwargs, model)
    } else if model.is_class(callee) {
        instantiate(callee, posargs, kwargs, functions, model, depth)
    } else if model.is_int_to_bytes_bound(callee) {
        let receiver = model.bound_receiver(callee);
        let mut length = match posargs.first() {
            Some(value) => value.as_int().ok_or(Trap::TypeError)?,
            None => 1,
        };
        let mut byteorder = match posargs.get(1) {
            Some(value) => model.str_value(*value).map(String::from).ok_or(Trap::TypeError)?,
            None => String::from("big"),
        };
        if posargs.len() > 2 {
            return Err(Trap::TypeError);
        }
        let mut signed = false;
        for &(name, value) in kwargs {
            match name {
                "signed" => signed = model.py_truthy(value)?.unwrap_or(false),
                "length" if posargs.is_empty() => length = value.as_int().ok_or(Trap::TypeError)?,
                "byteorder" if posargs.len() < 2 => {
                    byteorder = model.str_value(value).map(String::from).ok_or(Trap::TypeError)?;
                }
                other => {
                    let message =
                        alloc::format!("to_bytes() got an unexpected keyword argument '{other}'");
                    return Err(model.raise_named_exception("TypeError", &message));
                }
            }
        }
        model.int_to_bytes(receiver, length, &byteorder, signed)
    } else if model.is_list_sort_bound(callee) {
        let receiver = model.bound_receiver(callee);
        list_sort_kw(posargs, kwargs, receiver, functions, model, depth)
    } else if model.is_str_format_bound(callee) {
        let receiver = model.bound_receiver(callee);
        let template = model.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
        let rendered = model.format_template(&template, posargs, kwargs, functions, depth)?;
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
    } else if model.is_uart_shim_factory(callee) {
        model.call_uart_shim_factory(callee, posargs, kwargs)
    } else if model.is_spi_shim_factory(callee) {
        model.call_spi_shim_factory(callee, posargs, kwargs)
    } else if model.is_i2c_shim_factory(callee) {
        model.call_i2c_shim_factory(callee, posargs, kwargs)
    } else if model.is_bound_method(callee) && model.is_spi_shim(model.bound_receiver(callee)) {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        model.call_spi_shim_method(receiver, method_id, posargs, kwargs)
    } else if model.is_bound_method(callee) && model.is_i2c_shim(model.bound_receiver(callee)) {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        model.call_i2c_shim_method(receiver, method_id, posargs, kwargs)
    } else if model.is_adc_shim_factory(callee) {
        model.call_adc_shim_factory(callee, posargs)
    } else if model.is_bound_method(callee) && model.is_adc_shim(model.bound_receiver(callee)) {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        model.call_adc_shim_method(receiver, method_id, posargs)
    } else if model.is_bound_method(callee)
        && (model.is_uart(model.bound_receiver(callee))
            || model.is_uart_port(model.bound_receiver(callee)))
    {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        model.call_uart_bound_kw(receiver, method_id, posargs, kwargs)
    } else if model.is_bound_method(callee)
        && (model.is_spi(model.bound_receiver(callee))
            || model.is_spi_bus(model.bound_receiver(callee)))
    {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        model.call_spi_bound_kw(receiver, method_id, posargs, kwargs)
    } else if model.is_bound_method(callee)
        && (model.is_i2c(model.bound_receiver(callee))
            || model.is_i2c_bus(model.bound_receiver(callee)))
    {
        let receiver = model.bound_receiver(callee);
        let method_id = model.bound_method_id(callee);
        model.call_i2c_bound_kw(receiver, method_id, posargs, kwargs)
    } else if model.is_bound_method(callee) {
        let receiver = model.bound_receiver(callee);
        if model.is_ntinstance(receiver) && model.bound_method_id(callee) == crate::object::NT_REPLACE
        {
            if !posargs.is_empty() {
                return Err(Trap::TypeError);
            }
            let class = model.ntinstance_class(receiver).ok_or(Trap::TypeError)?;
            let fields = model.ntclass_fields(class);
            let mut elements = model.seq_elements(receiver).unwrap_or_default();
            let mut unexpected = Vec::new();
            for &(name, value) in kwargs {
                match fields.iter().position(|f| f == name) {
                    Some(at) => elements[at] = value,
                    None => unexpected.push(alloc::format!("'{name}'")),
                }
            }
            if !unexpected.is_empty() {
                let message =
                    alloc::format!("Got unexpected field names: [{}]", unexpected.join(", "));
                return Err(model.raise_named_exception("TypeError", &message));
            }
            return model.new_ntinstance(class, elements);
        }
        if model.bound_method_id(callee) == crate::object::OBJECT_NEW {
            let (class, rest) = posargs.split_first().ok_or_else(|| {
                model.raise_named_exception("TypeError", "object.__new__(): not enough arguments")
            })?;
            return model.object_new(*class, rest, kwargs);
        }
        if kwargs.is_empty() {
            model.call_bound_method(callee, posargs)
        } else {
            Err(Trap::TypeError)
        }
    } else if let Some(id) = callee.as_builtin_id() {
        crate::builtins::call_builtin_kw(id, posargs, kwargs, functions, model, depth)
    } else if let Some(call_method) = model.find_dunder(callee, "__call__") {
        call_value_kw(call_method, posargs, kwargs, functions, model, depth + 1)
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
            return Err(too_many_positional_error(code, n_regular, defaults.len(), posargs.len(), model));
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
                    let message = alloc::format!(
                        "{}() got multiple values for argument '{name}'",
                        code.name
                    );
                    return Err(model.raise_named_exception("TypeError", &message));
                }
                slots[idx] = Some(value);
            }
            None if code.has_varkwargs => extra.push((model.new_str(name)?, value)),
            None => {
                let message = alloc::format!(
                    "{}() got an unexpected keyword argument '{name}'",
                    code.name
                );
                return Err(model.raise_named_exception("TypeError", &message));
            }
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

    let kwonly_start = n_regular + code.has_varargs as usize;
    let mut missing_positional: Vec<&str> = Vec::new();
    let mut missing_kwonly: Vec<&str> = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        if slot.is_none() {
            if i < n_regular {
                missing_positional.push(code.params[i].name.as_str());
            } else if (kwonly_start..kwonly_start + kwonly).contains(&i) {
                missing_kwonly.push(code.params[i].name.as_str());
            }
        }
    }
    if !missing_positional.is_empty() {
        return Err(missing_args_error(code, &missing_positional, "positional", model));
    }
    if !missing_kwonly.is_empty() {
        return Err(missing_args_error(code, &missing_kwonly, "keyword-only", model));
    }
    let mut locals = Vec::with_capacity(nparams);
    for slot in slots {
        locals.push(slot.ok_or(Trap::Malformed)?);
    }
    Ok(locals)
}

/// Joins parameter names for a "missing arguments" message, Oxford-comma style like CPython:
/// `'a'`, `'a' and 'b'`, `'a', 'b', and 'c'`.
fn join_arg_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => alloc::format!("'{only}'"),
        [first, second] => alloc::format!("'{first}' and '{second}'"),
        [rest @ .., last] => {
            let mut joined = String::new();
            for name in rest {
                joined.push('\'');
                joined.push_str(name);
                joined.push_str("', ");
            }
            joined.push_str("and '");
            joined.push_str(last);
            joined.push('\'');
            joined
        }
    }
}

/// The `TypeError` for a call missing required arguments -- CPython's `NAME() missing N required KIND
/// argument(s): 'a' and 'b'` (`KIND` is `positional` or `keyword-only`; the `()` suffix + qualified
/// name match CPython, e.g. `Dog.__init__() missing 1 required positional argument: 'breed'`).
fn missing_args_error(code: &CodeObject, missing: &[&str], kind: &str, model: &mut ObjectModel) -> Trap {
    let count = missing.len();
    let plural = if count == 1 { "argument" } else { "arguments" };
    let names = join_arg_names(missing);
    let message = alloc::format!("{}() missing {count} required {kind} {plural}: {names}", code.name);
    model.raise_named_exception("TypeError", &message)
}

/// The `TypeError` for a call with too many positional arguments -- CPython's `NAME() takes N (or
/// `from MIN to MAX`) positional argument(s) but G were/was given`. `n_defaults` trailing positional
/// params have defaults, so the accepted count is the range `[n_regular - n_defaults, n_regular]`.
fn too_many_positional_error(
    code: &CodeObject,
    n_regular: usize,
    n_defaults: usize,
    given: usize,
    model: &mut ObjectModel,
) -> Trap {
    let min = n_regular - n_defaults;
    let takes = if min < n_regular {
        alloc::format!("from {min} to {n_regular}")
    } else {
        alloc::format!("{n_regular}")
    };
    let arg_word = if n_regular == 1 && min == n_regular { "argument" } else { "arguments" };
    let verb = if given == 1 { "was" } else { "were" };
    let message =
        alloc::format!("{}() takes {takes} positional {arg_word} but {given} {verb} given", code.name);
    model.raise_named_exception("TypeError", &message)
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
    let frame = match model.take_generator_frame(generator) {
        Some(frame) => frame,
        None => {
            return match resume {
                Resume::Send(_) => {
                    model.set_generator_return(Value::NONE);
                    Ok(Value::STOP)
                }
                Resume::Throw(exc) => {
                    model.set_pending_exception(exc);
                    Err(Trap::Raised)
                }
                Resume::Close => Ok(Value::NONE),
            };
        }
    };
    let outcome = drive_taken_generator(generator, frame, resume, functions, model, depth);
    model.end_generator_resume(generator);
    outcome
}

/// One resume, with the frame already OUT of the model's table (and its slot reserved).
///
/// Split from [`resume_generator`] for that reservation alone: every exit here is an exit from a
/// function whose caller releases it, rather than another place to remember to.
fn drive_taken_generator(
    generator: Value,
    mut frame: Frame,
    resume: Resume,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    let close_mode = matches!(resume, Resume::Close);
    let module_id = model.generator_module(generator);
    let gen_functions_rc = model.managed_functions_rc(module_id);
    let gen_functions: &[CodeObject] = gen_functions_rc.as_deref().unwrap_or(functions);
    let entry = match frame.code {
        CodeId::Func(index) => gen_functions.get(index as usize).ok_or(Trap::Malformed)?,
        CodeId::Entry | CodeId::ModuleBody => return Err(Trap::Malformed),
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
        Resume::Throw(exc) => {
            if frame.yield_from_active {
                model.set_yield_from_throw(exc);
            } else {
                match find_handler(&entry.exc_table, frame.ip as u32) {
                    Some(handler) => frame.enter_handler(exc, handler),
                    None => {
                        model.set_pending_exception(exc);
                        return Err(Trap::Raised);
                    }
                }
            }
        }
        Resume::Close => {
            let exc = model.new_exception("GeneratorExit")?;
            if frame.yield_from_active {
                model.set_yield_from_throw(exc);
            } else {
                match find_handler(&entry.exc_table, frame.ip as u32) {
                    Some(handler) => frame.enter_handler(exc, handler),
                    None => return Ok(Value::NONE),
                }
            }
        }
    }
    let mut frames: Vec<Frame> = Vec::new();
    frames.push(frame);
    let saved_module = model.set_current_module(module_id);
    let outcome = drive(&mut frames, entry, gen_functions, model, depth + 1);
    model.set_current_module(saved_module);
    match outcome {
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
        Ok(DriveOutcome::Returned(returned)) => {
            if close_mode {
                Ok(Value::NONE)
            } else {
                model.set_generator_return(returned);
                Ok(Value::STOP)
            }
        }
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
        let returned = model.take_generator_return().unwrap_or(Value::NONE);
        Err(model.raise_named_exception_with_value("StopIteration", returned))
    } else {
        Ok(value)
    }
}

/// One step of a `yield from` delegation ([`advance_yield_from`]): the sub either yielded a value
/// (re-yield it) or is exhausted (its return value becomes the `yield from` expression's value).
enum YieldFromStep {
    Yielded(Value),
    Returned(Value),
}

/// How to advance a `yield from` sub-iterator one step: forward a sent value (`send`/`next`) or throw
/// an exception into it (`gen.throw`/`gen.close` into a delegating generator).
enum YieldFromAction {
    Send(Value),
    Throw(Value),
}

/// Drives the sub-iterator of a `yield from` by one step. A GENERATOR sub is resumed with the sent
/// value or the thrown exception (its return value on exhaustion -- its `StopIteration.value` --
/// becomes the step's `Returned`; a propagated raise returns `Err`). Any other iterable is advanced by
/// one `next` for a send (a non-None send is a TypeError -- a plain iterator has no `send` -- and its
/// return value is always `None`); a throw into a plain iterator raises the exception here.
fn advance_yield_from(
    sub: Value,
    action: YieldFromAction,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<YieldFromStep, Trap> {
    if model.is_generator(sub) {
        let resume = match action {
            YieldFromAction::Send(value) => Resume::Send(value),
            YieldFromAction::Throw(exc) => Resume::Throw(exc),
        };
        let value = resume_generator(sub, resume, functions, model, depth + 1)?;
        if value.is_stop() {
            Ok(YieldFromStep::Returned(model.take_generator_return().unwrap_or(Value::NONE)))
        } else {
            Ok(YieldFromStep::Yielded(value))
        }
    } else {
        match action {
            YieldFromAction::Send(send_value) => {
                if send_value != Value::NONE {
                    return Err(Trap::TypeError);
                }
                match py_next_value(sub, functions, model, depth + 1)? {
                    Some(value) => Ok(YieldFromStep::Yielded(value)),
                    None => Ok(YieldFromStep::Returned(Value::NONE)),
                }
            }
            YieldFromAction::Throw(exc) => {
                model.set_pending_exception(exc);
                Err(Trap::Raised)
            }
        }
    }
}

/// Attribute lookup with CPython's `__getattr__` fallback: the normal lookup first, and on an
/// AttributeError miss the type's `__getattr__(name)` hook if it defines one (a proxy / lazy /
/// computed attribute). A type without the hook re-raises the bare [`Trap::AttributeError`], so
/// `Op::LoadAttr` still synthesizes the descriptive `'X' object has no attribute 'NAME'` and
/// `getattr`/`hasattr` still see the bare trap for their default / boolean handling. (`__getattr__`
/// is looked up on the type, so it never recurses through the instance dict.) Shared by the
/// `getattr`/`hasattr` builtins; `Op::LoadAttr` inlines the same fallback to keep its property path.
pub(crate) fn getattr_hooked(
    receiver: Value,
    name: &str,
    slot: &mut InlineCache,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if let Some(value) = descriptor_read(receiver, name, functions, model, depth)? {
        return Ok(value);
    }
    match model.getattr(receiver, name, slot) {
        Err(Trap::AttributeError) => match model.find_dunder(receiver, "__getattr__") {
            Some(hook) => {
                let name_value = model.new_str(name)?;
                call_value(hook, &[name_value], functions, model, depth + 1)
            }
            None => Err(Trap::AttributeError),
        },
        other => other,
    }
}

/// The descriptor read hook shared by `Op::LoadAttr` and `getattr_hooked`: a class attribute whose
/// own class defines `__get__` intercepts `instance.name` per CPython precedence (a DATA descriptor
/// wins over the instance dict; a NON-data one yields to it). `None` = no user descriptor applies, so
/// the caller runs its normal read path (the fast path for methods/property/plain values). `__get__`
/// is invoked with `(instance, type(instance))`.
fn descriptor_read(
    receiver: Value,
    name: &str,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Option<Value>, Trap> {
    if name == "__dict__" {
        return Ok(None);
    }
    match model.instance_descriptor_read(receiver, name) {
        Some(DescriptorRead::Value(value)) => Ok(Some(value)),
        Some(DescriptorRead::Get(get)) => {
            let objtype = model.instance_class(receiver);
            Ok(Some(call_value(get, &[receiver, objtype], functions, model, depth + 1)?))
        }
        None => Ok(None),
    }
}

/// CPython's `operator.index`: coerces an object used where an INTEGER index is required (a built-in
/// sequence subscript) via its `__index__()` -- so a custom integer-like value can index `list` /
/// `tuple` / `str` / `bytes`. An int already, or a value with no `__index__`, passes through
/// unchanged (so a slice reaches the slice path and a non-index value reaches the caller's own
/// error). `__index__` returning a non-int is a TypeError. Callers exempt `dict` (it keys by the
/// object itself, not an int).
pub(crate) fn coerce_index(
    value: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if !model.is_instance(value) {
        return Ok(value);
    }
    let Some(hook) = model.find_dunder(value, "__index__") else {
        return Ok(value);
    };
    let result = call_value(hook, &[], functions, model, depth + 1)?;
    if model.is_int(result) {
        Ok(result)
    } else {
        Err(Trap::TypeError)
    }
}

/// [`coerce_index`] for a subscript key: a plain index is coerced directly; a SLICE has each of its
/// non-`None` bounds coerced via `__index__` (`lst[Idx(1):Idx(4)]`), rebuilding the slice only when a
/// bound actually changed so the original slice object keeps its bounds. Any other value (incl. a
/// slice with plain int/None bounds) passes through untouched.
fn coerce_subscript(
    index: Value,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<Value, Trap> {
    if !model.is_slice(index) {
        return coerce_index(index, functions, model, depth);
    }
    let (start, stop, step) = model.slice_components(index);
    let new_start = coerce_index(start, functions, model, depth)?;
    let new_stop = coerce_index(stop, functions, model, depth)?;
    let new_step = coerce_index(step, functions, model, depth)?;
    if new_start != start || new_stop != stop || new_step != step {
        model.new_slice(new_start, new_stop, new_step)
    } else {
        Ok(index)
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
    if model.is_file(value) {
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
        match model.new_iter(value) {
            Ok(iter) => Ok(iter),
            Err(Trap::TypeError) if model.find_dunder(value, "__getitem__").is_some() => {
                let sources = model.new_tuple(alloc::vec![value])?;
                let zero = Value::fixnum(0).ok_or(Trap::Overflow)?;
                model.new_lazy_iter(crate::object::LAZY_GETITEM, zero, sources)
            }
            Err(Trap::TypeError) => Err(model.object_is_not(value, "iterable")),
            Err(other) => Err(other),
        }
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
    } else if model.is_file(iterator) {
        match model.call_file_method(iterator, crate::fileio::FILE_NEXT, &[]) {
            Ok(line) => Ok(Some(line)),
            Err(Trap::Raised) if model.pending_exception_is("StopIteration") => {
                model.take_pending_exception();
                Ok(None)
            }
            Err(other) => Err(other),
        }
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

/// `locals()`: the running frame's local bindings as a dict. At MODULE level this is the module
/// globals (CPython's rule -- module-level `locals()` IS `globals()`); in a FUNCTION it is each BOUND
/// local name mapped to its value (an unassigned slot, [`Value::UNBOUND`], is omitted, as CPython omits
/// a not-yet-bound local). Needs the running frame, so it is driven from the `Op::Call` site.
fn build_frame_locals(
    frame: &Frame,
    code: &CodeObject,
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    if frame.is_module {
        let pairs = model.current_module_globals();
        return model.namespace_from_globals(pairs);
    }
    let mut pairs: Vec<(Value, Value)> = Vec::new();
    for (i, name) in code.local_names.iter().enumerate() {
        let value = frame.locals.get(i).copied().unwrap_or(Value::UNBOUND);
        if value != Value::UNBOUND {
            let key = model.new_str(name)?;
            pairs.push((key, value));
        }
    }
    model.new_dict(pairs)
}

/// CPython's `zip(strict=True)` length-mismatch message: `index` is the 0-based position of the
/// offending source (the one shorter, or -- for `longer` -- the later one that outlives source 0),
/// and the sources it is compared against are `1` (singular) or `1-index` (plural).
fn zip_strict_message(index: usize, longer: bool) -> String {
    let relation = if longer { "longer" } else { "shorter" };
    if index == 1 {
        alloc::format!("zip() argument 2 is {relation} than argument 1")
    } else {
        alloc::format!("zip() argument {} is {relation} than arguments 1-{index}", index + 1)
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
            let strict = model.lazy_iter_state(iterator).is_truthy();
            let mut row = Vec::with_capacity(sources.len());
            for (i, source) in sources.iter().enumerate() {
                match py_next_value(*source, functions, model, depth)? {
                    Some(value) => row.push(value),
                    None => {
                        if strict {
                            if i > 0 {
                                let message = zip_strict_message(i, false);
                                return Err(model.with_message(Trap::ValueError, &message));
                            }
                            for (j, later) in sources.iter().enumerate().skip(1) {
                                if py_next_value(*later, functions, model, depth)?.is_some() {
                                    let message = zip_strict_message(j, true);
                                    return Err(model.with_message(Trap::ValueError, &message));
                                }
                            }
                        }
                        return Ok(None);
                    }
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
        crate::object::LAZY_GETITEM => {
            let target = sources[0];
            let index = model.lazy_iter_state(iterator);
            let getitem = model.find_dunder(target, "__getitem__").ok_or(Trap::TypeError)?;
            match call_value(getitem, &[index], functions, model, depth) {
                Ok(value) => {
                    let next = model.new_long(model.as_i128(index).unwrap_or(0) + 1)?;
                    model.lazy_iter_set_state(iterator, next);
                    Ok(Some(value))
                }
                Err(Trap::IndexError) => Ok(None),
                Err(Trap::Raised) if model.pending_exception_is("IndexError") => {
                    model.take_pending_exception();
                    Ok(None)
                }
                Err(other) => Err(other),
            }
        }
        _ => Err(Trap::TypeError),
    }
}

/// Resolves a [`CodeId`] to its [`CodeObject`] against the driver's entry, the frame's own module's
/// function table, and that module's body. Every stored `CodeId` is validated when its frame is
/// created, so the index is in range; a missing `body` is a frame claiming to be a module body in a
/// module that has none, which is malformed rather than possible.
fn resolve_code<'a>(
    id: CodeId,
    entry: &'a CodeObject,
    functions: &'a [CodeObject],
    body: Option<&'a CodeObject>,
) -> Result<&'a CodeObject, Trap> {
    match id {
        CodeId::Entry => Ok(entry),
        CodeId::Func(index) => Ok(&functions[index as usize]),
        CodeId::ModuleBody => body.ok_or(Trap::Malformed),
    }
}

/// Builds a fresh frame for `code`, binding `args` to its leading local slots. A wrong argument
/// count is a `TypeError` (CPython call binding), not malformed bytecode. `id` records which
/// code the frame runs, `is_module` whether it is the module body, and `module` which module the
/// code belongs to (the callee's home, which the driver installs while the frame is on top).
#[allow(clippy::too_many_arguments)]
fn new_frame(
    code: &CodeObject,
    id: CodeId,
    args: &[Value],
    is_module: bool,
    captured_cells: &[Value],
    module: u16,
    model: &mut ObjectModel,
) -> Result<Frame, Trap> {
    if args.len() != code.params.len() {
        return Err(if args.len() < code.params.len() {
            let missing: Vec<&str> =
                code.params[args.len()..].iter().map(|p| p.name.as_str()).collect();
            missing_args_error(code, &missing, "positional", model)
        } else {
            too_many_positional_error(code, code.params.len(), 0, args.len(), model)
        });
    }
    let mut frame = match model.take_pooled_frame() {
        Some(mut pooled) => {
            pooled.locals.clear();
            pooled.locals.resize(code.n_locals, Value::UNBOUND);
            pooled.caches.clear();
            pooled.caches.resize(code.cache_count, InlineCache::empty());
            pooled.stack.clear();
            pooled.handled.clear();
            pooled.ip = 0;
            pooled
        }
        None => Frame::new(code.n_locals, code.cache_count),
    };
    frame.code = id;
    frame.is_module = is_module;
    frame.module = module;
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
/// `Op::Call` / `Op::CallKw` of any Python function pushes a frame -- a plain one, a defaulted one,
/// a closure, a bound method, and whichever module it comes from, because a frame carries the module
/// it resolves against; `Op::Return` pops one and hands its value to the caller; a raise/trap
/// unwinds across the frames, each consulting its own `exc_table`. Builtins, class init, dunders and
/// generator resumes run through [`call_value`], which re-enters this driver -- bounded native
/// recursion, guarded by `depth`.
///
/// GC: the collector traces EVERY frame on the stack, so a mid-call collection sees all roots
/// (the fix for the old one-frame-at-a-time root gap). **That is why the module rides the frame:
/// a call that re-entered this driver put its frames somewhere the safe point could not reach, so a
/// program whose work lives in an imported module reclaimed nothing until the call returned.**
/// `is_module` marks the entry frame as the module body (its `StoreFast`s mirror into the globals).
/// The explicit stack is bounded by [`MAX_CALL_DEPTH`] frames -> a catchable `RecursionError`;
/// `depth` bounds the native callback recursion.
#[allow(clippy::too_many_arguments)]
fn run_frames(
    entry: &CodeObject,
    functions: &[CodeObject],
    args: &[Value],
    cells: &[Value],
    model: &mut ObjectModel,
    is_module: bool,
    depth: usize,
    module_id: u16,
) -> Result<Value, Trap> {
    if depth > MAX_CALL_DEPTH {
        return Err(Trap::RecursionError);
    }
    let saved_module = model.set_current_module(module_id);
    let mut frames: Vec<Frame> = Vec::new();
    let outcome = match new_frame(entry, CodeId::Entry, args, is_module, cells, module_id, model) {
        Ok(frame) => {
            frames.push(frame);
            drive(&mut frames, entry, functions, model, depth)
        }
        Err(trap) => Err(trap),
    };
    model.set_current_module(saved_module);
    match outcome? {
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
///
/// Announces this loop to the model and un-announces it on EVERY exit path, so whether a loop is the
/// outermost one is decided by the nesting that actually happened rather than by a number its caller
/// passed in. That distinction is the collector's safe point (see [`ObjectModel::enter_drive`]); keeping
/// the bookkeeping here means a call site cannot get it wrong.
fn drive(
    frames: &mut Vec<Frame>,
    entry: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
) -> Result<DriveOutcome, Trap> {
    #[cfg(feature = "gc-collect")]
    let outermost = model.enter_drive();
    #[cfg(not(feature = "gc-collect"))]
    let outermost = false;
    let outcome = drive_frames(frames, entry, functions, model, depth, outermost);
    #[cfg(feature = "gc-collect")]
    model.leave_drive();
    outcome
}

/// The loop itself. `outermost` says whether this is the only driver loop running, which is what the
/// safe point requires; [`drive`] is the only caller and computes it. Without the collector there is no
/// safe point, so nothing reads it.
#[cfg_attr(not(feature = "gc-collect"), allow(unused_variables))]
fn drive_frames(
    frames: &mut Vec<Frame>,
    entry: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
    depth: usize,
    outermost: bool,
) -> Result<DriveOutcome, Trap> {
    let mut current_module = u16::MAX;
    let mut home_funcs: Option<Rc<[CodeObject]>> = None;
    let mut home_body: Option<Rc<CodeObject>> = None;
    loop {
        #[cfg(feature = "gc-collect")]
        if outermost && model.under_memory_pressure() {
            model.collect(&mut |visit| {
                for frame in frames.iter_mut() {
                    frame.trace(visit);
                }
            });
        }
        let top = frames.len() - 1;
        if frames[top].module != current_module {
            current_module = frames[top].module;
            home_funcs = model.managed_functions_rc(current_module);
            home_body = model.managed_module_body_rc(current_module);
            model.set_current_module(current_module);
        }
        let functions: &[CodeObject] = home_funcs.as_deref().unwrap_or(functions);
        let code = resolve_code(frames[top].code, entry, functions, home_body.as_deref())?;
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
                let value = if frame.is_module {
                    let name = code.local_names.get(idx as usize).ok_or(Trap::Malformed)?;
                    match model.current_module_global(name) {
                        Some(value) => value,
                        None => frame.load_local(idx as usize)?,
                    }
                } else {
                    frame.load_local(idx as usize)?
                };
                frame.push(value);
            }
            Op::StoreFast(idx) => {
                let value = frame.pop()?;
                frame.store_local(idx as usize, value)?;
                if frame.is_module {
                    let name = code.local_names.get(idx as usize).ok_or(Trap::Malformed)?;
                    model.set_current_module_global(name, value);
                }
            }
            Op::LoadGlobal(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let value = resolve_global(name, functions, model).ok_or(Trap::NameError)?;
                frame.push(value);
            }
            Op::StoreGlobal(name_idx) => {
                let value = frame.pop()?;
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                model.set_current_module_global(name, value);
            }
            Op::LoadAttr { name, cache } => {
                let receiver = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                if let Some(value) = descriptor_read(receiver, attr, functions, model, depth)? {
                    frame.push(value);
                } else {
                    let slot = frame.caches.get_mut(cache as usize).ok_or(Trap::Malformed)?;
                    match model.getattr(receiver, attr, slot) {
                        Ok(value) => {
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
                        Err(Trap::AttributeError) => match model.find_dunder(receiver, "__getattr__") {
                            Some(hook) => {
                                let name_value = model.new_str(attr)?;
                                let result =
                                    call_value(hook, &[name_value], functions, model, depth + 1)?;
                                frame.push(result);
                            }
                            None => return Err(model.attribute_error(receiver, attr)),
                        },
                        Err(other) => return Err(other),
                    }
                }
            }
            Op::Binary(binop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let result = match dispatch_binary(binop, lhs, rhs, functions, model, depth) {
                    Ok(value) => value,
                    Err(Trap::TypeError) => return Err(model.binop_type_error(binop, lhs, rhs)),
                    Err(other) => return Err(other),
                };
                frame.push(result);
            }
            Op::InplaceBinOp(binop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let result = match dispatch_inplace_binary(binop, lhs, rhs, functions, model, depth) {
                    Ok(value) => value,
                    Err(Trap::TypeError) => {
                        return Err(model.inplace_binop_type_error(binop, lhs, rhs));
                    }
                    Err(other) => return Err(other),
                };
                frame.push(result);
            }
            Op::Unary(unop) => {
                let value = frame.pop()?;
                let result = if let Some(method) = model.find_dunder(value, unary_dunder_name(unop)) {
                    call_value(method, &[], functions, model, depth + 1)?
                } else {
                    match unary(unop, value, model) {
                        Ok(value) => value,
                        Err(Trap::TypeError) => return Err(model.unary_type_error(unop, value)),
                        Err(other) => return Err(other),
                    }
                };
                frame.push(result);
            }
            Op::Compare(cmpop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let result = if matches!(cmpop, CmpOp::Is | CmpOp::IsNot) {
                    let same = lhs == rhs;
                    Value::from_bool(if matches!(cmpop, CmpOp::IsNot) { !same } else { same })
                } else {
                    match dispatch_compare(cmpop, lhs, rhs, functions, model, depth) {
                        Ok(value) => value,
                        Err(Trap::TypeError)
                            if matches!(cmpop, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) =>
                        {
                            return Err(model.compare_type_error(cmpop, lhs, rhs));
                        }
                        Err(other) => return Err(other),
                    }
                };
                frame.push(result);
            }
            Op::Subscript { cache: _ } => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                let class_getitem = if model.is_class(container) {
                    model.class_method_dunder(container, "__class_getitem__")
                } else {
                    None
                };
                let result = if let Some(func) = class_getitem {
                    call_value(func, &[container, index], functions, model, depth + 1)?
                } else if let Some(method) = model.find_dunder(container, "__getitem__") {
                    call_value(method, &[index], functions, model, depth + 1)?
                } else {
                    let index = if model.is_dict(container) {
                        index
                    } else {
                        coerce_subscript(index, functions, model, depth)?
                    };
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
                } else if model.is_slice(index)
                    && (model.is_list(container) || model.is_bytearray(container))
                {
                    let elements = crate::builtins::collect_iterable(model, &[value], functions, depth)?;
                    model.seq_setitem_slice(container, index, elements)?;
                } else {
                    let index = if model.is_dict(container) {
                        index
                    } else {
                        coerce_subscript(index, functions, model, depth)?
                    };
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
                } else if model.is_dict_view(container) {
                    let elems = model.dict_view_elems(container)?.unwrap_or_default();
                    elems_contain(element, &elems, functions, model, depth)?
                } else if model.is_deque(container) {
                    let elems = model.deque_elems(container).cloned().unwrap_or_default();
                    elems_contain(element, &elems, functions, model, depth)?
                } else if model.is_instance(container) {
                    let iterator = iterator_for(container, functions, model, depth + 1)?;
                    let mut found = false;
                    while let Some(item) = py_next_value(iterator, functions, model, depth)? {
                        if elem_eq(element, item, functions, model, depth)? {
                            found = true;
                            break;
                        }
                    }
                    found
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
                if callee.as_builtin_id() == Some(crate::builtins::Builtin::Locals.id())
                    && call_args.is_empty()
                {
                    let result = build_frame_locals(frame, code, model)?;
                    frame.push(result);
                } else if callee.as_builtin_id() == Some(crate::builtins::Builtin::Dir.id())
                    && call_args.is_empty()
                {
                    let bindings = build_frame_locals(frame, code, model)?;
                    let result = model.sorted_key_names(bindings)?;
                    frame.push(result);
                } else {
                    match plan_py_call(callee, call_args, &[], functions, model)? {
                        PyCallPlan::Frame(pending) if pending.is_generator => {
                            let module = pending.module;
                            let generator = new_planned_frame(&pending, functions, model)?;
                            frame.push(model.new_generator(generator, module)?);
                        }
                        PyCallPlan::Frame(pending) => return Ok(Flow::Call(pending)),
                        PyCallPlan::NotAFrame(call_args) => {
                            let result = call_value(callee, &call_args, functions, model, depth + 1)?;
                            frame.push(result);
                        }
                    }
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
                    model.py_setattr_instance(exception, "__suppress_context__", Value::TRUE)?;
                    exception
                } else if argc == 1 {
                    let value = frame.pop()?;
                    model.raise_value(value)?
                } else {
                    match frame.handling() {
                        Some(active) => active,
                        None => {
                            let message = "No active exception to reraise";
                            return Err(model.raise_named_exception("RuntimeError", message));
                        }
                    }
                };
                if argc != 0 {
                    if let Some(active) = frame.handling() {
                        if active != exception {
                            model.py_setattr_instance(exception, "__context__", active)?;
                        }
                    }
                }
                model.set_pending_exception(exception);
                return Err(Trap::Raised);
            }
            Op::MatchExc => {
                let exc_type = frame.pop()?;
                let active = frame.handling().ok_or(Trap::Malformed)?;
                let matched = if let Some(types) = model.seq_value(exc_type).cloned() {
                    types.iter().any(|&ty| model.exception_isinstance(active, ty))
                } else {
                    model.exception_isinstance(active, exc_type)
                };
                frame.push(Value::from_bool(matched));
            }
            Op::LoadExc => {
                let active = frame.handling().ok_or(Trap::Malformed)?;
                frame.push(active);
            }
            Op::PopExcept => {
                frame.handled.pop();
            }
            Op::Reraise => {
                let active = frame.handling().ok_or(Trap::Malformed)?;
                model.set_pending_exception(active);
                return Err(Trap::Raised);
            }
            Op::MakeFunction { func, flags } => {
                let name = code.names.get(func as usize).ok_or(Trap::Malformed)?;
                let index = functions
                    .iter()
                    .position(|f| f.name == *name)
                    .ok_or(Trap::NameError)? as u32;
                let home = model.current_module();
                if flags == 0 {
                    frame.push(Value::function_ref_in_module(home, index));
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
                        Some(cells) => model.new_closure(index, defaults, kwdefaults, cells, home)?,
                        None => model.new_py_function(index, defaults, kwdefaults, home)?,
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
                match plan_py_call(callee, call_args, &kwargs, functions, model)? {
                    PyCallPlan::Frame(pending) if pending.is_generator => {
                        let module = pending.module;
                        let generator = new_planned_frame(&pending, functions, model)?;
                        frame.push(model.new_generator(generator, module)?);
                    }
                    PyCallPlan::Frame(pending) => return Ok(Flow::Call(pending)),
                    PyCallPlan::NotAFrame(call_args) => {
                        let result =
                            call_value_kw(callee, &call_args, &kwargs, functions, model, depth + 1)?;
                        frame.push(result);
                    }
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
                            let items =
                                crate::builtins::collect_iterable(model, &[val], functions, depth)?;
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
                match plan_py_call(callee, posargs, &kwargs, functions, model)? {
                    PyCallPlan::Frame(pending) if pending.is_generator => {
                        let module = pending.module;
                        let generator = new_planned_frame(&pending, functions, model)?;
                        frame.push(model.new_generator(generator, module)?);
                    }
                    PyCallPlan::Frame(pending) => return Ok(Flow::Call(pending)),
                    PyCallPlan::NotAFrame(posargs) => {
                        let result =
                            call_value_kw(callee, &posargs, &kwargs, functions, model, depth + 1)?;
                        frame.push(result);
                    }
                }
            }
            Op::Yield => {
                return Ok(Flow::Yield(frame.pop()?));
            }
            Op::YieldFrom => {
                let action = if let Some(exc) = model.take_yield_from_throw() {
                    YieldFromAction::Throw(exc)
                } else if frame.yield_from_active {
                    YieldFromAction::Send(frame.pop()?)
                } else {
                    frame.yield_from_active = true;
                    YieldFromAction::Send(Value::NONE)
                };
                let sub = frame.peek()?;
                match advance_yield_from(sub, action, functions, model, depth) {
                    Ok(YieldFromStep::Yielded(value)) => {
                        frame.ip -= 1;
                        return Ok(Flow::Yield(value));
                    }
                    Ok(YieldFromStep::Returned(result)) => {
                        frame.pop()?;
                        frame.push(result);
                        frame.yield_from_active = false;
                    }
                    Err(trap) => {
                        frame.yield_from_active = false;
                        return Err(trap);
                    }
                }
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
                match begin_import(name, model)? {
                    ImportOutcome::Ready(module) => frame.push(module),
                    ImportOutcome::RunBody { module_id, completion } => {
                        return Ok(Flow::ImportBody { module_id, completion })
                    }
                }
            }
            Op::ImportFrom(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let module = frame.peek()?;
                let value = model.import_from(module, name)?;
                frame.push(value);
            }
            Op::ImportStar => {
                let module = frame.pop()?;
                model.import_star(module)?;
            }
            Op::BuildClassKw { kwnames } => {
                let names = match code.consts.get(kwnames as usize) {
                    Some(Const::KwNames(names)) => names.clone(),
                    _ => return Err(Trap::Malformed),
                };
                let namespace = match frame.class_namespace.take() {
                    Some(namespace) => namespace,
                    None => frame.pop()?,
                };
                let mut values = Vec::with_capacity(names.len());
                for _ in 0..names.len() {
                    values.push(frame.pop()?);
                }
                values.reverse();
                let bases = frame.pop()?;
                let name = frame.pop()?;
                let class = model.new_class(name, bases, namespace)?;
                model.set_class_module(class, model.current_module())?;
                for (name_value, hook) in model.set_name_hooks(class) {
                    call_value(hook, &[class, name_value], functions, model, depth + 1)?;
                }
                let kwargs: Vec<(&str, Value)> =
                    names.iter().map(alloc::string::String::as_str).zip(values).collect();
                match model.inherited_init_subclass(class) {
                    Some(hook) => {
                        call_value_kw(hook, &[class], &kwargs, functions, model, depth + 1)?;
                    }
                    None => {
                        let class_name = model.class_display_name(class);
                        let message = alloc::format!(
                            "{class_name}.__init_subclass__() takes no keyword arguments"
                        );
                        return Err(model.raise_named_exception("TypeError", &message));
                    }
                }
                frame.push(class);
            }
            Op::BuildClass => {
                let namespace = match frame.class_namespace.take() {
                    Some(namespace) => namespace,
                    None => frame.pop()?,
                };
                let bases = frame.pop()?;
                let name = frame.pop()?;
                let class = model.new_class(name, bases, namespace)?;
                model.set_class_module(class, model.current_module())?;
                for (name_value, hook) in model.set_name_hooks(class) {
                    call_value(hook, &[class, name_value], functions, model, depth + 1)?;
                }
                if let Some(hook) = model.inherited_init_subclass(class) {
                    call_value(hook, &[class], functions, model, depth + 1)?;
                }
                frame.push(class);
            }
            Op::SetAttr { name, cache: _ } => {
                let object = frame.pop()?;
                let value = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                set_attr(object, attr, value, functions, model, depth)?;
            }
            Op::DeleteItem => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                if let Some(method) = model.find_dunder(container, "__delitem__") {
                    call_value(method, &[index], functions, model, depth + 1)?;
                } else {
                    let index = if model.is_dict(container) {
                        index
                    } else {
                        coerce_subscript(index, functions, model, depth)?
                    };
                    model.py_delitem_dyn(container, index, functions, depth)?;
                }
            }
            Op::DeleteAttr { name } => {
                let object = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                if model.is_instance(object) {
                    if let Some(hook) = model.find_dunder(object, "__delattr__") {
                        let name_value = model.new_str(attr)?;
                        call_value(hook, &[name_value], functions, model, depth + 1)?;
                    } else if let Some(delete) = model.instance_delete_descriptor(object, attr) {
                        call_value(delete, &[object], functions, model, depth + 1)?;
                    } else {
                        model.py_delattr_instance(object, attr)?;
                    }
                } else {
                    return Err(Trap::AttributeError);
                }
            }
            Op::DeleteFast(idx) => {
                frame.store_local(idx as usize, Value::UNBOUND)?;
                if frame.is_module {
                    let name = code.local_names.get(idx as usize).ok_or(Trap::Malformed)?;
                    model.delete_current_module_global(name);
                }
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
                let self_value = frame.load_local(0)?;
                let class = match model.current_module_global(class_name) {
                    Some(class) => class,
                    None => {
                        let verified = class_name
                            .rsplit_once('.')
                            .and_then(|(_, own)| model.current_module_global(own))
                            .filter(|&candidate| model.is_class(candidate))
                            .filter(|&candidate| {
                                model.is_instance_of(self_value, candidate)
                                    || (model.is_class(self_value)
                                        && model.is_subclass_of(self_value, candidate))
                            });
                        match verified {
                            Some(class) => class,
                            None => {
                                let message = alloc::format!(
                                    "super(): cannot resolve the enclosing class '{class_name}' -- \
                                     a class defined inside a function is not supported yet"
                                );
                                return Err(model.raise_named_exception("RuntimeError", &message));
                            }
                        }
                    }
                };
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
                let mut import = None;
                if let Some(mut done) = frames.pop() {
                    import = done.finishes_import.take().map(|pending| (done.module, pending));
                    model.recycle_frame(done);
                }
                let handed = match import {
                    Some((module_id, pending)) => match finish_import(module_id, pending, model) {
                        Ok(module) => module,
                        Err(trap) => {
                            let ip = frames.last().map_or(0, |f| (f.ip as u32).saturating_sub(1));
                            unwind_exception(trap, frames, ip, entry, functions, model)?;
                            continue;
                        }
                    },
                    None => value,
                };
                match frames.last_mut() {
                    Some(caller) => caller.push(handed),
                    None => return Ok(DriveOutcome::Returned(handed)),
                }
            }
            Ok(Flow::Yield(value)) => return Ok(DriveOutcome::Yielded(value)),
            Ok(Flow::Call(pending)) => {
                let trap = if frames.len() >= MAX_CALL_DEPTH {
                    Some(Trap::RecursionError)
                } else {
                    new_planned_frame(&pending, functions, model).map_or_else(Some, |new| {
                        frames.push(new);
                        None
                    })
                };
                if let Some(trap) = trap {
                    unwind_exception(trap, frames, faulting_ip, entry, functions, model)?;
                }
            }
            Ok(Flow::ImportBody { module_id, completion }) => {
                let name = completion.name.clone();
                let trap = if frames.len() >= MAX_CALL_DEPTH {
                    Some(Trap::RecursionError)
                } else {
                    match model.managed_module_body_rc(module_id) {
                        None => Some(Trap::Malformed),
                        Some(body) => {
                            new_frame(&body, CodeId::ModuleBody, &[], true, &[], module_id, model)
                                .map_or_else(Some, |mut new| {
                                    new.finishes_import = Some(completion);
                                    frames.push(new);
                                    None
                                })
                        }
                    }
                };
                if let Some(trap) = trap {
                    model.uncache_module(&name);
                    unwind_exception(trap, frames, faulting_ip, entry, functions, model)?;
                }
            }
            Err(trap) => unwind_exception(trap, frames, faulting_ip, entry, functions, model)?,
        }
    }
}

/// Routes an in-flight exception/trap across the frame stack. The faulting frame searches its exception
/// table at `faulting_ip`; each caller searches at its pending call op (`ip - 1`). The first covering
/// handler CATCHES (truncate its stack to the handler depth, bind the exception, jump to the handler)
/// and this returns `Ok(())`. If the stack empties with no handler, the exception escapes -- it stays
/// pending on the model and this returns `Err(trap)` for the driver's caller. A non-catchable internal
/// fault (no exception object) also returns `Err(trap)`.
fn unwind_exception(
    trap: Trap,
    frames: &mut Vec<Frame>,
    faulting_ip: u32,
    entry: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
) -> Result<(), Trap> {
    let exception = match model.take_pending_exception() {
        Some(exception) => exception,
        None => match model.trap_to_exception(trap) {
            Some(exception) => exception,
            None => return Err(trap),
        },
    };
    if let Some(active) = frames.last().and_then(Frame::handling) {
        model.chain_context_if_unset(exception, active)?;
    }
    let mut search_ip = faulting_ip;
    loop {
        let top = frames.len() - 1;
        let home_funcs = model.managed_functions_rc(frames[top].module);
        let home_body = model.managed_module_body_rc(frames[top].module);
        let functions: &[CodeObject] = home_funcs.as_deref().unwrap_or(functions);
        let code = resolve_code(frames[top].code, entry, functions, home_body.as_deref())?;
        if let Some(handler) = find_handler(&code.exc_table, search_ip) {
            frames[top].enter_handler(exception, handler);
            return Ok(());
        }
        if let Some(pending) = frames[top].finishes_import.take() {
            model.uncache_module(&pending.name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::PyType;
    use lamella_py_bytecode::{Bundle, Module, Param, StaticType};

    #[test]
    fn frame_pool_recycles_buffers() {
        let mut model = no_objects();
        let c = code(2, 0, vec![], vec![], 0, vec![]);
        let f1 = new_frame(&c, CodeId::Entry, &[], false, &[], 0, &mut model).unwrap();
        let cap = f1.locals.capacity();
        model.recycle_frame(f1);
        let f2 = new_frame(&c, CodeId::Func(0), &[], false, &[], 0, &mut model).unwrap();
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
            doc: None,
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
        let mut m = ObjectModel::new(Vec::new(), 16 * 1024);
        assert_eq!(bind_arguments(&c, &[f(1), f(2), f(3)], &[], &[], Value::NONE, &mut m), Err(Trap::Raised));
        assert_eq!(bind_arguments(&c, &[f(1)], &[], &[], Value::NONE, &mut m), Err(Trap::Raised));
        let exc = m.take_pending_exception().unwrap();
        assert_eq!(m.exception_type_name(exc), Some("TypeError"));
        assert_eq!(bind_arguments(&c, &[f(1)], &[("nope", f(2))], &[], Value::NONE, &mut m), Err(Trap::Raised));
        let exc = m.take_pending_exception().unwrap();
        assert_eq!(m.exception_type_name(exc), Some("TypeError"));
        assert!(
            m.repr(exc).contains("got an unexpected keyword argument 'nope'"),
            "the unexpected keyword is named: {}",
            m.repr(exc)
        );
        assert_eq!(bind_arguments(&c, &[f(1)], &[("a0", f(2))], &[], Value::NONE, &mut m), Err(Trap::Raised));
        let exc = m.take_pending_exception().unwrap();
        assert!(
            m.repr(exc).contains("got multiple values for argument 'a0'"),
            "the doubly-bound parameter is named: {}",
            m.repr(exc)
        );
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
        let pyfunc = model.new_py_function(0, defaults, Value::NONE, 0).unwrap();
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
        assert_eq!(bind_arguments(&c, &[f(5)], &[], &[], Value::NONE, &mut m), Err(Trap::Raised));
        let exc = m.take_pending_exception().unwrap();
        assert_eq!(m.exception_type_name(exc), Some("TypeError"));
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
        let init_fn = model.new_py_function(0, defaults, Value::NONE, 0).unwrap();
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
    fn module_body_loadfast_reads_the_module_global() {
        use Op::*;
        let mut body = code(1, 0, Vec::new(), Vec::new(), 0, vec![LoadFast(0), Return]);
        body.local_names = vec![String::from("count")];
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        model.set_global("count", Value::fixnum(42).unwrap());
        let result = run_module(&body, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(42));
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
        let pyfunc = model.new_py_function(0, defaults, Value::NONE, 0).unwrap();
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
    fn a_call_arity_typeerror_is_caught_at_the_call_site() {
        use Op::*;
        let mut needs_two =
            code(2, 2, vec![Const::None], Vec::new(), 0, vec![LoadConst(0), Return]);
        needs_two.name = String::from("needs_two");
        let mut main = code(0, 0, vec![Const::Int(7)], vec![String::from("needs_two")], 0,
            vec![LoadGlobal(0), Call(0), PopTop, PopExcept, LoadConst(0), Return]);
        main.exc_table = vec![ExcEntry { start: 0, end: 2, target: 3, depth: 0 }];
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run(&main, core::slice::from_ref(&needs_two), &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(7));
    }

    #[test]
    fn an_uncaught_call_arity_error_escapes_as_a_typeerror() {
        use Op::*;
        let mut needs_two =
            code(2, 2, vec![Const::None], Vec::new(), 0, vec![LoadConst(0), Return]);
        needs_two.name = String::from("needs_two");
        let main = code(0, 0, Vec::new(), vec![String::from("needs_two")], 0,
            vec![LoadGlobal(0), Call(0), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&main, core::slice::from_ref(&needs_two), &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().unwrap();
        assert_eq!(model.exception_type_name(exc), Some("TypeError"));
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
    fn inplace_binop_extends_a_list_in_place_preserving_identity() {
        use Op::*;
        let code = code(
            2,
            0,
            vec![Const::Int(1), Const::Int(2), Const::Int(3)],
            Vec::new(),
            0,
            vec![
                LoadConst(0),
                BuildList(1),
                StoreFast(0),
                LoadFast(0),
                StoreFast(1),
                LoadFast(0),
                LoadConst(1),
                LoadConst(2),
                BuildTuple(2),
                InplaceBinOp(BinOp::Add),
                StoreFast(0),
                LoadFast(0),
                LoadFast(1),
                Compare(CmpOp::Is),
                LoadFast(1),
                BuildTuple(2),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let result = run(&code, &[], &[], &mut model).unwrap();
        assert_eq!(model.repr(result), "(True, [1, 2, 3])");
    }

    #[test]
    fn inplace_binop_falls_back_to_the_plain_op_for_immutables() {
        use Op::*;
        let code = code(
            1,
            0,
            vec![Const::Int(5), Const::Int(1)],
            Vec::new(),
            0,
            vec![
                LoadConst(0),
                StoreFast(0),
                LoadFast(0),
                LoadConst(1),
                InplaceBinOp(BinOp::Add),
                StoreFast(0),
                LoadFast(0),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model).unwrap().as_fixnum(), Some(6));
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
        frame.enter_handler(exc, ExcEntry { start: 0, end: 1, target: 0, depth: 0 });

        model.heap_mut().collect(|visit| frame.trace(visit));

        let relocated = frame.handling().expect("the in-flight exception survives the collection");
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


    /// A `"<module>"` body with explicit `local_names` (a module body mirrors each top-level
    /// `StoreFast` into a global by its local name) and `names` pool.
    fn module_body(
        local_names: &[&str],
        consts: Vec<Const>,
        names: &[&str],
        cache_count: usize,
        ops: Vec<Op>,
    ) -> CodeObject {
        let name_pool = names.iter().map(|s| String::from(*s)).collect();
        let mut co = code(local_names.len(), 0, consts, name_pool, cache_count, ops);
        co.name = String::from("<module>");
        co.local_names = local_names.iter().map(|s| String::from(*s)).collect();
        co
    }

    /// A named member of a module's function table, with `n_args` params and an explicit `names` pool.
    fn named_fn(name: &str, n_args: usize, n_locals: usize, names: &[&str], ops: Vec<Op>) -> CodeObject {
        let name_pool = names.iter().map(|s| String::from(*s)).collect();
        let mut co = code(n_locals, n_args, Vec::new(), name_pool, 0, ops);
        co.name = String::from(name);
        co
    }

    fn managed_module(name: &str, functions: Vec<CodeObject>, body: CodeObject) -> Module {
        Module { name: String::from(name), functions, body }
    }

    #[test]
    fn managed_module_body_runs_in_its_own_namespace() {
        use Op::*;
        let m = managed_module(
            "m",
            Vec::new(),
            module_body(&["X"], vec![Const::Int(10), Const::None], &[], 0,
                vec![LoadConst(0), StoreFast(0), LoadConst(1), Return]),
        );
        let entry = module_body(&["m"], Vec::new(), &["m", "X"], 1, vec![
            ImportName(0),
            StoreFast(0),
            LoadFast(0),
            LoadAttr { name: 1, cache: 0 },
            Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(vec![m]);
        let result = run_module(&entry, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(10));
        assert!(model.get_global("X").is_none());
        assert!(model.is_module_object(model.get_global("m").unwrap()));
    }

    #[test]
    fn from_managed_module_import_binds_the_member() {
        use Op::*;
        let config = managed_module(
            "config",
            Vec::new(),
            module_body(&["MAX"], vec![Const::Int(99), Const::None], &[], 0,
                vec![LoadConst(0), StoreFast(0), LoadConst(1), Return]),
        );
        let entry = module_body(&["MAX"], Vec::new(), &["config", "MAX"], 0, vec![
            ImportName(0),
            ImportFrom(1),
            StoreFast(0),
            PopTop,
            LoadFast(0),
            Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(vec![config]);
        let result = run_module(&entry, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(99));
        assert_eq!(model.get_global("MAX").and_then(Value::as_fixnum), Some(99));
    }

    #[test]
    fn a_managed_module_function_resolves_module_globals_during_the_body() {
        use Op::*;
        let compute = named_fn("compute", 0, 0, &["BASE"], vec![LoadGlobal(0), Return]);
        let calc = managed_module(
            "calc",
            vec![compute],
            module_body(&["BASE", "RESULT"], vec![Const::Int(5), Const::None], &["compute"], 0, vec![
                LoadConst(0),
                StoreFast(0),
                LoadGlobal(0),
                Call(0),
                StoreFast(1),
                LoadConst(1),
                Return,
            ]),
        );
        let entry = module_body(&["calc"], Vec::new(), &["calc", "RESULT"], 1, vec![
            ImportName(0),
            StoreFast(0),
            LoadFast(0),
            LoadAttr { name: 1, cache: 0 },
            Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(vec![calc]);
        let result = run_module(&entry, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(5));
    }

    #[test]
    fn a_native_module_shadows_a_managed_one_of_the_same_name() {
        use Op::*;
        let managed_math = managed_module(
            "math",
            Vec::new(),
            module_body(&["pi"], vec![Const::Int(999), Const::None], &[], 0,
                vec![LoadConst(0), StoreFast(0), LoadConst(1), Return]),
        );
        let entry = module_body(&["math"], Vec::new(), &["math", "pi"], 1, vec![
            ImportName(0),
            StoreFast(0),
            LoadFast(0),
            LoadAttr { name: 1, cache: 0 },
            Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(vec![managed_math]);
        let pi = run_module(&entry, &[], &mut model).unwrap();
        let pi = model.float_value(pi).expect("native math.pi is a float");
        assert!((pi - core::f64::consts::PI).abs() < 1e-9);
    }

    #[test]
    fn a_circular_managed_import_terminates_via_cache_before_run() {
        use Op::*;
        let selfmod = managed_module(
            "selfmod",
            Vec::new(),
            module_body(&["selfmod", "X"], vec![Const::Int(1), Const::None], &["selfmod"], 0, vec![
                ImportName(0),
                StoreFast(0),
                LoadConst(0),
                StoreFast(1),
                LoadConst(1),
                Return,
            ]),
        );
        let entry = module_body(&["selfmod"], Vec::new(), &["selfmod", "X"], 1, vec![
            ImportName(0),
            StoreFast(0),
            LoadFast(0),
            LoadAttr { name: 1, cache: 0 },
            Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(vec![selfmod]);
        let result = run_module(&entry, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(1));
    }

    #[test]
    fn an_import_of_neither_a_native_nor_a_managed_module_raises() {
        use Op::*;
        let entry = module_body(&["x"], vec![Const::None], &["nope"], 0,
            vec![ImportName(0), StoreFast(0), LoadConst(0), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(Vec::new());
        assert_eq!(run_module(&entry, &[], &mut model), Err(Trap::Raised));
    }

    #[test]
    fn a_module_function_resolves_its_module_globals_when_called_after_import() {
        use Op::*;
        let compute = named_fn("compute", 0, 0, &["BASE"], vec![LoadGlobal(0), Return]);
        let helpers = managed_module(
            "helpers",
            vec![compute],
            module_body(&["BASE", "compute"], vec![Const::Int(5), Const::None], &["compute"], 0, vec![
                LoadConst(0),
                StoreFast(0),
                MakeFunction { func: 0, flags: 0 },
                StoreFast(1),
                LoadConst(1),
                Return,
            ]),
        );
        let entry = module_body(
            &["BASE", "compute"],
            vec![Const::Int(99)],
            &["helpers", "compute"],
            0,
            vec![
                LoadConst(0),
                StoreFast(0),
                ImportName(0),
                ImportFrom(1),
                StoreFast(1),
                PopTop,
                LoadFast(1),
                Call(0),
                Return,
            ],
        );
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        model.set_managed_modules(vec![helpers]);
        let result = run_module(&entry, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(5));
        assert_eq!(model.get_global("BASE").and_then(Value::as_fixnum), Some(99));
    }

    #[test]
    fn a_cross_module_call_chains_through_each_function_s_home_module() {
        use Op::*;
        let get = named_fn("get", 0, 0, &["VAL"], vec![LoadGlobal(0), Return]);
        let b = managed_module(
            "b",
            vec![get],
            module_body(&["VAL", "get"], vec![Const::Int(7), Const::None], &["get"], 0, vec![
                LoadConst(0),
                StoreFast(0),
                MakeFunction { func: 0, flags: 0 },
                StoreFast(1),
                LoadConst(1),
                Return,
            ]),
        );
        let relay = named_fn("relay", 0, 0, &["get"], vec![LoadGlobal(0), Call(0), Return]);
        let a = managed_module(
            "a",
            vec![relay],
            module_body(&["get", "relay"], vec![Const::None], &["b", "get", "relay"], 0, vec![
                ImportName(0),
                ImportFrom(1),
                StoreFast(0),
                PopTop,
                MakeFunction { func: 2, flags: 0 },
                StoreFast(1),
                LoadConst(0),
                Return,
            ]),
        );
        let entry = module_body(&["relay"], Vec::new(), &["a", "relay"], 0, vec![
            ImportName(0),
            ImportFrom(1),
            StoreFast(0),
            PopTop,
            LoadFast(0),
            Call(0),
            Return,
        ]);
        let mut model = ObjectModel::new(Vec::new(), 128 * 1024);
        model.set_managed_modules(vec![a, b]);
        let result = run_module(&entry, &[], &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(7));
    }


    #[test]
    fn run_bundle_installs_the_registry_then_runs_the_entry() {
        use Op::*;
        let compute = named_fn("compute", 0, 0, &["BASE"], vec![LoadGlobal(0), Return]);
        let helpers = managed_module(
            "helpers",
            vec![compute],
            module_body(&["BASE", "compute"], vec![Const::Int(5), Const::None], &["compute"], 0, vec![
                LoadConst(0),
                StoreFast(0),
                MakeFunction { func: 0, flags: 0 },
                StoreFast(1),
                LoadConst(1),
                Return,
            ]),
        );
        let entry_body = module_body(&["BASE", "compute"], vec![Const::Int(99)], &["helpers", "compute"], 0, vec![
            LoadConst(0),
            StoreFast(0),
            ImportName(0),
            ImportFrom(1),
            StoreFast(1),
            PopTop,
            LoadFast(1),
            Call(0),
            Return,
        ]);
        let bundle = Bundle {
            entry: managed_module("__main__", Vec::new(), entry_body),
            modules: vec![helpers],
        };
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run_bundle(bundle, &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(5));
        assert_eq!(model.get_global("BASE").and_then(Value::as_fixnum), Some(99));
    }

    #[test]
    fn a_managed_module_calling_an_entry_defined_function_resolves_it_against_the_entry() {
        use Op::*;
        let cb = named_fn("cb", 0, 0, &["X"], vec![LoadGlobal(0), Return]);
        let apply = named_fn("apply", 1, 1, &[], vec![LoadFast(0), Call(0), Return]);
        let m = managed_module(
            "m",
            vec![apply],
            module_body(&["apply"], vec![Const::None], &["apply"], 0, vec![
                MakeFunction { func: 0, flags: 0 },
                StoreFast(0),
                LoadConst(0),
                Return,
            ]),
        );
        let entry_body = module_body(&["X", "cb", "m"], vec![Const::Int(42)], &["cb", "m", "apply"], 1, vec![
            LoadConst(0),
            StoreFast(0),
            MakeFunction { func: 0, flags: 0 },
            StoreFast(1),
            ImportName(1),
            StoreFast(2),
            LoadFast(2),
            LoadAttr { name: 2, cache: 0 },
            LoadFast(1),
            Call(1),
            Return,
        ]);
        let bundle = Bundle {
            entry: managed_module("__main__", vec![cb], entry_body),
            modules: vec![m],
        };
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let result = run_bundle(bundle, &mut model).unwrap();
        assert_eq!(result.as_fixnum(), Some(42));
    }

    #[test]
    fn run_bundle_with_no_modules_is_a_plain_single_file_program() {
        use Op::*;
        let entry_body = module_body(&["x"], vec![Const::Int(42)], &[], 0, vec![
            LoadConst(0),
            StoreFast(0),
            LoadFast(0),
            Return,
        ]);
        let bundle = Bundle {
            entry: managed_module("__main__", Vec::new(), entry_body),
            modules: Vec::new(),
        };
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        assert_eq!(run_bundle(bundle, &mut model).unwrap().as_fixnum(), Some(42));
    }

    #[test]
    fn a_binary_op_type_error_raises_with_a_cpython_message() {
        use Op::*;
        let code = code(0, 0, vec![Const::Int(1), Const::Str(String::from("a"))], Vec::new(), 0,
            vec![LoadConst(0), LoadConst(1), Binary(BinOp::Add), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().expect("a pending TypeError");
        let repr = model.repr(exc);
        assert!(
            repr.contains("unsupported operand type(s) for +: 'int' and 'str'"),
            "got: {repr}"
        );
    }

    #[test]
    fn an_ordering_comparison_type_error_raises_with_a_cpython_message() {
        use Op::*;
        let code = code(0, 0, vec![Const::Int(1), Const::Str(String::from("a"))], Vec::new(), 0,
            vec![LoadConst(0), LoadConst(1), Compare(CmpOp::Lt), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().expect("a pending TypeError");
        let repr = model.repr(exc);
        assert!(
            repr.contains("'<' not supported between instances of 'int' and 'str'"),
            "got: {repr}"
        );
    }

    #[test]
    fn len_of_a_value_with_no_length_raises_with_a_cpython_message() {
        use Op::*;
        let code = code(0, 0, vec![Const::Int(5)], vec![String::from("len")], 0,
            vec![LoadGlobal(0), LoadConst(0), Call(1), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().expect("a pending TypeError");
        let repr = model.repr(exc);
        assert!(repr.contains("object of type 'int' has no len()"), "got: {repr}");
    }

    #[test]
    fn calling_a_non_callable_raises_with_a_cpython_message() {
        use Op::*;
        let code = code(0, 0, vec![Const::Int(5)], Vec::new(), 0,
            vec![LoadConst(0), Call(0), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().expect("a pending TypeError");
        let repr = model.repr(exc);
        assert!(repr.contains("'int' object is not callable"), "got: {repr}");
    }

    #[test]
    fn a_unary_op_type_error_raises_with_a_cpython_message() {
        use Op::*;
        let code = code(0, 0, vec![Const::Str(String::from("a"))], Vec::new(), 0,
            vec![LoadConst(0), Unary(UnaryOp::Neg), Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().expect("a pending TypeError");
        let repr = model.repr(exc);
        assert!(repr.contains("bad operand type for unary -: 'str'"), "got: {repr}");
    }

    #[test]
    fn a_missing_attribute_raises_attributeerror_with_a_cpython_message() {
        use Op::*;
        let code = code(0, 0, vec![Const::Int(5)], vec![String::from("foo")], 1,
            vec![LoadConst(0), LoadAttr { name: 0, cache: 0 }, Return]);
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Raised));
        let exc = model.take_pending_exception().expect("a pending AttributeError");
        let repr = model.repr(exc);
        assert!(repr.contains("'int' object has no attribute 'foo'"), "got: {repr}");
    }

    #[test]
    fn subscripting_a_non_subscriptable_carries_a_cpython_message() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let five = Value::fixnum(5).unwrap();
        let zero = Value::fixnum(0).unwrap();
        assert_eq!(model.py_getitem(five, zero), Err(Trap::TypeError));
        let exc = model.trap_to_exception(Trap::TypeError).expect("a TypeError exception");
        assert!(
            model.repr(exc).contains("'int' object is not subscriptable"),
            "got: {}",
            model.repr(exc)
        );
    }

    #[test]
    fn a_class_repr_is_qualified_by_its_module() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let name = model.new_str("Foo").unwrap();
        let ns = model.new_dict(Vec::new()).unwrap();
        let class = model.new_class(name, Value::NONE, ns).unwrap();
        model.set_class_module(class, 0).unwrap();
        assert_eq!(model.repr(class), "<class '__main__.Foo'>");
        let bare_name = model.new_str("Bar").unwrap();
        let bare_ns = model.new_dict(Vec::new()).unwrap();
        let bare = model.new_class(bare_name, Value::NONE, bare_ns).unwrap();
        assert_eq!(model.repr(bare), "<class 'Bar'>");
    }

    #[test]
    fn a_bad_index_type_carries_a_cpython_message() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let list = model.new_list(alloc::vec![Value::fixnum(1).unwrap()]).unwrap();
        let key = model.new_str("a").unwrap();
        assert_eq!(model.py_getitem(list, key), Err(Trap::TypeError));
        let exc = model.trap_to_exception(Trap::TypeError).expect("a TypeError");
        assert!(
            model.repr(exc).contains("list indices must be integers or slices, not str"),
            "got: {}",
            model.repr(exc)
        );
        let text = model.new_str("abc").unwrap();
        assert_eq!(model.py_getitem(text, Value::NONE), Err(Trap::TypeError));
        let exc2 = model.trap_to_exception(Trap::TypeError).expect("a TypeError");
        assert!(
            model.repr(exc2).contains("string indices must be integers, not 'NoneType'"),
            "got: {}",
            model.repr(exc2)
        );
    }

    #[test]
    fn the_selected_board_drives_the_gpio_register_map() {
        let board = crate::gpio::Board::Stm32f4;
        assert_eq!(board.pin_id("LED"), Some(13));
        assert_eq!(board.pin_id("PC5"), Some(5));
        assert_eq!(board.pin_id("PC99"), None);
        let regs = board.pin_regs(13);
        assert_eq!(regs.set_val, 1 << 13);
        assert_eq!(regs.clr_val, 1 << (13 + 16));
        let mut model = ObjectModel::new(Vec::new(), 4096);
        model.set_board(board);
    }

    #[test]
    fn a_managed_module_exports_a_plain_top_level_def_for_from_import() {
        use Op::*;
        let f = named_fn("f", 0, 0, &["VAL"], vec![LoadGlobal(0), Return]);
        let m = managed_module(
            "m",
            vec![f],
            module_body(&["VAL"], vec![Const::Int(7), Const::None], &[], 0,
                vec![LoadConst(0), StoreFast(0), LoadConst(1), Return]),
        );
        let entry_body = module_body(&["m", "f"], Vec::new(), &["m", "f"], 0, vec![
            ImportName(0),
            StoreFast(0),
            LoadFast(0),
            ImportFrom(1),
            StoreFast(1),
            PopTop,
            LoadFast(1),
            Call(0),
            Return,
        ]);
        let bundle = Bundle {
            entry: managed_module("__main__", Vec::new(), entry_body),
            modules: vec![m],
        };
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run_bundle(bundle, &mut model).unwrap().as_fixnum(), Some(7));
    }

    #[test]
    fn a_managed_module_exports_a_plain_top_level_def_for_attribute_access() {
        use Op::*;
        let f = named_fn("f", 0, 0, &["VAL"], vec![LoadGlobal(0), Return]);
        let m = managed_module(
            "m",
            vec![f],
            module_body(&["VAL"], vec![Const::Int(7), Const::None], &[], 0,
                vec![LoadConst(0), StoreFast(0), LoadConst(1), Return]),
        );
        let entry_body = module_body(&["m"], Vec::new(), &["m", "f"], 1, vec![
            ImportName(0),
            StoreFast(0),
            LoadFast(0),
            LoadAttr { name: 1, cache: 0 },
            Call(0),
            Return,
        ]);
        let bundle = Bundle {
            entry: managed_module("__main__", Vec::new(), entry_body),
            modules: vec![m],
        };
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        assert_eq!(run_bundle(bundle, &mut model).unwrap().as_fixnum(), Some(7));
    }
}
