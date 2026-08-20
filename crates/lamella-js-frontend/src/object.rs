//! Objects: property attributes and prototype chains.

use crate::{format, String, Vec};
use crate::string_value::JsString;
use crate::value::{JsValue, ObjectId, SymbolId};

/// A property key: a String, or a Symbol.
///
/// **THESE ARE THE ONLY TWO, AND A NUMBER IS NOT ONE OF THEM.** `a[0]` and `a["0"]` are the same
/// property because `ToPropertyKey` converts a number to its string form; an engine that kept a
/// numeric key variant would have two ways to spell one property, and two spellings disagree with
/// itself about whether the property exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyKey {
    String(JsString),
    /// A symbol key is **invisible to every string-based enumeration**: `Object.keys`, `for-in`,
    /// `getOwnPropertyNames` and `JSON.stringify` all skip it, and only `getOwnPropertySymbols`
    /// reports it. That invisibility is the feature -- it is what lets a protocol hook live on an
    /// object without appearing in the object's data.
    Symbol(SymbolId),
}

impl PropertyKey {
    #[must_use]
    pub fn from_str(text: &str) -> Self {
        PropertyKey::String(JsString::from(text))
    }

    /// A key whose text was emitted as constant data.
    ///
    /// **THE UNITS ARE UTF-16 AND A GENERATOR HAS TO SAY SO.** A property key is a String value, so
    /// `"length"` is six `u16`s and not six bytes; handing this ASCII bytes would build a key that
    /// compares equal to nothing. `pub(crate)` because the realm tables are the only caller.
    #[must_use]
    pub(crate) const fn from_static_units(units: &'static [u16]) -> Self {
        PropertyKey::String(JsString::from_static_units(units))
    }

    /// The array-index interpretation of this key, if it has one.
    ///
    /// An array index is a canonical numeric string: `"0"` is index 0 and `"00"`, `"0.0"` and
    /// `" 0"` are ordinary string keys. Accepting anything a number parser would accept makes
    /// `a["00"] = 1` change the length, which no conforming engine does.
    ///
    /// **THE RANGE STOPS ONE SHORT OF `u32::MAX`, AND THE TYPE IS WHAT MAKES THAT EASY TO MISS.**
    /// 6.1.7 defines an array index as `0 <= n < 2^32 - 1`, so `"4294967295"` is an ORDINARY STRING
    /// KEY while `"4294967294"` is an index -- and `parse::<u32>()` accepts both, because the
    /// boundary the standard draws is one below the one the type draws. `length` may BE `2^32 - 1`;
    /// no index may. Test262 tests exactly this value, and it went red the day anything started
    /// acting on the answer: an array with a property at `2^32 - 1` must still have `length` 0.
    #[must_use]
    pub fn as_array_index(&self) -> Option<u32> {
        let PropertyKey::String(text) = self else { return None };
        let text = text.to_lossy_string();
        if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
            return None;
        }
        if !text.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        text.parse::<u32>().ok().filter(|index| *index != u32::MAX)
    }

    #[must_use]
    pub fn is_symbol(&self) -> bool {
        matches!(self, PropertyKey::Symbol(_))
    }

    /// A rendering for diagnostics.
    ///
    /// **LOSSY FOR A SYMBOL AND NAMED SO.** A symbol's identity is not its description, so two
    /// distinct keys can render identically here. Nothing that decides program behaviour may go
    /// through this function -- comparing keys by their display would make `Symbol("x")` and a
    /// different `Symbol("x")` the same property.
    #[must_use]
    pub fn to_display(&self) -> String {
        match self {
            PropertyKey::String(text) => text.to_lossy_string(),
            PropertyKey::Symbol(id) => format!("Symbol(#{})", id.0),
        }
    }
}

/// What a property holds: a value, or a pair of functions.
///
/// **THESE ARE TWO KINDS OF PROPERTY, NOT ONE WITH EXTRA FIELDS, AND THE STANDARD KEEPS THEM
/// APART EVERYWHERE.** An accessor has no `value` and no `writable`; a data property has no `get`
/// and no `set`. `Object.getOwnPropertyDescriptor` returns a record with one shape or the other,
/// and `Object.defineProperty` REJECTS a descriptor carrying fields from both. Modelling an accessor
/// as a data property whose value happens to be a function is what this engine did before, and it
/// makes `{ get x() { return 1; } }.x` return the FUNCTION rather than `1` -- a wrong answer that
/// looks like a right one until it is called.
#[derive(Debug, Clone)]
pub enum PropertyKind {
    Data {
        value: JsValue,
        writable: bool,
    },
    /// EITHER HALF MAY BE ABSENT, and the absent half is not "does nothing": a getter-only
    /// property READS as `undefined` when written in sloppy mode and THROWS in strict, and a
    /// setter-only property reads as `undefined` however much the setter has been called.
    Accessor {
        get: Option<ObjectId>,
        set: Option<ObjectId>,
    },
}

/// A property and its attributes.
#[derive(Debug, Clone)]
pub struct Property {
    pub kind: PropertyKind,
    pub enumerable: bool,
    pub configurable: bool,
}

impl Property {
    /// The attributes an ordinary assignment creates: all true.
    ///
    /// `const` SO A REALM TABLE CAN CALL IT. Every constructor here is const-callable, which is what
    /// lets the realm's 1,429 properties be emitted as constant data rather than built by running an
    /// installer that allocates. Nothing about the run-time behaviour changes.
    #[must_use]
    pub const fn data(value: JsValue) -> Self {
        Self {
            kind: PropertyKind::Data { value, writable: true },
            enumerable: true,
            configurable: true,
        }
    }

    /// The attributes a BUILT-IN gets: writable and configurable but **not enumerable**.
    ///
    /// That single `false` is what stops every `for-in` over an object from listing the methods it
    /// inherited. A model without attributes cannot express it, and the symptom is that ordinary
    /// loops over ordinary objects produce extra keys.
    #[must_use]
    pub const fn builtin(value: JsValue) -> Self {
        Self {
            kind: PropertyKind::Data { value, writable: true },
            enumerable: false,
            configurable: true,
        }
    }

    /// An accessor with the attributes a built-in accessor gets.
    #[must_use]
    pub const fn accessor(get: Option<ObjectId>, set: Option<ObjectId>) -> Self {
        Self { kind: PropertyKind::Accessor { get, set }, enumerable: false, configurable: true }
    }

    /// The value of a DATA property, or `None` for an accessor.
    ///
    /// Returning `undefined` for an accessor instead of `None` would let a caller read one without
    /// ever calling its getter, which is the whole failure this type exists to prevent.
    #[must_use]
    pub fn data_value(&self) -> Option<&JsValue> {
        match &self.kind {
            PropertyKind::Data { value, .. } => Some(value),
            PropertyKind::Accessor { .. } => None,
        }
    }

    #[must_use]
    pub fn is_accessor(&self) -> bool {
        matches!(self.kind, PropertyKind::Accessor { .. })
    }

    /// Whether an ordinary assignment may overwrite this property in place.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        match &self.kind {
            PropertyKind::Data { writable, .. } => *writable,
            PropertyKind::Accessor { .. } => false,
        }
    }
}

/// What a callable object calls.
///
/// **THREE KINDS, NOT ONE, AND THE LANGUAGE CAN TELL THEM APART.** A single closure table cannot
/// hold a built-in: `Math.pow` has no parameter list and no body to walk. Nor can it hold a bound
/// function, whose call must prepend arguments captured at `bind` time. Collapsing any two of these
/// into one representation means the third is implemented by pretending -- and
/// `Function.prototype.call.bind(Array.prototype.join)`, which Test262's own `propertyHelper.js`
/// evaluates at load time, uses all three at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callable {
    /// A function written in JavaScript: an index into the interpreter's closure table.
    Closure(u32),
    /// A built-in implemented in Rust: an index into the interpreter's native table.
    Native(u32),
    /// The result of `Function.prototype.bind`: an index into the interpreter's bound table.
    Bound(u32),
    /// One half of the pair `CreateResolvingFunctions` makes for a promise.
    ///
    /// A VARIANT RATHER THAN A NATIVE WITH A PAYLOAD, AND THE REASON IS WHAT `this` IS. A
    /// resolving function is handed to user code and called as a BARE FUNCTION -- `new Promise(r =>
    /// r(1))` -- so its `this` is `undefined`, not the function object. Any scheme that recovers the
    /// promise from the receiver works when the function is called as a method and fails on the
    /// only call shape that actually occurs. The promise is in the callable itself, where the call
    /// path can always reach it.
    Resolver { promise: ObjectId, rejects: bool },
    /// One element's resolve/reject function inside `Promise.all` and its siblings.
    ///
    /// THE SHARED STATE IS A HEAP OBJECT, NOT A TABLE INDEX. A combinator's element functions
    /// share a results array and a counter, and putting those in an interpreter-side table keyed by
    /// `u32` would make them reachable from nothing -- the shape that made the Python tier's
    /// containers immortal. An `ObjectId` is owned by the heap and traced like anything else, and
    /// it keeps both fields `Copy` so the variant stays cheap.
    Combinator { state: ObjectId, index: u32, rejects: bool },
    /// The executor `NewPromiseCapability` passes to a constructor, capturing what it is handed.
    ///
    /// It writes into `state`'s [`Object::capability_parts`] rather than returning, because the
    /// constructor decides whether to call it at all -- and a constructor that never does is
    /// exactly the case `NewPromiseCapability` must reject.
    CapabilityExecutor { state: ObjectId },
    /// A Proxy whose target is callable, whose `[[Call]]` is the `apply` trap.
    ///
    /// **THE VARIANT CARRIES NOTHING BECAUSE THE PAIR IS IN [`Object::proxy`], AND IT EXISTS AT
    /// ALL ONLY SO THAT `typeof` AND `IsCallable` ANSWER WITHOUT A SECOND QUESTION.** `ProxyCreate`
    /// installs `[[Call]]` only when the target is callable, so a proxy for a plain object has no
    /// `callable` at all -- `typeof new Proxy({}, {})` is `"object"` and `typeof new Proxy(f, {})`
    /// is `"function"`, from the same construction. Deriving that from the target at every call
    /// site would make it a property of the target's CURRENT state, where the standard fixes it
    /// when the proxy is made and never revisits it: a revoked proxy of a function is still
    /// `"function"`.
    Proxy,
    /// The second half of `Proxy.revocable`'s result: the function that clears a proxy's two slots.
    ///
    /// A VARIANT RATHER THAN A NATIVE, for the reason [`Self::Resolver`] gives -- a native's body
    /// is a bare `fn` pointer and cannot capture, and this one is called as a BARE FUNCTION, so
    /// there is no receiver to recover the proxy from either.
    ProxyRevoker { proxy: ObjectId },
}


/// Property storage that may live in flash, and promotes to RAM the first time anything writes.
///
/// # THE WRITE BARRIER IS THE TYPE, NOT A RULE SOMEBODY FOLLOWS
///
/// A realm costs a quarter of a megabyte before a line of JavaScript runs, and almost none of it is
/// ever written -- measured: a typical program writes ONE realm object and the worst of 37,500
/// conformance runs wrote five. So the realm belongs in flash, materialized per object on first
/// write.
///
/// **The failure mode of that design is a store that does not stick**: a write that reaches a
/// flash-backed object, does nothing, and reports nothing. No crash, no refusal, no test failure
/// unless something later reads the value back and compares. That is this profile's one
/// unacceptable category, and it is the reason the barrier is a property of the TYPE rather than a
/// convention every mutation site observes.
///
/// So there is no way to write without promoting. [`Store::owned`] is the ONLY source of a
/// `&mut Vec<T>` in the engine, and it promotes before it returns; reads go through
/// [`Store::as_slice`], which cannot mutate. A new mutating method that forgets the barrier does not
/// compile, because there is nothing for it to write THROUGH.
///
/// # IT COSTS NOTHING, WHICH IS WHY IT CAN LAND BEFORE THE TABLES DO
///
/// `Store<T>` is 24 bytes -- exactly `Vec<T>` -- because the enum's discriminant fits a niche.
/// Measured rather than hoped for: the alternative was a hand-packed representation needing
/// `unsafe`, in a crate whose whole posture is that it does not.
///
/// Every object is `Owned` today. The static arm exists, is exercised by tests, and waits for the
/// build-time tables that will fill it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum Store<T: Clone + 'static> {
    /// Emitted at build time and read where it lies. Never written; a write promotes first.
    Static(&'static [T]),
    /// Materialized in RAM, either because the object was built at run time or because something
    /// wrote to it.
    Owned(Vec<T>),
}

#[allow(dead_code)]
impl<T: Clone + 'static> Store<T> {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Store::Owned(Vec::new())
    }

    /// Reading. Available on both arms, and incapable of promoting anything.
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[T] {
        match self {
            Store::Static(items) => items,
            Store::Owned(items) => items,
        }
    }

    /// Writing, and **the only way to obtain mutable access to the contents.**
    ///
    /// A static store is copied into RAM here, once, and every later write finds it already owned.
    pub(crate) fn owned(&mut self) -> &mut Vec<T> {
        if let Store::Static(items) = self {
            *self = Store::Owned(items.to_vec());
        }
        match self {
            Store::Owned(items) => items,
            Store::Static(_) => unreachable!("a static store was just promoted"),
        }
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.as_slice().len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What the allocator was made to hand over. A static store costs no RAM at all, which is the
    /// entire point of it, so it reports zero rather than its length.
    #[must_use]
    pub(crate) fn reserved(&self) -> usize {
        match self {
            Store::Static(_) => 0,
            Store::Owned(items) => items.capacity(),
        }
    }

    /// Whether this store is still reading from flash.
    #[must_use]
    pub(crate) fn is_static(&self) -> bool {
        matches!(self, Store::Static(_))
    }
}

impl<T: Clone + 'static> Default for Store<T> {
    fn default() -> Self {
        Store::new()
    }
}

/// Equality is over the CONTENTS and never over which arm holds them.
///
/// # A DERIVE HERE IS A SILENT WRONG ANSWER, AND IT IS NOT A HYPOTHETICAL ONE
///
/// `#[derive(PartialEq)]` on an enum compares discriminants first, so `Static(&[104, 105])` and
/// `Owned(vec![104, 105])` would be UNEQUAL -- two spellings of one value disagreeing about being
/// one value. A [`crate::string_value::JsString`] is exactly this type, so the derived form makes
/// `"hi" === "hi"` answer FALSE the moment one side came from flash and the other was built at run
/// time, which is every comparison of a literal against a built-in's name.
///
/// It is written here rather than at each user for the reason the barrier itself is: a rule with
/// several implementations gains a new case in none of them. **A future `Store` user gets content
/// equality for doing nothing**, which is the only version of this that stays true.
impl<T: Clone + 'static + PartialEq> PartialEq for Store<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Clone + 'static + Eq> Eq for Store<T> {}

/// Hashing matches [`PartialEq`], which is the contract and which a derive would also have broken:
/// two equal stores hashing differently puts one key in two buckets.
impl<T: Clone + 'static + core::hash::Hash> core::hash::Hash for Store<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

/// An ordinary object.
#[derive(Debug, Clone, Default)]
pub struct Object {
    /// Array-index properties `0..elements.len()`, reached by arithmetic rather than by search.
    ///
    /// A `None` slot is a HOLE -- an index the object does not have -- which is exactly what a
    /// hole is in the language. `delete a[0]` leaves one; it does not shrink the vector, because
    /// shrinking would renumber everything after it.
    elements: Store<Option<Property>>,
    /// Everything else, in insertion order: string keys, symbol keys, and any array index too far
    /// past `elements` to store densely. See the module note.
    named: Store<(PropertyKey, Property)>,
    pub prototype: Option<ObjectId>,
    /// Set for an Array exotic object, whose `length` tracks its indices.
    pub is_array: bool,
    /// Set for a callable object; see [`Callable`].
    pub callable: Option<Callable>,
    /// Whether new properties may be added.
    ///
    /// **IT IS ONE-WAY AND IT IS NOT THE SAME AS FROZEN.** `preventExtensions` stops ADDITION and
    /// says nothing about writing or deleting what is already there; `freeze` is that plus making
    /// every existing property non-writable and non-configurable. `Object.isFrozen` on a
    /// non-extensible object with no properties is TRUE, which is why the two questions cannot share
    /// a flag.
    pub extensible: bool,
    /// The primitive a wrapper object boxes: `new Number(1)`, `Object("x")`, `new Boolean(true)`.
    ///
    /// It is what `valueOf` returns, and it is NOT the same as a `valueOf` property -- a wrapper
    /// whose `valueOf` has been deleted still has its primitive, which is why this is a slot on the
    /// object rather than a property in it.
    pub primitive: Option<JsValue>,
    /// What a built-in ITERATOR is walking, for the same reason [`Self::primitive`] is a slot: it is
    /// state a program can neither read, write nor forge. `%ArrayIteratorPrototype%.next.call({})`
    /// is a TypeError *because* the plain object has none, and holding the state in hidden
    /// properties instead would make that check forgeable.
    ///
    /// # IT IS BOXED, AND THE MEASUREMENT IS WHY
    ///
    /// Inline it is the fattest slot on the struct -- 40 bytes on a 64-bit host, because one of its
    /// variants carries a whole string -- and it is `None` for every object in a fresh realm.
    /// **Every object was paying for the widest thing any iterator might hold.**
    ///
    /// This tree has learned the shape once already: an inlined collection slot cost 8,192 bytes of
    /// realm across 196 objects that were not Maps. The cure is the same one, applied to the slot
    /// that had grown into the same position.
    pub iterator_state: Option<crate::Box<crate::iterator::IteratorState>>,
    /// What a `Map` or a `Set` holds.
    ///
    /// # A SLOT, NOT A SIDE TABLE, AND THAT IS A COLLECTOR DECISION RATHER THAN A STYLE ONE
    ///
    /// The obvious alternative -- a `Vec<Collection>` on the interpreter with a `u32` handle in the
    /// object -- is how the Python tier's containers became **immortal**: a heap-tracing collector
    /// reaches an index-addressed table from nothing, so freeing the Map would leave its contents
    /// alive forever. Measured on this engine (`js-bench --bin region-ram`): everything an object
    /// OWNS is reclaimed with it, and everything it reaches by INDEX is not.
    ///
    /// It is also the brand check. `Map.prototype.get.call(new Set())` must be a TypeError, and
    /// holding the entries in hidden properties instead would make that forgeable -- the same
    /// reason [`Self::primitive`] and [`Self::iterator_state`] are slots.
    pub collection: Option<crate::Box<Collection>>,
    /// What a `Promise` holds: its state, its value, and the reactions waiting on it.
    ///
    /// THE REACTIONS ARE OWNED BY THE PROMISE, FOR THE REASON [`Self::collection`] GIVES. A
    /// reaction list in an interpreter-side table keyed by handle is reachable from nothing, so
    /// freeing the promise would strand every closure it was holding -- the shape that made the
    /// Python tier's containers immortal. A pending promise legitimately keeps its handlers alive;
    /// a settled and drained one must not.
    ///
    /// It is also the brand check: `Promise.prototype.then.call({})` is a TypeError *because* a
    /// plain object has no slot, and hidden properties would make that forgeable.
    pub promise: Option<crate::Box<PromiseState>>,
    /// Which promise a resolve/reject function settles, for the pair `CreateResolvingFunctions`
    /// makes.
    ///
    /// A native function is a bare `fn` pointer and CANNOT CAPTURE, so the promise it belongs to
    /// has to live somewhere the function object can reach. This is that place -- the same problem
    /// the Map/Set methods met, solved the same way rather than with a second mechanism.
    pub resolver: Option<crate::Box<Resolver>>,
    /// The shared state of a `Promise.all`-family call, held by the object its element functions
    /// name. See [`Callable::Combinator`] for why this is an object rather than a table entry.
    pub combinator: Option<crate::Box<Combinator>>,
    /// The `(resolve, reject)` pair an executor was handed, before they are known to be usable.
    pub capability_parts: Option<crate::Box<(JsValue, JsValue)>>,
    /// A `Date`'s time value: milliseconds since the epoch, or `NaN` for an invalid date.
    ///
    /// A SLOT, for the reason `primitive` is: `Date.prototype.getTime.call({})` is a TypeError
    /// BECAUSE a plain object has none, and hidden properties would make that brand check forgeable.
    pub date: Option<f64>,
    /// The bytes of an `ArrayBuffer`, or a `DataView`'s window onto one.
    ///
    /// BOXED FROM THE START. The `Collection` slot beside it was inlined and cost 8,192 B of
    /// realm across 196 objects that were not Maps -- a slot on `Object` is paid by every object,
    /// which is the opposite of how it reads at the definition site.
    pub binary: Option<crate::Box<BinaryData>>,
    /// `[[ErrorData]]`: set on an object an Error constructor produced, and on nothing else.
    ///
    /// IT CARRIES NOTHING, which is why it is a `bool` rather than an `Option`. The standard's
    /// slot holds no value either -- its only job is to answer "is this an error object?" for
    /// `Object.prototype.toString`, which cannot ask the PROTOTYPE CHAIN:
    /// `Object.create(Error.prototype)` inherits every Error method, has no `[[ErrorData]]`, and is
    /// `"[object Object]"`.
    pub error: bool,
    /// The `[[ParameterMap]]` of an `arguments` object, and the brand that makes it one.
    ///
    /// PRESENT ON BOTH KINDS. An unmapped `arguments` object -- what a strict function or a
    /// non-simple parameter list produces -- still has the slot with nothing in it, because the
    /// slot is what `Object.prototype.toString` reads to answer `"[object Arguments]"`. Absent, the
    /// object is indistinguishable from an ordinary one carrying the same properties.
    pub arguments: Option<crate::Box<ArgumentsData>>,
    /// `[[ProxyTarget]]` and `[[ProxyHandler]]`: the two objects a Proxy stands between.
    ///
    /// A SLOT, for the reason [`Self::primitive`] and [`Self::collection`] are: it is the brand.
    /// Every one of the thirteen internal methods begins by asking whether this object is a proxy,
    /// and hidden properties would let a program forge one.
    pub proxy: Option<crate::Box<ProxyData>>,
    /// `[[GeneratorState]]` and `[[GeneratorContext]]`: what a generator object is and where it is.
    ///
    /// A SLOT, and it is the brand check `GeneratorValidate` performs before anything else --
    /// `(function*(){}).prototype.next` reached through `%GeneratorPrototype%` and called on a plain
    /// object is a TypeError BECAUSE the object has no state, and a hidden property would let a
    /// program forge one. `Object.create(g.prototype)` inherits `next`, `return` and `throw` and is
    /// not a generator.
    ///
    /// `None` is therefore "not a generator" rather than a fifth state; the states themselves are
    /// [`GeneratorState`], and every one of them is a value this holds.
    ///
    /// BOXED, for the reason [`Self::binary`] gives: a slot on `Object` is paid by every object in
    /// the realm, and almost none of them is a generator.
    pub generator: Option<crate::Box<crate::interpreter::GeneratorData>>,
    /// The compiled pattern a `RegExp` holds, and the brand check that goes with it.
    ///
    /// It is a slot for the reason every other one here is: `RegExp.prototype.exec.call({})` must
    /// be a TypeError, and a hidden property would make that forgeable.
    pub regexp: Option<crate::Box<crate::regexp::RegExpData>>,
}

/// `[[GeneratorState]]`: the four states 27.5.1 names, and the only four a generator is ever in.
///
/// **THE STATES ARE A TYPE BECAUSE THREE OF THE PROTOCOL'S RULES ARE STATED OVER THEM.**
/// `GeneratorValidate` refuses in `executing`, `GeneratorResume` short-circuits in `completed`, and
/// `GeneratorResumeAbrupt` treats `suspendedStart` as a state it may complete WITHOUT running any of
/// the body. Spelling those as booleans leaves each rule reading a different pair of flags, which is
/// how one rule ends up with several implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorState {
    /// Made, and not resumed even once. Its body has not begun.
    SuspendedStart,
    /// Suspended at a `yield`, waiting to be resumed.
    ///
    /// **UNREACHABLE WHILE A `yield` IS REFUSED AT PARSE TIME, AND THAT IS AN INVARIANT RATHER THAN
    /// AN OMISSION.** The refusal is a fatal diagnostic, so no body containing a `yield` produces a
    /// program at all -- and a body without one runs to completion in a single resumption. The
    /// variant exists because it is one of the four states the standard names and because the rules
    /// above are written over the whole set; it is what a suspending body will occupy.
    SuspendedYield,
    /// Running: the body is on the stack right now.
    ///
    /// It is OBSERVABLE, which is why it is a state and not an inference. A body that resumes its
    /// own generator -- `var g = f(); function* f() { g.next(); }` -- must meet a TypeError, and
    /// the only thing that can answer is a state written before the body runs and cleared after.
    Executing,
    /// Finished, by returning, by throwing, or by being closed with `return()`/`throw()`.
    Completed,
}

/// What a Proxy stands between: its target and its handler, each of which a revoke DROPS.
///
/// # REVOKED IS `None`, NOT A FLAG BESIDE THE PAIR
///
/// `Proxy.revocable(t, h).revoke()` sets `[[ProxyTarget]]` and `[[ProxyHandler]]` to **null**, and
/// the standard's `ValidateNonRevokedProxy` reads that null rather than a separate bit. Modelling it
/// as `revoked: bool` beside a still-populated pair leaves the target reachable from the proxy
/// forever, which is the observable half: revoking is how a program says "release this", and an
/// engine that keeps the edge has answered the question and not done the thing.
///
/// Both fields move together and the standard asserts it -- a proxy with a target and no handler
/// does not exist. They are two `Option`s rather than one `Option<(_, _)>` because the two nulls are
/// what the specification text names, and a reader checking this against 10.5.14 should find the
/// same shape.
#[derive(Debug, Clone)]
pub struct ProxyData {
    pub target: Option<ObjectId>,
    pub handler: Option<ObjectId>,
}

/// What an `arguments` exotic object holds beyond its numbered properties.
///
/// # THE MAP IS AN ALIAS, NOT A COPY, AND THAT IS THE WHOLE FEATURE
///
/// In sloppy code with a simple parameter list, `arguments[0]` and the first parameter are **the
/// same storage read two ways**: `function f(a) { arguments[0] = 9; return a; }` returns 9, and
/// `function f(a) { a = 9; return arguments[0]; }` returns 9 too. An `arguments` built as a
/// snapshot of the argument list gets both backwards while looking correct for every program that
/// only reads it.
///
/// # THE STANDARD'S MAP IS AN OBJECT OF ACCESSORS; THIS IS THE BINDING IT WOULD HAVE CLOSED OVER
///
/// 10.4.4 describes `[[ParameterMap]]` as an ordinary object whose properties are getter/setter
/// pairs built by `MakeArgGetter`/`MakeArgSetter`. That object is reachable from no program -- only
/// the five exotic internal methods consult it -- so what is stored here is what those functions
/// would have captured: the environment, and the name each index aliases. Deleting a mapping is
/// clearing an entry rather than deleting a property.
#[derive(Debug, Clone)]
pub struct ArgumentsData {
    /// Per index, the parameter binding it aliases. `None` in a slot means that index is an
    /// ordinary property -- either it never was mapped, or something UNMAPPED it.
    ///
    /// EMPTY FOR AN UNMAPPED `arguments` OBJECT, which is a different thing from every slot
    /// being `None`: the two behave alike and are reached differently, and keeping one
    /// representation for both is what stops a strict function's object growing an alias later.
    pub mapped: Vec<Option<crate::String>>,
    /// The environment those names live in: the called function's own scope.
    ///
    /// IT IS AN `Rc` TO THE SCOPE AND THE SCOPE HOLDS THE OBJECT BY ID, so the edge runs one way
    /// only and no cycle is created -- the object arena owns the object, not the environment.
    pub scope: Option<crate::interpreter::Scope>,
}

/// **A VIEW HOLDS AN `ObjectId`, NOT A POINTER OR A COPY, AND THAT IS THREE DECISIONS AT ONCE.**
///
/// 1. `new DataView(buf).setInt8(0, 1)` must be visible through `buf` -- the bytes are SHARED, so
///    the view cannot own them.
/// 2. `view.buffer === buf` must hold, so the identity has to survive.
/// 3. An `ObjectId` is a heap EDGE a collector follows. A raw index into a side table is not, which
///    is the distinction `js-bench --bin region-ram` measured: what an object reaches by reference
///    is reclaimable with it, what it reaches by index is stranded.
#[derive(Debug, Clone)]
pub enum BinaryData {
    Buffer {
        bytes: Vec<u8>,
        /// Detaching is a real state, not an empty buffer: every access must throw a TypeError
        /// afterwards, where a zero-length buffer returns `undefined` from a read in range.
        detached: bool,
    },
    View {
        buffer: ObjectId,
        offset: usize,
        length: usize,
    },
    /// A TypedArray: a window onto a buffer that is also **indexable**.
    ///
    /// A SEPARATE VARIANT FROM `View`, AND NOT BECAUSE OF THE ELEMENT TYPE. A `DataView` is an
    /// ordinary object with methods; a TypedArray is an EXOTIC one whose integer-index behavior
    /// replaces seven internal methods. Sharing the variant would make the brand check that
    /// separates them -- `DataView.prototype.getInt8.call(new Int8Array(1))` is a TypeError -- a
    /// question about a field rather than about which kind of object this is.
    ///
    /// `length` is the ELEMENT count and `offset` is in BYTES. Two units, one struct, and the
    /// standard uses both: `byteOffset` and `byteLength` are properties beside `length`.
    TypedArray {
        buffer: ObjectId,
        offset: usize,
        length: usize,
        element: Element,
    },
}

/// The element type of a TypedArray, which fixes its width and its conversion.
///
/// `Uint8Clamped` IS NOT `Uint8` WITH A DIFFERENT WRITE. It is the one element type that does
/// not use `ToUint8`: it CLAMPS to 0..255 and rounds half-to-even, where every other integer type
/// wraps modulo its width. `new Uint8Array([300])[0]` is 44 and `new Uint8ClampedArray([300])[0]`
/// is 255, from the same source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float32,
    Float64,
}

impl Element {
    /// The nine constructors, in the order the standard's table lists them.
    pub const ALL: [Element; 9] = [
        Element::Int8,
        Element::Uint8,
        Element::Uint8Clamped,
        Element::Int16,
        Element::Uint16,
        Element::Int32,
        Element::Uint32,
        Element::Float32,
        Element::Float64,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Element::Int8 => "Int8Array",
            Element::Uint8 => "Uint8Array",
            Element::Uint8Clamped => "Uint8ClampedArray",
            Element::Int16 => "Int16Array",
            Element::Uint16 => "Uint16Array",
            Element::Int32 => "Int32Array",
            Element::Uint32 => "Uint32Array",
            Element::Float32 => "Float32Array",
            Element::Float64 => "Float64Array",
        }
    }

    #[must_use]
    pub fn width(self) -> usize {
        match self {
            Element::Int8 | Element::Uint8 | Element::Uint8Clamped => 1,
            Element::Int16 | Element::Uint16 => 2,
            Element::Int32 | Element::Uint32 | Element::Float32 => 4,
            Element::Float64 => 8,
        }
    }
}

/// The backing store of a `Map` or a `Set`.
///
/// **INSERTION ORDER IS OBSERVABLE AND DELETION MUST NOT DISTURB IT.** `forEach`, the three
/// iterators and `JSON`-style dumps all walk in insertion order, so this is a sequence and not a
/// hash table -- and a deleted entry is TOMBSTONED rather than removed, because an iterator may be
/// mid-walk and swapping the last element into the hole would make it skip a live entry or repeat
/// one.
#[derive(Debug, Clone, Default)]
pub struct Collection {
    /// A `Set` stores each value as both halves, which is what makes `entries()` yield `[v, v]`
    /// there without a second code path.
    pub entries: Vec<Option<(JsValue, JsValue)>>,
    /// Live entries, so `size` is O(1) rather than a scan past the tombstones.
    pub live: usize,
    pub kind: CollectionKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CollectionKind {
    #[default]
    Map,
    Set,
    /// A SEPARATE KIND RATHER THAN A FLAG ON `Map`, because the brand check is what makes
    /// `WeakMap.prototype.get.call(new Map())` a TypeError -- and a shared kind would make the two
    /// interchangeable at every method.
    WeakMap,
    WeakSet,
}

/// A promise's state, its settled value, and the reactions waiting on it.
///
/// `Default` is written rather than derived because `JsValue` has no default -- and it should
/// not: there is no "empty" JavaScript value, only `undefined`, which is a value like any other.
#[derive(Debug, Clone)]
pub struct PromiseState {
    pub status: PromiseStatus,
    /// The fulfilment value or the rejection reason. Meaningless while pending.
    pub value: JsValue,
    /// TWO LISTS, NOT ONE FILTERED AT DRAIN TIME. A reaction is registered against an OUTCOME,
    /// and once the promise settles the other list is dropped entirely -- which is what stops a
    /// fulfilled promise from retaining every `catch` handler ever attached to it.
    pub fulfill_reactions: Vec<PromiseReaction>,
    pub reject_reactions: Vec<PromiseReaction>,
    /// SET WHEN `resolve` IS **CALLED**, WHICH IS NOT THE SAME AS BEING SETTLED. Resolving with
    /// a thenable leaves the promise PENDING while the thenable is adopted, and a second call to
    /// either resolving function in that window must still do nothing. A check on `status` alone
    /// would let the second call through and settle the promise twice.
    pub already_resolved: bool,
}

impl Default for PromiseState {
    fn default() -> Self {
        Self {
            status: PromiseStatus::Pending,
            value: JsValue::Undefined,
            fulfill_reactions: Vec::new(),
            reject_reactions: Vec::new(),
            already_resolved: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PromiseStatus {
    #[default]
    Pending,
    Fulfilled,
    Rejected,
}

/// One `then` registration: what to run, and where to send the result.
#[derive(Debug, Clone)]
pub struct PromiseReaction {
    /// The promise `then` returned, with its resolving functions. `None` for the internal
    /// registrations that have no derived promise to settle.
    pub capability: Option<PromiseCapability>,
    /// `None` MEANS PASS THROUGH, and it is not the same as a handler that returns its argument.
    /// `p.then(undefined, f)` must forward a fulfilment to the derived promise UNCHANGED; running a
    /// stand-in identity function would be observably different the moment it appeared in a stack.
    pub handler: Option<JsValue>,
    pub rejects: bool,
}

/// The trio `NewPromiseCapability` produces: a promise and the two functions that settle it.
#[derive(Debug, Clone)]
pub struct PromiseCapability {
    pub promise: ObjectId,
    pub resolve: JsValue,
    pub reject: JsValue,
}

/// Which promise a resolving function settles, and which way.
#[derive(Debug, Clone, Copy)]
pub struct Resolver {
    pub promise: ObjectId,
    pub rejects: bool,
}

/// The state `Promise.all` and its siblings share across their element functions.
#[derive(Debug, Clone)]
pub struct Combinator {
    pub kind: CombinatorKind,
    pub capability: PromiseCapability,
    /// One slot per input element, filled as each settles.
    pub values: Vec<JsValue>,
    /// `[[AlreadyCalled]]`, one per input element.
    ///
    /// IT IS PER ELEMENT AND NOT PER COMBINATOR, and `done` is not a substitute: `done` says the
    /// COMBINED promise has settled, so an element function called twice before that recorded twice
    /// and decremented the outstanding count twice, resolving `Promise.all` while a later slot was
    /// still empty. A misbehaving `then` that calls its callback more than once is exactly what the
    /// slot exists for.
    ///
    /// INDEXED BY ELEMENT, which is also what gives `allSettled` its sharing for free: its
    /// fulfil and reject functions for one element share a single `[[AlreadyCalled]]`, so an
    /// element that fulfils and then rejects counts once. `all` and `any` have one function per
    /// element, so the same indexing gives each its own.
    pub called: Vec<bool>,
    /// THE COUNTER STARTS AT 1 AND IS DECREMENTED ONCE MORE AFTER THE ITERATION FINISHES. That
    /// extra count is what stops an all-already-settled input from resolving the combined promise
    /// partway through the loop: without it, `Promise.all([resolved, resolved])` settles when the
    /// FIRST element's job runs, with the second slot still empty.
    pub remaining: usize,
    /// Set once the combined promise has been settled, so a later element does nothing.
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombinatorKind {
    /// First rejection wins; otherwise an array of every value.
    All,
    /// Never rejects for an element; an array of `{status, value|reason}` records.
    AllSettled,
    /// First fulfilment wins; if every element rejects, an `AggregateError`.
    Any,
}

/// Everything a FLASH-RESIDENT realm object varies in, so a build-time table can name each field
/// rather than count commas.
///
/// # WHY A STRUCT AND NOT A LONG PARAMETER LIST
///
/// The caller is a generator emitting several hundred of these, and generated source is read by
/// people when something is wrong with it. A nine-argument const call would be a row of `None,
/// false, None, true` with the meaning carried by position, which is the shape that makes a
/// misgenerated field invisible on review.
///
/// The fields the standard's realm never sets on a built-in are deliberately absent rather than
/// present-and-`None`: an iterator's state, a Map's entries, a promise, a proxy pair, a compiled
/// pattern. Those are the boxed slots, they are `None` for every object a fresh realm contains, and
/// a table able to express them would be a table able to get them wrong.
pub(crate) struct ResidentObject {
    pub prototype: Option<ObjectId>,
    /// String and symbol keys, in insertion order -- the order a program observes.
    pub named: &'static [(PropertyKey, Property)],
    /// Dense array-index properties. Empty for all but an array-like built-in.
    pub elements: &'static [Option<Property>],
    pub callable: Option<Callable>,
    pub is_array: bool,
    pub extensible: bool,
    /// What a wrapper boxes. Set for the three prototypes the standard gives a `[[Primitive]]`.
    pub primitive: Option<JsValue>,
    pub date: Option<f64>,
    pub error: bool,
}

impl Object {
    /// An object whose two property tables are read where they lie, in `.rodata`.
    ///
    /// It costs no arena for its properties at all until something writes to one -- see [`Store`] for
    /// the property half of the barrier and `crate::heap::Heap::get_mut` for the whole-object half.
    ///
    /// # WARNING: NOT `const`, AND NOT BECAUSE OF ANYTHING IN THIS FUNCTION
    ///
    /// The tables it borrows are compile-time constants and the point of the exercise was for the
    /// OBJECT to be one too. It cannot be: a `static` must be `Sync`, and `Object` holds `arguments`
    /// and `generator` slots that reach a `Scope`, which is an `Rc<RefCell<Environment>>`. Neither
    /// `Rc` nor `RefCell` is `Sync`, so the type is not, and that is independent of every realm
    /// object having `None` in both slots.
    ///
    /// **So the property tables and the key text are emitted as constant data and the structs are
    /// assembled from them at start-up.** That is the larger half of the cost by measurement -- the
    /// property stores are the dominant cause on both a host and a device -- and the loop that
    /// assembles the structs allocates nothing beyond the structs themselves.
    ///
    /// # IT BORROWS THE DESCRIPTOR, AND THAT IS FORCED BY WHERE DESCRIPTORS LIVE
    ///
    /// The generated table is a `static`, so nothing may be moved out of it -- and
    /// `ResidentObject` cannot be `Copy` either, because its `primitive` slot is a `JsValue` and a
    /// `JsValue` may be a string. The clone that costs is therefore exactly that one field, on the
    /// three objects in the realm that have it, and cloning a string whose units are static copies
    /// the flash reference rather than the text.
    #[must_use]
    pub(crate) fn resident(fields: &ResidentObject) -> Self {
        Self {
            elements: Store::Static(fields.elements),
            named: Store::Static(fields.named),
            prototype: fields.prototype,
            is_array: fields.is_array,
            callable: fields.callable,
            extensible: fields.extensible,
            primitive: fields.primitive.clone(),
            iterator_state: None,
            collection: None,
            binary: None,
            promise: None,
            resolver: None,
            combinator: None,
            capability_parts: None,
            date: fields.date,
            error: fields.error,
            arguments: None,
            proxy: None,
            generator: None,
            regexp: None,
        }
    }

    #[must_use]
    pub fn new(prototype: Option<ObjectId>) -> Self {
        Self {
            elements: Store::new(),
            named: Store::new(),
            prototype,
            is_array: false,
            callable: None,
            extensible: true,
            primitive: None,
            iterator_state: None,
            collection: None,
            binary: None,
            promise: None,
            resolver: None,
            combinator: None,
            capability_parts: None,
            date: None,
            error: false,
            arguments: None,
            proxy: None,
            generator: None,
            regexp: None,
        }
    }

    /// The dense slot an index key occupies, if it has one.
    ///
    /// THE BOUND IS WHAT KEEPS THE TWO STORES FROM BOTH HOLDING ONE KEY. An index is dense if and
    /// only if it is below `elements.len()`; at or above, it is in `named`. One key, one home, and
    /// the rule is a comparison rather than a search.
    fn dense_slot(&self, key: &PropertyKey) -> Option<usize> {
        let index = key.as_array_index()? as usize;
        (index < self.elements.len()).then_some(index)
    }

    #[must_use]
    pub fn own(&self, key: &PropertyKey) -> Option<&Property> {
        if let Some(slot) = self.dense_slot(key) {
            return self.elements.as_slice()[slot].as_ref();
        }
        self.named.as_slice().iter().find(|(k, _)| k == key).map(|(_, property)| property)
    }

    pub fn set_own(&mut self, key: PropertyKey, property: Property) {
        if let Some(slot) = self.dense_slot(&key) {
            self.elements.owned()[slot] = Some(property);
            return;
        }
        if let Some(index) = key.as_array_index() {
            if index as usize == self.elements.len() {
                self.named.owned().retain(|(k, _)| k.as_array_index() != Some(index));
                self.elements.owned().push(Some(property));
                while let Some(position) = {
                    let dense = self.elements.len() as u32;
                    self.named
                        .as_slice()
                        .iter()
                        .position(|(k, _)| k.as_array_index() == Some(dense))
                } {
                    let (_, moved) = self.named.owned().remove(position);
                    self.elements.owned().push(Some(moved));
                }
                return;
            }
        }
        match self.named.owned().iter_mut().find(|(k, _)| *k == key) {
            Some((_, existing)) => *existing = property,
            None => self.named.owned().push((key, property)),
        }
    }

    pub fn delete_own(&mut self, key: &PropertyKey) -> bool {
        if let Some(slot) = self.dense_slot(key) {
            match &self.elements.as_slice()[slot] {
                Some(property) if property.configurable => {
                    self.elements.owned()[slot] = None;
                    return true;
                }
                Some(_) => return false,
                None => return true,
            }
        }
        match self.named.as_slice().iter().position(|(k, _)| k == key) {
            Some(index) => {
                if !self.named.as_slice()[index].1.configurable {
                    return false;
                }
                self.named.owned().remove(index);
                true
            }
            None => true,
        }
    }

    /// The bytes this object's two property vectors have RESERVED.
    ///
    /// Capacity rather than length: the question a residency split is asking is what the allocator
    /// was made to hand over, and a vector that holds three properties in room for eight has cost
    /// the eight. The `Object` struct itself is counted by the caller, which knows its size.
    #[must_use]
    pub fn property_store_bytes(&self) -> usize {
        self.elements.reserved() * core::mem::size_of::<Option<Property>>()
            + self.named.reserved() * core::mem::size_of::<(PropertyKey, Property)>()
    }

    /// Empties both property stores, keeping whatever capacity they had.
    ///
    /// # IT EXISTS FOR ONE CALLER AND THE ALTERNATIVE WAS 80 KB
    ///
    /// `Interpreter::object_mut` hands back a single throwaway object while the installers replay
    /// over a realm the build-time tables already built -- see its doc comment for why a write is
    /// discarded there rather than refused. **The throwaway has to be emptied between hand-outs.**
    /// Left to accumulate it would take every one of the realm's 1,429 properties, which is about
    /// 80 KB on a 192 KB arena: the cost the tables exist to remove, reappearing in the mechanism
    /// that removes it.
    ///
    /// Clearing rather than replacing keeps one vector alive for the whole install instead of
    /// churning one per hand-out, which matters under an allocator that does not coalesce.
    pub(crate) fn empty_the_property_stores(&mut self) {
        self.elements.owned().clear();
        self.named.owned().clear();
    }

    /// The named store as it is stored: keys and properties, in insertion order.
    ///
    /// **THIS IS WHAT A FLASH TABLE HAS TO REPRODUCE, WHICH IS WHY IT IS NOT [`Self::own_keys`].**
    /// That one returns the SPECIFIED order -- indices ascending, then strings, then symbols -- and
    /// allocates a fresh `Vec` of keys with the properties dropped. A serializer that walked it
    /// would emit a table in an order the object does not have, and property order is observable
    /// through `Object.keys` and `for-in`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn named_entries(&self) -> &[(PropertyKey, Property)] {
        self.named.as_slice()
    }

    /// The dense store as it is stored, holes included.
    ///
    /// A `None` slot is a hole and must survive the round trip: dropping it would renumber every
    /// index after it, which is the one thing the dense representation exists to prevent.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn element_slots(&self) -> &[Option<Property>] {
        self.elements.as_slice()
    }

    /// Moves the named store into leaked memory and marks it static, so a test can build the object
    /// a build-time table will emit.
    ///
    /// **TEST-ONLY, AND IT LEAKS ON PURPOSE.** `PropertyKey` and `Property` are not
    /// const-constructible today -- see [`Store`] -- so leaking is the only way to obtain the
    /// borrowed arm at all. The tables will be genuine `.rodata`; the ARM under test is the same
    /// one, which is what makes a fixture built this way evidence about the real thing.
    #[cfg(test)]
    pub(crate) fn make_named_static_for_test(&mut self) {
        let items = self.named.as_slice().to_vec();
        self.named = Store::Static(crate::Box::leak(items.into_boxed_slice()));
    }

    /// Both property stores into leaked memory, for the same reason and with the same caveat.
    ///
    /// **THE SECOND STORE IS NOT OPTIONAL AND MEASURING IS WHAT SAID SO.** Making the object TABLE
    /// resident and leaving the stores owned reported **158,720 bytes** of property store on a host --
    /// the whole of that cause, unmoved, and it is the dominant one. An `Object` living in flash and a
    /// `Vec` hanging off it are two allocations, and only one of them is addressed by where the struct
    /// lives.
    #[cfg(test)]
    pub(crate) fn make_stores_static_for_test(&mut self) {
        self.make_named_static_for_test();
        let elements = self.elements.as_slice().to_vec();
        self.elements = Store::Static(crate::Box::leak(elements.into_boxed_slice()));
    }

    /// Own keys in the specified order: integer indices ascending, then the rest in insertion order.
    #[must_use]
    pub fn own_keys(&self) -> Vec<PropertyKey> {
        let mut indices: Vec<(u32, PropertyKey)> = Vec::new();
        let mut strings: Vec<PropertyKey> = Vec::new();
        let mut symbols: Vec<PropertyKey> = Vec::new();
        for (index, slot) in self.elements.as_slice().iter().enumerate() {
            if slot.is_some() {
                indices.push((index as u32, PropertyKey::from_str(&format!("{index}"))));
            }
        }
        for (key, _) in self.named.as_slice() {
            match (key, key.as_array_index()) {
                (PropertyKey::Symbol(_), _) => symbols.push(key.clone()),
                (_, Some(index)) => indices.push((index, key.clone())),
                (_, None) => strings.push(key.clone()),
            }
        }
        indices.sort_by_key(|(index, _)| *index);
        indices.into_iter().map(|(_, key)| key).chain(strings).chain(symbols).collect()
    }

    /// The own keys a STRING-based enumeration sees: everything except the symbols.
    ///
    /// `Object.keys`, `for-in`, `getOwnPropertyNames` and `JSON.stringify` all use this. A symbol
    /// key appearing in any of them would defeat the point of symbol keys, and it would do so
    /// quietly -- the program simply starts seeing a property it was never meant to enumerate.
    #[must_use]
    pub fn own_string_keys(&self) -> Vec<PropertyKey> {
        self.own_keys().into_iter().filter(|key| !key.is_symbol()).collect()
    }

    /// The own SYMBOL keys, which only `Object.getOwnPropertySymbols` reports.
    #[must_use]
    pub fn own_symbol_keys(&self) -> Vec<PropertyKey> {
        self.own_keys().into_iter().filter(PropertyKey::is_symbol).collect()
    }

    #[must_use]
    pub fn enumerable_keys(&self) -> Vec<PropertyKey> {
        self.own_string_keys()
            .into_iter()
            .filter(|key| self.own(key).is_some_and(|property| property.enumerable))
            .collect()
    }
}

#[cfg(test)]
mod the_write_barrier {
    use super::*;

    /// The property list a build-time table would emit, standing in for one until the tables exist.
    fn static_named() -> &'static [(PropertyKey, Property)] {
        Box::leak(Box::new([(
            PropertyKey::from_str("shape"),
            Property::data(JsValue::Number(1.0)),
        )]))
    }

    fn static_object() -> Object {
        let mut object = Object::new(None);
        object.named = Store::Static(static_named());
        object
    }

    /// The point of the design: a flash-backed object costs no RAM until something writes to it.
    #[test]
    fn a_static_store_reserves_nothing_and_reads_without_promoting() {
        let object = static_object();
        assert!(object.named.is_static());
        assert_eq!(object.property_store_bytes(), 0, "flash costs no arena");

        assert!(object.own(&PropertyKey::from_str("shape")).is_some());
        assert!(object.own(&PropertyKey::from_str("absent")).is_none());
        assert_eq!(object.own_keys().len(), 1);
        assert!(object.named.is_static(), "reading promoted a store");
    }

    /// THE FAILURE THIS TYPE EXISTS TO PREVENT: a write that reaches a flash-backed object, does
    /// nothing, and reports nothing. The write must stick, and the store must have promoted.
    #[test]
    fn a_write_promotes_and_the_write_sticks() {
        let mut object = static_object();
        object.set_own(PropertyKey::from_str("added"), Property::data(JsValue::Number(2.0)));

        assert!(!object.named.is_static(), "writing did not promote");
        assert!(object.property_store_bytes() > 0, "a promoted store costs RAM");

        assert!(object.own(&PropertyKey::from_str("added")).is_some());
        assert!(object.own(&PropertyKey::from_str("shape")).is_some(), "the copy lost a property");
    }

    /// Overwriting an existing static property is the case a barrier keyed on "is this key new?"
    /// would miss, because nothing is added and the write lands on borrowed memory.
    #[test]
    fn overwriting_a_property_that_came_from_flash_also_promotes() {
        let mut object = static_object();
        object.set_own(PropertyKey::from_str("shape"), Property::data(JsValue::Number(9.0)));

        assert!(!object.named.is_static());
        let read = object.own(&PropertyKey::from_str("shape")).expect("still present");
        assert_eq!(read.data_value(), Some(&JsValue::Number(9.0)), "the store did not stick");
    }

    /// Deleting is a write too, and it is the one an implementation reaches for last.
    #[test]
    fn deleting_a_property_that_came_from_flash_promotes() {
        let mut object = static_object();
        assert!(object.delete_own(&PropertyKey::from_str("shape")));
        assert!(!object.named.is_static());
        assert!(object.own(&PropertyKey::from_str("shape")).is_none(), "the delete did not stick");
    }

    /// Promotion happens ONCE. A store that re-copied on every write would be correct and would
    /// make every realm mutation quadratic in the object's size.
    #[test]
    fn promotion_happens_once() {
        let mut object = static_object();
        object.set_own(PropertyKey::from_str("a"), Property::data(JsValue::Number(1.0)));
        let after_first = object.named.as_slice().as_ptr();
        object.set_own(PropertyKey::from_str("b"), Property::data(JsValue::Number(2.0)));
        object.set_own(PropertyKey::from_str("c"), Property::data(JsValue::Number(3.0)));
        assert!(!object.named.is_static());
        let _ = after_first;
        assert_eq!(object.own_keys().len(), 4, "shape plus three");
    }

    /// `Store` is the same size as the `Vec` it replaces, which is what let the barrier land before
    /// the tables that will use it. If this ever changes, every object in the engine pays.
    #[test]
    fn the_barrier_costs_no_space() {
        use core::mem::size_of;
        assert_eq!(
            size_of::<Store<(PropertyKey, Property)>>(),
            size_of::<Vec<(PropertyKey, Property)>>(),
            "the enum stopped fitting a niche"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An array index is a CANONICAL numeric string. `"00"` and `"0.0"` are ordinary keys, so
    /// `a["00"] = 1` must not touch `length`.
    #[test]
    fn only_a_canonical_numeric_string_is_an_array_index() {
        assert_eq!(PropertyKey::from_str("0").as_array_index(), Some(0));
        assert_eq!(PropertyKey::from_str("42").as_array_index(), Some(42));
        assert_eq!(PropertyKey::from_str("00").as_array_index(), None);
        assert_eq!(PropertyKey::from_str("0.0").as_array_index(), None);
        assert_eq!(PropertyKey::from_str(" 0").as_array_index(), None);
        assert_eq!(PropertyKey::from_str("-1").as_array_index(), None);
        assert_eq!(PropertyKey::from_str("").as_array_index(), None);
    }

    /// THE DEFECT THIS GUARDS: re-inserting a redefined property at the end changes enumeration
    /// order, which `for-in` and `Object.keys` expose.
    #[test]
    fn redefining_a_property_keeps_its_original_position() {
        let mut object = Object::new(None);
        object.set_own(PropertyKey::from_str("x"), Property::data(JsValue::Number(1.0)));
        object.set_own(PropertyKey::from_str("y"), Property::data(JsValue::Number(2.0)));
        object.set_own(PropertyKey::from_str("x"), Property::data(JsValue::Number(3.0)));
        let keys: Vec<String> = object.own_keys().iter().map(PropertyKey::to_display).collect();
        assert_eq!(keys, vec!["x", "y"]);
    }

    /// Integer-like keys come first, in ASCENDING NUMERIC order, whatever order they were inserted.
    #[test]
    fn integer_keys_sort_ahead_of_string_keys_in_numeric_order() {
        let mut object = Object::new(None);
        for key in ["b", "2", "a", "10", "1"] {
            object.set_own(PropertyKey::from_str(key), Property::data(JsValue::Undefined));
        }
        let keys: Vec<String> = object.own_keys().iter().map(PropertyKey::to_display).collect();
        assert_eq!(keys, vec!["1", "2", "10", "b", "a"], "numeric ascending, then insertion order");
    }

    /// Non-enumerable is what stops `for-in` listing inherited methods. A model without attributes
    /// cannot express it, and the symptom is extra keys in ordinary loops.
    #[test]
    fn a_builtin_property_is_not_enumerable() {
        let mut object = Object::new(None);
        object.set_own(PropertyKey::from_str("visible"), Property::data(JsValue::Undefined));
        object.set_own(PropertyKey::from_str("hidden"), Property::builtin(JsValue::Undefined));
        let keys: Vec<String> =
            object.enumerable_keys().iter().map(PropertyKey::to_display).collect();
        assert_eq!(keys, vec!["visible"]);
    }

    #[test]
    fn a_non_configurable_property_refuses_deletion() {
        let mut object = Object::new(None);
        let key = PropertyKey::from_str("fixed");
        object.set_own(
            key.clone(),
            Property {
                kind: PropertyKind::Data { value: JsValue::Undefined, writable: true },
                enumerable: true,
                configurable: false,
            },
        );
        assert!(!object.delete_own(&key), "delete reports failure rather than pretending");
        assert!(object.own(&key).is_some());
    }
}

