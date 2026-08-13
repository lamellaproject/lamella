//! `Proxy`: the thirteen internal methods, implemented by calling back into the program.

use crate::builtins::{
    arg, array_like_to_vec, describe_raw_descriptor, is_compatible_property_descriptor,
    to_property_descriptor, RawDescriptor,
};
use crate::interpreter::{Completion, Interpreter};
use crate::iterator::get_method;
use crate::object::{Callable, Object, Property, PropertyKey, PropertyKind, ProxyData};
use crate::value::{JsValue, ObjectId};
use crate::{vec, String, Vec};

/// `ProxyCreate` (10.5.15).
///
/// CALLABILITY IS DECIDED HERE AND NEVER RECONSIDERED. The standard installs `[[Call]]` only when
/// the target is callable at creation time, so `typeof` is a property of the proxy rather than a
/// question forwarded to a target that may since have been revoked away.
pub(crate) fn create(
    interpreter: &mut Interpreter,
    target: &JsValue,
    handler: &JsValue,
) -> Result<ObjectId, Completion> {
    let JsValue::Object(target) = target else {
        return Err(interpreter.type_error("a proxy's target must be an object"));
    };
    let JsValue::Object(handler) = handler else {
        return Err(interpreter.type_error("a proxy's handler must be an object"));
    };
    let mut object = Object::new(None);
    object.proxy = Some(crate::Box::new(ProxyData {
        target: Some(*target),
        handler: Some(*handler),
    }));
    if interpreter.is_callable(&JsValue::Object(*target)) {
        object.callable = Some(Callable::Proxy);
    }
    Ok(interpreter.allocate(object))
}

/// `ValidateNonRevokedProxy` (10.5.14), returning the pair every trap needs.
fn parts(
    interpreter: &mut Interpreter,
    id: ObjectId,
) -> Result<(ObjectId, ObjectId), Completion> {
    let data = interpreter
        .object(id)
        .proxy
        .as_ref()
        .map(|data| (data.target, data.handler));
    match data {
        Some((Some(target), Some(handler))) => Ok((target, handler)),
        Some(_) => Err(interpreter
            .type_error("this proxy has been revoked and can no longer be used")),
        None => Err(interpreter.type_error("not a proxy")),
    }
}

/// `GetMethod(handler, name)` -- absent means "forward to the target".
fn trap(
    interpreter: &mut Interpreter,
    handler: ObjectId,
    name: &str,
) -> Result<Option<JsValue>, Completion> {
    get_method(interpreter, &JsValue::Object(handler), &PropertyKey::from_str(name))
}

/// Calls a trap with the handler as its `this`.
fn call_trap(
    interpreter: &mut Interpreter,
    trap: &JsValue,
    handler: ObjectId,
    arguments: Vec<JsValue>,
) -> Result<JsValue, Completion> {
    match interpreter.call_value(trap, JsValue::Object(handler), arguments) {
        Completion::Normal(value) => Ok(value),
        abrupt => Err(abrupt),
    }
}

/// A property key as the value a trap is handed: a String stays a String, a Symbol stays a Symbol.
fn key_value(key: &PropertyKey) -> JsValue {
    match key {
        PropertyKey::String(name) => JsValue::String(name.clone()),
        PropertyKey::Symbol(symbol) => JsValue::Symbol(*symbol),
    }
}

fn type_error<T>(interpreter: &mut Interpreter, message: &str) -> Result<T, Completion> {
    Err(interpreter.type_error(message))
}


/// `[[GetPrototypeOf]]` (10.5.1).
pub(crate) fn get_prototype_of(
    interpreter: &mut Interpreter,
    id: ObjectId,
) -> Result<Option<ObjectId>, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "getPrototypeOf")? else {
        return interpreter.get_prototype_of(target);
    };
    let reported = call_trap(interpreter, &trap, handler, vec![JsValue::Object(target)])?;
    let reported = match reported {
        JsValue::Object(prototype) => Some(prototype),
        JsValue::Null => None,
        _ => {
            return type_error(
                interpreter,
                "a `getPrototypeOf` trap must answer an object or null",
            )
        }
    };
    if interpreter.is_extensible(target)? {
        return Ok(reported);
    }
    if reported != interpreter.get_prototype_of(target)? {
        return type_error(
            interpreter,
            "a `getPrototypeOf` trap reported a prototype the non-extensible target does not have",
        );
    }
    Ok(reported)
}

/// `[[SetPrototypeOf]]` (10.5.2).
pub(crate) fn set_prototype_of(
    interpreter: &mut Interpreter,
    id: ObjectId,
    prototype: Option<ObjectId>,
) -> Result<bool, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "setPrototypeOf")? else {
        return interpreter.set_prototype_of(target, prototype);
    };
    let requested = prototype.map_or(JsValue::Null, JsValue::Object);
    let answer =
        call_trap(interpreter, &trap, handler, vec![JsValue::Object(target), requested])?;
    if !crate::abstract_ops::to_boolean(&answer) {
        return Ok(false);
    }
    if interpreter.is_extensible(target)? {
        return Ok(true);
    }
    if prototype != interpreter.get_prototype_of(target)? {
        return type_error(
            interpreter,
            "a `setPrototypeOf` trap reported success for a prototype the non-extensible target does not have",
        );
    }
    Ok(true)
}


/// `[[IsExtensible]]` (10.5.3).
pub(crate) fn is_extensible(
    interpreter: &mut Interpreter,
    id: ObjectId,
) -> Result<bool, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "isExtensible")? else {
        return interpreter.is_extensible(target);
    };
    let answer = call_trap(interpreter, &trap, handler, vec![JsValue::Object(target)])?;
    let reported = crate::abstract_ops::to_boolean(&answer);
    if reported != interpreter.is_extensible(target)? {
        return type_error(
            interpreter,
            "an `isExtensible` trap must agree with its target",
        );
    }
    Ok(reported)
}

/// `[[PreventExtensions]]` (10.5.4).
pub(crate) fn prevent_extensions(
    interpreter: &mut Interpreter,
    id: ObjectId,
) -> Result<bool, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "preventExtensions")? else {
        return interpreter.prevent_extensions(target);
    };
    let answer = call_trap(interpreter, &trap, handler, vec![JsValue::Object(target)])?;
    let reported = crate::abstract_ops::to_boolean(&answer);
    if reported && interpreter.is_extensible(target)? {
        return type_error(
            interpreter,
            "a `preventExtensions` trap reported success while its target is still extensible",
        );
    }
    Ok(reported)
}


/// `[[GetOwnProperty]]` (10.5.5).
pub(crate) fn own_property(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: &PropertyKey,
) -> Result<Option<Property>, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "getOwnPropertyDescriptor")? else {
        return interpreter.own_property(target, key);
    };
    let reported =
        call_trap(interpreter, &trap, handler, vec![JsValue::Object(target), key_value(key)])?;
    if !reported.is_object() && !matches!(reported, JsValue::Undefined) {
        return type_error(
            interpreter,
            "a `getOwnPropertyDescriptor` trap must answer an object or undefined",
        );
    }
    let existing = interpreter.own_property(target, key)?;
    if matches!(reported, JsValue::Undefined) {
        let Some(existing) = existing else { return Ok(None) };
        if !existing.configurable {
            return type_error(
                interpreter,
                "a `getOwnPropertyDescriptor` trap reported a non-configurable property as absent",
            );
        }
        if !interpreter.is_extensible(target)? {
            return type_error(
                interpreter,
                "a `getOwnPropertyDescriptor` trap reported an existing property of a non-extensible target as absent",
            );
        }
        return Ok(None);
    }
    let extensible = interpreter.is_extensible(target)?;
    let raw = to_property_descriptor(interpreter, &reported)?;
    let completed = complete(&raw);
    if !is_compatible_property_descriptor(extensible, &raw, existing.as_ref()) {
        return type_error(
            interpreter,
            "a `getOwnPropertyDescriptor` trap reported a descriptor incompatible with its target",
        );
    }
    if !completed.configurable {
        match &existing {
            None => {
                return type_error(
                    interpreter,
                    "a `getOwnPropertyDescriptor` trap reported a non-configurable property the target does not have",
                )
            }
            Some(existing) if existing.configurable => {
                return type_error(
                    interpreter,
                    "a `getOwnPropertyDescriptor` trap reported a configurable property as non-configurable",
                )
            }
            Some(existing) => {
                if let PropertyKind::Data { writable: false, .. } = completed.kind {
                    if existing.is_writable() {
                        return type_error(
                            interpreter,
                            "a `getOwnPropertyDescriptor` trap reported a writable property as non-writable",
                        );
                    }
                }
            }
        }
    }
    Ok(Some(completed))
}

/// `CompletePropertyDescriptor` (6.2.6.6): a partial descriptor with every absent field defaulted.
///
/// A descriptor that names NEITHER shape is a DATA one -- value `undefined`, not writable. The
/// standard's `IsGenericDescriptor` case falls into the data branch, and treating it as an accessor
/// with two absent halves would produce a property that reads as `undefined` and cannot be written,
/// which is the same answer for the wrong reason and a different `getOwnPropertyDescriptor`.
fn complete(raw: &RawDescriptor) -> Property {
    let kind = if raw.has_accessor_field() {
        PropertyKind::Accessor { get: raw.get, set: raw.set }
    } else {
        PropertyKind::Data { value: raw.value.clone(), writable: raw.writable }
    };
    Property { kind, enumerable: raw.enumerable, configurable: raw.configurable }
}

/// `[[DefineOwnProperty]]` (10.5.6). `Ok(None)` defined it; `Ok(Some(reason))` is the standard's
/// `false`.
pub(crate) fn define_own_property(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: PropertyKey,
    descriptor: &RawDescriptor,
) -> Result<Option<String>, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "defineProperty")? else {
        return crate::builtins::define_own_property(interpreter, target, key, descriptor);
    };
    let described = describe_raw_descriptor(interpreter, descriptor);
    let answer = call_trap(
        interpreter,
        &trap,
        handler,
        vec![JsValue::Object(target), key_value(&key), JsValue::Object(described)],
    )?;
    if !crate::abstract_ops::to_boolean(&answer) {
        return Ok(Some("the proxy's `defineProperty` trap refused".into()));
    }
    let existing = interpreter.own_property(target, &key)?;
    let extensible = interpreter.is_extensible(target)?;
    let setting_non_configurable = descriptor.declares_non_configurable();
    match &existing {
        None => {
            if !extensible {
                return type_error(
                    interpreter,
                    "a `defineProperty` trap added a property to a non-extensible target",
                );
            }
            if setting_non_configurable {
                return type_error(
                    interpreter,
                    "a `defineProperty` trap reported a non-configurable property the target does not have",
                );
            }
        }
        Some(existing) => {
            if !is_compatible_property_descriptor(extensible, descriptor, Some(existing)) {
                return type_error(
                    interpreter,
                    "a `defineProperty` trap reported a definition its target would refuse",
                );
            }
            if setting_non_configurable && existing.configurable {
                return type_error(
                    interpreter,
                    "a `defineProperty` trap made a configurable property non-configurable without its target",
                );
            }
            if !existing.configurable
                && !existing.is_accessor()
                && existing.is_writable()
                && descriptor.declares_non_writable()
            {
                return type_error(
                    interpreter,
                    "a `defineProperty` trap made a non-configurable writable property non-writable without its target",
                );
            }
        }
    }
    Ok(None)
}


/// `[[HasProperty]]` (10.5.7).
pub(crate) fn has_property(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: &PropertyKey,
) -> Result<bool, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "has")? else {
        return interpreter.has_property(target, key);
    };
    let answer =
        call_trap(interpreter, &trap, handler, vec![JsValue::Object(target), key_value(key)])?;
    let reported = crate::abstract_ops::to_boolean(&answer);
    if !reported {
        if let Some(existing) = interpreter.own_property(target, key)? {
            if !existing.configurable {
                return type_error(
                    interpreter,
                    "a `has` trap denied a non-configurable property of its target",
                );
            }
            if !interpreter.is_extensible(target)? {
                return type_error(
                    interpreter,
                    "a `has` trap denied an existing property of a non-extensible target",
                );
            }
        }
    }
    Ok(reported)
}

/// `[[Get]]` (10.5.8).
pub(crate) fn get(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: &PropertyKey,
    receiver: JsValue,
) -> Completion {
    let (target, handler) = match parts(interpreter, id) {
        Ok(pair) => pair,
        Err(abrupt) => return abrupt,
    };
    let trap = match trap(interpreter, handler, "get") {
        Ok(trap) => trap,
        Err(abrupt) => return abrupt,
    };
    let Some(trap) = trap else {
        return interpreter.get_property_with_receiver(target, key, receiver);
    };
    let answer = match call_trap(
        interpreter,
        &trap,
        handler,
        vec![JsValue::Object(target), key_value(key), receiver],
    ) {
        Ok(answer) => answer,
        Err(abrupt) => return abrupt,
    };
    match check_frozen_read(interpreter, target, key, &answer) {
        Ok(()) => Completion::Normal(answer),
        Err(abrupt) => abrupt,
    }
}

/// The half of 10.5.8 that keeps `Object.freeze`'s promise: a non-configurable, non-writable
/// property reads as ITS VALUE and nothing else.
fn check_frozen_read(
    interpreter: &mut Interpreter,
    target: ObjectId,
    key: &PropertyKey,
    answer: &JsValue,
) -> Result<(), Completion> {
    let Some(existing) = interpreter.own_property(target, key)? else { return Ok(()) };
    if existing.configurable {
        return Ok(());
    }
    match &existing.kind {
        PropertyKind::Data { value, writable: false } => {
            if !crate::abstract_ops::same_value(answer, value) {
                return type_error(
                    interpreter,
                    "a `get` trap answered a value its target's frozen property does not hold",
                );
            }
        }
        PropertyKind::Accessor { get: None, .. } => {
            if !matches!(answer, JsValue::Undefined) {
                return type_error(
                    interpreter,
                    "a `get` trap answered a value for a non-configurable accessor with no getter",
                );
            }
        }
        _ => {}
    }
    Ok(())
}

/// `[[Set]]` (10.5.9). `Ok(None)` wrote; `Ok(Some(reason))` is the standard's `false`.
pub(crate) fn set(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: PropertyKey,
    value: JsValue,
    receiver: JsValue,
) -> Result<Option<&'static str>, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "set")? else {
        return interpreter.ordinary_set(target, key, value, receiver);
    };
    let answer = call_trap(
        interpreter,
        &trap,
        handler,
        vec![JsValue::Object(target), key_value(&key), value.clone(), receiver],
    )?;
    if !crate::abstract_ops::to_boolean(&answer) {
        return Ok(Some("the proxy's `set` trap refused"));
    }
    let Some(existing) = interpreter.own_property(target, &key)? else { return Ok(None) };
    if existing.configurable {
        return Ok(None);
    }
    match &existing.kind {
        PropertyKind::Data { value: held, writable: false } => {
            if !crate::abstract_ops::same_value(&value, held) {
                return type_error(
                    interpreter,
                    "a `set` trap reported success for a value its target's frozen property does not hold",
                );
            }
        }
        PropertyKind::Accessor { set: None, .. } => {
            return type_error(
                interpreter,
                "a `set` trap reported success for a non-configurable accessor with no setter",
            )
        }
        _ => {}
    }
    Ok(None)
}

/// `[[Delete]]` (10.5.10).
pub(crate) fn delete(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: &PropertyKey,
) -> Result<bool, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "deleteProperty")? else {
        return interpreter.delete_own_property(target, key);
    };
    let answer =
        call_trap(interpreter, &trap, handler, vec![JsValue::Object(target), key_value(key)])?;
    if !crate::abstract_ops::to_boolean(&answer) {
        return Ok(false);
    }
    let Some(existing) = interpreter.own_property(target, key)? else { return Ok(true) };
    if !existing.configurable {
        return type_error(
            interpreter,
            "a `deleteProperty` trap reported the removal of a non-configurable property",
        );
    }
    if !interpreter.is_extensible(target)? {
        return type_error(
            interpreter,
            "a `deleteProperty` trap reported the removal of a property of a non-extensible target",
        );
    }
    Ok(true)
}

/// `[[OwnPropertyKeys]]` (10.5.11).
pub(crate) fn own_keys(
    interpreter: &mut Interpreter,
    id: ObjectId,
) -> Result<Vec<PropertyKey>, Completion> {
    let (target, handler) = parts(interpreter, id)?;
    let Some(trap) = trap(interpreter, handler, "ownKeys")? else {
        return interpreter.own_keys_of(target);
    };
    let answer = call_trap(interpreter, &trap, handler, vec![JsValue::Object(target)])?;
    let reported = key_list(interpreter, &answer)?;
    for (index, key) in reported.iter().enumerate() {
        if reported[..index].contains(key) {
            return type_error(interpreter, "an `ownKeys` trap listed a key twice");
        }
    }
    let extensible = interpreter.is_extensible(target)?;
    let target_keys = interpreter.own_keys_of(target)?;
    let mut non_configurable = Vec::new();
    let mut configurable = Vec::new();
    for key in target_keys {
        match interpreter.own_property(target, &key)? {
            Some(property) if !property.configurable => non_configurable.push(key),
            _ => configurable.push(key),
        }
    }
    if extensible && non_configurable.is_empty() {
        return Ok(reported);
    }
    let mut unchecked = reported.clone();
    for key in &non_configurable {
        match unchecked.iter().position(|candidate| candidate == key) {
            Some(at) => {
                unchecked.remove(at);
            }
            None => {
                return type_error(
                    interpreter,
                    "an `ownKeys` trap omitted a non-configurable key of its target",
                )
            }
        }
    }
    if extensible {
        return Ok(reported);
    }
    for key in &configurable {
        match unchecked.iter().position(|candidate| candidate == key) {
            Some(at) => {
                unchecked.remove(at);
            }
            None => {
                return type_error(
                    interpreter,
                    "an `ownKeys` trap omitted a key of its non-extensible target",
                )
            }
        }
    }
    if !unchecked.is_empty() {
        return type_error(
            interpreter,
            "an `ownKeys` trap listed a key its non-extensible target does not have",
        );
    }
    Ok(reported)
}

/// `CreateListFromArrayLike(value, property-key)`: the one call whose element type is restricted.
///
/// A NUMBER IN THE LIST IS A TypeError, NOT A COERCION. `ownKeys` returning `[0]` is refused --
/// the standard requires the elements to ALREADY be property keys, because a coercion here would
/// invent a key the handler never named.
fn key_list(
    interpreter: &mut Interpreter,
    value: &JsValue,
) -> Result<Vec<PropertyKey>, Completion> {
    if !value.is_object() {
        return type_error(interpreter, "an `ownKeys` trap must answer an array-like object");
    }
    let values = array_like_to_vec(interpreter, value)?;
    let mut keys = Vec::with_capacity(values.len());
    for value in values {
        match value {
            JsValue::String(name) => keys.push(PropertyKey::String(name)),
            JsValue::Symbol(symbol) => keys.push(PropertyKey::Symbol(symbol)),
            _ => {
                return type_error(
                    interpreter,
                    "an `ownKeys` trap listed something that is not a property key",
                )
            }
        }
    }
    Ok(keys)
}


/// `[[Call]]` (10.5.12).
pub(crate) fn call(
    interpreter: &mut Interpreter,
    id: ObjectId,
    this: JsValue,
    arguments: Vec<JsValue>,
) -> Completion {
    let (target, handler) = match parts(interpreter, id) {
        Ok(pair) => pair,
        Err(abrupt) => return abrupt,
    };
    let trap = match trap(interpreter, handler, "apply") {
        Ok(trap) => trap,
        Err(abrupt) => return abrupt,
    };
    let Some(trap) = trap else {
        return interpreter.call_value(&JsValue::Object(target), this, arguments);
    };
    let list = interpreter.new_array(arguments);
    match call_trap(
        interpreter,
        &trap,
        handler,
        vec![JsValue::Object(target), this, JsValue::Object(list)],
    ) {
        Ok(value) => Completion::Normal(value),
        Err(abrupt) => abrupt,
    }
}

/// `[[Construct]]` (10.5.13).
pub(crate) fn construct(
    interpreter: &mut Interpreter,
    id: ObjectId,
    arguments: Vec<JsValue>,
    new_target: ObjectId,
) -> Completion {
    let (target, handler) = match parts(interpreter, id) {
        Ok(pair) => pair,
        Err(abrupt) => return abrupt,
    };
    let trap = match trap(interpreter, handler, "construct") {
        Ok(trap) => trap,
        Err(abrupt) => return abrupt,
    };
    let Some(trap) = trap else {
        return interpreter.construct_with_new_target(target, arguments, new_target);
    };
    let list = interpreter.new_array(arguments);
    let produced = match call_trap(
        interpreter,
        &trap,
        handler,
        vec![JsValue::Object(target), JsValue::Object(list), JsValue::Object(new_target)],
    ) {
        Ok(value) => value,
        Err(abrupt) => return abrupt,
    };
    if !produced.is_object() {
        return interpreter.type_error("a `construct` trap must answer an object");
    }
    Completion::Normal(produced)
}


pub(crate) fn install(interpreter: &mut Interpreter) {
    let constructor = interpreter.native_constructor(
        "Proxy",
        2,
        |interpreter, _this, _arguments| {
            interpreter.type_error("Proxy must be called with `new`")
        },
        |interpreter, _this, arguments| {
            match create(interpreter, &arg(arguments, 0), &arg(arguments, 1)) {
                Ok(id) => Completion::Normal(JsValue::Object(id)),
                Err(abrupt) => abrupt,
            }
        },
    );
    interpreter.define_global("Proxy", JsValue::Object(constructor));

    interpreter.define_method(constructor, "revocable", 2, |interpreter, _this, arguments| {
        let proxy = match create(interpreter, &arg(arguments, 0), &arg(arguments, 1)) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let object_prototype = interpreter.intrinsics.object_prototype;
        let result = interpreter.allocate(Object::new(Some(object_prototype)));
        let revoke = interpreter.new_proxy_revoker(proxy);
        let _ = interpreter.create_data_property(
            result,
            PropertyKey::from_str("proxy"),
            JsValue::Object(proxy),
        );
        let _ = interpreter.create_data_property(
            result,
            PropertyKey::from_str("revoke"),
            JsValue::Object(revoke),
        );
        Completion::Normal(JsValue::Object(result))
    });
}

/// What the function `Proxy.revocable` hands back does: 10.5.14's two nulls.
///
/// IT IS IDEMPOTENT AND ANSWERS `undefined` EITHER WAY. Revoking twice is not an error; the
/// standard's revocation function returns `undefined` whether it found a live proxy or not.
pub(crate) fn revoke(interpreter: &mut Interpreter, proxy: ObjectId) {
    if let Some(data) = interpreter.object_mut(proxy).proxy.as_mut() {
        data.target = None;
        data.handler = None;
    }
}

/// `Object.prototype.toString` and `Array.isArray` both follow a proxy to its target.
///
/// `Array.isArray(new Proxy([], {}))` IS **TRUE**, and it is the one question that pierces a
/// proxy rather than being trapped: `IsArray` recurses into `[[ProxyTarget]]`, so a proxy for an
/// array IS an array everywhere the language asks. A revoked one is a TypeError instead, which is
/// why this answers with a `Result` rather than a `bool`.
pub(crate) fn target_of(
    interpreter: &mut Interpreter,
    id: ObjectId,
) -> Result<ObjectId, Completion> {
    let (target, _) = parts(interpreter, id)?;
    Ok(target)
}

/// Whether a proxy is a constructor: whether its TARGET is, decided when the proxy was made.
pub(crate) fn is_constructor(interpreter: &Interpreter, id: ObjectId) -> bool {
    let Some(data) = interpreter.object(id).proxy.as_ref() else { return false };
    let Some(target) = data.target else { return false };
    interpreter.is_constructor(target)
}

