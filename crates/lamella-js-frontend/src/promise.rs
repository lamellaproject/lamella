//! `Promise`, and the job queue underneath it.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::object::{
    Combinator, CombinatorKind, Object, PromiseCapability, PromiseReaction, PromiseState,
    PromiseStatus, PropertyKey, Resolver,
};
use crate::value::{JsValue, ObjectId};
use crate::Vec;

/// A unit of deferred work.
///
/// A job holds `ObjectId`s, which a tracing collector reaches from nothing -- so **the queue is a
/// ROOT**. A promise with a job in flight and no other reference is live precisely because the job
/// is going to settle it, and a collector that cannot see the queue would free it first.
#[derive(Debug, Clone)]
pub enum Job {
    /// Run one `then` registration against a settled value.
    Reaction { reaction: PromiseReaction, argument: JsValue },
    /// Adopt a thenable: call its `then` with this promise's resolving functions.
    ///
    /// IT IS A JOB RATHER THAN A DIRECT CALL because `then` is user code, and calling it inside
    /// `resolve` would run a stranger's function in the middle of the resolution algorithm.
    Thenable { promise: ObjectId, thenable: JsValue, then: JsValue },
}

pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.promise_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "Promise",
        1,
        |interpreter, _this, _arguments| {
            interpreter.type_error("Promise requires new")
        },
        |interpreter, _this, arguments| {
            let executor = arg(arguments, 0);
            if !interpreter.is_callable(&executor) {
                return interpreter.type_error("the Promise executor is not a function");
            }
            let promise = new_promise(interpreter);
            let (resolve, reject) = resolving_functions(interpreter, promise);
            if let Completion::Throw(error) =
                interpreter.call_value(&executor, JsValue::Undefined, crate::vec![resolve, reject])
            {
                settle(interpreter, promise, error, true);
            }
            Completion::Normal(JsValue::Object(promise))
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Promise", JsValue::Object(constructor));
    interpreter.define_to_string_tag(prototype, "Promise");
    interpreter.intrinsics.promise_constructor = constructor;
    interpreter.define_species_getter(constructor);

    interpreter.define_method(constructor, "resolve", 1, |interpreter, this, arguments| {
        let JsValue::Object(receiver) = this else {
            return interpreter.type_error("Promise.resolve must be called on an object");
        };
        let value = arg(arguments, 0);
        if let JsValue::Object(id) = &value {
            if interpreter.object(*id).promise.is_some() {
                let key = PropertyKey::from_str("constructor");
                match interpreter.get_property(*id, &key) {
                    Completion::Normal(JsValue::Object(owner)) if owner == receiver => {
                        return Completion::Normal(value)
                    }
                    Completion::Normal(_) => {}
                    abrupt => return abrupt,
                }
            }
        }
        let capability = match new_capability_from(interpreter, &JsValue::Object(receiver)) {
            Ok(capability) => capability,
            Err(abrupt) => return abrupt,
        };
        let resolve = capability.resolve.clone();
        let completion = interpreter.call_value(&resolve, JsValue::Undefined, crate::vec![value]);
        if completion.is_abrupt() {
            return completion;
        }
        Completion::Normal(JsValue::Object(capability.promise))
    });

    interpreter.define_method(constructor, "reject", 1, |interpreter, this, arguments| {
        let capability = match new_capability_from(interpreter, &this) {
            Ok(capability) => capability,
            Err(abrupt) => return abrupt,
        };
        let reject = capability.reject.clone();
        let completion =
            interpreter.call_value(&reject, JsValue::Undefined, crate::vec![arg(arguments, 0)]);
        if completion.is_abrupt() {
            return completion;
        }
        Completion::Normal(JsValue::Object(capability.promise))
    });

    interpreter.define_method(constructor, "all", 1, |interpreter, this, arguments| {
        combine(interpreter, CombinatorKind::All, &this, &arg(arguments, 0))
    });
    interpreter.define_method(constructor, "allSettled", 1, |interpreter, this, arguments| {
        combine(interpreter, CombinatorKind::AllSettled, &this, &arg(arguments, 0))
    });
    interpreter.define_method(constructor, "any", 1, |interpreter, this, arguments| {
        combine(interpreter, CombinatorKind::Any, &this, &arg(arguments, 0))
    });

    interpreter.define_method(constructor, "race", 1, |interpreter, this, arguments| {
        let capability = match new_capability_from(interpreter, &this) {
            Ok(capability) => capability,
            Err(abrupt) => return abrupt,
        };
        let raced = capability.promise;
        let promise_resolve = match get_promise_resolve(interpreter, &this) {
            Ok(resolve) => resolve,
            Err(Completion::Throw(error)) => {
                return reject_through_capability(interpreter, &capability, error);
            }
            Err(abrupt) => return abrupt,
        };
        let mut record = match crate::iterator::get_iterator(interpreter, &arg(arguments, 0)) {
            Ok(record) => record,
            Err(Completion::Throw(error)) => {
                return reject_through_capability(interpreter, &capability, error);
            }
            Err(abrupt) => return abrupt,
        };
        loop {
            let stepped = match crate::iterator::iterator_step(interpreter, &mut record) {
                Ok(stepped) => stepped,
                Err(Completion::Throw(error)) => {
                    return reject_through_capability(interpreter, &capability, error);
                }
                Err(abrupt) => return abrupt,
            };
            let Some(result) = stepped else { break };
            let element = match crate::iterator::iterator_value(interpreter, result) {
                Ok(element) => element,
                Err(Completion::Throw(error)) => {
                    return reject_through_capability(interpreter, &capability, error);
                }
                Err(abrupt) => return abrupt,
            };
            let outcome = (|| {
                let element_promise =
                    match resolve_through(interpreter, &this, &promise_resolve, element) {
                        Ok(promise) => promise,
                        Err(abrupt) => return abrupt,
                    };
                let then_method = match interpreter
                    .get_member(&element_promise, &PropertyKey::from_str("then"))
                {
                    Completion::Normal(method) => method,
                    abrupt => return abrupt,
                };
                interpreter.call_value(
                    &then_method,
                    element_promise,
                    crate::vec![capability.resolve.clone(), capability.reject.clone()],
                )
            })();
            if let Completion::Throw(error) = outcome {
                let closed = crate::iterator::iterator_close(
                    interpreter,
                    &record,
                    Completion::Throw(error.clone()),
                );
                let reason = match closed {
                    Completion::Throw(from_close) => from_close,
                    _ => error,
                };
                return reject_through_capability(interpreter, &capability, reason);
            }
        }
        Completion::Normal(JsValue::Object(raced))
    });

    interpreter.define_method(prototype, "then", 2, |interpreter, this, arguments| {
        then(interpreter, &this, arg(arguments, 0), arg(arguments, 1))
    });

    interpreter.define_method(prototype, "catch", 1, |interpreter, this, arguments| {
        let then_method = match interpreter.get_member(&this, &PropertyKey::from_str("then")) {
            Completion::Normal(method) => method,
            abrupt => return abrupt,
        };
        interpreter.call_value(
            &then_method,
            this,
            crate::vec![JsValue::Undefined, arg(arguments, 0)],
        )
    });

    interpreter.define_method(prototype, "finally", 1, |interpreter, _this, _arguments| {
        interpreter.refuse(crate::absence::Absence::PromiseFinally)
    });
}

/// `Promise.all` / `allSettled` / `any`, which differ only in what an element does to the total.
///
/// THEY ARE ONE ALGORITHM. Each iterates the input, calls `Promise.resolve` on every element,
/// and attaches a pair of element functions that write into a shared results array and decrement a
/// shared counter. Writing three of them separately is how the three drift apart on the parts they
/// share -- the iteration, the counter and the already-settled guard are identical in all three.
fn combine(
    interpreter: &mut Interpreter,
    kind: CombinatorKind,
    this: &JsValue,
    iterable: &JsValue,
) -> Completion {
    let capability = match new_capability_from(interpreter, this) {
        Ok(capability) => capability,
        Err(abrupt) => return abrupt,
    };
    let combined = capability.promise;
    let state_object = {
        let mut object = Object::new(None);
        object.combinator = Some(crate::Box::new(Combinator {
            kind,
            capability: capability.clone(),
            values: Vec::new(),
            called: Vec::new(),
            remaining: 1,
            done: false,
        }));
        interpreter.allocate(object)
    };
    let promise_resolve = match get_promise_resolve(interpreter, this) {
        Ok(resolve) => resolve,
        Err(Completion::Throw(error)) => {
            return reject_through_capability(interpreter, &capability, error);
        }
        Err(abrupt) => return abrupt,
    };
    let mut record = match crate::iterator::get_iterator(interpreter, iterable) {
        Ok(record) => record,
        Err(Completion::Throw(error)) => {
            return reject_through_capability(interpreter, &capability, error);
        }
        Err(abrupt) => return abrupt,
    };
    let mut index = 0u32;
    loop {
        let stepped = match crate::iterator::iterator_step(interpreter, &mut record) {
            Ok(stepped) => stepped,
            Err(Completion::Throw(error)) => {
                return reject_through_capability(interpreter, &capability, error);
            }
            Err(abrupt) => return abrupt,
        };
        let Some(result) = stepped else { break };
        let element = match crate::iterator::iterator_value(interpreter, result) {
            Ok(element) => element,
            Err(Completion::Throw(error)) => {
                return reject_through_capability(interpreter, &capability, error);
            }
            Err(abrupt) => return abrupt,
        };
        {
            let Some(state) = interpreter.object_mut(state_object).combinator.as_deref_mut() else {
                return interpreter.internal_defect("a combinator lost its state");
            };
            state.values.push(JsValue::Undefined);
            state.called.push(false);
            state.remaining += 1;
        }
        let outcome =
            attach_element(interpreter, state_object, index, this, &promise_resolve, element);
        if let Completion::Throw(error) = outcome {
            let closed = crate::iterator::iterator_close(
                interpreter,
                &record,
                Completion::Throw(error.clone()),
            );
            let reason = match closed {
                Completion::Throw(from_close) => from_close,
                _ => error,
            };
            return reject_through_capability(interpreter, &capability, reason);
        }
        index += 1;
    }
    release_one(interpreter, state_object);
    Completion::Normal(JsValue::Object(combined))
}

/// `Promise.resolve(element).then(onOk, onErr)` for one element of a combinator.
fn attach_element(
    interpreter: &mut Interpreter,
    state_object: ObjectId,
    index: u32,
    constructor: &JsValue,
    promise_resolve: &JsValue,
    element: JsValue,
) -> Completion {
    let element_promise =
        match resolve_through(interpreter, constructor, promise_resolve, element) {
            Ok(promise) => promise,
            Err(abrupt) => return abrupt,
        };
    let (kind, capability) = {
        let Some(record) = interpreter.object(state_object).combinator.as_deref() else {
            return interpreter.internal_defect("a combinator lost its state");
        };
        (record.kind, record.capability.clone())
    };
    let on_ok = match kind {
        CombinatorKind::Any => capability.resolve.clone(),
        CombinatorKind::All | CombinatorKind::AllSettled => {
            JsValue::Object(combinator_function(interpreter, state_object, index, false))
        }
    };
    let on_err = match kind {
        CombinatorKind::All => capability.reject.clone(),
        CombinatorKind::AllSettled | CombinatorKind::Any => {
            JsValue::Object(combinator_function(interpreter, state_object, index, true))
        }
    };
    let then_method = match interpreter.get_member(&element_promise, &PropertyKey::from_str("then"))
    {
        Completion::Normal(method) => method,
        abrupt => return abrupt,
    };
    interpreter.call_value(&then_method, element_promise, crate::vec![on_ok, on_err])
}

/// `GetPromiseResolve(C)`: read `resolve` off the constructor ONCE, before the loop.
///
/// IT IS THE CONSTRUCTOR'S `resolve`, NOT THE INTRINSIC ONE, AND THAT IS LOAD-BEARING RATHER
/// THAN PEDANTRY. A test that replaces `Promise.resolve` with a thrower and passes an INFINITE
/// iterator is relying on the throw to stop the loop -- an implementation that quietly uses its own
/// resolve never throws, never stops, and allocates until the process dies. That is exactly what
/// happened here: 2,513 MB live before the watchdog fired.
///
/// Read ONCE and reused, not looked up per element. A getter on `resolve` must be observed a
/// single time however many elements there are.
fn get_promise_resolve(
    interpreter: &mut Interpreter,
    constructor: &JsValue,
) -> Result<JsValue, Completion> {
    let resolve = match interpreter.get_member(constructor, &PropertyKey::from_str("resolve")) {
        Completion::Normal(resolve) => resolve,
        abrupt => return Err(abrupt),
    };
    if !interpreter.is_callable(&resolve) {
        return Err(interpreter.type_error("the constructor's `resolve` is not callable"));
    }
    Ok(resolve)
}

/// `IfAbruptRejectPromise(value, capability)`: a combinator's abrupt completion becomes a rejection
/// of the capability's promise, delivered THROUGH the capability's own reject FUNCTION.
///
/// # IT IS A CALL, AND FOR THE INTRINSIC `Promise` A WRITE LOOKS EXACTLY THE SAME
///
/// A rejection path in `race` or `combine` must NOT reach into the promise object and set its
/// state. For a promise this engine built, that is what the capability's reject function would do
/// anyway, so every test using `Promise.all` directly agrees -- and it is wrong the moment the
/// constructor is not ours: `Promise.all.call(BadPromise, iterable)`.
///
/// `NewPromiseCapability(BadPromise)` hands back an ORDINARY OBJECT with no promise slot and the two
/// functions `BadPromise` gave its executor. Writing the state then found no slot and returned
/// silently -- so the program's own `reject` was never called, the rejection VANISHED, and the
/// combinator answered a pending object nobody could observe. Subclassing `Promise` is the common
/// case of the same thing.
///
/// A THROW FROM `reject` PROPAGATES. The standard's step is `? Call(...)`, so a capability whose
/// reject function throws makes `Promise.all` itself throw rather than answering the promise.
fn reject_through_capability(
    interpreter: &mut Interpreter,
    capability: &PromiseCapability,
    reason: JsValue,
) -> Completion {
    match interpreter.call_value(&capability.reject, JsValue::Undefined, crate::vec![reason]) {
        Completion::Normal(_) => Completion::Normal(JsValue::Object(capability.promise)),
        abrupt => abrupt,
    }
}

/// `Call(promiseResolve, C, «element»)`, which is how each element becomes a promise.
fn resolve_through(
    interpreter: &mut Interpreter,
    constructor: &JsValue,
    promise_resolve: &JsValue,
    element: JsValue,
) -> Result<JsValue, Completion> {
    match interpreter.call_value(promise_resolve, constructor.clone(), crate::vec![element]) {
        Completion::Normal(promise) => Ok(promise),
        abrupt => Err(abrupt),
    }
}

fn combinator_function(
    interpreter: &mut Interpreter,
    state: ObjectId,
    index: u32,
    rejects: bool,
) -> ObjectId {
    let function_prototype = interpreter.intrinsics.function_prototype;
    let mut object = Object::new(Some(function_prototype));
    object.callable = Some(crate::object::Callable::Combinator { state, index, rejects });
    let id = interpreter.allocate(object);
    interpreter.set_function_shape(id, "", 1);
    id
}

/// One element settled: record it, and settle the combined promise when it is the last one.
pub(crate) fn call_combinator(
    interpreter: &mut Interpreter,
    state: ObjectId,
    index: u32,
    rejects: bool,
    arguments: &[JsValue],
) -> Completion {
    let argument = arg(arguments, 0);
    let kind = {
        let Some(record) = interpreter.object_mut(state).combinator.as_deref_mut() else {
            return interpreter.internal_defect("a combinator element lost its state");
        };
        if record.done {
            return Completion::Normal(JsValue::Undefined);
        }
        match record.called.get_mut(index as usize) {
            Some(called) if *called => return Completion::Normal(JsValue::Undefined),
            Some(called) => *called = true,
            None => return interpreter.internal_defect("a combinator element has no slot"),
        }
        record.kind
    };
    let recorded = if kind == CombinatorKind::AllSettled {
        let record = interpreter.allocate(Object::new(Some(interpreter.intrinsics.object_prototype)));
        let status = if rejects { "rejected" } else { "fulfilled" };
        let _ = interpreter.create_data_property(record, PropertyKey::from_str("status"), JsValue::string(status));
        let key = if rejects { "reason" } else { "value" };
        let _ = interpreter.create_data_property(record, PropertyKey::from_str(key), argument);
        JsValue::Object(record)
    } else {
        argument
    };
    {
        let Some(record) = interpreter.object_mut(state).combinator.as_deref_mut() else {
            return interpreter.internal_defect("a combinator element lost its state");
        };
        if let Some(slot) = record.values.get_mut(index as usize) {
            *slot = recorded;
        }
    }
    release_one(interpreter, state);
    Completion::Normal(JsValue::Undefined)
}

/// Drop one from the outstanding count, settling the combined promise when it reaches zero.
fn release_one(interpreter: &mut Interpreter, state: ObjectId) {
    let finished = {
        let Some(record) = interpreter.object_mut(state).combinator.as_deref_mut() else { return };
        if record.done {
            return;
        }
        record.remaining = record.remaining.saturating_sub(1);
        record.remaining == 0
    };
    if !finished {
        return;
    }
    let (kind, capability, values) = {
        let Some(record) = interpreter.object_mut(state).combinator.as_deref_mut() else { return };
        record.done = true;
        (record.kind, record.capability.clone(), core::mem::take(&mut record.values))
    };
    match kind {
        CombinatorKind::Any => {
            let errors = interpreter.new_array(values);
            let error = interpreter.make_error("AggregateError", "every promise was rejected");
            if let JsValue::Object(id) = &error {
                let _ = interpreter.create_data_property(
                    *id,
                    PropertyKey::from_str("errors"),
                    JsValue::Object(errors),
                );
            }
            let reject = capability.reject.clone();
            let _ = interpreter.call_value(&reject, JsValue::Undefined, crate::vec![error]);
        }
        CombinatorKind::All | CombinatorKind::AllSettled => {
            let list = interpreter.new_array(values);
            let resolve = capability.resolve.clone();
            let _ = interpreter.call_value(
                &resolve,
                JsValue::Undefined,
                crate::vec![JsValue::Object(list)],
            );
        }
    }
}

/// A fresh pending promise with the promise slot the brand checks look for.
pub(crate) fn new_promise(interpreter: &mut Interpreter) -> ObjectId {
    let prototype = interpreter.intrinsics.promise_prototype;
    let mut object = Object::new(Some(prototype));
    object.promise = Some(crate::Box::new(PromiseState::default()));
    interpreter.allocate(object)
}

/// `CreateResolvingFunctions`: the pair a program is handed, each knowing its promise.
///
/// THE PAIR IS FRESH EVERY TIME and the two are distinct objects, which a program can see:
/// `resolve !== reject`, and neither is `===` to the pair from another promise.
fn resolving_functions(interpreter: &mut Interpreter, promise: ObjectId) -> (JsValue, JsValue) {
    let resolve = resolving_function(interpreter, promise, false);
    let reject = resolving_function(interpreter, promise, true);
    (JsValue::Object(resolve), JsValue::Object(reject))
}

fn resolving_function(
    interpreter: &mut Interpreter,
    promise: ObjectId,
    rejects: bool,
) -> ObjectId {
    let function_prototype = interpreter.intrinsics.function_prototype;
    let mut object = Object::new(Some(function_prototype));
    object.callable = Some(crate::object::Callable::Resolver { promise, rejects });
    object.resolver = Some(crate::Box::new(Resolver { promise, rejects }));
    let id = interpreter.allocate(object);
    interpreter.set_function_shape(id, "", 1);
    id
}

/// The body of a call to either resolving function, reached from the interpreter's call path.
pub(crate) fn call_resolver(
    interpreter: &mut Interpreter,
    promise: ObjectId,
    rejects: bool,
    arguments: &[JsValue],
) -> Completion {
    if rejects {
        settle(interpreter, promise, arg(arguments, 0), true);
    } else {
        resolve_promise(interpreter, promise, arg(arguments, 0));
    }
    Completion::Normal(JsValue::Undefined)
}

/// `NewPromiseCapability(%Promise%)`: the promise `then` will return, plus its settling functions.
fn new_capability(interpreter: &mut Interpreter) -> PromiseCapability {
    let promise = new_promise(interpreter);
    let (resolve, reject) = resolving_functions(interpreter, promise);
    PromiseCapability { promise, resolve, reject }
}

/// `NewPromiseCapability(C)` for an arbitrary constructor.
///
/// IT IS NOT ENOUGH THAT `C` IS A CONSTRUCTOR. `NewPromiseCapability` constructs it with an
/// EXECUTOR and then checks that the executor was handed two callable functions -- so
/// `function ZeroArgConstructor() {}`, which is a perfectly good constructor that ignores its
/// argument, must be REFUSED. Six conformance runs turn on exactly that, and they had been passing
/// for the wrong reason: before `Promise.all` existed at all, `Promise.race.call(X, [3])` threw
/// "not a function", which is accidentally the right error.
///
/// The pair is captured through a slot rather than returned, because the constructor decides
/// whether to call the executor at all -- and "it never did" is the case being detected.
fn new_capability_from(
    interpreter: &mut Interpreter,
    constructor: &JsValue,
) -> Result<PromiseCapability, Completion> {
    if matches!(constructor, JsValue::Object(id) if *id == interpreter.intrinsics.promise_constructor)
    {
        return Ok(new_capability(interpreter));
    }
    let Some(target) = (match constructor {
        JsValue::Object(id) => Some(*id),
        _ => None,
    }) else {
        return Err(interpreter.type_error("not a constructor"));
    };
    let state = {
        let mut object = Object::new(None);
        object.capability_parts =
            Some(crate::Box::new((JsValue::Undefined, JsValue::Undefined)));
        interpreter.allocate(object)
    };
    let executor = {
        let function_prototype = interpreter.intrinsics.function_prototype;
        let mut object = Object::new(Some(function_prototype));
        object.callable = Some(crate::object::Callable::CapabilityExecutor { state });
        let id = interpreter.allocate(object);
        interpreter.set_function_shape(id, "", 2);
        id
    };
    let promise = match interpreter.construct(target, crate::vec![JsValue::Object(executor)]) {
        Completion::Normal(JsValue::Object(id)) => id,
        Completion::Normal(_) => {
            return Err(interpreter.type_error("a promise constructor must return an object"))
        }
        abrupt => return Err(abrupt),
    };
    let (resolve, reject) = match interpreter.object(state).capability_parts.as_deref() {
        Some(pair) => pair.clone(),
        None => (JsValue::Undefined, JsValue::Undefined),
    };
    if !interpreter.is_callable(&resolve) || !interpreter.is_callable(&reject) {
        return Err(interpreter
            .type_error("the promise constructor did not supply resolve and reject functions"));
    }
    Ok(PromiseCapability { promise, resolve, reject })
}

/// The executor body: record what the constructor handed it.
pub(crate) fn call_capability_executor(
    interpreter: &mut Interpreter,
    state: ObjectId,
    arguments: &[JsValue],
) -> Completion {
    if let Some(pair) = interpreter.object(state).capability_parts.as_deref() {
        if !matches!(pair.0, JsValue::Undefined) || !matches!(pair.1, JsValue::Undefined) {
            return interpreter.type_error("the promise capability is already set");
        }
    }
    let pair = (arg(arguments, 0), arg(arguments, 1));
    if let Some(slot) = interpreter.object_mut(state).capability_parts.as_deref_mut() {
        *slot = pair;
    }
    Completion::Normal(JsValue::Undefined)
}

/// `ResolvePromise`: fulfil with a plain value, or ADOPT a thenable.
pub(crate) fn resolve_promise(interpreter: &mut Interpreter, promise: ObjectId, value: JsValue) {
    {
        let Some(state) = interpreter.object_mut(promise).promise.as_deref_mut() else { return };
        if state.already_resolved {
            return;
        }
        state.already_resolved = true;
    }
    if matches!(&value, JsValue::Object(id) if *id == promise) {
        let error = interpreter.make_error("TypeError", "a promise cannot resolve to itself");
        settle_settled(interpreter, promise, error, true);
        return;
    }
    if value.is_object() {
        let then_method = match interpreter.get_member(&value, &PropertyKey::from_str("then")) {
            Completion::Normal(method) => method,
            Completion::Throw(error) => {
                settle_settled(interpreter, promise, error, true);
                return;
            }
            _ => JsValue::Undefined,
        };
        if interpreter.is_callable(&then_method) {
            interpreter.enqueue(Job::Thenable { promise, thenable: value, then: then_method });
            return;
        }
    }
    settle_settled(interpreter, promise, value, false);
}

/// Settle from the outside, honouring `already_resolved`. Used by `reject` and by a throwing
/// executor.
fn settle(interpreter: &mut Interpreter, promise: ObjectId, value: JsValue, rejects: bool) {
    {
        let Some(state) = interpreter.object_mut(promise).promise.as_deref_mut() else { return };
        if state.already_resolved {
            return;
        }
        state.already_resolved = true;
    }
    settle_settled(interpreter, promise, value, rejects);
}

/// `FulfillPromise` / `RejectPromise`: record the outcome and queue everything waiting on it.
fn settle_settled(
    interpreter: &mut Interpreter,
    promise: ObjectId,
    value: JsValue,
    rejects: bool,
) {
    let reactions = {
        let Some(state) = interpreter.object_mut(promise).promise.as_deref_mut() else { return };
        if state.status != PromiseStatus::Pending {
            return;
        }
        state.status = if rejects { PromiseStatus::Rejected } else { PromiseStatus::Fulfilled };
        state.value = value.clone();
        let taken = if rejects {
            core::mem::take(&mut state.reject_reactions)
        } else {
            core::mem::take(&mut state.fulfill_reactions)
        };
        state.fulfill_reactions = Vec::new();
        state.reject_reactions = Vec::new();
        taken
    };
    for reaction in reactions {
        interpreter.enqueue(Job::Reaction { reaction, argument: value.clone() });
    }
}

/// `Promise.prototype.then`.
fn then(
    interpreter: &mut Interpreter,
    this: &JsValue,
    on_fulfilled: JsValue,
    on_rejected: JsValue,
) -> Completion {
    let JsValue::Object(promise) = this else {
        return interpreter.type_error("then requires a promise");
    };
    let promise = *promise;
    if interpreter.object(promise).promise.is_none() {
        return interpreter.type_error("then requires a promise");
    }
    let default = interpreter.intrinsics.promise_constructor;
    let constructor = match interpreter.species_constructor(promise, default) {
        Ok(constructor) => constructor,
        Err(abrupt) => return abrupt,
    };
    let capability = match new_capability_from(interpreter, &JsValue::Object(constructor)) {
        Ok(capability) => capability,
        Err(abrupt) => return abrupt,
    };
    let fulfil_handler = interpreter.is_callable(&on_fulfilled).then_some(on_fulfilled);
    let reject_handler = interpreter.is_callable(&on_rejected).then_some(on_rejected);
    let fulfil = PromiseReaction {
        capability: Some(capability.clone()),
        handler: fulfil_handler,
        rejects: false,
    };
    let reject = PromiseReaction {
        capability: Some(capability.clone()),
        handler: reject_handler,
        rejects: true,
    };
    let status = {
        let state = interpreter.object(promise).promise.as_deref().expect("checked above");
        state.status
    };
    match status {
        PromiseStatus::Pending => {
            let state =
                interpreter.object_mut(promise).promise.as_deref_mut().expect("checked above");
            state.fulfill_reactions.push(fulfil);
            state.reject_reactions.push(reject);
        }
        PromiseStatus::Fulfilled => {
            let value =
                interpreter.object(promise).promise.as_deref().expect("checked").value.clone();
            interpreter.enqueue(Job::Reaction { reaction: fulfil, argument: value });
        }
        PromiseStatus::Rejected => {
            let value =
                interpreter.object(promise).promise.as_deref().expect("checked").value.clone();
            interpreter.enqueue(Job::Reaction { reaction: reject, argument: value });
        }
    }
    Completion::Normal(JsValue::Object(capability.promise))
}

/// Run one job. Called by the interpreter's drain loop.
pub(crate) fn run_job(interpreter: &mut Interpreter, job: Job) {
    match job {
        Job::Reaction { reaction, argument } => {
            let outcome = match &reaction.handler {
                None => {
                    if reaction.rejects {
                        Err(argument)
                    } else {
                        Ok(argument)
                    }
                }
                Some(handler) => {
                    match interpreter.call_value(handler, JsValue::Undefined, crate::vec![argument])
                    {
                        Completion::Normal(value) => Ok(value),
                        Completion::Throw(error) => Err(error),
                        _ => Ok(JsValue::Undefined),
                    }
                }
            };
            let Some(capability) = reaction.capability else { return };
            match outcome {
                Ok(value) => {
                    let resolve = capability.resolve.clone();
                    let _ = interpreter.call_value(&resolve, JsValue::Undefined, crate::vec![value]);
                }
                Err(error) => {
                    let reject = capability.reject.clone();
                    let _ = interpreter.call_value(&reject, JsValue::Undefined, crate::vec![error]);
                }
            }
        }
        Job::Thenable { promise, thenable, then } => {
            let (resolve, reject) = resolving_functions(interpreter, promise);
            {
                let Some(state) = interpreter.object_mut(promise).promise.as_deref_mut() else {
                    return;
                };
                state.already_resolved = false;
            }
            if let Completion::Throw(error) =
                interpreter.call_value(&then, thenable, crate::vec![resolve, reject.clone()])
            {
                let _ = interpreter.call_value(&reject, JsValue::Undefined, crate::vec![error]);
            }
        }
    }
}




