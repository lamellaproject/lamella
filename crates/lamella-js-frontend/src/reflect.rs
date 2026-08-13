//! `Reflect`: the object protocol with the answers left as answers.

use crate::builtins::{
    arg, array_like_to_vec, define_own_property, describe_property, to_property_descriptor,
};
use crate::interpreter::{Completion, Interpreter};
use crate::object::{Object, PropertyKey};
use crate::value::{JsValue, ObjectId};
use crate::{format, Vec};

pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let reflect = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.define_global("Reflect", JsValue::Object(reflect));
    interpreter.define_to_string_tag(reflect, "Reflect");

    interpreter.define_method(reflect, "apply", 3, |interpreter, _this, arguments| {
        let target = arg(arguments, 0);
        if !interpreter.is_callable(&target) {
            return interpreter.type_error("Reflect.apply needs a callable target");
        }
        let list = match argument_list(interpreter, &arg(arguments, 2)) {
            Ok(list) => list,
            Err(abrupt) => return abrupt,
        };
        interpreter.call_value(&target, arg(arguments, 1), list)
    });

    interpreter.define_method(reflect, "construct", 2, |interpreter, _this, arguments| {
        let JsValue::Object(target) = arg(arguments, 0) else {
            return interpreter.type_error("Reflect.construct needs a constructor");
        };
        if !interpreter.is_constructor(target) {
            return interpreter.type_error("Reflect.construct needs a constructor");
        }
        let new_target = if arguments.len() > 2 {
            match arg(arguments, 2) {
                JsValue::Object(id) if interpreter.is_constructor(id) => id,
                _ => return interpreter.type_error("Reflect.construct needs a constructor as its `newTarget`"),
            }
        } else {
            target
        };
        let list = match argument_list(interpreter, &arg(arguments, 1)) {
            Ok(list) => list,
            Err(abrupt) => return abrupt,
        };
        interpreter.construct_with_new_target(target, list, new_target)
    });

    interpreter.define_method(reflect, "defineProperty", 3, |interpreter, _this, arguments| {
        let (id, key) = match target_and_key(interpreter, arguments, "defineProperty") {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let descriptor = match to_property_descriptor(interpreter, &arg(arguments, 2)) {
            Ok(descriptor) => descriptor,
            Err(abrupt) => return abrupt,
        };
        match define_own_property(interpreter, id, key, &descriptor) {
            Ok(refusal) => Completion::Normal(JsValue::Boolean(refusal.is_none())),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "deleteProperty", 2, |interpreter, _this, arguments| {
        let (id, key) = match target_and_key(interpreter, arguments, "deleteProperty") {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        match interpreter.delete_own_property(id, &key) {
            Ok(removed) => Completion::Normal(JsValue::Boolean(removed)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "get", 2, |interpreter, _this, arguments| {
        let (id, key) = match target_and_key(interpreter, arguments, "get") {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let receiver =
            if arguments.len() > 2 { arg(arguments, 2) } else { JsValue::Object(id) };
        interpreter.get_property_with_receiver(id, &key, receiver)
    });

    interpreter.define_method(
        reflect,
        "getOwnPropertyDescriptor",
        2,
        |interpreter, _this, arguments| {
            let (id, key) =
                match target_and_key(interpreter, arguments, "getOwnPropertyDescriptor") {
                    Ok(pair) => pair,
                    Err(abrupt) => return abrupt,
                };
            let property = match interpreter.own_property(id, &key) {
                Ok(Some(property)) => property,
                Ok(None) => return Completion::Normal(JsValue::Undefined),
                Err(abrupt) => return abrupt,
            };
            Completion::Normal(JsValue::Object(describe_property(interpreter, &property)))
        },
    );

    interpreter.define_method(reflect, "getPrototypeOf", 1, |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return interpreter.type_error("Reflect.getPrototypeOf needs an object");
        };
        match interpreter.get_prototype_of(id) {
            Ok(Some(prototype)) => Completion::Normal(JsValue::Object(prototype)),
            Ok(None) => Completion::Normal(JsValue::Null),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "has", 2, |interpreter, _this, arguments| {
        let (id, key) = match target_and_key(interpreter, arguments, "has") {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        match interpreter.has_property(id, &key) {
            Ok(has) => Completion::Normal(JsValue::Boolean(has)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "isExtensible", 1, |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return interpreter.type_error("Reflect.isExtensible needs an object");
        };
        match interpreter.is_extensible(id) {
            Ok(extensible) => Completion::Normal(JsValue::Boolean(extensible)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "ownKeys", 1, |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return interpreter.type_error("Reflect.ownKeys needs an object");
        };
        let keys = match interpreter.own_keys_of(id) {
            Ok(keys) => keys,
            Err(abrupt) => return abrupt,
        };
        let keys: Vec<JsValue> = keys
            .into_iter()
            .map(|key| match key {
                PropertyKey::Symbol(symbol) => JsValue::Symbol(symbol),
                PropertyKey::String(name) => JsValue::String(name),
            })
            .collect();
        let array = interpreter.new_array(keys);
        Completion::Normal(JsValue::Object(array))
    });

    interpreter.define_method(reflect, "preventExtensions", 1, |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return interpreter.type_error("Reflect.preventExtensions needs an object");
        };
        match interpreter.prevent_extensions(id) {
            Ok(prevented) => Completion::Normal(JsValue::Boolean(prevented)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "set", 3, |interpreter, _this, arguments| {
        let (id, key) = match target_and_key(interpreter, arguments, "set") {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let receiver =
            if arguments.len() > 3 { arg(arguments, 3) } else { JsValue::Object(id) };
        match interpreter.ordinary_set(id, key, arg(arguments, 2), receiver) {
            Ok(refusal) => Completion::Normal(JsValue::Boolean(refusal.is_none())),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(reflect, "setPrototypeOf", 2, |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return interpreter.type_error("Reflect.setPrototypeOf needs an object");
        };
        let prototype = match arg(arguments, 1) {
            JsValue::Object(prototype) => Some(prototype),
            JsValue::Null => None,
            _ => {
                return interpreter
                    .type_error("Reflect.setPrototypeOf needs an object or null as the prototype")
            }
        };
        match interpreter.set_prototype_of(id, prototype) {
            Ok(changed) => Completion::Normal(JsValue::Boolean(changed)),
            Err(abrupt) => abrupt,
        }
    });
}

/// `CreateListFromArrayLike`, which is where `Reflect`'s two calling functions get strict.
///
/// A non-object is a TypeError HERE rather than an empty list. `Reflect.apply(f, t)` and
/// `Reflect.construct(f)` both reach this with `undefined` and both must refuse.
fn argument_list(
    interpreter: &mut Interpreter,
    list: &JsValue,
) -> Result<Vec<JsValue>, Completion> {
    if !list.is_object() {
        return Err(interpreter.type_error("an argument list must be an object"));
    }
    array_like_to_vec(interpreter, list)
}

/// The first two arguments every keyed `Reflect` function takes, in the order the standard coerces
/// them.
///
/// THE TARGET IS CHECKED BEFORE THE KEY IS COERCED, and the key's coercion can run user code.
/// `Reflect.get(1, {toString() { sideEffect(); }})` must throw without ever calling `toString` --
/// coercing first and checking after runs a program's code on the way to telling it the call was
/// invalid.
fn target_and_key(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
    name: &str,
) -> Result<(ObjectId, PropertyKey), Completion> {
    let JsValue::Object(id) = arg(arguments, 0) else {
        let message = format!("Reflect.{name} needs an object");
        return Err(interpreter.type_error(&message));
    };
    match interpreter.to_property_key_value(&arg(arguments, 1)) {
        Ok(key) => Ok((id, key)),
        Err(abrupt) => Err(abrupt),
    }
}
