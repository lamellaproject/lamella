#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python bytecode contract -- the single source of truth.


extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The four bytes that open a serialized module: "LPYC" (Lamella PYthon Code).
pub const MAGIC: [u8; 4] = *b"LPYC";

/// The binary format version. Bumped when the container or instruction encoding
/// changes incompatibly; readers reject a version they do not recognize.
///
/// Version 14 added closures: the three deref ops, `CodeObject`'s `cellvars`/`freevars`,
/// and the `CLOSURE` bit on [`Op::MakeFunction`]'s flags.
///
/// Version 15 added the class-body namespace: `SetupClassNamespace` / `StoreName` / `LoadName`,
/// so a class body can read a name it just bound (namespace -> global -> built-in).
///
/// Version 16 added the import system: `ImportName` (import a module and push it) and
/// `ImportFrom` (read a name off the module on top of the stack), so `import m` and
/// `from m import a` bind their names.
pub const FORMAT_VERSION: u16 = 16;

/// The feature-flag bits a module's header carries, declaring which language
/// surface its bytecode assumes. A reader lacking a required feature rejects the
/// artifact rather than mis-executing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeatureFlags(pub u16);

impl FeatureFlags {
    /// The first-light subset: a typed integer function plus one dynamic attribute
    /// access. The only flag defined so far.
    pub const FIRST_LIGHT: FeatureFlags = FeatureFlags(0x0001);

    /// Whether every bit in `other` is also set here.
    #[must_use]
    pub fn contains(self, other: FeatureFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// A binary arithmetic/bitwise operator carried by [`Op::Binary`] -- add/sub/mul, floor-division
/// and modulo, true division (`/`, float-producing), exponentiation (`**`), and the bitwise operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BinOp {
    /// `a + b`.
    Add = 0,
    /// `a - b`.
    Sub = 1,
    /// `a * b`.
    Mul = 2,
    /// `a // b` -- floor division.
    FloorDiv = 3,
    /// `a % b` -- modulo (the result takes the sign of the divisor, per Python).
    Mod = 4,
    /// `a & b` -- bitwise AND.
    BitAnd = 5,
    /// `a | b` -- bitwise OR.
    BitOr = 6,
    /// `a ^ b` -- bitwise XOR.
    BitXor = 7,
    /// `a << b` -- left shift.
    LShift = 8,
    /// `a >> b` -- right shift (arithmetic: Python ints are signed).
    RShift = 9,
    /// `a / b` -- true division; always produces a float (even for integer operands).
    TrueDiv = 10,
    /// `a ** b` -- exponentiation, right-associative. A non-negative integer exponent gives an
    /// integer (promoting past the fixnum range to a long); a negative exponent produces a float.
    Pow = 11,
    /// `a @ b` -- matrix multiplication (`__matmul__` / `__rmatmul__`). No builtin numeric type
    /// implements it (int/float `@` is a `TypeError`); it exists for user classes that define the
    /// dunder.
    MatMul = 12,
}

impl BinOp {
    /// The operator for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<BinOp> {
        match byte {
            0 => Some(BinOp::Add),
            1 => Some(BinOp::Sub),
            2 => Some(BinOp::Mul),
            3 => Some(BinOp::FloorDiv),
            4 => Some(BinOp::Mod),
            5 => Some(BinOp::BitAnd),
            6 => Some(BinOp::BitOr),
            7 => Some(BinOp::BitXor),
            8 => Some(BinOp::LShift),
            9 => Some(BinOp::RShift),
            10 => Some(BinOp::TrueDiv),
            11 => Some(BinOp::Pow),
            12 => Some(BinOp::MatMul),
            _ => None,
        }
    }
}

/// A comparison operator carried by [`Op::Compare`]. Each compares the two values
/// below it and pushes a Python boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CmpOp {
    /// `a == b`.
    Eq = 0,
    /// `a != b`.
    Ne = 1,
    /// `a < b`.
    Lt = 2,
    /// `a <= b`.
    Le = 3,
    /// `a > b`.
    Gt = 4,
    /// `a >= b`.
    Ge = 5,
    /// `a is b` -- object identity (not value equality).
    Is = 6,
    /// `a is not b` -- object non-identity.
    IsNot = 7,
}

impl CmpOp {
    /// The operator for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<CmpOp> {
        match byte {
            0 => Some(CmpOp::Eq),
            1 => Some(CmpOp::Ne),
            2 => Some(CmpOp::Lt),
            3 => Some(CmpOp::Le),
            4 => Some(CmpOp::Gt),
            5 => Some(CmpOp::Ge),
            6 => Some(CmpOp::Is),
            7 => Some(CmpOp::IsNot),
            _ => None,
        }
    }
}

/// A unary operator carried by [`Op::Unary`]. Pops the operand and pushes the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum UnaryOp {
    /// `-a` -- arithmetic negation (`__neg__`).
    Neg = 0,
    /// `+a` -- unary plus (`__pos__`; identity for ints).
    Pos = 1,
    /// `~a` -- bitwise inversion (`__invert__`; `-a - 1` for ints).
    Invert = 2,
}

impl UnaryOp {
    /// The operator for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<UnaryOp> {
        match byte {
            0 => Some(UnaryOp::Neg),
            1 => Some(UnaryOp::Pos),
            2 => Some(UnaryOp::Invert),
            _ => None,
        }
    }
}


/// One bytecode instruction -- the decoded, in-memory form the interpreter
/// dispatches and the lowering walks. The set is deliberately small and orthogonal
/// for first light; it grows behind the version stamp as the language surface
/// widens. Operand indices reference the owning [`CodeObject`]'s pools.
///
/// # Op-tag registry
///
/// The wire tag for each op -- the leading `u8` in `encode_op` and the matching `decode`
/// arm. The binary encoding is stable and versioned; a reader rejects an unknown tag. Tags
/// 22 and 23 are unused (a historical gap).
///
/// | tag(s) | ops | group |
/// |-------:|-----|-------|
/// |   0-13 | LoadConst, LoadFast, StoreFast, LoadGlobal, LoadAttr, Binary, Compare, PopTop, Jump, PopJumpIfFalse, Call, Return, Unary, Subscript | core |
/// |  14-21 | BuildSlice, BuildList, BuildTuple, BuildDict, GetIter, ForIter, Setitem, Contains | containers + iteration |
/// |  22-23 | (free) | |
/// |  24-29 | Raise, MatchExc, LoadExc, PopExcept, Reraise, DeleteFast | exceptions |
/// |  30-32 | MakeFunction, BuildClass, SetAttr | classes |
/// |     33 | UnpackSequence | tuple-unpacking |
/// |  34-36 | ListAppend, SetAdd, DictInsert | comprehensions |
/// |     37 | LoadSuper | super() |
/// |     38 | BuildSet | set literals |
/// |     39 | UnpackEx | starred unpacking |
/// |     40 | CallKw | keyword calls |
/// |     41 | Yield | generators |
/// |     42 | CallEx | star-call unpacking |
/// |  43-44 | DeleteItem, DeleteAttr | del subscript/attribute |
/// |  45-47 | LoadDeref, StoreDeref, LoadClosure | closures |
/// |  48-50 | SetupClassNamespace, StoreName, LoadName | class-body namespace |
/// |  51-52 | ImportName, ImportFrom | imports |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push `consts[idx]`.
    LoadConst(u32),
    /// Push the local variable in slot `idx`.
    LoadFast(u32),
    /// Pop and store into the local variable in slot `idx`.
    StoreFast(u32),
    /// Push the global or built-in named `names[idx]`. DEFINED for completeness;
    /// the first-light parity slice emits function bodies only (no globals), so the
    /// interpreter may leave this unimplemented until typed calls are enabled.
    LoadGlobal(u32),
    /// Pop an object and push `getattr(object, names[name])` -- the one dynamic
    /// operation in first light, lowering to the `py_getattr` intrinsic. `cache` is
    /// this site's inline-cache slot (RAM side array; see [`CodeObject::cache_count`]).
    LoadAttr {
        /// The attribute name's index into the code object's `names` pool.
        name: u32,
        /// This site's inline-cache slot, assigned by ascending static position.
        cache: u32,
    },
    /// Pop the index then the container, and push `container[index]` (subscript),
    /// lowering to the `py_getitem` intrinsic. `cache` is this site's inline-cache slot.
    Subscript {
        /// This site's inline-cache slot, assigned by ascending static position.
        cache: u32,
    },
    /// Pop the step, then upper, then lower bound (each a `None` on the stack when
    /// omitted) and push `slice(lower, upper, step)`; the slice then feeds a `Subscript`.
    /// Used for `s[i:j]` / `s[i:j:k]`.
    BuildSlice,
    /// Pop `count` values (pushed left to right) and push a new list of them. For a list
    /// display `[a, b, c]`.
    BuildList(u32),
    /// Pop `count` values (pushed left to right) and push a new tuple. For `(a, b, c)`.
    BuildTuple(u32),
    /// Pop `count` key-value pairs (each pushed key then value, pairs left to right) and
    /// push a new dict. For a dict display `{k: v, ...}`.
    BuildDict(u32),
    /// Pop an iterable and push an iterator over it (`iter(obj)`). For `for x in obj`.
    GetIter,
    /// Pop the iterator and advance it: on a value, push the value; on exhaustion, set the
    /// instruction pointer to `target` (absolute). The loop reloads the iterator each pass
    /// (it lives in a local), so the stack stays balanced. For `for x in obj`.
    ForIter(u32),
    /// Pop the index, then the container, then the value, and do `container[index] = value`
    /// (a side-effecting store; nothing is pushed). For `c[i] = v` on a mutable container.
    Setitem,
    /// Pop the container, then the element, and push whether the container contains the
    /// element. For the membership test `x in c` (`negate` flips it to `x not in c`).
    Contains {
        /// Whether this is `not in` (the boolean result is inverted).
        negate: bool,
    },
    /// Raise an exception: `argc` 1 pops the exception value (a class is instantiated with
    /// no arguments); `argc` 0 re-raises the active exception. For `raise`.
    Raise(u8),
    /// Pop a type and push whether the active exception is an instance of it -- the
    /// `except E` type test.
    MatchExc,
    /// Push the active exception, to bind it in `except ... as name`.
    LoadExc,
    /// Clear the active-exception state once a handler has dealt with it.
    PopExcept,
    /// Re-raise the active exception (a handler chain ended with no matching clause).
    Reraise,
    /// Make local slot `slot` unbound (a `del`); a later `LoadFast` of it raises `NameError`.
    /// Emitted for the `except ... as name` auto-deletion at the end of the handler.
    DeleteFast(u32),
    /// Push a function value for the Module function `names[func]`. `flags` (mirroring CPython's
    /// MAKE_FUNCTION bits) says what sits on the stack below, popped top-down: bit0 (`0x01`) a
    /// positional-defaults TUPLE, bit1 (`0x02`) a kwdefaults DICT, bit2 (`0x04`, `CLOSURE`) the
    /// captured cells for a closure. The stack layout, bottom to top, is
    /// `[defaults-tuple?] [kwdefaults-dict?] [cell0 .. cell{f-1}]`, so the cells (on top) are popped
    /// first, then the kwdefaults dict, then the defaults tuple. When `CLOSURE` is set, exactly
    /// `functions[func].freevars.len()` cells were pushed (by [`Op::LoadClosure`], in freevar order)
    /// and are stored in the new function object's captured-cells field -- a closure is just a
    /// function object WITH cells. `flags == 0` is a plain stateless function reference; `flags != 0`
    /// builds a function object carrying the def-time defaults and/or captured cells.
    MakeFunction {
        /// The index into `names` of the Module function to make.
        func: u32,
        /// What sits on the stack below (bit0 a defaults tuple, bit1 a kwdefaults dict, bit2/`0x04`
        /// `CLOSURE` the captured cells).
        flags: u8,
    },
    /// Pop the namespace dict, then the base, then the name, and push a new class object (a
    /// type). For a `class` definition.
    BuildClass,
    /// Pop the object, then the value, and do `object.<names[name]> = value` (`cache` is the
    /// inline-cache slot). For an attribute assignment `obj.attr = value`.
    SetAttr {
        /// The attribute name (index into the names pool).
        name: u32,
        /// The inline-cache slot.
        cache: u32,
    },
    /// Pop a sequence and push its `count` elements in REVERSE, so following `StoreFast`s bind
    /// the first element first. A length mismatch raises `ValueError`. For tuple-unpacking
    /// (`a, b = expr` and `for a, b in iter`).
    UnpackSequence(u32),
    /// Pop the value, then the list, and append the value to the list (in place). For a list
    /// comprehension.
    ListAppend,
    /// Pop the element, then the set, and add the element. For a set comprehension.
    SetAdd,
    /// Pop the value, then the key, then the dict, and insert `key -> value`. For a dict
    /// comprehension.
    DictInsert,
    /// Push a super object bound to the enclosing class `names[name]` and the frame's first
    /// local (`self`). For a no-arg `super()` in a method; a following `LoadAttr` finds the
    /// base class's attribute bound to `self`.
    LoadSuper(u32),
    /// Pop `count` elements and push a new set (deduped). For a set literal `{a, b, c}`; a
    /// set comprehension builds `BuildSet(0)` then `SetAdd`s.
    BuildSet(u32),
    /// Pop a sequence and unpack it for a starred target `a, *b, c = seq`: push (reversed, so
    /// the following `StoreFast`s take them left-to-right) the `before` head elements, then a
    /// LIST of the middle (`len - before - after` elements), then the `after` tail elements.
    /// Requires `len >= before + after`, else `ValueError`. For `a, *b = seq`.
    UnpackEx {
        /// The number of targets before the star.
        before: u32,
        /// The number of targets after the star.
        after: u32,
    },
    /// Pop the right operand then the left, and push `left <op> right`.
    Binary(BinOp),
    /// Pop the right operand then the left, and push the boolean `left <cmp> right`.
    Compare(CmpOp),
    /// Pop the operand and push `<op> operand`.
    Unary(UnaryOp),
    /// Pop and discard the top of the stack -- used after an expression statement
    /// whose value is unused.
    PopTop,
    /// Set the instruction pointer to op index `target` (absolute).
    Jump(u32),
    /// Pop a value; if it is not truthy, set the instruction pointer to op index
    /// `target` (absolute).
    PopJumpIfFalse(u32),
    /// Call a callable: the stack holds `[callable, arg0, .., arg{argc-1}]`; pop them
    /// and push the result. DEFINED for completeness; deferred for the first-light
    /// parity slice (the harness drives the call boundary), like [`Op::LoadGlobal`].
    Call(u32),
    /// Call a callable with keyword arguments: the stack holds `[callable, pos0 .. pos{argc-1},
    /// kwval0 .. kwval{k-1}]`, where `consts[kwnames]` (a [`Const::KwNames`]) gives the `k` keyword
    /// NAMES in the order their values were pushed. Pop them all plus the callable, bind by CPython's
    /// call rules, and push the result. [`Op::Call`] stays the positional-only fast path.
    CallKw {
        /// The number of positional arguments (those below the keyword-argument values).
        argc: u32,
        /// The index into `consts` of the [`Const::KwNames`] naming the keyword arguments in order.
        kwnames: u32,
    },
    /// Pop a value and return it from the current function.
    Return,
    /// Suspend the current generator, yielding the popped value to the caller; on resume, push the
    /// value the caller injected (`None` under `next`). A `yield` expression -- appears only in a
    /// generator function (one whose [`CodeObject::is_generator`] is set).
    Yield,
    /// A call with `*args` / `**kwargs` unpacking. The stack holds `[callee, arg0 .. arg{argc-1}]`;
    /// `consts[kinds]` (a [`Const::ArgKinds`]) tags each of the `argc` values (positional / `*` /
    /// keyword / `**`), and `consts[kwnames]` (a [`Const::KwNames`]) names the keyword-tagged slots
    /// in order. The runtime flattens the `*`/`**` operands and binds the result; pushes the result.
    CallEx {
        /// The number of argument values on the stack (each tagged by `kinds`).
        argc: u32,
        /// The index into `consts` of the [`Const::ArgKinds`] tagging each argument slot.
        kinds: u32,
        /// The index into `consts` of the [`Const::KwNames`] naming the keyword-tagged slots.
        kwnames: u32,
    },
    /// `del container[index]`. Pop the index, then the container, and delete the element (a step-1
    /// slice deletes a range). Pushes nothing.
    DeleteItem,
    /// `del object.attr` -- delete `object.<names[name]>`. Pop the object. Pushes nothing.
    DeleteAttr {
        /// The index into `names` of the attribute name.
        name: u32,
    },
    /// Push the value held by the cell at deref index `idx` (`cell[idx].get()`). The deref
    /// index space is `[0 .. cellvars.len())` -- this frame's OWN cells (locals captured by a
    /// nested function) -- then `[cellvars.len() .. cellvars.len() + freevars.len())` -- the cells
    /// CAPTURED from the enclosing function (free variables). One index space covers both. For
    /// reading a cell variable or a free variable.
    LoadDeref(u32),
    /// Pop a value and store it into the cell at deref index `idx` (`cell[idx].set(v)`). Same
    /// deref index space as [`Op::LoadDeref`]. For writing a cell variable (or, with `nonlocal`,
    /// a captured free variable), so the enclosing frame and the closure see one shared box.
    StoreDeref(u32),
    /// Push the CELL object itself at deref index `idx` (not its contents), to hand to a following
    /// [`Op::MakeFunction`] carrying the `CLOSURE` flag. Emitted once per free variable of the
    /// nested function, in that function's freevar order. For building a closure's captured cells.
    LoadClosure(u32),
    /// Install a fresh, empty class-body namespace dict as the frame's active namespace. Emitted at
    /// the start of a `class` body; [`Op::StoreName`] / [`Op::LoadName`] then target it, and
    /// [`Op::BuildClass`] consumes it. This is how a class body reads a name it just bound
    /// (`class C: a = 5; b = a + 1`, `@radius.setter`), which a plain dict-display cannot.
    SetupClassNamespace,
    /// Pop a value and bind it as `names[idx]` in the active class-body namespace (the class-body
    /// form of a name store). A member assignment / method definition in a `class` body.
    StoreName(u32),
    /// Push the value bound to `names[idx]`, resolving the active class-body namespace FIRST, then
    /// the module global / built-in namespace (a `NameError` if unbound). The class-body read that
    /// sees earlier class-body bindings and falls back outward, mirroring CPython's `LOAD_NAME`.
    LoadName(u32),
    /// Import the module named `names[idx]` and push the module object: resolve it (a cached
    /// `sys.modules` entry, else a native stdlib module, else `ModuleNotFoundError`), running its
    /// body once. Emitted for BOTH `import m` (the pushed module is then stored) and the lead-in of
    /// `from m import ...` (the module is the source for the following [`Op::ImportFrom`]s). A dotted
    /// name (`a.b`) is a single string here; package submodules are a later addition.
    ImportName(u32),
    /// Read `names[idx]` off the module on top of the stack (WITHOUT popping it) and push the value
    /// -- `getattr(module, name)`, an `ImportError` if the module has no such member. For each name in
    /// `from m import a, b`; the frontend emits a following store, then one [`Op::PopTop`] after the
    /// last to discard the module.
    ImportFrom(u32),
}

/// A compile-time constant in a code object's constant pool. Every value the running
/// program needs that is not a name -- integers and the singletons -- is referenced
/// by [`Op::LoadConst`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Const {
    /// The singleton `None`.
    None,
    /// `True` or `False`.
    Bool(bool),
    /// An integer literal. The compiler keeps it in an `i64`; the interpreter materializes it as a
    /// tagged 31-bit fixnum, widening to a heap `long` (and arbitrary-precision bignum) as needed.
    Int(i64),
    /// A string literal (its decoded value); the interpreter materializes it as a heap `str`.
    Str(String),
    /// A per-argument tag list for [`Op::CallEx`]: one byte per argument slot -- 0 positional,
    /// 1 `*` unpack, 2 keyword, 3 `**` unpack. Compile-time metadata, never a runtime value.
    ArgKinds(Vec<u8>),
    /// A compile-time tuple of keyword-argument names -- the kwnames a [`Op::CallKw`] references.
    /// Never a runtime Python value; only `CallKw` reads it out of the const pool.
    KwNames(Vec<String>),
    /// A floating-point literal -- the `f64` value's raw bits (`to_bits`), so `Const` stays `Eq`.
    /// The interpreter materializes it as a heap float; the typed AOT lane rejects it.
    Float(u64),
    /// An imaginary literal `Nj` -- the imaginary part's `f64` bits (real part is 0). The interpreter
    /// materializes it as a heap `complex` (when the `complex` feature is on); the typed lane rejects it.
    Imaginary(u64),
    /// An integer literal too large for `i64` -- its decimal digits (no sign, no separators). The
    /// interpreter materializes it as an arbitrary-precision `int` (bigint); the typed lane rejects it.
    BigInt(String),
    /// A bytes literal `b"..."` -- its raw bytes. The interpreter materializes a heap `bytes`; the
    /// typed lane rejects it.
    Bytes(Vec<u8>),
}

/// The first-light type lattice for an annotated value. Annotations (PEP 484), inert
/// at runtime in CPython, are honored here at compile time as the contract that
/// drives the typed fast path (the mypyc model). First light distinguishes only "a
/// machine integer" from "anything dynamic"; the lattice widens later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum StaticType {
    /// No usable static type: a boxed, dynamically-typed value. The default.
    #[default]
    Dynamic = 0,
    /// Annotated (or inferred) `int`: lowers to a machine integer on the typed path.
    /// First light maps it to MIR `i32` with bignum overflow deferred.
    Int = 1,
    /// Annotated (or inferred) `float`: lowers to a native `f64` on the typed path -- the shared
    /// MIR's F64, the same the C#/.NET `double` codegen targets. A dynamic (un-annotated) float is
    /// a heap object in the interpreter instead.
    Float = 2,
}

impl StaticType {
    /// The type for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<StaticType> {
        match byte {
            0 => Some(StaticType::Dynamic),
            1 => Some(StaticType::Int),
            2 => Some(StaticType::Float),
            _ => None,
        }
    }
}

/// A function parameter: its name and its annotated type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The parameter's name (it also occupies the matching leading local slot).
    pub name: String,
    /// The parameter's annotated type, or [`StaticType::Dynamic`] if unannotated.
    pub ty: StaticType,
}

/// A compiled code object: the bytecode and tables for one function (or the module's
/// top-level body). It is what the interpreter executes and what the typed lowering
/// consumes. The interpreter ignores the typing fields (`params`/`ret_ty`/
/// `local_types`); the lowering uses them to drive the typed fast path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeObject {
    /// The function's name, or `"<module>"` for a module's top-level body.
    pub name: String,
    /// The parameters, in order (`[positional-only | positional-or-keyword | keyword-only]`). They
    /// occupy the first `params.len()` local slots.
    pub params: Vec<Param>,
    /// How many leading `params` are positional-only (bindable only by position). 0 until the
    /// positional-only `/` marker is supported.
    pub posonly_count: u32,
    /// How many trailing `params` are keyword-only (bindable only by name, after a `*`). 0 until
    /// keyword-only parameters are supported.
    pub kwonly_count: u32,
    /// Whether this is a generator function (its body contains `yield`). A CALL of a generator
    /// function does not run the body -- it returns a generator object; the body runs on `next`.
    pub is_generator: bool,
    /// Whether the parameter list has a `*args` slot -- the param at index
    /// `params.len() - kwonly_count - (has_varkwargs as usize) - 1`, which collects surplus
    /// positional arguments into a tuple. When set, extra positionals are NOT a TypeError.
    pub has_varargs: bool,
    /// Whether the parameter list has a `**kwargs` slot (the last param), which collects
    /// unmatched keyword arguments into a dict. 0 until `**kwargs` is supported.
    pub has_varkwargs: bool,
    /// The return annotation, or [`StaticType::Dynamic`] if unannotated.
    pub ret_ty: StaticType,
    /// The total number of local-variable slots (parameters first, then the
    /// function's other assigned names). [`Op::LoadFast`] / [`Op::StoreFast`] index
    /// this range.
    pub n_locals: usize,
    /// The name of each local slot, indexed by slot number; `local_names.len() ==
    /// n_locals`. Kept for diagnostics and for the typed lowering.
    pub local_names: Vec<String>,
    /// The locals of this function that a nested function captures, so they must live in heap
    /// cells rather than plain slots. Their order defines the low half of the deref index space
    /// (`0 .. cellvars.len()`), which [`Op::LoadDeref`] / [`Op::StoreDeref`] / [`Op::LoadClosure`]
    /// index. Empty for a function that no nested function reads. A cellvar that is also a
    /// parameter has its bound argument copied into its fresh cell at frame setup.
    pub cellvars: Vec<String>,
    /// The names this function uses that are cellvars of an enclosing function -- reached through
    /// captured cells, not this frame's own locals. Their order continues the deref index space
    /// after `cellvars` (`cellvars.len() .. cellvars.len() + freevars.len()`). A closure built with
    /// [`Op::MakeFunction`]'s `CLOSURE` flag receives exactly `freevars.len()` cells in this order.
    pub freevars: Vec<String>,
    /// The annotated/inferred type of each local slot, indexed by slot number;
    /// `local_types.len() == n_locals`. Drives the typed fast path.
    pub local_types: Vec<StaticType>,
    /// The constant pool, indexed by [`Op::LoadConst`].
    pub consts: Vec<Const>,
    /// The attribute/global name pool, indexed by [`Op::LoadAttr`] and
    /// [`Op::LoadGlobal`].
    pub names: Vec<String>,
    /// The instructions, in order.
    pub ops: Vec<Op>,
    /// How many inline-cache slots a running frame allocates for this code: the count
    /// of cacheable sites (each [`Op::LoadAttr`]), numbered in ascending static order.
    pub cache_count: usize,
    /// The exception table: covering `[start, end)` op ranges mapped to a handler op
    /// index, innermost first. Empty for a function with no `try`. A raise searches it
    /// for the tightest entry covering the faulting op; the try body itself costs nothing.
    pub exc_table: Vec<ExcEntry>,
}

/// One entry in a [`CodeObject::exc_table`]: a protected op range and where to go when an
/// exception is raised within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExcEntry {
    /// First op index of the protected (try-body) range.
    pub start: u32,
    /// One past the last op index of the protected range (`[start, end)`).
    pub end: u32,
    /// The handler's op index -- where an in-range raise jumps.
    pub target: u32,
    /// The value-stack depth to truncate to before entering the handler.
    pub depth: u32,
}

/// A compiled module: its top-level function definitions plus the code object for its
/// top-level statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// The module's name (for diagnostics; e.g. the source stem).
    pub name: String,
    /// The functions defined at module scope, in source order.
    pub functions: Vec<CodeObject>,
    /// The `"<module>"` code object: the top-level statements, run on import.
    pub body: CodeObject,
}


/// Why decoding a serialized [`Module`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The data ran out before a field was complete.
    UnexpectedEof,
    /// The leading four bytes were not [`MAGIC`].
    BadMagic,
    /// The format version is not one this build understands.
    UnsupportedVersion(u16),
    /// A tagged union (an [`Op`], [`Const`], [`StaticType`], ...) held an unknown tag.
    BadTag(&'static str, u8),
    /// A string field was not valid UTF-8.
    BadUtf8,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::UnexpectedEof => f.write_str("unexpected end of bytecode"),
            DecodeError::BadMagic => f.write_str("not a Lamella Python bytecode module (bad magic)"),
            DecodeError::UnsupportedVersion(v) => {
                write!(f, "unsupported bytecode format version {v}")
            }
            DecodeError::BadTag(what, tag) => write!(f, "invalid {what} tag {tag}"),
            DecodeError::BadUtf8 => f.write_str("invalid UTF-8 in bytecode string"),
        }
    }
}

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_len(buf: &mut Vec<u8>, n: usize) {
    put_u32(buf, n as u32);
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_len(buf, s.len());
    buf.extend_from_slice(s.as_bytes());
}

fn put_const(buf: &mut Vec<u8>, c: &Const) {
    match c {
        Const::None => buf.push(0),
        Const::Bool(b) => {
            buf.push(1);
            buf.push(u8::from(*b));
        }
        Const::Int(v) => {
            buf.push(2);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Const::Str(s) => {
            buf.push(3);
            put_str(buf, s);
        }
        Const::KwNames(names) => {
            buf.push(4);
            put_len(buf, names.len());
            for n in names {
                put_str(buf, n);
            }
        }
        Const::Float(bits) => {
            buf.push(5);
            buf.extend_from_slice(&bits.to_le_bytes());
        }
        Const::Imaginary(bits) => {
            buf.push(6);
            buf.extend_from_slice(&bits.to_le_bytes());
        }
        Const::ArgKinds(kinds) => {
            buf.push(7);
            put_len(buf, kinds.len());
            buf.extend_from_slice(kinds);
        }
        Const::BigInt(digits) => {
            buf.push(8);
            put_str(buf, digits);
        }
        Const::Bytes(data) => {
            buf.push(9);
            put_len(buf, data.len());
            buf.extend_from_slice(data);
        }
    }
}

fn put_op(buf: &mut Vec<u8>, op: &Op) {
    match op {
        Op::LoadConst(i) => {
            buf.push(0);
            put_u32(buf, *i);
        }
        Op::LoadFast(i) => {
            buf.push(1);
            put_u32(buf, *i);
        }
        Op::StoreFast(i) => {
            buf.push(2);
            put_u32(buf, *i);
        }
        Op::LoadGlobal(i) => {
            buf.push(3);
            put_u32(buf, *i);
        }
        Op::LoadAttr { name, cache } => {
            buf.push(4);
            put_u32(buf, *name);
            put_u32(buf, *cache);
        }
        Op::Binary(b) => {
            buf.push(5);
            buf.push(*b as u8);
        }
        Op::Compare(c) => {
            buf.push(6);
            buf.push(*c as u8);
        }
        Op::Unary(u) => {
            buf.push(12);
            buf.push(*u as u8);
        }
        Op::PopTop => buf.push(7),
        Op::Jump(t) => {
            buf.push(8);
            put_u32(buf, *t);
        }
        Op::PopJumpIfFalse(t) => {
            buf.push(9);
            put_u32(buf, *t);
        }
        Op::Call(argc) => {
            buf.push(10);
            put_u32(buf, *argc);
        }
        Op::Return => buf.push(11),
        Op::Subscript { cache } => {
            buf.push(13);
            put_u32(buf, *cache);
        }
        Op::BuildSlice => buf.push(14),
        Op::BuildList(count) => {
            buf.push(15);
            put_u32(buf, *count);
        }
        Op::BuildTuple(count) => {
            buf.push(16);
            put_u32(buf, *count);
        }
        Op::BuildDict(count) => {
            buf.push(17);
            put_u32(buf, *count);
        }
        Op::GetIter => buf.push(18),
        Op::ForIter(target) => {
            buf.push(19);
            put_u32(buf, *target);
        }
        Op::Setitem => buf.push(20),
        Op::Contains { negate } => {
            buf.push(21);
            buf.push(*negate as u8);
        }
        Op::Raise(argc) => {
            buf.push(24);
            buf.push(*argc);
        }
        Op::MatchExc => buf.push(25),
        Op::LoadExc => buf.push(26),
        Op::PopExcept => buf.push(27),
        Op::Reraise => buf.push(28),
        Op::DeleteFast(slot) => {
            buf.push(29);
            put_u32(buf, *slot);
        }
        Op::MakeFunction { func, flags } => {
            buf.push(30);
            put_u32(buf, *func);
            buf.push(*flags);
        }
        Op::BuildClass => buf.push(31),
        Op::SetAttr { name, cache } => {
            buf.push(32);
            put_u32(buf, *name);
            put_u32(buf, *cache);
        }
        Op::UnpackSequence(count) => {
            buf.push(33);
            put_u32(buf, *count);
        }
        Op::ListAppend => buf.push(34),
        Op::SetAdd => buf.push(35),
        Op::DictInsert => buf.push(36),
        Op::LoadSuper(name) => {
            buf.push(37);
            put_u32(buf, *name);
        }
        Op::BuildSet(count) => {
            buf.push(38);
            put_u32(buf, *count);
        }
        Op::UnpackEx { before, after } => {
            buf.push(39);
            put_u32(buf, *before);
            put_u32(buf, *after);
        }
        Op::CallKw { argc, kwnames } => {
            buf.push(40);
            put_u32(buf, *argc);
            put_u32(buf, *kwnames);
        }
        Op::Yield => buf.push(41),
        Op::CallEx {
            argc,
            kinds,
            kwnames,
        } => {
            buf.push(42);
            put_u32(buf, *argc);
            put_u32(buf, *kinds);
            put_u32(buf, *kwnames);
        }
        Op::DeleteItem => buf.push(43),
        Op::DeleteAttr { name } => {
            buf.push(44);
            put_u32(buf, *name);
        }
        Op::LoadDeref(i) => {
            buf.push(45);
            put_u32(buf, *i);
        }
        Op::StoreDeref(i) => {
            buf.push(46);
            put_u32(buf, *i);
        }
        Op::LoadClosure(i) => {
            buf.push(47);
            put_u32(buf, *i);
        }
        Op::SetupClassNamespace => buf.push(48),
        Op::StoreName(name) => {
            buf.push(49);
            put_u32(buf, *name);
        }
        Op::LoadName(name) => {
            buf.push(50);
            put_u32(buf, *name);
        }
        Op::ImportName(name) => {
            buf.push(51);
            put_u32(buf, *name);
        }
        Op::ImportFrom(name) => {
            buf.push(52);
            put_u32(buf, *name);
        }
    }
}

fn put_code_object(buf: &mut Vec<u8>, co: &CodeObject) {
    put_str(buf, &co.name);
    put_len(buf, co.params.len());
    for p in &co.params {
        put_str(buf, &p.name);
        buf.push(p.ty as u8);
    }
    put_u32(buf, co.posonly_count);
    put_u32(buf, co.kwonly_count);
    buf.push(u8::from(co.is_generator));
    buf.push(u8::from(co.has_varargs));
    buf.push(u8::from(co.has_varkwargs));
    buf.push(co.ret_ty as u8);
    put_len(buf, co.n_locals);
    put_len(buf, co.local_names.len());
    for n in &co.local_names {
        put_str(buf, n);
    }
    put_len(buf, co.cellvars.len());
    for n in &co.cellvars {
        put_str(buf, n);
    }
    put_len(buf, co.freevars.len());
    for n in &co.freevars {
        put_str(buf, n);
    }
    put_len(buf, co.local_types.len());
    for t in &co.local_types {
        buf.push(*t as u8);
    }
    put_len(buf, co.consts.len());
    for c in &co.consts {
        put_const(buf, c);
    }
    put_len(buf, co.names.len());
    for n in &co.names {
        put_str(buf, n);
    }
    put_len(buf, co.cache_count);
    put_len(buf, co.ops.len());
    for op in &co.ops {
        put_op(buf, op);
    }
    put_len(buf, co.exc_table.len());
    for e in &co.exc_table {
        put_u32(buf, e.start);
        put_u32(buf, e.end);
        put_u32(buf, e.target);
        put_u32(buf, e.depth);
    }
}

/// Write a module's CONTENT (name + functions + body) with no container header -- shared by a bare
/// [`Module`] and each module inside a [`Bundle`].
fn put_module_content(buf: &mut Vec<u8>, module: &Module) {
    put_str(buf, &module.name);
    put_len(buf, module.functions.len());
    for f in &module.functions {
        put_code_object(buf, f);
    }
    put_code_object(buf, &module.body);
}

impl Module {
    /// Serialize this module to the versioned binary container.
    #[must_use]
    pub fn encode(&self, features: FeatureFlags) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        put_u16(&mut buf, FORMAT_VERSION);
        put_u16(&mut buf, features.0);
        put_module_content(&mut buf, self);
        buf
    }

    /// Decode a module from the versioned binary container, also returning the
    /// feature flags the artifact declared.
    pub fn decode(data: &[u8]) -> Result<(Module, FeatureFlags), DecodeError> {
        let mut r = Reader { data, pos: 0 };
        if r.bytes(4)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = r.u16()?;
        if version != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let features = FeatureFlags(r.u16()?);
        let module = r.module_content()?;
        Ok((module, features))
    }
}

/// The binary format version of a [`Bundle`] container -- a program's entry module plus its
/// importable managed (Python-authored) modules. Distinct from [`FORMAT_VERSION`] (a bare single
/// module) so a reader dispatches on the version: [`FORMAT_VERSION`] -> a `Module`, this -> a `Bundle`.
pub const BUNDLE_FORMAT_VERSION: u16 = 17;

/// A compiled multi-module program: the `entry` module (run at startup) plus the importable managed
/// modules an `import` resolves to (by `name`). The Python analog of a corlib bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    /// The program's main module -- run at startup.
    pub entry: Module,
    /// The importable managed modules, resolved by their `name`.
    pub modules: Vec<Module>,
}

impl Bundle {
    /// Serialize this bundle to the versioned binary container (magic + [`BUNDLE_FORMAT_VERSION`]).
    #[must_use]
    pub fn encode(&self, features: FeatureFlags) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        put_u16(&mut buf, BUNDLE_FORMAT_VERSION);
        put_u16(&mut buf, features.0);
        put_module_content(&mut buf, &self.entry);
        put_len(&mut buf, self.modules.len());
        for m in &self.modules {
            put_module_content(&mut buf, m);
        }
        buf
    }

    /// Decode a bundle from the versioned binary container, also returning the declared feature flags.
    pub fn decode(data: &[u8]) -> Result<(Bundle, FeatureFlags), DecodeError> {
        let mut r = Reader { data, pos: 0 };
        if r.bytes(4)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = r.u16()?;
        if version != BUNDLE_FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let features = FeatureFlags(r.u16()?);
        let entry = r.module_content()?;
        let n_modules = r.u32()? as usize;
        let mut modules = Vec::with_capacity(n_modules);
        for _ in 0..n_modules {
            modules.push(r.module_content()?);
        }
        Ok((Bundle { entry, modules }, features))
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::UnexpectedEof)?;
        let slice = self.data.get(self.pos..end).ok_or(DecodeError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        let b = self.bytes(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| DecodeError::BadUtf8)
    }

    /// Read a module's CONTENT (name + functions + body) -- the header-less body of a bare module or
    /// of one module inside a bundle.
    fn module_content(&mut self) -> Result<Module, DecodeError> {
        let name = self.string()?;
        let n_functions = self.u32()? as usize;
        let mut functions = Vec::with_capacity(n_functions);
        for _ in 0..n_functions {
            functions.push(self.code_object()?);
        }
        let body = self.code_object()?;
        Ok(Module {
            name,
            functions,
            body,
        })
    }

    fn py_type(&mut self) -> Result<StaticType, DecodeError> {
        let tag = self.u8()?;
        StaticType::from_u8(tag).ok_or(DecodeError::BadTag("StaticType", tag))
    }

    fn const_value(&mut self) -> Result<Const, DecodeError> {
        let tag = self.u8()?;
        let c = match tag {
            0 => Const::None,
            1 => Const::Bool(self.u8()? != 0),
            2 => Const::Int(self.i64()?),
            3 => Const::Str(self.string()?),
            4 => {
                let n = self.u32()? as usize;
                let mut names = Vec::with_capacity(n);
                for _ in 0..n {
                    names.push(self.string()?);
                }
                Const::KwNames(names)
            }
            5 => Const::Float(self.i64()? as u64),
            6 => Const::Imaginary(self.i64()? as u64),
            7 => {
                let n = self.u32()? as usize;
                Const::ArgKinds(self.bytes(n)?.to_vec())
            }
            8 => Const::BigInt(self.string()?),
            9 => {
                let n = self.u32()? as usize;
                Const::Bytes(self.bytes(n)?.to_vec())
            }
            _ => return Err(DecodeError::BadTag("Const", tag)),
        };
        Ok(c)
    }

    fn op(&mut self) -> Result<Op, DecodeError> {
        let tag = self.u8()?;
        let op = match tag {
            0 => Op::LoadConst(self.u32()?),
            1 => Op::LoadFast(self.u32()?),
            2 => Op::StoreFast(self.u32()?),
            3 => Op::LoadGlobal(self.u32()?),
            4 => Op::LoadAttr {
                name: self.u32()?,
                cache: self.u32()?,
            },
            5 => {
                let b = self.u8()?;
                Op::Binary(BinOp::from_u8(b).ok_or(DecodeError::BadTag("BinOp", b))?)
            }
            6 => {
                let c = self.u8()?;
                Op::Compare(CmpOp::from_u8(c).ok_or(DecodeError::BadTag("CmpOp", c))?)
            }
            7 => Op::PopTop,
            8 => Op::Jump(self.u32()?),
            9 => Op::PopJumpIfFalse(self.u32()?),
            10 => Op::Call(self.u32()?),
            11 => Op::Return,
            12 => {
                let u = self.u8()?;
                Op::Unary(UnaryOp::from_u8(u).ok_or(DecodeError::BadTag("UnaryOp", u))?)
            }
            13 => Op::Subscript {
                cache: self.u32()?,
            },
            14 => Op::BuildSlice,
            15 => Op::BuildList(self.u32()?),
            16 => Op::BuildTuple(self.u32()?),
            17 => Op::BuildDict(self.u32()?),
            18 => Op::GetIter,
            19 => Op::ForIter(self.u32()?),
            20 => Op::Setitem,
            21 => Op::Contains {
                negate: self.u8()? != 0,
            },
            24 => Op::Raise(self.u8()?),
            25 => Op::MatchExc,
            26 => Op::LoadExc,
            27 => Op::PopExcept,
            28 => Op::Reraise,
            29 => Op::DeleteFast(self.u32()?),
            30 => Op::MakeFunction {
                func: self.u32()?,
                flags: self.u8()?,
            },
            31 => Op::BuildClass,
            32 => Op::SetAttr {
                name: self.u32()?,
                cache: self.u32()?,
            },
            33 => Op::UnpackSequence(self.u32()?),
            34 => Op::ListAppend,
            35 => Op::SetAdd,
            36 => Op::DictInsert,
            37 => Op::LoadSuper(self.u32()?),
            38 => Op::BuildSet(self.u32()?),
            39 => Op::UnpackEx {
                before: self.u32()?,
                after: self.u32()?,
            },
            40 => Op::CallKw {
                argc: self.u32()?,
                kwnames: self.u32()?,
            },
            41 => Op::Yield,
            42 => Op::CallEx {
                argc: self.u32()?,
                kinds: self.u32()?,
                kwnames: self.u32()?,
            },
            43 => Op::DeleteItem,
            44 => Op::DeleteAttr { name: self.u32()? },
            45 => Op::LoadDeref(self.u32()?),
            46 => Op::StoreDeref(self.u32()?),
            47 => Op::LoadClosure(self.u32()?),
            48 => Op::SetupClassNamespace,
            49 => Op::StoreName(self.u32()?),
            50 => Op::LoadName(self.u32()?),
            51 => Op::ImportName(self.u32()?),
            52 => Op::ImportFrom(self.u32()?),
            _ => return Err(DecodeError::BadTag("Op", tag)),
        };
        Ok(op)
    }

    fn code_object(&mut self) -> Result<CodeObject, DecodeError> {
        let name = self.string()?;
        let n_params = self.u32()? as usize;
        let mut params = Vec::with_capacity(n_params);
        for _ in 0..n_params {
            let pname = self.string()?;
            let ty = self.py_type()?;
            params.push(Param { name: pname, ty });
        }
        let posonly_count = self.u32()?;
        let kwonly_count = self.u32()?;
        let is_generator = self.u8()? != 0;
        let has_varargs = self.u8()? != 0;
        let has_varkwargs = self.u8()? != 0;
        let ret_ty = self.py_type()?;
        let n_locals = self.u32()? as usize;
        let n_local_names = self.u32()? as usize;
        let mut local_names = Vec::with_capacity(n_local_names);
        for _ in 0..n_local_names {
            local_names.push(self.string()?);
        }
        let n_cellvars = self.u32()? as usize;
        let mut cellvars = Vec::with_capacity(n_cellvars);
        for _ in 0..n_cellvars {
            cellvars.push(self.string()?);
        }
        let n_freevars = self.u32()? as usize;
        let mut freevars = Vec::with_capacity(n_freevars);
        for _ in 0..n_freevars {
            freevars.push(self.string()?);
        }
        let n_local_types = self.u32()? as usize;
        let mut local_types = Vec::with_capacity(n_local_types);
        for _ in 0..n_local_types {
            local_types.push(self.py_type()?);
        }
        let n_consts = self.u32()? as usize;
        let mut consts = Vec::with_capacity(n_consts);
        for _ in 0..n_consts {
            consts.push(self.const_value()?);
        }
        let n_names = self.u32()? as usize;
        let mut names = Vec::with_capacity(n_names);
        for _ in 0..n_names {
            names.push(self.string()?);
        }
        let cache_count = self.u32()? as usize;
        let n_ops = self.u32()? as usize;
        let mut ops = Vec::with_capacity(n_ops);
        for _ in 0..n_ops {
            ops.push(self.op()?);
        }
        let n_exc = self.u32()? as usize;
        let mut exc_table = Vec::with_capacity(n_exc);
        for _ in 0..n_exc {
            exc_table.push(ExcEntry {
                start: self.u32()?,
                end: self.u32()?,
                target: self.u32()?,
                depth: self.u32()?,
            });
        }
        Ok(CodeObject {
            name,
            params,
            posonly_count,
            kwonly_count,
            is_generator,
            has_varargs,
            has_varkwargs,
            ret_ty,
            n_locals,
            local_names,
            cellvars,
            freevars,
            local_types,
            consts,
            names,
            ops,
            cache_count,
            exc_table,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample_module() -> Module {
        let func = CodeObject {
            name: String::from("inc"),
            params: vec![Param {
                name: String::from("n"),
                ty: StaticType::Int,
            }],
            posonly_count: 0,
            kwonly_count: 0,
            is_generator: false,
            has_varargs: false,
            has_varkwargs: false,
            ret_ty: StaticType::Int,
            n_locals: 1,
            local_names: vec![String::from("n")],
            cellvars: Vec::new(),
            freevars: Vec::new(),
            local_types: vec![StaticType::Int],
            consts: vec![Const::Int(1), Const::None, Const::KwNames(vec![String::from("x")])],
            names: vec![String::from("x")],
            ops: vec![
                Op::LoadFast(0),
                Op::LoadConst(0),
                Op::Binary(BinOp::Add),
                Op::Return,
                Op::LoadAttr { name: 0, cache: 0 },
                Op::PopTop,
            ],
            cache_count: 1,
            exc_table: vec![ExcEntry {
                start: 0,
                end: 5,
                target: 8,
                depth: 0,
            }],
        };
        Module {
            name: String::from("m"),
            functions: vec![func],
            body: CodeObject {
                name: String::from("<module>"),
                params: Vec::new(),
                posonly_count: 0,
                kwonly_count: 0,
                is_generator: false,
                has_varargs: false,
                has_varkwargs: false,
                ret_ty: StaticType::Dynamic,
                n_locals: 0,
                local_names: Vec::new(),
                cellvars: Vec::new(),
                freevars: Vec::new(),
                local_types: Vec::new(),
                consts: vec![Const::None],
                names: Vec::new(),
                ops: vec![Op::LoadConst(0), Op::Return],
                cache_count: 0,
                exc_table: Vec::new(),
            },
        }
    }

    #[test]
    fn module_container_round_trips() {
        let module = sample_module();
        let bytes = module.encode(FeatureFlags::FIRST_LIGHT);
        assert_eq!(&bytes[..4], &MAGIC);
        let (decoded, features) = Module::decode(&bytes).expect("decodes");
        assert_eq!(decoded, module);
        assert!(features.contains(FeatureFlags::FIRST_LIGHT));
    }

    #[test]
    fn bundle_container_round_trips() {
        let named = |name: &str| {
            let mut m = sample_module();
            m.name = String::from(name);
            m
        };
        let bundle = Bundle {
            entry: named("__main__"),
            modules: vec![named("helpers"), named("config")],
        };
        let bytes = bundle.encode(FeatureFlags::FIRST_LIGHT);
        assert_eq!(&bytes[..4], &MAGIC);
        let (decoded, features) = Bundle::decode(&bytes).expect("decodes");
        assert_eq!(decoded, bundle);
        assert_eq!(decoded.modules.len(), 2);
        assert_eq!(decoded.modules[0].name, "helpers");
        assert!(features.contains(FeatureFlags::FIRST_LIGHT));
        assert!(matches!(
            Module::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(v)) if v == BUNDLE_FORMAT_VERSION
        ));
        let solo = Bundle {
            entry: named("__main__"),
            modules: Vec::new(),
        };
        let (d2, _) = Bundle::decode(&solo.encode(FeatureFlags::FIRST_LIGHT)).expect("solo decodes");
        assert!(d2.modules.is_empty());
    }

    #[test]
    fn every_op_variant_round_trips() {
        let ops = vec![
            Op::LoadConst(7),
            Op::LoadFast(1),
            Op::StoreFast(2),
            Op::LoadGlobal(3),
            Op::LoadAttr { name: 4, cache: 5 },
            Op::Binary(BinOp::Mod),
            Op::Compare(CmpOp::Le),
            Op::PopTop,
            Op::Jump(9),
            Op::PopJumpIfFalse(10),
            Op::Call(2),
            Op::Unary(UnaryOp::Neg),
            Op::Subscript { cache: 6 },
            Op::BuildSlice,
            Op::BuildList(3),
            Op::BuildTuple(2),
            Op::BuildDict(1),
            Op::GetIter,
            Op::ForIter(7),
            Op::Setitem,
            Op::Contains { negate: true },
            Op::Raise(1),
            Op::MatchExc,
            Op::LoadExc,
            Op::PopExcept,
            Op::Reraise,
            Op::DeleteFast(2),
            Op::MakeFunction { func: 0, flags: 1 },
            Op::BuildClass,
            Op::SetAttr { name: 0, cache: 7 },
            Op::UnpackSequence(2),
            Op::ListAppend,
            Op::SetAdd,
            Op::DictInsert,
            Op::LoadSuper(3),
            Op::BuildSet(2),
            Op::UnpackEx { before: 1, after: 1 },
            Op::CallKw { argc: 2, kwnames: 1 },
            Op::Yield,
            Op::CallEx { argc: 3, kinds: 2, kwnames: 1 },
            Op::DeleteItem,
            Op::DeleteAttr { name: 4 },
            Op::LoadDeref(2),
            Op::StoreDeref(1),
            Op::LoadClosure(0),
            Op::SetupClassNamespace,
            Op::StoreName(1),
            Op::LoadName(2),
            Op::ImportName(3),
            Op::ImportFrom(4),
            Op::Return,
        ];
        let mut buf = Vec::new();
        for op in &ops {
            put_op(&mut buf, op);
        }
        let mut r = Reader {
            data: &buf,
            pos: 0,
        };
        for expected in &ops {
            assert_eq!(r.op().unwrap(), *expected);
        }
    }

    #[test]
    fn code_object_cellvars_freevars_round_trip() {
        let co = CodeObject {
            name: String::from("inner"),
            params: Vec::new(),
            posonly_count: 0,
            kwonly_count: 0,
            is_generator: false,
            has_varargs: false,
            has_varkwargs: false,
            ret_ty: StaticType::Dynamic,
            n_locals: 1,
            local_names: vec![String::from("n")],
            cellvars: vec![String::from("n")],
            freevars: vec![String::from("outer_x")],
            local_types: vec![StaticType::Dynamic],
            consts: vec![Const::Int(1)],
            names: Vec::new(),
            ops: vec![
                Op::LoadDeref(1),
                Op::LoadConst(0),
                Op::Binary(BinOp::Add),
                Op::StoreDeref(0),
                Op::LoadClosure(0),
                Op::MakeFunction { func: 0, flags: 0x04 },
                Op::Return,
            ],
            cache_count: 0,
            exc_table: Vec::new(),
        };
        let mut buf = Vec::new();
        put_code_object(&mut buf, &co);
        let mut r = Reader { data: &buf, pos: 0 };
        assert_eq!(r.code_object().unwrap(), co);
    }

    #[test]
    fn decode_rejects_bad_magic_and_version() {
        assert_eq!(Module::decode(b"XXXX...."), Err(DecodeError::BadMagic));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        put_u16(&mut bytes, FORMAT_VERSION + 1);
        put_u16(&mut bytes, 0);
        assert_eq!(
            Module::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(FORMAT_VERSION + 1))
        );
    }

    #[test]
    fn selector_bytes_round_trip() {
        for byte in 0u8..=12 {
            assert_eq!(BinOp::from_u8(byte).unwrap() as u8, byte);
        }
        for byte in 0u8..=7 {
            assert_eq!(CmpOp::from_u8(byte).unwrap() as u8, byte);
        }
        for byte in 0u8..=2 {
            assert_eq!(UnaryOp::from_u8(byte).unwrap() as u8, byte);
        }
        assert_eq!(BinOp::from_u8(13), None);
        assert_eq!(CmpOp::from_u8(8), None);
        assert_eq!(UnaryOp::from_u8(3), None);
    }
}
