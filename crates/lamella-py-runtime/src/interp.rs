//! The bytecode interpreter.

use alloc::vec::Vec;

use lamella_gc::Ref;
use lamella_py_bytecode::{BinOp, CmpOp, CodeObject, Const, ExcEntry, Op, UnaryOp};

use crate::object::{InlineCache, ObjectModel};
use crate::trap::Trap;
use crate::value::{Value, FIXNUM_MAX, FIXNUM_MIN};

/// One activation: the local-variable slots and the evaluation stack.
///
/// Both hold tagged [`Value`]s, so both are GC roots traced by tag ([`Frame::trace`]).
pub struct Frame {
    locals: Vec<Value>,
    stack: Vec<Value>,
}

impl Frame {
    /// A frame with `num_locals` slots, every local initially [`Value::UNBOUND`] so a
    /// read before assignment traps rather than reading garbage.
    #[must_use]
    pub fn new(num_locals: usize) -> Frame {
        let mut locals = Vec::with_capacity(num_locals);
        locals.resize(num_locals, Value::UNBOUND);
        Frame {
            locals,
            stack: Vec::new(),
        }
    }

    /// Pushes a value onto the evaluation stack.
    fn push(&mut self, value: Value) {
        self.stack.push(value);
    }

    /// Pops a value, or [`Trap::StackUnderflow`] on an empty stack.
    fn pop(&mut self) -> Result<Value, Trap> {
        self.stack.pop().ok_or(Trap::StackUnderflow)
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
        Const::Str(_) => Err(Trap::Unsupported),
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
/// a `ValueError`. A result outside the 31-bit fixnum range overflows (`Trap::Overflow`)
/// -- bignum promotion is not provided. `//` floors toward negative infinity
/// and `%` takes the divisor's sign (with `x == (x // y) * y + (x % y)`, Python 3.14.6
/// "Binary arithmetic operations"); a zero divisor raises `ZeroDivisionError`.
pub(crate) fn binary(op: BinOp, a: Value, b: Value) -> Result<Value, Trap> {
    let x = i128::from(a.as_int().ok_or(Trap::TypeError)?);
    let y = i128::from(b.as_int().ok_or(Trap::TypeError)?);
    let result: i128 = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::BitAnd => x & y,
        BinOp::BitOr => x | y,
        BinOp::BitXor => x ^ y,
        BinOp::LShift => {
            if y < 0 {
                return Err(Trap::ValueError);
            } else if x == 0 {
                0
            } else if y >= 31 {
                return Err(Trap::Overflow);
            } else {
                x << y
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
            let (q, r) = (x / y, x % y);
            if r != 0 && (r < 0) != (y < 0) { q - 1 } else { q }
        }
        BinOp::Mod => {
            if y == 0 {
                return Err(Trap::ZeroDivisionError);
            }
            let r = x % y;
            if r != 0 && (r < 0) != (y < 0) { r + y } else { r }
        }
    };
    if result < i128::from(FIXNUM_MIN) || result > i128::from(FIXNUM_MAX) {
        return Err(Trap::Overflow);
    }
    Value::fixnum(result as i32).ok_or(Trap::Overflow)
}

/// Evaluates a unary `-`/`+`/`~` over an `int`/`bool` operand (Python int semantics:
/// `+x == x`, `-x`, `~x == -x - 1`); other types are a `TypeError`. The customizable
/// `__neg__`/`__pos__`/`__invert__` protocol composes with the broader object model.
fn unary(op: UnaryOp, v: Value) -> Result<Value, Trap> {
    let x = i128::from(v.as_int().ok_or(Trap::TypeError)?);
    let result: i128 = match op {
        UnaryOp::Neg => -x,
        UnaryOp::Pos => x,
        UnaryOp::Invert => !x,
    };
    if result < i128::from(FIXNUM_MIN) || result > i128::from(FIXNUM_MAX) {
        return Err(Trap::Overflow);
    }
    Value::fixnum(result as i32).ok_or(Trap::Overflow)
}

/// Evaluates a comparison (Python 3.14.6 Language Reference, "Comparisons", 6.10).
///
/// `int`/`bool` operands compare numerically (numbers compare mathematically correct).
/// For any other operands the default applies: `==`/`!=` are based on object identity
/// (so `None == None` is true, and two distinct objects are unequal), and the ordering
/// operators `<`/`<=`/`>`/`>=` have no default and raise `TypeError`. The customizable
/// `__eq__`/`__lt__`/... protocol (the `py_compare` intrinsic) composes with the broader
/// object model.
fn compare(op: CmpOp, a: Value, b: Value) -> Result<Value, Trap> {
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        let result = match op {
            CmpOp::Lt => x < y,
            CmpOp::Le => x <= y,
            CmpOp::Eq => x == y,
            CmpOp::Ne => x != y,
            CmpOp::Gt => x > y,
            CmpOp::Ge => x >= y,
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
    exec(code, functions, args, model, 0, false)
}

/// Runs the module body (the top-level statements). Its local bindings mirror into the module
/// globals as they happen, so a function reaches a top-level name (a class, a global) by
/// `LoadGlobal`. Run this before invoking module functions.
pub fn run_module(
    body: &CodeObject,
    functions: &[CodeObject],
    model: &mut ObjectModel,
) -> Result<Value, Trap> {
    exec(body, functions, &[], model, 0, true)
}

/// Executes one function activation at call depth `depth` (0 for the entry). [`Op::Call`]
/// recurses through here, so the native call stack mirrors the Python one, bounded by
/// [`MAX_CALL_DEPTH`].
///
/// GC note: each activation roots only its own [`Frame`]; a collection that
/// ran mid-call would not see the suspended caller frames. The typed/recursive corpus
/// allocates nothing during a call, so this is not exercised -- a frame-chain root walk
/// covers it once allocation can occur inside a call.
/// One op's control-flow outcome: fall through to the next op, or return `value` from the
/// function. A jump just mutates `ip` and falls through with [`Flow::Next`].
enum Flow {
    Next,
    Return(Value),
}

/// The innermost `exc_table` entry whose protected range covers op `ip`, or `None`. Entries
/// are emitted innermost-first, so the first cover is the tightest handler.
fn find_handler(exc_table: &[ExcEntry], ip: u32) -> Option<ExcEntry> {
    exc_table.iter().copied().find(|e| e.start <= ip && ip < e.end)
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
        let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
        exec(code, functions, args, model, depth, false)
    } else if let Some(id) = callee.as_builtin_id() {
        crate::builtins::call_builtin(id, args, functions, model, depth)
    } else if model.is_bound_method(callee) {
        model.call_bound_method(callee, args)
    } else if model.is_py_bound(callee) {
        let func = model.bound_func(callee);
        let index = func.as_function_index().ok_or(Trap::TypeError)?;
        let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
        let mut method_args = Vec::with_capacity(args.len() + 1);
        method_args.push(model.bound_self(callee));
        method_args.extend_from_slice(args);
        exec(code, functions, &method_args, model, depth, false)
    } else if model.is_class(callee) {
        let instance = model.new_object(callee)?;
        if let Some(init) = model.find_init(callee) {
            let index = init.as_function_index().ok_or(Trap::TypeError)?;
            let code = functions.get(index as usize).ok_or(Trap::Malformed)?;
            let mut init_args = Vec::with_capacity(args.len() + 1);
            init_args.push(instance);
            init_args.extend_from_slice(args);
            exec(code, functions, &init_args, model, depth, false)?;
        }
        Ok(instance)
    } else {
        Err(Trap::TypeError)
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
    if let Some(iter_method) = model.find_dunder(value, "__iter__") {
        let result = call_value(iter_method, &[], functions, model, depth)?;
        if model.is_iter(result) {
            Ok(result)
        } else {
            model.new_iter(result)
        }
    } else {
        model.new_iter(value)
    }
}

fn exec(
    code: &CodeObject,
    functions: &[CodeObject],
    args: &[Value],
    model: &mut ObjectModel,
    depth: usize,
    is_module: bool,
) -> Result<Value, Trap> {
    if depth > MAX_CALL_DEPTH {
        return Err(Trap::RecursionError);
    }
    if args.len() != code.params.len() {
        return Err(Trap::TypeError);
    }
    let mut frame = Frame::new(code.n_locals);
    for (i, arg) in args.iter().enumerate() {
        frame.locals[i] = *arg;
    }
    let mut caches = Vec::with_capacity(code.cache_count);
    caches.resize(code.cache_count, InlineCache::empty());

    let mut active_exception: Option<Value> = None;
    let mut ip = 0usize;
    loop {
        let faulting_ip = ip as u32;
        let op = *code.ops.get(ip).ok_or(Trap::Malformed)?;
        ip += 1;
        let flow = (|| -> Result<Flow, Trap> {
            match op {
            Op::LoadConst(idx) => {
                let c = code.consts.get(idx as usize).ok_or(Trap::Malformed)?;
                let value = match c {
                    Const::Str(s) => model.new_str(s)?,
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
                if is_module {
                    let name = code.local_names.get(idx as usize).ok_or(Trap::Malformed)?;
                    model.set_global(name, value);
                }
            }
            Op::LoadGlobal(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let value = if let Some(index) = functions.iter().position(|f| f.name == *name) {
                    Value::function_ref(index as u32)
                } else if let Some(global) = model.get_global(name) {
                    global
                } else if let Some(id) = crate::builtins::builtin_id(name) {
                    Value::builtin_ref(id)
                } else if let Some(class) = model.exception_class(name) {
                    class
                } else {
                    return Err(Trap::NameError);
                };
                frame.push(value);
            }
            Op::LoadAttr { name, cache } => {
                let receiver = frame.pop()?;
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                let slot = caches.get_mut(cache as usize).ok_or(Trap::Malformed)?;
                let value = model.getattr(receiver, attr, slot)?;
                frame.push(value);
            }
            Op::Binary(binop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let result = match model.py_binary(binop, lhs, rhs)? {
                    Some(value) => value,
                    None => binary(binop, lhs, rhs)?,
                };
                frame.push(result);
            }
            Op::Unary(unop) => {
                let value = frame.pop()?;
                frame.push(unary(unop, value)?);
            }
            Op::Compare(cmpop) => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let eq_method = if matches!(cmpop, CmpOp::Eq | CmpOp::Ne) {
                    model.find_dunder(lhs, "__eq__")
                } else {
                    None
                };
                let result = if let Some(eq_method) = eq_method {
                    let outcome = call_value(eq_method, &[rhs], functions, model, depth + 1)?;
                    let equal = model.py_truthy(outcome)?.unwrap_or(true);
                    Value::from_bool(if matches!(cmpop, CmpOp::Ne) { !equal } else { equal })
                } else {
                    match model.py_compare(cmpop, lhs, rhs)? {
                        Some(value) => value,
                        None => compare(cmpop, lhs, rhs)?,
                    }
                };
                frame.push(result);
            }
            Op::Subscript { cache: _ } => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                frame.push(model.py_getitem(container, index)?);
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
                frame.push(model.new_dict(pairs)?);
            }
            Op::Setitem => {
                let index = frame.pop()?;
                let container = frame.pop()?;
                let value = frame.pop()?;
                model.py_setitem(container, index, value)?;
            }
            Op::Contains { negate } => {
                let container = frame.pop()?;
                let element = frame.pop()?;
                let result = model.py_contains(container, element)? ^ negate;
                frame.push(Value::from_bool(result));
            }
            Op::GetIter => {
                let iterable = frame.pop()?;
                let iterator = iterator_for(iterable, functions, model, depth + 1)?;
                frame.push(iterator);
            }
            Op::ForIter(target) => {
                let iterator = frame.pop()?;
                match model.py_next(iterator)? {
                    Some(value) => frame.push(value),
                    None => ip = target as usize,
                }
            }
            Op::PopTop => {
                frame.pop()?;
            }
            Op::Jump(target) => {
                ip = target as usize;
            }
            Op::PopJumpIfFalse(target) => {
                let value = frame.pop()?;
                let truthy = match model.py_truthy(value)? {
                    Some(b) => b,
                    None => value.is_truthy(),
                };
                if !truthy {
                    ip = target as usize;
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
                let result = call_value(callee, &call_args, functions, model, depth + 1)?;
                frame.push(result);
            }
            Op::Return => return Ok(Flow::Return(frame.pop()?)),
            Op::Raise(argc) => {
                let exception = if argc == 2 {
                    let _cause = frame.pop()?;
                    let value = frame.pop()?;
                    model.raise_value(value)?
                } else if argc == 1 {
                    let value = frame.pop()?;
                    model.raise_value(value)?
                } else {
                    match active_exception {
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
                let active = active_exception.ok_or(Trap::Malformed)?;
                frame.push(Value::from_bool(model.exception_isinstance(active, exc_type)));
            }
            Op::LoadExc => {
                let active = active_exception.ok_or(Trap::Malformed)?;
                frame.push(active);
            }
            Op::PopExcept => {
                active_exception = None;
            }
            Op::Reraise => {
                let active = active_exception.ok_or(Trap::Malformed)?;
                model.set_pending_exception(active);
                return Err(Trap::Raised);
            }
            Op::MakeFunction(name_idx) => {
                let name = code.names.get(name_idx as usize).ok_or(Trap::Malformed)?;
                let index = functions
                    .iter()
                    .position(|f| f.name == *name)
                    .ok_or(Trap::NameError)?;
                frame.push(Value::function_ref(index as u32));
            }
            Op::BuildClass => {
                let namespace = frame.pop()?;
                let base = frame.pop()?;
                let name = frame.pop()?;
                frame.push(model.new_class(name, base, namespace)?);
            }
            Op::SetAttr { name, cache: _ } => {
                let object = frame.pop()?;
                let value = frame.pop()?;
                if !model.is_instance(object) {
                    return Err(Trap::AttributeError);
                }
                let attr = code.names.get(name as usize).ok_or(Trap::Malformed)?;
                model.py_setattr_instance(object, attr, value)?;
            }
            Op::DeleteFast(idx) => {
                frame.store_local(idx as usize, Value::UNBOUND)?;
            }
            Op::UnpackSequence(count) => {
                let value = frame.pop()?;
                let elements = model.unpack_sequence(value, count as usize)?;
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
                model.set_add(set, value)?;
            }
            Op::DictInsert => {
                let value = frame.pop()?;
                let key = frame.pop()?;
                let dict = frame.pop()?;
                model.py_setitem(dict, key, value)?;
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
                frame.push(model.new_set(elements)?);
            }
            }
            Ok(Flow::Next)
        })();
        match flow {
            Ok(Flow::Next) => {}
            Ok(Flow::Return(value)) => return Ok(value),
            Err(trap) => {
                let exception = match model.take_pending_exception() {
                    Some(exception) => exception,
                    None => match model.trap_to_exception(trap) {
                        Some(exception) => exception,
                        None => return Err(trap),
                    },
                };
                if let Some(entry) = find_handler(&code.exc_table, faulting_ip) {
                    frame.stack.truncate(entry.depth as usize);
                    active_exception = Some(exception);
                    ip = entry.target as usize;
                } else {
                    model.set_pending_exception(exception);
                    return Err(trap);
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

    /// An empty object space, for code that never touches an object.
    fn no_objects() -> ObjectModel {
        ObjectModel::new(Vec::new(), 64)
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
            ret_ty: StaticType::Dynamic,
            n_locals,
            local_names: (0..n_locals).map(|i| format!("v{i}")).collect(),
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
    fn arithmetic_overflow_traps() {
        use Op::*;
        let code = code(
            0,
            0,
            vec![Const::Int(i64::from(FIXNUM_MAX)), Const::Int(2)],
            Vec::new(),
            0,
            vec![LoadConst(0), LoadConst(1), Binary(BinOp::Mul), Return],
        );
        let mut model = no_objects();
        assert_eq!(run(&code, &[], &[], &mut model), Err(Trap::Overflow));
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
            binary(BinOp::Add, Value::TRUE, Value::fixnum(1).unwrap()).unwrap().as_fixnum(),
            Some(2)
        );
        assert_eq!(compare(CmpOp::Eq, Value::fixnum(0).unwrap(), Value::FALSE), Ok(Value::TRUE));
        assert_eq!(compare(CmpOp::Eq, Value::fixnum(1).unwrap(), Value::TRUE), Ok(Value::TRUE));
        assert_eq!(compare(CmpOp::Eq, Value::NONE, Value::NONE), Ok(Value::TRUE));
        assert_eq!(compare(CmpOp::Eq, Value::NONE, Value::fixnum(1).unwrap()), Ok(Value::FALSE));
        assert_eq!(binary(BinOp::Add, Value::NONE, Value::fixnum(1).unwrap()), Err(Trap::TypeError));
        assert_eq!(compare(CmpOp::Lt, Value::NONE, Value::NONE), Err(Trap::TypeError));
    }

    #[test]
    fn floor_div_and_mod_match_python_signs() {
        let f = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(binary(BinOp::FloorDiv, f(7), f(2)).unwrap().as_fixnum(), Some(3));
        assert_eq!(binary(BinOp::FloorDiv, f(-7), f(2)).unwrap().as_fixnum(), Some(-4));
        assert_eq!(binary(BinOp::FloorDiv, f(7), f(-2)).unwrap().as_fixnum(), Some(-4));
        assert_eq!(binary(BinOp::FloorDiv, f(-7), f(-2)).unwrap().as_fixnum(), Some(3));
        assert_eq!(binary(BinOp::Mod, f(7), f(2)).unwrap().as_fixnum(), Some(1));
        assert_eq!(binary(BinOp::Mod, f(-7), f(2)).unwrap().as_fixnum(), Some(1));
        assert_eq!(binary(BinOp::Mod, f(7), f(-2)).unwrap().as_fixnum(), Some(-1));
        assert_eq!(binary(BinOp::Mod, f(-7), f(-2)).unwrap().as_fixnum(), Some(-1));
        assert_eq!(binary(BinOp::FloorDiv, f(5), f(0)), Err(Trap::ZeroDivisionError));
        assert_eq!(binary(BinOp::Mod, f(5), f(0)), Err(Trap::ZeroDivisionError));
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
        assert_eq!(binary(BinOp::BitAnd, f(12), f(10)).unwrap().as_fixnum(), Some(8));
        assert_eq!(binary(BinOp::BitOr, f(12), f(10)).unwrap().as_fixnum(), Some(14));
        assert_eq!(binary(BinOp::BitXor, f(12), f(10)).unwrap().as_fixnum(), Some(6));
        assert_eq!(binary(BinOp::LShift, f(1), f(10)).unwrap().as_fixnum(), Some(1024));
        assert_eq!(binary(BinOp::RShift, f(-8), f(1)).unwrap().as_fixnum(), Some(-4));
        assert_eq!(binary(BinOp::RShift, f(7), f(1)).unwrap().as_fixnum(), Some(3));
        assert_eq!(binary(BinOp::BitOr, Value::TRUE, f(2)).unwrap().as_fixnum(), Some(3));
        assert_eq!(binary(BinOp::LShift, f(1), f(-1)), Err(Trap::ValueError));
        assert_eq!(binary(BinOp::RShift, f(1), f(-1)), Err(Trap::ValueError));
        assert_eq!(binary(BinOp::LShift, f(1), f(40)), Err(Trap::Overflow));
    }

    #[test]
    fn unary_ops() {
        let f = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(unary(UnaryOp::Neg, f(5)).unwrap().as_fixnum(), Some(-5));
        assert_eq!(unary(UnaryOp::Pos, f(5)).unwrap().as_fixnum(), Some(5));
        assert_eq!(unary(UnaryOp::Invert, f(5)).unwrap().as_fixnum(), Some(-6));
        assert_eq!(unary(UnaryOp::Invert, f(0)).unwrap().as_fixnum(), Some(-1));
        assert_eq!(unary(UnaryOp::Neg, Value::TRUE).unwrap().as_fixnum(), Some(-1));
        assert_eq!(unary(UnaryOp::Neg, Value::NONE), Err(Trap::TypeError));
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

        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                use Op::*;
                let mut model = no_objects();
                let mut loop_fn = code(0, 0, Vec::new(), vec![String::from("loop_fn")], 0,
                    vec![LoadGlobal(0), Call(0), Return]);
                loop_fn.name = String::from("loop_fn");
                assert_eq!(
                    run(&loop_fn, core::slice::from_ref(&loop_fn), &[], &mut model),
                    Err(Trap::RecursionError)
                );
            })
            .unwrap()
            .join()
            .unwrap();
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

    #[test]
    fn the_shared_gc_scans_a_frame_by_tag() {
        let mut model = ObjectModel::new(vec![PyType::with_slots("Point", &["x"])], 4096);

        let _garbage = model.new_instance(0, &[Value::fixnum(111).unwrap()]).unwrap();
        let live = model.new_instance(0, &[Value::fixnum(7).unwrap()]).unwrap();
        let live_addr_before = live.as_ref().unwrap();

        let mut frame = Frame::new(2);
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
}
