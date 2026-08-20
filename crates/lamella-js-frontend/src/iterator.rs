//! The iterator protocol: the abstract operations, and the two built-in iterators.

use crate::builtins::{arg, array_length};
use crate::interpreter::{Completion, Interpreter};
use crate::object::{Object, Property, PropertyKey};
use crate::string_value::JsString;
use crate::value::{JsValue, ObjectId};
use crate::{format, Vec};

/// What a built-in iterator is iterating, and how far it has got.
///
/// This is an INTERNAL SLOT, not a property, and the difference is observable: a program cannot
/// read it, cannot delete it, and cannot forge one. `%ArrayIteratorPrototype%.next.call({})` is a
/// TypeError precisely because the plain object has no slot, and a model that kept the state in
/// hidden properties would let a program build an object that passes that check.
#[derive(Debug, Clone)]
pub enum IteratorState {
    /// `[[IteratedObject]]`, `[[ArrayIteratorNextIndex]]`, `[[ArrayIterationKind]]`.
    ///
    /// The target is `None` once the walk has finished, and that is a LATCH rather than
    /// bookkeeping: an exhausted iterator must keep answering `{done: true}` forever, even if the
    /// array it came from grows afterwards.
    Array { target: Option<ObjectId>, index: f64, kind: IterationKind },
    /// A string iterator holds the STRING, not the object it came from. `String.prototype.at`
    /// reads through the receiver every time; this snapshots, because `CreateStringIterator` does
    /// `ToString(this)` once and a String value cannot change afterwards.
    String { text: JsString, index: usize, done: bool },
    /// A `Map` or `Set` iterator.
    ///
    /// IT HOLDS AN INDEX INTO THE ENTRY LIST AND RE-READS THAT LIST EVERY STEP, which is what
    /// makes the specified live behaviour fall out: an entry ADDED during iteration is visited, and
    /// one DELETED before the walk reaches it is not. Snapshotting the entries at creation would be
    /// simpler and would be a different, quieter language.
    ///
    /// That is also why a deleted entry is tombstoned rather than removed -- see
    /// [`crate::object::Collection`]. Compacting would shift every later index under a live
    /// iterator's feet.
    Collection { target: Option<ObjectId>, index: usize, kind: IterationKind },
    /// `matchAll`'s walk over a subject.
    ///
    /// It holds a CLONE of the pattern rather than the caller's, so iterating does not move a
    /// cursor the caller is using and two concurrent walks do not interfere. The pattern is `None`
    /// once the walk has finished, which is the same latch the array iterator uses and for the same
    /// reason: an exhausted iterator answers `{done: true}` forever.
    RegExpString {
        pattern: Option<ObjectId>,
        text: JsString,
        global: bool,
        code_point_mode: bool,
    },
}

/// Which of the three views of an indexed collection an iterator yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IterationKind {
    Key,
    Value,
    KeyAndValue,
}

/// An Iterator Record: the iterator, the `next` method **captured at the start**, and whether the
/// walk is over.
///
/// **`next` IS A CAPTURED VALUE AND NOT A LOOKUP, AND A PROGRAM CAN TELL.** Re-reading
/// `iterator.next` on each step means an iterator that replaces its own `next` mid-walk changes what
/// drives it -- which no conforming engine does, and which nothing would report, because the loop
/// still terminates and still produces values.
pub(crate) struct IteratorRecord {
    pub iterator: ObjectId,
    pub next_method: JsValue,
    /// Set once the walk has ended or failed. A record marked done is never closed: an iterator that
    /// has already reported completion, or that threw out of its own `next`, is not asked to clean
    /// up after itself.
    pub done: bool,
}

/// `GetMethod(V, P)`: a property that must be callable if it is there at all.
///
/// **`undefined` AND `null` BOTH MEAN "ABSENT" HERE, AND EVERYTHING ELSE THAT IS NOT CALLABLE IS A
/// TypeError.** That asymmetry is the standard's and it is load-bearing twice below: an object with
/// no `[Symbol.iterator]` is "not iterable" (a TypeError raised by the caller with a better message),
/// while an object whose `[Symbol.iterator]` is `42` is a TypeError raised here.
pub(crate) fn get_method(
    interpreter: &mut Interpreter,
    value: &JsValue,
    key: &PropertyKey,
) -> Result<Option<JsValue>, Completion> {
    let function = match interpreter.get_member(value, key) {
        Completion::Normal(function) => function,
        abrupt => return Err(abrupt),
    };
    match function {
        JsValue::Undefined | JsValue::Null => Ok(None),
        function if interpreter.is_callable(&function) => Ok(Some(function)),
        _ => Err(interpreter.type_error(&format!("{} is not a function", key.to_display()))),
    }
}

/// `GetIterator(obj, sync)`: asks a value for its iterator and captures the `next` method.
///
/// The three failures are distinct and a program can tell them apart: no `[Symbol.iterator]` at
/// all, one that is not callable, and one that returns a primitive. Collapsing them into one message
/// costs nothing at run time and costs a reader the cause.
pub(crate) fn get_iterator(
    interpreter: &mut Interpreter,
    value: &JsValue,
) -> Result<IteratorRecord, Completion> {
    let key = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));
    let Some(method) = get_method(interpreter, value, &key)? else {
        return Err(interpreter.type_error(&format!("{} is not iterable", describe(value))));
    };
    get_iterator_from_method(interpreter, value, &method)
}

/// `GetIteratorFromMethod(obj, method)`: the half of `GetIterator` that runs once the method is
/// already in hand.
///
/// # IT EXISTS SO A CALLER THAT ALREADY LOOKED THE METHOD UP DOES NOT LOOK IT UP AGAIN
///
/// `Array.from` asks `GetMethod(items, @@iterator)` to decide WHICH of its two algorithms to run,
/// and then the standard hands that same method to `GetIteratorFromMethod`. Calling `GetIterator`
/// instead reads `@@iterator` a SECOND time, and a program can see it: an accessor that counts its
/// reads reports two, and one that answers differently the second time picks the branch under one
/// method and iterates under another.
pub(crate) fn get_iterator_from_method(
    interpreter: &mut Interpreter,
    value: &JsValue,
    method: &JsValue,
) -> Result<IteratorRecord, Completion> {
    let iterator = match interpreter.call_value(method, value.clone(), Vec::new()) {
        Completion::Normal(iterator) => iterator,
        abrupt => return Err(abrupt),
    };
    let JsValue::Object(iterator) = iterator else {
        return Err(interpreter.type_error("an iterator must be an object"));
    };
    let next_method = match interpreter.get_member(
        &JsValue::Object(iterator),
        &PropertyKey::from_str("next"),
    ) {
        Completion::Normal(next) => next,
        abrupt => return Err(abrupt),
    };
    Ok(IteratorRecord { iterator, next_method, done: false })
}

/// `IteratorNext`: one step, whose result must be an object.
pub(crate) fn iterator_next(
    interpreter: &mut Interpreter,
    record: &mut IteratorRecord,
) -> Result<ObjectId, Completion> {
    let iterator = JsValue::Object(record.iterator);
    let result = match interpreter.call_value(&record.next_method, iterator, Vec::new()) {
        Completion::Normal(result) => result,
        abrupt => {
            record.done = true;
            return Err(abrupt);
        }
    };
    match result {
        JsValue::Object(id) => Ok(id),
        _ => {
            record.done = true;
            Err(interpreter.type_error("an iterator result must be an object"))
        }
    }
}

/// `IteratorStep`: a step, or `None` when the walk is over.
///
/// **`done` IS READ AND `value` IS NOT.** A result object whose `value` is a getter must not have
/// that getter run when `done` is true -- the loop is over and the value was never asked for.
pub(crate) fn iterator_step(
    interpreter: &mut Interpreter,
    record: &mut IteratorRecord,
) -> Result<Option<ObjectId>, Completion> {
    let result = iterator_next(interpreter, record)?;
    let done = match interpreter
        .get_member(&JsValue::Object(result), &PropertyKey::from_str("done"))
    {
        Completion::Normal(done) => done,
        abrupt => {
            record.done = true;
            return Err(abrupt);
        }
    };
    if crate::abstract_ops::to_boolean(&done) {
        record.done = true;
        return Ok(None);
    }
    Ok(Some(result))
}

/// `IteratorValue`: the value out of a step's result object.
pub(crate) fn iterator_value(
    interpreter: &mut Interpreter,
    result: ObjectId,
) -> Result<JsValue, Completion> {
    match interpreter.get_member(&JsValue::Object(result), &PropertyKey::from_str("value")) {
        Completion::Normal(value) => Ok(value),
        abrupt => Err(abrupt),
    }
}

/// `IteratorClose(iteratorRecord, completion)`: tells an iterator the consumer has stopped early.
///
/// # THE ORIGINAL COMPLETION WINS, AND THAT IS THE WHOLE ALGORITHM
///
/// Four separate things can go wrong here -- reading `return`, `return` not being callable, calling
/// it, and it returning a primitive -- and **every one of them is DISCARDED when the reason for
/// closing was a throw.** The rule is that a program must see the error its own code raised, not one
/// raised by the cleanup that error triggered; an engine that lets the cleanup's error through
/// reports a `return()` bug at the site of an unrelated `throw` and buries the real cause.
///
/// The mirror case is just as specified and easy to get wrong in the other direction: when the loop
/// ended NORMALLY -- or by `break` or `return`, which are also normal reasons to stop -- a throwing
/// `return()` **does** propagate, and a `return()` that hands back a primitive **is** a TypeError.
/// So this cannot be written as "try to close, ignore failures": that silently passes the cases the
/// standard makes loud.
pub(crate) fn iterator_close(
    interpreter: &mut Interpreter,
    record: &IteratorRecord,
    completion: Completion,
) -> Completion {
    if record.done {
        return completion;
    }
    let original_threw = matches!(completion, Completion::Throw(_));
    let iterator = JsValue::Object(record.iterator);

    let inner = match get_method(interpreter, &iterator, &PropertyKey::from_str("return")) {
        Ok(None) => return completion,
        Ok(Some(method)) => interpreter.call_value(&method, iterator, Vec::new()),
        Err(abrupt) => abrupt,
    };

    if original_threw {
        return completion;
    }
    match inner {
        Completion::Normal(value) if value.is_object() => completion,
        Completion::Normal(_) => {
            interpreter.type_error("an iterator's return() must return an object")
        }
        abrupt => abrupt,
    }
}

/// Drives an iterator to exhaustion, collecting the values. Spread and array destructuring rest
/// elements are both exactly this.
pub(crate) fn iterate_to_list(
    interpreter: &mut Interpreter,
    value: &JsValue,
) -> Result<Vec<JsValue>, Completion> {
    let mut record = get_iterator(interpreter, value)?;
    let mut values = Vec::new();
    loop {
        match iterator_step(interpreter, &mut record) {
            Ok(None) => return Ok(values),
            Ok(Some(result)) => match iterator_value(interpreter, result) {
                Ok(value) => values.push(value),
                Err(abrupt) => return Err(abrupt),
            },
            Err(abrupt) => return Err(abrupt),
        }
    }
}

/// `CreateIterResultObject(value, done)`. Ordinary properties in the specified order: `value`, then
/// `done`. A program can see the order through `Object.keys`.
/// `CreateDataProperty`, NOT assignment. The standard says so, and the difference stopped being
/// theoretical when `Object.prototype` grew a `__proto__` accessor: an assignment consults the
/// prototype chain, so a program that installs a `value` or `done` setter on `Object.prototype`
/// would intercept every iteration result in the realm and leave the object empty.
pub(crate) fn iter_result(interpreter: &mut Interpreter, value: JsValue, done: bool) -> JsValue {
    let id = interpreter.allocate(Object::new(Some(interpreter.intrinsics.object_prototype)));
    let _ = interpreter.create_data_property(id, PropertyKey::from_str("value"), value);
    let _ = interpreter.create_data_property(id, PropertyKey::from_str("done"), JsValue::Boolean(done));
    JsValue::Object(id)
}

/// How a value is named in a "not iterable" message. Deliberately short of running any user code:
/// a diagnostic about a failed iteration must not itself call `toString` on the thing that failed.
fn describe(value: &JsValue) -> &'static str {
    match value {
        JsValue::Undefined => "undefined",
        JsValue::Null => "null",
        JsValue::Boolean(_) => "a boolean",
        JsValue::Number(_) => "a number",
        JsValue::String(_) => "a string",
        JsValue::Symbol(_) => "a symbol",
        JsValue::Object(_) => "this value",
    }
}


/// Builds `%IteratorPrototype%`, `%ArrayIteratorPrototype%`, `%StringIteratorPrototype%` and the
/// methods that produce them.
///
/// **THIS RUNS AFTER `install_symbol`, AND IT HAS TO.** Every entry point here is a symbol-keyed
/// property, so the well-known symbols must already exist as values. That ordering is why the array
/// and string iteration methods live here rather than beside their neighbours in `install_array` and
/// `install_string` -- and it is the better home anyway, because they are one feature.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let iterator_symbol = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));

    let iterator_prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.iterator_prototype = iterator_prototype;
    let self_iterator = interpreter.native_function("[Symbol.iterator]", 0, |_, this, _| {
        Completion::Normal(this)
    });
    interpreter
        .object_mut(iterator_prototype)
        .set_own(iterator_symbol.clone(), Property::builtin(JsValue::Object(self_iterator)));

    install_array_iterator(interpreter, &iterator_symbol);
    install_string_iterator(interpreter, &iterator_symbol);

    let array_constructor = interpreter.intrinsics.array_constructor;
    interpreter.define_method(array_constructor, "from", 1, array_from);
    interpreter.define_method(array_constructor, "of", 0, array_of);

}

/// `GroupBy(items, callback, keyCoercion)`: the walk both `groupBy`s run, in insertion order.
///
/// THE GROUPS ARE A LIST AND NOT A MAP, because ORDER IS THE ANSWER: the first time a key is
/// seen fixes where its group appears in the result, and a later element joins the group already
/// there. Collecting into a hash and reading it back gives the right elements in the wrong order.
///
/// `keyCoercion` is the ONE difference between the two callers, and it is not cosmetic:
/// `Object.groupBy` runs `ToPropertyKey`, so `1` and `"1"` are one group and a `toString` runs on
/// every key; `Map.groupBy` compares with SameValueZero, so they are two groups, an object may BE a
/// key, and nothing is converted -- except `-0`, which becomes `+0` because a Map has no other.
fn group_by(
    interpreter: &mut Interpreter,
    items: &JsValue,
    callback: &JsValue,
    by_property: bool,
) -> Result<Vec<(JsValue, Vec<JsValue>)>, Completion> {
    if matches!(items, JsValue::Undefined | JsValue::Null) {
        return Err(interpreter.type_error("cannot group null or undefined"));
    }
    if !interpreter.is_callable(callback) {
        return Err(interpreter.type_error("the grouping callback is not a function"));
    }
    let mut record = get_iterator(interpreter, items)?;
    let mut groups: Vec<(JsValue, Vec<JsValue>)> = Vec::new();
    let mut index = 0.0f64;
    loop {
        let result = match iterator_step(interpreter, &mut record)? {
            None => return Ok(groups),
            Some(result) => result,
        };
        let value = iterator_value(interpreter, result)?;
        let key = match interpreter.call_value(
            callback,
            JsValue::Undefined,
            crate::vec![value.clone(), JsValue::Number(index)],
        ) {
            Completion::Normal(key) => key,
            abrupt => return Err(iterator_close(interpreter, &record, abrupt)),
        };
        let key = if by_property {
            match interpreter.to_property_key_value(&key) {
                Ok(key) => match key {
                    PropertyKey::String(text) => JsValue::String(text),
                    PropertyKey::Symbol(id) => JsValue::Symbol(id),
                },
                Err(abrupt) => return Err(iterator_close(interpreter, &record, abrupt)),
            }
        } else if matches!(key, JsValue::Number(n) if n == 0.0) {
            JsValue::Number(0.0)
        } else {
            key
        };
        match groups.iter_mut().find(|(existing, _)| {
            crate::abstract_ops::same_value_zero(existing, &key)
        }) {
            Some((_, elements)) => elements.push(value),
            None => groups.push((key, crate::vec![value])),
        }
        index += 1.0;
    }
}

/// THE RESULT HAS A **NULL PROTOTYPE**, so a group keyed `"toString"` is the group and not the
/// inherited method. An ordinary object here would make `Object.groupBy(xs, f).constructor` answer
/// something that was never grouped.
pub(crate) fn object_group_by(
    interpreter: &mut Interpreter,
    _this: JsValue,
    arguments: &[JsValue],
) -> Completion {
    let groups = match group_by(interpreter, &arg(arguments, 0), &arg(arguments, 1), true) {
        Ok(groups) => groups,
        Err(abrupt) => return abrupt,
    };
    let result = interpreter.allocate(crate::object::Object::new(None));
    for (key, elements) in groups {
        let key = match key {
            JsValue::Symbol(id) => PropertyKey::Symbol(id),
            other => PropertyKey::from_str(&interpreter.to_string_value(&other).map_or_else(
                |_| crate::String::new(),
                |text| text.to_lossy_string(),
            )),
        };
        let array = interpreter.new_array(elements);
        if let Err(abrupt) = crate::builtins::create_data_property_or_throw(
            interpreter,
            result,
            key,
            JsValue::Object(array),
        ) {
            return abrupt;
        }
    }
    Completion::Normal(JsValue::Object(result))
}

/// THE ENTRIES ARE APPENDED TO THE MAP'S DATA DIRECTLY. The standard says so in as many words --
/// it does not call `Map.prototype.set` -- so a program that has replaced `set` cannot observe or
/// divert the grouping.
pub(crate) fn map_group_by(
    interpreter: &mut Interpreter,
    _this: JsValue,
    arguments: &[JsValue],
) -> Completion {
    let groups = match group_by(interpreter, &arg(arguments, 0), &arg(arguments, 1), false) {
        Ok(groups) => groups,
        Err(abrupt) => return abrupt,
    };
    let mut entries = Vec::new();
    for (key, elements) in groups {
        let array = interpreter.new_array(elements);
        entries.push(Some((key, JsValue::Object(array))));
    }
    let live = entries.len();
    let prototype = interpreter.intrinsics.map_prototype;
    let mut object = crate::object::Object::new(Some(prototype));
    object.collection = Some(crate::Box::new(crate::object::Collection {
        entries,
        live,
        kind: crate::object::CollectionKind::Map,
    }));
    Completion::Normal(JsValue::Object(interpreter.allocate(object)))
}

fn install_array_iterator(interpreter: &mut Interpreter, iterator_symbol: &PropertyKey) {
    let prototype = interpreter.allocate(Object::new(Some(interpreter.intrinsics.iterator_prototype)));
    interpreter.intrinsics.array_iterator_prototype = prototype;
    interpreter.define_method(prototype, "next", 0, array_iterator_next);
    define_to_string_tag(interpreter, prototype, "Array Iterator");

    let array_prototype = interpreter.intrinsics.array_prototype;
    interpreter.define_method(array_prototype, "keys", 0, |interpreter, this, _| {
        create_array_iterator(interpreter, &this, IterationKind::Key)
    });
    interpreter.define_method(array_prototype, "entries", 0, |interpreter, this, _| {
        create_array_iterator(interpreter, &this, IterationKind::KeyAndValue)
    });
    interpreter.define_method(array_prototype, "values", 0, |interpreter, this, _| {
        create_array_iterator(interpreter, &this, IterationKind::Value)
    });

    let values = interpreter
        .object(array_prototype)
        .own(&PropertyKey::from_str("values"))
        .and_then(|property| property.data_value().cloned())
        .unwrap_or(JsValue::Undefined);
    if let JsValue::Object(id) = values {
        interpreter.intrinsics.array_prototype_values = id;
    }
    interpreter
        .object_mut(array_prototype)
        .set_own(iterator_symbol.clone(), Property::builtin(values));
}

fn install_string_iterator(interpreter: &mut Interpreter, iterator_symbol: &PropertyKey) {
    let prototype = interpreter.allocate(Object::new(Some(interpreter.intrinsics.iterator_prototype)));
    interpreter.intrinsics.string_iterator_prototype = prototype;
    interpreter.define_method(prototype, "next", 0, string_iterator_next);
    define_to_string_tag(interpreter, prototype, "String Iterator");

    let string_prototype = interpreter.intrinsics.string_prototype;
    let method = interpreter.native_function("[Symbol.iterator]", 0, |interpreter, this, _| {
        if matches!(this, JsValue::Undefined | JsValue::Null) {
            return interpreter.type_error("cannot iterate null or undefined as a string");
        }
        let text = match interpreter.to_string_value(&this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let prototype = interpreter.intrinsics.string_iterator_prototype;
        let mut object = Object::new(Some(prototype));
        object.iterator_state = Some(crate::Box::new(IteratorState::String { text, index: 0, done: false }));
        Completion::Normal(JsValue::Object(interpreter.allocate(object)))
    });
    interpreter
        .object_mut(string_prototype)
        .set_own(iterator_symbol.clone(), Property::builtin(JsValue::Object(method)));
}

/// `CreateMapIterator` / `CreateSetIterator`, which differ only in the prototype they hand back.
pub(crate) fn create_collection_iterator(
    interpreter: &mut Interpreter,
    target: ObjectId,
    kind: IterationKind,
    prototype: ObjectId,
) -> Completion {
    let mut object = Object::new(Some(prototype));
    object.iterator_state =
        Some(crate::Box::new(IteratorState::Collection { target: Some(target), index: 0, kind }));
    Completion::Normal(JsValue::Object(interpreter.allocate(object)))
}

pub(crate) fn collection_iterator_next(
    interpreter: &mut Interpreter,
    this: JsValue,
    _arguments: &[JsValue],
) -> Completion {
    let JsValue::Object(id) = this else {
        return interpreter.type_error("next() requires a Map or Set Iterator");
    };
    let Some(IteratorState::Collection { target, index, kind }) =
        interpreter.object(id).iterator_state.as_deref().cloned()
    else {
        return interpreter.type_error("next() requires a Map or Set Iterator");
    };
    let Some(target) = target else {
        return Completion::Normal(iter_result(interpreter, JsValue::Undefined, true));
    };

    let mut at = index;
    loop {
        let entry = match &interpreter.object(target).collection {
            Some(collection) => collection.entries.get(at).cloned(),
            None => return interpreter.type_error("next() requires a Map or Set Iterator"),
        };
        let Some(slot) = entry else {
            set_collection_target(interpreter, id, None);
            return Completion::Normal(iter_result(interpreter, JsValue::Undefined, true));
        };
        at += 1;
        let Some((key, value)) = slot else { continue };
        set_collection_index(interpreter, id, at);
        let yielded = match kind {
            IterationKind::Key => key,
            IterationKind::Value => value,
            IterationKind::KeyAndValue => {
                JsValue::Object(interpreter.new_array(crate::vec![key, value]))
            }
        };
        return Completion::Normal(iter_result(interpreter, yielded, false));
    }
}

fn set_collection_target(interpreter: &mut Interpreter, id: ObjectId, target: Option<ObjectId>) {
    if let Some(IteratorState::Collection { target: slot, .. }) =
        interpreter.object_mut(id).iterator_state.as_deref_mut()
    {
        *slot = target;
    }
}

fn set_collection_index(interpreter: &mut Interpreter, id: ObjectId, next: usize) {
    if let Some(IteratorState::Collection { index, .. }) =
        interpreter.object_mut(id).iterator_state.as_deref_mut()
    {
        *index = next;
    }
}

/// `Symbol.toStringTag` on an iterator prototype is non-writable and **configurable**, which is
/// neither of the two shapes the realm builder already had. Using `define_constant` would make it
/// non-configurable and fail `verifyProperty` on every iterator test at once.
pub(crate) fn define_to_string_tag(interpreter: &mut Interpreter, target: ObjectId, tag: &str) {
    interpreter.define_to_string_tag(target, tag);
}

/// `CreateArrayIterator`. `ToObject` first, so `Array.prototype.values.call("ab")` iterates the
/// wrapper's index properties rather than throwing.
pub(crate) fn create_array_iterator(
    interpreter: &mut Interpreter,
    this: &JsValue,
    kind: IterationKind,
) -> Completion {
    let target = match interpreter.to_object(this) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let prototype = interpreter.intrinsics.array_iterator_prototype;
    let mut object = Object::new(Some(prototype));
    object.iterator_state =
        Some(crate::Box::new(IteratorState::Array { target: Some(target), index: 0.0, kind }));
    Completion::Normal(JsValue::Object(interpreter.allocate(object)))
}

fn array_iterator_next(
    interpreter: &mut Interpreter,
    this: JsValue,
    _arguments: &[JsValue],
) -> Completion {
    let JsValue::Object(id) = this else {
        return interpreter.type_error("next() requires an Array Iterator");
    };
    let Some(IteratorState::Array { target, index, kind }) =
        interpreter.object(id).iterator_state.as_deref().cloned()
    else {
        return interpreter.type_error("next() requires an Array Iterator");
    };
    let Some(target) = target else {
        return Completion::Normal(iter_result(interpreter, JsValue::Undefined, true));
    };

    let length = if interpreter.typed_array_view(target).is_some() {
        match crate::typed_array::validated_view(interpreter, &JsValue::Object(target)) {
            Ok(view) => view.length as f64,
            Err(abrupt) => return exhaust_array(interpreter, id, abrupt),
        }
    } else {
        match array_length(interpreter, &JsValue::Object(target)) {
            Ok(length) => length,
            Err(abrupt) => return exhaust_array(interpreter, id, abrupt),
        }
    };

    if index >= length {
        set_array_target(interpreter, id, None);
        return Completion::Normal(iter_result(interpreter, JsValue::Undefined, true));
    }

    let value = if kind == IterationKind::Key {
        JsValue::Number(index)
    } else {
        let key = PropertyKey::from_str(&crate::abstract_ops::number_to_string(index));
        let element = match interpreter.get_member(&JsValue::Object(target), &key) {
            Completion::Normal(element) => element,
            abrupt => return exhaust_array(interpreter, id, abrupt),
        };
        if kind == IterationKind::Value {
            element
        } else {
            JsValue::Object(interpreter.new_array(crate::vec![JsValue::Number(index), element]))
        }
    };
    set_array_index(interpreter, id, index + 1.0);
    Completion::Normal(iter_result(interpreter, value, false))
}

/// **AN ITERATOR THAT THREW IS FINISHED.** The standard builds these out of generator closures,
/// and an abrupt completion moves a generator to `completed` -- so a `next()` that threw is followed
/// by `{done: true}`, not by a retry of the same index. Leaving the state untouched instead gives an
/// iterator that raises the same error forever, which turns one throwing element getter into an
/// infinite loop in any consumer that catches and continues.
fn exhaust_array(interpreter: &mut Interpreter, id: ObjectId, abrupt: Completion) -> Completion {
    set_array_target(interpreter, id, None);
    abrupt
}

fn set_array_target(interpreter: &mut Interpreter, id: ObjectId, target: Option<ObjectId>) {
    if let Some(IteratorState::Array { target: slot, .. }) =
        interpreter.object_mut(id).iterator_state.as_deref_mut()
    {
        *slot = target;
    }
}

fn set_array_index(interpreter: &mut Interpreter, id: ObjectId, next: f64) {
    if let Some(IteratorState::Array { index, .. }) =
        interpreter.object_mut(id).iterator_state.as_deref_mut()
    {
        *index = next;
    }
}

/// **A STRING ITERATOR ADVANCES BY CODE POINT, AND IT IS THE ONE PLACE THE LANGUAGE READS A
/// STRING DIFFERENTLY FROM `s[i]`.** `"\u{1F600}".length` is 2 and `s[0]` is a lone high surrogate,
/// but `[..."\u{1F600}"]` has ONE element. An iterator built on `unit_at` produces two broken halves
/// of an emoji and looks completely correct on every ASCII test in the corpus.
///
/// And a LONE surrogate is yielded VERBATIM as a one-unit string. It is not an error and it is not
/// replaced -- `[..."\uD834"]` is `["\uD834"]`. Substituting U+FFFD here would be the same loss this
/// engine built [`JsString`] to avoid, reintroduced at the one operation that walks a string.
fn string_iterator_next(
    interpreter: &mut Interpreter,
    this: JsValue,
    _arguments: &[JsValue],
) -> Completion {
    let JsValue::Object(id) = this else {
        return interpreter.type_error("next() requires a String Iterator");
    };
    let Some(IteratorState::String { text, index, done }) =
        interpreter.object(id).iterator_state.as_deref().cloned()
    else {
        return interpreter.type_error("next() requires a String Iterator");
    };
    if done || index >= text.len() {
        if let Some(IteratorState::String { done, .. }) =
            interpreter.object_mut(id).iterator_state.as_deref_mut()
        {
            *done = true;
        }
        return Completion::Normal(iter_result(interpreter, JsValue::Undefined, true));
    }

    let units = text.units();
    let first = units[index];
    let width = if (0xD800..0xDC00).contains(&first)
        && index + 1 < units.len()
        && (0xDC00..0xE000).contains(&units[index + 1])
    {
        2
    } else {
        1
    };
    let value = JsValue::String(JsString::from_units(&units[index..index + width]));
    if let Some(IteratorState::String { index: slot, .. }) =
        interpreter.object_mut(id).iterator_state.as_deref_mut()
    {
        *slot = index + width;
    }
    Completion::Normal(iter_result(interpreter, value, false))
}

/// `Array.from(items, mapfn?, thisArg?)` -- the iterator protocol's most-used consumer, and the one
/// the Test262 harness itself reaches for.
///
/// It takes the ITERATOR path when the argument is iterable and the ARRAY-LIKE path when it is
/// not, and the two are genuinely different programs: the array-like path reads `length` once and
/// indexes, the iterator path never reads `length` at all.
pub(crate) fn array_from(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
) -> Completion {
    let items = arg(arguments, 0);
    let mapper = arg(arguments, 1);
    let this_arg = arg(arguments, 2);
    if !matches!(mapper, JsValue::Undefined) && !interpreter.is_callable(&mapper) {
        return interpreter.type_error("Array.from's mapping function is not a function");
    }

    let key = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));
    let using_iterator = match get_method(interpreter, &items, &key) {
        Ok(method) => method,
        Err(abrupt) => return abrupt,
    };

    let mut values = Vec::new();
    if let Some(using_iterator) = using_iterator {
        let target = match construct_result(interpreter, &this, None) {
            Ok(target) => target,
            Err(abrupt) => return abrupt,
        };
        let mut record = match get_iterator_from_method(interpreter, &items, &using_iterator) {
            Ok(record) => record,
            Err(abrupt) => return abrupt,
        };
        let mut index = 0.0f64;
        loop {
            let result = match iterator_step(interpreter, &mut record) {
                Ok(None) => break,
                Ok(Some(result)) => result,
                Err(abrupt) => return abrupt,
            };
            let value = match iterator_value(interpreter, result) {
                Ok(value) => value,
                Err(abrupt) => return abrupt,
            };
            let value = match map_one(interpreter, &mapper, &this_arg, value, index) {
                Ok(value) => value,
                Err(abrupt) => return iterator_close(interpreter, &record, abrupt),
            };
            let key = PropertyKey::from_str(&crate::abstract_ops::number_to_string(index));
            if let Err(abrupt) =
                crate::builtins::create_data_property_or_throw(interpreter, target, key, value)
            {
                return iterator_close(interpreter, &record, abrupt);
            }
            index += 1.0;
        }
        let completion = interpreter.set_property_or_throw(
            target,
            PropertyKey::from_str("length"),
            JsValue::Number(index),
        );
        if completion.is_abrupt() {
            return completion;
        }
        return Completion::Normal(JsValue::Object(target));
    }
    {
        let target = match interpreter.to_object(&items) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &JsValue::Object(target)) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let mut index = 0.0f64;
        while index < length {
            let key = PropertyKey::from_str(&crate::abstract_ops::number_to_string(index));
            let value = match interpreter.get_member(&JsValue::Object(target), &key) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            match map_one(interpreter, &mapper, &this_arg, value, index) {
                Ok(value) => values.push(value),
                Err(abrupt) => return abrupt,
            }
            index += 1.0;
        }
    }
    collect_into(interpreter, &this, values)
}

fn map_one(
    interpreter: &mut Interpreter,
    mapper: &JsValue,
    this_arg: &JsValue,
    value: JsValue,
    index: f64,
) -> Result<JsValue, Completion> {
    if matches!(mapper, JsValue::Undefined) {
        return Ok(value);
    }
    match interpreter.call_value(
        mapper,
        this_arg.clone(),
        crate::vec![value, JsValue::Number(index)],
    ) {
        Completion::Normal(mapped) => Ok(mapped),
        abrupt => Err(abrupt),
    }
}

/// `Array.of(...items)`. Not the iterator protocol at all -- it is here because it is the other half
/// of the pair a reader looks for, and putting it anywhere else invites someone to implement
/// `Array.from`'s semantics for it.
pub(crate) fn array_of(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
) -> Completion {
    collect_into(interpreter, &this, arguments.to_vec())
}

/// Steps 4 through 9 of both `Array.of` and `Array.from`: build the result THROUGH the receiver
/// when it is a constructor, define each element, then set `length`.
///
/// # THE RECEIVER IS THE CONSTRUCTOR, AND BOTH OF THESE IGNORED IT
///
/// `Array.of` and `Array.from` are inherited by every Array subclass, and `MyArray.from(xs)` must
/// produce a `MyArray`. Building a plain array whatever the receiver was is the silent-degradation
/// shape `@@species` exists to prevent, in the two methods that take their constructor from `this`
/// rather than from a property.
///
/// AND THE ELEMENTS ARE **DEFINED**, NOT ASSIGNED. `CreateDataPropertyOrThrow` must report a
/// refusal, so a constructor that returns a frozen object makes the call throw rather than hand
/// back an object with none of the elements in it.
/// `IsConstructor(ctor) ? Construct(ctor, args) : ArrayCreate(...)`: the object `Array.of` and both
/// halves of `Array.from` build their result in.
///
/// **`length` IS AN ARGUMENT AND NOT A CONSTANT.** `Array.from`'s iterator path constructs with
/// NO arguments and its array-like path constructs with the length; `Array.of` passes its count.
/// One function with the argument passed IN, rather than three sites each deciding -- which is how
/// the iterator path came to send a length the standard never sends.
fn construct_result(
    interpreter: &mut Interpreter,
    receiver: &JsValue,
    length: Option<f64>,
) -> Result<ObjectId, Completion> {
    match receiver {
        JsValue::Object(id) if interpreter.is_constructor(*id) => {
            let arguments = match length {
                Some(length) => crate::vec![JsValue::Number(length)],
                None => Vec::new(),
            };
            match interpreter.construct(*id, arguments) {
                Completion::Normal(JsValue::Object(built)) => Ok(built),
                Completion::Normal(_) => {
                    Err(interpreter.type_error("that constructor did not produce an object"))
                }
                abrupt => Err(abrupt),
            }
        }
        _ => Ok(interpreter.new_array(Vec::new())),
    }
}

fn collect_into(interpreter: &mut Interpreter, receiver: &JsValue, values: Vec<JsValue>) -> Completion {
    let length = values.len() as f64;
    let target = match construct_result(interpreter, receiver, Some(length)) {
        Ok(target) => target,
        Err(abrupt) => return abrupt,
    };
    for (index, value) in values.into_iter().enumerate() {
        let key = PropertyKey::from_str(&crate::abstract_ops::number_to_string(index as f64));
        if let Err(abrupt) = crate::builtins::create_data_property_or_throw(
            interpreter, target, key, value,
        ) {
            return abrupt;
        }
    }
    let completion = interpreter.set_property_or_throw(
        target,
        PropertyKey::from_str("length"),
        JsValue::Number(length),
    );
    if completion.is_abrupt() {
        return completion;
    }
    Completion::Normal(JsValue::Object(target))
}
