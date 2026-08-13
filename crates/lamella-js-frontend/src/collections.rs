//! `Map` and `Set`.

use crate::abstract_ops::same_value_zero;
use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::iterator::{create_collection_iterator, collection_iterator_next, IterationKind};
use crate::object::{Collection, CollectionKind, Object, Property, PropertyKey, PropertyKind};
use crate::value::{JsValue, ObjectId};
use crate::Vec;

pub(crate) fn install(interpreter: &mut Interpreter) {
    install_one(interpreter, CollectionKind::Map, construct_map);
    install_one(interpreter, CollectionKind::Set, construct_set);
    install_weak(interpreter, CollectionKind::WeakMap);
    install_weak(interpreter, CollectionKind::WeakSet);
}

/// `WeakMap` and `WeakSet`.
///
/// # THEY ARE NOT A `Map` WITH A FLAG, AND THE SURFACE IS THE SMALLER HALF OF WHY
///
/// No `size`, no `clear`, no `forEach`, no iterators, no `Symbol.iterator`. That is not an
/// abbreviation of `Map` -- **every one of those would let a program observe collection**, which is
/// the property a weak collection exists to withhold. A `size` that changes because a collector ran
/// makes garbage collection part of the language's observable semantics.
///
/// # THE RETENTION IS A LISTED DEVIATION, NOT A SECRET
///
/// This engine's collector has no weak references, so **the entries here are held STRONGLY**: a key
/// that becomes unreachable everywhere else is still retained by the collection. From inside
/// JavaScript that is INDISTINGUISHABLE from a conforming implementation -- there is no way to ask
/// whether a key was collected, which is exactly why the surface above is so small -- so every
/// conformance test passes either way. It is a MEMORY deviation, and it is written here rather than
/// discovered by a long-running program that grows without an obvious cause.
///
/// Filling it is the weak-reference substrate the project already has on its v1 list for three
/// languages; it is not a change to this module.
fn install_weak(interpreter: &mut Interpreter, kind: CollectionKind) {
    let is_map = kind == CollectionKind::WeakMap;
    let name = if is_map { "WeakMap" } else { "WeakSet" };
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    if is_map {
        interpreter.intrinsics.weak_map_prototype = prototype;
    } else {
        interpreter.intrinsics.weak_set_prototype = prototype;
    }

    let constructor = interpreter.native_constructor(
        name,
        0,
        |interpreter, _this, _arguments| {
            interpreter.type_error("this constructor requires `new`")
        },
        if is_map { construct_weak_map } else { construct_weak_set },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global(name, JsValue::Object(constructor));
    to_string_tag(interpreter, prototype, name);

    interpreter.define_method(prototype, "has", 1, if is_map { weak_map_has } else { weak_set_has });
    interpreter
        .define_method(prototype, "delete", 1, if is_map { weak_map_delete } else { weak_set_delete });
    if is_map {
        interpreter.define_method(prototype, "get", 1, weak_map_get);
        interpreter.define_method(prototype, "set", 2, weak_map_set);
    } else {
        interpreter.define_method(prototype, "add", 1, weak_set_add);
    }
}

fn construct_weak_map(i: &mut Interpreter, _t: JsValue, a: &[JsValue]) -> Completion {
    construct_weak(i, a, CollectionKind::WeakMap)
}
fn construct_weak_set(i: &mut Interpreter, _t: JsValue, a: &[JsValue]) -> Completion {
    construct_weak(i, a, CollectionKind::WeakSet)
}
fn weak_map_has(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion {
    weak_lookup(i, t, a, CollectionKind::WeakMap, Lookup::Has)
}
fn weak_set_has(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion {
    weak_lookup(i, t, a, CollectionKind::WeakSet, Lookup::Has)
}
fn weak_map_get(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion {
    weak_lookup(i, t, a, CollectionKind::WeakMap, Lookup::Get)
}
fn weak_map_delete(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion {
    weak_lookup(i, t, a, CollectionKind::WeakMap, Lookup::Delete)
}
fn weak_set_delete(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion {
    weak_lookup(i, t, a, CollectionKind::WeakSet, Lookup::Delete)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Has,
    Get,
    Delete,
}

/// A NON-OBJECT KEY IS **NOT AN ERROR** WHEN READING, AND IT IS WHEN WRITING. `wm.get(1)` is
/// `undefined`, `wm.has(1)` is `false`, `wm.delete(1)` is `false` -- but `wm.set(1, x)` is a
/// TypeError. The asymmetry is deliberate in the standard: asking about a key that could never have
/// been stored has an obvious answer, while storing one is a mistake the program wants to hear
/// about. An implementation that threw on all four would break `wm.has(anything)` guards.
fn weak_lookup(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    kind: CollectionKind,
    lookup: Lookup,
) -> Completion {
    let id = match entries_of(interpreter, &this, kind) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let key = arg(arguments, 0);
    if !can_be_held_weakly(&key) {
        return Completion::Normal(match lookup {
            Lookup::Get => JsValue::Undefined,
            _ => JsValue::Boolean(false),
        });
    }
    let Some(at) = find(interpreter, id, &key) else {
        return Completion::Normal(match lookup {
            Lookup::Get => JsValue::Undefined,
            _ => JsValue::Boolean(false),
        });
    };
    match lookup {
        Lookup::Has => Completion::Normal(JsValue::Boolean(true)),
        Lookup::Get => {
            let value = interpreter
                .object(id)
                .collection
                .as_ref()
                .and_then(|c| c.entries[at].as_ref().map(|(_, value)| value.clone()));
            Completion::Normal(value.unwrap_or(JsValue::Undefined))
        }
        Lookup::Delete => {
            if let Some(collection) = &mut interpreter.object_mut(id).collection {
                collection.entries[at] = None;
                collection.live = collection.live.saturating_sub(1);
            }
            Completion::Normal(JsValue::Boolean(true))
        }
    }
}

fn weak_map_set(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let id = match entries_of(interpreter, &this, CollectionKind::WeakMap) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let key = arg(arguments, 0);
    if !can_be_held_weakly(&key) {
        return interpreter.type_error("a WeakMap key must be an object");
    }
    insert(interpreter, id, key, arg(arguments, 1));
    Completion::Normal(this)
}

fn weak_set_add(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let id = match entries_of(interpreter, &this, CollectionKind::WeakSet) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let value = arg(arguments, 0);
    if !can_be_held_weakly(&value) {
        return interpreter.type_error("a WeakSet value must be an object");
    }
    insert(interpreter, id, value.clone(), value);
    Completion::Normal(this)
}

/// `CanBeHeldWeakly`: an object, and nothing else in this profile.
///
/// Later editions admit a non-registered SYMBOL as well, on the grounds that such a symbol has an
/// identity nothing else can forge. This profile has no `Symbol` registry distinction wired to the
/// question, so it admits objects only -- narrower than the newest edition and never wrong about
/// what it does admit.
fn can_be_held_weakly(value: &JsValue) -> bool {
    value.is_object()
}

/// The weak constructors, which are the shared body with a different kind.
fn construct_weak(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
    kind: CollectionKind,
) -> Completion {
    construct_of_kind(interpreter, arguments, kind)
}

fn construct_map(interpreter: &mut Interpreter, _this: JsValue, arguments: &[JsValue]) -> Completion {
    construct(interpreter, arguments, true)
}

fn construct_set(interpreter: &mut Interpreter, _this: JsValue, arguments: &[JsValue]) -> Completion {
    construct(interpreter, arguments, false)
}

fn install_one(
    interpreter: &mut Interpreter,
    kind: CollectionKind,
    construct: crate::interpreter::NativeFn,
) {
    let is_map = kind == CollectionKind::Map;
    let name = if is_map { "Map" } else { "Set" };
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));

    let iterator_prototype =
        interpreter.allocate(Object::new(Some(interpreter.intrinsics.iterator_prototype)));
    interpreter.define_method(iterator_prototype, "next", 0, collection_iterator_next);
    to_string_tag(interpreter, iterator_prototype, if is_map { "Map Iterator" } else { "Set Iterator" });
    if is_map {
        interpreter.intrinsics.map_prototype = prototype;
        interpreter.intrinsics.map_iterator_prototype = iterator_prototype;
    } else {
        interpreter.intrinsics.set_prototype = prototype;
        interpreter.intrinsics.set_iterator_prototype = iterator_prototype;
    }

    let constructor = interpreter.native_constructor(
        name,
        0,
        |interpreter, _this, _arguments| {
            interpreter.type_error("this constructor requires `new`")
        },
        construct,
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global(name, JsValue::Object(constructor));
    to_string_tag(interpreter, prototype, name);
    interpreter.define_species_getter(constructor);

    let getter = interpreter.native_function(
        "get size",
        0,
        if is_map { map_size } else { set_size },
    );
    interpreter.object_mut(prototype).set_own(
        PropertyKey::from_str("size"),
        Property {
            kind: PropertyKind::Accessor { get: Some(getter), set: None },
            enumerable: false,
            configurable: true,
        },
    );

    interpreter.define_method(prototype, "has", 1, if is_map { map_has } else { set_has });
    interpreter.define_method(prototype, "delete", 1, if is_map { map_delete } else { set_delete });
    interpreter.define_method(prototype, "clear", 0, if is_map { map_clear } else { set_clear });
    interpreter
        .define_method(prototype, "forEach", 1, if is_map { map_for_each } else { set_for_each });

    if is_map {
        interpreter.define_method(prototype, "get", 1, map_get);
        interpreter.define_method(prototype, "set", 2, map_set);
        interpreter.define_method(constructor, "groupBy", 2, crate::iterator::map_group_by);
    } else {
        interpreter.define_method(prototype, "add", 1, set_add);
    }

    install_iterators(interpreter, prototype, is_map);
}


fn map_size(i: &mut Interpreter, t: JsValue, _a: &[JsValue]) -> Completion { size(i, t, CollectionKind::Map) }
fn set_size(i: &mut Interpreter, t: JsValue, _a: &[JsValue]) -> Completion { size(i, t, CollectionKind::Set) }
fn map_has(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion { has(i, t, a, CollectionKind::Map) }
fn set_has(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion { has(i, t, a, CollectionKind::Set) }
fn map_delete(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion { remove(i, t, a, CollectionKind::Map) }
fn set_delete(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion { remove(i, t, a, CollectionKind::Set) }
fn map_clear(i: &mut Interpreter, t: JsValue, _a: &[JsValue]) -> Completion { clear(i, t, CollectionKind::Map) }
fn set_clear(i: &mut Interpreter, t: JsValue, _a: &[JsValue]) -> Completion { clear(i, t, CollectionKind::Set) }
fn map_for_each(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion { for_each(i, t, a, CollectionKind::Map) }
fn set_for_each(i: &mut Interpreter, t: JsValue, a: &[JsValue]) -> Completion { for_each(i, t, a, CollectionKind::Set) }

fn size(interpreter: &mut Interpreter, this: JsValue, kind: CollectionKind) -> Completion {
    match entries_of(interpreter, &this, kind) {
        Ok(id) => {
            let live = interpreter.object(id).collection.as_ref().map_or(0, |c| c.live);
            Completion::Normal(JsValue::Number(live as f64))
        }
        Err(abrupt) => abrupt,
    }
}

fn has(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    kind: CollectionKind,
) -> Completion {
    let id = match entries_of(interpreter, &this, kind) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    Completion::Normal(JsValue::Boolean(find(interpreter, id, &arg(arguments, 0)).is_some()))
}

fn remove(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    kind: CollectionKind,
) -> Completion {
    let id = match entries_of(interpreter, &this, kind) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let Some(at) = find(interpreter, id, &arg(arguments, 0)) else {
        return Completion::Normal(JsValue::Boolean(false));
    };
    if let Some(collection) = &mut interpreter.object_mut(id).collection {
        collection.entries[at] = None;
        collection.live -= 1;
    }
    Completion::Normal(JsValue::Boolean(true))
}

fn clear(interpreter: &mut Interpreter, this: JsValue, kind: CollectionKind) -> Completion {
    let id = match entries_of(interpreter, &this, kind) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    if let Some(collection) = &mut interpreter.object_mut(id).collection {
        for slot in &mut collection.entries {
            *slot = None;
        }
        collection.live = 0;
    }
    Completion::Normal(JsValue::Undefined)
}

fn for_each(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    kind: CollectionKind,
) -> Completion {
    let id = match entries_of(interpreter, &this, kind) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let callback = arg(arguments, 0);
    if !interpreter.is_callable(&callback) {
        return interpreter.type_error("the callback is not a function");
    }
    let this_arg = arg(arguments, 1);
    let mut at = 0;
    loop {
        let slot = match &interpreter.object(id).collection {
            Some(collection) => match collection.entries.get(at) {
                Some(slot) => slot.clone(),
                None => break,
            },
            None => break,
        };
        at += 1;
        let Some((key, value)) = slot else { continue };
        let completion = interpreter.call_value(
            &callback,
            this_arg.clone(),
            crate::vec![value, key, JsValue::Object(id)],
        );
        if completion.is_abrupt() {
            return completion;
        }
    }
    Completion::Normal(JsValue::Undefined)
}

fn map_get(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let id = match entries_of(interpreter, &this, CollectionKind::Map) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let found = find(interpreter, id, &arg(arguments, 0)).and_then(|at| {
        interpreter.object(id).collection.as_ref().and_then(|c| {
            c.entries[at].as_ref().map(|(_, value)| value.clone())
        })
    });
    Completion::Normal(found.unwrap_or(JsValue::Undefined))
}

fn map_set(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let id = match entries_of(interpreter, &this, CollectionKind::Map) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    insert(interpreter, id, arg(arguments, 0), arg(arguments, 1));
    Completion::Normal(this)
}

fn set_add(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let id = match entries_of(interpreter, &this, CollectionKind::Set) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let value = arg(arguments, 0);
    insert(interpreter, id, value.clone(), value);
    Completion::Normal(this)
}

/// `keys`, `values`, `entries`, and the `Symbol.iterator` that must be the SAME function object as
/// one of them.
fn install_iterators(interpreter: &mut Interpreter, prototype: ObjectId, is_map: bool) {
    let (keys, values, entries): (
        crate::interpreter::NativeFn,
        crate::interpreter::NativeFn,
        crate::interpreter::NativeFn,
    ) = if is_map {
        (
            |i, t, _| iterator_for(i, t, IterationKind::Key, CollectionKind::Map),
            |i, t, _| iterator_for(i, t, IterationKind::Value, CollectionKind::Map),
            |i, t, _| iterator_for(i, t, IterationKind::KeyAndValue, CollectionKind::Map),
        )
    } else {
        (
            |i, t, _| iterator_for(i, t, IterationKind::Key, CollectionKind::Set),
            |i, t, _| iterator_for(i, t, IterationKind::Value, CollectionKind::Set),
            |i, t, _| iterator_for(i, t, IterationKind::KeyAndValue, CollectionKind::Set),
        )
    };
    interpreter.define_method(prototype, "keys", 0, keys);
    interpreter.define_method(prototype, "values", 0, values);
    interpreter.define_method(prototype, "entries", 0, entries);

    let alias = if is_map { "entries" } else { "values" };
    let same = interpreter
        .object(prototype)
        .own(&PropertyKey::from_str(alias))
        .and_then(|property| property.data_value().cloned())
        .unwrap_or(JsValue::Undefined);
    let key = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));
    interpreter.object_mut(prototype).set_own(key, Property::builtin(same));

    if !is_map {
        let values = interpreter
            .object(prototype)
            .own(&PropertyKey::from_str("values"))
            .and_then(|property| property.data_value().cloned())
            .unwrap_or(JsValue::Undefined);
        interpreter
            .object_mut(prototype)
            .set_own(PropertyKey::from_str("keys"), Property::builtin(values));
    }
}

fn iterator_for(
    interpreter: &mut Interpreter,
    this: JsValue,
    kind: IterationKind,
    expected: CollectionKind,
) -> Completion {
    let id = match entries_of(interpreter, &this, expected) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let is_map =
        interpreter.object(id).collection.as_ref().map(|c| c.kind) == Some(CollectionKind::Map);
    let prototype = if is_map {
        interpreter.intrinsics.map_iterator_prototype
    } else {
        interpreter.intrinsics.set_iterator_prototype
    };
    create_collection_iterator(interpreter, id, kind, prototype)
}

/// `new Map(iterable)` / `new Set(iterable)`.
fn construct(interpreter: &mut Interpreter, arguments: &[JsValue], is_map: bool) -> Completion {
    let kind = if is_map { CollectionKind::Map } else { CollectionKind::Set };
    construct_of_kind(interpreter, arguments, kind)
}

/// The body all four constructors share.
///
/// THE WEAK PAIR GOES THROUGH THIS RATHER THAN A COPY OF IT. The first version of `WeakMap`
/// duplicated the loop below, and the whole value of this function is in what the loop gets RIGHT
/// -- stepping instead of draining, calling the real adder, closing the iterator on failure. Each
/// of those is a defect this engine has already shipped once, and a second copy is a second place
/// for them to come back. The four constructors differ in a kind and a prototype, so that is all
/// that is parameterized.
fn construct_of_kind(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
    kind: CollectionKind,
) -> Completion {
    let is_map = matches!(kind, CollectionKind::Map | CollectionKind::WeakMap);
    let intrinsic = match kind {
        CollectionKind::Map => interpreter.intrinsics.map_prototype,
        CollectionKind::Set => interpreter.intrinsics.set_prototype,
        CollectionKind::WeakMap => interpreter.intrinsics.weak_map_prototype,
        CollectionKind::WeakSet => interpreter.intrinsics.weak_set_prototype,
    };
    let prototype = interpreter.instance_prototype(intrinsic);
    let mut object = Object::new(Some(prototype));
    object.collection = Some(crate::Box::new(Collection { entries: Vec::new(), live: 0, kind }));
    let id = interpreter.allocate(object);

    let source = arg(arguments, 0);
    if matches!(source, JsValue::Undefined | JsValue::Null) {
        return Completion::Normal(JsValue::Object(id));
    }
    let adder_name = if is_map { "set" } else { "add" };
    let adder = match interpreter.get_member(&JsValue::Object(id), &PropertyKey::from_str(adder_name)) {
        Completion::Normal(adder) => adder,
        abrupt => return abrupt,
    };
    if !interpreter.is_callable(&adder) {
        return interpreter.type_error(&crate::format!("`{adder_name}` is not a function"));
    }
    let mut record = match crate::iterator::get_iterator(interpreter, &source) {
        Ok(record) => record,
        Err(abrupt) => return abrupt,
    };
    loop {
        let step = match crate::iterator::iterator_step(interpreter, &mut record) {
            Ok(Some(result)) => result,
            Ok(None) => break,
            Err(abrupt) => return abrupt,
        };
        let item = match crate::iterator::iterator_value(interpreter, step) {
            Ok(item) => item,
            Err(abrupt) => {
                record.done = true;
                return abrupt;
            }
        };
        let failure = add_entry(interpreter, id, &adder, &item, is_map);
        if let Some(abrupt) = failure {
            return crate::iterator::iterator_close(interpreter, &record, abrupt);
        }
    }
    Completion::Normal(JsValue::Object(id))
}

/// One entry, through the collection's own adder. `Some(..)` is an abrupt completion.
fn add_entry(
    interpreter: &mut Interpreter,
    id: ObjectId,
    adder: &JsValue,
    item: &JsValue,
    is_map: bool,
) -> Option<Completion> {
    let arguments = if is_map {
        if !matches!(item, JsValue::Object(_)) {
            return Some(interpreter.type_error("a Map entry must be an object with 0 and 1"));
        }
        let key = match interpreter.get_member(item, &PropertyKey::from_str("0")) {
            Completion::Normal(value) => value,
            abrupt => return Some(abrupt),
        };
        let value = match interpreter.get_member(item, &PropertyKey::from_str("1")) {
            Completion::Normal(value) => value,
            abrupt => return Some(abrupt),
        };
        crate::vec![key, value]
    } else {
        crate::vec![item.clone()]
    };
    match interpreter.call_value(adder, JsValue::Object(id), arguments) {
        Completion::Normal(_) => None,
        abrupt => Some(abrupt),
    }
}

/// The brand check: this must BE a collection **of the right kind**, not merely look like one.
///
/// **CHECKING ONLY THAT A COLLECTION SLOT EXISTS IS NOT THE BRAND CHECK, AND THE FIRST VERSION
/// OF THIS DID EXACTLY THAT.** `Map.prototype.get.call(new Set())` then returned `undefined`
/// instead of throwing -- a Set has entries, so the weak test passed and the method read a
/// collection it has no business reading. `CollectionKind` exists for this and was being ignored.
fn entries_of(
    interpreter: &mut Interpreter,
    this: &JsValue,
    expected: CollectionKind,
) -> Result<ObjectId, Completion> {
    if let JsValue::Object(id) = this {
        if interpreter.object(*id).collection.as_ref().map(|c| c.kind) == Some(expected) {
            return Ok(*id);
        }
    }
    let name = match expected {
        CollectionKind::Map => "a Map",
        CollectionKind::Set => "a Set",
        CollectionKind::WeakMap => "a WeakMap",
        CollectionKind::WeakSet => "a WeakSet",
    };
    Err(interpreter.type_error(&crate::format!("this method requires {name}")))
}

/// The index of a live entry with this key, by `SameValueZero`.
fn find(interpreter: &Interpreter, id: ObjectId, key: &JsValue) -> Option<usize> {
    let collection = interpreter.object(id).collection.as_ref()?;
    collection.entries.iter().position(|slot| {
        slot.as_ref().is_some_and(|(existing, _)| same_value_zero(existing, key))
    })
}

/// SETTING AN EXISTING KEY REPLACES ITS VALUE AND KEEPS ITS POSITION. Appending instead would
/// reorder the collection on an ordinary overwrite, which `forEach` would then report.
fn insert(interpreter: &mut Interpreter, id: ObjectId, key: JsValue, value: JsValue) {
    let key = match key {
        JsValue::Number(number) if number == 0.0 => JsValue::Number(0.0),
        other => other,
    };
    let is_set =
        interpreter.object(id).collection.as_ref().map(|c| c.kind) == Some(CollectionKind::Set);
    let value = if is_set { key.clone() } else { value };
    if let Some(at) = find(interpreter, id, &key) {
        if let Some(collection) = &mut interpreter.object_mut(id).collection {
            collection.entries[at] = Some((key, value));
        }
        return;
    }
    if let Some(collection) = &mut interpreter.object_mut(id).collection {
        collection.entries.push(Some((key, value)));
        collection.live += 1;
    }
}

fn to_string_tag(interpreter: &mut Interpreter, target: ObjectId, tag: &str) {
    interpreter.define_to_string_tag(target, tag);
}



