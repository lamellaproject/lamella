//! A tree-walking evaluator, wired to the abstract operations and to nothing else.

use crate::{format, vec, String, ToString, Vec};
use alloc::collections::BTreeMap;
use alloc::rc::Rc;

use crate::abstract_ops::{self as ops, Comparison, NeedsObjectProtocol};
use crate::ast::*;
use crate::object::{Callable, Object, Property, PropertyKey, PropertyKind};
use crate::string_value::JsString;
use crate::source::Span;
use crate::value::{JsValue, ObjectId, SymbolId};

/// What an evaluation produced.
///
/// The abrupt variants carry an optional target label, because `break outer` must pass through every
/// enclosing loop until it finds the one that owns the label.
#[derive(Debug, Clone, PartialEq)]
pub enum Completion {
    Normal(JsValue),
    Break(Option<String>),
    Continue(Option<String>),
    Return(JsValue),
    Throw(JsValue),
}

impl Completion {
    #[must_use]
    pub fn is_abrupt(&self) -> bool {
        !matches!(self, Completion::Normal(_))
    }

    #[must_use]
    pub fn value(&self) -> JsValue {
        match self {
            Completion::Normal(value) | Completion::Return(value) | Completion::Throw(value) => {
                value.clone()
            }
            _ => JsValue::Undefined,
        }
    }
}

/// Propagates an abrupt completion, or unwraps a normal one.
macro_rules! normal {
    ($expression:expr) => {{
        let completion = $expression;
        match completion {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        }
    }};
}

/// Propagates an abrupt completion carried in a `Result`'s `Err`, or unwraps the answer.
///
/// The sibling of [`normal!`], for the operations that answer a VALUE and can also throw --
/// `[[HasProperty]]`, `[[GetOwnProperty]]`, `[[OwnPropertyKeys]]`, `ResolveBinding`. A `Completion`
/// carries a value or an abruptness and not both, so those return `Result` and this is what a
/// function returning `Completion` unwraps them with.
///
/// ONE DEFINITION, EXPORTED, rather than a copy per module. It was written twice within an hour
/// of itself -- once here and once in `builtins` -- which is the shape a rule with two spellings
/// starts as.
macro_rules! ok_or_abrupt {
    ($expression:expr) => {{
        match $expression {
            Ok(value) => value,
            Err(abrupt) => return abrupt,
        }
    }};
}

pub(crate) use ok_or_abrupt;

/// What a destructuring pattern does when it reaches a leaf.
///
/// These are the ONLY three things that differ between `var [a] = x`, `let [a] = x` and
/// `[a] = x`. Naming them as a mode is what lets the rest of the algorithm -- the iterator, the
/// elisions, the defaults, the close -- exist once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binds {
    /// `var [a] = x`: find the binding hoisting already created, wherever it went.
    Var,
    /// A `var` at the TOP LEVEL OF A SCRIPT, whose binding is also a property of the global object
    /// and has to move with it.
    ///
    /// A SEPARATE MODE RATHER THAN A TEST ON THE SCOPE, because `let` and `const` reach the same
    /// leaf through [`Binds::Var`] and must NOT touch the object. Mirroring on "is this the global
    /// scope?" made `let Array;` overwrite the global `Array` PROPERTY with `undefined` -- the
    /// lexical declaration is supposed to SHADOW it, leaving `this.Array` a function. This engine
    /// keeps both kinds in one map, so the declaration's own kind is the only thing that can tell
    /// them apart here.
    GlobalVar,
    /// `let [a] = x`, a `catch` parameter, a function parameter: create it here.
    Declare,
    /// `[a] = x`: assign to whatever the target already names -- and the only mode in which a leaf
    /// may be a member expression.
    Assign,
}

/// How a binding was declared, which decides what may be done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutability {
    Mutable,
    /// A `const`. Assigning to one is a TypeError at RUN time, not a syntax error -- the binding
    /// exists and the write is what fails.
    Immutable,
    /// A NAMED FUNCTION EXPRESSION'S OWN NAME, which is immutable and does **not** throw in sloppy
    /// code.
    ///
    /// THE STANDARD MAKES THIS ONE `CreateImmutableBinding(name, false)` AND EVERY OTHER
    /// IMMUTABLE BINDING `true`, and that boolean is the whole difference: a `const` refuses a
    /// write in both modes, while `var g = function f() { f = 1; };` silently does nothing in
    /// sloppy code and is a TypeError in strict. Sharing [`Mutability::Immutable`] makes the sloppy
    /// case throw, which rejects a program the language accepts.
    ImmutableSelfName,
}

#[derive(Debug, Clone)]
struct Binding {
    value: JsValue,
    mutability: Mutability,
    /// A `let`/`const` binding EXISTS from the top of its block and cannot be read until its
    /// declaration runs -- the temporal dead zone. Modelling it as "not yet declared" instead would
    /// make `typeof x` return `"undefined"` where the standard requires a ReferenceError, and would
    /// let an enclosing scope's `x` be found instead.
    initialized: bool,
    /// Whether this came from a `let`, `const` or `class` rather than a `var`, a function or a
    /// parameter.
    ///
    /// # THE GLOBAL ENVIRONMENT IS **TWO** RECORDS AND THIS ENGINE HAS ONE MAP
    ///
    /// The standard's global environment record holds a `[[DeclarativeRecord]]` for `let`/`const`/
    /// `class` and an `[[ObjectRecord]]` over the global object for `var` and functions, and three
    /// rules ask which one a name is in: `HasLexicalDeclaration` (a `var` over a `let` is a
    /// SyntaxError, and so is a second `let`), and `SetMutableBinding`, which must NOT write a
    /// lexical global through to the object.
    ///
    /// IT CANNOT BE DERIVED FROM "HAS AN OWN PROPERTY". That reading is right for every name the
    /// realm did not already have and wrong for `let Object = 1` -- legal, because the global
    /// `Object` property is configurable -- where the property exists and the binding is still
    /// lexical. Deriving it also let that `let` overwrite `globalThis.Object`.
    ///
    /// Away from the global scope it is inert: a block's `let` and a block's `var` live in
    /// different scopes already, so nothing consults this outside the two rules above.
    lexical: bool,
}

impl Binding {
    /// An ordinary initialized, mutable, NON-lexical binding: a `var`, a function, a parameter, or
    /// anything the realm installs. The common case by a wide margin.
    fn var(value: JsValue) -> Self {
        Self { value, mutability: Mutability::Mutable, initialized: true, lexical: false }
    }
}

/// Whether a closure is a class constructor, and which kind.
///
/// **A CLASS CONSTRUCTOR HAS NO `[[Call]]`, AND THAT IS THE PART AN ORDINARY FUNCTION FLAG
/// CANNOT EXPRESS.** `class C {}` then `C()` is a TypeError -- so is `C.call(null)`, and so is
/// calling the result of `C.bind(...)` without `new`. This engine ran the body instead, with `this`
/// bound to `undefined`, and the constructor's every `this.x = ...` then threw somewhere further in
/// about a property of a non-object: a plausible wrong answer standing in for a named refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    /// Not a class constructor at all: an ordinary function, an arrow, a method.
    NotAClass,
    /// `class C {}` -- its `this` exists from the first statement.
    Base,
    /// `class C extends X {}`, INCLUDING `extends null`.
    Derived,
}

/// How a derived constructor's return breaks the rules in 10.2.2 -- two different errors, and
/// naming them keeps the decision separate from the reporting.
#[derive(Debug, Clone, Copy)]
enum Derived {
    /// It returned `undefined` without ever calling `super()`: a ReferenceError.
    NoSuper,
    /// It returned a primitive other than `undefined`: a TypeError, even if `super()` did run.
    NotAnObject,
}

/// What a prototype-chain walk found, which is three answers rather than two.
///
/// # A PROXY IS NOT A PLACE THE PROPERTY MIGHT BE, IT IS THE END OF THE WALK
///
/// "Own property, else the parent, else missing" is two answers, and a proxy adds a third that
/// neither of the other two can stand in for: the remaining question belongs to a handler, and the
/// caller has to hand it over rather than continue. Reporting `Missing` instead
/// would say a proxy has no properties -- every read through one answering `undefined`; reporting a
/// `Property` is impossible, because there is nothing to report without running code, and this walk
/// is used by callers that must not.
enum Found {
    Property(Property),
    /// The proxy the walk reached, which may be where it started or somewhere up the chain.
    Proxy(ObjectId),
    Missing,
}

/// A lexical environment: a scope, and the one that encloses it.
///
/// # TWO KINDS OF ENVIRONMENT RECORD IN ONE STRUCT, AND `with_object` IS WHICH
///
/// The standard has a DECLARATIVE record (a map of names) and an OBJECT record (a binding object
/// whose properties are the bindings). This holds both: `bindings` is the declarative half, and
/// `with_object` -- when it is `Some` -- makes this an object record for a `with` statement.
///
/// **A `with` ENVIRONMENT HAS NO DECLARATIVE BINDINGS**, so the two halves never both answer for
/// one name: the walkers check `bindings` first and it is empty for exactly these environments. The
/// alternative -- a separate `Scope` variant -- would have forked every one of the five chain walks
/// into a match, which is the shape that gains a case in one spelling of a rule and not the rest.
///
/// **`[[IsWithEnvironment]]` IS NOT MODELLED SEPARATELY BECAUSE THE ONLY OTHER OBJECT RECORD IN
/// THIS ENGINE IS THE GLOBAL ONE, AND IT IS NOT A `Scope`.** The global record lives in
/// [`Interpreter::global_object_binding`], reached when the chain runs out. So `with_object.is_some()`
/// and `[[IsWithEnvironment]]` are the same fact here, which is why `@@unscopables` and
/// `WithBaseObject` can key off it directly. If a second kind of object record ever becomes a
/// `Scope`, this needs a flag beside it -- an object record that is NOT a `with` must not consult
/// `@@unscopables` and must not supply a receiver.
#[derive(Debug, Default)]
pub struct Environment {
    bindings: BTreeMap<String, Binding>,
    parent: Option<Rc<core::cell::RefCell<Environment>>>,
    /// `[[BindingObject]]` of a `with` statement's object environment record.
    with_object: Option<ObjectId>,
}

/// A handle on a lexical environment.
///
/// PUBLIC because an `arguments` object holds one: its `[[ParameterMap]]` aliases the parameter
/// bindings, so the object model needs to name the environment those bindings live in.
pub type Scope = Rc<core::cell::RefCell<Environment>>;

/// `[[Base]]` of the Reference Record `ResolveBinding` returns: WHICH record a bare name resolved
/// in, held so the write can go back to it.
///
/// # A NAME IS RESOLVED ONCE AND WRITTEN THROUGH THAT ANSWER, AND THIS ENGINE RESOLVED IT TWICE
///
/// `x *= 3` is `lref = ResolveBinding("x")`, `GetValue(lref)`, evaluate the right-hand side, then
/// `PutValue(lref, ...)` -- **the same reference**. Walking the chain again for the write is only
/// indistinguishable while nothing moves in between, and the whole point of the two-step shape is
/// that something can:
///
/// ```text
/// var x = 0;
/// var scope = { get x() { delete this.x; return 2; } };
/// with (scope) { x *= 3; }        // scope.x === 6, and the outer x is STILL 0
/// ```
///
/// The getter deletes the binding it was asked for. A second walk falls past the `with` record and
/// writes the OUTER `x`, so both bindings end up wrong: the outer one is clobbered and the object
/// never gets the value. The mirror case is a binding that APPEARS -- `with (scope) { x = (scope.x
/// = 2, 1); }` resolves `x` to the enclosing function record, and a second walk finds the property
/// the right-hand side just created and writes there instead.
///
/// **`node` FAILS EVERY ONE OF THESE**, so the oracle cannot settle it and the text does:
/// 6.2.5.6 `PutValue` step 6 passes `refRecord.[[Base]]`, not the current scope.
///
/// It holds an `Rc` to the environment rather than a depth, because a depth is only meaningful
/// against the chain it was counted on and the chain the write runs against may be a different one.
#[derive(Debug, Clone)]
pub(crate) enum ReferenceBase {
    /// A declarative record -- a function scope, a block, or the global record's declarative half.
    Declarative(Scope),
    /// An object environment record: a `with` statement's binding object.
    Object(ObjectId),
    /// The global environment record's OBJECT half, which is not a [`Scope`] in this engine.
    Global,
    /// `IsUnresolvableReference` is true: the name is on nothing in the chain.
    Unresolvable,
}

/// The Reference Record itself: its `[[Base]]` and its `[[Strict]]`.
///
/// # `[[Strict]]` BELONGS TO THE REFERENCE, NOT TO THE ENVIRONMENT IT NAMES
///
/// A `with` statement is sloppy-only, so it is tempting to read "this is a `with` record" as "the
/// write is sloppy" -- and that is wrong, because the code running INSIDE the object environment
/// record can be strict. A strict function declared in a `with` body closes over that record:
///
/// ```text
/// var scope = { x: 1 };
/// with (scope) { (function () { "use strict"; x = (delete scope.x, 2); })(); }   // ReferenceError
/// ```
///
/// `SetMutableBinding` is reached with `S` true, its binding has vanished, and step 3 throws. This
/// engine carried a comment saying that arm was unreachable; sixteen corpus rows reach it, and they
/// were passing only because the old re-walk fell past the record and complained about an
/// undeclared name instead -- the right exception for the wrong reason.
#[derive(Debug, Clone)]
pub(crate) struct Reference {
    base: ReferenceBase,
    /// `[[Strict]]`: the strictness of the code that CREATED the reference, captured here rather
    /// than read at write time so that user code running in between cannot change the answer.
    strict: bool,
}

impl Reference {
    /// `WithBaseObject()`: the binding object of a `with` record, and `undefined` for every other
    /// kind -- which is what a call through a bare name takes as its `this`.
    fn with_base_object(&self) -> Option<ObjectId> {
        match self.base {
            ReferenceBase::Object(object) => Some(object),
            _ => None,
        }
    }

    /// `IsUnresolvableReference`.
    fn is_unresolvable(&self) -> bool {
        matches!(self.base, ReferenceBase::Unresolvable)
    }
}

fn new_scope(parent: Option<Scope>) -> Scope {
    Rc::new(core::cell::RefCell::new(Environment {
        bindings: BTreeMap::new(),
        parent,
        with_object: None,
    }))
}

/// `NewObjectEnvironment(obj, true, outerEnv)`: the environment a `with` statement runs its body in.
///
/// It is a REAL LINK IN THE CHAIN and outlives the statement, because a closure made inside the
/// body captures it -- `with (o) { f = function () { return x; } }` then `o.x = 99` makes `f()`
/// answer 99. Popping it at the end of the statement would be an environment the program can still
/// reach and this engine no longer has.
fn new_with_scope(object: ObjectId, parent: Scope) -> Scope {
    Rc::new(core::cell::RefCell::new(Environment {
        bindings: BTreeMap::new(),
        parent: Some(parent),
        with_object: Some(object),
    }))
}

/// `CreatePerIterationEnvironment`: a fresh environment holding the loop head's `let` bindings,
/// carrying their current values forward. Returns the environment unchanged when there is nothing
/// to copy, which is every loop but a `let`-headed `for`.
fn copy_for_iteration(previous: &Scope, names: &[String]) -> Scope {
    if names.is_empty() {
        return Rc::clone(previous);
    }
    let parent = previous.borrow().parent.clone();
    let next = new_scope(parent);
    {
        let source = previous.borrow();
        let mut target = next.borrow_mut();
        for name in names {
            if let Some(binding) = source.bindings.get(name) {
                target.bindings.insert(name.clone(), binding.clone());
            }
        }
    }
    next
}

/// The artifact walker: **the whole evaluator.** A program is parsed, encoded and then executed
/// from the encoded form; this module is what executes it, and there is no second evaluator.
///
/// Declared HERE, after `macro_rules! normal!`, because macro_rules is TEXTUALLY scoped: a module
/// declared above the macro cannot use it.
pub mod artifact;

/// What a freshly built realm contains. See [`Interpreter::realm_census`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RealmCensus {
    /// Objects on the heap. Every intrinsic, every prototype, every builtin function is one.
    ///
    /// **ADDRESSABLE, NOT RESIDENT IN RAM.** Once the realm lives in flash the two differ, and
    /// they are the whole point of the exercise -- see [`RealmCensus::resident_objects`].
    pub objects: usize,
    /// Of [`RealmCensus::objects`], how many are being read from FLASH and cost no RAM at all.
    ///
    /// Zero until the build-time tables exist. Reported separately rather than folded into the byte
    /// figures so that the sum is checkable by a reader:
    /// `objects == resident_objects - promoted_objects + <RAM objects>`.
    pub resident_objects: usize,
    /// How many resident objects something has WRITTEN to, so a RAM copy exists.
    ///
    /// **A FIGURE FAR ABOVE THE HANDFUL THIS SHOULD BE MEANS THE BARRIER IS PROMOTING THINGS
    /// NOTHING WROTE** -- or that whatever priced the expectation was counting something else.
    /// Both are worth knowing, which is why this is reported rather than assumed.
    pub promoted_objects: usize,
    /// Own properties across every object -- the realm's dominant repeated structure.
    pub properties: usize,
    /// Of those, accessor pairs rather than data properties.
    pub accessors: usize,
    /// Objects that are callable, i.e. builtin functions.
    pub callable_objects: usize,
    /// User closures. Zero in a fresh realm, which is the point of reporting it.
    pub closures: usize,
    /// Native function entries behind the callable objects.
    pub natives: usize,
    /// Symbols, including the well-known ones.
    pub symbols: usize,
    /// `Function.prototype.bind` results, each holding its bound `this` and arguments.
    ///
    /// THE NEXT THREE ARE HERE BECAUSE THEY ARE THE TABLES A HEAP-TRACING COLLECTOR WOULD NOT
    /// SEE. A heap object reaches them by INDEX (`Callable::Closure`/`Bound`) or by span, so
    /// reclaiming the object does not reclaim the entry -- which is the shape that made Python's
    /// container elements immortal, with different nouns. Counting them is the first step to being
    /// able to say so with a number instead of an opinion.
    pub bounds: usize,
    /// Tagged-template strings arrays, cached per CALL SITE and keyed by span.
    pub template_objects: usize,
    /// Loaded program artifacts. Never released -- see `Interpreter::programs`.
    pub programs: usize,
    /// Bytes of property-key TEXT, before any per-key structure. The floor on what naming the
    /// surface costs, and the number a smaller BCL surface moves directly.
    pub property_key_bytes: usize,


    /// `size_of::<Object>()` on the build being measured.
    ///
    /// It is REPORTED rather than assumed because it differs between a host and a device -- a
    /// pointer-width difference across nineteen optional slots -- and a host numerator over a
    /// device denominator is two populations wearing one number.
    pub object_size: usize,
    /// The object table's live bytes: one `Object` per entry **that is in RAM**.
    ///
    /// A RESIDENT OBJECT CONTRIBUTES NOTHING HERE, which is the saving the flash tables exist to
    /// produce. It was `objects * object_size` while every object was in RAM and those were the same
    /// figure; reading this as "one per addressable id" after the split would credit the design with
    /// nothing it had done.
    pub object_struct_bytes: usize,
    /// What the RAM-side object tables have RESERVED, which is what the allocator was asked for.
    ///
    /// Includes the promotion table -- four bytes per resident object -- because that is a real
    /// allocation a resident realm costs and hiding it here would flatter the design.
    ///
    /// THE GAP BETWEEN THIS AND [`RealmCensus::object_struct_bytes`] IS NOT THE WASTE. A Vec that
    /// doubles abandons every intermediate buffer, so the bytes it has consumed over its life are
    /// the reserved figure again, near enough -- and under an allocator with no class large enough
    /// to reclaim them, the abandoned ones are gone for the run.
    pub object_table_bytes: usize,
    /// The backing stores of every object's own-property vectors, summed by CAPACITY.
    ///
    /// Capacity rather than length for the same reason: the reserve is what was allocated.
    pub property_store_bytes: usize,
    /// The native registry's entries. **Its names are NOT in here and that is deliberate**: they
    /// are `&'static str`, so the text is in constant data and costs no arena at all. See
    /// [`RealmCensus::native_name_bytes`], which measures it as an inventory.
    pub native_table_bytes: usize,
    /// How much text the registry's names amount to. **AN INVENTORY, NOT A COST.**
    ///
    /// Kept beside [`RealmCensus::native_table_bytes`] rather than folded into it, because the two
    /// answer different questions. Folded into one number, the figure simply vanishes when the names
    /// move to constant data -- reporting the saving and hiding what was saved, so the next person
    /// asking how big the built-in names are has nothing to read.
    pub native_name_bytes: usize,
}

/// How many objects and native entries the realm is expected to build, so the tables that hold
/// them are allocated once instead of doubling into place.
///
/// # A DOUBLING VECTOR DOES NOT COST THE SLACK, IT COSTS THE SUM
///
/// A `Vec` reaching 462 entries has also held buffers of 256, 128, 64 and so on, and it abandoned
/// each one. Under a general-purpose allocator most of those come back. Under the size-class
/// allocator the device legs use, a buffer larger than the top class is carved from the frontier
/// and has no class to return to -- so the two largest abandoned buffers are gone for the run.
///
/// So a table's final doubling asks for one contiguous block larger than everything it has already
/// abandoned, and the abandoned ones may have no class to return to. **A realm whose LIVE set fits
/// comfortably can still fail on the way it was reached** -- which is what these hints exist to
/// skip, rather than to bound the realm.
///
/// **THESE ARE HINTS AND NOT LIMITS.** A realm that grows past them still works and merely doubles
/// from here, so the numbers being stale is a performance question rather than a correctness one.
/// They are deliberately a little above the measured figures for that reason.
const REALM_OBJECT_HINT: usize = 512;
const REALM_NATIVE_HINT: usize = 448;

/// A loaded program's bytes: heap-owned, or borrowed from flash and never copied.
///
/// # THIS IS AN ENUM RATHER THAN THE CFG'D TYPE ALIAS THAT WAS PLANNED, AND THE REASON IS CARGO
///
/// The design on file was `Rc<[u8]>` on a host and `&'static [u8]` on a device, selected by cfg --
/// the shape `lamella-cil-runtime` used for `RawCil`. **Cargo features are per PACKAGE, not per
/// binary**, and `js-pico2` builds seven images from one package: `xip-engine` wants the borrowed
/// form, while `engine` and `interp-only` call `run` and must own bytes an encoder just produced.
/// One flag cannot answer for both, and splitting the package to get one would be a worse cure.
///
/// The enum is also strictly more capable than the alias would have been. A device can hold a
/// BAKED program borrowed from flash *and* one delivered over the air into RAM at the same time;
/// `&'static [u8]` can only ever express the first, and an OTA payload is not `'static`.
///
/// The discriminant is paid ONCE PER LOADED PROGRAM, not per closure and not per node, so the
/// branch it adds is on `Artifact::open` rather than anywhere that runs in a loop.
#[derive(Debug, Clone)]
pub enum ProgramBytes {
    /// The encoder's output on a host, or a payload delivered into RAM on a device.
    Owned(Rc<[u8]>),
    /// **Borrowed from flash and never copied** -- the XIP case, and the whole point of the tier.
    Static(&'static [u8]),
}

impl core::ops::Deref for ProgramBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Static(bytes) => bytes,
        }
    }
}

impl From<Rc<[u8]>> for ProgramBytes {
    fn from(bytes: Rc<[u8]>) -> Self {
        Self::Owned(bytes)
    }
}

impl From<&'static [u8]> for ProgramBytes {
    fn from(bytes: &'static [u8]) -> Self {
        Self::Static(bytes)
    }
}

/// Where a closure's body lives: **which** program, and where in it.
///
/// # THE PROGRAM INDEX IS NOT DECORATION -- ONE INTERPRETER RUNS MANY PROGRAMS
///
/// A closure recorded only an offset for as long as the interpreter held only one program, and that
/// held for exactly as long as the only caller of `run_artifact` was the differential -- which
/// builds a fresh `Interpreter` per program and runs it once. **Test262 does not work that way**:
/// every plan runs its harness prelude and then the test through ONE interpreter, so `assert.throws`
/// is a closure from the first program being called after the second has been loaded.
///
/// And the failure is silent by construction. A stale offset is still a VALID offset; it simply
/// addresses a different node, so the function runs somebody else's code and returns a plausible
/// answer. Nothing throws unless the bytes happen to disagree about a tag.
///
/// It is also FREE. `Option<u32>` occupies eight bytes once padded, and so does this -- the
/// per-closure RAM story is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeRef {
    /// Which of the interpreter's loaded programs the offset belongs to.
    pub program: u32,
    /// The byte offset of the closure's own function node within that program.
    pub at: u32,
}

/// A user-defined function.
#[derive(Debug, Clone)]
pub struct Closure {
    /// WHERE THE WHOLE FUNCTION LIVES: which loaded program, and the offset of this function's
    /// own node inside it. Parameters and body alike are read from there at call time.
    ///
    /// `None` IS A THIRD STATE, not a missing one: **"no body at all"**. A class with no declared
    /// `constructor` has no function node to point at, and the TREE has none either -- which is
    /// exactly why the round trip stays lossless. `call_body` handles it, and it is why this cannot
    /// become a plain `CodeRef`.
    pub code_at: Option<CodeRef>,
    /// The EFFECTIVE strictness of this function: its own directive, or the code it was written in.
    ///
    /// It is captured when the CLOSURE IS MADE, not when it is called. A strict function stays
    /// strict however sloppy its caller is, and a sloppy one called from strict code stays sloppy --
    /// reading the caller's strictness at call time gets both backwards.
    pub strict: bool,
    /// Whether this function's own code ever names `arguments`.
    ///
    /// **AN EAGER `arguments` WAS THE ENGINE'S LARGEST SINGLE RETENTION.** `js-bench --bin
    /// region-ram` measured 501.4 B retained per FUNCTION call against 0.0 B per ARROW call, and an
    /// arrow is exactly a call that does not build one -- so the whole difference was this array,
    /// allocated on every call whether the body named it or not. ~160 calls exhausted the M33's
    /// whole fixed realm again.
    ///
    /// Decided by the COMPILER, not by a check here: it is a static property of the body, and
    /// the encoder is the walk that can see it once instead of on every call.
    pub needs_arguments: bool,
    pub scope: Scope,
    pub is_arrow: bool,
    /// The `this` an ARROW captured where it was written. `None` for everything else, which gets
    /// its own binding at call time instead.
    ///
    /// # "AN ARROW DOES NOT PUSH A FRAME" IS NOT THE SAME RULE AS "AN ARROW IS LEXICAL"
    ///
    /// Not pushing makes an arrow read whatever frame is on the stack UNDERNEATH IT, which is the
    /// defining frame only while that frame is still running. The moment an arrow OUTLIVES the
    /// function that made it -- which is the reason arrows exist, and what every callback does --
    /// it reads the frame of whoever happens to be calling it. `var f = o.m()` returning `() => this`
    /// answered with the CALLER'S `this`, not `o`. **Dynamic scope wearing lexical scope's syntax**,
    /// and it agreed with the standard in exactly the inline case anybody writes a quick test for.
    ///
    /// It hid behind an empty stack answering `undefined`: the escaped-arrow tests that existed
    /// expected `undefined` in strict code and got it, from the wrong mechanism. Giving the global
    /// scope its proper `this` removed the coincidence and the real bug surfaced at once.
    pub captured_this: Option<JsValue>,
    /// The derived constructor's `this`-bound cell, when an arrow was written inside one.
    ///
    /// It is the SAME cell, not a copy: `super()` called through the arrow has to initialize the
    /// constructor's `this`, and a copy would leave the constructor reporting that it never ran.
    pub captured_this_cell: Option<Rc<core::cell::RefCell<Option<JsValue>>>>,
    /// Written with a METHOD DEFINITION -- `{ m() {} }`, `class C { m() {} }`, a getter, a setter.
    ///
    /// THE ONLY THING THIS DECIDES IS THAT `new` REFUSES, AND IT IS NOT THE SAME AS `is_arrow`.
    /// Both are non-constructors, but an arrow is also lexically scoped for `this`, `super` and
    /// `arguments`, and a method is emphatically not -- reusing `is_arrow` to get the `new` refusal
    /// would have changed what `this` means inside every method in the language. Two facts, two
    /// flags. A CLASS CONSTRUCTOR IS A METHOD DEFINITION AND IS STILL CONSTRUCTIBLE, so it does
    /// not set this.
    pub is_method: bool,
    /// The object a method was defined ON, which is what `super.m` looks up from.
    ///
    /// IT IS THE DEFINITION SITE, NOT THE RECEIVER. `super.m()` in a method of `D.prototype`
    /// means `C.prototype.m` whatever object it was called on -- resolving it from `this` instead
    /// finds the same method again and recurses forever.
    pub home_object: Option<ObjectId>,
    /// Set for `class D extends C {}` with no constructor written: forward every argument to `C`.
    pub implicit_derived: bool,
    /// The CLASS CONSTRUCTOR OBJECT this closure is the body of, for a class constructor.
    ///
    /// # IT IS THE CLASS AND NOT THE PARENT, BECAUSE `super()` IS A LIVE LOOKUP
    ///
    /// `GetSuperConstructor()` is *"Let activeFunction be envRec.[[FunctionObject]]. Return !
    /// activeFunction.[[GetPrototypeOf]]()"* -- read at the moment `super()` runs. A class's
    /// prototype is an ordinary, configurable, WRITABLE-by-`setPrototypeOf` link, so a constructor
    /// that runs `Object.setPrototypeOf(E, other)` and THEN calls `super()` must call `other`.
    /// Holding the parent from definition time calls `baseFunc`, which is a different program --
    /// and `Object.setPrototypeOf(F, null); super()` must be a TypeError, where a captured parent
    /// quietly succeeds.
    ///
    /// Holding the class also subsumes the `extends null` case rather than needing a second field:
    /// `class C extends null {}` gets `%Function.prototype%` as its own prototype, which is not a
    /// constructor, so the live lookup answers the TypeError that a two-state encoding has to
    /// answer by storing `None`.
    pub class_object: Option<ObjectId>,
    /// Whether this is a class constructor, and if so the standard's `[[ConstructorKind]]`.
    ///
    /// **"DERIVED" IS NOT THE SAME QUESTION AS "IS THERE A PARENT CONSTRUCTOR", AND
    /// `extends null` IS THE CASE THAT SEPARATES THEM.** `class C extends null {}` is derived --
    /// its `this` is uninitialized until `super()` runs, and its return value obeys the derived
    /// rules -- while having no parent to call, so `super()` in it is a TypeError. Deriving the
    /// answer from a stored parent says "base" for that class and gives it an ordinary
    /// `this` it should not have.
    ///
    /// ONE FIELD FOR TWO FACTS BECAUSE THE SECOND ONLY EXISTS INSIDE THE FIRST: nothing that is
    /// not a class has a constructor kind, and the third state is what refuses `[[Call]]`.
    pub class_kind: ClassKind,
    /// The `new.target` an ARROW captured where it was written, for the reason
    /// [`Self::captured_this`] gives: `new.target` is lexical, so an arrow that outlives its
    /// constructor must still report the constructor rather than whoever is calling it now.
    ///
    /// `None` means "not an arrow". An arrow written outside any construct captures
    /// `Some(undefined)`, which is a value and not an absence.
    pub captured_new_target: Option<JsValue>,
    /// Written `function*`: calling it BUILDS a generator object instead of running the body.
    ///
    /// A THIRD FLAG BESIDE [`Self::is_arrow`] AND [`Self::is_method`], because it agrees with them
    /// on one answer and differs on the other. All three are non-constructors -- `new` on any of
    /// them is a TypeError -- but an arrow and a method have no `prototype` PROPERTY at all, and a
    /// generator function has one: an ordinary object, writable, whose own prototype is
    /// `%GeneratorPrototype%` and which carries NO `constructor` back-reference. Deriving "has a
    /// prototype" from "is a constructor" is right for the other two and wrong for this one, which
    /// is why [`Self::prototype_kind`] exists rather than a second reading of `constructs`.
    pub is_generator: bool,
}

/// What `prototype` property a freshly built function gets, which is THREE answers and not two.
///
/// Splitting this out of the constructibility question is the whole point: `new f()` and
/// `'prototype' in f` are separate questions that agree for an ordinary function, agree for an
/// arrow and a method, and DISAGREE for a generator function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrototypeKind {
    /// No `prototype` property at all: an arrow, a method definition, an accessor.
    None,
    /// An ordinary object inheriting `%Object.prototype%`, carrying a `constructor` back-reference.
    Constructor,
    /// An ordinary object inheriting `%GeneratorPrototype%`, with NO `constructor` back-reference.
    ///
    /// The missing back-reference is not an oversight in the standard and the corpus tests it
    /// directly: 27.3.4 gives a generator function's `prototype` no such property, so
    /// `Object.getPrototypeOf(g()).constructor` finds `%GeneratorFunction.prototype%` up the chain
    /// rather than `g`.
    Generator,
}

impl Closure {
    /// `IsConstructor` for a closure, in ONE place.
    ///
    /// **ONE DEFINITION, BECAUSE A RULE WRITTEN THREE TIMES GAINS ITS NEXT CASE IN ONE OF THEM.**
    /// `is_constructor`, `[[Construct]]`'s refusal ladder and the decision to give a function a
    /// `prototype` each spelled `!is_arrow && !is_method` separately; adding generators to the
    /// first two and not the third is a `new g()` that refuses while `g.prototype.constructor`
    /// still points back at `g`. The repair is to name the rule once and call it, rather than to
    /// write a careful fourth copy.
    #[must_use]
    pub(crate) fn constructs(&self) -> bool {
        !self.is_arrow && !self.is_method && !self.is_generator
    }

    /// Which `prototype` property this function gets. See [`PrototypeKind`].
    #[must_use]
    pub(crate) fn prototype_kind(&self) -> PrototypeKind {
        if self.is_generator {
            PrototypeKind::Generator
        } else if self.is_arrow || self.is_method {
            PrototypeKind::None
        } else {
            PrototypeKind::Constructor
        }
    }
}

/// Which HALF of a call the body walk is being asked for.
///
/// # THE TWO HALVES ARE SEPARATE BECAUSE THE STANDARD RUNS THEM AT DIFFERENT TIMES
///
/// An ordinary call binds its parameters and runs its body as one indivisible step, and nothing can
/// observe the join. `EvaluateGeneratorBody` splits them: `FunctionDeclarationInstantiation` is step
/// 1 and the body does not run until a `next()` that may never come. So the walk takes a phase
/// rather than there being a second walk for generators -- the parameter rules, the `arguments`
/// object and the hoisting are one implementation used three ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyPhase {
    /// Bind the parameters and run the body: every ordinary call.
    Whole,
    /// Bind the parameters and stop: calling a generator function.
    ParametersOnly,
    /// Run the body with the parameters ALREADY bound: resuming a generator.
    ///
    /// `simple` is carried in rather than recomputed because it was decided by the binding pass
    /// that is no longer running, and it decides whether the body gets its own variable
    /// environment. Two answers to it would put a body's `var`s in a different scope from the one
    /// its parameters were bound in.
    BodyOnly { simple: bool },
}

/// `[[GeneratorState]]` and `[[GeneratorContext]]`, which move together and are read together.
///
/// The context is `None` exactly when the generator is `completed`: finishing DROPS it, because it
/// holds the arguments and the scope the parameters were bound into, and a heap with no collector
/// would otherwise retain them for the life of the realm.
#[derive(Debug, Clone)]
pub struct GeneratorData {
    pub state: crate::object::GeneratorState,
    pub context: Option<GeneratorContext>,
}

/// `[[GeneratorContext]]`: everything a resumption needs that the generator object does not hold.
///
/// # THE SCOPE IS CAPTURED, NOT REBUILT, AND THAT IS WHAT MAKES THE PARAMETER TIMING RIGHT
///
/// `EvaluateGeneratorBody` runs `FunctionDeclarationInstantiation` FIRST and creates the generator
/// SECOND, so the parameters are bound -- and any default expression among them has run -- before
/// the caller ever sees a generator object. Rebuilding the environment at the first `next()` would
/// agree with the standard for every simple parameter list and disagree for
/// `function* g(a = effect())`, where the effect happens at the call, and for
/// `function* g([a]) {}` called with nothing, where the TypeError does.
#[derive(Debug, Clone)]
pub struct GeneratorContext {
    /// Which closure this is a running instance of. The body's offset, its strictness, its home
    /// object and its class all come from there, so they are not copied here -- one record stays
    /// the source of truth for what the function IS, and this one holds what this RUN of it has.
    pub closure: u32,
    /// The environment the parameters were bound into.
    pub scope: Scope,
    /// The environment the BODY runs in, **created once and reused by every resumption**.
    ///
    /// # THE STANDARD SUSPENDS AN EXECUTION CONTEXT, NOT A PROGRAM COUNTER
    ///
    /// A generator's environment is part of what is suspended, so a `var` written before a `yield`
    /// is still there afterwards. Deriving the body environment per resumption instead gives the
    /// right answer only when it happens to BE the parameter environment -- which
    /// `body_environment` returns for a SIMPLE parameter list and not otherwise. Measured against a
    /// conforming engine before this field existed: `function* g(a = 1) { var n = 0; yield 1;
    /// n = n + 1; yield n; }` answered `undefined` where it must answer 1, while the same body with
    /// a simple parameter list was correct. **A defect that is invisible until a parameter has a
    /// default, a destructure or a rest.**
    pub body_scope: Scope,
    /// Whether the parameter list was simple, which decides whether the body gets a variable
    /// environment of its own. Decided by the binding pass and CARRIED rather than recomputed, so
    /// that the two halves of one call cannot answer it differently.
    pub simple: bool,
    /// The `this` the call bound, already substituted. It is stored rather than re-derived because
    /// `OrdinaryCallBindThis` happens once, when the generator function is called.
    pub this: JsValue,
    /// The frame the desugared body dispatches on: an ordinary object holding `state`.
    ///
    /// **IT IS A HEAP OBJECT AND NOT A FIELD HERE, BECAUSE THE PROGRAM READS AND WRITES IT.** The
    /// body the transform produced is ordinary JavaScript -- `switch (FRAME.state)` and
    /// `FRAME.state = 1` -- so the frame has to be a value the walker can reach through an ordinary
    /// binding. Keeping the number in Rust instead would need a tag for the dispatch, which is
    /// exactly the format move the desugaring exists to avoid.
    pub frame: ObjectId,
}

/// A built-in function: what it is called, how many arguments it declares, and what it does.
///
/// `name` and `length` are not bookkeeping. Test262 checks both on essentially every built-in it
/// touches (`verifyCallableProperty`), and `assert.throws` reports
/// `expectedErrorConstructor.name`, so a built-in without a name fails tests that have nothing to do
/// with naming.
pub struct NativeFunction {
    /// **BORROWED, NOT OWNED, AND THAT IS 413 ALLOCATIONS THE REALM NO LONGER MAKES.** Every
    /// built-in's name is known when the engine is compiled, so the text belongs in constant data
    /// beside the function it names rather than in a `String` copied into the arena once per realm.
    ///
    /// The type is what enforces it. A `&'static str` cannot be handed a value composed at run
    /// time, so an accessor's `"get length"` is written as a literal at the site that installs it,
    /// beside the property name it belongs to, where a reader can see the two agree.
    pub name: &'static str,
    pub length: u32,
    pub call: NativeFn,
    /// `[[Construct]]`, which is a SEPARATE internal method from `[[Call]]` and absent on most
    /// built-ins.
    ///
    /// **THEY ARE NOT THE SAME FUNCTION AND THE DIFFERENCE IS OBSERVABLE.** `Number("1")` is the
    /// primitive `1`; `new Number("1")` is an object that is truthy and whose `typeof` is
    /// `"object"`. Modelling one built-in as one function makes half of those wrong whichever half
    /// is chosen. `None` means the built-in is not a constructor, which is why `new Math.pow()` is
    /// a TypeError rather than a quiet success.
    pub construct: Option<NativeFn>,
}

/// The signature every built-in has: the interpreter, the `this` it was called on, its arguments.
pub type NativeFn = fn(&mut Interpreter, JsValue, &[JsValue]) -> Completion;

/// An anchor plus a monotonic source. See `Interpreter::set_host_clock` for why it is two numbers.
struct HostClock {
    /// `None` when only the monotonic half was installed: the clock counts from the epoch.
    anchor_epoch_millis: Option<f64>,
    anchor_monotonic_millis: f64,
    monotonic: fn() -> f64,
}

/// The state the installers run against when the realm came from the build-time tables.
///
/// # THE INSTALLERS STILL RUN, AND THEY STILL HAVE A JOB
///
/// Three of the four things installation produces cannot be generated, so the installers are not
/// replaced by the tables -- they are relieved of one duty:
///
/// - the **native registry**, whose order is what every `Callable::Native(u32)` in the tables MEANS,
///   and which cannot be a table at all because a built-in's body is an item no outside path names;
/// - the **`Intrinsics`** ids, which the engine holds by identity so that assigning `Array =
///   undefined` cannot break array indexing;
/// - the **global scope bindings** and the **symbol table**, neither of which is an object.
///
/// What they stop doing is creating the objects and writing the properties, because those already
/// exist. `next_id` hands back the ids the creation would have produced, in the order it would have
/// produced them, and `discard` absorbs the writes -- see `Interpreter::object_mut`.
///
/// **`next_id` is checked rather than trusted.** `Interpreter::finish_replay` refuses a realm whose
/// installers handed out a different number of ids than the tables hold, because from the first
/// divergence onward every id names the wrong object and nothing else would say so.
struct Replay {
    /// The id the next `allocate` returns. Starts at zero and ends at the table's length.
    next_id: u32,
    /// Where a replaying installer's property writes go. Emptied on every hand-out.
    discard: Object,
}

/// A function produced by `Function.prototype.bind`.
///
/// The bound arguments PRECEDE the call's own, and the bound `this` REPLACES the call's -- which
/// is the whole mechanism behind `Function.prototype.call.bind(f)`, an idiom the standard's own
/// harness uses to capture a method before a test can destroy it.
pub struct BoundFunction {
    pub target: ObjectId,
    pub this: JsValue,
    pub arguments: Vec<JsValue>,
}

/// The intrinsic objects a realm is built from, held by identity rather than looked up by name.
///
/// **A BUILT-IN MUST NOT BE REACHED THROUGH THE GLOBAL IT IS PUBLISHED AS.** `Array.prototype` is
/// how an array's methods are found, and a test that assigns `Array = undefined` -- Test262 has
/// many -- must not thereby break array indexing. Holding the id is what keeps the two independent.
#[derive(Debug, Default, Clone, Copy)]
pub struct Intrinsics {
    pub object_prototype: ObjectId,
    pub map_prototype: ObjectId,
    pub date_prototype: ObjectId,
    pub weak_map_prototype: ObjectId,
    pub weak_set_prototype: ObjectId,
    pub promise_prototype: ObjectId,
    pub promise_constructor: ObjectId,
    pub array_buffer_prototype: ObjectId,
    pub data_view_prototype: ObjectId,
    pub set_prototype: ObjectId,
    pub map_iterator_prototype: ObjectId,
    pub set_iterator_prototype: ObjectId,
    pub function_prototype: ObjectId,
    pub array_prototype: ObjectId,
    pub string_prototype: ObjectId,
    pub number_prototype: ObjectId,
    pub boolean_prototype: ObjectId,
    pub error_prototype: ObjectId,
    pub symbol_prototype: ObjectId,
    pub regexp_prototype: ObjectId,
    pub regexp_constructor: ObjectId,
    pub regexp_string_iterator_prototype: ObjectId,
    /// `%IteratorPrototype%`. Not reachable by name from a program -- there is no `Iterator`
    /// global in this profile -- but every iterator in the realm inherits from it, and its
    /// `[Symbol.iterator]` is what makes an iterator itself iterable.
    pub iterator_prototype: ObjectId,
    pub array_iterator_prototype: ObjectId,
    pub string_iterator_prototype: ObjectId,
    /// `%GeneratorFunction%`: the constructor every generator function is an instance of.
    ///
    /// NOT PUBLISHED AS A GLOBAL, and the standard says so -- there is no `GeneratorFunction` name
    /// in the language. The only route to it is `Object.getPrototypeOf(function*(){}).constructor`,
    /// which is exactly why it is held by identity: nothing can look it up by name to find it.
    pub generator_function: ObjectId,
    /// `%GeneratorFunction.prototype%`, which the standard also calls `%Generator%`.
    ///
    /// **IT IS NOT A FUNCTION.** It is an ordinary object sitting between every generator function
    /// and `%Function.prototype%`, and giving it a `[[Call]]` would make `Object.getPrototypeOf(
    /// function*(){})()` a call rather than the TypeError it is.
    pub generator_function_prototype: ObjectId,
    /// `%GeneratorPrototype%`, which the standard also spells
    /// `%GeneratorFunction.prototype.prototype%`: where `next`, `return` and `throw` live.
    ///
    /// A GENERATOR OBJECT DOES NOT INHERIT FROM IT DIRECTLY. It inherits from its own function's
    /// `prototype` property, which in turn inherits from this -- so `g.prototype` can be replaced
    /// and the instances of that one function stop being iterable while every other generator in
    /// the realm is untouched. Two links, and the corpus checks both.
    pub generator_prototype: ObjectId,
    /// Held because `Array.from` and `Array.of` are installed with the protocol rather than with the
    /// rest of `Array`, and by then the constructor is only reachable through the global object.
    pub array_constructor: ObjectId,
    /// `%ArrayBuffer%`, which is `ArrayBuffer.prototype.slice`'s default species.
    pub array_buffer_constructor: ObjectId,
    /// `%Object.prototype.toString%`, which `Array.prototype.toString` falls back to.
    ///
    /// THE INTRINSIC AND NOT THE PROPERTY: the standard's step names `%Object.prototype.toString%`
    /// directly, so a program that replaces `Object.prototype.toString` does not change what that
    /// step calls.
    pub object_prototype_to_string: ObjectId,
    /// `%Array.prototype.values%`, which is also every `arguments` object's `@@iterator`.
    ///
    /// HELD AS AN INTRINSIC rather than read from `Array.prototype` when an `arguments` object is
    /// built: a program that deletes `Array.prototype.values` must not change what the next call's
    /// `arguments` is iterable by, and the corpus checks the two are the SAME function object.
    pub array_prototype_values: ObjectId,
    /// `%TypedArray%.prototype`, which the nine concrete prototypes inherit from.
    pub typed_array_prototype: ObjectId,
    /// `%TypedArray%`, the abstract constructor the nine inherit from.
    pub typed_array_constructor: ObjectId,
    /// The nine concrete constructors and prototypes, in [`crate::object::Element::ALL`] order.
    ///
    /// A TABLE RATHER THAN NINE FIELDS, because every use of it is a lookup BY ELEMENT: which
    /// prototype a new view gets, and which element a constructor makes. Nine named fields would
    /// turn both into a nine-arm match written twice.
    pub typed_arrays: [(ObjectId, ObjectId); 9],
    /// `%ThrowTypeError%`: the function every poisoned accessor in the realm is.
    ///
    /// **ONE OBJECT PER REALM, AND THE IDENTITY IS THE OBSERVABLE PART.** The standard says the
    /// `caller` and `arguments` accessors on `Function.prototype` and the `callee` accessor on every
    /// unmapped arguments object are all the SAME function object, and Test262 checks it directly by
    /// comparing two of them with `===`. Building a fresh one per site would answer every behavioral
    /// question correctly and fail that comparison -- which is why this is an intrinsic rather than a
    /// helper that makes one on demand.
    pub throw_type_error: ObjectId,
    /// One per native error type, in [`ERROR_KINDS`] order.
    pub native_error_prototypes: [ObjectId; ERROR_KINDS.len()],
    pub native_error_constructors: [ObjectId; ERROR_KINDS.len()],
    /// One per entry of [`WELL_KNOWN_SYMBOLS`], in that order.
    ///
    /// These are VALUES the realm creates once, not names looked up later. `Symbol.iterator` and
    /// the symbol a program reaches through it must be the SAME symbol, or a user object's
    /// `[Symbol.iterator]` method is invisible to `for-of` -- and nothing would report it, because
    /// an object with an unreachable iterator method looks exactly like one without it.
    pub well_known_symbols: [SymbolId; WELL_KNOWN_SYMBOLS.len()],
    /// Set once the array above holds the realm's symbols rather than its default.
    ///
    /// A separate flag rather than a sentinel value, because `SymbolId` has no spare
    /// representation: `SymbolId(0)` is the FIRST symbol the realm allocates, so "unset" and
    /// "`Symbol.iterator`" are the same bits. That collision is exactly what made reading a slot
    /// too early write a property under a valid, wrong key instead of failing.
    pub well_known_symbols_ready: bool,
}

/// The well-known symbols this profile carries, in the order [`Intrinsics::well_known_symbols`]
/// indexes them.
///
/// **A WELL-KNOWN SYMBOL PRESENT BUT NEVER CONSULTED IS WORSE THAN A MISSING ONE.**
/// `Symbol.toPrimitive` defined and ignored makes a program's conversion hook SILENTLY DO NOTHING,
/// where an absent property is honest and visible. That is why the list is two entries and not the
/// standard's fifteen.
///
/// Where each is consulted, so this claim can be checked rather than trusted:
/// - **`toStringTag`** -- `Object.prototype.toString` reads it off the prototype chain.
/// - **`iterator`** -- read by [`crate::iterator::get_iterator`], which is the single entry point
///   `for-of`, spread, array destructuring and `Array.from` all go through. It is read as a
///   PROPERTY of the value being iterated, so a user object's own `[Symbol.iterator]` and an
///   inherited one both work, and `delete Array.prototype[Symbol.iterator]` genuinely makes arrays
///   un-iterable -- which is the test that this is a lookup rather than a special case for arrays.
///
///   A symbol that is DEFINED AND NEVER CONSULTED is the failure to watch for here: it is safe
///   only while every construct that would read it refuses by name, so that no program can reach a
///   user's hook and find it ignored -- and it stops being safe the moment one of those constructs
///   starts working, silently. `well_known_symbol_iterator_is_actually_consulted` is the test that
///   keeps this paragraph honest without a reader having to trust it.
/// - **`species`** -- read by [`Interpreter::species_constructor`], which is what
///   `Array.prototype.map`/`filter`/`slice`/`splice`/`concat`, `ArrayBuffer.prototype.slice` and
///   `Promise.prototype.then` all ask before they build their result. The getter is defined only
///   on the constructors those operations consult, for the reason the paragraph above gives.
/// - **`unscopables`** -- read by [`Interpreter::with_has_binding`], which is `HasBinding` on the
///   object environment record a `with` statement creates, and the ONLY place the standard consults
///   it. It arrived WITH `with` rather than before it: defined earlier it would have been a
///   property a program could set and watch be ignored, which is the failure the paragraph above
///   describes. It has no earlier use because there is no earlier caller.
/// - **`hasInstance`** -- read by [`Interpreter::instanceof_operator`], the only place the standard
///   consults it, and it arrived WITH that lookup for the reason `unscopables` did.
///   **IT IS THE PARAGRAPH ABOVE IN ITS OTHER DIRECTION.** The symbol being ABSENT did not make
///   the engine honest here, because the operator was going to answer anyway: `Symbol.hasInstance`
///   read `undefined`, so `C[Symbol.hasInstance] = f` set a property named `"undefined"`, and
///   `x instanceof C` ran the ordinary walk and answered a plausible `false`. A hook a program can
///   install and watch be ignored is the failure whether or not the KEY exists -- so the fix is the
///   lookup, and the symbol comes with it.
/// - **`isConcatSpreadable`** -- read by `Array.prototype.concat`'s `IsConcatSpreadable`, the only
///   place the standard consults it, and it has to arrive WITH that lookup for the same reason.
///   Falling back to `IsArray` and saying so in a note is honest and still a wrong answer for
///   `{ length: 2, 0: 'a', 1: 'b', [Symbol.isConcatSpreadable]: true }` -- an array-like the
///   standard spreads and a fallback appends whole.
pub const WELL_KNOWN_SYMBOLS: [&str; 12] = [
    "iterator",
    "toStringTag",
    "toPrimitive",
    "species",
    "unscopables",
    "hasInstance",
    "isConcatSpreadable",
    "match",
    "matchAll",
    "replace",
    "search",
    "split",
];

/// The error types the standard calls "NativeError", plus `Error` itself at index 0.
///
/// The order is load-bearing -- [`Intrinsics`] indexes by it -- so it is one list rather than a
/// name repeated at each use site.
pub const ERROR_KINDS: [&str; 10] = [
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "EvalError",
    "URIError",
    "AggregateError",
    "InternalError",
    "InternalDefect",
];

/// The last entry of [`ERROR_KINDS`] that the standard defines, and therefore that gets a global.
pub const STANDARD_ERROR_KINDS: usize = 8;

/// Every feature this profile does not implement AT RUN TIME, as a closed set.
///
/// # THE LIST HAS TO BE A TYPE, NOT A HABIT
///
/// "A feature is either absent **and listed**, or it is correct" is this tier's founding rule, and
/// it is only true if the list is an ARTIFACT somebody can read. It was free-text strings spread
/// across several files -- [`crate::absence`] records how many, and owns that count --
/// discoverable only by running a twenty-minute conformance pass and reading the
/// report, which is a habit, not a list. As an enum the set is exhaustive by construction: a new
/// refusal cannot be written without adding a variant, and the generator that publishes the list
/// enumerates the variants rather than grepping for a phrase.
///
/// **AND IT SEPARATES TWO THINGS A SHARED MARKER CONFLATES.** Strings like "a method must be a
/// function object" and "a tagged template needs a template literal" are not absences at all: they
/// are INTERNAL INVARIANTS that mean the engine is broken if they fire. Reported under the same
/// marker as a real gap, a defect in the engine is published as a gap the engine had chosen, so
/// they go through [`Interpreter::internal_defect`], which is never an absence. See
/// [`crate::absence::Absence`] for the set itself.
pub use crate::absence::Absence;

/// The interpreter.
pub struct Interpreter {
    global: Scope,
    /// Every precompiled program this interpreter has loaded, indexed by [`CodeRef::program`].
    ///
    /// A TABLE RATHER THAN ONE SLOT, BECAUSE A CLOSURE OUTLIVES THE PROGRAM THAT DEFINED IT.
    /// A single slot was correct for as long as `run_artifact` had one caller that used one
    /// program per interpreter; a second `run` on the same interpreter silently re-pointed every
    /// existing function value at the new bytes. See [`CodeRef`].
    ///
    /// `Rc<[u8]>` RATHER THAN A LIFETIME PARAMETER, AND THAT IS A LESSON REUSED FROM THE C# IMPLEMENTATION.
    /// An `Interpreter<'a>` would ripple a lifetime through every signature that takes one --
    /// `lamella-cil-runtime` kept `Module` non-generic on a fixed `'static` for exactly that
    /// reason, to avoid threading `'a` through ~200 `fn(&mut Vm, &Module, ..)` intrinsics.
    ///
    /// It is also what makes the borrow work: cloning the `Rc` is a refcount bump, so a caller
    /// can hold the bytes while `&mut self` walks them. Opening the artifact from `self` directly
    /// would borrow the interpreter immutably for the whole walk.
    ///
    /// A DEVICE BUILD WANTS `&'static [u8]` HERE INSTEAD -- the flash slice, zero copy, where an
    /// `Rc` would copy the program into RAM and give back the entire win. Both are `Clone` and
    /// `Deref<Target = [u8]>`, so that is a cfg'd type alias when the device path needs it, in the
    /// shape `lamella-cil-runtime` used for `RawCil`. A device loads ONE program, so the table
    /// has one entry there and the index is always zero -- it costs a device nothing.
    ///
    /// PUBLISHED LIMIT: entries are never released. A closure can reference any of them and
    /// nothing tracks which, so a long-lived REPL retains one artifact per submission (~1.53 flash
    /// bytes per source byte). Bounded and small for a runner that builds a fresh realm per test;
    /// it is the REPL that would want reference counting.
    programs: Vec<ProgramBytes>,
    /// The program the walker is currently inside, so a closure built mid-walk records the right
    /// one. Saved and restored around a call, exactly like `strict`.
    current_program: u32,
    /// Walk coverage accumulated INSIDE called function bodies.
    ///
    /// THESE EXIST BECAUSE THE COVERAGE FIGURE WAS SILENTLY NARROWER THAN IT LOOKED. A call
    /// reaches a body through `call_body`, which has no `Coverage` to thread -- so the artifact
    /// body evaluator was making a fresh one and DISCARDING it, and every node inside every called
    /// function was invisible to the instrument, walked and fallen-back alike. The tell was
    /// `StmtReturn` never appearing in the fallback list despite having no arm.
    ///
    /// A progress metric that cannot see inside a function is measuring top-level code, which is
    /// the least representative part of any real program. Folded into the caller's `Coverage` by
    /// `run_artifact` before it returns.
    pub(crate) nested_walked: u32,
    pub(crate) nested_refused: u32,
    /// Which tags fell back inside a body, so the work list stays evidence-led there too.
    pub(crate) nested_by_tag: [u32; 256],
    closures: Vec<Closure>,
    natives: Vec<NativeFunction>,
    bounds: Vec<BoundFunction>,
    /// The microtask queue: work deferred until the running program has nothing left to do.
    ///
    /// **IT IS A COLLECTOR ROOT.** A job holds `ObjectId`s, and a promise with a job in flight
    /// and no other reference is live PRECISELY BECAUSE the job is going to settle it. A tracing
    /// collector that cannot see this queue would free the promise and then run a job against a
    /// dead id -- the same index-reachability trap that made the Python tier's containers immortal,
    /// running in the opposite direction.
    jobs: Vec<crate::promise::Job>,
    /// What the host `print` has been called with, when a harness installed one. Empty otherwise.
    host_prints: Vec<String>,
    /// The embedder's clock, if one was installed. `None` means the epoch, not advancing.
    host_clock: Option<HostClock>,
    /// The object heap. A `JsValue::Object` is an id it resolves.
    ///
    /// **THE REALM IS WHY THIS IS NOT A FLAT `Vec<Object>`.** Every realm object would cost its
    /// full slot width in RAM before a line of JavaScript runs. [`crate::heap::Heap`] splits the id
    /// space so a realm object can be read from flash and copied into RAM only when something
    /// writes to it -- see that module's header for the barrier and for why the promotion table is
    /// an indirection rather than a pointer.
    heap: crate::heap::Heap,
    /// Set only while the installers replay over a realm the build-time tables already built.
    ///
    /// `None` in every other instant, including for the whole life of a running program, so what a
    /// finished interpreter carries for it is one null pointer. See [`Replay`].
    replay: Option<crate::Box<Replay>>,
    /// How many of [`Self::heap`] the realm installed. Objects below it are the realm's.
    #[cfg(feature = "mutation-census")]
    realm_boundary: usize,
    /// Which realm objects a program has reached for mutation since the realm was sealed.
    #[cfg(feature = "mutation-census")]
    mutated_realm_objects: alloc::collections::BTreeSet<u32>,
    /// Every Symbol this realm has made. A `JsValue::Symbol` is an index into it.
    ///
    /// APPEND-ONLY AND NEVER DEDUPED. Two entries with the same description are two DIFFERENT
    /// symbols, which is the type's whole purpose -- a map keyed by description would silently make
    /// `Symbol("x") === Symbol("x")` true and collapse every private key in a program into one.
    symbols: Vec<Option<JsString>>,
    /// `Symbol.for`'s registry: the key a symbol was registered under, in `symbols` order.
    ///
    /// **A REGISTERED SYMBOL IS A DIFFERENT KIND OF THING FROM `Symbol(x)`, AND THE KEY IS NOT
    /// THE DESCRIPTION.** `Symbol.for("k") !== Symbol("k")`, and `Symbol.keyFor(Symbol("k"))` is
    /// `undefined` even though that symbol's description is `"k"`. Deriving `keyFor` from the
    /// description would report every unregistered symbol as registered, which is exactly backwards
    /// for a registry whose purpose is to say which symbols are shared.
    symbol_registry: Vec<(JsString, SymbolId)>,
    pub(crate) intrinsics: Intrinsics,
    /// The `this` of the running function, innermost last.
    ///
    /// A stack rather than a field, because an ARROW does not get its own -- it uses the one from
    /// where it was WRITTEN, so an arrow's call must not push. Storing `this` on the closure would
    /// work too; a stack keeps the arrow rule to one `if`.
    this_stack: Vec<JsValue>,
    /// Whether each frame's `this` has been INITIALIZED, parallel to `this_stack`.
    ///
    /// A DERIVED CONSTRUCTOR'S `this` DOES NOT EXIST UNTIL `super()` RUNS. Reading it first is a
    /// ReferenceError, not an object with unset fields -- `class D extends B { constructor() {
    /// this.x = 1; super(); } }` must throw, and this engine created the instance eagerly and
    /// handed it over, so the write landed and was then overwritten by the parent's own work. The
    /// symptom is a field that silently is not there.
    ///
    /// `None` MEANS "ALREADY BOUND, NO CELL NEEDED" AND COSTS NOTHING. Only a DERIVED
    /// CONSTRUCTOR can have an unbound `this`, so only it allocates -- every other call pushes
    /// `None` and the per-call cost is unchanged.
    ///
    /// AND IT IS A SHARED CELL RATHER THAN A FLAG, BECAUSE AN ARROW'S `this` IS LEXICAL. An
    /// arrow captures the cell where it is WRITTEN and pushes that same cell when called, so
    /// `iter.f = () => super()` initializes the constructor's `this` even when it is invoked from
    /// deep inside an iterator's `return()` method. Reading the nearest bound frame on the RUNTIME
    /// stack instead looks identical in every direct case and finds the wrong frame in that one.
    derived_this: Vec<Option<Rc<core::cell::RefCell<Option<JsValue>>>>>,
    /// The home object of the running method, for `super.m`; parallel to `this_stack`.
    home_stack: Vec<Option<ObjectId>>,
    /// The parent constructor of the running derived constructor, for `super(...)`.
    class_stack: Vec<Option<ObjectId>>,
    /// `new.target` for each frame, parallel to [`Self::this_stack`]: the constructor `new` named,
    /// or `undefined` in an ordinary call.
    new_target_stack: Vec<JsValue>,
    /// The `new.target` the NEXT call is to receive, set by `[[Construct]]` immediately before it
    /// invokes the body.
    ///
    /// IT IS SET LAST, AFTER THE `prototype` READ THAT PRECEDES IT, AND THE ORDER IS LOAD-BEARING.
    /// `prototype` can be an accessor, so that read can run a program -- and a program that calls
    /// any function would consume this value through [`Self::take_new_target`] and leave the
    /// constructor itself reporting `undefined`.
    pending_new_target: Option<JsValue>,
    /// The prototype `new.target` asks a BUILT-IN constructor's instance to have, for the whole of
    /// that constructor's body. `None` when `new.target` is the built-in itself, which is every
    /// ordinary `new Map()`.
    ///
    /// READING IT IN TIME IS NOT THE SAME AS APPLYING IT IN TIME. `[[Construct]]` reads
    /// `new.target.prototype` BEFORE the body -- deliberately, so a throwing `prototype` getter is
    /// reported ahead of anything the constructor did -- and applying it to the finished object
    /// afterwards is indistinguishable from that, for every built-in that never looks at what it is
    /// building. See [`Interpreter::instance_prototype`] for the four that look.
    ///
    /// Saved and restored around the call rather than taken once, because a built-in constructor
    /// can run user code (an adder, a `prototype` getter, an iterator) that constructs something
    /// else, and the inner construction must not leave its answer behind for the outer one.
    pending_instance_prototype: Option<ObjectId>,
    /// The function object the call about to start was reached THROUGH, for a sloppy `arguments`
    /// object's `callee`.
    ///
    /// A CLOSURE CANNOT NAME ITS OWN FUNCTION OBJECT. A `Callable::Closure` is an index into a
    /// table and the object wrapping it belongs to whoever called it -- `f()`, `f.call()` and
    /// `super()` all arrive at one body through three different objects -- so `callee` was simply
    /// absent, and an absent `callee` reads to a program as a STRICT arguments object. Left the
    /// same way `pending_new_target` is: set at the three sites that know, taken once.
    pending_callee: Option<ObjectId>,
    /// Whether the parameter list just bound was simple, on its way out of the walk that decided it.
    ///
    /// Set only under [`BodyPhase::ParametersOnly`] and taken immediately by the frame below. It
    /// exists because the walk answers with a `Completion` and this is a second answer -- the same
    /// problem, and the same solution, as the `pending_` fields above it.
    pending_params_simple: Option<bool>,
    /// The parameter shape and the bound `this` a generator's creation needs, carried out of the
    /// frame that is about to be popped. Set and taken in the same function.
    pending_generator_frame: Option<(bool, JsValue)>,
    /// The body environment a generator RESUMPTION must run in, handed to the walk that would
    /// otherwise derive a fresh one. See `GeneratorContext::body_scope`.
    pending_generator_body_scope: Option<Scope>,
    /// The `arguments` object built for a sloppy call, waiting for its parameter list to be read.
    ///
    /// Mapped or unmapped depends on whether the parameter list is SIMPLE, which is a fact about
    /// nodes the artifact holds and this frame has not opened yet. See
    /// [`Interpreter::adopt_parameter_map`].
    pending_arguments: Option<ObjectId>,
    /// Whether the code now running is strict.
    ///
    /// IT DECIDES WHETHER A FAILED WRITE IS SILENT OR AN ERROR, which is the single largest
    /// behavioural difference between the two modes and the one this engine had never had. A write
    /// to a non-writable property, to a setter-less accessor or to a frozen object does NOTHING in
    /// sloppy mode and THROWS in strict -- so an engine with only the sloppy behaviour accepts
    /// programs the standard rejects, silently.
    strict: bool,
    /// The strings array each tagged-template CALL SITE hands its tag, keyed by span.
    template_objects: BTreeMap<(usize, usize), ObjectId>,
    /// How deep the call stack is, used only to know when the stack base may be re-read.
    depth: u32,
    /// The address of a local in the OUTERMOST active call, against which stack use is measured.
    stack_base: Option<usize>,
    /// How many bytes of host stack this interpreter may use before reporting recursion.
    ///
    /// A BUDGET IS A PROPERTY OF THE STACK THE CALLER PROVIDED, not of the engine. The default
    /// is safe on the smallest thread we run on; a caller that has deliberately given the engine a
    /// larger stack -- as the conformance runner does -- raises it, and a caller that has not must
    /// never be silently assumed to have done so.
    stack_budget: usize,
    /// How many AST nodes of each kind the evaluator has dispatched on.
    ///
    /// PRESENT ONLY UNDER `bench-counters`, so a timed build does not carry the field, the two
    /// increments or the cache line they touch. See [`crate::counters`].
    #[cfg(feature = "bench-counters")]
    pub counters: crate::counters::NodeCounters,
    /// How many objects the realm itself created, so a lookup can tell a BUILTIN property from one
    /// the program made. Everything below this index belongs to the realm rather than to the run.
    ///
    /// An INDEX rather than a set, because the heap is append-only and the realm is built first:
    /// a comparison is exact and costs nothing, where a set would be a second thing to keep true.
    #[cfg(feature = "bench-counters")]
    realm_objects: usize,
    /// Which realm properties a program actually READ, as `(object, key)`.
    ///
    /// **THIS IS THE MEASUREMENT THAT DECIDES WHETHER LAZY BUILTINS PAY.** Half the realm is
    /// property table (507 properties at 109.8 B); building it lazily is only worth doing if most
    /// of it is never touched, and "most programs use a handful of builtins" is a belief until it
    /// is a count.
    ///
    /// `RefCell` because the lookup path takes `&self` -- and behind the feature, so a timed
    /// build carries neither the cell nor the insert.
    #[cfg(feature = "bench-counters")]
    touched_realm: core::cell::RefCell<alloc::collections::BTreeSet<(u32, String)>>,
}

/// How much host stack one program may consume before recursion is reported as an error.
///
/// **A FRAME COUNT WAS THE WRONG INSTRUMENT AND MEASURING SHOWED IT.** A recursive JavaScript
/// function recurses in Rust here, so `function f(){ f(); } f();` overflows the HOST stack -- which
/// is not catchable, takes the whole run with it, and appears in a report as a crash rather than as
/// the RangeError the standard specifies. The first version counted frames: a limit of 120 survived
/// and 200 overflowed, **for one particular function body**. That number is not a property of the
/// engine -- a function with a heavier body consumes more stack per call, so the same count would
/// overflow for one program and be generous for another, and the failure returns as a crash.
///
/// **What actually runs out is BYTES, so bytes are what is counted.** The budget is measured against
/// a local's address in the outermost call, which makes it independent of how heavy the frames are
/// and of how large the host thread's stack is. Chosen to SEPARATE: comfortably below the 1 MiB a
/// Windows thread gets by default, and far above what any non-recursive program uses.
///
/// It is a deviation to publish rather than hide: a conforming engine permits deeper recursion
/// than this, and where the limit falls depends on the host. Every engine has such a limit for the
/// same reason; ours is stated instead of discovered.
const STACK_BUDGET_BYTES: usize = 384 * 1024;

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a fresh interpreter's realm objects come from.
///
/// **The two arms must produce the same realm, and that they do is checked rather than argued** --
/// `crate::realm_tables` renders both and compares them against each other and against a committed
/// copy. Keeping the second arm alive is what keeps that comparison honest: without it the tables
/// could only ever be compared with themselves.
#[derive(Clone, Copy)]
enum RealmSource {
    /// Assembled from the generated descriptors. What every build does.
    Tables,
    /// Built by running the installers, which is how it worked before the tables existed.
    ///
    /// It costs the realm's whole property store in RAM, which is the cost the tables remove, so
    /// nothing ships this. It is the drift gate's other arm.
    #[cfg_attr(not(test), allow(dead_code))]
    Installers,
}

impl Interpreter {
    #[must_use]
    pub fn new() -> Self {
        Self::with_realm_from(RealmSource::Tables)
    }

    /// An interpreter whose realm was built by RUNNING the installers rather than read from the
    /// generated tables.
    ///
    /// The oracle for [`crate::realm::REALM`], and the only thing that can be one: a table compared
    /// against itself agrees no matter what either of them says.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_installed_realm() -> Self {
        Self::with_realm_from(RealmSource::Installers)
    }

    fn with_realm_from(source: RealmSource) -> Self {
        let global = new_scope(None);
        let mut interpreter = Self {
            global,
            programs: Vec::new(),
            current_program: 0,
            nested_walked: 0,
            nested_refused: 0,
            nested_by_tag: [0; 256],
            closures: Vec::new(),
            natives: Vec::with_capacity(REALM_NATIVE_HINT),
            bounds: Vec::new(),
            jobs: Vec::new(),
            host_prints: Vec::new(),
            host_clock: None,
            heap: crate::heap::Heap::new(REALM_OBJECT_HINT),
            replay: None,
            #[cfg(feature = "mutation-census")]
            realm_boundary: 0,
            #[cfg(feature = "mutation-census")]
            mutated_realm_objects: alloc::collections::BTreeSet::new(),
            symbols: Vec::new(),
            symbol_registry: Vec::new(),
            intrinsics: Intrinsics::default(),
            this_stack: Vec::new(),
            derived_this: Vec::new(),
            home_stack: Vec::new(),
            class_stack: Vec::new(),
            new_target_stack: Vec::new(),
            pending_new_target: None,
            pending_instance_prototype: None,
            pending_callee: None,
            pending_params_simple: None,
            pending_generator_frame: None,
            pending_generator_body_scope: None,
            pending_arguments: None,
            template_objects: BTreeMap::new(),
            strict: false,
            depth: 0,
            stack_base: None,
            stack_budget: STACK_BUDGET_BYTES,
            #[cfg(feature = "bench-counters")]
            counters: crate::counters::NodeCounters::default(),
            #[cfg(feature = "bench-counters")]
            realm_objects: 0,
            #[cfg(feature = "bench-counters")]
            touched_realm: core::cell::RefCell::new(alloc::collections::BTreeSet::new()),
        };
        if let RealmSource::Tables = source {
            for descriptor in &crate::realm::REALM {
                interpreter.heap.allocate(Object::resident(descriptor));
            }
            interpreter.replay =
                Some(crate::Box::new(Replay { next_id: 0, discard: Object::new(None) }));
        }
        crate::builtins::install(&mut interpreter);
        interpreter.finish_replay();
        #[cfg(feature = "bench-counters")]
        {
            interpreter.realm_objects = interpreter.heap.len();
        }
        interpreter
    }

    /// Ends a replay and **refuses a realm whose installers and tables disagree about how many
    /// objects there are.**
    ///
    /// # THIS IS A REAL ASSERT AND THE REASON IS THE ONE THIS CRATE ALREADY WROTE DOWN
    ///
    /// A `debug_assert` here would be no check at all: conformance is measured from `--release`,
    /// this crate's suite is run with `--release`, and a device image is nothing else. The mistake
    /// it guards is the one that reaches every one of those builds and none of the others.
    ///
    /// The failure it catches is total and silent. An installer that creates one object more or
    /// fewer than the tables hold shifts every id from that point on, so each later descriptor
    /// describes its neighbour: `Array.prototype`'s methods land on `String.prototype`, every one of
    /// them a valid object with a plausible shape, and the first thing to notice is a program
    /// getting a wrong answer. Comparing the counts costs one integer and turns it into a refusal at
    /// start-up.
    fn finish_replay(&mut self) {
        let Some(replay) = self.replay.take() else { return };
        assert!(
            replay.next_id as usize == crate::realm::REALM.len(),
            "the installers built a different realm than the tables describe"
        );
    }

    /// Moves this interpreter's realm into leaked memory so it is read the way a flash realm is.
    ///
    /// **A TEST INSTRUMENT AND NOT A SAVING** -- see [`crate::heap::Heap::make_resident_by_leaking`]
    /// for what it does and does not measure. It exists because until the build-time tables land,
    /// the resident path would otherwise run in no build anyone makes.
    ///
    /// Only the heap changes. Everything else holding an `ObjectId` -- the intrinsics table, every
    /// property value, the global scope -- is untouched, because the ids are preserved.
    #[cfg(test)]
    pub(crate) fn make_realm_resident_by_leaking(&mut self) {
        self.heap.make_resident_by_leaking();
    }

    /// The realm properties a program has READ so far, as `(object, key)` pairs.
    ///
    /// Only what was FOUND. A miss walks the prototype chain and finds nothing, and nothing is
    /// what a lazy realm would have had to materialise for it -- so a miss is correctly not counted.
    #[cfg(feature = "bench-counters")]
    #[must_use]
    pub fn touched_realm_properties(&self) -> Vec<(u32, String)> {
        self.touched_realm.borrow().iter().cloned().collect()
    }

    /// Adds an object to the heap and returns its id -- **unless the installers are replaying**, in
    /// which case the object already exists and this hands back the id it was given.
    ///
    /// See [`Replay`]. The id sequence is the same either way because it comes from the same code
    /// running in the same order; that it really is the same is not left to argument, because
    /// [`Interpreter::finish_replay`] refuses a realm whose object count moved.
    pub(crate) fn allocate(&mut self, object: Object) -> ObjectId {
        if let Some(replay) = self.replay.as_mut() {
            drop(object);
            let id = ObjectId(replay.next_id);
            replay.next_id += 1;
            return id;
        }
        self.heap.allocate(object)
    }

    pub(crate) fn object(&self, id: ObjectId) -> &Object {
        self.heap.get(id)
    }

    /// The native registry in registration order, which is what `Callable::Native(u32)` INDEXES.
    ///
    /// **THIS ORDER IS THE MEANING OF EVERY NATIVE INDEX IN THE REALM, AND IT IS THE ONE THING A
    /// GENERATED DESCRIPTOR TABLE CANNOT CARRY** -- see `crate::realm_tables`. A `builtins.rs` edit
    /// that registers a native in the middle repoints every later index at the wrong function,
    /// silently. The snapshot gate exists to read this back and say so.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn native_registry(&self) -> &[NativeFunction] {
        &self.natives
    }

    /// Every symbol the realm allocated, in allocation order: `SymbolId(i)` is entry `i`.
    ///
    /// A `None` description is a symbol created without one, which is not the same as an empty one.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn symbol_table(&self) -> &[Option<JsString>] {
        &self.symbols
    }

    /// **THE WRITE BARRIER FOR A WHOLE OBJECT, AND THE ONLY SOURCE OF A `&mut Object` IN THE
    /// ENGINE.** A flash-resident realm object is copied into RAM by [`crate::heap::Heap::get_mut`]
    /// before this returns, which is what covers the nineteen slots `Store` does not -- `prototype`,
    /// `extensible`, `callable`, `primitive` and every boxed exotic.
    pub(crate) fn object_mut(&mut self, id: ObjectId) -> &mut Object {
        #[cfg(feature = "mutation-census")]
        if (id.0 as usize) < self.realm_boundary {
            self.mutated_realm_objects.insert(id.0);
        }
        if let Some(replay) = self.replay.as_mut() {
            replay.discard.empty_the_property_stores();
            return &mut replay.discard;
        }
        self.heap.get_mut(id)
    }

    /// How many objects the realm installed, and which of them a program has since reached for
    /// mutation.
    ///
    /// The boundary is the heap length at the end of installation, so "is this a realm object" is
    /// an index comparison rather than a flag on every object.
    #[cfg(feature = "mutation-census")]
    #[must_use]
    pub fn mutation_census(&self) -> (usize, &alloc::collections::BTreeSet<u32>) {
        (self.realm_boundary, &self.mutated_realm_objects)
    }

    /// Marks the end of realm installation. Everything allocated before this is realm; everything
    /// after is the program's.
    #[cfg(feature = "mutation-census")]
    pub(crate) fn seal_realm(&mut self) {
        self.realm_boundary = self.heap.len();
        self.mutated_realm_objects.clear();
    }

    /// Makes a NEW symbol. Every call produces a distinct one, description or not.
    pub(crate) fn allocate_symbol(&mut self, description: Option<JsString>) -> SymbolId {
        self.symbols.push(description);
        SymbolId((self.symbols.len() - 1) as u32)
    }

    /// A symbol's description, which is `undefined` when it was created without one.
    ///
    /// `Symbol()` and `Symbol(undefined)` have NO description; `Symbol("")` has an EMPTY one, and
    /// the two are distinguishable (`.description` is `undefined` versus `""`). Collapsing them
    /// makes `String(Symbol(""))` print `Symbol()` instead of `Symbol()`'s neighbour `Symbol()`.
    pub(crate) fn symbol_description(&self, id: SymbolId) -> Option<&JsString> {
        self.symbols[id.0 as usize].as_ref()
    }

    /// `Symbol.for`: the symbol registered under `key`, creating it on first use.
    pub(crate) fn symbol_for(&mut self, key: &JsString) -> SymbolId {
        if let Some((_, id)) = self.symbol_registry.iter().find(|(name, _)| name == key) {
            return *id;
        }
        let id = self.allocate_symbol(Some(key.clone()));
        self.symbol_registry.push((key.clone(), id));
        id
    }

    /// `Symbol.keyFor`: the key a symbol was REGISTERED under, or `None` if it never was.
    pub(crate) fn symbol_key_for(&self, id: SymbolId) -> Option<&JsString> {
        self.symbol_registry.iter().find(|(_, candidate)| *candidate == id).map(|(key, _)| key)
    }

    /// A well-known symbol, by its index in [`WELL_KNOWN_SYMBOLS`].
    ///
    /// CALLING THIS BEFORE THE REALM FILLS THE SLOTS RETURNS `SymbolId(0)`, WHICH IS A REAL
    /// SYMBOL -- so the failure is not a crash but a property written under the WRONG well-known
    /// key. Installing `Symbol.toStringTag` one loop too early stored the tag under
    /// `Symbol.iterator`, nothing complained, and `Object.prototype.toString` went on reporting
    /// `"[object Object]"` because the read looked under the right key and found nothing. The
    /// default value colliding with a valid id is what makes it silent; the assertion below is what
    /// makes it loud.
    ///
    /// # IT IS A REAL ASSERT, BECAUSE A `debug_assert` IS THE SAME AS NO CHECK HERE
    ///
    /// The argument for the weaker form is that this is a build-order invariant rather than a
    /// program-input one, so no input can reach it in a correct build. True, and it misses the
    /// point: **the builds that matter are all release builds.** Conformance is measured from
    /// `--release`, this crate's own suite is run with `--release`, and a device image is nothing
    /// else -- so a `debug_assert` is compiled out of every build anyone ever looks at, and the
    /// mistake it describes gets made a second time under a green gate. It did: `@@species` went
    /// onto the `Array` CONSTRUCTOR under `Symbol.iterator` and `Array[Symbol.species]` read
    /// `undefined`.
    ///
    /// The cost is a bool test beside a linear name search that is already here. The message is
    /// deliberately short and un-formatted: a panic payload is ~2 KB of flash on a part that has
    /// 256 KB of it, and the explanation belongs in this comment rather than in the image.
    pub(crate) fn well_known_symbol(&self, name: &str) -> SymbolId {
        let index = WELL_KNOWN_SYMBOLS
            .iter()
            .position(|candidate| *candidate == name)
            .expect("a well-known symbol this profile carries");
        assert!(self.intrinsics.well_known_symbols_ready, "well-known symbol read before the realm made it");
        self.intrinsics.well_known_symbols[index]
    }

    /// `Symbol.prototype.toString`'s rendering, which is also what `String(sym)` produces.
    ///
    /// It is NOT reachable through `ToString` -- that operation refuses a symbol on purpose. This
    /// is the EXPLICIT form, and the two must stay apart or the refusal has a hole in it.
    pub(crate) fn symbol_to_display(&self, id: SymbolId) -> String {
        match self.symbol_description(id) {
            Some(text) => format!("Symbol({})", text.to_lossy_string()),
            None => "Symbol()".to_string(),
        }
    }

    /// Defines a global binding. Used by the realm builder.
    ///
    /// IT ALSO PUBLISHES ONTO `globalThis`, BECAUSE ONE REALM MAY NOT ANSWER TWO WAYS. A global
    /// object built once from the bindings that existed at that moment is a SNAPSHOT, and anything
    /// installed afterwards -- `print` and `$262` from the host harness, and every
    /// [`Interpreter::define_host_function`] a host adds -- was reachable as a bare identifier and
    /// `undefined` through the object. Mirroring on the way in makes the ordering of the realm
    /// builder stop mattering, which is better than a comment asking the next person to preserve it.
    ///
    /// Nothing happens before `globalThis` itself exists, which is what the realm builder's own
    /// bootstrap needs: the mirror is populated from the bindings once, and from here after.
    pub(crate) fn define_global(&mut self, name: &str, value: JsValue) {
        self.global.borrow_mut().bindings.insert(
            name.to_string(),
            Binding::var(value.clone()),
        );
        if let Some(JsValue::Object(global_object)) = self.global_value("globalThis") {
            self.define_builtin(global_object, name, value);
        }
    }


    /// Publishes a host function as a global.
    ///
    /// A `fn` POINTER CARRIES NO CAPTURED STATE, and that is the honest shape rather than a
    /// limitation to work around. A JS agent is bound to one thread by ECMA-262 -- run-to-completion
    /// and the microtask checkpoint are per-AGENT and normative -- so host state reachable from JS
    /// may only ever be touched from that agent's thread. A thread-local in the host says that in
    /// the type system; a captured closure here would let the constraint be broken silently.
    ///
    /// # THE NAME IS `&'static str`, AND THAT IS A CONSTRAINT WORTH STATING
    ///
    /// A built-in's name is stored beside its function pointer and is never copied into the heap,
    /// which is what keeps a realm's 413 registry entries free of 413 string allocations. A host
    /// function joins the same registry, so its name has to be constant data too -- a literal, or
    /// anything else with a `'static` lifetime.
    ///
    /// In practice a host names its functions in its own source, so this costs nothing. A host that
    /// genuinely computes a name at run time has to arrange for it to outlive the engine.
    pub fn define_host_function(&mut self, name: &'static str, length: u32, call: NativeFn) {
        let function = self.native_function(name, length, call);
        self.define_global(name, JsValue::Object(function));
    }

    /// Builds a JavaScript array a host function can return.
    pub fn array_from(&mut self, values: Vec<JsValue>) -> JsValue {
        let id = self.new_array(values);
        JsValue::Object(id)
    }

    /// Reports a host-side failure to the running program as a real, catchable `Error`.
    ///
    /// A host that returns `undefined` on failure makes every device error look like a missing
    /// property somewhere later. The tier's whole complaint about small engines is silent deviation;
    /// a host adapter is the easiest place to reintroduce it.
    pub fn host_error(&mut self, message: &str) -> Completion {
        self.throw("Error", message)
    }

    /// Reads a global binding, for the realm builder's own cross-references.
    pub(crate) fn global_value(&self, name: &str) -> Option<JsValue> {
        self.global.borrow().bindings.get(name).map(|binding| binding.value.clone())
    }

    /// Every global binding's name, so the realm builder can mirror them ALL onto `globalThis`
    /// instead of naming them again.
    ///
    /// A SECOND LIST OF THE GLOBALS IS A TWIN, AND IT DRIFTED. `globalThis` was built from a
    /// hand-written array of ten names, so `globalThis.Date`, `.JSON`, `.Symbol`, `.Promise`,
    /// `.Map`, `.eval` and every typed array were `undefined` while the same identifiers worked as
    /// bare names -- one realm answering two ways about what is in it. The array grew whenever
    /// somebody remembered, which is not a mechanism. The bindings are the realm; this is how you
    /// ask what they are.
    ///
    /// `BTreeMap`, so the order is the same on every run and a difference in the mirror is a
    /// difference in the realm rather than in a hash seed.
    pub(crate) fn global_binding_names(&self) -> Vec<String> {
        self.global.borrow().bindings.keys().cloned().collect()
    }

    /// Builds an Array exotic object holding the given elements.
    ///
    /// `length` IS NON-ENUMERABLE AND NON-CONFIGURABLE, which is why it is set here rather than
    /// left to an ordinary assignment -- an enumerable `length` shows up in every `for-in` over
    /// every array the built-ins produce.
    pub(crate) fn new_array(&mut self, elements: Vec<JsValue>) -> ObjectId {
        let mut object = Object::new(Some(self.intrinsics.array_prototype));
        object.is_array = true;
        let id = self.allocate(object);
        let length = elements.len();
        for (index, value) in elements.into_iter().enumerate() {
            self.object_mut(id)
                .set_own(PropertyKey::from_str(&index.to_string()), Property::data(value));
        }
        self.object_mut(id).set_own(
            PropertyKey::from_str("length"),
            Property { kind: PropertyKind::Data { value: JsValue::Number(length as f64), writable: true }, enumerable: false, configurable: false },
        );
        id
    }


    /// Creates a built-in function object with its specified `name` and `length`.
    ///
    /// **BOTH ARE NON-WRITABLE AND CONFIGURABLE, AND THE FUNCTION ITSELF IS NOT.** The standard
    /// states these attributes once, for every built-in, in "ECMAScript Standard Built-in Objects";
    /// `verifyCallableProperty` checks them on every built-in a test touches. Creating them as
    /// ordinary data properties would fail those checks everywhere at once, which reads as a spread
    /// of unrelated defects rather than as one wrong default.
    pub(crate) fn native_function(
        &mut self,
        name: &'static str,
        length: u32,
        call: NativeFn,
    ) -> ObjectId {
        self.native_callable(name, length, call, None)
    }

    /// Creates a built-in that is also a constructor, with `[[Call]]` and `[[Construct]]` separate.
    pub(crate) fn native_constructor(
        &mut self,
        name: &'static str,
        length: u32,
        call: NativeFn,
        construct: NativeFn,
    ) -> ObjectId {
        self.native_callable(name, length, call, Some(construct))
    }

    fn native_callable(
        &mut self,
        name: &'static str,
        length: u32,
        call: NativeFn,
        construct: Option<NativeFn>,
    ) -> ObjectId {
        self.natives.push(NativeFunction { name, length, call, construct });
        let index = (self.natives.len() - 1) as u32;
        let mut object = Object::new(Some(self.intrinsics.function_prototype));
        object.callable = Some(Callable::Native(index));
        let id = self.allocate(object);
        self.set_function_metadata(id, name, length);
        id
    }

    /// The revocation function `Proxy.revocable` returns beside its proxy.
    ///
    /// ITS `name` IS THE EMPTY STRING AND ITS `length` IS 0, which the corpus checks directly --
    /// it is an anonymous built-in function, like the resolving functions a promise hands out, and
    /// giving it a helpful name would fail `revocation-function-name.js` on the name it was given.
    pub(crate) fn new_proxy_revoker(&mut self, proxy: ObjectId) -> ObjectId {
        let mut object = Object::new(Some(self.intrinsics.function_prototype));
        object.callable = Some(Callable::ProxyRevoker { proxy });
        let id = self.allocate(object);
        self.set_function_metadata(id, "", 0);
        id
    }

    /// [`Self::set_function_metadata`] reached from outside this module, for the one object that
    /// cannot be built by the usual route: `Function.prototype` exists before any function can.
    pub(crate) fn set_function_shape(&mut self, id: ObjectId, name: &str, length: u32) {
        self.set_function_metadata(id, name, length);
    }

    fn set_function_metadata(&mut self, id: ObjectId, name: &str, length: u32) {
        let fixed = |value: JsValue| Property {
            kind: PropertyKind::Data { value, writable: false },
            enumerable: false,
            configurable: true,
        };
        self.object_mut(id)
            .set_own(PropertyKey::from_str("length"), fixed(JsValue::Number(f64::from(length))));
        self.object_mut(id)
            .set_own(PropertyKey::from_str("name"), fixed(JsValue::string(name)));
    }

    /// Attaches a built-in method to an object, with the attributes every built-in method has.
    pub(crate) fn define_method(
        &mut self,
        target: ObjectId,
        name: &'static str,
        length: u32,
        call: NativeFn,
    ) {
        let function = self.native_function(name, length, call);
        self.object_mut(target).set_own(
            PropertyKey::from_str(name),
            Property::builtin(JsValue::Object(function)),
        );
    }

    /// Attaches a method under a well-known symbol, with the attributes the standard gives them.
    ///
    /// A SYMBOL-KEYED BUILT-IN IS NOT WRITABLE, where a string-keyed one is -- `{ [[Writable]]:
    /// false, [[Enumerable]]: false, [[Configurable]]: true }` is stated method by method for these
    /// and differs from [`Interpreter::define_method`]'s attributes. Two of them exist, so this is
    /// the one place that shape is written down.
    pub(crate) fn define_well_known_symbol_method(
        &mut self,
        target: ObjectId,
        symbol: &str,
        function: ObjectId,
    ) {
        let key = PropertyKey::Symbol(self.well_known_symbol(symbol));
        self.object_mut(target).set_own(
            key,
            Property {
                kind: PropertyKind::Data { value: JsValue::Object(function), writable: false },
                enumerable: false,
                configurable: true,
            },
        );
    }

    /// Attaches a non-enumerable data property, which is what a built-in value property is.
    pub(crate) fn define_builtin(&mut self, target: ObjectId, name: &str, value: JsValue) {
        self.object_mut(target)
            .set_own(PropertyKey::from_str(name), Property::builtin(value));
    }

    /// Installs the host functions a TEST HARNESS needs, which are **not** part of the language.
    ///
    /// # WHY THIS IS OPT-IN AND NOT PART OF THE REALM
    ///
    /// `print` and `$262` are not in ECMA-262. `print` is a shell convention and `$262` is defined
    /// by the conformance suite's own interpreting guide as the interface a HOST provides to it.
    /// Installing
    /// them unconditionally would put two non-standard globals in every realm -- including a device
    /// image, which has nothing to print to -- and would make the engine carry surface it exists to
    /// avoid.
    ///
    /// It also keeps a line this project cares about: **an engine that grades itself can pass by
    /// mis-grading.** The harness owns the observation; this is the engine offering a seam for it,
    /// not the engine deciding what its results are.
    pub fn install_host_harness(&mut self) {
        let print = self.native_function("print", 1, |interpreter, _this, arguments| {
            let text = match interpreter.to_string_value(&crate::builtins::arg(arguments, 0)) {
                Ok(text) => text,
                Err(abrupt) => return abrupt,
            };
            interpreter.host_prints.push(text.to_lossy_string());
            Completion::Normal(JsValue::Undefined)
        });
        self.define_global("print", JsValue::Object(print));

        let host = self.allocate(Object::new(Some(self.intrinsics.object_prototype)));
        if let Some(global) = self.global_value("globalThis") {
            self.define_builtin(host, "global", global);
        }
        self.define_method(host, "detachArrayBuffer", 1, |interpreter, _this, arguments| {
            crate::binary::detach(interpreter, &crate::builtins::arg(arguments, 0))
        });
        self.define_method(host, "evalScript", 1, |interpreter, _this, arguments| {
            let source = match interpreter.to_string_value(&crate::builtins::arg(arguments, 0)) {
                Ok(source) => source,
                Err(abrupt) => return abrupt,
            };
            match interpreter.run_source(&source.to_lossy_string()) {
                Ok(completion) => completion,
                Err(message) => interpreter.throw("SyntaxError", &message),
            }
        });
        self.define_global("$262", JsValue::Object(host));
    }

    /// Everything `print` was called with since the last call, in order.
    ///
    /// It DRAINS. A runner that read without clearing would see the previous test's output in the
    /// next test's observation, and an async test's completion line is exactly the sort of thing
    /// that would then be attributed to the wrong run.
    pub fn take_host_prints(&mut self) -> Vec<String> {
        core::mem::take(&mut self.host_prints)
    }


    /// Installs the host clock: an anchor in milliseconds since the epoch, and a monotonic source.
    ///
    /// # TWO NUMBERS BECAUSE THERE ARE TWO QUESTIONS
    ///
    /// *"How much time has passed"* is answerable wherever a monotonic counter exists and **must be
    /// answered, never refused**. *"What is the date"* is not answerable without an anchor, and the
    /// epoch is the honest answer to it. A clock that returns a CONSTANT when unset answers the
    /// first question wrongly in order to answer the second honestly: code that times itself then
    /// reads `0 ms` for real work, and a gate over that number passes.
    ///
    /// So an embedder may install either half:
    ///
    /// - **monotonic only** (every board in this tree today): `Date.now()` starts at 1970 and
    ///   COUNTS CORRECTLY. `t1 - t0` is right; `getFullYear()` is 1970, which is obviously wrong
    ///   rather than plausibly wrong.
    /// - **both**: an ordinary wall clock.
    /// - **neither**: the epoch, not advancing. The engine cannot invent elapsed time it has no
    ///   source for, and inventing one is the failure this whole split exists to avoid.
    ///
    /// THE ANCHOR IS TAKEN ONCE AND RIDES THE MONOTONIC COUNT, rather than being re-read. A wall
    /// clock that is re-read can go BACKWARDS, and ECMAScript has no other clock -- so a `Date.now()`
    /// that jumps would be the only timing source in the language, jumping.
    ///
    /// AND THE QUERY "IS THE CLOCK SET" IS DELIBERATELY NOT EXPOSED TO JAVASCRIPT YET. It
    /// belongs in a `Lamella.*` namespace rather than on `Date` -- which is ECMA-262's and not ours
    /// to extend -- and **its name is not settled**. Until then a program cannot distinguish 1970
    /// from a real date, which is a stated gap.
    pub fn set_host_clock(&mut self, epoch_millis: Option<f64>, monotonic_millis: fn() -> f64) {
        self.host_clock = Some(HostClock {
            anchor_epoch_millis: epoch_millis,
            anchor_monotonic_millis: monotonic_millis(),
            monotonic: monotonic_millis,
        });
    }

    /// `Date.now()`'s value: the anchor plus however much has elapsed since it was taken.
    pub(crate) fn host_now_millis(&self) -> f64 {
        let Some(clock) = &self.host_clock else { return 0.0 };
        let elapsed = (clock.monotonic)() - clock.anchor_monotonic_millis;
        clock.anchor_epoch_millis.unwrap_or(0.0) + elapsed
    }

    /// Adds a job to the microtask queue.
    pub(crate) fn enqueue(&mut self, job: crate::promise::Job) {
        self.jobs.push(job);
    }

    /// Runs queued jobs until there are none, which is what makes a promise settle.
    ///
    /// A JOB MAY QUEUE MORE JOBS, AND THAT IS THE NORMAL CASE -- `p.then(f).then(g)` runs `g`'s
    /// job only because `f`'s job settled the promise `g` is waiting on. So this drains until empty
    /// rather than iterating the queue as it stood on entry, and an `n`-deep chain takes `n` turns.
    ///
    /// THE BOUND IS DELIBERATE AND IT IS NOT A CORRECTNESS DEVICE. A program can write a promise
    /// loop that queues a job from every job -- `function again() { Promise.resolve().then(again); }`
    /// -- which is a legal program with no completion, exactly like `while (true) {}`. An unbounded
    /// drain hangs the process; the corpus contains tests that do this ON PURPOSE, and a watchdog
    /// killing the run reports it as a timeout rather than as what it is. The cap stops the drain
    /// and leaves the remaining jobs queued, which is the closest thing to "the turn ended" this
    /// engine can say.
    pub(crate) fn drain_jobs(&mut self) {
        const MAX_JOBS_PER_TURN: usize = 100_000;
        let mut ran = 0usize;
        while ran < MAX_JOBS_PER_TURN {
            if self.jobs.is_empty() {
                return;
            }
            let job = self.jobs.remove(0);
            crate::promise::run_job(self, job);
            ran += 1;
        }
    }

    /// Attaches `Symbol.toStringTag`, which is what makes `Object.prototype.toString.call(x)` say
    /// `"[object Map]"` rather than `"[object Object]"`.
    ///
    /// THE ATTRIBUTES ARE THE POINT, and they are an odd pair: **non-writable and CONFIGURABLE**.
    /// Every built-in tag in the standard has exactly that shape, and the suite checks it directly
    /// -- so a tag installed with the ordinary built-in attributes is a wrong answer, not a
    /// near-miss. This exists as one function because it was three identical copies in three
    /// modules, which is how the fourth one gets the attributes wrong.
    pub(crate) fn define_to_string_tag(&mut self, target: ObjectId, tag: &str) {
        let key = PropertyKey::Symbol(self.well_known_symbol("toStringTag"));
        self.object_mut(target).set_own(
            key,
            Property {
                kind: PropertyKind::Data { value: JsValue::string(tag), writable: false },
                enumerable: false,
                configurable: true,
            },
        );
    }

    /// Attaches a property that a program may neither write, enumerate nor delete: `Math.PI`.
    pub(crate) fn define_constant(&mut self, target: ObjectId, name: &str, value: JsValue) {
        self.object_mut(target).set_own(
            PropertyKey::from_str(name),
            Property { kind: PropertyKind::Data { value, writable: false }, enumerable: false, configurable: false },
        );
    }


    /// Builds an Error object of the named kind.
    ///
    /// **A THROWN ERROR MUST BE AN OBJECT, AND THIS ENGINE THREW STRINGS.** Every `negative`
    /// runtime test names a CONSTRUCTOR, and the runner reads `thrown.constructor.name`; a string
    /// has no constructor, so every such test failed for a reason that had nothing to do with the
    /// rule under test. `assert.throws` is worse -- it reports "Thrown value was not an object!",
    /// burying the real cause under a harness message.
    pub(crate) fn make_error(&mut self, kind: &str, message: &str) -> JsValue {
        let index = ERROR_KINDS.iter().position(|k| *k == kind).unwrap_or(0);
        let prototype = self.intrinsics.native_error_prototypes[index];
        let mut object = Object::new(Some(prototype));
        object.error = true;
        let id = self.allocate(object);
        self.object_mut(id).set_own(
            PropertyKey::from_str("message"),
            Property::builtin(JsValue::string(message)),
        );
        JsValue::Object(id)
    }

    /// The abrupt completion that throws a fresh error of the named kind.
    pub(crate) fn throw(&mut self, kind: &str, message: &str) -> Completion {
        let error = self.make_error(kind, message);
        Completion::Throw(error)
    }

    pub(crate) fn type_error(&mut self, message: &str) -> Completion {
        self.throw("TypeError", message)
    }

    /// Refuses a feature this profile does not implement, from the CLOSED set.
    ///
    /// Taking an [`Absence`] rather than a string is the whole point: a refusal cannot be written
    /// without appearing on the published list, because there is no way to spell one that is not
    /// already a variant.
    pub(crate) fn refuse(&mut self, absence: Absence) -> Completion {
        self.throw("InternalError", absence.message())
    }

    /// Reports a condition that means THIS ENGINE IS BROKEN, never a published absence.
    ///
    /// **THESE ARE NOT ABSENCES AND MUST NOT BE THROWN AS ONE.** "a method must be a function
    /// object", "a tagged template needs a template literal" and "a for-in head with no binding"
    /// are invariants: if one fires, the engine has a defect. Under the absence marker the report
    /// publishes that defect as a gap the engine had chosen -- the bucket dishonesty running in the
    /// one direction nothing guards. A distinct kind keeps them loud and keeps the list honest.
    pub(crate) fn internal_defect(&mut self, message: &str) -> Completion {
        self.throw("InternalDefect", message)
    }


    /// Calls any callable value: a closure, a built-in or a bound function.
    ///
    /// **ONE ENTRY POINT, BECAUSE THREE CALL SITES WOULD DRIFT.** `f()`, `f.m()`, `new f()`,
    /// `Function.prototype.call` and every built-in that takes a callback all arrive here, so a rule
    /// about calling -- the depth limit, the `this` binding, what a non-callable does -- is stated
    /// once and cannot be right in one path and wrong in another.
    pub(crate) fn call_value(
        &mut self,
        callee: &JsValue,
        this: JsValue,
        arguments: Vec<JsValue>,
    ) -> Completion {
        let JsValue::Object(id) = callee else {
            return self.type_error("not a function");
        };
        let Some(callable) = self.object(*id).callable else {
            return self.type_error("not a function");
        };
        if self.stack_is_exhausted() {
            return self.throw("RangeError", "maximum call stack size exceeded");
        }
        self.depth += 1;
        let completion = match callable {
            Callable::Closure(index) => {
                let closure = self.closures[index as usize].clone();
                if closure.class_kind != ClassKind::NotAClass {
                    self.depth -= 1;
                    return self.type_error("a class constructor cannot be called without `new`");
                }
                self.pending_callee = Some(*id);
                if closure.is_generator {
                    self.call_generator(&closure, index, *id, arguments, this)
                } else {
                    self.call(&closure, arguments, this)
                }
            }
            Callable::Native(index) => {
                let native = self.natives[index as usize].call;
                native(self, this, &arguments)
            }
            Callable::Resolver { promise, rejects } => {
                crate::promise::call_resolver(self, promise, rejects, &arguments)
            }
            Callable::Combinator { state, index, rejects } => {
                crate::promise::call_combinator(self, state, index, rejects, &arguments)
            }
            Callable::CapabilityExecutor { state } => {
                crate::promise::call_capability_executor(self, state, &arguments)
            }
            Callable::Proxy => crate::proxy::call(self, *id, this, arguments),
            Callable::ProxyRevoker { proxy } => {
                crate::proxy::revoke(self, proxy);
                Completion::Normal(JsValue::Undefined)
            }
            Callable::Bound(index) => {
                let target = JsValue::Object(self.bounds[index as usize].target);
                let bound_this = self.bounds[index as usize].this.clone();
                let mut all = self.bounds[index as usize].arguments.clone();
                all.extend(arguments);
                self.call_value(&target, bound_this, all)
            }
        };
        self.depth -= 1;
        completion
    }

    /// Raises the host-stack budget, which a caller may do only after guaranteeing the stack.
    ///
    /// **THE GUARANTEE IS THE CALLER'S AND CANNOT BE CHECKED HERE.** Rust cannot portably ask how
    /// much stack a thread has, so setting this larger than the thread actually owns converts a
    /// catchable RangeError back into the process death it exists to prevent. A caller that has not
    /// spawned its own thread with an explicit size has no business calling this.
    pub fn set_stack_budget(&mut self, bytes: usize) {
        self.stack_budget = bytes;
    }

    /// Whether this call would take the host stack past its budget.
    ///
    /// The base is re-read whenever nothing is executing, because a later program may start from
    /// a different point on the host stack -- a base captured once and kept would measure a
    /// difference against an address that no longer means anything.
    fn stack_is_exhausted(&mut self) -> bool {
        let probe = 0u8;
        let here = core::ptr::addr_of!(probe) as usize;
        if self.depth == 0 {
            self.stack_base = Some(here);
            return false;
        }
        let base = *self.stack_base.get_or_insert(here);
        base.abs_diff(here) > self.stack_budget
    }

    /// `[[Construct]]`: what `new f(...)` does, where the constructor and `new.target` are one
    /// object.
    pub(crate) fn construct(&mut self, id: ObjectId, arguments: Vec<JsValue>) -> Completion {
        self.construct_with_new_target(id, arguments, id)
    }

    /// `[[Construct]]` where `new.target` is somebody ELSE.
    ///
    /// THE TWO ARE THE SAME OBJECT FOR `new f()` AND DIFFERENT FOR EVERYTHING INTERESTING.
    /// `super()` propagates the class `new` named rather than the parent being entered, and
    /// `Reflect.construct(target, args, newTarget)` lets a program choose. **`new.target` is what
    /// decides the new object's prototype**, so passing the wrong one produces an instance of the
    /// wrong class -- an object that answers every method call and fails `instanceof`.
    pub(crate) fn construct_with_new_target(
        &mut self,
        id: ObjectId,
        arguments: Vec<JsValue>,
        new_target: ObjectId,
    ) -> Completion {
        let Some(callable) = self.object(id).callable else {
            return self.type_error("not a constructor");
        };
        if self.stack_is_exhausted() {
            return self.throw("RangeError", "maximum call stack size exceeded");
        }
        self.depth += 1;
        let completion = match callable {
            Callable::Native(index) => match self.natives[index as usize].construct {
                Some(construct) => {
                    let requested = if new_target == id {
                        None
                    } else {
                        match self.get_property(new_target, &PropertyKey::from_str("prototype")) {
                            Completion::Normal(JsValue::Object(prototype)) => Some(prototype),
                            Completion::Normal(_) => None,
                            abrupt => {
                                self.depth -= 1;
                                return abrupt;
                            }
                        }
                    };
                    let outer = core::mem::replace(&mut self.pending_instance_prototype, requested);
                    let produced = construct(self, JsValue::Object(id), &arguments);
                    self.pending_instance_prototype = outer;
                    if let (Some(prototype), Completion::Normal(JsValue::Object(object))) =
                        (requested, &produced)
                    {
                        self.object_mut(*object).prototype = Some(prototype);
                    }
                    produced
                }
                None => self.type_error("not a constructor"),
            },
            Callable::Resolver { .. }
            | Callable::Combinator { .. }
            | Callable::CapabilityExecutor { .. }
            | Callable::ProxyRevoker { .. } => self.type_error("not a constructor"),
            Callable::Proxy => {
                if self.is_constructor(id) {
                    crate::proxy::construct(self, id, arguments, new_target)
                } else {
                    self.type_error("not a constructor")
                }
            }
            Callable::Bound(index) => {
                let target = self.bounds[index as usize].target;
                let mut all = self.bounds[index as usize].arguments.clone();
                all.extend(arguments);
                let inner = if new_target == id { target } else { new_target };
                self.construct_with_new_target(target, all, inner)
            }
            Callable::Closure(index) => {
                let closure = self.closures[index as usize].clone();
                if closure.is_arrow {
                    self.type_error("an arrow function is not a constructor")
                } else if closure.is_method {
                    self.type_error("a method is not a constructor")
                } else if closure.is_generator {
                    self.type_error("a generator function is not a constructor")
                } else if closure.class_kind == ClassKind::Derived {
                    self.pending_new_target = Some(JsValue::Object(new_target));
                    self.pending_callee = Some(id);
                    self.call(&closure, arguments, JsValue::Undefined)
                } else {
                    let prototype =
                        match self.get_property(new_target, &PropertyKey::from_str("prototype")) {
                            Completion::Normal(JsValue::Object(proto)) => proto,
                            Completion::Normal(_) => self.intrinsics.object_prototype,
                            abrupt => {
                                self.depth -= 1;
                                return abrupt;
                            }
                        };
                    let instance = self.allocate(Object::new(Some(prototype)));
                    let this = JsValue::Object(instance);
                    self.pending_new_target = Some(JsValue::Object(new_target));
                    self.pending_callee = Some(id);
                    match self.call(&closure, arguments, this.clone()) {
                        Completion::Normal(JsValue::Object(returned)) => {
                            Completion::Normal(JsValue::Object(returned))
                        }
                        Completion::Normal(_) => Completion::Normal(this),
                        abrupt => abrupt,
                    }
                }
            }
        };
        self.depth -= 1;
        completion
    }

    /// Reads a property and calls it: the `obj.m(...)` shape, used throughout the built-ins.
    pub(crate) fn invoke(
        &mut self,
        target: &JsValue,
        name: &str,
        arguments: Vec<JsValue>,
    ) -> Completion {
        let method = normal!(self.get_member(target, &PropertyKey::from_str(name)));
        self.call_value(&method, target.clone(), arguments)
    }

    #[must_use]
    pub(crate) fn is_callable(&self, value: &JsValue) -> bool {
        matches!(value, JsValue::Object(id) if self.object(*id).callable.is_some())
    }

    /// Registers a bound function and returns the object that calls it.
    ///
    /// `prototype` is `BoundFunctionCreate` step 1's answer -- the TARGET's
    /// `[[GetPrototypeOf]]()`, read by the caller because its trap is observable and the standard
    /// runs it before the `length` and `name` reads. It is `%Function.prototype%` for every
    /// ordinary function, which is what an unconditional intrinsic here looked like.
    pub(crate) fn bind_function(
        &mut self,
        target: ObjectId,
        this: JsValue,
        arguments: Vec<JsValue>,
        name: &str,
        length: f64,
        prototype: Option<ObjectId>,
    ) -> ObjectId {
        self.bounds.push(BoundFunction { target, this, arguments });
        let index = (self.bounds.len() - 1) as u32;
        let mut object = Object::new(prototype);
        object.callable = Some(Callable::Bound(index));
        let id = self.allocate(object);
        self.set_function_metadata(id, name, 0);
        self.object_mut(id).set_own(
            PropertyKey::from_str("length"),
            Property {
                kind: PropertyKind::Data { value: JsValue::Number(length), writable: false },
                enumerable: false,
                configurable: true,
            },
        );
        id
    }

    /// `[[Get]]`: an own property, else the PROTOTYPE CHAIN, else `undefined`.
    ///
    /// WARNING: THE CHAIN IS THE POINT. An object model that looks only at own properties makes
    /// every inherited method missing, and the failure is `undefined is not a function` far from
    /// the cause.
    pub(crate) fn get_property(&mut self, id: ObjectId, key: &PropertyKey) -> Completion {
        self.get_property_with_receiver(id, key, JsValue::Object(id))
    }

    /// `[[Get]]` where the search STARTS somewhere other than the receiver.
    ///
    /// A PRIMITIVE'S PROPERTIES ARE LOOKED UP ON ITS WRAPPER PROTOTYPE WHILE `this` STAYS THE
    /// PRIMITIVE. `"abc".charAt(0)` finds the method on `String.prototype` and calls it with the
    /// string; an accessor there would have to see the string too, not the prototype object.
    pub(crate) fn get_property_with_receiver(
        &mut self,
        id: ObjectId,
        key: &PropertyKey,
        receiver: JsValue,
    ) -> Completion {
        if let Some(view) = self.typed_array_view(id) {
            if let Some(index) = crate::typed_array::canonical_numeric_index(key) {
                return Completion::Normal(crate::typed_array::get_element(self, &view, index));
            }
        }
        if let Some((scope, name)) = self.parameter_alias(id, key) {
            return Completion::Normal(self.read_parameter_alias(&scope, &name));
        }
        match self.find_property(id, key) {
            Found::Proxy(proxy) => crate::proxy::get(self, proxy, key, receiver),
            Found::Property(property) => match property.kind {
                PropertyKind::Data { value, .. } => Completion::Normal(value),
                PropertyKind::Accessor { get: None, .. } => Completion::Normal(JsValue::Undefined),
                PropertyKind::Accessor { get: Some(getter), .. } => {
                    self.call_value(&JsValue::Object(getter), receiver, Vec::new())
                }
            },
            Found::Missing => Completion::Normal(JsValue::Undefined),
        }
    }

    /// The property record a lookup finds, walking the chain. Runs no code.
    ///
    /// A PROXY ENDS THE WALK WITHOUT ANSWERING IT: see [`Found::Proxy`]. Callers that cannot run
    /// code -- [`Self::peek_property`] is the only one -- treat that as "nothing found", which is
    /// the same answer they would give for a getter.
    fn find_property(&self, id: ObjectId, key: &PropertyKey) -> Found {
        let mut current = Some(id);
        while let Some(object_id) = current {
            let object = self.object(object_id);
            if object.proxy.is_some() {
                return Found::Proxy(object_id);
            }
            if let Some(property) = object.own(key) {
                #[cfg(feature = "bench-counters")]
                if (object_id.0 as usize) < self.realm_objects {
                    self.touched_realm
                        .borrow_mut()
                        .insert((object_id.0, key.to_display().to_string()));
                }
                return Found::Property(property.clone());
            }
            current = object.prototype;
        }
        Found::Missing
    }

    /// A read that must NOT run code, for the two places that format a diagnostic.
    ///
    /// **THIS IS NOT A SECOND `[[Get]]` AND MUST NEVER BE USED AS ONE.** Rendering a thrown value
    /// happens while an error is already being reported; invoking a getter there could throw again,
    /// or run arbitrary user code to produce a message about user code that just failed. An accessor
    /// therefore reads as `undefined` HERE and only here -- everywhere a program can observe the
    /// result, `get_property` calls the getter.
    ///
    /// A PROXY READS AS `undefined` TOO, for the same reason and more strongly: its trap is not
    /// merely user code, it is user code that has already been given a reason to misbehave.
    fn peek_property(&self, id: ObjectId, key: &PropertyKey) -> JsValue {
        match self.find_property(id, key) {
            Found::Property(property) => property.data_value().cloned().unwrap_or(JsValue::Undefined),
            Found::Proxy(_) | Found::Missing => JsValue::Undefined,
        }
    }

    /// `CreateDataProperty`: define an own, fully-permissive data property **without consulting the
    /// prototype chain**.
    ///
    /// AN OBJECT LITERAL DEFINES; IT DOES NOT ASSIGN, and this engine was assigning. The two
    /// differ exactly when the prototype chain has something to say, which for `Object.prototype`
    /// is now the case: adding the `__proto__` accessor made `{['__proto__']: 1}` go through
    /// `[[Set]]`, find the inherited SETTER, call it, and create no own property at all. The object
    /// came out with no `__proto__` of its own and the wrong prototype -- from a fix to an unrelated
    /// feature, because the literal was using the wrong operation and nothing had exercised the
    /// difference before.
    ///
    /// The same difference is what `__proto__-poisoned-object-prototype.js` is about, and it is
    /// general: any accessor a program installs on `Object.prototype` would have hijacked every
    /// object literal in the realm.
    ///
    /// # AND IT IS `[[DefineOwnProperty]]`, WHICH MEANS IT CAN SAY NO
    ///
    /// This wrote into the store and could not fail, so every caller was a definition that always
    /// succeeded. `InternalizeJSONProperty` is the one whose target belongs to the program: a
    /// reviver that makes a sibling element non-configurable makes the NEXT
    /// `CreateDataProperty(val, k, newElement)` return `false`, and the standard's `Perform ?` on a
    /// normal completion carrying `false` IGNORES it -- so the element keeps its parsed value and
    /// no exception is raised. Storing regardless replaced a value the object had refused to give
    /// up, which is the one outcome a non-configurable property exists to prevent.
    ///
    /// `false` IS NOT AN ERROR, which is why this reports a Boolean rather than throwing.
    /// `CreateDataPropertyOrThrow` is a DIFFERENT operation, and the callers that want it say so.
    pub(crate) fn create_data_property(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
    ) -> Result<bool, Completion> {
        let mut descriptor = crate::builtins::RawDescriptor::empty();
        descriptor.value = value;
        descriptor.writable = true;
        descriptor.enumerable = true;
        descriptor.configurable = true;
        descriptor.present.value = true;
        descriptor.present.writable = true;
        descriptor.present.enumerable = true;
        descriptor.present.configurable = true;
        Ok(crate::builtins::define_own_property(self, id, key, &descriptor)?.is_none())
    }

    /// The write half of `super.x = v`: the RULE comes from `id` and the PROPERTY lands on
    /// `receiver`.
    ///
    /// `super.x = v` DOES NOT WRITE TO THE PROTOTYPE. The lookup starts at the home object's
    /// prototype -- which is what makes `super.x` mean the overridden member rather than the
    /// overriding one -- and the property is created on `this`. An engine that writes where it
    /// looked mutates the CLASS, so every instance of it changes at once and the one being
    /// initialized does not.
    pub(crate) fn set_property_with_receiver(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
        receiver: JsValue,
    ) -> Completion {
        let throw = self.strict;
        match self.ordinary_set(id, key.clone(), value, receiver) {
            Ok(None) => Completion::Normal(JsValue::Undefined),
            Ok(Some(why)) => self.refuse_write(&key, why, throw),
            Err(abrupt) => abrupt,
        }
    }

    /// Step 2 of a super property reference: `env.GetThisBinding()`, which is where an
    /// uninitialized `this` is reported.
    ///
    /// IT IS SEPARATE FROM THE BASE BECAUSE THE KEY EXPRESSION RUNS BETWEEN THEM. `super[f()]`
    /// evaluates `f()` at step 3 and reads the home object's prototype at step 7, so a `f` that
    /// re-parents the object changes what `super` resolves to -- and `superPropOrdering.js` tests
    /// exactly that, in both directions. Bundling the two put the base read before the key
    /// expression and made the ordering wrong the other way.
    pub(crate) fn super_this(&mut self) -> Result<JsValue, Completion> {
        if self.home_stack.last().copied().flatten().is_none() {
            return Err(self.throw("SyntaxError", "`super.x` is only legal inside a method"));
        }
        if !self.this_is_initialized() {
            return Err(self.this_before_super());
        }
        Ok(self.current_this())
    }

    /// `GetSuperBase`: the home object's prototype, read at step 7 -- after the key expression and
    /// before `ToPropertyKey`.
    ///
    /// # A NULL PROTOTYPE IS A VALUE HERE, NOT A REFUSAL
    ///
    /// `GetSuperBase` returns `home.[[GetPrototypeOf]]()` whatever it is, and `null` is a perfectly
    /// ordinary answer. The refusal belongs to the `ToObject` step of `GetValue`/`PutValue`, which
    /// runs LATER -- and for a write, later than the right-hand side:
    ///
    /// ```text
    /// class C { static m() { super[0] = count += 1; } }
    /// Object.setPrototypeOf(C, null);
    /// C.m()        // TypeError -- and `count` is 1, because the value ran first
    /// ```
    ///
    /// Throwing here produced the same TypeError one step too early, so the increment never
    /// happened. The exception was right and the program's effects were not.
    pub(crate) fn super_prototype(&mut self) -> Result<JsValue, Completion> {
        let Some(Some(home)) = self.home_stack.last().copied() else {
            return Err(self.throw("SyntaxError", "`super.x` is only legal inside a method"));
        };
        Ok(match self.object(home).prototype {
            Some(start) => JsValue::Object(start),
            None => JsValue::Null,
        })
    }

    /// The read half of a compound or logical assignment, which for `super.x` starts on the home
    /// object's prototype and keeps `this` as the receiver.
    ///
    /// For an ordinary target `base` and `receiver` are the same value and this is exactly
    /// `get_member`, primitive receivers included.
    pub(crate) fn read_with_receiver(
        &mut self,
        base: &JsValue,
        receiver: &JsValue,
        key: &PropertyKey,
    ) -> Completion {
        match base {
            JsValue::Object(id) => self.get_property_with_receiver(*id, key, receiver.clone()),
            _ => self.get_member(base, key),
        }
    }

    /// `PutValue` step 5 for a property reference: `ToObject` the base, then `[[Set]]` with the
    /// reference's own this-value as the receiver.
    ///
    /// # A PRIMITIVE BASE IS WRITTEN THROUGH, NOT REFUSED
    ///
    /// `ToObject(sym)` is a Symbol wrapper and its `[[Set]]` walks `Symbol.prototype`, so
    /// `sym.prop = 1` **calls a setter defined there, with the SYMBOL as `this`** -- and with no
    /// setter it fails the receiver-is-not-an-object test, which sloppy code drops and strict code
    /// reports. Two arms of the assignment evaluator refused a non-object base outright instead,
    /// which threw for `sym.foo = 1` in sloppy code, where the standard does nothing at all, and
    /// never ran a setter that existed. Only `null` and `undefined` refuse, and they refuse in
    /// `ToObject`.
    ///
    /// The read half is [`Self::read_with_receiver`], directly above; this is the same shape and
    /// the reason both are here rather than at their call sites.
    pub(crate) fn write_with_receiver(
        &mut self,
        base: &JsValue,
        receiver: JsValue,
        key: PropertyKey,
        value: JsValue,
    ) -> Completion {
        let id = match base {
            JsValue::Object(id) => *id,
            JsValue::Undefined | JsValue::Null => {
                return self.type_error("cannot set a property of null or undefined");
            }
            primitive => match self.to_object(&primitive.clone()) {
                Ok(id) => id,
                Err(abrupt) => return abrupt,
            },
        };
        self.set_property_with_receiver(id, key, value, receiver)
    }

    /// `Set(O, P, V, true)`: the form **every built-in uses**, whose `Throw` is not the caller's
    /// mode.
    ///
    /// THE STANDARD PASSES `true` FROM `Array.prototype.push` AND FRIENDS REGARDLESS OF THE
    /// CALLING CODE'S MODE, and this engine had only the assignment form -- so `Object.freeze(a);
    /// a.push(1)` returned a new length and changed nothing, in sloppy code, where the standard
    /// requires a TypeError. **A refused write that reports success is the failure mode this whole
    /// tier exists to refuse**: the call appears to work and the next read disagrees with it. 64
    /// runs across `push`, `pop`, `shift`, `unshift` and `splice` were in exactly that state.
    pub(crate) fn set_property_or_throw(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
    ) -> Completion {
        self.set_with_throw(id, key, value, true)
    }

    /// `DeletePropertyOrThrow`: a delete a built-in performs must REPORT a refusal.
    ///
    /// `pop`, `shift` and `splice` removed elements with the raw own-property delete and dropped
    /// its answer, so a non-configurable element was silently left in place while `length` moved
    /// past it.
    pub(crate) fn delete_property_or_throw(
        &mut self,
        id: ObjectId,
        key: &PropertyKey,
    ) -> Completion {
        match self.delete_own_property(id, key) {
            Ok(true) => return Completion::Normal(JsValue::Undefined),
            Ok(false) => {}
            Err(abrupt) => return abrupt,
        }
        let message = format!("cannot delete `{}`: it is not configurable", key.to_display());
        self.type_error(&message)
    }

    /// `[[Delete]]`: whether the property is gone afterwards.
    ///
    /// `delete ta[0]` IS **FALSE** FOR A LIVE INDEX AND **TRUE** FOR EVERY OTHER NUMERIC KEY --
    /// the one place in the language where deleting something that is not there succeeds and
    /// deleting something that is there fails. Reaching the ordinary store instead reported `true`
    /// for both, so a program could believe it had removed an element that is still readable.
    pub(crate) fn delete_own_property(
        &mut self,
        id: ObjectId,
        key: &PropertyKey,
    ) -> Result<bool, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::delete(self, id, key);
        }
        if let Some(view) = self.typed_array_view(id) {
            if let Some(index) = crate::typed_array::canonical_numeric_index(key) {
                return Ok(matches!(
                    crate::typed_array::get_element(self, &view, index),
                    JsValue::Undefined
                ));
            }
        }
        let removed = self.object_mut(id).delete_own(key);
        if removed {
            self.unmap_parameter(id, key);
        }
        Ok(removed)
    }

    fn set_with_throw(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
        throw: bool,
    ) -> Completion {
        match self.ordinary_set(id, key.clone(), value, JsValue::Object(id)) {
            Ok(None) => Completion::Normal(JsValue::Undefined),
            Ok(Some(why)) => self.refuse_write(&key, why, throw),
            Err(abrupt) => abrupt,
        }
    }

    /// `OrdinarySet`: the write, which asks the PROTOTYPE CHAIN what the rule is and then lands on
    /// the RECEIVER.
    ///
    /// `Ok(None)` means the write happened. `Ok(Some(reason))` is a REFUSAL -- the standard's
    /// `false` -- which a caller turns into silence or a TypeError according to its own `Throw`
    /// argument. `Err` is a genuinely abrupt completion: a setter that threw, an invalid array
    /// length, a coercion that failed. **The two are not the same answer** and collapsing them was
    /// what made a refused write indistinguishable from a successful one.
    ///
    /// # THE OBJECT THAT DECIDES AND THE OBJECT THAT IS WRITTEN ARE DIFFERENT OBJECTS
    ///
    /// The chain is searched for the property that governs the write; the write itself is made on
    /// `receiver`. For a plain assignment the two are the same object and the distinction is
    /// invisible -- which is why an engine can go a long way without it. It becomes visible at
    /// `Reflect.set(target, key, value, receiver)` and at `super.x = v`, where the standard sends
    /// the rule to one object and the property to another.
    ///
    /// AND AN INHERITED NON-WRITABLE DATA PROPERTY REFUSES THE WRITE. This engine required the
    /// property to be OWN before refusing, so `Object.create(Object.freeze({y: 1})).y = 2` created
    /// an own `y` and reported success -- in strict mode too, where the standard throws. A
    /// non-writable property on a prototype is how a program makes a default permanent for every
    /// object that inherits it, and an engine that lets an instance shadow it has silently removed
    /// the guarantee rather than failing to provide one.
    pub(crate) fn ordinary_set(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
        receiver: JsValue,
    ) -> Result<Option<&'static str>, Completion> {
        let mut holder = Some(id);
        let governing = loop {
            let Some(current) = holder else { break None };
            if self.object(current).proxy.is_some() {
                return crate::proxy::set(self, current, key, value, receiver);
            }
            if let Some(view) = self.typed_array_view(current) {
                if let Some(index) = crate::typed_array::canonical_numeric_index(&key) {
                    if matches!(receiver, JsValue::Object(target) if target == current) {
                        return match crate::typed_array::set_element(self, &view, index, &value) {
                            Ok(()) => Ok(None),
                            Err(abrupt) => Err(abrupt),
                        };
                    }
                    let live = !matches!(
                        crate::typed_array::get_element(self, &view, index),
                        JsValue::Undefined
                    );
                    if !live {
                        return Ok(None);
                    }
                }
            }
            if matches!(receiver, JsValue::Object(target) if target == current) {
                if let Some((aliased, name)) = self.parameter_alias(current, &key) {
                    self.write_parameter_alias(&aliased, &name, value.clone());
                }
            }
            if let Some(property) = self.own_property(current, &key)? {
                break Some(property);
            }
            holder = self.object(current).prototype;
        };

        match governing.as_ref().map(|property| &property.kind) {
            Some(PropertyKind::Accessor { set, .. }) => {
                return match *set {
                    Some(setter) => {
                        match self.call_value(&JsValue::Object(setter), receiver, vec![value]) {
                            Completion::Normal(_) => Ok(None),
                            abrupt => Err(abrupt),
                        }
                    }
                    None => Ok(Some("it has no setter")),
                };
            }
            Some(PropertyKind::Data { writable: false, .. }) => {
                return Ok(Some("it is not writable"))
            }
            Some(PropertyKind::Data { .. }) | None => {}
        }

        let JsValue::Object(target) = receiver else {
            return Ok(Some("the receiver is not an object"));
        };
        match self.own_property(target, &key)? {
            Some(existing) if existing.is_accessor() => {
                Ok(Some("the receiver already has an accessor there"))
            }
            Some(existing) if !existing.is_writable() => Ok(Some("it is not writable")),
            existing => self.define_value_on(target, key, value, existing.is_some()),
        }
    }

    /// The value-only `[[DefineOwnProperty]]` a `[[Set]]` finishes with, over an ordinary or
    /// array-exotic object.
    ///
    /// **THE RECEIVER MAY BE A PROXY EVEN WHEN THE OBJECT THAT GOVERNED THE WRITE WAS NOT**, and
    /// this wrote straight into its property store. `new Proxy(t, {}).x = 1` reaches here with the
    /// PROXY as the receiver -- 10.5.9 forwards to `target.[[Set]](P, V, Receiver)` and passes the
    /// proxy on, which is what makes the write land on `t` through the proxy's own
    /// `[[DefineOwnProperty]]`. Writing the slot instead created a property ON THE PROXY: the write
    /// reported success, `p.x` read it back from a store nothing else consults, and the target never
    /// changed. A transparent proxy that quietly stops forwarding writes is precisely the plausible
    /// wrong answer this profile refuses.
    fn define_value_on(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
        redefines: bool,
    ) -> Result<Option<&'static str>, Completion> {
        if self.object(id).proxy.is_some() {
            let mut descriptor = crate::builtins::RawDescriptor::empty();
            descriptor.present.value = true;
            descriptor.value = value;
            if !redefines {
                descriptor.present.writable = true;
                descriptor.writable = true;
                descriptor.present.enumerable = true;
                descriptor.enumerable = true;
                descriptor.present.configurable = true;
                descriptor.configurable = true;
            }
            return match crate::proxy::define_own_property(self, id, key, &descriptor)? {
                None => Ok(None),
                Some(_) => Ok(Some("the proxy's `defineProperty` trap refused")),
            };
        }
        if self.object(id).own(&key).is_none() && !self.object(id).extensible {
            return Ok(Some("the object is not extensible"));
        }
        if self.object(id).is_array {
            if let Some(index) = key.as_array_index() {
                let length_key = PropertyKey::from_str("length");
                let refuses = match self.object(id).own(&length_key) {
                    Some(length) if !length.is_writable() => match length.data_value() {
                        Some(JsValue::Number(current)) => f64::from(index) >= *current,
                        _ => true,
                    },
                    _ => false,
                };
                if refuses {
                    return Ok(Some("the array's `length` is not writable"));
                }
            }
        }
        let mut value = value;
        if self.object(id).is_array && key == PropertyKey::from_str("length") {
            value = JsValue::Number(self.canonical_array_length(&value)?);
        }
        Ok(self.set_data_property(id, key, value))
    }

    /// `ArraySetLength` steps 3-5: the value a `length` write is allowed to take.
    ///
    /// # IT IS **TWO** CONVERSIONS OF THE SAME VALUE, AND A RANGE CHECK IS NOT A SUBSTITUTE
    ///
    /// `ToUint32(V)` and then `ToNumber(V)`, compared with SameValueZero. One `ToNumber` plus
    /// `0 ..= 2^32-1 and integral` accepts and rejects exactly the same NUMBERS -- which is why it
    /// stood -- and differs on everything observable about getting there:
    ///
    /// - `a.length = {valueOf(){ calls++; return 1 }}` counts **two** calls, not one.
    /// - The value is asked twice, so a `valueOf` that answers differently each time is a
    ///   RangeError rather than being silently accepted on whichever answer arrived.
    ///
    /// Both conversions run BEFORE the writability of `length` is consulted, so a frozen `length`
    /// still runs the program's `valueOf` -- twice -- and then refuses.
    pub(crate) fn canonical_array_length(&mut self, value: &JsValue) -> Result<f64, Completion> {
        let unsigned = f64::from(ops::to_uint32(self.to_number_value(value)?));
        let number = self.to_number_value(value)?;
        if !ops::same_value_zero(&JsValue::Number(unsigned), &JsValue::Number(number)) {
            return Err(self.throw("RangeError", "invalid array length"));
        }
        Ok(unsigned)
    }

    /// What a write that cannot happen does: nothing, or a TypeError, as `throw` says.
    ///
    /// **THIS IS THE LARGEST BEHAVIOURAL DIFFERENCE BETWEEN THE TWO MODES.** An engine carrying
    /// only the silent half ACCEPTS PROGRAMS THE STANDARD REJECTS, which is not a missing feature
    /// but a wrong answer: the write appears to succeed and the next read disagrees with it.
    ///
    /// AND `throw` IS A PARAMETER RATHER THAN `self.strict` FOR THE SECOND HALF OF THE SAME
    /// FINDING. It read the CALLER'S mode, which is right for an assignment and wrong for every
    /// write a built-in makes: the standard's `Set(O, P, V, Throw)` is called with `Throw` = **true**
    /// from `Array.prototype.push` and its neighbours whatever mode the calling program is in. With
    /// the flag welded to `self.strict`, `Object.freeze(a); a.push(1)` in sloppy code reported a new
    /// length and changed nothing. The mode question and the built-in question are two questions;
    /// answering both from one field made the sloppy half of the language silently lossy.
    fn refuse_write(&mut self, key: &PropertyKey, why: &str, throw: bool) -> Completion {
        if !throw {
            return Completion::Normal(JsValue::Undefined);
        }
        let message = format!("cannot assign to `{}` because {why}", key.to_display());
        self.type_error(&message)
    }

    /// IT ANSWERS A REFUSAL BECAUSE SHORTENING AN ARRAY CAN FAIL. `a.length = 0` where an element
    /// below is non-configurable stops at that element, leaves `length` at index + 1, and **returns
    /// false** -- so the assignment is silent in sloppy code and a TypeError in strict. Dropping
    /// that answer here with `let _ = self.shorten_array(...)` leaves `define_array_length` -- the
    /// same rule, reached from `Object.defineProperty` -- as the only half that reports it: one
    /// rule, two implementations, and the case in one of them.
    fn set_data_property(
        &mut self,
        id: ObjectId,
        key: PropertyKey,
        value: JsValue,
    ) -> Option<&'static str> {
        if let Some(existing) = self.object(id).own(&key) {
            if !existing.is_writable() {
                return None;
            }
            let previous_length = if self.object(id).is_array && key == PropertyKey::from_str("length")
            {
                match existing.data_value() {
                    Some(JsValue::Number(length)) => Some(*length),
                    _ => Some(f64::INFINITY),
                }
            } else {
                None
            };
            let property = Property { kind: PropertyKind::Data { value: value.clone(), writable: true }, ..existing.clone() };
            self.object_mut(id).set_own(key.clone(), property);
            if let (Some(previous), JsValue::Number(length)) = (previous_length, &value) {
                if *length < previous && self.shorten_array(id, *length).is_err() {
                    return Some("an element below the new length is not configurable");
                }
            }
            return None;
        }
        if !self.object(id).extensible {
            return None;
        }
        let is_array = self.object(id).is_array;
        let index = key.as_array_index();
        self.object_mut(id).set_own(key, Property::data(value));
        if is_array {
            if let Some(index) = index {
                self.grow_array_length_past(id, index);
            }
        }
        None
    }

    /// An Array's `length` tracks its largest index: step 2.j of the Array exotic
    /// `[[DefineOwnProperty]]`.
    ///
    /// IT IS SHARED BY ASSIGNMENT AND BY `Object.defineProperty`, AND REACHING IT FROM ASSIGNMENT
    /// ALONE IS NOT ENOUGH. If `a[2] = 1` grows the length and `Object.defineProperty(a, "2",
    /// {...})` does not, an array with an element at index 2 reports `length === 0` -- and every
    /// method that walks `0 .. length` then iterates zero times and returns the answer for an EMPTY
    /// array. `every` says true, `forEach` calls nothing, `indexOf` says -1: all plausible, none
    /// wrong-looking, and one branch away in a function nobody suspects because assignment works.
    ///
    /// THE EXISTING ATTRIBUTES ARE KEPT, NOT REASSERTED. Writing `writable: true` here every time
    /// makes `Object.defineProperty(a, 'length', {writable: false})` survive exactly until the next
    /// element is added -- the bookkeeping quietly undoing the program's own descriptor, whose only
    /// visible symptom is a `push` that should have thrown and did not.
    pub(crate) fn grow_array_length_past(&mut self, id: ObjectId, index: u32) {
        let length_key = PropertyKey::from_str("length");
        let current =
            match self.object(id).own(&length_key).and_then(|property| property.data_value().cloned())
            {
                Some(JsValue::Number(length)) => length,
                _ => 0.0,
            };
        if f64::from(index) < current {
            return;
        }
        let (writable, enumerable, configurable) = match self.object(id).own(&length_key) {
            Some(existing) => (existing.is_writable(), existing.enumerable, existing.configurable),
            None => (true, false, false),
        };
        self.object_mut(id).set_own(
            length_key,
            Property {
                kind: PropertyKind::Data {
                    value: JsValue::Number(f64::from(index) + 1.0),
                    writable,
                },
                enumerable,
                configurable,
            },
        );
    }

    /// The property key a computed access uses: `ToPropertyKey`, which is `ToString`.
    ///
    /// WARNING: `a[1]` and `a["1"]` ARE THE SAME PROPERTY, because the index converts to a string
    /// first. An object model keyed on the value rather than on the converted key makes them
    /// different, and array indexing stops agreeing with itself.
    fn to_property_key(&mut self, value: &JsValue) -> Result<PropertyKey, Completion> {
        self.to_property_key_value(value)
    }

    /// Runs a program and returns its completion.
    ///
    /// # THIS COMPILES AND THEN WALKS THE ARTIFACT. THE TREE IS A TRANSIENT PRODUCT.
    ///
    /// `parse -> encode -> run`: **one representation, not two.** The AST exists
    /// long enough to be encoded and is then dropped, so the per-source-byte RAM cost a
    /// tree costs are not resident while the program runs -- and that holds for `eval` and the REPL
    /// too, not only for a precompiled device image.
    ///
    /// Until this line changed, `run_artifact` was reached ONLY by the differential test, so
    /// every conformance number published described the AST evaluator. A port that is
    /// complete but not on the path is a port nothing measures.
    pub fn run(&mut self, program: &Program) -> Completion {
        let bytes = crate::bytecode::encode(program, &crate::bytecode::Options::minimal());
        self.run_artifact(Rc::from(bytes))
    }


    /// The value of a global binding, or `None` if the program never bound that name.
    ///
    /// **AN EMBEDDER NEEDS THIS AND THE ALTERNATIVE PULLS IN THE PARSER.** The obvious way to ask
    /// a finished program for a result is `eval_source("answer")` -- which links the lexer, the
    /// parser and the early-error passes into an image whose entire purpose is to ship none of
    /// them. A device reading one number back would pay ~40 KB of flash for the question.
    ///
    /// # THE OBJECT FIRST, THEN THE BINDING, BECAUSE THE GLOBAL RECORD IS TWO RECORDS
    ///
    /// A top-level `var`, a function declaration and an assignment to an undeclared name are all
    /// PROPERTIES of the global object -- so a read that consults only the scope map answers
    /// `undefined` for the very results an embedder asks about. `let`, `const` and `class` are the
    /// other half: bindings with deliberately no property, which is why the map is still consulted
    /// rather than replaced. Reading the object is also what makes `this.answer = 42` visible here.
    ///
    /// AN OWN **DATA** PROPERTY ONLY. This takes `&self` and must stay that way: running an
    /// accessor is user code, and an embedder reading a finished program's result should not be
    /// able to re-enter it. A global that is an accessor falls through to the binding.
    #[must_use]
    pub fn global(&self, name: &str) -> Option<JsValue> {
        if let Some(JsValue::Object(global)) = self.global_value("globalThis") {
            if let Some(property) = self.object(global).own(&PropertyKey::from_str(name)) {
                if let PropertyKind::Data { value, .. } = &property.kind {
                    return Some(value.clone());
                }
            }
        }
        let binding = self.global.borrow().bindings.get(name).cloned()?;
        binding.initialized.then_some(binding.value)
    }

    /// What the realm is MADE OF, counted rather than estimated.
    ///
    /// # WHY A CENSUS RATHER THAN AN ESTIMATE
    ///
    /// On a part whose RAM the realm dominates, the realm IS the fixed cost -- and a fixed cost
    /// nothing measures is one nobody can argue with. The program's own representation competes
    /// with it only while that representation is large per source byte. **Once it is not, the fixed
    /// term is the whole story** and the first question is where it goes.
    ///
    /// Counts, not bytes. The allocator gives the TOTAL honestly; dividing it up by structure
    /// size would be a model of the allocator rather than a measurement of it. A count paired with
    /// a measured total is two facts; a computed per-item byte figure would be one fact and one
    /// assumption wearing the same units.
    #[must_use]
    pub fn realm_census(&self) -> RealmCensus {
        let mut census = RealmCensus {
            objects: self.heap.len(),
            resident_objects: self.heap.resident_len(),
            promoted_objects: self.heap.promoted_len(),
            closures: self.closures.len(),
            natives: self.natives.len(),
            symbols: self.symbols.len(),
            bounds: self.bounds.len(),
            template_objects: self.template_objects.len(),
            programs: self.programs.len(),
            ..RealmCensus::default()
        };
        census.object_size = core::mem::size_of::<Object>();
        census.object_struct_bytes = self.heap.ram_objects() * census.object_size;
        census.object_table_bytes = self.heap.ram_table_bytes();
        census.native_table_bytes =
            self.natives.capacity() * core::mem::size_of::<NativeFunction>();
        census.native_name_bytes = self.natives.iter().map(|native| native.name.len()).sum();

        for object in self.heap.iter() {
            census.property_store_bytes += object.property_store_bytes();
            let keys = object.own_keys();
            census.properties += keys.len();
            if object.callable.is_some() {
                census.callable_objects += 1;
            }
            for key in &keys {
                census.property_key_bytes += key.to_display().len();
                if matches!(
                    object.own(key).map(|p| &p.kind),
                    Some(crate::object::PropertyKind::Accessor { .. })
                ) {
                    census.accessors += 1;
                }
            }
        }
        census
    }

    /// Parses, compiles, **drops the tree**, and runs.
    ///
    /// # THE `drop` IS THE ENTIRE POINT, AND WITHOUT IT THE TIER DELIVERS NOTHING
    ///
    /// `run` takes a `&Program`, so the CALLER decides how long the tree lives -- and every caller
    /// so far held it across the whole run. The artifact was resident *beside* a tree that was no
    /// longer read, which costs MORE RAM than not having a tier at all. Compiling and walking is
    /// only half the change; the other half is that nothing keeps the tree afterwards.
    ///
    /// Which is why this is a method rather than advice in a doc comment. "Remember to drop the
    /// parse" is a rule every future caller has to rediscover, and the one who forgets gets a
    /// working program and a silently doubled representation.
    /// # AN UNCAUGHT THROW COMES BACK AS A VALUE, AND `Debug` WILL NOT RENDER IT
    ///
    /// A program that throws answers `Ok(Completion::Throw(value))` -- not `Err` -- because a throw
    /// is a completion the caller may want to inspect rather than a failure of this call. The value
    /// is an Error OBJECT, so `Debug` prints its identity (`Object(ObjectId(468))`) and its ordinary
    /// string conversion is `[object]`: both discard the kind and the message that make one failure
    /// distinguishable from another.
    ///
    /// Render it with [`Interpreter::describe`], which answers `TypeError: ...`, or use
    /// [`Interpreter::eval_source`], which does that already and reports the throw as an `Err`.
    /// An embedder that reached for `Debug` and got an object id was looking in the right place and
    /// finding nothing that pointed here.
    pub fn run_source(&mut self, source: &str) -> Result<Completion, String> {
        let parsed = crate::parse_script(source);
        if parsed.has_errors() {
            let first = parsed.diagnostics.iter().find(|d| d.is_error()).expect("has_errors");
            return Err(format!("{:?}: {}", first.kind, first.message));
        }
        let bytes = crate::bytecode::encode(&parsed.program, &crate::bytecode::Options::minimal());
        drop(parsed);
        Ok(self.run_artifact(Rc::from(bytes)))
    }

    /// Convenience for tests and for the eventual seam: run and report the last value.
    pub fn eval_source(&mut self, source: &str) -> Result<JsValue, String> {
        match self.run_source(source)? {
            Completion::Normal(value) | Completion::Return(value) => Ok(value),
            Completion::Throw(value) => Err(format!("uncaught: {}", self.describe(&value))),
            other => Err(format!("unexpected completion: {other:?}")),
        }
    }

    /// The name of the constructor of a thrown value, which is what a `negative` test names.
    ///
    /// WARNING: IT IS `thrown.constructor.name`, NOT A RENDERING OF THE ERROR. A runner comparing
    /// against a message would match the wrong tests and miss the right ones -- the standard names
    /// the CONSTRUCTOR, which is why the object model has to be consulted rather than the text.
    #[must_use]
    pub fn constructor_name_of(&self, value: &JsValue) -> Option<String> {
        let JsValue::Object(id) = value else { return None };
        let constructor = self.peek_property(*id, &PropertyKey::from_str("constructor"));
        let JsValue::Object(constructor_id) = constructor else { return None };
        match self.peek_property(constructor_id, &PropertyKey::from_str("name")) {
            JsValue::String(name) => Some(name.to_lossy_string()),
            _ => None,
        }
    }

    /// A human-readable rendering of a value, for a diagnostic.
    #[must_use]
    pub fn describe(&self, value: &JsValue) -> String {
        match value {
            JsValue::Object(id) => {
                let name = match self.peek_property(*id, &PropertyKey::from_str("name")) {
                    JsValue::String(name) => name.to_lossy_string(),
                    _ => String::new(),
                };
                let message = match self.peek_property(*id, &PropertyKey::from_str("message")) {
                    JsValue::String(message) => message.to_lossy_string(),
                    _ => String::new(),
                };
                match (name.is_empty(), message.is_empty()) {
                    (true, true) => "[object]".to_string(),
                    (true, false) => message,
                    (false, true) => name,
                    (false, false) => format!("{name}: {message}"),
                }
            }
            other => self.display(other),
        }
    }

    fn display(&self, value: &JsValue) -> String {
        match ops::to_string(value) {
            Ok(text) => text.to_lossy_string(),
            Err(NeedsObjectProtocol) => "[object]".to_string(),
        }
    }






    /// The twin of [`Self::make_closure`] for a function that stays in the artifact.
    ///
    /// Deliberately beside the original rather than folded into it: the two differ in exactly one
    /// thing -- where the code lives -- and keeping them adjacent is what makes that visible. When
    /// the AST path is deleted, this one stays and the other goes.
    ///
    /// `length` arrives already computed rather than derived from a parameter list, because there
    /// is no parameter list to derive it from -- the parameters are in the artifact, beside the
    /// body, and are read at call time.
    pub(crate) fn push_artifact_closure(
        &mut self,
        length: u32,
        name: &str,
        code_at: u32,
        strict: bool,
        is_arrow: bool,
        is_method: bool,
        is_generator: bool,
        needs_arguments: bool,
        scope: &Scope,
    ) -> JsValue {
        let (captured_this, captured_this_cell, home_object, class_object, new_target) =
            if is_arrow {
                (
                    Some(self.current_this()),
                    self.derived_this.last().cloned().flatten(),
                    self.home_stack.last().copied().flatten(),
                    self.class_stack.last().copied().flatten(),
                    Some(self.current_new_target()),
                )
            } else {
                (None, None, None, None, None)
            };
        self.closures.push(Closure {
            code_at: Some(CodeRef { program: self.current_program, at: code_at }),
            needs_arguments,
            scope: Rc::clone(scope),
            is_arrow,
            captured_this,
            captured_this_cell,
            is_method,
            is_generator,
            home_object,
            implicit_derived: false,
            class_object,
            class_kind: ClassKind::NotAClass,
            captured_new_target: new_target,
            strict: self.strict || strict,
        });
        let value = self.callable_object((self.closures.len() - 1) as u32);
        if let JsValue::Object(id) = value {
            self.set_function_metadata(id, name, length);
        }
        value
    }

    /// Turns a freshly built function into a **method**: the three things a method definition does
    /// that `function () {}` in the same position does not.
    ///
    /// A METHOD IS NOT A CONSTRUCTOR, AND THIS ENGINE WAS MAKING IT ONE. `{ m() {} }.m` and
    /// `class C { m() {} }` produce functions with **no `prototype` property**, and `new o.m()` is a
    /// TypeError. `callable_object` gives every non-arrow a `prototype` because that is right for
    /// `function`, so a method arrived here carrying one; `'prototype' in desc.get` came back true
    /// for every getter in the corpus, and `new`ing a method silently produced an object.
    ///
    /// AND ITS `name` WAS THE EMPTY STRING, on every method in the language. A method's function
    /// has no name of its own -- the name comes from the PROPERTY KEY, which the parser has and the
    /// function node does not. An accessor's name carries a **`get `/`set ` prefix**, so
    /// `Object.getOwnPropertyDescriptor(o, 'a').get.name` is `"get a"` and not `"a"`; the prefix is
    /// part of the name rather than a display convention, and the suite checks it directly.
    ///
    /// A SYMBOL-KEYED METHOD IS NAMED `[description]`, brackets included, and an anonymous symbol
    /// gives the empty string rather than `[]`.
    pub(crate) fn make_method(
        &mut self,
        function: ObjectId,
        home: ObjectId,
        key: &PropertyKey,
        kind: crate::ast::MethodKind,
    ) {
        if let Some(Callable::Closure(closure)) = self.object(function).callable {
            self.closures[closure as usize].home_object = Some(home);
        }
        let base = match key {
            PropertyKey::String(text) => text.clone(),
            PropertyKey::Symbol(id) => match self.symbol_description(*id) {
                Some(description) => {
                    let mut text = JsString::from("[");
                    text.extend_from(&description);
                    text.extend_from(&JsString::from("]"));
                    text
                }
                None => JsString::new(),
            },
        };
        let mut name = match kind {
            crate::ast::MethodKind::Get => JsString::from("get "),
            crate::ast::MethodKind::Set => JsString::from("set "),
            _ => JsString::new(),
        };
        name.extend_from(&base);
        self.object_mut(function).set_own(
            PropertyKey::from_str("name"),
            Property {
                kind: PropertyKind::Data { value: JsValue::String(name), writable: false },
                enumerable: false,
                configurable: true,
            },
        );
    }

    /// Wraps a closure index in a heap object, so a function IS an object -- which is what makes
    /// `f.call`, `f.length` and a function used as a value work at all.
    fn callable_object(&mut self, closure: u32) -> JsValue {
        let kind = self.closures[closure as usize].prototype_kind();
        let parent = match kind {
            PrototypeKind::Generator => self.intrinsics.generator_function_prototype,
            PrototypeKind::None | PrototypeKind::Constructor => self.intrinsics.function_prototype,
        };
        let mut object = Object::new(Some(parent));
        object.callable = Some(Callable::Closure(closure));
        let id = self.allocate(object);
        if kind != PrototypeKind::None {
            let parent = match kind {
                PrototypeKind::Generator => self.intrinsics.generator_prototype,
                PrototypeKind::Constructor | PrototypeKind::None => self.intrinsics.object_prototype,
            };
            let prototype = self.allocate(Object::new(Some(parent)));
            if kind == PrototypeKind::Constructor {
                self.object_mut(prototype).set_own(
                    PropertyKey::from_str("constructor"),
                    Property::builtin(JsValue::Object(id)),
                );
            }
            self.object_mut(id).set_own(
                PropertyKey::from_str("prototype"),
                Property { kind: PropertyKind::Data { value: JsValue::Object(prototype), writable: true }, enumerable: false, configurable: false },
            );
        }
        JsValue::Object(id)
    }

    /// `ResolveBinding` followed by `GetBindingValue`: what a bare name reads.
    ///
    /// IT TAKES `&mut self` AND CAN FAIL, because the last link of the chain is an OBJECT. A
    /// global that is an accessor runs its getter here, and that getter can throw -- see
    /// [`Self::global_object_binding`].
    fn lookup(&mut self, scope: &Scope, name: &str) -> Result<JsValue, Completion> {
        Ok(self.lookup_reference(scope, name)?.0)
    }

    /// `ResolveBinding` alone: WHICH record the name is in, without reading it.
    ///
    /// # ONE WALK, AND EVERY QUESTION ABOUT A BARE NAME IS ANSWERED FROM IT
    ///
    /// A second walk RUNS THE TRAPS AGAIN: the corpus pins the exact sequence a proxy binding object
    /// sees for `with (proxy) { Object(); }` at four entries -- `has` and `get` from `HasBinding`,
    /// then `has` and `get` from `GetBindingValue` -- and re-resolving makes it six or seven. So the
    /// three things a caller can want (does it resolve, what is its value, where does a write go)
    /// all come from this one answer. See [`ReferenceBase`] for why the *write* in particular cannot
    /// walk again.
    ///
    /// It reports [`ReferenceBase::Unresolvable`] rather than failing: that is a legal answer for
    /// `typeof x` and for a sloppy `x = 1`, and only strict code turns it into a ReferenceError.
    fn resolve_binding(&mut self, scope: &Scope, name: &str) -> Result<Reference, Completion> {
        let strict = self.strict;
        let base = self.resolve_binding_base(scope, name)?;
        Ok(Reference { base, strict })
    }

    fn resolve_binding_base(
        &mut self,
        scope: &Scope,
        name: &str,
    ) -> Result<ReferenceBase, Completion> {
        let mut current = Some(Rc::clone(scope));
        while let Some(environment) = current {
            let borrowed = environment.borrow();
            let found = borrowed.bindings.get(name).map(|binding| binding.lexical);
            if let Some(lexical) = found {
                drop(borrowed);
                if !lexical && Rc::ptr_eq(&environment, &self.global) {
                    return Ok(ReferenceBase::Global);
                }
                return Ok(ReferenceBase::Declarative(environment));
            }
            let with_object = borrowed.with_object;
            let parent = borrowed.parent.clone();
            drop(borrowed);
            if let Some(object) = with_object {
                if self.with_has_binding(object, name)? {
                    return Ok(ReferenceBase::Object(object));
                }
            }
            current = parent;
        }
        if self.global_object_has_binding(name)? {
            return Ok(ReferenceBase::Global);
        }
        Ok(ReferenceBase::Unresolvable)
    }

    /// `GetBindingValue` on an already-resolved base: the value, or the ReferenceError the standard
    /// raises in place of one.
    ///
    /// # THE OPERATION HAS **TWO** REFUSALS AND THE SECOND IS THE ONE A CALLER CAN OMIT
    ///
    /// A bare name that resolves to nothing is `ReferenceError: x is not defined`, and one that
    /// resolves to an UNINITIALIZED binding is `ReferenceError: cannot access x before
    /// initialization`. Both are this function's to raise. Its five callers are the identifier
    /// read, the callee read, `+=`, `&&=` and `++`, and handing a `Binding` back whole lets any of
    /// them take `binding.value` and read a dead zone as `undefined`:
    ///
    /// ```text
    /// (function () { let a = (a &&= 0); })()   // ReferenceError, not undefined
    /// (function () { let a = (a += 1); })()    // ReferenceError, not NaN
    /// (function () { let a = (a++); })()       // ReferenceError, not NaN
    /// ```
    ///
    /// `&&=` IS THE ONE THAT SHOWS IT: `||=` and `??=` on an `undefined` read go on to WRITE, and
    /// the write refuses a dead zone on its own -- so two of the three logical operators look
    /// correct while sharing the defect. `&&=` short-circuits on a falsy read and never reaches the
    /// write.
    ///
    /// So the declarative arm returns a VALUE rather than a `Binding`, which is what makes the
    /// omission unspellable: a caller that wants only the value cannot get one without deciding
    /// something about `initialized`.
    fn binding_value_of(
        &mut self,
        reference: &Reference,
        name: &str,
    ) -> Result<JsValue, Completion> {
        match &reference.base {
            ReferenceBase::Declarative(environment) => {
                let binding = environment.borrow().bindings.get(name).cloned();
                let Some(binding) = binding else { return Err(self.not_defined(name)) };
                if !binding.initialized {
                    return Err(self.throw(
                        "ReferenceError",
                        &crate::format!("cannot access `{name}` before initialization"),
                    ));
                }
                Ok(binding.value)
            }
            ReferenceBase::Object(object) => {
                self.object_binding_value(*object, name, reference.strict)
            }
            ReferenceBase::Global => self.global_object_binding_value(name, reference.strict),
            ReferenceBase::Unresolvable => Err(self.not_defined(name)),
        }
    }

    /// The other half of [`Self::binding_value_of`]'s refusal, named so the two cannot be worded
    /// differently at different call sites.
    fn not_defined(&mut self, name: &str) -> Completion {
        self.throw("ReferenceError", &crate::format!("{name} is not defined"))
    }

    /// [`Self::lookup`] plus the reference the name resolved to.
    ///
    /// The base is what a call through a bare name takes its `this` from -- `with (o) { m() }`
    /// calls `m` with `this === o` -- and what a write to that name must go back through.
    fn lookup_reference(
        &mut self,
        scope: &Scope,
        name: &str,
    ) -> Result<(JsValue, Reference), Completion> {
        let reference = self.resolve_binding(scope, name)?;
        let value = self.binding_value_of(&reference, name)?;
        Ok((value, reference))
    }

    /// `HasBinding` on a `with` statement's object environment record: `HasProperty`, then the
    /// `@@unscopables` veto.
    ///
    /// # THE ORDER IS THE RULE, NOT AN OPTIMIZATION
    ///
    /// `@@unscopables` is read ONLY when `HasProperty` already said yes -- the corpus asserts that a
    /// getter on `Symbol.unscopables` is never called for a name the object does not have. Reading
    /// the block first and filtering afterwards would run user code on every unresolved name in
    /// every `with` body, which is a side effect the program never asked for.
    ///
    /// `unscopables` blocks a name by being an OBJECT whose property for that name is truthy.
    /// A non-object -- `42`, `null`, `undefined` -- blocks nothing, and neither does a falsy value.
    fn with_has_binding(&mut self, object: ObjectId, name: &str) -> Result<bool, Completion> {
        let key = PropertyKey::from_str(name);
        if !self.has_property(object, &key)? {
            return Ok(false);
        }
        let unscopables_key = PropertyKey::Symbol(self.well_known_symbol("unscopables"));
        let unscopables = match self.get_property(object, &unscopables_key) {
            Completion::Normal(value) => value,
            abrupt => return Err(abrupt),
        };
        let JsValue::Object(block) = unscopables else { return Ok(true) };
        let blocked = match self.get_property(block, &key) {
            Completion::Normal(value) => value,
            abrupt => return Err(abrupt),
        };
        Ok(!crate::abstract_ops::to_boolean(&blocked))
    }

    /// `GetBindingValue` on an object environment record.
    ///
    /// **IT ASKS `HasProperty` AGAIN, AND THE SECOND ASK IS NOT REDUNDANT.** `HasBinding` ran
    /// first and may have run user code -- an `@@unscopables` getter -- which is free to DELETE the
    /// property it was consulted about. The corpus has that exact program, and the answer is
    /// `undefined` rather than a fall-through to the enclosing scope: the reference is already bound
    /// to this record, so a name that has since vanished reads as `undefined` in sloppy code rather
    /// than resolving somewhere else.
    ///
    /// **AND THE STRICT ARM OF STEP 3 IS REACHABLE, WHICH "`with` IS SLOPPY-ONLY" WOULD DENY.** That
    /// is true of the STATEMENT and not of the code that runs inside the record it creates. A strict function declared in a `with` body closes over
    /// the object environment record and reads through it with `S` true. See [`Reference`].
    fn object_binding_value(
        &mut self,
        object: ObjectId,
        name: &str,
        strict: bool,
    ) -> Result<JsValue, Completion> {
        let key = PropertyKey::from_str(name);
        if !self.has_property(object, &key)? {
            if strict {
                let message = crate::format!("{name} is not defined");
                return Err(self.throw("ReferenceError", &message));
            }
            return Ok(JsValue::Undefined);
        }
        match self.get_property(object, &key) {
            Completion::Normal(value) => Ok(value),
            abrupt => Err(abrupt),
        }
    }

    /// The last link of the scope chain: an OWN property of the global object.
    ///
    /// # A PROPERTY PUT ON `globalThis` IS A GLOBAL, AND THIS ENGINE HAD ONLY ONE DIRECTION
    ///
    /// The realm's globals were mirrored onto the global object, so `globalThis.Array` worked. The
    /// way back did not: `globalThis.hello = f` created a property and no binding, so `hello()`
    /// was a ReferenceError while `globalThis.hello()` worked -- the same realm answering two ways
    /// again, in the direction a PROGRAM can reach rather than the one the realm builder does.
    ///
    /// It is not a corner: the standard's own SpiderMonkey shell files are written
    /// `(function(global) { global.testLenientAndStrict = ...; })(this)`, so seventy runs met
    /// `testLenientAndStrict is not defined` after the file that defines it had run successfully.
    ///
    /// # IT IS AN OBJECT ENVIRONMENT RECORD, NOT A PEEK AT OWN PROPERTIES
    ///
    /// A global environment record is a declarative record (`let`, `const`, `class`) laid over an
    /// **object** record whose binding object is the global object -- so `HasBinding` is
    /// `HasProperty` and `GetBindingValue` is `[[Get]]`, both of which WALK THE PROTOTYPE CHAIN and
    /// RUN CODE. Reading own properties only, and never calling a getter, is three wrong answers
    /// rather than one narrowing:
    ///
    /// 1. **A global that is an ACCESSOR read as `undefined`** from a bare name while reading
    ///    correctly through `globalThis`. The same realm, answering two ways, in the direction a
    ///    program reaches.
    /// 2. **A write to such a name never ran the setter.**
    /// 3. **An inherited name did not resolve at all.** The comment that stood here said resolving
    ///    `toString` and `hasOwnProperty` as bare names would "turn a corpus full of 'is not
    ///    defined' expectations into a corpus full of surprises" -- and that is simply what a
    ///    conforming engine does, verified against one. The names it makes resolvable are exactly
    ///    the members of the global object's prototype, and a name that is on NOTHING in the chain
    ///    is still a ReferenceError.
    ///
    /// The one thing that stays narrower: an unresolvable name must remain unresolvable, and that
    /// question is [`Self::global_object_has_binding`]'s -- reached during `ResolveBinding`, before
    /// this runs at all. The difference is the whole of `typeof x` versus `x`.
    ///
    /// # IT ASKS `HasProperty` A SECOND TIME, FOR THE SAME REASON THE OBJECT RECORD DOES
    ///
    /// `HasBinding` asked during `ResolveBinding` and this is a separate step, with the program's
    /// own code free to run in between -- a getter reached through the compound-assignment shape can
    /// delete the very global it was asked for. Step 3 of 9.1.1.2.6 says what happens then, and it
    /// is not a fall-through to anywhere: the reference is already bound to this record, so sloppy
    /// code reads `undefined` and strict code gets a ReferenceError. Reusing the earlier answer
    /// would call `[[Get]]` on a property that no longer exists.
    fn global_object_binding_value(
        &mut self,
        name: &str,
        strict: bool,
    ) -> Result<JsValue, Completion> {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else {
            return Ok(JsValue::Undefined);
        };
        let key = PropertyKey::from_str(name);
        if !self.has_property(global, &key)? {
            if strict {
                let message = crate::format!("{name} is not defined");
                return Err(self.throw("ReferenceError", &message));
            }
            return Ok(JsValue::Undefined);
        }
        match self.get_property(global, &key) {
            Completion::Normal(value) => Ok(value),
            abrupt => Err(abrupt),
        }
    }

    /// `ResolveBinding` without `GetBindingValue`: whether the name resolves AT ALL.
    ///
    /// IT IS A SEPARATE QUESTION FROM [`Self::lookup`] AND NOT AN OPTIMIZATION OF IT. The two
    /// callers -- `typeof x` and a strict assignment's unresolvable-target check -- must not READ
    /// the binding: `typeof` would run an accessor global's getter twice, and `x = 1` would run one
    /// on a write that never reads. Asking with a read and discarding it is the same answer through
    /// an observable side effect.
    pub(crate) fn resolves(&mut self, scope: &Scope, name: &str) -> Result<bool, Completion> {
        let base = self.resolve_binding_base(scope, name)?;
        Ok(!matches!(base, ReferenceBase::Unresolvable))
    }

    /// `HasBinding` alone -- whether the name resolves, without reading it.
    ///
    /// `typeof x` MUST NOT RUN A GETTER TWICE AND MUST NOT RUN ONE IT DOES NOT NEED... but it
    /// DOES run one: `typeof` on a resolvable name is `GetValue` followed by the type of the
    /// result, so an accessor global's getter runs and `typeof` reports the type of what it
    /// returned. What this exists for is the OTHER half -- deciding whether the name resolves at
    /// all -- which `typeof` needs without a value and which must not throw for an absent name.
    fn global_object_has_binding(&mut self, name: &str) -> Result<bool, Completion> {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else {
            return Ok(false);
        };
        self.has_property(global, &PropertyKey::from_str(name))
    }

    /// Fills in a binding the hoister created UNINITIALIZED: what a `class` declaration does when
    /// its statement is finally reached.
    ///
    /// IT IS NOT AN ASSIGNMENT AND MUST NOT GO THROUGH ONE. `InitializeBinding` is the operation
    /// that ENDS a temporal dead zone; an assignment is the operation that is refused during it, so
    /// sending a class declaration through `assign_existing` makes the declaration trip the rule it
    /// exists to satisfy.
    ///
    /// NO `with` ARM, and the silence is the rule rather than an omission: `InitializeBinding` is
    /// only ever called on a binding a HOISTER created, and a `with` environment has none -- a
    /// `let`, `const` or `class` written inside a `with` body is in the body's own block record,
    /// which is a declarative one nested INSIDE this. The empty `bindings` map makes the walk pass
    /// straight through, which is the correct behaviour rather than an accident of shape.
    pub(crate) fn initialize_existing(&mut self, scope: &Scope, name: &str, value: JsValue) {
        let mut current = Some(Rc::clone(scope));
        while let Some(environment) = current {
            {
                let mut borrowed = environment.borrow_mut();
                if let Some(binding) = borrowed.bindings.get_mut(name) {
                    binding.value = value;
                    binding.initialized = true;
                    return;
                }
            }
            let parent = environment.borrow().parent.clone();
            current = parent;
        }
    }

    /// `ResolveBinding` followed immediately by `PutValue`: what a write to a bare name does when
    /// nothing runs in between.
    ///
    /// **THE PATHS WHERE SOMETHING *DOES* RUN IN BETWEEN MUST NOT USE THIS.** A compound
    /// assignment, an increment and a logical assignment all read the binding, run user code, and
    /// then write -- so they resolve once with [`Self::resolve_binding_base`] and write through
    /// that answer with [`Self::put_value`]. See [`ReferenceBase`] for what the second walk gets
    /// wrong.
    fn assign_existing(&mut self, scope: &Scope, name: &str, value: JsValue) -> Result<(), Completion> {
        let reference = self.resolve_binding(scope, name)?;
        self.put_value(&reference, name, value)
    }

    /// `PutValue` steps 2 and 6: the write goes through the record the reference was resolved
    /// against, whatever has happened to the chain since.
    fn put_value(
        &mut self,
        reference: &Reference,
        name: &str,
        value: JsValue,
    ) -> Result<(), Completion> {
        let strict = reference.strict;
        match &reference.base {
            ReferenceBase::Declarative(environment) => {
                let environment = Rc::clone(environment);
                self.declarative_set_mutable_binding(&environment, name, value, strict)
            }
            ReferenceBase::Object(object) => {
                self.object_set_mutable_binding(*object, name, value, strict)
            }
            ReferenceBase::Global => self.global_set_mutable_binding(name, value, strict),
            ReferenceBase::Unresolvable => self.unresolvable_put_value(name, value, strict),
        }
    }

    /// `InstanceofOperator(V, target)`: the `instanceof` operator, which is a HOOK before it is a
    /// prototype-chain walk.
    ///
    /// # `@@hasInstance` IS ASKED FIRST, AND THIS ENGINE NEVER ASKED
    ///
    /// The chain walk is `OrdinaryHasInstance`, and it is what
    /// `Function.prototype[@@hasInstance]` does -- **not** what the operator does. The operator
    /// looks the method up on the right operand and calls it, so a class that defines one decides
    /// for itself:
    ///
    /// ```text
    /// class C { static [Symbol.hasInstance](v) { return v === 'yes'; } }
    /// 'yes' instanceof C        // true
    /// ```
    ///
    /// This answered **`false`**. Not a refusal, not a TypeError -- the ordinary walk, run against a
    /// class the value is genuinely not an instance of, producing a perfectly plausible answer to a
    /// question the program had overridden. And a plain object carrying a `@@hasInstance` was worse
    /// still: "the right operand of `instanceof` must be callable", for an operand the standard
    /// never requires to be callable once the method is there.
    ///
    /// THE CALLABILITY CHECK MOVED, it was not removed. Step 4 still refuses a non-callable
    /// right operand -- but only AFTER step 2 has found no method, which is the whole difference.
    fn instanceof_operator(&mut self, value: &JsValue, target: &JsValue) -> Completion {
        let JsValue::Object(constructor) = target else {
            return self.type_error("the right operand of `instanceof` must be an object");
        };
        let constructor = *constructor;
        let key = PropertyKey::Symbol(self.well_known_symbol("hasInstance"));
        let handler = match crate::iterator::get_method(self, target, &key) {
            Ok(handler) => handler,
            Err(abrupt) => return abrupt,
        };
        if let Some(handler) = handler {
            let answer =
                normal!(self.call_value(&handler, target.clone(), vec![value.clone()]));
            return Completion::Normal(JsValue::Boolean(crate::abstract_ops::to_boolean(&answer)));
        }
        if self.object(constructor).callable.is_none() {
            return self.type_error("the right operand of `instanceof` must be callable");
        }
        match self.ordinary_has_instance(constructor, value) {
            Ok(answer) => Completion::Normal(JsValue::Boolean(answer)),
            Err(abrupt) => abrupt,
        }
    }

    /// `OrdinaryHasInstance` reached from `Function.prototype[@@hasInstance]`, which is a native
    /// function and so cannot name a private method of this type.
    pub(crate) fn ordinary_has_instance_for_builtin(
        &mut self,
        constructor: ObjectId,
        value: JsValue,
    ) -> Result<bool, Completion> {
        self.ordinary_has_instance(constructor, &value)
    }

    /// `OrdinaryHasInstance(C, O)`: the prototype-chain walk `instanceof` falls back to.
    ///
    /// A BOUND FUNCTION DEFERS TO ITS TARGET AND DOES NOT ANSWER FOR ITSELF. `f.bind(null)` has
    /// no `prototype` property at all, so walking against it raised "the constructor's `prototype`
    /// is not an object" for `new f() instanceof f.bind(null)` -- which the standard answers
    /// **true**, by restarting the whole operator on `[[BoundTargetFunction]]`.
    ///
    /// It restarts `InstanceofOperator`, not this walk, so a bound function whose target carries
    /// a `@@hasInstance` reaches it.
    fn ordinary_has_instance(
        &mut self,
        constructor: ObjectId,
        value: &JsValue,
    ) -> Result<bool, Completion> {
        if self.object(constructor).callable.is_none() {
            return Ok(false);
        }
        if let Some(Callable::Bound(index)) = self.object(constructor).callable {
            let target = self.bounds[index as usize].target;
            return match self.instanceof_operator(value, &JsValue::Object(target)) {
                Completion::Normal(JsValue::Boolean(answer)) => Ok(answer),
                Completion::Normal(other) => Ok(crate::abstract_ops::to_boolean(&other)),
                abrupt => Err(abrupt),
            };
        }
        let JsValue::Object(mut current) = *value else { return Ok(false) };
        let prototype = match self.get_property(constructor, &PropertyKey::from_str("prototype")) {
            Completion::Normal(prototype) => prototype,
            abrupt => return Err(abrupt),
        };
        let JsValue::Object(prototype) = prototype else {
            return Err(self.type_error("the constructor's `prototype` is not an object"));
        };
        loop {
            match self.get_prototype_of(current)? {
                Some(next) if next == prototype => return Ok(true),
                Some(next) => current = next,
                None => return Ok(false),
            }
        }
    }

    /// `SetMutableBinding` on a declarative environment record (9.1.1.1.5).
    fn declarative_set_mutable_binding(
        &mut self,
        environment: &Scope,
        name: &str,
        value: JsValue,
        strict: bool,
    ) -> Result<(), Completion> {
        {
            let mut borrowed = environment.borrow_mut();
            let Some(binding) = borrowed.bindings.get_mut(name) else {
                drop(borrowed);
                if strict {
                    let message = crate::format!("{name} is not defined");
                    return Err(self.throw("ReferenceError", &message));
                }
                environment.borrow_mut().bindings.insert(
                    name.to_string(),
                    Binding::var(value),
                );
                return Ok(());
            };
            if !binding.initialized {
                drop(borrowed);
                let message =
                    format!("cannot assign to `{name}` before its declaration is reached");
                return Err(self.throw("ReferenceError", &message));
            }
            if binding.mutability == Mutability::Immutable && binding.initialized {
                return Err(self.type_error("assignment to a constant"));
            }
            if binding.mutability == Mutability::ImmutableSelfName {
                drop(borrowed);
                if strict {
                    return Err(self.type_error("assignment to a function's own name"));
                }
                return Ok(());
            }
            binding.value = value.clone();
            binding.initialized = true;
        }
        if Rc::ptr_eq(environment, &self.global) && !self.has_lexical_declaration(name) {
            self.update_on_global_object(name, value);
        }
        Ok(())
    }

    /// `SetMutableBinding` on the global environment record's OBJECT half (9.1.1.2.5 reached
    /// through 9.1.1.4.5).
    ///
    /// A NAME THAT IS ONLY A PROPERTY OF THE GLOBAL OBJECT is still a global, and assigning to
    /// it writes THERE rather than creating a second home for the same name.
    ///
    /// THROUGH `[[Set]]`, NOT BY EDITING THE PROPERTY. `SetMutableBinding` on an object
    /// record is `Set(bindings, N, V, S)`, so a global that is an ACCESSOR must run its setter
    /// and an inherited one must be found on the chain. `update_on_global_object` did neither:
    /// it looked for an OWN data property and silently did nothing when it found an accessor,
    /// so `acc = 7` on an accessor global was a write that reported success and changed nothing.
    fn global_set_mutable_binding(
        &mut self,
        name: &str,
        value: JsValue,
        strict: bool,
    ) -> Result<(), Completion> {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else {
            return Ok(());
        };
        let key = PropertyKey::from_str(name);
        if !self.has_property(global, &key)? && strict {
            let message = crate::format!("{name} is not defined");
            return Err(self.throw("ReferenceError", &message));
        }
        match self.ordinary_set(global, key.clone(), value, JsValue::Object(global)) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => match self.refuse_write(&key, why, strict) {
                Completion::Normal(_) => Ok(()),
                abrupt => Err(abrupt),
            },
            Err(abrupt) => Err(abrupt),
        }
    }

    /// `PutValue` step 2: what a write to a name that resolves to nothing does.
    fn unresolvable_put_value(
        &mut self,
        name: &str,
        value: JsValue,
        strict: bool,
    ) -> Result<(), Completion> {
        if strict {
            let message = format!("assignment to the undeclared `{name}`");
            return Err(self.throw("ReferenceError", &message));
        }
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return Ok(()) };
        self.object_mut(global).set_own(
            PropertyKey::from_str(name),
            Property {
                kind: PropertyKind::Data { value, writable: true },
                enumerable: true,
                configurable: true,
            },
        );
        Ok(())
    }

    /// Whether this scope IS the global one, which is what makes a `var` in it a property of the
    /// global object rather than only a binding beside it.
    pub(crate) fn is_global_scope(&self, scope: &Scope) -> bool {
        Rc::ptr_eq(scope, &self.global)
    }

    /// `CreateGlobalVarBinding(N, D)`: a top-level `var` is a PROPERTY of the global object, and a
    /// non-configurable one.
    ///
    /// # THE BINDING AND THE PROPERTY ARE ONE THING, AND THIS ENGINE HAD ONLY THE BINDING
    ///
    /// `var x = 1;` at the top level of a script makes `globalThis.x` read 1, makes
    /// `'x' in globalThis` true, and makes `delete x` **false**. Keeping it as a scope binding only
    /// answers the bare name correctly and every other question wrong -- and the failure is a realm
    /// disagreeing with itself, which is the shape `global_object_binding` was written to close in
    /// the other direction.
    ///
    /// `let`, `const` and `class` at the top level are deliberately NOT properties: the corpus
    /// asserts `this.hasOwnProperty('x') === false` for them directly, and that asymmetry is the
    /// whole reason the two kinds cannot share this path.
    ///
    /// # IT DOES NOTHING AT ALL WHEN THE PROPERTY IS ALREADY THERE
    ///
    /// The standard's step is *"If hasProperty is false and extensible is true"* -- and there is no
    /// `else`. A `var` over an EXISTING property leaves that property exactly as it was, attributes
    /// and all. Redefining a configurable one to `{writable: true, enumerable: true, configurable:
    /// false}` instead turns a read-only hidden property into a writable enumerable one, which
    /// `Object.defineProperty(globalThis, 'q', {configurable: true, writable: false, enumerable:
    /// false, value: 0}); $262.evalScript('var q;')` sees directly. **The redeclaration is a no-op,
    /// not a re-creation**, and only `CreateGlobalFunctionBinding` -- a different operation, below
    /// -- overwrites.
    ///
    /// NON-CONFIGURABLE when it does create, which is what makes `delete x` false for a declared
    /// global where it is TRUE for one an assignment created.
    ///
    /// THE EXTENSIBILITY TEST IS HERE AND ALSO IN [`Self::can_declare_global_var`]. That is the
    /// standard's own duplication: the predicate decides whether to THROW, and this decides whether
    /// to write, and a `[[Set]]` between them can change the answer.
    pub(crate) fn create_global_var_binding(&mut self, name: &str) {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return };
        let key = PropertyKey::from_str(name);
        if self.object(global).own(&key).is_some() || !self.object(global).extensible {
            return;
        }
        self.object_mut(global).set_own(
            key,
            Property {
                kind: PropertyKind::Data { value: JsValue::Undefined, writable: true },
                enumerable: true,
                configurable: false,
            },
        );
    }

    /// `CreateGlobalFunctionBinding(N, V, D)`: a top-level function declaration, which unlike a
    /// `var` ALWAYS writes.
    ///
    /// THE TWO OPERATIONS ARE OPPOSITES ON AN EXISTING PROPERTY, so they cannot share an
    /// implementation. A `var` leaves it alone; a function REPLACES it -- with the full
    /// `{writable: true, enumerable: true, configurable: D}` triple when the property is absent or
    /// configurable, and with a value-only descriptor otherwise, so a non-configurable global keeps
    /// its attributes and takes the new function.
    ///
    /// The final `Set` is the standard's step and not belt-and-braces: `DefinePropertyOrThrow`
    /// with a value-only descriptor over a NON-WRITABLE property is legal and changes nothing, and
    /// the `Set` is then what actually refuses -- silently, because its `Throw` argument is false.
    /// Both are needed and neither is sufficient.
    pub(crate) fn create_global_function_binding(&mut self, name: &str, value: JsValue) {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return };
        let key = PropertyKey::from_str(name);
        let existing = self.object(global).own(&key).cloned();
        let replace = match &existing {
            None => true,
            Some(property) => property.configurable,
        };
        if replace {
            self.object_mut(global).set_own(
                key,
                Property {
                    kind: PropertyKind::Data { value, writable: true },
                    enumerable: true,
                    configurable: false,
                },
            );
            return;
        }
        let Some(property) = existing else { return };
        if !property.is_writable() || property.is_accessor() {
            return;
        }
        let kept =
            Property { kind: PropertyKind::Data { value, writable: true }, ..property.clone() };
        self.object_mut(global).set_own(key, kept);
    }

    /// `HasLexicalDeclaration(N)`: whether the global DECLARATIVE record holds this name -- a
    /// `let`, a `const` or a `class` at the top level.
    fn has_lexical_declaration(&self, name: &str) -> bool {
        self.global.borrow().bindings.get(name).is_some_and(|binding| binding.lexical)
    }

    /// `HasRestrictedGlobalProperty(N)`: a global-object property a lexical declaration may not
    /// shadow, which is exactly a NON-CONFIGURABLE own property.
    ///
    /// It is about `[[Configurable]]` and nothing else, so `let Object` is legal (the realm's
    /// constructors are configurable) and `let undefined` is a SyntaxError. A `var` or a function
    /// creates non-configurable properties, which is why a `let` cannot follow one either.
    fn has_restricted_global_property(&mut self, name: &str) -> bool {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return false };
        self.object(global)
            .own(&PropertyKey::from_str(name))
            .is_some_and(|property| !property.configurable)
    }

    /// `CanDeclareGlobalVar(N)`: a redundant `var`, or a `var` for a name the object already has,
    /// is always allowed; a NEW one needs an extensible global object.
    fn can_declare_global_var(&mut self, name: &str) -> bool {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return true };
        self.object(global).own(&PropertyKey::from_str(name)).is_some()
            || self.object(global).extensible
    }

    /// `CanDeclareGlobalFunction(N)`: stricter than the `var` rule, because the declaration is going
    /// to WRITE.
    ///
    /// Step 4's escape needs `{[[Writable]]: true, [[Enumerable]]: true}` **together**. A
    /// non-configurable global that is writable but not enumerable fails it -- which is the case
    /// `script-decl-func-err-non-configurable.js` is about, and the one an "is it writable" reading
    /// lets through.
    fn can_declare_global_function(&mut self, name: &str) -> bool {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return true };
        let key = PropertyKey::from_str(name);
        let Some(property) = self.object(global).own(&key).cloned() else {
            return self.object(global).extensible;
        };
        if property.configurable {
            return true;
        }
        !property.is_accessor() && property.is_writable() && property.enumerable
    }

    /// The refusals `GlobalDeclarationInstantiation` performs BEFORE it creates anything.
    ///
    /// # EVERY ONE OF THEM FIRES BEFORE THE SCRIPT'S FIRST STATEMENT RUNS
    ///
    /// That is what makes them worth a separate pass rather than a check inside the hoisters: a
    /// script that collides with an existing global must have NO effect at all, and a check woven
    /// into the walk would already have created the names it visited before reaching the one that
    /// refuses.
    ///
    /// THE TWO ERROR KINDS ARE NOT INTERCHANGEABLE AND **node DISAGREES WITH THE STANDARD HERE**.
    /// A lexical collision is a SyntaxError; a `CanDeclareGlobal*` failure is a **TypeError**. V8
    /// reports `SyntaxError: Identifier 'x' has already been declared` for the function cases too,
    /// and the corpus (`script-decl-func-err-non-configurable.js`,
    /// `script-decl-func-err-non-extensible.js`) asserts TypeError. The specification text is the
    /// oracle here, not the neighbouring engine.
    pub(crate) fn global_declaration_refusals(
        &mut self,
        lexical_names: &[String],
        var_names: &[String],
        function_names: &[String],
    ) -> Result<(), Completion> {
        for name in lexical_names {
            if self.has_lexical_declaration(name) {
                let message = crate::format!("`{name}` has already been declared in this scope");
                return Err(self.throw("SyntaxError", &message));
            }
            if self.has_restricted_global_property(name) {
                let message =
                    crate::format!("`{name}` is a global property that cannot be shadowed");
                return Err(self.throw("SyntaxError", &message));
            }
        }
        for name in var_names.iter().chain(function_names) {
            if self.has_lexical_declaration(name) {
                let message = crate::format!("`{name}` has already been declared in this scope");
                return Err(self.throw("SyntaxError", &message));
            }
        }
        for name in function_names {
            if !self.can_declare_global_function(name) {
                let message = crate::format!("`{name}` cannot be declared on the global object");
                return Err(self.type_error(&message));
            }
        }
        for name in var_names {
            if function_names.iter().any(|declared| declared == name) {
                continue;
            }
            if !self.can_declare_global_var(name) {
                let message = crate::format!("`{name}` cannot be declared on the global object");
                return Err(self.type_error(&message));
            }
        }
        Ok(())
    }

    /// `DeleteBinding` for a bare name: what `delete x` answers.
    ///
    /// # IT WAS `true` FOR EVERYTHING, AND `delete` IS THE ONE OPERATOR WHOSE ANSWER IS THE
    /// WHOLE POINT
    ///
    /// `var y = 1; delete y` is **false** -- the property a `var` creates is non-configurable, and
    /// that is the difference between a declared global and one an assignment made. `delete` on a
    /// `let`, a `const`, a parameter or a function declaration is false too, because a declarative
    /// binding cannot be removed at all. Only two things answer `true`: a configurable property of
    /// the global object, and a name that resolves to nothing.
    /// And a third: a property of a `with` statement's binding object, which is where the
    /// `[[Delete]]` lands rather than on anything in the enclosing scope.
    ///
    /// **IT ANSWERS A `Result` BECAUSE THE `with` ARM CAN THROW AND NOTHING ELSE HERE CAN.**
    /// `HasBinding` on an object record runs a proxy's `has` trap and an `@@unscopables` getter, and
    /// `[[Delete]]` runs a `deleteProperty` trap -- three places user code can raise. Answering a
    /// bare `bool` would have made `delete x` report `false` for a throw, which is the operator
    /// whose whole value is its answer.
    pub(crate) fn delete_binding(&mut self, scope: &Scope, name: &str) -> Result<bool, Completion> {
        let mut current = Some(Rc::clone(scope));
        while let Some(environment) = current {
            let with_object = environment.borrow().with_object;
            if let Some(object) = with_object {
                if self.with_has_binding(object, name)? {
                    return self.delete_own_property(object, &PropertyKey::from_str(name));
                }
                current = environment.borrow().parent.clone();
                continue;
            }
            let found = environment.borrow().bindings.contains_key(name);
            if found {
                if !self.is_global_scope(&environment) {
                    return Ok(false);
                }
                break;
            }
            let parent = environment.borrow().parent.clone();
            current = parent;
        }
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return Ok(true) };
        let key = PropertyKey::from_str(name);
        let Some(property) = self.object(global).own(&key).cloned() else {
            return Ok(!self.global.borrow().bindings.contains_key(name));
        };
        if !property.configurable {
            return Ok(false);
        }
        self.object_mut(global).delete_own(&key);
        self.global.borrow_mut().bindings.remove(name);
        Ok(true)
    }

    /// `SetMutableBinding` on an object environment record: `HasProperty`, then `Set`.
    ///
    /// The `HasProperty` is the SECOND one for this name -- `HasBinding` already asked -- and it
    /// is not redundant for the reason [`Self::object_binding_value`] gives: the `@@unscopables`
    /// getter between them can delete the property, and so can the getter a compound assignment's
    /// own read runs.
    ///
    /// # STEP 3 FIRES, AND THE ARGUMENT THAT IT CANNOT IS ABOUT THE WRONG THING
    ///
    /// That argument runs: `with` is sloppy-only, so `S` is never true. It is a claim about the
    /// STATEMENT, and the rule is about the code doing the WRITING -- a strict function declared
    /// inside a `with` body closes over the object environment record, and every reference it takes
    /// through that record is strict. Sixteen corpus rows do exactly this, and an engine that
    /// re-walks the chain and complains about an undeclared name passes them by raising the right
    /// exception from the wrong rule, which stops being right the moment the walk is fixed.
    ///
    /// THE THROW COMES BEFORE THE `Set`, so a refused write leaves the binding object untouched:
    /// the corpus asserts `!('x' in scope)` after the ReferenceError, and a `Set` first would have
    /// re-created the very property whose absence is the error.
    ///
    /// A REFUSED WRITE IS SILENT IN SLOPPY CODE. `Set(O, P, V, false)` discards the failure, so
    /// `with (o) { x = 2 }` against a non-writable `x` changes nothing and throws nothing.
    fn object_set_mutable_binding(
        &mut self,
        object: ObjectId,
        name: &str,
        value: JsValue,
        strict: bool,
    ) -> Result<(), Completion> {
        let key = PropertyKey::from_str(name);
        let still_exists = self.has_property(object, &key)?;
        if !still_exists && strict {
            let message = crate::format!("{name} is not defined");
            return Err(self.throw("ReferenceError", &message));
        }
        match self.ordinary_set(object, key.clone(), value, JsValue::Object(object)) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => match self.refuse_write(&key, why, strict) {
                Completion::Normal(_) => Ok(()),
                abrupt => Err(abrupt),
            },
            Err(abrupt) => Err(abrupt),
        }
    }

    fn update_on_global_object(&mut self, name: &str, value: JsValue) {
        let Some(JsValue::Object(global)) = self.global_value("globalThis") else { return };
        let key = PropertyKey::from_str(name);
        let Some(existing) = self.object(global).own(&key).cloned() else { return };
        if !existing.is_writable() {
            return;
        }
        let property = Property { kind: PropertyKind::Data { value, writable: true }, ..existing };
        self.object_mut(global).set_own(key, property);
    }





    /// Shortens an array to `length`, deleting the elements above it from the TOP DOWN.
    ///
    /// **A NON-CONFIGURABLE ELEMENT STOPS THE SHORTENING WHERE IT STANDS, and `length` ends up
    /// just past it rather than where it was asked to go.** Make `a[1]` non-configurable, then
    /// `a.length = 0` leaves `length` at 2 and reports failure -- an array cannot forget an element
    /// it is not allowed to delete. An engine that deletes what it can and records the REQUESTED
    /// number produces an array whose `length` disagrees with its own contents, which is the same
    /// silent shape as not truncating at all.
    ///
    /// TOP DOWN IS THE SPECIFIED ORDER, and it means the elements ABOVE a stuck one are already
    /// gone when it refuses -- which is consistent rather than lossy, because those were deletable
    /// and the array genuinely no longer has them. **I first wrote the opposite in this comment and
    /// a test encoded it; the engine was right and the reasoning was not.** Bottom-up would stop at
    /// the stuck element with everything above it still present and `length` claiming otherwise.
    fn shorten_array(&mut self, id: ObjectId, length: f64) -> Result<(), f64> {
        let mut doomed: Vec<(u32, PropertyKey)> = self
            .object(id)
            .own_keys()
            .into_iter()
            .filter_map(|key| key.as_array_index().map(|index| (index, key)))
            .filter(|(index, _)| f64::from(*index) >= length)
            .collect();
        doomed.sort_by(|a, b| b.0.cmp(&a.0));
        for (index, key) in doomed {
            if !self.object_mut(id).delete_own(&key) {
                let stopped = f64::from(index) + 1.0;
                self.write_length(id, stopped);
                return Err(stopped);
            }
        }
        self.write_length(id, length);
        Ok(())
    }

    fn write_length(&mut self, id: ObjectId, length: f64) {
        let key = PropertyKey::from_str("length");
        let writable = self.object(id).own(&key).is_none_or(Property::is_writable);
        self.object_mut(id).set_own(
            key,
            Property {
                kind: PropertyKind::Data { value: JsValue::Number(length), writable },
                enumerable: false,
                configurable: false,
            },
        );
    }

    /// `ArraySetLength` for `Object.defineProperty`, which must REPORT the refusal rather than
    /// swallow it the way a sloppy assignment does.
    pub(crate) fn define_array_length(&mut self, id: ObjectId, length: f64) -> Option<String> {
        match self.shorten_array(id, length) {
            Ok(()) => None,
            Err(stopped) => Some(format!(
                "cannot shorten past index {} because it is not configurable",
                stopped as u64 - 1
            )),
        }
    }

    /// The frozen strings array a tagged template hands its tag.
    ///
    /// **THE SAME CALL SITE MUST HAND OVER THE SAME ARRAY EVERY TIME, and that is observable:**
    /// `function f(s) { return s; } f`a` === f`a`` is TRUE. Tags rely on it to cache work against
    /// the site -- a fresh array each evaluation makes every such cache miss forever and turns a
    /// memoised template into a leak. Keyed on the SPAN, which is the call site's identity.
    ///
    /// An invalid escape is NOT an error here, unlike in a plain template: `cooked` is
    /// `undefined` and `raw` still carries the text as written. That is the whole reason a tag can
    /// implement a DSL with its own escape rules.
    fn template_object(&mut self, span: Span, quasis: &[TemplateElement]) -> ObjectId {
        if let Some(cached) = self.template_objects.get(&(span.start, span.end)) {
            return *cached;
        }
        let cooked: Vec<JsValue> = quasis
            .iter()
            .map(|quasi| match &quasi.cooked {
                Some(text) => JsValue::String(text.clone()),
                None => JsValue::Undefined,
            })
            .collect();
        let raw: Vec<JsValue> =
            quasis.iter().map(|quasi| JsValue::string(&quasi.raw)).collect();
        let strings = self.new_array(cooked);
        let raw_array = self.new_array(raw);
        self.freeze_template(raw_array);
        self.object_mut(strings)
            .set_own(PropertyKey::from_str("raw"), Property::builtin(JsValue::Object(raw_array)));
        self.freeze_template(strings);
        self.template_objects.insert((span.start, span.end), strings);
        strings
    }

    fn freeze_template(&mut self, id: ObjectId) {
        self.object_mut(id).extensible = false;
        for key in self.object(id).own_keys() {
            if let Some(mut property) = self.object(id).own(&key).cloned() {
                property.configurable = false;
                if let PropertyKind::Data { writable, .. } = &mut property.kind {
                    *writable = false;
                }
                self.object_mut(id).set_own(key, property);
            }
        }
    }




    /// The one loop driver, and the one place `break`/`continue` are decided.
    ///
    /// A `break`/`continue` with NO label belongs to the nearest loop; one WITH a label belongs to
    /// the loop that carries it and passes through every other. Absorbing an unmatched label here
    /// would strand `break outer` inside the inner loop.
    ///
    /// # `for (let i = 0; ...)` GETS A FRESH `i` PER ITERATION, AND A SHARED ONE IS THE CLASSIC
    /// WRONG ANSWER
    ///
    /// ```text
    /// var fs = []; for (let i = 0; i < 3; i++) { fs.push(() => i); }
    /// fs.map(f => f())     // 0,1,2   -- and 3,3,3 with one shared binding
    /// ```
    ///
    /// `CreatePerIterationEnvironment` copies the head's `let` bindings into a NEW environment
    /// before the first test and again after every body, so each closure captures a different one.
    /// The increment then runs in the NEW environment, which is what carries the value forward.
    /// This is the reason `let` in a loop head is not merely a scoped `var`, and an engine that
    /// shares the binding produces `3,3,3` -- a plausible answer, wrong, and silent.
    ///
    /// `const` IS EXCLUDED, because a `const` head cannot be updated and there is nothing for a
    /// second environment to carry. And each new environment's parent is the scope AROUND the
    /// loop rather than the previous iteration's, or the chain grows by one link per iteration and
    /// every lookup in a long loop gets slower than the one before it.
    fn loop_body(
        &mut self,
        label: Option<String>,
        scope: &Scope,
        per_iteration: &[String],
        mut step: impl FnMut(&mut Self, &Scope) -> Completion,
    ) -> Completion {
        const MAX_ITERATIONS: u64 = 10_000_000;
        let mut iterations = 0u64;
        let mut head = copy_for_iteration(scope, per_iteration);
        loop {
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                return self.internal_defect("loop iteration limit");
            }
            let inner = new_scope(Some(Rc::clone(&head)));
            let completion = step(self, &inner);
            head = copy_for_iteration(&head, per_iteration);
            match completion {
                Completion::Normal(_) | Completion::Continue(None) => {}
                Completion::Continue(Some(target)) if Some(&target) == label.as_ref() => {}
                Completion::Break(None) => return Completion::Normal(JsValue::Undefined),
                Completion::Break(Some(target)) if Some(&target) == label.as_ref() => {
                    return Completion::Normal(JsValue::Undefined)
                }
                abrupt => return abrupt,
            }
        }
    }




    /// The leaf of every binding pattern, in all three modes.
    ///
    /// ONE COPY OF THE THREE MODES. Which binding a name means -- the hoisted `var`, a fresh one
    /// here, or whatever it already names -- is the part that is easy to get subtly wrong, and the
    /// walker reaches this rather than reimplementing it. Private, and visible to the walker
    /// because `interpreter::artifact` is a descendant module rather than a stranger.
    fn bind_identifier(
        &mut self,
        name: &str,
        value: JsValue,
        scope: &Scope,
        binds: Binds,
    ) -> Result<(), Completion> {
        match binds {
            Binds::Var | Binds::GlobalVar => {
                {
                    let mut borrowed = scope.borrow_mut();
                    if let Some(binding) = borrowed.bindings.get_mut(name) {
                        binding.value = value.clone();
                        binding.initialized = true;
                        drop(borrowed);
                        if binds == Binds::GlobalVar {
                            self.update_on_global_object(name, value);
                        }
                        return Ok(());
                    }
                }
                self.assign_existing(scope, name, value)
            }
            Binds::Declare => {
                scope.borrow_mut().bindings.insert(
                    name.to_string(),
                    Binding::var(value),
                );
                Ok(())
            }
            Binds::Assign => self.assign_existing(scope, name, value),
        }
    }






    /// `CopyDataProperties`: the operation behind object rest (`{a, ...rest}`) and object spread
    /// (`{...o}`).
    ///
    /// **THE SAME OPERATION REACHED THROUGH TWO SYNTAXES, WHICH IS WHY IT IS ONE FUNCTION.** The
    /// published absence list carried `object-spread` and `destructuring` as separate entries, and
    /// implementing one without the other is how two doors into one operation come to disagree --
    /// the shape that has been a finding here twice (`map`/`reduce`, then `defineProperty` and
    /// plain assignment into an array's `length`).
    ///
    /// It copies OWN ENUMERABLE properties, symbol keys included, and it copies VALUES -- an
    /// accessor's getter runs and the result lands as a plain data property.
    fn copy_data_properties(
        &mut self,
        target: ObjectId,
        source: &JsValue,
        excluded: &[PropertyKey],
    ) -> Result<(), Completion> {
        if matches!(source, JsValue::Undefined | JsValue::Null) {
            return Ok(());
        }
        let from = self.to_object(source)?;
        for key in self.object(from).own_keys() {
            if excluded.contains(&key) {
                continue;
            }
            let Some(enumerable) = self.object(from).own(&key).map(|p| p.enumerable) else {
                continue;
            };
            if !enumerable {
                continue;
            }
            let item = match self.get_property(from, &key) {
                Completion::Normal(item) => item,
                abrupt => return Err(abrupt),
            };
            self.object_mut(target).set_own(key, Property::data(item));
        }
        Ok(())
    }





    fn call(&mut self, closure: &Closure, arguments: Vec<JsValue>, this: JsValue) -> Completion {
        let caller_strict = self.strict;
        self.strict = closure.strict;
        let completion = self.call_body(
            closure,
            arguments,
            this,
            &new_scope(Some(Rc::clone(&closure.scope))),
            BodyPhase::Whole,
        );
        self.strict = caller_strict;
        completion
    }

    /// `EvaluateGeneratorBody`: bind the parameters, build the generator, run NONE of the body.
    ///
    /// The parameter binding goes through the ordinary call path with the body suppressed, so the
    /// `this` substitution, the `arguments` object and every parameter rule are the SAME code an
    /// ordinary call uses. Writing a second binding pass here is the shape this engine has paid for
    /// repeatedly -- one rule with two implementations, and the case that lands in only one.
    fn call_generator(
        &mut self,
        closure: &Closure,
        index: u32,
        function: ObjectId,
        arguments: Vec<JsValue>,
        this: JsValue,
    ) -> Completion {
        let caller_strict = self.strict;
        self.strict = closure.strict;
        let scope = new_scope(Some(Rc::clone(&closure.scope)));
        let bound = self.call_body(closure, arguments, this, &scope, BodyPhase::ParametersOnly);
        self.strict = caller_strict;
        let Completion::Normal(_) = bound else { return bound };
        let Some((simple, this)) = self.pending_generator_frame.take() else {
            return self.internal_defect("binding a generator's parameters recorded no frame");
        };
        let frame = self.allocate(Object::new(Some(self.intrinsics.object_prototype)));
        let _ = self.create_data_property(
            frame,
            PropertyKey::from_str(crate::generator_transform::STATE),
            JsValue::Number(0.0),
        );
        scope.borrow_mut().bindings.insert(
            crate::generator_transform::FRAME.to_string(),
            Binding::var(JsValue::Object(frame)),
        );
        let body_scope = crate::interpreter::artifact::body_environment(&scope, simple);
        crate::generator::create(
            self,
            function,
            GeneratorContext { closure: index, scope, body_scope, simple, this, frame },
        )
    }

    /// Runs a generator's body to completion, in the environment its parameters were bound into.
    ///
    /// **THE `this` AND THE `arguments` OBJECT ARE NOT REBUILT.** Both were made when the generator
    /// function was called, and the scope carries the second; re-deriving either at resumption
    /// would give the generator whatever frame happened to be calling `next()`, which is the same
    /// dynamic-scope-wearing-lexical-syntax mistake [`Closure::captured_this`] records.
    pub(crate) fn run_generator_body(&mut self, context: &GeneratorContext) -> Completion {
        let mut closure = self.closures[context.closure as usize].clone();
        closure.captured_this = Some(context.this.clone());
        closure.needs_arguments = false;
        let caller_strict = self.strict;
        self.strict = closure.strict;
        self.pending_generator_body_scope = Some(Rc::clone(&context.body_scope));
        let completion = self.call_body(
            &closure,
            Vec::new(),
            JsValue::Undefined,
            &context.scope,
            BodyPhase::BodyOnly { simple: context.simple },
        );
        self.pending_generator_body_scope = None;
        self.strict = caller_strict;
        completion
    }

    /// `GetSuperConstructor()`: the running class's own `[[GetPrototypeOf]]`, which
    /// [`Self::construct_parent`] then checks is a constructor.
    ///
    /// # IT IS READ WHEN `super()` RUNS, NOT WHEN THE CLASS WAS DEFINED
    ///
    /// A class's prototype link is ordinary and configurable, so a constructor may change it before
    /// calling `super()` and the standard follows the change:
    /// `class E extends baseFunc { constructor() { Object.setPrototypeOf(E, other); super(); } }`
    /// calls `other`. A parent captured at class-definition time calls `baseFunc` instead -- a
    /// different program, and one no test that never reassigns a prototype can see.
    ///
    /// `class C extends null {}` needs no special case: its own prototype is `%Function.prototype%`,
    /// which is not a constructor, so the check answers the TypeError directly.
    pub(crate) fn super_constructor(&mut self, class_object: ObjectId) -> Option<ObjectId> {
        self.object(class_object).prototype
    }

    /// `SuperCall` steps 5-6: refuse a superclass that is not a constructor, then
    /// `Construct(func, argList, newTarget)` and answer the object the parent produced.
    ///
    /// # IT CONSTRUCTS, AND A DERIVED CONSTRUCTOR HAS NO INSTANCE FOR IT TO STRAND
    ///
    /// The reading to avoid is *"`super(...)` runs the parent against the instance that already
    /// exists; constructing would initialize a SECOND object and leave the one being built
    /// untouched"*. The consequence is real and the premise is not: `OrdinaryCallBindThis` is
    /// skipped for a derived constructor, `this` is uninitialized until `super()`, and step 8 --
    /// `BindThisValue(result)` -- is where the instance comes from. Allocating one up front is what
    /// makes constructing look as though it would strand it.
    ///
    /// What constructing buys: a parent whose constructor RETURNS AN OBJECT hands that object to
    /// the subclass, which no amount of property copying can imitate; and a native parent needs no
    /// slot-moving path at all -- `class AB extends ArrayBuffer {}` gets the very buffer
    /// `ArrayBuffer` built, with its prototype set from `new.target`, so the slot is present
    /// because it was never anywhere else. COPYING PROPERTIES CANNOT CARRY A SLOT: what makes an
    /// `ArrayBuffer` an ArrayBuffer is [`crate::object::Object::binary`] and what makes a `Date` a
    /// Date is its time value, and neither is a property -- they are the brand every one of those
    /// prototypes' methods checks.
    ///
    /// A native parent needs `[[Construct]]` for a second reason: `[[Call]]` and `[[Construct]]`
    /// are separate internal methods on a built-in, and `ArrayBuffer`, `Map`, `Promise` and
    /// `DataView` all *refuse* their call entry outright -- so reaching for the call half answers
    /// "this constructor requires `new`".
    ///
    /// # THE READ AND THE CHECK ARE **STEPS 3 AND 5**, WITH THE ARGUMENTS BETWEEN THEM
    ///
    /// Doing both at once is the natural way to write it and it is wrong in BOTH directions, which
    /// is why `staging/sm/class/superCallOrder.js` tests both in one file:
    ///
    /// - `super(Object.setPrototypeOf(C, null))` must SUCCEED. The prototype is read at step 3,
    ///   before the argument runs, so the class it names is still the parent.
    /// - `Object.setPrototypeOf(C, Math.sin)` and then `super(thrower())` must throw the ARGUMENT'S
    ///   error, not a TypeError about `Math.sin`. The check is step 5, after the arguments.
    pub(crate) fn construct_parent(
        &mut self,
        parent: Option<ObjectId>,
        arguments: Vec<JsValue>,
    ) -> Completion {
        let Some(parent) = parent.filter(|id| self.is_constructor(*id)) else {
            return self.type_error("the superclass is not a constructor");
        };
        let new_target = match self.current_new_target() {
            JsValue::Object(id) => id,
            _ => return self.internal_defect("`super()` ran with no `new.target`"),
        };
        self.construct_with_new_target(parent, arguments, new_target)
    }


    /// The `this` a running expression sees: the frame's binding, or the GLOBAL OBJECT when there
    /// is no frame.
    ///
    /// AN EMPTY STACK MEANS GLOBAL SCOPE, AND IT MEANT `undefined`. That is not a missing case,
    /// it is a wrong answer with a wide blast radius: `this` at the top level of a script is the
    /// global object in strict code and sloppy code alike -- there is no mode in which it is
    /// `undefined`. Every `verifyProperty(this, 'Array', ...)` in the corpus met a TypeError about
    /// converting undefined, and the standard's own SpiderMonkey shell files are written as
    /// `(function(global) { global.raisesException = ...; })(this)`, so THREE harness files failed
    /// to load and took every test that includes them with them.
    ///
    /// An arrow does not push a frame, so an arrow written at the top level lands here too and
    /// gets the same answer -- which is what a lexical `this` means.
    pub(crate) fn current_this(&self) -> JsValue {
        if let Some(this) = self.bound_this() {
            return this;
        }
        match self.this_stack.last() {
            Some(value) => value.clone(),
            None => self.global_value("globalThis").unwrap_or(JsValue::Undefined),
        }
    }

    /// `IsConstructor`: whether `new` on this value could ever succeed.
    ///
    /// IT IS NOT "IS CALLABLE". `[[Construct]]` is a separate internal method and most callable
    /// objects lack it -- an arrow, a method definition, `Math.max`, and the resolving functions a
    /// promise hands out are all callable and none is a constructor. `class C extends f {}` with a
    /// callable-but-not-constructible `f` is a TypeError at the CLASS, before any instance exists,
    /// and answering from callability alone defers it to a confusing failure later.
    pub(crate) fn is_constructor(&self, id: ObjectId) -> bool {
        match self.object(id).callable {
            Some(Callable::Closure(index)) => self.closures[index as usize].constructs(),
            Some(Callable::Native(index)) => self.natives[index as usize].construct.is_some(),
            Some(Callable::Bound(index)) => self.is_constructor(self.bounds[index as usize].target),
            Some(Callable::Proxy) => crate::proxy::is_constructor(self, id),
            Some(_) | None => false,
        }
    }

    /// The TypedArray this object is, if it is one. `None` for everything else, including a
    /// `DataView` -- the brand check that separates them.
    pub(crate) fn typed_array_view(&self, id: ObjectId) -> Option<crate::typed_array::View> {
        match self.object(id).binary.as_deref() {
            Some(crate::object::BinaryData::TypedArray { buffer, offset, length, element }) => {
                Some(crate::typed_array::View {
                    buffer: *buffer,
                    offset: *offset,
                    length: *length,
                    element: *element,
                })
            }
            _ => None,
        }
    }

    /// Whether an ArrayBuffer has been detached. `false` for anything that is not one, because a
    /// question about a buffer that is not there is not a detachment.
    pub(crate) fn buffer_is_detached(&self, id: ObjectId) -> bool {
        matches!(
            self.object(id).binary.as_deref(),
            Some(crate::object::BinaryData::Buffer { detached: true, .. })
        )
    }

    /// Which of the nine element types a constructor object makes.
    pub(crate) fn typed_array_kind_of_constructor(
        &self,
        id: ObjectId,
    ) -> Option<crate::object::Element> {
        self.intrinsics
            .typed_arrays
            .iter()
            .position(|(constructor, _)| *constructor == id)
            .map(|index| crate::object::Element::ALL[index])
    }

    pub(crate) fn typed_array_prototype_for(
        &self,
        element: crate::object::Element,
    ) -> ObjectId {
        let index = crate::object::Element::ALL
            .iter()
            .position(|candidate| *candidate == element)
            .expect("every element type is in the table");
        self.intrinsics.typed_arrays[index].1
    }

    /// "the intrinsic object associated with the constructor name `exemplar.[[TypedArrayName]]`",
    /// which is the DEFAULT `TypedArraySpeciesCreate` falls back to when a program has not named
    /// one.
    pub(crate) fn typed_array_constructor_for(
        &self,
        element: crate::object::Element,
    ) -> ObjectId {
        let index = crate::object::Element::ALL
            .iter()
            .position(|candidate| *candidate == element)
            .expect("every element type is in the table");
        self.intrinsics.typed_arrays[index].0
    }

    pub(crate) fn register_typed_array_constructor(
        &mut self,
        constructor: ObjectId,
        prototype: ObjectId,
        element: crate::object::Element,
    ) {
        let index = crate::object::Element::ALL
            .iter()
            .position(|candidate| *candidate == element)
            .expect("every element type is in the table");
        self.intrinsics.typed_arrays[index] = (constructor, prototype);
    }

    /// A fresh zero-filled `ArrayBuffer`, refusing a size this part cannot hold.
    pub(crate) fn new_array_buffer(&mut self, bytes: usize) -> Result<ObjectId, Completion> {
        crate::binary::new_buffer_checked(self, bytes)
    }

    /// `ToIndex`: a non-negative integer that fits, or a RangeError by the caller's name for it.
    pub(crate) fn to_index(
        &mut self,
        value: &JsValue,
        message: &str,
    ) -> Result<usize, Completion> {
        if matches!(value, JsValue::Undefined) {
            return Ok(0);
        }
        let number = self.to_number_value(value)?;
        let integer = if number.is_nan() { 0.0 } else { crate::math::trunc(number) };
        if integer < 0.0 || integer > 9_007_199_254_740_991.0 {
            return Err(self.throw("RangeError", message));
        }
        Ok(integer as usize)
    }

    /// `SpeciesConstructor(obj, defaultCtor)`: which constructor a derived object's methods should
    /// build their results with.
    ///
    /// # WITHOUT IT A SUBCLASS SILENTLY DEGRADES TO ITS BASE, WHICH IS A WRONG ANSWER
    ///
    /// `class A extends Array {}` then `new A(...).map(f)` must produce an `A`. Returning a plain
    /// `Array` loses every method the subclass added, and it does so quietly: the result still
    /// walks, still has a `length`, and fails only where the program relies on being what it
    /// subclassed. The whole point of `@@species` is that a class can say what its derived results
    /// are, and an engine that never asks has decided for it.
    ///
    /// `undefined` and `null` BOTH mean "use the default", and they are the only two values that
    /// do -- anything else that is not a constructor is a TypeError rather than a fallback.
    pub(crate) fn species_constructor(
        &mut self,
        object: ObjectId,
        default_constructor: ObjectId,
    ) -> Result<ObjectId, Completion> {
        let constructor =
            match self.get_property(object, &PropertyKey::from_str("constructor")) {
                Completion::Normal(value) => value,
                abrupt => return Err(abrupt),
            };
        let constructor = match constructor {
            JsValue::Undefined => return Ok(default_constructor),
            JsValue::Object(id) => id,
            _ => return Err(self.type_error("`constructor` is not an object")),
        };
        let key = PropertyKey::Symbol(self.well_known_symbol("species"));
        let species = match self.get_property(constructor, &key) {
            Completion::Normal(value) => value,
            abrupt => return Err(abrupt),
        };
        match species {
            JsValue::Undefined | JsValue::Null => Ok(default_constructor),
            JsValue::Object(id) if self.is_constructor(id) => Ok(id),
            _ => Err(self.type_error("`Symbol.species` is not a constructor")),
        }
    }

    /// Defines `get [Symbol.species]() { return this; }` on a constructor.
    ///
    /// IT RETURNS THE RECEIVER, NOT THE CONSTRUCTOR IT WAS DEFINED ON, and that is the whole
    /// mechanism: a subclass inherits this getter and answers with ITSELF, which is how
    /// `subclassInstance.map(f)` comes back as the subclass without the subclass writing anything.
    pub(crate) fn define_species_getter(&mut self, constructor: ObjectId) {
        let key = PropertyKey::Symbol(self.well_known_symbol("species"));
        let getter = self.native_function("get [Symbol.species]", 0, |_, this, _| {
            Completion::Normal(this)
        });
        self.object_mut(constructor)
            .set_own(key, Property::accessor(Some(getter), None));
    }

    /// Whether the running frame is a derived constructor -- or an arrow written inside one, which
    /// shares its `this` cell and may therefore call `super()` too.
    pub(crate) fn frame_is_derived(&self) -> bool {
        matches!(self.derived_this.last(), Some(Some(_)))
    }

    /// Whether the running frame's `this` may be read yet.
    ///
    /// AN EMPTY STACK IS INITIALIZED: the global `this` needs no constructor to bring it into
    /// existence, and answering "not yet" there would make every top-level `this` a ReferenceError.
    pub(crate) fn this_is_initialized(&self) -> bool {
        match self.derived_this.last() {
            Some(Some(cell)) => cell.borrow().is_some(),
            _ => true,
        }
    }

    /// The instance `super()` bound in the running frame, if it has run.
    fn bound_this(&self) -> Option<JsValue> {
        match self.derived_this.last() {
            Some(Some(cell)) => cell.borrow().clone(),
            _ => None,
        }
    }

    /// Records that `super()` has run, and refuses a SECOND one.
    ///
    /// Calling `super()` twice is a ReferenceError rather than a harmless re-initialization: the
    /// standard binds `this` once and refuses to rebind it, and an engine that simply re-ran the
    /// parent would leave an instance built by two constructors with the first one's work lost.
    pub(crate) fn bind_this_value(&mut self, value: JsValue) -> Result<(), Completion> {
        let Some(Some(cell)) = self.derived_this.last().cloned() else { return Ok(()) };
        if cell.borrow().is_some() {
            return Err(self.throw(
                "ReferenceError",
                "`super()` was already called in this constructor",
            ));
        }
        *cell.borrow_mut() = Some(value);
        Ok(())
    }

    /// The ReferenceError a `this` read raises before `super()` has run.
    pub(crate) fn this_before_super(&mut self) -> Completion {
        self.throw(
            "ReferenceError",
            "`this` is not initialized until `super()` has been called",
        )
    }

    /// `OrdinaryCallBindThis`: what a call actually binds, which is not always what it was passed.
    ///
    /// **SLOPPY MODE SUBSTITUTES, STRICT MODE DOES NOT, AND THE DIFFERENCE IS THE WHOLE RULE.**
    /// A non-strict function called with no receiver gets the GLOBAL OBJECT, and one called on a
    /// primitive gets that primitive BOXED -- so `function f() { return this; } f()` is the global
    /// object and `f.call(1)` is a `Number` wrapper, while the same function with `"use strict"`
    /// returns `undefined` and `1`. An engine that skips this step reports `undefined` for the
    /// commonest `this` in the language.
    ///
    /// The strictness is the CLOSURE'S, captured where it was written. Reading the caller's gets
    /// both directions wrong: a sloppy function called from strict code still substitutes.
    fn bind_this(&mut self, closure: &Closure, this: JsValue) -> JsValue {
        if closure.strict {
            return this;
        }
        match this {
            JsValue::Undefined | JsValue::Null => self.current_global_object(),
            primitive @ (JsValue::Number(_)
            | JsValue::String(_)
            | JsValue::Boolean(_)
            | JsValue::Symbol(_)) => match self.to_object(&primitive) {
                Ok(id) => JsValue::Object(id),
                Err(_) => primitive,
            },
            object => object,
        }
    }

    fn current_global_object(&self) -> JsValue {
        self.global_value("globalThis").unwrap_or(JsValue::Undefined)
    }

    /// Leaves a call frame. One function so the three stacks cannot be popped out of step.
    fn pop_frame(&mut self) {
        self.this_stack.pop();
        self.derived_this.pop();
        self.home_stack.pop();
        self.class_stack.pop();
        self.new_target_stack.pop();
    }

    /// The `new.target` of the running frame: `undefined` outside any call, which is what a script's
    /// top level sees.
    pub(crate) fn current_new_target(&self) -> JsValue {
        self.new_target_stack.last().cloned().unwrap_or(JsValue::Undefined)
    }

    /// Consumes the value `[[Construct]]` left for the call it is about to make.
    ///
    /// It is a TAKE rather than a read: an ordinary call that follows a construct must see
    /// `undefined`, and a value left behind would make every function called from a constructor
    /// body report that constructor as its own `new.target`.
    fn take_new_target(&mut self) -> JsValue {
        self.pending_new_target.take().unwrap_or(JsValue::Undefined)
    }

    /// `OrdinaryCreateFromConstructor`'s prototype: what the instance a BUILT-IN is constructing
    /// should inherit from, given the intrinsic it would use if nobody had subclassed it.
    ///
    /// **ONLY A CONSTRUCTOR THAT READS ITS OWN OBJECT NEEDS TO ASK.** `[[Construct]]` writes the
    /// same prototype onto whatever the built-in returns, so an implementation that allocates from
    /// the intrinsic and never looks is already correct and stays that way. The four collection
    /// constructors are the ones the standard has looking: each takes its adder with
    /// `Get(obj, "set")` or `Get(obj, "add")`, and a lookup is a prototype walk. They share one
    /// body, so there is one caller.
    ///
    /// A non-object `prototype` on `new.target` is `None` here, exactly as it is for the write
    /// afterwards -- the standard falls back to the intrinsic in that case rather than refusing.
    pub(crate) fn instance_prototype(&self, intrinsic: ObjectId) -> ObjectId {
        self.pending_instance_prototype.unwrap_or(intrinsic)
    }

    fn call_body(
        &mut self,
        closure: &Closure,
        arguments: Vec<JsValue>,
        this: JsValue,
        scope: &Scope,
        phase: BodyPhase,
    ) -> Completion {
        let scope = Rc::clone(scope);
        let this = match &closure.captured_this {
            Some(captured) => captured.clone(),
            None => self.bind_this(closure, this),
        };
        self.this_stack.push(this.clone());
        self.derived_this.push(if closure.is_arrow {
            closure.captured_this_cell.clone()
        } else if closure.class_kind == ClassKind::Derived {
            Some(Rc::new(core::cell::RefCell::new(None)))
        } else {
            None
        });
        self.home_stack.push(closure.home_object);
        self.class_stack.push(closure.class_object);
        let new_target = match &closure.captured_new_target {
            Some(captured) => captured.clone(),
            None => self.take_new_target(),
        };
        self.new_target_stack.push(new_target);
        if !closure.is_arrow {
            if closure.needs_arguments {
                let callee = self.pending_callee.take();
                let arguments_object =
                    self.new_arguments_object(&arguments, closure.strict, callee, &scope);
                scope.borrow_mut().bindings.insert(
                    "arguments".to_string(),
                    Binding::var(JsValue::Object(arguments_object)),
                );
            }
        }
        if closure.implicit_derived {
            let Some(class_object) = closure.class_object else {
                self.pop_frame();
                return self.type_error("the superclass is not a constructor");
            };
            let parent = self.super_constructor(class_object);
            let completion = self.construct_parent(parent, arguments.clone());
            let Completion::Normal(result) = completion else {
                self.pop_frame();
                return completion;
            };
            if let Err(abrupt) = self.bind_this_value(result) {
                self.pop_frame();
                return abrupt;
            }
        }
        let Some(at) = closure.code_at else {
            let bound = if closure.class_kind == ClassKind::Derived { self.bound_this() } else { None };
            self.pop_frame();
            return Completion::Normal(bound.unwrap_or(JsValue::Undefined));
        };
        let completion = self.call_body_from_artifact(at, &arguments, &scope, phase);
        if phase == BodyPhase::ParametersOnly {
            let simple = self.pending_params_simple.take().unwrap_or(true);
            self.pending_generator_frame = Some((simple, this.clone()));
        }
        let mut bound = None;
        let verdict = match &completion {
            Completion::Normal(value)
                if closure.class_kind == ClassKind::Derived && !closure.is_arrow =>
            {
                match value {
                    JsValue::Object(_) => Ok(()),
                    JsValue::Undefined if self.this_is_initialized() => {
                        bound = self.bound_this();
                        Ok(())
                    }
                    JsValue::Undefined => Err(Derived::NoSuper),
                    _ => Err(Derived::NotAnObject),
                }
            }
            _ => Ok(()),
        };
        self.pop_frame();
        match verdict {
            Ok(()) => match bound {
                Some(this) => Completion::Normal(this),
                None => completion,
            },
            Err(Derived::NoSuper) => self.this_before_super(),
            Err(Derived::NotAnObject) => {
                self.type_error("a derived constructor may return only an object or `undefined`")
            }
        }
    }



    /// The key an object-literal property names, from the DECODED form of that key.
    ///
    /// A COMPUTED key never arrives here. `property_key_node` recognises `Tag::KeyComputed` and
    /// WALKS its expression, because that expression is ordinary program text that can call
    /// anything. What is left is four single-node forms with no evaluation in them at all, which is
    /// why this takes no scope.
    fn property_key(&mut self, key: &crate::ast::PropertyKey) -> Result<PropertyKey, Completion> {
        use crate::ast::PropertyKey as Ast;
        match key {
            Ast::Identifier { name, .. } => Ok(PropertyKey::from_str(name)),
            Ast::String { value, .. } => Ok(PropertyKey::String(value.clone())),
            Ast::Number { value, .. } => Ok(PropertyKey::from_str(&ops::number_to_string(*value))),
            Ast::Private { name, .. } => Ok(PropertyKey::from_str(&format!("#{name}"))),
            Ast::Computed { .. } => {
                Err(self.internal_defect("a computed key reached the decoded key path"))
            }
        }
    }

    /// Reads a member from a value.
    ///
    /// WARNING: READING A PROPERTY OF `null` OR `undefined` IS A TypeError, not `undefined`. It is
    /// the most common runtime error in the language and an engine that returns `undefined` instead
    /// turns every one of them into a wrong answer somewhere later.
    pub(crate) fn get_member(&mut self, base: &JsValue, key: &PropertyKey) -> Completion {
        match base {
            JsValue::Undefined | JsValue::Null => {
                let message = format!(
                    "cannot read property `{}` of {}",
                    key.to_display(),
                    if matches!(base, JsValue::Null) { "null" } else { "undefined" }
                );
                self.type_error(&message)
            }
            JsValue::Object(id) => self.get_property(*id, key),
            JsValue::String(text) => {
                if *key == PropertyKey::from_str("length") {
                    return Completion::Normal(JsValue::Number(text.len() as f64));
                }
                if let Some(index) = key.as_array_index() {
                    return Completion::Normal(match text.unit_at(index as usize) {
                        Some(unit) => JsValue::String(JsString::from_units(&[unit])),
                        None => JsValue::Undefined,
                    });
                }
                let prototype = self.intrinsics.string_prototype;
                self.get_property_with_receiver(prototype, key, base.clone())
            }
            JsValue::Number(_) => {
                let prototype = self.intrinsics.number_prototype;
                self.get_property_with_receiver(prototype, key, base.clone())
            }
            JsValue::Boolean(_) => {
                let prototype = self.intrinsics.boolean_prototype;
                self.get_property_with_receiver(prototype, key, base.clone())
            }
            JsValue::Symbol(_) => {
                let prototype = self.intrinsics.symbol_prototype;
                self.get_property_with_receiver(prototype, key, base.clone())
            }
        }
    }


    /// `ToPrimitive`, with the object protocol supplied.
    pub(crate) fn to_primitive(
        &mut self,
        value: &JsValue,
        hint: ops::Hint,
    ) -> Result<JsValue, Completion> {
        if !value.is_object() {
            return Ok(value.clone());
        }
        let exotic = match crate::iterator::get_method(
            self,
            value,
            &PropertyKey::Symbol(self.well_known_symbol("toPrimitive")),
        ) {
            Ok(method) => method,
            Err(abrupt) => return Err(abrupt),
        };
        if let Some(exotic) = exotic {
            let hint_name = match hint {
                ops::Hint::String => "string",
                ops::Hint::Number => "number",
                _ => "default",
            };
            return match self.call_value(&exotic, value.clone(), vec![JsValue::string(hint_name)]) {
                Completion::Normal(result) if !result.is_object() => Ok(result),
                Completion::Normal(_) => {
                    Err(self.type_error("Symbol.toPrimitive returned an object"))
                }
                abrupt => Err(abrupt),
            };
        }
        self.ordinary_to_primitive(value, hint)
    }

    /// `OrdinaryToPrimitive`: the `valueOf`/`toString` pair, with the hint choosing the order.
    ///
    /// IT IS SEPARATE FROM [`Interpreter::to_primitive`] BECAUSE `@@toPrimitive` HAS TO BE ABLE
    /// TO CALL IT. `Date.prototype[@@toPrimitive]` exists precisely to redefine what the DEFAULT
    /// hint asks for, and then delegates to this pair -- so a version of it that went back through
    /// `to_primitive` would find its own `@@toPrimitive` again and recurse until the stack ran out.
    pub(crate) fn ordinary_to_primitive(
        &mut self,
        value: &JsValue,
        hint: ops::Hint,
    ) -> Result<JsValue, Completion> {
        let order: [&str; 2] = match hint {
            ops::Hint::String => ["toString", "valueOf"],
            _ => ["valueOf", "toString"],
        };
        for name in order {
            let method = match self.get_member(value, &PropertyKey::from_str(name)) {
                Completion::Normal(method) => method,
                abrupt => return Err(abrupt),
            };
            if !self.is_callable(&method) {
                continue;
            }
            match self.call_value(&method, value.clone(), Vec::new()) {
                Completion::Normal(result) if !result.is_object() => return Ok(result),
                Completion::Normal(_) => continue,
                abrupt => return Err(abrupt),
            }
        }
        Err(self.type_error("cannot convert an object to a primitive value"))
    }

    /// `IsLooselyEqual` with the object cases filled in.
    ///
    /// **AN OBJECT IS COMPARED BY IDENTITY TO ANOTHER OBJECT, AND CONVERTED ONLY AGAINST A
    /// PRIMITIVE.** `{} == {}` is false however alike they are, and `null == {}` is false without
    /// any conversion happening at all -- the null/undefined pair is closed. Converting first in
    /// either case would call a user `valueOf` the standard says is never reached.
    pub(crate) fn loose_equals(
        &mut self,
        left: &JsValue,
        right: &JsValue,
    ) -> Result<bool, Completion> {
        match ops::loose_equals(left, right) {
            Ok(result) => Ok(result),
            Err(NeedsObjectProtocol) => match (left, right) {
                (JsValue::Object(a), JsValue::Object(b)) => Ok(a == b),
                (JsValue::Object(_), JsValue::Undefined | JsValue::Null)
                | (JsValue::Undefined | JsValue::Null, JsValue::Object(_)) => Ok(false),
                (JsValue::Object(_), _) => {
                    let primitive = self.to_primitive(left, ops::Hint::Default)?;
                    self.loose_equals(&primitive, right)
                }
                _ => {
                    let primitive = self.to_primitive(right, ops::Hint::Default)?;
                    self.loose_equals(left, &primitive)
                }
            },
        }
    }

    /// `HasProperty`: an own property or an inherited one, which is what `in` reports.
    ///
    /// IT TAKES `&mut self` AND CAN FAIL because a proxy's `has` trap is program code: it may
    /// throw, and it may run anything at all on the way to answering.
    pub(crate) fn has_property(
        &mut self,
        id: ObjectId,
        key: &PropertyKey,
    ) -> Result<bool, Completion> {
        if let Some(view) = self.typed_array_view(id) {
            if let Some(index) = crate::typed_array::canonical_numeric_index(key) {
                return Ok(!matches!(
                    crate::typed_array::get_element(self, &view, index),
                    JsValue::Undefined
                ));
            }
        }
        let mut current = Some(id);
        while let Some(object_id) = current {
            if self.object(object_id).proxy.is_some() {
                return crate::proxy::has_property(self, object_id, key);
            }
            let object = self.object(object_id);
            if object.own(key).is_some() {
                return Ok(true);
            }
            current = object.prototype;
        }
        Ok(false)
    }

    /// `CreateUnmappedArgumentsObject` and `CreateMappedArgumentsObject`, which differ in three
    /// things and are otherwise one construction.
    ///
    /// # IT IS NOT AN ARRAY, AND IT WAS ONE
    ///
    /// An Array with the right elements answers `length` and indexing correctly -- which is what
    /// almost every program uses `arguments` for -- and is wrong about everything else:
    /// `Array.isArray(arguments)` was true, `arguments instanceof Array` was true,
    /// `Object.prototype.toString.call(arguments)` was `"[object Array]"`, its prototype was
    /// `Array.prototype`, and `length` came out non-configurable where the standard makes it
    /// configurable and writable. **That is a deviation rather than a gap**: the object answers,
    /// and answers differently, which is the one category this tier does not permit.
    ///
    /// # THE THREE DIFFERENCES BETWEEN THE TWO KINDS
    ///
    /// A strict function, or ANY function whose parameter list is not simple, gets the UNMAPPED
    /// one: no aliasing, and a `callee` whose getter and setter are both `%ThrowTypeError%`,
    /// non-configurable. Everything else gets the MAPPED one: aliasing, and a `callee` that is the
    /// function itself, writable and configurable. The parameter list's shape is not known here --
    /// the parameters live in the artifact and are read a moment later -- so this builds the mapped
    /// shape for a sloppy call and [`Interpreter::adopt_parameter_map`] confirms or withdraws it
    /// before any parameter is bound.
    fn new_arguments_object(
        &mut self,
        values: &[JsValue],
        strict: bool,
        callee: Option<ObjectId>,
        scope: &Scope,
    ) -> ObjectId {
        let mut object = Object::new(Some(self.intrinsics.object_prototype));
        object.arguments = Some(crate::Box::new(crate::object::ArgumentsData {
            mapped: Vec::new(),
            scope: None,
        }));
        let id = self.allocate(object);
        for (index, value) in values.iter().enumerate() {
            self.object_mut(id).set_own(
                PropertyKey::from_str(&format!("{index}")),
                Property::data(value.clone()),
            );
        }
        self.object_mut(id).set_own(
            PropertyKey::from_str("length"),
            Property::builtin(JsValue::Number(values.len() as f64)),
        );
        let values_function = self.intrinsics.array_prototype_values;
        let iterator = PropertyKey::Symbol(self.well_known_symbol("iterator"));
        self.object_mut(id)
            .set_own(iterator, Property::builtin(JsValue::Object(values_function)));
        match (strict, callee) {
            (false, Some(callee)) => {
                self.object_mut(id).set_own(
                    PropertyKey::from_str("callee"),
                    Property::builtin(JsValue::Object(callee)),
                );
                if let Some(data) = self.object_mut(id).arguments.as_deref_mut() {
                    data.mapped = crate::vec![None; values.len()];
                    data.scope = Some(Rc::clone(scope));
                }
                self.pending_arguments = Some(id);
            }
            _ => self.poison_callee(id),
        }
        id
    }

    /// Replaces `callee` with the poisoned accessor pair, for an object built as mapped whose
    /// parameter list turned out not to be simple.
    fn poison_callee(&mut self, id: ObjectId) {
        let refuse = self.intrinsics.throw_type_error;
        self.object_mut(id).set_own(
            PropertyKey::from_str("callee"),
            Property {
                kind: PropertyKind::Accessor { get: Some(refuse), set: Some(refuse) },
                enumerable: false,
                configurable: false,
            },
        );
    }

    /// Settles what the `arguments` object built for this call actually is, now that the parameter
    /// list has been read.
    ///
    /// IT RUNS BEFORE ANY PARAMETER IS BOUND, and that is a requirement rather than tidiness: a
    /// non-simple list can run user code in a default, and by then the object must already be the
    /// unmapped one -- a program that reads `arguments.callee` from inside its own default
    /// parameter would otherwise see the mapped shape.
    ///
    /// ONLY THE FIRST OCCURRENCE OF A DUPLICATED NAME IS MAPPED, counting from the END.
    /// `function f(a, a) { arguments[0] = 1; return a; }` leaves the FIRST `a` unmapped, because
    /// both parameters bind one name and the later one wins the binding.
    pub(crate) fn adopt_parameter_map(&mut self, names: Option<&[String]>) {
        let Some(id) = self.pending_arguments.take() else { return };
        let Some(names) = names else {
            let Some(data) = self.object_mut(id).arguments.as_deref_mut() else { return };
            data.mapped = Vec::new();
            data.scope = None;
            self.poison_callee(id);
            return;
        };
        let Some(data) = self.object_mut(id).arguments.as_deref_mut() else { return };
        let mut seen: Vec<&String> = Vec::new();
        for (index, name) in names.iter().enumerate().rev() {
            if seen.contains(&name) {
                continue;
            }
            seen.push(name);
            if let Some(slot) = data.mapped.get_mut(index) {
                *slot = Some(name.clone());
            }
        }
    }

    /// The parameter binding an `arguments` index aliases, if it is still mapped.
    pub(crate) fn parameter_alias(&self, id: ObjectId, key: &PropertyKey) -> Option<(Scope, String)> {
        let data = self.object(id).arguments.as_deref()?;
        let scope = data.scope.as_ref()?;
        let index = key.as_array_index()? as usize;
        let name = data.mapped.get(index)?.clone()?;
        Some((Rc::clone(scope), name))
    }

    /// Reads an aliased parameter: the map's getter, without the function object the standard
    /// would have built to hold it.
    pub(crate) fn read_parameter_alias(&self, scope: &Scope, name: &str) -> JsValue {
        scope.borrow().bindings.get(name).map(|binding| binding.value.clone()).unwrap_or(JsValue::Undefined)
    }

    /// Writes an aliased parameter, which is how `arguments[0] = v` changes the first parameter.
    pub(crate) fn write_parameter_alias(&mut self, scope: &Scope, name: &str, value: JsValue) {
        if let Some(binding) = scope.borrow_mut().bindings.get_mut(name) {
            binding.value = value;
        }
    }

    /// Removes an index from the `[[ParameterMap]]`, which is what breaks the alias for good.
    ///
    /// Deleting the property, making it non-writable, or redefining it as an ACCESSOR all do
    /// this -- three different programs, one consequence, and each of them is a rule a Test262 file
    /// checks on its own.
    pub(crate) fn unmap_parameter(&mut self, id: ObjectId, key: &PropertyKey) {
        let Some(index) = key.as_array_index().map(|index| index as usize) else { return };
        let Some(data) = self.object_mut(id).arguments.as_deref_mut() else { return };
        if let Some(slot) = data.mapped.get_mut(index) {
            *slot = None;
        }
    }

    /// `OrdinarySetPrototypeOf`: whether the object now has that prototype.
    ///
    /// A PROTOTYPE CYCLE MUST BE REFUSED BEFORE IT IS CREATED. `set-failure-cycle.js` builds one
    /// on purpose, and every later chain walk -- `[[Get]]`, `in`, `instanceof` -- follows it
    /// forever; the conformance watchdog stopped the process at 30 seconds twice, which is the
    /// report noticing what the engine should have refused.
    ///
    /// AND SETTING THE PROTOTYPE IT ALREADY HAS SUCCEEDS EVEN ON A FROZEN OBJECT. That is the
    /// standard's first step and it is not a nicety: `Object.setPrototypeOf(o, Object.getPrototypeOf(o))`
    /// is how a program asserts a prototype it believes is already there, and refusing it turns a
    /// no-op into a TypeError on every sealed object.
    pub(crate) fn set_prototype_of(
        &mut self,
        id: ObjectId,
        prototype: Option<ObjectId>,
    ) -> Result<bool, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::set_prototype_of(self, id, prototype);
        }
        if self.object(id).prototype == prototype {
            return Ok(true);
        }
        if !self.object(id).extensible {
            return Ok(false);
        }
        let mut walk = prototype;
        while let Some(step) = walk {
            if step == id {
                return Ok(false);
            }
            if self.object(step).proxy.is_some() {
                break;
            }
            walk = self.object(step).prototype;
        }
        self.object_mut(id).prototype = prototype;
        Ok(true)
    }

    /// `[[GetPrototypeOf]]`, which for a proxy is the `getPrototypeOf` trap.
    ///
    /// AN ORDINARY OBJECT'S PROTOTYPE IS A FIELD AND EVERY INTERNAL CHAIN WALK STILL READS IT
    /// DIRECTLY. This is the operation a PROGRAM reaches -- `Object.getPrototypeOf`,
    /// `Reflect.getPrototypeOf`, `__proto__`, `instanceof` -- and it exists separately because those
    /// must consult a handler where a chain walk must hand the whole operation over instead.
    pub(crate) fn get_prototype_of(
        &mut self,
        id: ObjectId,
    ) -> Result<Option<ObjectId>, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::get_prototype_of(self, id);
        }
        Ok(self.object(id).prototype)
    }

    /// `IsArray` (7.2.2), which is the one question that FOLLOWS a proxy instead of trapping it.
    ///
    /// # `Array.isArray(new Proxy([], {}))` IS TRUE, AND THERE IS NO TRAP FOR IT
    ///
    /// Every other question about a proxy goes to the handler. This one recurses into
    /// `[[ProxyTarget]]` -- a proxy for an array IS an array to `Array.isArray`, to
    /// `Object.prototype.toString`, to `JSON.stringify` and to `concat`'s spreadable test. A handler
    /// cannot make it answer otherwise, which is why reading the `is_array` FLAG on the proxy gets
    /// it wrong in the direction that matters: the flag is false, so a proxied array serialized as
    /// an object and concatenated as a single element.
    ///
    /// A REVOKED PROXY IS A TypeError HERE, which is why this answers a `Result`. It is the only
    /// operation on this list that can fail without calling anything.
    pub(crate) fn is_array(&mut self, id: ObjectId) -> Result<bool, Completion> {
        if self.object(id).is_array {
            return Ok(true);
        }
        if self.object(id).proxy.is_some() {
            let target = crate::proxy::target_of(self, id)?;
            return self.is_array(target);
        }
        Ok(false)
    }

    /// `IsExtensible`.
    pub(crate) fn is_extensible(&mut self, id: ObjectId) -> Result<bool, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::is_extensible(self, id);
        }
        Ok(self.object(id).extensible)
    }

    /// `[[PreventExtensions]]`, which for an ordinary object always succeeds.
    pub(crate) fn prevent_extensions(&mut self, id: ObjectId) -> Result<bool, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::prevent_extensions(self, id);
        }
        self.object_mut(id).extensible = false;
        Ok(true)
    }

    /// `[[GetOwnProperty]]`, which for a typed array's indices is synthesized from the buffer.
    ///
    /// Every element descriptor is `{writable, enumerable, configurable}` all true -- an array
    /// whose elements are `configurable: false` is what an earlier reading of the standard said,
    /// and `Object.freeze(ta)` turns on the difference.
    pub(crate) fn own_property(
        &mut self,
        id: ObjectId,
        key: &PropertyKey,
    ) -> Result<Option<Property>, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::own_property(self, id, key);
        }
        if let Some(view) = self.typed_array_view(id) {
            if let Some(index) = crate::typed_array::canonical_numeric_index(key) {
                let value = crate::typed_array::get_element(self, &view, index);
                if matches!(value, JsValue::Undefined) {
                    return Ok(None);
                }
                return Ok(Some(Property::data(value)));
            }
        }
        let Some(property) = self.object(id).own(key).cloned() else { return Ok(None) };
        if let Some((scope, name)) = self.parameter_alias(id, key) {
            let value = self.read_parameter_alias(&scope, &name);
            return Ok(Some(Property {
                kind: PropertyKind::Data { value, writable: true },
                ..property
            }));
        }
        Ok(Some(property))
    }

    /// `[[OwnPropertyKeys]]`, which for a typed array puts every live index first.
    ///
    /// EVERY ENUMERATION GOES THROUGH THESE THREE AND NOT THROUGH `Object`'s OWN VERSIONS, and
    /// the difference is invisible until an exotic object turns up: a typed array's indices are
    /// BYTES IN A BUFFER rather than entries in the property store, so `Object` cannot list them and
    /// answers with whatever ordinary properties happen to be there -- which for a fresh
    /// `new Int8Array(4)` is nothing at all.
    pub(crate) fn own_keys_of(&mut self, id: ObjectId) -> Result<Vec<PropertyKey>, Completion> {
        if self.object(id).proxy.is_some() {
            return crate::proxy::own_keys(self, id);
        }
        Ok(match self.typed_array_view(id) {
            Some(view) => crate::typed_array::own_keys(self, id, &view),
            None => self.object(id).own_keys(),
        })
    }

    pub(crate) fn own_string_keys_of(
        &mut self,
        id: ObjectId,
    ) -> Result<Vec<PropertyKey>, Completion> {
        Ok(self.own_keys_of(id)?.into_iter().filter(|key| !key.is_symbol()).collect())
    }

    /// The symbol half of `GetOwnPropertyKeys(O, symbol)`, and the exact mirror of the string half
    /// above it.
    ///
    /// IT EXISTS SO THAT `Object.getOwnPropertySymbols` DOES NOT READ THE PROPERTY STORE DIRECTLY.
    /// Both halves of one named operation begin `Let keys be ? O.[[OwnPropertyKeys]]()` and then
    /// filter by key type, so the symbol half has to go through the object's key protocol exactly
    /// as the string half does. A PROXY is the whole difference: reaching past it into
    /// `own_symbol_keys()` makes `Object.getOwnPropertyNames(new Proxy(o, {}))` run the trap while
    /// `Object.getOwnPropertySymbols(new Proxy(o, {}))` answers an EMPTY ARRAY -- for a transparent
    /// proxy, over an object that has symbol keys.
    pub(crate) fn own_symbol_keys_of(
        &mut self,
        id: ObjectId,
    ) -> Result<Vec<PropertyKey>, Completion> {
        Ok(self.own_keys_of(id)?.into_iter().filter(PropertyKey::is_symbol).collect())
    }

    pub(crate) fn enumerable_keys_of(
        &mut self,
        id: ObjectId,
    ) -> Result<Vec<PropertyKey>, Completion> {
        let keys = self.own_string_keys_of(id)?;
        self.retain_enumerable(id, keys)
    }

    /// Enumerable own keys of BOTH kinds, which only `Object.assign` wants.
    ///
    /// `Object.keys`, `for-in`, `getOwnPropertyNames` and `JSON.stringify` are all STRING-key
    /// operations and must keep using [`Self::enumerable_keys_of`]; a symbol appearing in any of
    /// them would defeat the point of symbol keys. `assign` is the one that copies everything
    /// enumerable, symbols included, and it was using the string-only list.
    pub(crate) fn enumerable_own_keys(
        &mut self,
        id: ObjectId,
    ) -> Result<Vec<PropertyKey>, Completion> {
        let keys = self.own_keys_of(id)?;
        self.retain_enumerable(id, keys)
    }

    /// Keeps the keys whose `[[GetOwnProperty]]` reports them enumerable.
    ///
    /// IT IS A LOOP RATHER THAN A `filter` BECAUSE THE TEST CAN THROW AND CAN RUN CODE. On a
    /// proxy each of these is a `getOwnPropertyDescriptor` trap call, so a key listed by `ownKeys`
    /// may be reported as absent by the time it is asked about -- which the standard allows, and
    /// which a closure returning `bool` has no way to report.
    fn retain_enumerable(
        &mut self,
        id: ObjectId,
        keys: Vec<PropertyKey>,
    ) -> Result<Vec<PropertyKey>, Completion> {
        let mut kept = Vec::with_capacity(keys.len());
        for key in keys {
            if self.own_property(id, &key)?.is_some_and(|property| property.enumerable) {
                kept.push(key);
            }
        }
        Ok(kept)
    }

    /// `ToPropertyKey` on an arbitrary value, using the object protocol when it meets one.
    pub(crate) fn to_property_key_value(
        &mut self,
        value: &JsValue,
    ) -> Result<PropertyKey, Completion> {
        let primitive = self.to_primitive(value, ops::Hint::String)?;
        if let JsValue::Symbol(id) = primitive {
            return Ok(PropertyKey::Symbol(id));
        }
        Ok(PropertyKey::String(ops::to_string(&primitive).map_err(|NeedsObjectProtocol| {
            self.type_error("cannot convert this value to a property key")
        })?))
    }

    pub(crate) fn to_number_value(&mut self, value: &JsValue) -> Result<f64, Completion> {
        let primitive = self.to_primitive(value, ops::Hint::Number)?;
        ops::to_number(&primitive).map_err(|NeedsObjectProtocol| {
            self.type_error("cannot convert this value to a number")
        })
    }

    pub(crate) fn to_string_value(&mut self, value: &JsValue) -> Result<JsString, Completion> {
        let primitive = self.to_primitive(value, ops::Hint::String)?;
        ops::to_string(&primitive).map_err(|NeedsObjectProtocol| {
            self.type_error("cannot convert this value to a string")
        })
    }

    /// `ToObject`: the wrapper a primitive is boxed in when one is genuinely required.
    pub(crate) fn to_object(&mut self, value: &JsValue) -> Result<ObjectId, Completion> {
        match value {
            JsValue::Object(id) => Ok(*id),
            JsValue::Undefined | JsValue::Null => {
                Err(self.type_error("cannot convert null or undefined to an object"))
            }
            primitive => {
                let prototype = match primitive {
                    JsValue::String(_) => self.intrinsics.string_prototype,
                    JsValue::Number(_) => self.intrinsics.number_prototype,
                    JsValue::Boolean(_) => self.intrinsics.boolean_prototype,
                    JsValue::Symbol(_) => self.intrinsics.symbol_prototype,
                    JsValue::Undefined | JsValue::Null | JsValue::Object(_) => {
                        unreachable!("taken by the arms above")
                    }
                };
                let mut object = Object::new(Some(prototype));
                object.primitive = Some(primitive.clone());
                let id = self.allocate(object);
                if let JsValue::String(text) = primitive {
                    for (index, unit) in text.units().iter().enumerate() {
                        self.object_mut(id).set_own(
                            PropertyKey::from_str(&index.to_string()),
                            Property { kind: PropertyKind::Data { value: JsValue::String(JsString::from_units(&[*unit])), writable: false }, enumerable: true, configurable: false },
                        );
                    }
                    let length = text.len();
                    self.object_mut(id).set_own(
                        PropertyKey::from_str("length"),
                        Property { kind: PropertyKind::Data { value: JsValue::Number(length as f64), writable: false }, enumerable: false, configurable: false },
                    );
                }
                Ok(id)
            }
        }
    }

    fn unary(&mut self, operator: UnaryOperator, value: &JsValue) -> Completion {
        match operator {
            UnaryOperator::TypeOf => {
                if self.is_callable(value) {
                    return Completion::Normal(JsValue::string("function"));
                }
                Completion::Normal(JsValue::string(value.type_of()))
            }
            UnaryOperator::Void => Completion::Normal(JsValue::Undefined),
            UnaryOperator::Not => Completion::Normal(JsValue::Boolean(!ops::to_boolean(value))),
            UnaryOperator::Minus | UnaryOperator::Plus | UnaryOperator::BitwiseNot => {
                let number = match self.to_number_value(value) {
                    Ok(number) => number,
                    Err(abrupt) => return abrupt,
                };
                Completion::Normal(JsValue::Number(match operator {
                    UnaryOperator::Minus => -number,
                    UnaryOperator::Plus => number,
                    _ => f64::from(!ops::to_int32(number)),
                }))
            }
            UnaryOperator::Delete => Completion::Normal(JsValue::Boolean(true)),
            UnaryOperator::Await => {
                self.refuse(Absence::Await)
            }
        }
    }


    fn binary(&mut self, operator: BinaryOperator, left: &JsValue, right: &JsValue) -> Completion {
        use BinaryOperator as B;

        if operator == B::Add {
            let a = match self.to_primitive(left, ops::Hint::Default) {
                Ok(value) => value,
                Err(abrupt) => return abrupt,
            };
            let b = match self.to_primitive(right, ops::Hint::Default) {
                Ok(value) => value,
                Err(abrupt) => return abrupt,
            };
            if matches!(a, JsValue::String(_)) || matches!(b, JsValue::String(_)) {
                let a = match self.to_string_value(&a) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                };
                let b = match self.to_string_value(&b) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                };
                let mut out = a;
                out.extend_from(&b);
                return Completion::Normal(JsValue::String(out));
            }
            let a = match self.to_number_value(&a) {
                Ok(number) => number,
                Err(abrupt) => return abrupt,
            };
            let b = match self.to_number_value(&b) {
                Ok(number) => number,
                Err(abrupt) => return abrupt,
            };
            return Completion::Normal(JsValue::Number(a + b));
        }

        match operator {
            B::StrictEqual => {
                return Completion::Normal(JsValue::Boolean(ops::strict_equals(left, right)))
            }
            B::StrictNotEqual => {
                return Completion::Normal(JsValue::Boolean(!ops::strict_equals(left, right)))
            }
            B::Equal | B::NotEqual => {
                let equal = match self.loose_equals(left, right) {
                    Ok(result) => result,
                    Err(abrupt) => return abrupt,
                };
                let result = if operator == B::Equal { equal } else { !equal };
                return Completion::Normal(JsValue::Boolean(result));
            }
            B::Less | B::Greater | B::LessEqual | B::GreaterEqual => {
                let left = match self.to_primitive(left, ops::Hint::Number) {
                    Ok(value) => value,
                    Err(abrupt) => return abrupt,
                };
                let right = match self.to_primitive(right, ops::Hint::Number) {
                    Ok(value) => value,
                    Err(abrupt) => return abrupt,
                };
                let (a, b, negate) = match operator {
                    B::Less => (&left, &right, false),
                    B::Greater => (&right, &left, false),
                    B::LessEqual => (&right, &left, true),
                    _ => (&left, &right, true),
                };
                return match ops::less_than(a, b) {
                    Ok(Comparison::Undefined) => Completion::Normal(JsValue::Boolean(false)),
                    Ok(Comparison::True) => Completion::Normal(JsValue::Boolean(!negate)),
                    Ok(Comparison::False) => Completion::Normal(JsValue::Boolean(negate)),
                    Err(NeedsObjectProtocol) => {
                        self.type_error("a relational comparison needs a primitive")
                    }
                };
            }
            B::In => {
                let JsValue::Object(id) = right else {
                    return self.type_error("the right operand of `in` must be an object");
                };
                let id = *id;
                let key = match self.to_property_key_value(left) {
                    Ok(key) => key,
                    Err(abrupt) => return abrupt,
                };
                return match self.has_property(id, &key) {
                    Ok(has) => Completion::Normal(JsValue::Boolean(has)),
                    Err(abrupt) => abrupt,
                };
            }
            B::InstanceOf => return self.instanceof_operator(left, right),
            _ => {}
        }

        let a = match self.to_number_value(left) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        let b = match self.to_number_value(right) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        let result = match operator {
            B::Subtract => a - b,
            B::Multiply => a * b,
            B::Divide => a / b,
            B::Remainder => ops::remainder(a, b),
            B::Exponent => ops::exponentiate(a, b),
            B::BitwiseAnd => f64::from(ops::to_int32(a) & ops::to_int32(b)),
            B::BitwiseOr => f64::from(ops::to_int32(a) | ops::to_int32(b)),
            B::BitwiseXor => f64::from(ops::to_int32(a) ^ ops::to_int32(b)),
            B::ShiftLeft => f64::from(ops::to_int32(a) << (ops::to_uint32(b) & 31)),
            B::ShiftRight => f64::from(ops::to_int32(a) >> (ops::to_uint32(b) & 31)),
            B::UnsignedShiftRight => f64::from(ops::to_uint32(a) >> (ops::to_uint32(b) & 31)),
            _ => return self.type_error("this operator is not a numeric operation"),
        };
        Completion::Normal(JsValue::Number(result))
    }
}

/// How a call expression's callee is written, for a diagnostic that names it.
///
/// It reconstructs only the shapes a reader would recognise and says `an expression` for the rest.
/// A half-rendered expression in an error message is worse than an honest placeholder, because the
/// reader takes it for the source text and looks for something that is not there.
fn callee_name(callee: &Expression) -> String {
    match callee {
        Expression::Identifier { name, .. } => name.clone(),
        Expression::Member { object, property, .. } => {
            let base = callee_name(object);
            match &**property {
                MemberProperty::Identifier { name, .. } => format!("{base}.{name}"),
                MemberProperty::Private { name, .. } => format!("{base}.#{name}"),
                MemberProperty::Computed { .. } => format!("{base}[...]"),
            }
        }
        Expression::Parenthesized { expression, .. } => callee_name(expression),
        Expression::Call { callee, .. } => format!("{}(...)", callee_name(callee)),
        Expression::This { .. } => "this".to_string(),
        _ => "an expression".to_string(),
    }
}



/// Which of the three LOGICAL assignments this is, if it is one.
///
/// THEY ARE NOT `binary()` WITH AN OPERATOR, WHICH IS WHY THEY ARE NOT IN THE TABLE BELOW.
/// `a &&= b` is not `a = a && b`: it **SHORT-CIRCUITS THE ASSIGNMENT ITSELF**, so when the test
/// fails the right-hand side is never evaluated AND NO WRITE HAPPENS AT ALL. That distinction is
/// observable through a setter -- `o.x ||= 1` with a truthy `o.x` must not call the setter -- and
/// through a frozen property, where the desugared form throws in strict mode and the real one does
/// not. It is also why the value assigned is the right-hand side itself rather than the result of
/// an operation on both.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogicalAssignment {
    And,
    Or,
    Nullish,
}

impl LogicalAssignment {
    /// Whether the right-hand side runs and the write happens, given what the target already holds.
    fn takes_the_right_side(self, current: &JsValue) -> bool {
        match self {
            LogicalAssignment::And => ops::to_boolean(current),
            LogicalAssignment::Or => !ops::to_boolean(current),
            LogicalAssignment::Nullish => matches!(current, JsValue::Undefined | JsValue::Null),
        }
    }
}

pub(crate) fn logical_assignment(operator: AssignmentOperator) -> Option<LogicalAssignment> {
    match operator {
        AssignmentOperator::LogicalAndAssign => Some(LogicalAssignment::And),
        AssignmentOperator::LogicalOrAssign => Some(LogicalAssignment::Or),
        AssignmentOperator::NullishAssign => Some(LogicalAssignment::Nullish),
        _ => None,
    }
}

fn compound_operator(operator: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    use BinaryOperator as B;
    Some(match operator {
        A::AddAssign => B::Add,
        A::SubtractAssign => B::Subtract,
        A::MultiplyAssign => B::Multiply,
        A::DivideAssign => B::Divide,
        A::RemainderAssign => B::Remainder,
        A::ExponentAssign => B::Exponent,
        A::ShiftLeftAssign => B::ShiftLeft,
        A::ShiftRightAssign => B::ShiftRight,
        A::UnsignedShiftRightAssign => B::UnsignedShiftRight,
        A::BitwiseAndAssign => B::BitwiseAnd,
        A::BitwiseOrAssign => B::BitwiseOr,
        A::BitwiseXorAssign => B::BitwiseXor,
        _ => return None,
    })
}







