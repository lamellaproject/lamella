//! The realm: the global object graph a program starts with.

use crate::{format, vec, String, ToString, Vec};
use crate::abstract_ops::{self as ops};
use crate::absence::Absence;
use crate::interpreter::{
    ok_or_abrupt, Completion, Interpreter, ERROR_KINDS, STANDARD_ERROR_KINDS, WELL_KNOWN_SYMBOLS,
};
use crate::object::{Object, Property, PropertyKey, PropertyKind};
use crate::string_value::JsString;
use crate::value::{JsValue, ObjectId};

/// The argument at a position, which is `undefined` when it was not passed.
///
/// A MISSING ARGUMENT AND AN EXPLICIT `undefined` ARE THE SAME VALUE to a built-in; the difference
/// is only visible through `arguments.length`. Reading past the end must not panic, which it would
/// if the natural indexing were used.
pub(crate) fn arg(arguments: &[JsValue], index: usize) -> JsValue {
    arguments.get(index).cloned().unwrap_or(JsValue::Undefined)
}

/// `ToIntegerOrInfinity`: truncation toward zero, with NaN becoming 0.
///
/// **IT ANSWERS A MATHEMATICAL VALUE, SO THERE IS NO NEGATIVE ZERO IN IT.** `trunc(-0)` and
/// `trunc(-0.5)` are both `-0` in IEEE, and that sign reached the caller: `[true].indexOf(true, -0)`
/// searched from `-0`, found its match there, and RETURNED `-0` -- an index no array has, which
/// compares equal to `0` under `===` and separates only under `Object.is` or a division. Every
/// `fromIndex`, `start` and `end` in the library flows through here, so the normalisation belongs
/// here and not at any one of them.
pub(crate) fn to_integer(number: f64) -> f64 {
    if number.is_nan() {
        return 0.0;
    }
    let truncated = crate::math::trunc(number);
    if truncated == 0.0 {
        0.0
    } else {
        truncated
    }
}

/// `ToLength`: an index count, clamped to the range an array length may take.
fn to_length(number: f64) -> f64 {
    let number = to_integer(number);
    number.clamp(0.0, 9_007_199_254_740_991.0)
}

/// Resolves a possibly-negative offset against a length, the way every slice-like method does.
fn relative_index(relative: f64, length: f64) -> f64 {
    if relative < 0.0 {
        (length + relative).max(0.0)
    } else {
        relative.min(length)
    }
}

/// The digit a character denotes in a given radix, or `None` if it is not one.
fn radix_digit(unit: u16, radix: u32) -> Option<u32> {
    let value = match u8::try_from(unit).ok()? {
        b'0'..=b'9' => u32::from(unit) - u32::from(b'0'),
        b'a'..=b'z' => u32::from(unit) - u32::from(b'a') + 10,
        b'A'..=b'Z' => u32::from(unit) - u32::from(b'A') + 10,
        _ => return None,
    };
    (value < radix).then_some(value)
}

/// The value of a run of digits, rounded to `f64` ONCE.
///
/// # WHY THIS IS NOT A LOOP OVER `f64`
///
/// `parseInt` is specified in two steps: read the digits as a MATHEMATICAL INTEGER, then convert
/// that integer to a Number. Accumulating `value * radix + digit` in `f64` instead rounds at EVERY
/// digit, and the error compounds into the answer rather than staying in the last place.
/// `parseInt("12345678901234567890", 10)` answered `12345678901234569216` where the one correct
/// rounding of that integer is `12345678901234567168` -- exactly one ulp, and invisible to any test
/// whose numbers fit in 53 bits.
///
/// A `u128` holds every integer up to 38 decimal digits, so the exact path covers what programs
/// contain and the cast at the end is the single rounding the standard asks for.
///
/// Past that width the answer is approximated, and this names the bound rather than claiming
/// exactness. Radix 10 hands the text to the same correctly-rounded decimal parser `Number()`
/// reaches, so it stays exact at any length; another radix resumes the accumulating loop. The
/// standard permits an implementation-approximated integer for a radix outside 2, 4, 8, 10, 16 and
/// 32, and for one of those an input needing more than 128 bits is this function's stated limit.
fn digits_to_number(digits: &[u16], radix: u32) -> f64 {
    let mut exact: u128 = 0;
    for (index, unit) in digits.iter().enumerate() {
        let digit = scanned_digit(*unit, radix);
        match exact
            .checked_mul(u128::from(radix))
            .and_then(|shifted| shifted.checked_add(u128::from(digit)))
        {
            Some(next) => exact = next,
            None => return wider_than_a_u128(digits, index, exact, radix),
        }
    }
    exact as f64
}

/// The digits outran a `u128`. See [`digits_to_number`] for why this is a stated bound.
fn wider_than_a_u128(digits: &[u16], resume_at: usize, so_far: u128, radix: u32) -> f64 {
    if radix == 10 {
        let mut text = String::with_capacity(digits.len());
        for unit in digits {
            let digit = scanned_digit(*unit, 10);
            text.push(char::from_digit(digit, 10).expect("a decimal digit"));
        }
        if let Ok(value) = text.parse::<f64>() {
            return value;
        }
    }
    let mut value = so_far as f64;
    for unit in &digits[resume_at..] {
        value = value * f64::from(radix) + f64::from(scanned_digit(*unit, radix));
    }
    value
}

/// A digit the caller has already scanned in this radix. A miss here would mean the scan and this
/// function disagree about what a digit is, which is an engine defect rather than bad input.
fn scanned_digit(unit: u16, radix: u32) -> u32 {
    radix_digit(unit, radix).expect("the caller scanned this run as digits in this radix")
}

/// `parseInt(string, radix)`.
///
/// IT IS A PREFIX PARSER AND NEVER THROWS. `parseInt("12abc")` is 12, `parseInt("abc")` is `NaN`,
/// and an out-of-range radix is `NaN` rather than a RangeError -- `parseInt` reports every failure
/// by VALUE. That is the whole reason it exists beside `Number(...)`, which refuses the same input.
///
/// RADIX 0 MEANS "DECIDE FROM THE TEXT", which is not the same as radix 10: a `0x` prefix selects
/// 16. And radix 16 accepts the prefix too, so `parseInt("0x10", 16)` is 16 rather than 0.
fn parse_int_radix(text: &JsString, radix: i32) -> f64 {
    let units = text.units();
    let mut at = 0usize;
    while at < units.len() && is_trimmable(units[at]) {
        at += 1;
    }
    let negative = match units.get(at).copied() {
        Some(u) if u == u16::from(b'-') => {
            at += 1;
            true
        }
        Some(u) if u == u16::from(b'+') => {
            at += 1;
            false
        }
        _ => false,
    };
    let mut radix = match radix {
        0 => 0u32,
        r if (2..=36).contains(&r) => r as u32,
        _ => return f64::NAN,
    };
    if radix == 0 || radix == 16 {
        let is_hex_prefix = units.get(at).copied() == Some(u16::from(b'0'))
            && matches!(units.get(at + 1).copied(), Some(u) if u == u16::from(b'x') || u == u16::from(b'X'));
        if is_hex_prefix {
            at += 2;
            radix = 16;
        } else if radix == 0 {
            radix = 10;
        }
    }
    let start = at;
    while units.get(at).copied().and_then(|u| radix_digit(u, radix)).is_some() {
        at += 1;
    }
    if at == start {
        return f64::NAN;
    }
    let value = digits_to_number(&units[start..at], radix);
    if negative {
        -value
    } else {
        value
    }
}

/// `Number.prototype.toString(radix)` for a radix other than 10.
///
/// THE FRACTION IS THE HARD HALF AND IT IS BOUNDED HERE ON PURPOSE. The standard leaves the
/// number of fractional digits implementation-defined, requiring only that the result round-trips
/// closely enough; an unbounded loop multiplying by the radix does not terminate for a value with
/// no finite representation in that base -- `0.1` in base 3 goes forever. 52 digits is past the
/// point where a `f64` carries information.
fn number_to_radix_string(number: f64, radix: u32) -> String {
    if number.is_nan() {
        return String::from("NaN");
    }
    if number == 0.0 {
        return String::from("0");
    }
    if number.is_infinite() {
        return String::from(if number > 0.0 { "Infinity" } else { "-Infinity" });
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let negative = number < 0.0;
    let magnitude = if negative { -number } else { number };
    let mut integer = crate::math::floor(magnitude);
    let mut fraction = magnitude - integer;

    let mut whole: Vec<u8> = Vec::new();
    if integer == 0.0 {
        whole.push(b'0');
    }
    while integer >= 1.0 {
        let quotient = crate::math::floor(integer / f64::from(radix));
        let digit = integer - quotient * f64::from(radix);
        whole.push(DIGITS[digit as usize]);
        integer = quotient;
    }
    whole.reverse();

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    for byte in &whole {
        out.push(char::from(*byte));
    }
    if fraction > 0.0 {
        out.push('.');
        let mut emitted = 0;
        while fraction > 0.0 && emitted < 52 {
            fraction *= f64::from(radix);
            let digit = crate::math::floor(fraction);
            out.push(char::from(DIGITS[(digit as usize).min(35)]));
            fraction -= digit;
            emitted += 1;
        }
    }
    out
}

/// A position clamped into `0..=length`, where a NEGATIVE value means the start rather than
/// counting back from the end.
///
/// NOT [`relative_index`], and confusing the two is a real difference in behaviour:
/// `"word".includes("d", -1)` searches the WHOLE string, where `"word".slice(-1)` is `"d"`. The
/// position arguments of `includes`, `startsWith` and `endsWith` are the clamping kind; the bounds
/// of `slice` and `splice` are the relative kind. Same shape, opposite rule for a negative number.
fn clamp_index(position: f64, length: f64) -> f64 {
    if position.is_nan() {
        return 0.0;
    }
    position.max(0.0).min(length)
}

/// Builds the whole realm. Called once per [`Interpreter`], which is once per Test262 run.
pub(crate) fn install(interpreter: &mut Interpreter) {
    interpreter.intrinsics.object_prototype = interpreter.allocate(Object::new(None));
    let object_prototype = interpreter.intrinsics.object_prototype;
    interpreter.intrinsics.function_prototype =
        interpreter.allocate(Object::new(Some(object_prototype)));

    install_function_prototype(interpreter);
    install_object(interpreter);
    install_errors(interpreter);
    install_array(interpreter);
    install_string(interpreter);
    install_number(interpreter);
    install_boolean(interpreter);
    install_symbol(interpreter);
    array_species(interpreter);
    array_unscopables(interpreter);
    function_has_instance(interpreter);
    crate::iterator::install(interpreter);
    crate::generator::install(interpreter);
    crate::regexp::install(interpreter);
    install_math(interpreter);
    crate::json::install(interpreter);
    crate::collections::install(interpreter);
    crate::binary::install(interpreter);
    crate::typed_array::install(interpreter);
    crate::promise::install(interpreter);
    crate::reflect::install(interpreter);
    crate::proxy::install(interpreter);
    crate::date::install(interpreter);
    install_global_functions(interpreter);
    crate::uri::install(interpreter);
    install_global_this(interpreter);

    #[cfg(feature = "mutation-census")]
    interpreter.seal_realm();

}


/// `%ThrowTypeError%`, and the two accessors on `Function.prototype` that are made of it.
///
/// # IT IS A FUNCTION WHOSE ONLY JOB IS TO REFUSE, AND ITS SHAPE IS TESTED HARDER THAN ITS BEHAVIOR
///
/// `Function.prototype.caller` and `Function.prototype.arguments` are accessor properties whose
/// getter and setter are both this one function -- the standard's way of making the legacy
/// `f.caller` walk unavailable without leaving the property simply absent, which programs feature-
/// detect on. Test262 checks `length` 0, `name` `""`, no `prototype` property, that it is not a
/// constructor, that it is FROZEN and non-extensible, and that every site in the realm hands back
/// **the same object**.
///
/// Its `name` and `length` are non-configurable, unlike every other built-in's. That is stated in
/// its own clause and nowhere else, and `verifyProperty` checks it directly.
fn install_throw_type_error(interpreter: &mut Interpreter) {
    let function = interpreter.native_function("", 0, |interpreter, _this, _arguments| {
        interpreter.type_error(
            "`caller`, `callee` and `arguments` may not be read on a strict function or its \
             arguments object",
        )
    });
    interpreter.intrinsics.throw_type_error = function;
    for name in ["name", "length"] {
        let value = match name {
            "name" => JsValue::String(JsString::from("")),
            _ => JsValue::Number(0.0),
        };
        interpreter.object_mut(function).set_own(
            PropertyKey::from_str(name),
            Property {
                kind: PropertyKind::Data { value, writable: false },
                enumerable: false,
                configurable: false,
            },
        );
    }
    interpreter.object_mut(function).delete_own(&PropertyKey::from_str("prototype"));
    interpreter.object_mut(function).extensible = false;

    let prototype = interpreter.intrinsics.function_prototype;
    for name in ["caller", "arguments"] {
        interpreter.object_mut(prototype).set_own(
            PropertyKey::from_str(name),
            Property::accessor(Some(function), Some(function)),
        );
    }
}

/// `Function.prototype[@@hasInstance]`, which is where the ORDINARY meaning of `instanceof` lives.
///
/// # ITS ABSENCE IS NOT A MISSING CONVENIENCE -- IT IS WHY THE OPERATOR'S HOOK WAS UNTESTABLE
///
/// The prototype-chain walk is `OrdinaryHasInstance`, and it is what THIS METHOD does -- not what
/// the operator does. `InstanceofOperator` looks `@@hasInstance` up on the right operand and calls
/// it; every function inherits this one, which is how the ordinary case still works. An engine that
/// hard-codes the walk into the operator answers correctly right up until a program overrides the
/// method, and then answers a plausible `false`.
///
/// NON-WRITABLE, NON-ENUMERABLE **AND NON-CONFIGURABLE** -- the standard names all three, and it
/// is one of a handful of properties in the language that cannot be replaced at all. Its `name` is
/// the bracketed form, which the corpus checks directly.
///
/// **INSTALLED AFTER `install_symbol`, NOT IN `install_function_prototype`.** That installer is
/// the FIRST thing the realm builds, before any well-known symbol is a value -- so keying a property
/// off one there writes it under a placeholder id that no lookup will ever produce, and nothing
/// fails. `Array`'s `@@species` was put there once and read `undefined` for it.
fn function_has_instance(interpreter: &mut Interpreter) {
    let prototype = interpreter.intrinsics.function_prototype;
    let has_instance =
        interpreter.native_function("[Symbol.hasInstance]", 1, |interpreter, this, arguments| {
            let JsValue::Object(constructor) = this else {
                return Completion::Normal(JsValue::Boolean(false));
            };
            match interpreter.ordinary_has_instance_for_builtin(constructor, arg(arguments, 0)) {
                Ok(answer) => Completion::Normal(JsValue::Boolean(answer)),
                Err(abrupt) => abrupt,
            }
        });
    let key = PropertyKey::Symbol(interpreter.well_known_symbol("hasInstance"));
    interpreter.object_mut(prototype).set_own(
        key,
        Property {
            kind: PropertyKind::Data { value: JsValue::Object(has_instance), writable: false },
            enumerable: false,
            configurable: false,
        },
    );
}

fn install_function_prototype(interpreter: &mut Interpreter) {
    let prototype = interpreter.intrinsics.function_prototype;

    let empty = interpreter.native_function("", 0, |_interpreter, _this, _arguments| {
        Completion::Normal(JsValue::Undefined)
    });
    if let Some(callable) = interpreter.object(empty).callable {
        interpreter.object_mut(prototype).callable = Some(callable);
    }
    interpreter.set_function_shape(prototype, "", 0);
    install_throw_type_error(interpreter);

    interpreter.define_method(prototype, "call", 1, |interpreter, this, arguments| {
        let rest = if arguments.is_empty() { Vec::new() } else { arguments[1..].to_vec() };
        interpreter.call_value(&this, arg(arguments, 0), rest)
    });

    interpreter.define_method(prototype, "apply", 2, |interpreter, this, arguments| {
        let list = arg(arguments, 1);
        let values = match &list {
            JsValue::Undefined | JsValue::Null => Vec::new(),
            other if !other.is_object() => {
                return interpreter.type_error("Function.prototype.apply needs an array-like object")
            }
            other => match array_like_to_vec(interpreter, other) {
                Ok(values) => values,
                Err(abrupt) => return abrupt,
            },
        };
        interpreter.call_value(&this, arg(arguments, 0), values)
    });

    interpreter.define_method(prototype, "bind", 1, |interpreter, this, arguments| {
        let JsValue::Object(target) = this else {
            return interpreter.type_error("bind must be called on a function");
        };
        if !interpreter.is_callable(&JsValue::Object(target)) {
            return interpreter.type_error("bind must be called on a function");
        }
        let bound = if arguments.is_empty() { Vec::new() } else { arguments[1..].to_vec() };
        let proto = match interpreter.get_prototype_of(target) {
            Ok(proto) => proto,
            Err(abrupt) => return abrupt,
        };
        let has_own_length =
            match interpreter.own_property(target, &PropertyKey::from_str("length")) {
                Ok(property) => property.is_some(),
                Err(abrupt) => return abrupt,
            };
        let length = if has_own_length {
            match interpreter
                .get_member(&JsValue::Object(target), &PropertyKey::from_str("length"))
            {
                Completion::Normal(JsValue::Number(length)) if length == f64::INFINITY => length,
                Completion::Normal(JsValue::Number(length)) if length.is_nan() => 0.0,
                Completion::Normal(JsValue::Number(length)) => {
                    (crate::math::trunc(length) - bound.len() as f64).max(0.0)
                }
                Completion::Normal(_) => 0.0,
                abrupt => return abrupt,
            }
        } else {
            0.0
        };
        let target_name = match interpreter
            .get_member(&JsValue::Object(target), &PropertyKey::from_str("name"))
        {
            Completion::Normal(JsValue::String(name)) => name.to_lossy_string(),
            Completion::Normal(_) => String::new(),
            abrupt => return abrupt,
        };
        let name = format!("bound {target_name}");
        let id = interpreter.bind_function(target, arg(arguments, 0), bound, &name, length, proto);
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        if !interpreter.is_callable(&this) {
            return interpreter.type_error("Function.prototype.toString requires a function");
        }
        let name = match interpreter.get_member(&this, &PropertyKey::from_str("name")) {
            Completion::Normal(JsValue::String(name)) => name.to_lossy_string(),
            _ => String::new(),
        };
        Completion::Normal(JsValue::string(&format!("function {name}() {{ [native code] }}")))
    });

    let refuse: crate::interpreter::NativeFn = |interpreter, _this, _arguments| {
        interpreter.refuse(Absence::FunctionConstructor)
    };
    let constructor = interpreter.native_constructor("Function", 1, refuse, refuse);
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Function", JsValue::Object(constructor));
}

/// Reads `length` and then each index: the array-like protocol, which is NOT the iterator protocol.
///
/// Using the iterator protocol here would be a different operation with different observable
/// calls. `apply`, `Array.prototype.map.call` and the other generic methods are specified over
/// `length` and indexed reads, which is why they work on `arguments` and on plain objects.
pub(crate) fn array_like_to_vec(
    interpreter: &mut Interpreter,
    value: &JsValue,
) -> Result<Vec<JsValue>, Completion> {
    let length = match interpreter.get_member(value, &PropertyKey::from_str("length")) {
        Completion::Normal(length) => length,
        abrupt => return Err(abrupt),
    };
    let length = to_length(interpreter.to_number_value(&length)?);
    if length > 4_294_967_295.0 {
        return Err(interpreter.throw("RangeError", "the list is longer than any array may be"));
    }
    let mut values = Vec::new();
    let mut index = 0.0f64;
    while index < length {
        let key = PropertyKey::from_str(&ops::number_to_string(index));
        match interpreter.get_member(value, &key) {
            Completion::Normal(element) => values.push(element),
            abrupt => return Err(abrupt),
        }
        index += 1.0;
    }
    Ok(values)
}


fn install_object(interpreter: &mut Interpreter) {
    let prototype = interpreter.intrinsics.object_prototype;

    interpreter.define_method(prototype, "hasOwnProperty", 1, |interpreter, this, arguments| {
        let key = match interpreter.to_property_key_value(&arg(arguments, 0)) {
            Ok(key) => key,
            Err(abrupt) => return abrupt,
        };
        let id = match interpreter.to_object(&this) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        match interpreter.own_property(id, &key) {
            Ok(property) => Completion::Normal(JsValue::Boolean(property.is_some())),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(
        prototype,
        "propertyIsEnumerable",
        1,
        |interpreter, this, arguments| {
            let key = match interpreter.to_property_key_value(&arg(arguments, 0)) {
                Ok(key) => key,
                Err(abrupt) => return abrupt,
            };
            let id = match interpreter.to_object(&this) {
                Ok(id) => id,
                Err(abrupt) => return abrupt,
            };
            let enumerable = match interpreter.own_property(id, &key) {
                Ok(property) => property.is_some_and(|property| property.enumerable),
                Err(abrupt) => return abrupt,
            };
            Completion::Normal(JsValue::Boolean(enumerable))
        },
    );

    let get_proto = interpreter.native_function("get __proto__", 0, |interpreter, this, _| {
        let id = match interpreter.to_object(&this) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        match interpreter.get_prototype_of(id) {
            Ok(Some(prototype)) => Completion::Normal(JsValue::Object(prototype)),
            Ok(None) => Completion::Normal(JsValue::Null),
            Err(abrupt) => abrupt,
        }
    });
    let set_proto = interpreter.native_function("set __proto__", 1, |interpreter, this, arguments| {
        if matches!(this, JsValue::Undefined | JsValue::Null) {
            return interpreter.type_error("cannot set the prototype of null or undefined");
        }
        let value = arg(arguments, 0);
        let JsValue::Object(id) = this else { return Completion::Normal(JsValue::Undefined) };
        let prototype = match value {
            JsValue::Object(prototype) => Some(prototype),
            JsValue::Null => None,
            _ => return Completion::Normal(JsValue::Undefined),
        };
        match interpreter.set_prototype_of(id, prototype) {
            Ok(true) => Completion::Normal(JsValue::Undefined),
            Ok(false) => interpreter.type_error(
                "that prototype cannot be set: the object is not extensible, or the chain would contain a cycle",
            ),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.object_mut(prototype).set_own(
        PropertyKey::from_str("__proto__"),
        Property {
            kind: PropertyKind::Accessor { get: Some(get_proto), set: Some(set_proto) },
            enumerable: false,
            configurable: true,
        },
    );

    interpreter.define_method(prototype, "__defineGetter__", 2, |interpreter, this, arguments| {
        define_accessor_half(interpreter, this, arguments, false)
    });
    interpreter.define_method(prototype, "__defineSetter__", 2, |interpreter, this, arguments| {
        define_accessor_half(interpreter, this, arguments, true)
    });
    interpreter.define_method(prototype, "__lookupGetter__", 1, |interpreter, this, arguments| {
        lookup_accessor_half(interpreter, this, arguments, false)
    });
    interpreter.define_method(prototype, "__lookupSetter__", 1, |interpreter, this, arguments| {
        lookup_accessor_half(interpreter, this, arguments, true)
    });

    interpreter.define_method(prototype, "isPrototypeOf", 1, |interpreter, this, arguments| {
        let JsValue::Object(target) = arg(arguments, 0) else {
            return Completion::Normal(JsValue::Boolean(false));
        };
        let this_id = match interpreter.to_object(&this) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let mut current = ok_or_abrupt!(interpreter.get_prototype_of(target));
        while let Some(id) = current {
            if id == this_id {
                return Completion::Normal(JsValue::Boolean(true));
            }
            current = ok_or_abrupt!(interpreter.get_prototype_of(id));
        }
        Completion::Normal(JsValue::Boolean(false))
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        let this = match &this {
            JsValue::Undefined => return Completion::Normal(JsValue::string("[object Undefined]")),
            JsValue::Null => return Completion::Normal(JsValue::string("[object Null]")),
            other => match interpreter.to_object(other) {
                Ok(id) => JsValue::Object(id),
                Err(abrupt) => return abrupt,
            },
        };
        let array = match &this {
            JsValue::Object(id) => ok_or_abrupt!(interpreter.is_array(*id)),
            _ => false,
        };
        let tag = match &this {
            JsValue::Undefined => "Undefined",
            JsValue::Null => "Null",
            JsValue::Number(_) => "Number",
            JsValue::String(_) => "String",
            JsValue::Boolean(_) => "Boolean",
            JsValue::Symbol(_) => "Symbol",
            JsValue::Object(id) => {
                let object = interpreter.object(*id);
                if array {
                    "Array"
                } else if object.error {
                    "Error"
                } else if object.arguments.is_some() {
                    "Arguments"
                } else if object.callable.is_some() {
                    "Function"
                } else if object.date.is_some() {
                    "Date"
                } else if object.regexp.is_some() {
                    "RegExp"
                } else {
                    match &object.primitive {
                        Some(JsValue::Boolean(_)) => "Boolean",
                        Some(JsValue::Number(_)) => "Number",
                        Some(JsValue::String(_)) => "String",
                        _ => "Object",
                    }
                }
            }
        };
        if let JsValue::Object(id) = &this {
            let key = PropertyKey::Symbol(interpreter.well_known_symbol("toStringTag"));
            let tagged = match interpreter.has_property(*id, &key) {
                Ok(tagged) => tagged,
                Err(abrupt) => return abrupt,
            };
            if tagged {
                match interpreter.get_property(*id, &key) {
                    Completion::Normal(JsValue::String(text)) => {
                        let mut rendered = JsString::from("[object ");
                        rendered.extend_from(&text);
                        rendered.push_str("]");
                        return Completion::Normal(JsValue::String(rendered));
                    }
                    Completion::Normal(_) => {}
                    abrupt => return abrupt,
                }
            }
        }
        Completion::Normal(JsValue::string(&format!("[object {tag}]")))
    });

    interpreter.define_method(prototype, "valueOf", 0, |interpreter, this, _| {
        match interpreter.to_object(&this) {
            Ok(id) => Completion::Normal(JsValue::Object(id)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "toLocaleString", 0, |interpreter, this, _| {
        interpreter.invoke(&this, "toString", Vec::new())
    });
    if let Some(JsValue::Object(id)) = interpreter
        .object(prototype)
        .own(&PropertyKey::from_str("toString"))
        .and_then(|property| property.data_value().cloned())
    {
        interpreter.intrinsics.object_prototype_to_string = id;
    }

    let constructor = interpreter.native_constructor(
        "Object",
        1,
        |interpreter, _this, arguments| object_constructor(interpreter, arguments),
        |interpreter, _this, arguments| object_constructor(interpreter, arguments),
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Object", JsValue::Object(constructor));

    interpreter.define_method(constructor, "keys", 1, |interpreter, _this, arguments| {
        own_key_list(interpreter, &arg(arguments, 0), true)
    });
    interpreter.define_method(
        constructor,
        "getOwnPropertyNames",
        1,
        |interpreter, _this, arguments| own_key_list(interpreter, &arg(arguments, 0), false),
    );

    interpreter.define_method(
        constructor,
        "getOwnPropertySymbols",
        1,
        |interpreter, _this, arguments| {
            let id = match interpreter.to_object(&arg(arguments, 0)) {
                Ok(id) => id,
                Err(abrupt) => return abrupt,
            };
            let keys = match interpreter.own_symbol_keys_of(id) {
                Ok(keys) => keys,
                Err(abrupt) => return abrupt,
            };
            let values: Vec<JsValue> = keys
                .into_iter()
                .map(|key| match key {
                    PropertyKey::Symbol(symbol) => JsValue::Symbol(symbol),
                    PropertyKey::String(_) => JsValue::Undefined,
                })
                .collect();
            let array = interpreter.new_array(values);
            Completion::Normal(JsValue::Object(array))
        },
    );

    interpreter.define_method(constructor, "values", 1, |interpreter, _this, arguments| {
        object_entries(interpreter, &arg(arguments, 0), false)
    });
    interpreter.define_method(constructor, "fromEntries", 1, |interpreter, _this, arguments| {
        let iterable = arg(arguments, 0);
        if matches!(iterable, JsValue::Undefined | JsValue::Null) {
            return interpreter.type_error("Object.fromEntries requires an iterable");
        }
        let entries = match crate::iterator::iterate_to_list(interpreter, &iterable) {
            Ok(entries) => entries,
            Err(abrupt) => return abrupt,
        };
        let object_prototype = interpreter.intrinsics.object_prototype;
        let target = interpreter.allocate(Object::new(Some(object_prototype)));
        for entry in entries {
            let JsValue::Object(pair) = entry else {
                return interpreter.type_error("Object.fromEntries needs each entry to be an object");
            };
            let key = match interpreter.get_property(pair, &PropertyKey::from_str("0")) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            let value = match interpreter.get_property(pair, &PropertyKey::from_str("1")) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            let key = match interpreter.to_property_key_value(&key) {
                Ok(key) => key,
                Err(abrupt) => return abrupt,
            };
            interpreter.object_mut(target).set_own(key, Property::data(value));
        }
        Completion::Normal(JsValue::Object(target))
    });

    interpreter.define_method(constructor, "entries", 1, |interpreter, _this, arguments| {
        object_entries(interpreter, &arg(arguments, 0), true)
    });

    interpreter.define_method(constructor, "groupBy", 2, crate::iterator::object_group_by);

    interpreter.define_method(constructor, "getPrototypeOf", 1, |interpreter, _this, arguments| {
        let id = match interpreter.to_object(&arg(arguments, 0)) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        match interpreter.get_prototype_of(id) {
            Ok(Some(prototype)) => Completion::Normal(JsValue::Object(prototype)),
            Ok(None) => Completion::Normal(JsValue::Null),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(constructor, "setPrototypeOf", 2, |interpreter, _this, arguments| {
        let target = arg(arguments, 0);
        let JsValue::Object(id) = target else {
            if matches!(target, JsValue::Undefined | JsValue::Null) {
                return interpreter.type_error("cannot set the prototype of null or undefined");
            }
            return match arg(arguments, 1) {
                JsValue::Object(_) | JsValue::Null => Completion::Normal(target),
                _ => interpreter.type_error("a prototype must be an object or null"),
            };
        };
        let prototype = match arg(arguments, 1) {
            JsValue::Object(prototype) => Some(prototype),
            JsValue::Null => None,
            _ => return interpreter.type_error("a prototype must be an object or null"),
        };
        match interpreter.set_prototype_of(id, prototype) {
            Ok(true) => return Completion::Normal(target),
            Ok(false) => {}
            Err(abrupt) => return abrupt,
        }
        interpreter.type_error("that prototype cannot be set: the object is not extensible, or the chain would contain a cycle")
    });

    interpreter.define_method(constructor, "create", 2, |interpreter, _this, arguments| {
        let prototype = match arg(arguments, 0) {
            JsValue::Object(id) => Some(id),
            JsValue::Null => None,
            _ => return interpreter.type_error("a prototype must be an object or null"),
        };
        let id = interpreter.allocate(Object::new(prototype));
        let created = JsValue::Object(id);
        if !matches!(arg(arguments, 1), JsValue::Undefined) {
            match define_properties(interpreter, &created, &arg(arguments, 1)) {
                Completion::Normal(_) => {}
                abrupt => return abrupt,
            }
        }
        Completion::Normal(created)
    });

    interpreter.define_method(constructor, "hasOwn", 2, |interpreter, _this, arguments| {
        let id = match interpreter.to_object(&arg(arguments, 0)) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let key = match interpreter.to_property_key_value(&arg(arguments, 1)) {
            Ok(key) => key,
            Err(abrupt) => return abrupt,
        };
        match interpreter.own_property(id, &key) {
            Ok(property) => Completion::Normal(JsValue::Boolean(property.is_some())),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(constructor, "is", 2, |_interpreter, _this, arguments| {
        Completion::Normal(JsValue::Boolean(same_value(&arg(arguments, 0), &arg(arguments, 1))))
    });

    interpreter.define_method(constructor, "defineProperties", 2, |interpreter, _this, arguments| {
        let target = arg(arguments, 0);
        if !target.is_object() {
            return interpreter.type_error("Object.defineProperties called on a non-object");
        }
        match define_properties(interpreter, &target, &arg(arguments, 1)) {
            Completion::Normal(_) => Completion::Normal(target),
            abrupt => abrupt,
        }
    });

    interpreter.define_method(constructor, "assign", 2, |interpreter, _this, arguments| {
        let target = arg(arguments, 0);
        let target_id = match interpreter.to_object(&target) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        for source in arguments.iter().skip(1) {
            if matches!(source, JsValue::Undefined | JsValue::Null) {
                continue;
            }
            let source_id = match interpreter.to_object(source) {
                Ok(id) => id,
                Err(abrupt) => return abrupt,
            };
            let keys = match interpreter.enumerable_own_keys(source_id) {
                Ok(keys) => keys,
                Err(abrupt) => return abrupt,
            };
            for key in keys {
                let value = match interpreter.get_property(source_id, &key) {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                };
                let completion = interpreter.set_property_or_throw(target_id, key, value);
                if completion.is_abrupt() {
                    return completion;
                }
            }
        }
        Completion::Normal(JsValue::Object(target_id))
    });

    for (name, call) in EXTENSIBILITY {
        interpreter.define_method(constructor, name, 1, call);
    }

    interpreter.define_method(
        constructor,
        "getOwnPropertyDescriptor",
        2,
        |interpreter, _this, arguments| {
            let id = match interpreter.to_object(&arg(arguments, 0)) {
                Ok(id) => id,
                Err(abrupt) => return abrupt,
            };
            let key = match interpreter.to_property_key_value(&arg(arguments, 1)) {
                Ok(key) => key,
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

    interpreter.define_method(
        constructor,
        "getOwnPropertyDescriptors",
        1,
        |interpreter, _this, arguments| {
            let id = match interpreter.to_object(&arg(arguments, 0)) {
                Ok(id) => id,
                Err(abrupt) => return abrupt,
            };
            let result =
                interpreter.allocate(Object::new(Some(interpreter.intrinsics.object_prototype)));
            let keys = match interpreter.own_keys_of(id) {
                Ok(keys) => keys,
                Err(abrupt) => return abrupt,
            };
            for key in keys {
                let property = match interpreter.own_property(id, &key) {
                    Ok(Some(property)) => property,
                    Ok(None) => continue,
                    Err(abrupt) => return abrupt,
                };
                let descriptor = describe_property(interpreter, &property);
                let _ = interpreter.create_data_property(result, key, JsValue::Object(descriptor));
            }
            Completion::Normal(JsValue::Object(result))
        },
    );

    interpreter.define_method(constructor, "defineProperty", 3, |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return interpreter.type_error("Object.defineProperty called on a non-object");
        };
        let key = match interpreter.to_property_key_value(&arg(arguments, 1)) {
            Ok(key) => key,
            Err(abrupt) => return abrupt,
        };
        let descriptor = match to_property_descriptor(interpreter, &arg(arguments, 2)) {
            Ok(descriptor) => descriptor,
            Err(abrupt) => return abrupt,
        };
        define_property_or_throw(interpreter, id, key, &descriptor)
    });
}

fn object_constructor(interpreter: &mut Interpreter, arguments: &[JsValue]) -> Completion {
    match arg(arguments, 0) {
        JsValue::Undefined | JsValue::Null => {
            let prototype = interpreter.intrinsics.object_prototype;
            let id = interpreter.allocate(Object::new(Some(prototype)));
            Completion::Normal(JsValue::Object(id))
        }
        value => match interpreter.to_object(&value) {
            Ok(id) => Completion::Normal(JsValue::Object(id)),
            Err(abrupt) => abrupt,
        },
    }
}

fn own_key_list(
    interpreter: &mut Interpreter,
    value: &JsValue,
    enumerable_only: bool,
) -> Completion {
    let id = match interpreter.to_object(value) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let keys = if enumerable_only {
        interpreter.enumerable_keys_of(id)
    } else {
        interpreter.own_string_keys_of(id)
    };
    let keys = match keys {
        Ok(keys) => keys,
        Err(abrupt) => return abrupt,
    };
    let values: Vec<JsValue> =
        keys.iter().map(|key| JsValue::string(&key.to_display())).collect();
    let array = interpreter.new_array(values);
    Completion::Normal(JsValue::Object(array))
}

/// `Object.values` and `Object.entries`, which differ only in what each element carries.
fn object_entries(interpreter: &mut Interpreter, value: &JsValue, pairs: bool) -> Completion {
    let id = match interpreter.to_object(value) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let keys = match interpreter.own_string_keys_of(id) {
        Ok(keys) => keys,
        Err(abrupt) => return abrupt,
    };
    let mut elements = Vec::new();
    for key in keys {
        match interpreter.own_property(id, &key) {
            Ok(Some(property)) if property.enumerable => {}
            Err(abrupt) => return abrupt,
            _ => continue,
        }
        let element = match interpreter.get_property(id, &key) {
            Completion::Normal(element) => element,
            abrupt => return abrupt,
        };
        elements.push(if pairs {
            let pair = interpreter
                .new_array(vec![JsValue::string(&key.to_display()), element]);
            JsValue::Object(pair)
        } else {
            element
        });
    }
    let array = interpreter.new_array(elements);
    Completion::Normal(JsValue::Object(array))
}

use ops::same_value;


fn install_errors(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;

    for (index, kind) in ERROR_KINDS.iter().enumerate() {
        let parent = if index == 0 { object_prototype } else { interpreter.intrinsics.error_prototype };
        let prototype = interpreter.allocate(Object::new(Some(parent)));
        interpreter.intrinsics.native_error_prototypes[index] = prototype;
        if index == 0 {
            interpreter.intrinsics.error_prototype = prototype;
        }

        interpreter.define_builtin(prototype, "name", JsValue::string(kind));
        interpreter.define_builtin(prototype, "message", JsValue::string(""));

        if index == 0 {
            interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
            if !matches!(this, JsValue::Object(_)) {
                return interpreter.type_error("Error.prototype.toString requires an object");
            }
            let name = match interpreter.get_member(&this, &PropertyKey::from_str("name")) {
                Completion::Normal(JsValue::Undefined) => JsString::from("Error"),
                Completion::Normal(value) => match interpreter.to_string_value(&value) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            };
            let message = match interpreter.get_member(&this, &PropertyKey::from_str("message")) {
                Completion::Normal(JsValue::Undefined) => JsString::from(""),
                Completion::Normal(value) => match interpreter.to_string_value(&value) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            };
            let rendered = match (name.is_empty(), message.is_empty()) {
                (true, _) => message.to_lossy_string(),
                (false, true) => name.to_lossy_string(),
                (false, false) => {
                    format!("{}: {}", name.to_lossy_string(), message.to_lossy_string())
                }
            };
            Completion::Normal(JsValue::string(&rendered))
            });
        }

        let make = ERROR_MAKERS[index];
        let constructor = interpreter.native_constructor(kind, 1, make, make);
        interpreter.intrinsics.native_error_constructors[index] = constructor;
        if index > 0 {
            let error = interpreter.intrinsics.native_error_constructors[0];
            interpreter.object_mut(constructor).prototype = Some(error);
        }
        interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
        interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
        if index < STANDARD_ERROR_KINDS {
            interpreter.define_global(kind, JsValue::Object(constructor));
        }
    }
}

/// One constructor body per error kind.
///
/// A `fn` pointer carries no captured state, so the kind cannot be closed over -- which is why
/// this is a table parallel to [`ERROR_KINDS`] rather than one function that reads its own name.
const ERROR_MAKERS: [crate::interpreter::NativeFn; ERROR_KINDS.len()] = [
    |interpreter, _this, arguments| make_error(interpreter, "Error", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "TypeError", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "RangeError", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "ReferenceError", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "SyntaxError", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "EvalError", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "URIError", arguments),
    |interpreter, _this, arguments| {
        let completion = make_error(interpreter, "AggregateError", &arguments[1.min(arguments.len())..]);
        let Completion::Normal(JsValue::Object(id)) = &completion else { return completion };
        let id = *id;
        let errors = match crate::iterator::iterate_to_list(interpreter, &arg(arguments, 0)) {
            Ok(errors) => errors,
            Err(abrupt) => return abrupt,
        };
        let list = interpreter.new_array(errors);
        interpreter.object_mut(id).set_own(
            PropertyKey::from_str("errors"),
            Property {
                kind: PropertyKind::Data { value: JsValue::Object(list), writable: true },
                enumerable: false,
                configurable: true,
            },
        );
        completion
    },
    |interpreter, _this, arguments| make_error(interpreter, "InternalError", arguments),
    |interpreter, _this, arguments| make_error(interpreter, "InternalDefect", arguments),
];

fn make_error(interpreter: &mut Interpreter, kind: &str, arguments: &[JsValue]) -> Completion {
    let message = match arg(arguments, 0) {
        JsValue::Undefined => None,
        value => match interpreter.to_string_value(&value) {
            Ok(text) => Some(text.to_lossy_string()),
            Err(abrupt) => return abrupt,
        },
    };
    let error = interpreter.make_error(kind, message.as_deref().unwrap_or(""));
    if message.is_none() {
        if let JsValue::Object(id) = error {
            interpreter.object_mut(id).delete_own(&PropertyKey::from_str("message"));
        }
    }
    if let JsValue::Object(id) = error {
        if let Err(abrupt) = install_error_cause(interpreter, id, &arg(arguments, 1)) {
            return abrupt;
        }
    }
    Completion::Normal(error)
}

/// `InstallErrorCause` (20.5.8.1).
///
/// **ONE FUNCTION BECAUSE THE STANDARD NAMES ONE OPERATION**, called from `Error`,
/// from each of the six NativeErrors, and from `AggregateError`. Every one of them reaches
/// [`make_error`], so this has a single call site -- and `AggregateError` arrives correctly aligned
/// for free, because its maker already slices off the `errors` argument to line its
/// `(message, options)` up with everyone else's.
///
/// **`HasProperty`, NOT `HasOwnProperty`**: an options bag that INHERITS `cause` installs one.
/// And a non-object `options` is not an error -- `new Error('m', 5)` is a perfectly good error with
/// no cause, because step 1's guard simply does not hold.
///
/// The attributes are `CreateNonEnumerableDataPropertyOrThrow`'s: writable, NOT enumerable,
/// configurable -- which is why `create_data_property` cannot serve, it publishes an enumerable
/// one. Spelled with `set_own` like the `errors` property beside it: the standard says
/// `DefinePropertyOrThrow`, and on a freshly built, extensible, ordinary object with no own
/// `cause` that operation cannot refuse, so the two are the same store.
fn install_error_cause(
    interpreter: &mut Interpreter,
    id: ObjectId,
    options: &JsValue,
) -> Result<(), Completion> {
    let JsValue::Object(bag) = options else { return Ok(()) };
    let key = PropertyKey::from_str("cause");
    if !interpreter.has_property(*bag, &key)? {
        return Ok(());
    }
    let cause = match interpreter.get_member(options, &key) {
        Completion::Normal(value) => value,
        abrupt => return Err(abrupt),
    };
    interpreter.object_mut(id).set_own(
        key,
        Property {
            kind: PropertyKind::Data { value: cause, writable: true },
            enumerable: false,
            configurable: true,
        },
    );
    Ok(())
}


/// Step 1 of every generic `Array.prototype` method: **`Let O be ? ToObject(this value)`**.
///
/// THIS ONE MISSING LINE WAS ~500 CONFORMANCE FAILURES ACROSS TWENTY METHODS, and every one of
/// them looked like a separate bug in a separate method. `Array.prototype.map`, `every`, `forEach`,
/// `reduce`, `concat`, `lastIndexOf` and the rest are GENERIC -- they are specified over `length`
/// and indexed reads, so they work on `arguments`, on a plain object, and on a PRIMITIVE. This
/// engine was doing the indexed reads correctly and skipping the conversion, which fails two ways
/// at once:
///
/// - **The callback's third argument is `O`, not the `this` value.** `Array.prototype.map.call(
///   false, cb)` must hand `cb` a **Boolean wrapper**, so `obj instanceof Boolean` is true. Passing
///   the primitive through makes it false, and the failure surfaces inside the user's callback --
///   about as far from the missing line as a failure can land.
/// - **`null` and `undefined` must throw a TypeError, and did not.** Skipping the conversion means
///   reading `length` off nothing, getting `undefined`, and quietly iterating zero times: a method
///   that is required to throw instead returns a plausible answer. That is the silent-deviation
///   shape this tier exists to avoid.
///
/// Boxing is not free and it is not optional. `to_object` allocates a wrapper for a primitive
/// receiver -- but a primitive receiver is the rare path, and correctness is not the place to spend
/// the saving. An object receiver, which is every ordinary call, returns itself unchanged.
///
/// `every_generic_array_method_converts_its_receiver` is what keeps a NEW method from forgetting
/// this. It walks `Array.prototype` and calls each function with a `null` receiver; a method that
/// skipped the step returns something instead of throwing. A comment here would only ask nicely.
fn receiver(interpreter: &mut Interpreter, this: JsValue) -> Result<JsValue, Completion> {
    match interpreter.to_object(&this) {
        Ok(id) => Ok(JsValue::Object(id)),
        Err(abrupt) => Err(abrupt),
    }
}

/// `HasProperty(O, key)` for a receiver that [`receiver`] has already converted.
///
/// THERE IS NO `_ => true` FALLBACK HERE, AND ITS ABSENCE IS LOAD-BEARING. `match &this {
/// Object(id) => has_property(..), _ => true }` says "a non-object receiver has every index",
/// which is not a rule in the standard: it is the only way hole detection can avoid crashing on a
/// `this` that should never have reached it. **A default arm that exists to survive an input the
/// code should have ruled out earlier hides the earlier bug and looks like defensiveness.** With
/// [`receiver`] running step 1, `this` is always an object, so there is no answer left to invent.
fn has_index(
    interpreter: &mut Interpreter,
    this: &JsValue,
    key: &PropertyKey,
) -> Result<bool, Completion> {
    match this {
        JsValue::Object(id) => interpreter.has_property(*id, key),
        _ => Ok(false),
    }
}

fn install_array(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let mut prototype_object = Object::new(Some(object_prototype));
    prototype_object.is_array = true;
    let prototype = interpreter.allocate(prototype_object);
    interpreter.intrinsics.array_prototype = prototype;
    interpreter.object_mut(prototype).set_own(
        PropertyKey::from_str("length"),
        Property { kind: PropertyKind::Data { value: JsValue::Number(0.0), writable: true }, enumerable: false, configurable: false },
    );

    let constructor = interpreter.native_constructor(
        "Array",
        1,
        |interpreter, _this, arguments| array_constructor(interpreter, arguments),
        |interpreter, _this, arguments| array_constructor(interpreter, arguments),
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Array", JsValue::Object(constructor));
    interpreter.intrinsics.array_constructor = constructor;

    interpreter.define_method(constructor, "isArray", 1, |interpreter, _this, arguments| {
        let is_array = match arg(arguments, 0) {
            JsValue::Object(id) => ok_or_abrupt!(interpreter.is_array(id)),
            _ => false,
        };
        Completion::Normal(JsValue::Boolean(is_array))
    });

    interpreter.define_method(prototype, "push", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let mut length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        const MAX_SAFE_INDEX: f64 = 9_007_199_254_740_991.0;
        if length + arguments.len() as f64 > MAX_SAFE_INDEX {
            return interpreter
                .type_error("push would take the length past the largest integer index");
        }
        for value in arguments {
            let key = PropertyKey::from_str(&ops::number_to_string(length));
            if let Err(abrupt) = set_element(interpreter, &this, key, value.clone()) {
                return abrupt;
            }
            length += 1.0;
        }
        if let Err(abrupt) = set_length(interpreter, &this, length) {
            return abrupt;
        }
        Completion::Normal(JsValue::Number(length))
    });

    interpreter.define_method(prototype, "pop", 0, |interpreter, this, _| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        if length == 0.0 {
            if let Err(abrupt) = set_length(interpreter, &this, 0.0) {
                return abrupt;
            }
            return Completion::Normal(JsValue::Undefined);
        }
        let index = length - 1.0;
        let key = PropertyKey::from_str(&ops::number_to_string(index));
        let value = match interpreter.get_member(&this, &key) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        if let JsValue::Object(id) = this {
            if let abrupt @ Completion::Throw(_) = interpreter.delete_property_or_throw(id, &key) {
                return abrupt;
            }
        }
        if let Err(abrupt) = set_length(interpreter, &this, index) {
            return abrupt;
        }
        Completion::Normal(value)
    });

    interpreter.define_method(prototype, "join", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let separator = match arg(arguments, 0) {
            JsValue::Undefined => JsString::from(","),
            value => match interpreter.to_string_value(&value) {
                Ok(text) => text,
                Err(abrupt) => return abrupt,
            },
        };
        let mut out = JsString::new();
        let mut index = 0.0f64;
        while index < length {
            if index > 0.0 {
                out.extend_from(&separator);
            }
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let element = match interpreter.get_member(&this, &key) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            if !matches!(element, JsValue::Undefined | JsValue::Null) {
                match interpreter.to_string_value(&element) {
                    Ok(text) => out.extend_from(&text),
                    Err(abrupt) => return abrupt,
                }
            }
            index += 1.0;
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let join = match interpreter.get_member(&this, &PropertyKey::from_str("join")) {
            Completion::Normal(join) => join,
            abrupt => return abrupt,
        };
        if interpreter.is_callable(&join) {
            return interpreter.call_value(&join, this, Vec::new());
        }
        let function = JsValue::Object(interpreter.intrinsics.object_prototype_to_string);
        interpreter.call_value(&function, this, Vec::new())
    });

    interpreter.define_method(prototype, "toLocaleString", 0, |interpreter, this, _| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let mut out = JsString::new();
        let mut index = 0.0;
        while index < length {
            if index > 0.0 {
                out.push_str(",");
            }
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let element = match interpreter.get_member(&this, &key) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            if !matches!(element, JsValue::Undefined | JsValue::Null) {
                let rendered = match interpreter.invoke(&element, "toLocaleString", Vec::new()) {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                };
                match interpreter.to_string_value(&rendered) {
                    Ok(text) => out.extend_from(&text),
                    Err(abrupt) => return abrupt,
                }
            }
            index += 1.0;
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(prototype, "indexOf", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        if length == 0.0 {
            return Completion::Normal(JsValue::Number(-1.0));
        }
        let target = arg(arguments, 0);
        let start = match arguments.get(1) {
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => relative_index(to_integer(number), length),
                Err(abrupt) => return abrupt,
            },
            None => 0.0,
        };
        let mut index = start;
        while index < length {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let present = ok_or_abrupt!(has_index(interpreter, &this, &key));
            if present {
                let element = match interpreter.get_member(&this, &key) {
                    Completion::Normal(element) => element,
                    abrupt => return abrupt,
                };
                if ops::strict_equals(&element, &target) {
                    return Completion::Normal(JsValue::Number(index));
                }
            }
            index += 1.0;
        }
        Completion::Normal(JsValue::Number(-1.0))
    });

    interpreter.define_method(prototype, "slice", 2, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let start = match arg(arguments, 0) {
            JsValue::Undefined => 0.0,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => relative_index(to_integer(number), length),
                Err(abrupt) => return abrupt,
            },
        };
        let end = match arg(arguments, 1) {
            JsValue::Undefined => length,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => relative_index(to_integer(number), length),
                Err(abrupt) => return abrupt,
            },
        };
        let id = match array_species_create(interpreter, &this, (end - start).max(0.0)) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let mut index = start;
        while index < end {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            if ok_or_abrupt!(has_index(interpreter, &this, &key)) {
                let element = match interpreter.get_member(&this, &key) {
                    Completion::Normal(element) => element,
                    abrupt => return abrupt,
                };
                let target = PropertyKey::from_str(&ops::number_to_string(index - start));
                if let Err(abrupt) = create_data_property_or_throw(interpreter, id, target, element) {
                    return abrupt;
                }
            }
            index += 1.0;
        }
        if let Err(abrupt) = set_length(interpreter, &JsValue::Object(id), (end - start).max(0.0)) {
            return abrupt;
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "concat", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let id = match array_species_create(interpreter, &this, 0.0) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let mut n = 0.0f64;
        for value in core::iter::once(&this).chain(arguments.iter()) {
            let spreadable = match value {
                JsValue::Object(id) => {
                    let key =
                        PropertyKey::Symbol(interpreter.well_known_symbol("isConcatSpreadable"));
                    match interpreter.get_property(*id, &key) {
                        Completion::Normal(JsValue::Undefined) => {
                            ok_or_abrupt!(interpreter.is_array(*id))
                        }
                        Completion::Normal(flag) => ops::to_boolean(&flag),
                        abrupt => return abrupt,
                    }
                }
                _ => false,
            };
            match value {
                JsValue::Object(_) if spreadable => {
                    let length = match array_length(interpreter, value) {
                        Ok(length) => length,
                        Err(abrupt) => return abrupt,
                    };
                    let mut index = 0.0f64;
                    while index < length {
                        let key = PropertyKey::from_str(&ops::number_to_string(index));
                        if ok_or_abrupt!(has_index(interpreter, value, &key)) {
                            let element = match interpreter.get_member(value, &key) {
                                Completion::Normal(element) => element,
                                abrupt => return abrupt,
                            };
                            let at = PropertyKey::from_str(&ops::number_to_string(n));
                            if let Err(abrupt) =
                                create_data_property_or_throw(interpreter, id, at, element)
                            {
                                return abrupt;
                            }
                        }
                        index += 1.0;
                        n += 1.0;
                    }
                }
                other => {
                    let at = PropertyKey::from_str(&ops::number_to_string(n));
                    if let Err(abrupt) =
                        create_data_property_or_throw(interpreter, id, at, other.clone())
                    {
                        return abrupt;
                    }
                    n += 1.0;
                }
            }
        }
        if let Err(abrupt) = set_length(interpreter, &JsValue::Object(id), n) {
            return abrupt;
        }
        Completion::Normal(JsValue::Object(id))
    });

    for (index, name) in
        ["forEach", "map", "filter", "every", "some", "find", "findIndex"].iter().enumerate()
    {
        interpreter.define_method(prototype, name, 1, ITERATION_BODIES[index]);
    }

    for (index, name) in ["reduce", "reduceRight"].iter().enumerate() {
        interpreter.define_method(prototype, name, 1, REDUCE_BODIES[index]);
    }

    interpreter.define_method(prototype, "lastIndexOf", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        if length == 0.0 {
            return Completion::Normal(JsValue::Number(-1.0));
        }
        let target = arg(arguments, 0);
        let mut index = match arguments.get(1) {
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => {
                    let n = to_integer(number);
                    if n >= 0.0 {
                        n.min(length - 1.0)
                    } else {
                        length + n
                    }
                }
                Err(abrupt) => return abrupt,
            },
            None => length - 1.0,
        };
        while index >= 0.0 {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let present = ok_or_abrupt!(has_index(interpreter, &this, &key));
            if present {
                let element = match interpreter.get_member(&this, &key) {
                    Completion::Normal(element) => element,
                    abrupt => return abrupt,
                };
                if ops::strict_equals(&element, &target) {
                    return Completion::Normal(JsValue::Number(index));
                }
            }
            index -= 1.0;
        }
        Completion::Normal(JsValue::Number(-1.0))
    });

    interpreter.define_method(prototype, "shift", 0, |interpreter, this, _| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        if length == 0.0 {
            if let Err(abrupt) = set_length(interpreter, &this, 0.0) {
                return abrupt;
            }
            return Completion::Normal(JsValue::Undefined);
        }
        let first = match interpreter.get_member(&this, &PropertyKey::from_str("0")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        let mut index = 1.0f64;
        while index < length {
            let from = PropertyKey::from_str(&ops::number_to_string(index));
            let to = PropertyKey::from_str(&ops::number_to_string(index - 1.0));
            if ok_or_abrupt!(has_index(interpreter, &this, &from)) {
                let value = match interpreter.get_member(&this, &from) {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                };
                if let Err(abrupt) = set_element(interpreter, &this, to, value) {
                    return abrupt;
                }
            } else if let Err(abrupt) = delete_index(interpreter, &this, &to) {
                return abrupt;
            }
            index += 1.0;
        }
        let last = PropertyKey::from_str(&ops::number_to_string(length - 1.0));
        if let Err(abrupt) = delete_index(interpreter, &this, &last) {
            return abrupt;
        }
        if let Err(abrupt) = set_length(interpreter, &this, length - 1.0) {
            return abrupt;
        }
        Completion::Normal(first)
    });

    interpreter.define_method(prototype, "unshift", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let added = arguments.len() as f64;
        if added > 0.0 {
            if length + added > 9_007_199_254_740_991.0 {
                return interpreter
                    .type_error("unshift would take the length past the largest integer index");
            }
            let mut index = length;
            while index > 0.0 {
                let from = PropertyKey::from_str(&ops::number_to_string(index - 1.0));
                let to = PropertyKey::from_str(&ops::number_to_string(index + added - 1.0));
                if ok_or_abrupt!(has_index(interpreter, &this, &from)) {
                    let value = match interpreter.get_member(&this, &from) {
                        Completion::Normal(value) => value,
                        abrupt => return abrupt,
                    };
                    if let Err(abrupt) = set_element(interpreter, &this, to, value) {
                        return abrupt;
                    }
                } else if let Err(abrupt) = delete_index(interpreter, &this, &to) {
                    return abrupt;
                }
                index -= 1.0;
            }
            for (slot, value) in arguments.iter().enumerate() {
                let key = PropertyKey::from_str(&slot.to_string());
                if let Err(abrupt) = set_element(interpreter, &this, key, value.clone()) {
                    return abrupt;
                }
            }
        }
        if let Err(abrupt) = set_length(interpreter, &this, length + added) {
            return abrupt;
        }
        Completion::Normal(JsValue::Number(length + added))
    });

    interpreter.define_method(prototype, "splice", 2, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let start = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => relative_index(to_integer(number), length),
            Err(abrupt) => return abrupt,
        };
        let delete_count = if arguments.is_empty() {
            0.0
        } else if arguments.len() == 1 {
            length - start
        } else {
            match interpreter.to_number_value(&arg(arguments, 1)) {
                Ok(number) => to_integer(number).max(0.0).min(length - start),
                Err(abrupt) => return abrupt,
            }
        };
        let insert: Vec<JsValue> = arguments.iter().skip(2).cloned().collect();
        if length + insert.len() as f64 - delete_count > 9_007_199_254_740_991.0 {
            return interpreter.type_error("the resulting length is too large");
        }
        let id = match array_species_create(interpreter, &this, delete_count) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let mut offset = 0.0f64;
        while offset < delete_count {
            let from = PropertyKey::from_str(&ops::number_to_string(start + offset));
            if ok_or_abrupt!(has_index(interpreter, &this, &from)) {
                let element = match interpreter.get_member(&this, &from) {
                    Completion::Normal(element) => element,
                    abrupt => return abrupt,
                };
                let at = PropertyKey::from_str(&ops::number_to_string(offset));
                if let Err(abrupt) = create_data_property_or_throw(interpreter, id, at, element) {
                    return abrupt;
                }
            }
            offset += 1.0;
        }
        if let Err(abrupt) = set_length(interpreter, &JsValue::Object(id), delete_count) {
            return abrupt;
        }

        let inserted = insert.len() as f64;
        if inserted < delete_count {
            let mut index = start;
            while index < length - delete_count {
                let from = PropertyKey::from_str(&ops::number_to_string(index + delete_count));
                let to = PropertyKey::from_str(&ops::number_to_string(index + inserted));
                if ok_or_abrupt!(has_index(interpreter, &this, &from)) {
                    let element = match interpreter.get_member(&this, &from) {
                        Completion::Normal(element) => element,
                        abrupt => return abrupt,
                    };
                    if let Err(abrupt) = set_element(interpreter, &this, to, element) {
                        return abrupt;
                    }
                } else if let Err(abrupt) = delete_index(interpreter, &this, &to) {
                    return abrupt;
                }
                index += 1.0;
            }
            let mut index = length;
            while index > length - delete_count + inserted {
                let key = PropertyKey::from_str(&ops::number_to_string(index - 1.0));
                if let Err(abrupt) = delete_index(interpreter, &this, &key) {
                    return abrupt;
                }
                index -= 1.0;
            }
        } else if inserted > delete_count {
            let mut index = length - delete_count;
            while index > start {
                let from = PropertyKey::from_str(&ops::number_to_string(index + delete_count - 1.0));
                let to = PropertyKey::from_str(&ops::number_to_string(index + inserted - 1.0));
                if ok_or_abrupt!(has_index(interpreter, &this, &from)) {
                    let element = match interpreter.get_member(&this, &from) {
                        Completion::Normal(element) => element,
                        abrupt => return abrupt,
                    };
                    if let Err(abrupt) = set_element(interpreter, &this, to, element) {
                        return abrupt;
                    }
                } else if let Err(abrupt) = delete_index(interpreter, &this, &to) {
                    return abrupt;
                }
                index -= 1.0;
            }
        }
        for (offset, element) in insert.into_iter().enumerate() {
            let at = PropertyKey::from_str(&ops::number_to_string(start + offset as f64));
            if let Err(abrupt) = set_element(interpreter, &this, at, element) {
                return abrupt;
            }
        }
        if let Err(abrupt) = set_length(interpreter, &this, length - delete_count + inserted) {
            return abrupt;
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "fill", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let value = arg(arguments, 0);
        let mut bound = |slot: usize, fallback: f64| match arguments.get(slot) {
            Some(JsValue::Undefined) | None => Ok(fallback),
            Some(given) => {
                interpreter.to_number_value(given).map(|n| relative_index(to_integer(n), length))
            }
        };
        let start = match bound(1, 0.0) {
            Ok(start) => start,
            Err(abrupt) => return abrupt,
        };
        let end = match bound(2, length) {
            Ok(end) => end,
            Err(abrupt) => return abrupt,
        };
        let mut index = start;
        while index < end {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            if let Err(abrupt) = set_element(interpreter, &this, key, value.clone()) {
                return abrupt;
            }
            index += 1.0;
        }
        Completion::Normal(this)
    });

    interpreter.define_method(prototype, "sort", 1, |interpreter, this, arguments| {
        let comparator = arg(arguments, 0);
        if !matches!(comparator, JsValue::Undefined) && !interpreter.is_callable(&comparator) {
            return interpreter.type_error("the comparator is not a function");
        }
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let present = match sort_indexed_properties(interpreter, &this, length, &comparator, false) {
            Ok(present) => present,
            Err(abrupt) => return abrupt,
        };
        for (at, value) in present.iter().enumerate() {
            let key = PropertyKey::from_str(&at.to_string());
            if let Err(abrupt) = set_element(interpreter, &this, key, value.clone()) {
                return abrupt;
            }
        }
        if let JsValue::Object(id) = &this {
            let mut at = present.len() as f64;
            while at < length {
                let key = PropertyKey::from_str(&ops::number_to_string(at));
                if let abrupt @ Completion::Throw(_) =
                    interpreter.delete_property_or_throw(*id, &key)
                {
                    return abrupt;
                }
                at += 1.0;
            }
        }
        Completion::Normal(this)
    });

    interpreter.define_method(prototype, "includes", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        if length == 0.0 {
            return Completion::Normal(JsValue::Boolean(false));
        }
        let target = arg(arguments, 0);
        let mut index = match arguments.get(1) {
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => relative_index(to_integer(number), length),
                Err(abrupt) => return abrupt,
            },
            None => 0.0,
        };
        while index < length {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let element = match interpreter.get_member(&this, &key) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            if ops::same_value_zero(&element, &target) {
                return Completion::Normal(JsValue::Boolean(true));
            }
            index += 1.0;
        }
        Completion::Normal(JsValue::Boolean(false))
    });

    interpreter.define_method(prototype, "flat", 0, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let depth = match arguments.first() {
            None | Some(JsValue::Undefined) => 1.0,
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => to_integer(number).max(0.0),
                Err(abrupt) => return abrupt,
            },
        };
        flatten(interpreter, &this, depth, None, &JsValue::Undefined)
    });

    interpreter.define_method(prototype, "flatMap", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let mapper = arg(arguments, 0);
        if !interpreter.is_callable(&mapper) {
            return interpreter.type_error("flatMap's mapping function is not a function");
        }
        flatten(interpreter, &this, 1.0, Some(&mapper), &arg(arguments, 1))
    });

    interpreter.define_method(prototype, "at", 1, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let relative = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        let index = if relative >= 0.0 { relative } else { length + relative };
        if index < 0.0 || index >= length {
            return Completion::Normal(JsValue::Undefined);
        }
        interpreter.get_member(&this, &PropertyKey::from_str(&ops::number_to_string(index)))
    });

    interpreter.define_method(prototype, "copyWithin", 2, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let mut bound = |slot: usize, fallback: f64| match arguments.get(slot) {
            Some(JsValue::Undefined) | None => Ok(fallback),
            Some(given) => {
                interpreter.to_number_value(given).map(|n| relative_index(to_integer(n), length))
            }
        };
        let (target, start, end) = match (bound(0, 0.0), bound(1, 0.0), bound(2, length)) {
            (Ok(target), Ok(start), Ok(end)) => (target, start, end),
            (Err(abrupt), _, _) | (_, Err(abrupt), _) | (_, _, Err(abrupt)) => return abrupt,
        };
        let count = (end - start).min(length - target).max(0.0);
        let mut lifted = Vec::new();
        let mut step = 0.0f64;
        while step < count {
            let key = PropertyKey::from_str(&ops::number_to_string(start + step));
            let present = ok_or_abrupt!(has_index(interpreter, &this, &key));
            let element = if present {
                match interpreter.get_member(&this, &key) {
                    Completion::Normal(element) => Some(element),
                    abrupt => return abrupt,
                }
            } else {
                None
            };
            lifted.push(element);
            step += 1.0;
        }
        for (offset, element) in lifted.into_iter().enumerate() {
            let key = PropertyKey::from_str(&ops::number_to_string(target + offset as f64));
            match element {
                Some(element) => {
                    if let Err(abrupt) = set_element(interpreter, &this, key, element) {
                        return abrupt;
                    }
                }
                None => {
                    if let JsValue::Object(id) = &this {
                        if !interpreter.object_mut(*id).delete_own(&key) {
                            return interpreter
                                .type_error("cannot delete a non-configurable element");
                        }
                    }
                }
            }
        }
        Completion::Normal(this)
    });

    interpreter.define_method(prototype, "reverse", 0, |interpreter, this, _| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let middle = crate::math::floor(length / 2.0);
        let mut lower = 0.0f64;
        while lower != middle {
            let upper = length - lower - 1.0;
            let lower_key = PropertyKey::from_str(&ops::number_to_string(lower));
            let upper_key = PropertyKey::from_str(&ops::number_to_string(upper));
            let lower_exists = ok_or_abrupt!(has_index(interpreter, &this, &lower_key));
            let lower_value = if lower_exists {
                match interpreter.get_member(&this, &lower_key) {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                }
            } else {
                JsValue::Undefined
            };
            let upper_exists = ok_or_abrupt!(has_index(interpreter, &this, &upper_key));
            let upper_value = if upper_exists {
                match interpreter.get_member(&this, &upper_key) {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                }
            } else {
                JsValue::Undefined
            };
            let outcome = match (lower_exists, upper_exists) {
                (true, true) => set_element(interpreter, &this, lower_key, upper_value)
                    .and_then(|()| set_element(interpreter, &this, upper_key, lower_value)),
                (false, true) => set_element(interpreter, &this, lower_key, upper_value)
                    .and_then(|()| delete_index(interpreter, &this, &upper_key)),
                (true, false) => delete_index(interpreter, &this, &lower_key)
                    .and_then(|()| set_element(interpreter, &this, upper_key, lower_value)),
                (false, false) => Ok(()),
            };
            if let Err(abrupt) = outcome {
                return abrupt;
            }
            lower += 1.0;
        }
        Completion::Normal(this)
    });

    install_array_copy_methods(interpreter, prototype);
}

/// The four methods that answer a NEW array where an older one edits the receiver: `toReversed`,
/// `toSorted`, `toSpliced` and `with`.
///
/// # THEY BUILD WITH `ArrayCreate` AND `CreateDataPropertyOrThrow`, NOT `ArraySpeciesCreate`
///
/// `reverse`/`sort`/`splice` consult `Symbol.species`; these four deliberately do NOT -- the
/// standard has them make a plain `%Array%` whatever the receiver's constructor says, and
/// `Reflect.getPrototypeOf(new MyArray().toReversed())` is `Array.prototype`. Reaching for the
/// species helper because the neighbouring method uses it is the mistake this note exists to stop.
///
/// AND THEY READ THROUGH HOLES. `[, 1].toReversed()` is `[1, undefined]` with TWO own properties,
/// where `reverse` preserves the hole. Every index from 0 to `length` is read, so a getter on the
/// prototype chain runs -- which is the observable half of "read-through-holes".
fn install_array_copy_methods(interpreter: &mut Interpreter, prototype: crate::value::ObjectId) {
    interpreter.define_method(prototype, "toReversed", 0, |interpreter, this, _| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let id = match new_array_of_length(interpreter, length) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let mut index = 0.0f64;
        while index < length {
            let from = PropertyKey::from_str(&ops::number_to_string(length - index - 1.0));
            let element = match interpreter.get_member(&this, &from) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            let at = PropertyKey::from_str(&ops::number_to_string(index));
            if let Err(abrupt) = create_data_property_or_throw(interpreter, id, at, element) {
                return abrupt;
            }
            index += 1.0;
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "toSorted", 1, |interpreter, this, arguments| {
        let comparator = arg(arguments, 0);
        if !matches!(comparator, JsValue::Undefined) && !interpreter.is_callable(&comparator) {
            return interpreter.type_error("the comparator is not a function");
        }
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let id = match new_array_of_length(interpreter, length) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let sorted = match sort_indexed_properties(interpreter, &this, length, &comparator, true) {
            Ok(sorted) => sorted,
            Err(abrupt) => return abrupt,
        };
        for (at, value) in sorted.into_iter().enumerate() {
            let key = PropertyKey::from_str(&at.to_string());
            if let Err(abrupt) = create_data_property_or_throw(interpreter, id, key, value) {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "toSpliced", 2, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let start = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => relative_index(to_integer(number), length),
            Err(abrupt) => return abrupt,
        };
        let insert: Vec<JsValue> = arguments.iter().skip(2).cloned().collect();
        let skip = if arguments.is_empty() {
            0.0
        } else if arguments.len() == 1 {
            length - start
        } else {
            match interpreter.to_number_value(&arg(arguments, 1)) {
                Ok(number) => to_integer(number).clamp(0.0, length - start),
                Err(abrupt) => return abrupt,
            }
        };
        let new_length = length + insert.len() as f64 - skip;
        if new_length > 9_007_199_254_740_991.0 {
            return interpreter.type_error("the resulting length is too large");
        }
        let id = match new_array_of_length(interpreter, new_length) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let mut write = 0.0f64;
        while write < start {
            let key = PropertyKey::from_str(&ops::number_to_string(write));
            let element = match interpreter.get_member(&this, &key) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            if let Err(abrupt) = create_data_property_or_throw(interpreter, id, key, element) {
                return abrupt;
            }
            write += 1.0;
        }
        for item in insert {
            let key = PropertyKey::from_str(&ops::number_to_string(write));
            if let Err(abrupt) = create_data_property_or_throw(interpreter, id, key, item) {
                return abrupt;
            }
            write += 1.0;
        }
        let mut read = start + skip;
        while write < new_length {
            let from = PropertyKey::from_str(&ops::number_to_string(read));
            let element = match interpreter.get_member(&this, &from) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            let key = PropertyKey::from_str(&ops::number_to_string(write));
            if let Err(abrupt) = create_data_property_or_throw(interpreter, id, key, element) {
                return abrupt;
            }
            write += 1.0;
            read += 1.0;
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "with", 2, |interpreter, this, arguments| {
        let this = match receiver(interpreter, this) {
            Ok(object) => object,
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &this) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let relative = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        let at = if relative >= 0.0 { relative } else { length + relative };
        if at < 0.0 || at >= length {
            return interpreter.throw("RangeError", "that index is outside the array");
        }
        let id = match new_array_of_length(interpreter, length) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let replacement = arg(arguments, 1);
        let mut index = 0.0f64;
        while index < length {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let element = if index == at {
                replacement.clone()
            } else {
                match interpreter.get_member(&this, &key) {
                    Completion::Normal(element) => element,
                    abrupt => return abrupt,
                }
            };
            if let Err(abrupt) = create_data_property_or_throw(interpreter, id, key, element) {
                return abrupt;
            }
            index += 1.0;
        }
        Completion::Normal(JsValue::Object(id))
    });
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum IterationKind {
    ForEach,
    Map,
    Filter,
    Every,
    Some,
    Find,
    FindIndex,
}

/// The callback-taking Array methods, which differ only in what they do with each result.
const ITERATION_BODIES: [crate::interpreter::NativeFn; 7] = [
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::ForEach),
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::Map),
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::Filter),
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::Every),
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::Some),
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::Find),
    |interpreter, this, arguments| iterate(interpreter, this, arguments, IterationKind::FindIndex),
];

const REDUCE_BODIES: [crate::interpreter::NativeFn; 2] = [
    |interpreter, this, arguments| reduce(interpreter, this, arguments, false),
    |interpreter, this, arguments| reduce(interpreter, this, arguments, true),
];

/// `reduce` and `reduceRight`, which differ only in the direction they walk.
///
/// **`reduce` ON AN EMPTY ARRAY WITH NO INITIAL VALUE IS A TypeError, and that is the whole
/// reason the two-argument form exists.** Returning `undefined` instead is the obvious
/// implementation and it silently turns a programming mistake into a value that flows onward.
/// **A HOLE IS SKIPPED, INCLUDING WHEN IT WOULD BE THE INITIAL VALUE** -- the accumulator starts
/// at the first PRESENT element, not at index 0.
fn reduce(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    from_right: bool,
) -> Completion {
    let this = match receiver(interpreter, this) {
        Ok(object) => object,
        Err(abrupt) => return abrupt,
    };
    let length = match array_length(interpreter, &this) {
        Ok(length) => length,
        Err(abrupt) => return abrupt,
    };
    let callback = arg(arguments, 0);
    if !interpreter.is_callable(&callback) {
        return interpreter.type_error("the callback is not a function");
    }
    let mut accumulator = if arguments.len() >= 2 { Some(arguments[1].clone()) } else { None };
    let mut saw_any = accumulator.is_some();
    let mut step = 0.0f64;
    while step < length {
        let index = if from_right { length - 1.0 - step } else { step };
        step += 1.0;
        let key = PropertyKey::from_str(&ops::number_to_string(index));
        let present = ok_or_abrupt!(has_index(interpreter, &this, &key));
        if !present {
            continue;
        }
        let element = match interpreter.get_member(&this, &key) {
            Completion::Normal(element) => element,
            abrupt => return abrupt,
        };
        saw_any = true;
        match accumulator.take() {
            None => accumulator = Some(element),
            Some(previous) => {
                let result = interpreter.call_value(
                    &callback,
                    JsValue::Undefined,
                    vec![previous, element, JsValue::Number(index), this.clone()],
                );
                accumulator = Some(match result {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                });
            }
        }
    }
    match accumulator {
        Some(value) if saw_any => Completion::Normal(value),
        _ => interpreter.type_error("reduce of an empty array with no initial value"),
    }
}


/// `SortIndexedProperties(obj, len, sortCompare, holes)`: the indices collected in order and
/// sorted, without a single write back to the receiver.
///
/// # IT IS ONE NAMED OPERATION AND IT HAS TWO CALLERS THAT DISAGREE ABOUT HOLES
///
/// `sort` passes `skip-holes` and `toSorted` passes `read-through-holes`, and that ONE argument is
/// the entire difference between them: `[, 1].sort()` keeps its hole, and `[, 1].toSorted()`
/// answers `[1, undefined]` -- two own properties. Everything after the collection is identical,
/// which is exactly why this is a function and not a passage copied into the second method. The
/// standard names it, so it is one place here.
///
/// `undefined` SORTS TO THE END WITHOUT EVER REACHING THE COMPARATOR. A sort that hands
/// `undefined` to a user comparator calls it where the standard says it is never called -- and the
/// default comparison is by STRING, so `[10, 9].sort()` is `[10, 9]`, which is the single most
/// surprising line in the language.
fn sort_indexed_properties(
    interpreter: &mut Interpreter,
    this: &JsValue,
    length: f64,
    comparator: &JsValue,
    read_through_holes: bool,
) -> Result<Vec<JsValue>, Completion> {
    let mut items = Vec::new();
    let mut index = 0.0f64;
    while index < length {
        let key = PropertyKey::from_str(&ops::number_to_string(index));
        let present = if read_through_holes { true } else { has_index(interpreter, this, &key)? };
        if present {
            match interpreter.get_member(this, &key) {
                Completion::Normal(element) => items.push(element),
                abrupt => return Err(abrupt),
            }
        }
        index += 1.0;
    }
    let (mut present, undefined): (Vec<JsValue>, Vec<JsValue>) =
        items.into_iter().partition(|value| !matches!(value, JsValue::Undefined));
    for boundary in 1..present.len() {
        let mut at = boundary;
        while at > 0 {
            let ordering =
                compare_for_sort(interpreter, comparator, &present[at - 1], &present[at])?;
            if ordering <= 0.0 {
                break;
            }
            present.swap(at - 1, at);
            at -= 1;
        }
    }
    present.extend(undefined);
    Ok(present)
}

fn compare_for_sort(
    interpreter: &mut Interpreter,
    comparator: &JsValue,
    left: &JsValue,
    right: &JsValue,
) -> Result<f64, Completion> {
    if interpreter.is_callable(comparator) {
        let result = interpreter.call_value(
            comparator,
            JsValue::Undefined,
            vec![left.clone(), right.clone()],
        );
        let value = match result {
            Completion::Normal(value) => value,
            abrupt => return Err(abrupt),
        };
        let number = interpreter.to_number_value(&value)?;
        return Ok(if number.is_nan() { 0.0 } else { number });
    }
    let left = interpreter.to_string_value(left)?;
    let right = interpreter.to_string_value(right)?;
    Ok(match left.units().cmp(right.units()) {
        core::cmp::Ordering::Less => -1.0,
        core::cmp::Ordering::Equal => 0.0,
        core::cmp::Ordering::Greater => 1.0,
    })
}

fn iterate(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    kind: IterationKind,
) -> Completion {
    let this = match receiver(interpreter, this) {
        Ok(object) => object,
        Err(abrupt) => return abrupt,
    };
    let length = match array_length(interpreter, &this) {
        Ok(length) => length,
        Err(abrupt) => return abrupt,
    };
    let callback = arg(arguments, 0);
    if !interpreter.is_callable(&callback) {
        return interpreter.type_error("the callback is not a function");
    }
    let this_argument = arg(arguments, 1);
    let mapped = match kind {
        IterationKind::Map => match array_species_create(interpreter, &this, length) {
            Ok(id) => Some(id),
            Err(abrupt) => return abrupt,
        },
        IterationKind::Filter => match array_species_create(interpreter, &this, 0.0) {
            Ok(id) => Some(id),
            Err(abrupt) => return abrupt,
        },
        _ => None,
    };
    let mut kept = 0.0f64;
    let mut index = 0.0f64;
    while index < length {
        let key = PropertyKey::from_str(&ops::number_to_string(index));
        let present = match kind {
            IterationKind::Find | IterationKind::FindIndex => true,
            _ => ok_or_abrupt!(has_index(interpreter, &this, &key)),
        };
        if present {
            let element = match interpreter.get_member(&this, &key) {
                Completion::Normal(element) => element,
                abrupt => return abrupt,
            };
            let result = interpreter.call_value(
                &callback,
                this_argument.clone(),
                vec![element.clone(), JsValue::Number(index), this.clone()],
            );
            let result = match result {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            match kind {
                IterationKind::ForEach => {}
                IterationKind::Map => {
                    let target = mapped.expect("map allocated its result above");
                    if let Err(abrupt) = create_data_property_or_throw(interpreter, target, key, result) {
                        return abrupt;
                    }
                }
                IterationKind::Filter => {
                    if ops::to_boolean(&result) {
                        let target = mapped.expect("filter allocated its result above");
                        let at = PropertyKey::from_str(&ops::number_to_string(kept));
                        if let Err(abrupt) = create_data_property_or_throw(interpreter, target, at, element)
                        {
                            return abrupt;
                        }
                        kept += 1.0;
                    }
                }
                IterationKind::Every => {
                    if !ops::to_boolean(&result) {
                        return Completion::Normal(JsValue::Boolean(false));
                    }
                }
                IterationKind::Some => {
                    if ops::to_boolean(&result) {
                        return Completion::Normal(JsValue::Boolean(true));
                    }
                }
                IterationKind::Find => {
                    if ops::to_boolean(&result) {
                        return Completion::Normal(element);
                    }
                }
                IterationKind::FindIndex => {
                    if ops::to_boolean(&result) {
                        return Completion::Normal(JsValue::Number(index));
                    }
                }
            }
        }
        index += 1.0;
    }
    match kind {
        IterationKind::ForEach => Completion::Normal(JsValue::Undefined),
        IterationKind::Map => {
            Completion::Normal(JsValue::Object(mapped.expect("map allocated its result above")))
        }
        IterationKind::Filter => {
            Completion::Normal(JsValue::Object(mapped.expect("filter allocated its result above")))
        }
        IterationKind::Every => Completion::Normal(JsValue::Boolean(true)),
        IterationKind::Some => Completion::Normal(JsValue::Boolean(false)),
        IterationKind::Find => Completion::Normal(JsValue::Undefined),
        IterationKind::FindIndex => Completion::Normal(JsValue::Number(-1.0)),
    }
}

/// Creates an array of a given length, REFUSING a length no array may take.
///
/// The maximum is 2^32 - 1 and it is a RangeError to ask for more -- not a clamp, and not a slow
/// allocation. `ToLength` admits up to 2^53 - 1, so the two limits differ and the gap between them
/// is exactly where an unbounded walk hides.
fn new_array_of_length(
    interpreter: &mut Interpreter,
    length: f64,
) -> Result<crate::value::ObjectId, Completion> {
    if length > 4_294_967_295.0 {
        return Err(interpreter.throw("RangeError", "invalid array length"));
    }
    let id = interpreter.new_array(Vec::new());
    set_length(interpreter, &JsValue::Object(id), length)?;
    Ok(id)
}

/// `Object.prototype.__defineGetter__` and `__defineSetter__`, which differ by one field.
fn define_accessor_half(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    setter: bool,
) -> Completion {
    let id = match interpreter.to_object(&this) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let accessor = arg(arguments, 1);
    if !interpreter.is_callable(&accessor) {
        return interpreter.type_error("that accessor is not a function");
    }
    let JsValue::Object(accessor) = accessor else {
        return interpreter.type_error("that accessor is not a function");
    };
    let key = match interpreter.to_property_key_value(&arg(arguments, 0)) {
        Ok(key) => key,
        Err(abrupt) => return abrupt,
    };
    let mut descriptor = RawDescriptor::empty();
    descriptor.present.enumerable = true;
    descriptor.enumerable = true;
    descriptor.present.configurable = true;
    descriptor.configurable = true;
    if setter {
        descriptor.present.set = true;
        descriptor.set = Some(accessor);
    } else {
        descriptor.present.get = true;
        descriptor.get = Some(accessor);
    }
    match define_property_or_throw(interpreter, id, key, &descriptor) {
        Completion::Normal(_) => Completion::Normal(JsValue::Undefined),
        abrupt => abrupt,
    }
}

/// `Object.prototype.__lookupGetter__` and `__lookupSetter__`.
///
/// IT WALKS THE CHAIN ITSELF RATHER THAN READING THE PROPERTY, because a getter found by an
/// ordinary read would have been CALLED. That is the whole point of the method, and it is why the
/// loop is here rather than a call to `get_property`.
fn lookup_accessor_half(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    setter: bool,
) -> Completion {
    let mut current = match interpreter.to_object(&this) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let key = match interpreter.to_property_key_value(&arg(arguments, 0)) {
        Ok(key) => key,
        Err(abrupt) => return abrupt,
    };
    loop {
        match ok_or_abrupt!(interpreter.own_property(current, &key)) {
            Some(Property { kind: PropertyKind::Accessor { get, set }, .. }) => {
                let found = if setter { set } else { get };
                return Completion::Normal(found.map_or(JsValue::Undefined, JsValue::Object));
            }
            Some(_) => return Completion::Normal(JsValue::Undefined),
            None => {}
        }
        match ok_or_abrupt!(interpreter.get_prototype_of(current)) {
            Some(next) => current = next,
            None => return Completion::Normal(JsValue::Undefined),
        }
    }
}

/// `FlattenIntoArray`, and the `ArraySpeciesCreate` in front of it that `flat` and `flatMap` share.
///
/// # AN EXPLICIT STACK, BECAUSE THE RECURSION DEPTH IS THE PROGRAM'S TO CHOOSE
///
/// The standard writes this recursively and the depth is `flat`'s argument, which may be `Infinity`
/// over an array a loop nested ten thousand deep. Host recursion there is a stack overflow -- an
/// ABORT, not a JavaScript error, and on a device with a two-kilobyte stack it is not even
/// theoretical. The frames are a `Vec` for that reason; the traversal order is identical.
///
/// THE MAPPER RUNS ON THE OUTERMOST LEVEL ONLY. It is passed into the first frame and not the
/// nested ones, which is what makes `flatMap` "map then flatten once" rather than "map everything".
fn flatten(
    interpreter: &mut Interpreter,
    source: &JsValue,
    depth: f64,
    mapper: Option<&JsValue>,
    this_arg: &JsValue,
) -> Completion {
    let source_length = match array_length(interpreter, source) {
        Ok(length) => length,
        Err(abrupt) => return abrupt,
    };
    let target = match array_species_create(interpreter, source, 0.0) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };

    /// One level of the walk: what is being read, how far it has got, and how much deeper it may go.
    struct Frame {
        source: JsValue,
        length: f64,
        index: f64,
        depth: f64,
    }

    let mut frames =
        crate::vec![Frame { source: source.clone(), length: source_length, index: 0.0, depth }];
    let mut target_index = 0.0f64;
    while let Some(frame) = frames.last_mut() {
        if frame.index >= frame.length {
            frames.pop();
            continue;
        }
        let index = frame.index;
        frame.index += 1.0;
        let (element_source, remaining) = (frame.source.clone(), frame.depth);
        let outermost = frames.len() == 1;
        let key = PropertyKey::from_str(&ops::number_to_string(index));
        if !ok_or_abrupt!(has_index(interpreter, &element_source, &key)) {
            continue;
        }
        let element = match interpreter.get_member(&element_source, &key) {
            Completion::Normal(element) => element,
            abrupt => return abrupt,
        };
        let element = match mapper {
            Some(mapper) if outermost => match interpreter.call_value(
                mapper,
                this_arg.clone(),
                crate::vec![element, JsValue::Number(index), element_source.clone()],
            ) {
                Completion::Normal(mapped) => mapped,
                abrupt => return abrupt,
            },
            _ => element,
        };
        let spreads = remaining > 0.0
            && match &element {
                JsValue::Object(id) => ok_or_abrupt!(interpreter.is_array(*id)),
                _ => false,
            };
        if spreads {
            let length = match array_length(interpreter, &element) {
                Ok(length) => length,
                Err(abrupt) => return abrupt,
            };
            frames.push(Frame { source: element, length, index: 0.0, depth: remaining - 1.0 });
            continue;
        }
        if target_index >= 9_007_199_254_740_991.0 {
            return interpreter.type_error("the flattened array would be longer than an array may be");
        }
        let key = PropertyKey::from_str(&ops::number_to_string(target_index));
        if let Err(abrupt) = create_data_property_or_throw(interpreter, target, key, element) {
            return abrupt;
        }
        target_index += 1.0;
    }
    Completion::Normal(JsValue::Object(target))
}

/// `ArraySpeciesCreate`: the array-shaped result an array method builds, which for a SUBCLASS is an
/// instance of that subclass.
///
/// **IT IS NOT `SpeciesConstructor` WITH `%Array%` AS THE DEFAULT, THOUGH IT READS LIKE IT.**
/// Two steps differ and both are observable: a non-array receiver skips the whole protocol and gets
/// a plain array -- `Array.prototype.map.call({length: 1}, f)` never reads `constructor` -- and a
/// `constructor` that is a non-object PRIMITIVE falls through to the IsConstructor test rather than
/// being rejected on the spot. Sharing one helper between the two would get one of them wrong.
///
/// The cross-realm step is the one thing left out, and it is left out because THERE IS ONE REALM:
/// `$262.createRealm` is not in this profile, so `GetFunctionRealm` can never differ from the
/// current one and the branch it guards is unreachable rather than skipped.
fn array_species_create(
    interpreter: &mut Interpreter,
    original: &JsValue,
    length: f64,
) -> Result<crate::value::ObjectId, Completion> {
    let JsValue::Object(source) = original else {
        return new_array_of_length(interpreter, length);
    };
    if !interpreter.object(*source).is_array {
        return new_array_of_length(interpreter, length);
    }
    let constructor = match interpreter.get_member(original, &PropertyKey::from_str("constructor")) {
        Completion::Normal(value) => value,
        abrupt => return Err(abrupt),
    };
    let constructor = match constructor {
        JsValue::Object(id) => {
            let key = PropertyKey::Symbol(interpreter.well_known_symbol("species"));
            match interpreter.get_property(id, &key) {
                Completion::Normal(JsValue::Null) => JsValue::Undefined,
                Completion::Normal(value) => value,
                abrupt => return Err(abrupt),
            }
        }
        other => other,
    };
    match constructor {
        JsValue::Undefined => new_array_of_length(interpreter, length),
        JsValue::Object(id) if interpreter.is_constructor(id) => {
            match interpreter.construct(id, vec![JsValue::Number(length)]) {
                Completion::Normal(JsValue::Object(id)) => Ok(id),
                Completion::Normal(_) => {
                    Err(interpreter.type_error("`Symbol.species` did not produce an object"))
                }
                abrupt => Err(abrupt),
            }
        }
        _ => Err(interpreter.type_error("`Symbol.species` is not a constructor")),
    }
}

/// `CreateDataPropertyOrThrow`: define an own data property with every attribute true.
///
/// IT IS NOT AN ASSIGNMENT, AND THE DIFFERENCE SHOWS THE MOMENT THE TARGET IS NOT A PLAIN ARRAY.
/// `Array.prototype.filter` on a subclass whose species is an object with a SETTER at index 0 must
/// define the property, not call the setter -- and must throw when the definition is refused, where
/// a sloppy-mode assignment would silently do nothing.
/// The write itself DELEGATES rather than reaching for `set_own`: an Array target keeps `length`
/// in step through its exotic path, and a second definition site here would be the one that forgot.
///
/// # AND THE *REFUSAL* DELEGATES TOO, BECAUSE THE HAND-WRITTEN ONE WAS NARROWER THAN THE RULE
///
/// It read "refused if the existing property is neither configurable NOR writable", which is a
/// summary of `ValidateAndApplyPropertyDescriptor` rather than the thing itself. `CreateDataProperty`
/// asks for `configurable: true`, and a NON-configurable property refuses that whatever its
/// writability -- so a target carrying `{configurable: false, writable: true, enumerable: true}` at
/// index 0 let `Array.of` and `Array.from` write straight through it and report success. An existing
/// non-configurable ACCESSOR was missed the same way.
///
/// `Object.defineProperty` had the rule right all along, in `define_own_property`. Two spellings of
/// one question, and the second one answered it approximately -- so this now asks the first.
pub(crate) fn create_data_property_or_throw(
    interpreter: &mut Interpreter,
    target: crate::value::ObjectId,
    key: PropertyKey,
    value: JsValue,
) -> Result<(), Completion> {
    define_complete_or_throw(interpreter, target, key, &Property::data(value))
}

/// `DefinePropertyOrThrow` with a COMPLETE descriptor: every field supplied, exactly as the
/// [`Property`] describes it.
///
/// The callers are the operations that build a whole property and then have to hand it to
/// `[[DefineOwnProperty]]` rather than to the store -- `CreateDataPropertyOrThrow` and
/// `DefineMethodProperty`. Both need the refusal, and neither should own a copy of the rule.
pub(crate) fn define_complete_or_throw(
    interpreter: &mut Interpreter,
    target: crate::value::ObjectId,
    key: PropertyKey,
    property: &Property,
) -> Result<(), Completion> {
    let mut descriptor = RawDescriptor::empty();
    descriptor.enumerable = property.enumerable;
    descriptor.configurable = property.configurable;
    descriptor.present.enumerable = true;
    descriptor.present.configurable = true;
    match &property.kind {
        PropertyKind::Data { value, writable } => {
            descriptor.value = value.clone();
            descriptor.writable = *writable;
            descriptor.present.value = true;
            descriptor.present.writable = true;
        }
        PropertyKind::Accessor { get, set } => {
            descriptor.get = *get;
            descriptor.set = *set;
            descriptor.present.get = true;
            descriptor.present.set = true;
        }
    }
    match define_own_property(interpreter, target, key, &descriptor) {
        Ok(None) => Ok(()),
        Ok(Some(why)) => Err(interpreter.type_error(&why)),
        Err(abrupt) => Err(abrupt),
    }
}

/// `FromPropertyDescriptor`: the plain object a descriptor is reported as.
///
/// THE DESCRIPTOR HAS ONE SHAPE OR THE OTHER, NEVER BOTH. A data descriptor carries
/// `value`/`writable`; an accessor carries `get`/`set`. `propertyHelper.js` reads the field list
/// back with `Object.getOwnPropertyNames(desc)` and asserts every name is one of the six, so
/// emitting all six would fail its own validity check.
pub(crate) fn describe_property(
    interpreter: &mut Interpreter,
    property: &Property,
) -> crate::value::ObjectId {
    let descriptor =
        interpreter.allocate(Object::new(Some(interpreter.intrinsics.object_prototype)));
    let fields: Vec<(&str, JsValue)> = match &property.kind {
        PropertyKind::Data { value, writable } => {
            vec![("value", value.clone()), ("writable", JsValue::Boolean(*writable))]
        }
        PropertyKind::Accessor { get, set } => vec![
            ("get", get.map_or(JsValue::Undefined, JsValue::Object)),
            ("set", set.map_or(JsValue::Undefined, JsValue::Object)),
        ],
    };
    for (name, value) in fields.into_iter().chain([
        ("enumerable", JsValue::Boolean(property.enumerable)),
        ("configurable", JsValue::Boolean(property.configurable)),
    ]) {
        interpreter.object_mut(descriptor).set_own(PropertyKey::from_str(name), Property::data(value));
    }
    descriptor
}

/// `get Array [ @@species ]`, installed from [`install`] once the well-known symbols exist.
fn array_species(interpreter: &mut Interpreter) {
    let constructor = interpreter.intrinsics.array_constructor;
    interpreter.define_species_getter(constructor);
}

/// The names an ARRAY hides from a `with` statement.
///
/// # IT IS WHAT KEEPS `with (array) { values }` FROM MEANING SOMETHING NEW
///
/// `@@unscopables` exists for one reason: `Array.prototype` grew methods after `with` had been in
/// the language for fifteen years, and each new method silently captured a name that had been
/// resolving to an enclosing binding. `var values = 1; with ([]) { values }` answered 1 for a
/// decade and would answer `function values()` the day the iterator methods landed. The block is
/// the fix, and **an engine that ships the methods without it recreates exactly the breakage the
/// block was invented to prevent** -- which is what this engine did until a differential probe put
/// the two side by side: node answered `outer` and we answered the function.
///
/// THE LIST IS THE STANDARD'S, VERBATIM, AND IT IS A PUBLISHED LIST RATHER THAN A DERIVED ONE --
/// so it does not silently change shape when a method arrives, and it may legitimately name one
/// this profile has not implemented. A name the prototype does not have never reaches the block
/// anyway: `HasProperty` answers false first.
///
/// The block's own prototype is `null`. A block inheriting from `Object.prototype` would make
/// `with (array) { toString }` resolve outward, because `blocked` would read a truthy inherited
/// `toString` off the block and veto a name the standard does not veto.
fn array_unscopables(interpreter: &mut Interpreter) {
    let block = interpreter.allocate(Object::new(None));
    for name in [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ] {
        interpreter
            .object_mut(block)
            .set_own(PropertyKey::from_str(name), Property::data(JsValue::Boolean(true)));
    }
    let prototype = interpreter.intrinsics.array_prototype;
    let key = PropertyKey::Symbol(interpreter.well_known_symbol("unscopables"));
    interpreter.object_mut(prototype).set_own(
        key,
        Property {
            kind: PropertyKind::Data { value: JsValue::Object(block), writable: false },
            enumerable: false,
            configurable: true,
        },
    );
}

fn array_constructor(interpreter: &mut Interpreter, arguments: &[JsValue]) -> Completion {
    if arguments.len() == 1 {
        if let JsValue::Number(length) = arguments[0] {
            if length < 0.0 || crate::math::fract(length) != 0.0 || length > 4_294_967_295.0 {
                return interpreter.throw("RangeError", "invalid array length");
            }
            let id = interpreter.new_array(Vec::new());
            if let Err(abrupt) = set_length(interpreter, &JsValue::Object(id), length) {
                return abrupt;
            }
            return Completion::Normal(JsValue::Object(id));
        }
    }
    let id = interpreter.new_array(arguments.to_vec());
    Completion::Normal(JsValue::Object(id))
}

/// `LengthOfArrayLike`. ONE definition, shared with the iterator protocol: an array iterator that
/// computed its own length would be a second answer to "how long is this?", and the two would
/// disagree the first time either learned something about coercion.
pub(crate) fn array_length(interpreter: &mut Interpreter, value: &JsValue) -> Result<f64, Completion> {
    let length = match interpreter.get_member(value, &PropertyKey::from_str("length")) {
        Completion::Normal(length) => length,
        abrupt => return Err(abrupt),
    };
    Ok(to_length(interpreter.to_number_value(&length)?))
}

/// `DeletePropertyOrThrow` for a receiver [`receiver`] has already converted.
fn delete_index(
    interpreter: &mut Interpreter,
    target: &JsValue,
    key: &PropertyKey,
) -> Result<(), Completion> {
    match target {
        JsValue::Object(id) => match interpreter.delete_property_or_throw(*id, key) {
            Completion::Normal(_) => Ok(()),
            abrupt => Err(abrupt),
        },
        _ => Err(interpreter.type_error("a built-in delete needs an object receiver")),
    }
}

fn set_length(
    interpreter: &mut Interpreter,
    target: &JsValue,
    length: f64,
) -> Result<(), Completion> {
    set_element(interpreter, target, PropertyKey::from_str("length"), JsValue::Number(length))
}

fn set_element(
    interpreter: &mut Interpreter,
    target: &JsValue,
    key: PropertyKey,
    value: JsValue,
) -> Result<(), Completion> {
    match target {
        JsValue::Object(id) => match interpreter.set_property_or_throw(*id, key, value) {
            Completion::Normal(_) => Ok(()),
            abrupt => Err(abrupt),
        },
        JsValue::Undefined | JsValue::Null => {
            Err(interpreter.type_error("cannot set a property of null or undefined"))
        }
        _ => Err(interpreter.type_error("a built-in write needs an object receiver")),
    }
}


fn install_string(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.object_mut(prototype).primitive = Some(JsValue::string(""));
    interpreter.object_mut(prototype).set_own(
        PropertyKey::from_str("length"),
        Property {
            kind: PropertyKind::Data { value: JsValue::Number(0.0), writable: false },
            enumerable: false,
            configurable: false,
        },
    );
    interpreter.intrinsics.string_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "String",
        1,
        |interpreter, _this, arguments| {
            if arguments.is_empty() {
                return Completion::Normal(JsValue::string(""));
            }
            if let JsValue::Symbol(id) = &arguments[0] {
                let text = interpreter.symbol_to_display(*id);
                return Completion::Normal(JsValue::string(&text));
            }
            match interpreter.to_string_value(&arguments[0]) {
                Ok(text) => Completion::Normal(JsValue::String(text)),
                Err(abrupt) => abrupt,
            }
        },
        |interpreter, _this, arguments| {
            let text = if arguments.is_empty() {
                JsString::new()
            } else {
                match interpreter.to_string_value(&arguments[0]) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                }
            };
            match interpreter.to_object(&JsValue::String(text)) {
                Ok(id) => Completion::Normal(JsValue::Object(id)),
                Err(abrupt) => abrupt,
            }
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("String", JsValue::Object(constructor));

    interpreter.define_method(constructor, "fromCharCode", 1, |interpreter, _this, arguments| {
        let mut out = JsString::new();
        for argument in arguments {
            match interpreter.to_number_value(argument) {
                Ok(number) => out.push_code_unit(ops::to_uint32(number) as u16),
                Err(abrupt) => return abrupt,
            }
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(constructor, "fromCodePoint", 1, |interpreter, _this, arguments| {
        let mut out = JsString::new();
        for argument in arguments {
            let number = match interpreter.to_number_value(argument) {
                Ok(number) => number,
                Err(abrupt) => return abrupt,
            };
            if number != to_integer(number) || !(0.0..0x11_0000 as f64).contains(&number) {
                return interpreter.throw("RangeError", "not a valid code point");
            }
            let point = number as u32;
            if point <= 0xFFFF {
                out.push_code_unit(point as u16);
            } else {
                let adjusted = point - 0x10000;
                out.push_code_unit(0xD800 + (adjusted >> 10) as u16);
                out.push_code_unit(0xDC00 + (adjusted & 0x3FF) as u16);
            }
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(constructor, "raw", 1, |interpreter, _this, arguments| {
        let raw = match interpreter.get_member(&arg(arguments, 0), &PropertyKey::from_str("raw")) {
            Completion::Normal(raw) => raw,
            abrupt => return abrupt,
        };
        let raw = match interpreter.to_object(&raw) {
            Ok(id) => JsValue::Object(id),
            Err(abrupt) => return abrupt,
        };
        let length = match array_length(interpreter, &raw) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        let mut out = JsString::new();
        let mut index = 0.0f64;
        while index < length {
            let key = PropertyKey::from_str(&ops::number_to_string(index));
            let part = match interpreter.get_member(&raw, &key) {
                Completion::Normal(part) => part,
                abrupt => return abrupt,
            };
            match interpreter.to_string_value(&part) {
                Ok(text) => out.extend_from(&text),
                Err(abrupt) => return abrupt,
            }
            index += 1.0;
            if index >= length {
                break;
            }
            if let Some(substitution) = arguments.get(index as usize) {
                match interpreter.to_string_value(substitution) {
                    Ok(text) => out.extend_from(&text),
                    Err(abrupt) => return abrupt,
                }
            }
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        this_string_value(interpreter, &this)
    });
    interpreter.define_method(prototype, "valueOf", 0, |interpreter, this, _| {
        this_string_value(interpreter, &this)
    });

    interpreter.define_method(prototype, "charAt", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let index = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        if index < 0.0 || index >= text.len() as f64 {
            return Completion::Normal(JsValue::string(""));
        }
        let unit = text.unit_at(index as usize).unwrap_or(0);
        Completion::Normal(JsValue::String(JsString::from_units(&[unit])))
    });

    interpreter.define_method(prototype, "charCodeAt", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let index = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        if index < 0.0 || index >= text.len() as f64 {
            return Completion::Normal(JsValue::Number(f64::NAN));
        }
        let unit = text.unit_at(index as usize).unwrap_or(0);
        Completion::Normal(JsValue::Number(f64::from(unit)))
    });

    interpreter.define_method(prototype, "codePointAt", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let index = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        if index < 0.0 || index >= text.len() as f64 {
            return Completion::Normal(JsValue::Undefined);
        }
        let units = text.units();
        let index = index as usize;
        let first = units[index];
        let combined = if (0xD800..0xDC00).contains(&first) && index + 1 < units.len() {
            let second = units[index + 1];
            if (0xDC00..0xE000).contains(&second) {
                Some(0x10000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00))
            } else {
                None
            }
        } else {
            None
        };
        Completion::Normal(JsValue::Number(f64::from(combined.unwrap_or(u32::from(first)))))
    });

    interpreter.define_method(prototype, "at", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let length = text.len() as f64;
        let relative = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        let index = if relative >= 0.0 { relative } else { length + relative };
        if index < 0.0 || index >= length {
            return Completion::Normal(JsValue::Undefined);
        }
        let unit = text.unit_at(index as usize).unwrap_or(0);
        Completion::Normal(JsValue::String(JsString::from_units(&[unit])))
    });

    for (index, name) in ["padStart", "padEnd"].iter().enumerate() {
        interpreter.define_method(prototype, name, 1, PAD_BODIES[index]);
    }

    for (index, name) in ["trimStart", "trimEnd"].iter().enumerate() {
        interpreter.define_method(prototype, name, 0, TRIM_BODIES[index]);
    }

    interpreter.define_method(prototype, "indexOf", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let search = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(search) => search,
            Err(abrupt) => return abrupt,
        };
        let position = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        let length = text.len() as f64;
        let start = to_integer(position).clamp(0.0, length) as usize;
        Completion::Normal(JsValue::Number(
            match find_units(text.units(), search.units(), start) {
                Some(index) => index as f64,
                None => -1.0,
            },
        ))
    });

    interpreter.define_method(prototype, "lastIndexOf", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let search = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(search) => search,
            Err(abrupt) => return abrupt,
        };
        let position = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        let position = if position.is_nan() { f64::INFINITY } else { to_integer(position) };
        let length = text.len() as f64;
        let last = length - search.len() as f64;
        let start = position.clamp(0.0, last.max(0.0));
        if search.units().is_empty() {
            return Completion::Normal(JsValue::Number(start));
        }
        let start = start as usize;
        let mut found = -1.0f64;
        let mut from = 0usize;
        while let Some(index) = find_units(text.units(), search.units(), from) {
            if index > start {
                break;
            }
            found = index as f64;
            from = index + 1;
        }
        Completion::Normal(JsValue::Number(found))
    });

    interpreter.define_method(prototype, "includes", 1, |interpreter, this, arguments| {
        if is_regexp(interpreter, &arg(arguments, 0)) {
            return interpreter.type_error("this String method does not take a regular expression");
        }
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let search = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(search) => search,
            Err(abrupt) => return abrupt,
        };
        let start = match arguments.get(1) {
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => clamp_index(to_integer(number), text.len() as f64),
                Err(abrupt) => return abrupt,
            },
            None => 0.0,
        };
        let found = find_units(text.units(), search.units(), start as usize).is_some();
        Completion::Normal(JsValue::Boolean(found))
    });

    interpreter.define_method(prototype, "slice", 2, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let length = text.len() as f64;
        let start = match arg(arguments, 0) {
            JsValue::Undefined => 0.0,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => relative_index(to_integer(number), length),
                Err(abrupt) => return abrupt,
            },
        };
        let end = match arg(arguments, 1) {
            JsValue::Undefined => length,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => relative_index(to_integer(number), length),
                Err(abrupt) => return abrupt,
            },
        };
        let units = text.units();
        let slice = if start < end {
            &units[start as usize..end as usize]
        } else {
            &[][..]
        };
        Completion::Normal(JsValue::String(JsString::from_units(slice)))
    });

    interpreter.define_method(prototype, "substring", 2, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let length = text.len() as f64;
        let clamp = |value: f64| value.clamp(0.0, length);
        let start = match arg(arguments, 0) {
            JsValue::Undefined => 0.0,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => clamp(to_integer(number)),
                Err(abrupt) => return abrupt,
            },
        };
        let end = match arg(arguments, 1) {
            JsValue::Undefined => length,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => clamp(to_integer(number)),
                Err(abrupt) => return abrupt,
            },
        };
        let (from, to) = if start <= end { (start, end) } else { (end, start) };
        let units = &text.units()[from as usize..to as usize];
        Completion::Normal(JsValue::String(JsString::from_units(units)))
    });

    interpreter.define_method(prototype, "substr", 2, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let size = text.len() as f64;
        let start = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => relative_index(to_integer(number), size),
            Err(abrupt) => return abrupt,
        };
        let count = match arg(arguments, 1) {
            JsValue::Undefined => size,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => to_integer(number).clamp(0.0, size),
                Err(abrupt) => return abrupt,
            },
        };
        let end = (start + count).min(size);
        if start >= end {
            return Completion::Normal(JsValue::String(JsString::from_units(&[])));
        }
        let units = &text.units()[start as usize..end as usize];
        Completion::Normal(JsValue::String(JsString::from_units(units)))
    });

    interpreter.define_method(prototype, "concat", 1, |interpreter, this, arguments| {
        let mut text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        for argument in arguments {
            match interpreter.to_string_value(argument) {
                Ok(other) => text.extend_from(&other),
                Err(abrupt) => return abrupt,
            }
        }
        Completion::Normal(JsValue::String(text))
    });

    interpreter.define_method(prototype, "trim", 0, |interpreter, this, _| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let units = text.units();
        let start = units.iter().position(|unit| !is_trimmable(*unit)).unwrap_or(units.len());
        let end = units.iter().rposition(|unit| !is_trimmable(*unit)).map_or(start, |i| i + 1);
        Completion::Normal(JsValue::String(JsString::from_units(&units[start..end])))
    });

    for (name, upper) in [("toUpperCase", true), ("toLowerCase", false)] {
        let call: crate::interpreter::NativeFn = if upper {
            |interpreter, this, _| change_case(interpreter, &this, true)
        } else {
            |interpreter, this, _| change_case(interpreter, &this, false)
        };
        interpreter.define_method(prototype, name, 0, call);
    }

    for (index, name) in ["startsWith", "endsWith"].iter().enumerate() {
        interpreter.define_method(prototype, name, 1, AFFIX_TESTS[index]);
    }

    interpreter.define_method(prototype, "repeat", 1, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let count = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => to_integer(number),
            Err(abrupt) => return abrupt,
        };
        if count < 0.0 || count.is_infinite() {
            return interpreter.throw("RangeError", "the repeat count must be finite and positive");
        }
        if count * text.len() as f64 > 4_294_967_295.0 {
            return interpreter.throw("RangeError", "the repeated string is too long");
        }
        let mut out = JsString::new();
        for _ in 0..count as u64 {
            out.extend_from(&text);
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(prototype, "replace", 2, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let pattern = arg(arguments, 0);
        if let Some(completion) = dispatch_replace(interpreter, &this, &pattern, arguments) {
            return completion;
        }
        let pattern = match interpreter.to_string_value(&pattern) {
            Ok(pattern) => pattern,
            Err(abrupt) => return abrupt,
        };
        let units = text.units().to_vec();
        let replacement = arg(arguments, 1);
        let functional = interpreter.is_callable(&replacement);
        let literal = if functional {
            None
        } else {
            match interpreter.to_string_value(&replacement) {
                Ok(text) => Some(text),
                Err(abrupt) => return abrupt,
            }
        };
        let Some(at) = find_units(&units, pattern.units(), 0) else {
            return Completion::Normal(JsValue::String(text));
        };
        let replacement = match literal {
            Some(literal) => match crate::regexp::get_substitution(
                interpreter,
                &pattern,
                &text,
                at,
                &[],
                &JsValue::Undefined,
                &literal,
            ) {
                Ok(text) => text,
                Err(abrupt) => return abrupt,
            },
            None => {
                let matched = JsValue::String(JsString::from_units(pattern.units()));
                let result = interpreter.call_value(
                    &replacement,
                    JsValue::Undefined,
                    vec![matched, JsValue::Number(at as f64), JsValue::String(text.clone())],
                );
                match result {
                    Completion::Normal(value) => match interpreter.to_string_value(&value) {
                        Ok(text) => text,
                        Err(abrupt) => return abrupt,
                    },
                    abrupt => return abrupt,
                }
            }
        };
        let mut out = JsString::from_units(&units[..at]);
        out.extend_from(&replacement);
        out.extend_from(&JsString::from_units(&units[at + pattern.len()..]));
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(prototype, "replaceAll", 2, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let pattern = arg(arguments, 0);
        if is_regexp(interpreter, &pattern) {
            let JsValue::Object(id) = &pattern else {
                unreachable!("is_regexp implies an object")
            };
            let flags = match interpreter.get_property(*id, &PropertyKey::from_str("flags")) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            if matches!(flags, JsValue::Undefined | JsValue::Null) {
                return interpreter.type_error("replaceAll needs a pattern with readable flags");
            }
            let flags = match interpreter.to_string_value(&flags) {
                Ok(flags) => flags,
                Err(abrupt) => return abrupt,
            };
            if !flags.to_lossy_string().contains('g') {
                return interpreter.type_error("replaceAll requires a global regular expression");
            }
        }
        if let Some(completion) = dispatch_replace(interpreter, &this, &pattern, arguments) {
            return completion;
        }
        let pattern = match interpreter.to_string_value(&pattern) {
            Ok(pattern) => pattern,
            Err(abrupt) => return abrupt,
        };
        let units = text.units().to_vec();
        let replacement = arg(arguments, 1);
        let functional = interpreter.is_callable(&replacement);
        let literal = if functional {
            None
        } else {
            match interpreter.to_string_value(&replacement) {
                Ok(text) => Some(text),
                Err(abrupt) => return abrupt,
            }
        };
        let mut out = JsString::new();
        let mut at = 0usize;
        while at <= units.len() {
            let Some(found) = find_units(&units, pattern.units(), at) else { break };
            out.extend_from(&JsString::from_units(&units[at..found]));
            let piece = match &literal {
                Some(literal) => match crate::regexp::get_substitution(
                    interpreter,
                    &pattern,
                    &text,
                    found,
                    &[],
                    &JsValue::Undefined,
                    literal,
                ) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                },
                None => {
                    let matched = JsValue::String(JsString::from_units(pattern.units()));
                    let result = interpreter.call_value(
                        &replacement,
                        JsValue::Undefined,
                        vec![matched, JsValue::Number(found as f64), JsValue::String(text.clone())],
                    );
                    match result {
                        Completion::Normal(value) => match interpreter.to_string_value(&value) {
                            Ok(text) => text,
                            Err(abrupt) => return abrupt,
                        },
                        abrupt => return abrupt,
                    }
                }
            };
            out.extend_from(&piece);
            if pattern.is_empty() {
                if found < units.len() {
                    out.push_code_unit(units[found]);
                }
                at = found + 1;
            } else {
                at = found + pattern.len();
            }
        }
        if at <= units.len() {
            out.extend_from(&JsString::from_units(&units[at..]));
        }
        Completion::Normal(JsValue::String(out))
    });

    interpreter.define_method(prototype, "match", 1, |interpreter, this, arguments| {
        dispatch_through_symbol(interpreter, this, arguments, "match", "")
    });
    interpreter.define_method(prototype, "search", 1, |interpreter, this, arguments| {
        dispatch_through_symbol(interpreter, this, arguments, "search", "")
    });

    interpreter.define_method(prototype, "matchAll", 1, |interpreter, this, arguments| {
        if matches!(this, JsValue::Undefined | JsValue::Null) {
            return interpreter.type_error("a String method requires a coercible receiver");
        }
        let pattern = arg(arguments, 0);
        if is_regexp(interpreter, &pattern) {
            let JsValue::Object(id) = &pattern else {
                unreachable!("is_regexp implies an object")
            };
            let flags = match interpreter.get_property(*id, &PropertyKey::from_str("flags")) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            if matches!(flags, JsValue::Undefined | JsValue::Null) {
                return interpreter.type_error("matchAll needs a pattern with readable flags");
            }
            let flags = match interpreter.to_string_value(&flags) {
                Ok(flags) => flags,
                Err(abrupt) => return abrupt,
            };
            if !flags.to_lossy_string().contains('g') {
                return interpreter.type_error("matchAll requires a global regular expression");
            }
        }
        dispatch_through_symbol(interpreter, this, arguments, "matchAll", "g")
    });

    interpreter.define_method(prototype, "normalize", 0, |interpreter, this, arguments| {
        if let Err(abrupt) = coerce_to_string(interpreter, &this) {
            return abrupt;
        }
        let form = match arg(arguments, 0) {
            JsValue::Undefined => JsString::from("NFC"),
            value => match interpreter.to_string_value(&value) {
                Ok(form) => form,
                Err(abrupt) => return abrupt,
            },
        };
        let form = form.to_lossy_string();
        if !matches!(form.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
            return interpreter
                .throw("RangeError", "normalize form must be NFC, NFD, NFKC or NFKD");
        }
        interpreter.refuse(Absence::StringNormalize)
    });

    interpreter.define_method(prototype, "localeCompare", 1, |interpreter, this, arguments| {
        let left = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let right = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let order = left.units().cmp(right.units());
        Completion::Normal(JsValue::Number(match order {
            core::cmp::Ordering::Less => -1.0,
            core::cmp::Ordering::Equal => 0.0,
            core::cmp::Ordering::Greater => 1.0,
        }))
    });

    for (name, upper) in [("toLocaleUpperCase", true), ("toLocaleLowerCase", false)] {
        let call: crate::interpreter::NativeFn = if upper {
            |interpreter, this, _| change_case(interpreter, &this, true)
        } else {
            |interpreter, this, _| change_case(interpreter, &this, false)
        };
        interpreter.define_method(prototype, name, 0, call);
    }

    interpreter.define_method(prototype, "split", 2, |interpreter, this, arguments| {
        let text = match coerce_to_string(interpreter, &this) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let separator = arg(arguments, 0);
        if let JsValue::Object(id) = &separator {
            let key = PropertyKey::Symbol(interpreter.well_known_symbol("split"));
            let method = match interpreter.get_property(*id, &key) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            if !matches!(method, JsValue::Undefined | JsValue::Null) {
                if !interpreter.is_callable(&method) {
                    return interpreter.type_error("a separator's Symbol.split is not callable");
                }
                return interpreter.call_value(
                    &method,
                    separator.clone(),
                    vec![this.clone(), arg(arguments, 1)],
                );
            }
        }
        let limit = match arguments.get(1) {
            Some(JsValue::Undefined) | None => u32::MAX as usize,
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => crate::binary::wrap_to_u32(number) as usize,
                Err(abrupt) => return abrupt,
            },
        };
        let separator_missing = matches!(separator, JsValue::Undefined);
        let separator = match interpreter.to_string_value(&separator) {
            Ok(separator) => separator,
            Err(abrupt) => return abrupt,
        };
        if limit == 0 {
            let id = interpreter.new_array(Vec::new());
            return Completion::Normal(JsValue::Object(id));
        }
        if separator_missing {
            let id = interpreter.new_array(vec![JsValue::String(text)]);
            return Completion::Normal(JsValue::Object(id));
        }
        let units = text.units().to_vec();
        let mut parts: Vec<JsValue> = Vec::new();
        if separator.is_empty() {
            for unit in &units {
                if parts.len() == limit {
                    break;
                }
                parts.push(JsValue::String(JsString::from_units(&[*unit])));
            }
        } else {
            let mut start = 0usize;
            while let Some(index) = find_units(&units, separator.units(), start) {
                if parts.len() == limit {
                    break;
                }
                parts.push(JsValue::String(JsString::from_units(&units[start..index])));
                start = index + separator.len();
            }
            if parts.len() < limit {
                parts.push(JsValue::String(JsString::from_units(&units[start..])));
            }
        }
        let id = interpreter.new_array(parts);
        Completion::Normal(JsValue::Object(id))
    });
}

/// `startsWith` and `endsWith`, which differ only in where they anchor.
///
/// **THE STANDARD'S `IsRegExp` STEP IS PERFORMED.** Step 3 of each is `? IsRegExp(searchString)`
/// and step 4 throws a TypeError if it answered true, so `"a".startsWith(/a/)` cannot look like it
/// worked. It is a `?` step: `Get(argument, @@match)` runs a getter that may throw, so its position
/// between the receiver's coercion and the search's is observable.
const AFFIX_TESTS: [crate::interpreter::NativeFn; 2] = [
    |interpreter, this, arguments| affix(interpreter, this, arguments, true),
    |interpreter, this, arguments| affix(interpreter, this, arguments, false),
];

const PAD_BODIES: [crate::interpreter::NativeFn; 2] = [
    |interpreter, this, arguments| pad(interpreter, this, arguments, true),
    |interpreter, this, arguments| pad(interpreter, this, arguments, false),
];

const TRIM_BODIES: [crate::interpreter::NativeFn; 2] = [
    |interpreter, this, _| trim_side(interpreter, &this, true, false),
    |interpreter, this, _| trim_side(interpreter, &this, false, true),
];

/// `padStart` and `padEnd`.
///
/// THE FILLER IS REPEATED AND THEN **TRUNCATED**, not repeated a whole number of times.
/// `"x".padStart(5, "ab")` is `"ababx"` -- four filler units from a two-unit pattern. Rounding up to
/// whole repeats gives `"ababax"`, one unit too long, which is a length bug rather than a cosmetic
/// one.
///
/// AN EMPTY FILLER RETURNS THE STRING UNCHANGED, and the check must come before the loop: an
/// empty pattern repeated to reach a target length never reaches it.
fn pad(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    at_start: bool,
) -> Completion {
    let text = match coerce_to_string(interpreter, &this) {
        Ok(text) => text,
        Err(abrupt) => return abrupt,
    };
    let target = match interpreter.to_number_value(&arg(arguments, 0)) {
        Ok(number) => to_length(number),
        Err(abrupt) => return abrupt,
    };
    if target <= text.len() as f64 {
        return Completion::Normal(JsValue::String(text));
    }
    let filler = match arguments.get(1) {
        Some(JsValue::Undefined) | None => JsString::from(" "),
        Some(value) => match interpreter.to_string_value(value) {
            Ok(filler) => filler,
            Err(abrupt) => return abrupt,
        },
    };
    if filler.is_empty() {
        return Completion::Normal(JsValue::String(text));
    }
    if target > 4_294_967_295.0 {
        return interpreter.throw("RangeError", "the padded string is longer than any string may be");
    }
    let needed = target as usize - text.len();
    let mut padding = Vec::with_capacity(needed);
    while padding.len() < needed {
        let take = needed - padding.len();
        let units = filler.units();
        padding.extend_from_slice(&units[..take.min(units.len())]);
    }
    let mut out = JsString::new();
    if at_start {
        out.extend_from(&JsString::from_units(&padding));
        out.extend_from(&text);
    } else {
        out.extend_from(&text);
        out.extend_from(&JsString::from_units(&padding));
    }
    Completion::Normal(JsValue::String(out))
}

/// `trimStart` and `trimEnd`, which trim the same character set `trim` does from one side.
fn trim_side(
    interpreter: &mut Interpreter,
    this: &JsValue,
    from_start: bool,
    from_end: bool,
) -> Completion {
    let text = match coerce_to_string(interpreter, this) {
        Ok(text) => text,
        Err(abrupt) => return abrupt,
    };
    let units = text.units();
    let mut start = 0usize;
    let mut end = units.len();
    if from_start {
        while start < end && is_trimmable(units[start]) {
            start += 1;
        }
    }
    if from_end {
        while end > start && is_trimmable(units[end - 1]) {
            end -= 1;
        }
    }
    Completion::Normal(JsValue::String(JsString::from_units(&units[start..end])))
}

fn affix(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    at_start: bool,
) -> Completion {
    let text = match coerce_to_string(interpreter, &this) {
        Ok(text) => text,
        Err(abrupt) => return abrupt,
    };
    if is_regexp(interpreter, &arg(arguments, 0)) {
        return interpreter.type_error("this String method does not take a regular expression");
    }
    let search = match interpreter.to_string_value(&arg(arguments, 0)) {
        Ok(search) => search,
        Err(abrupt) => return abrupt,
    };
    let units = text.units();
    let needle = search.units();
    let length = units.len() as f64;
    let position = match arguments.get(1) {
        Some(JsValue::Undefined) | None => {
            if at_start {
                0.0
            } else {
                length
            }
        }
        Some(value) => match interpreter.to_number_value(value) {
            Ok(number) => clamp_index(to_integer(number), length),
            Err(abrupt) => return abrupt,
        },
    };
    let position = position as usize;
    let window = if at_start { &units[position..] } else { &units[..position] };
    if needle.len() > window.len() {
        return Completion::Normal(JsValue::Boolean(false));
    }
    let found = if at_start {
        &window[..needle.len()] == needle
    } else {
        &window[window.len() - needle.len()..] == needle
    };
    Completion::Normal(JsValue::Boolean(found))
}

fn is_trimmable(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2028 | 0x2029
            | 0x202F | 0x205F | 0x3000 | 0xFEFF
    ) || (0x2000..=0x200A).contains(&unit)
}

/// `toUpperCase`/`toLowerCase`, over CODE POINTS and the full Unicode case mappings.
///
/// # THIS WAS ASCII-ONLY, AND "PASSED THROUGH UNCHANGED" IS AN ANSWER RATHER THAN AN ABSENCE
///
/// `"Σ".toLowerCase()` was `"Σ"` and `"ß".toUpperCase()` was `"ß"` -- the input, returned as
/// though it were the result. A program cannot tell that apart from a character that genuinely has
/// no case, so the failure is invisible at the call site and shows up as a comparison that will
/// not match. It was recorded as a published limit in a source comment, which is not the same as
/// being on the published list.
///
/// # AND THE UNIT-BY-UNIT WALK WAS WRONG EVEN WHERE IT MAPPED SOMETHING
///
/// Case mapping is defined over CODE POINTS. A surrogate pair walked as two units is two
/// unpaired surrogates, neither of which has a case, so every character outside the BMP -- Deseret,
/// Warang Citi, Adlam -- was left alone whatever the tables said. And a mapping may CHANGE LENGTH:
/// `ß` uppercases to two characters and `İ` lowercases to two, which a one-unit-in-one-unit-out
/// loop cannot express at all.
///
/// **PUBLISHED DEVIATION, `final-sigma-is-unconditional`**: the one CONDITIONAL rule in
/// `SpecialCasing.txt` is not applied, so a word-final `Σ` lowercases to `σ` where the standard
/// gives `ς`. The condition is defined over the `Cased` and `Case_Ignorable` properties, which live
/// in the shared Unicode home this crate deliberately does not widen on its own -- the same
/// decision `non-ascii-identifiers` records. Every UNCONDITIONAL mapping is applied.
fn change_case(interpreter: &mut Interpreter, this: &JsValue, upper: bool) -> Completion {
    let text = match coerce_to_string(interpreter, this) {
        Ok(text) => text,
        Err(abrupt) => return abrupt,
    };
    let units = text.units();
    let mut mapped: Vec<u16> = Vec::with_capacity(units.len());
    let mut index = 0usize;
    while index < units.len() {
        let unit = units[index];
        let (code_point, width) = match decode_code_point(&units, index) {
            Some(pair) => pair,
            None => {
                mapped.push(unit);
                index += 1;
                continue;
            }
        };
        if upper {
            for replacement in code_point.to_uppercase() {
                push_code_point(&mut mapped, replacement);
            }
        } else {
            for replacement in code_point.to_lowercase() {
                push_code_point(&mut mapped, replacement);
            }
        }
        index += width;
    }
    Completion::Normal(JsValue::String(JsString::from_units(&mapped)))
}

/// The code point at `index`, and how many units it took. `None` for an unpaired surrogate.
fn decode_code_point(units: &[u16], index: usize) -> Option<(char, usize)> {
    let first = units[index];
    if (0xD800..0xDC00).contains(&first) {
        let second = *units.get(index + 1)?;
        if !(0xDC00..0xE000).contains(&second) {
            return None;
        }
        let combined = 0x1_0000
            + ((u32::from(first) - 0xD800) << 10)
            + (u32::from(second) - 0xDC00);
        return char::from_u32(combined).map(|ch| (ch, 2));
    }
    char::from_u32(u32::from(first)).map(|ch| (ch, 1))
}

/// Appends a code point as UTF-16, which is one unit or a surrogate pair.
fn push_code_point(out: &mut Vec<u16>, ch: char) {
    let mut buffer = [0u16; 2];
    for unit in ch.encode_utf16(&mut buffer) {
        out.push(*unit);
    }
}

/// Finds a code-unit subsequence, which is what every string search is defined over.
fn find_units(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return if from <= haystack.len() { Some(from) } else { None };
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&start| &haystack[start..start + needle.len()] == needle)
}

/// `thisStringValue`: the primitive behind `this`, whether it is one or wraps one.
fn this_string_value(interpreter: &mut Interpreter, this: &JsValue) -> Completion {
    match this {
        JsValue::String(_) => Completion::Normal(this.clone()),
        JsValue::Object(id) => match interpreter.object(*id).primitive.clone() {
            Some(JsValue::String(text)) => Completion::Normal(JsValue::String(text)),
            _ => interpreter.type_error("String.prototype.toString requires a string"),
        },
        _ => interpreter.type_error("String.prototype.toString requires a string"),
    }
}

/// `RequireObjectCoercible` then `ToString`: what a generic String method does to its receiver.
fn coerce_to_string(
    interpreter: &mut Interpreter,
    this: &JsValue,
) -> Result<JsString, Completion> {
    if matches!(this, JsValue::Undefined | JsValue::Null) {
        return Err(interpreter.type_error("a String method requires a coercible receiver"));
    }
    interpreter.to_string_value(this)
}


/// `String.prototype.match`, `search` and `matchAll`, which differ in the symbol they look up and
/// in the flags their fallback pattern is built with.
///
/// All three are: coerce the receiver, look for the method on the ARGUMENT, and fall back to building a
/// regular expression from the argument when it has none. A `null` or `undefined` argument skips
/// the lookup and goes straight to construction, which is how `"a".match()` matches the empty
/// pattern rather than throwing.
fn dispatch_through_symbol(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    symbol: &str,
    fallback_flags: &str,
) -> Completion {
    if matches!(this, JsValue::Undefined | JsValue::Null) {
        return interpreter.type_error("a String method requires a coercible receiver");
    }

    let pattern = arg(arguments, 0);
    if !matches!(pattern, JsValue::Undefined | JsValue::Null) {
        if let JsValue::Object(id) = &pattern {
            let key = PropertyKey::Symbol(interpreter.well_known_symbol(symbol));
            let method = match interpreter.get_property(*id, &key) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            if !matches!(method, JsValue::Undefined | JsValue::Null) {
                if !interpreter.is_callable(&method) {
                    return interpreter.type_error("a String method's hook is not callable");
                }
                return interpreter.call_value(&method, pattern.clone(), vec![this.clone()]);
            }
        }
    }

    let text = match coerce_to_string(interpreter, &this) {
        Ok(text) => text,
        Err(abrupt) => return abrupt,
    };
    let source = match &pattern {
        JsValue::Undefined => crate::JsString::new(),
        other => match interpreter.to_string_value(other) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        },
    };
    let built = match crate::regexp::create(
        interpreter,
        &source,
        &crate::JsString::from(fallback_flags),
    ) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let key = PropertyKey::Symbol(interpreter.well_known_symbol(symbol));
    let method = match interpreter.get_property(built, &key) {
        Completion::Normal(value) => value,
        abrupt => return abrupt,
    };
    interpreter.call_value(&method, JsValue::Object(built), vec![JsValue::String(text)])
}

/// `IsRegExp`: the property, not the slot.
///
/// The standard asks for `@@match` first and only falls back to the internal slot, so an ordinary
/// object claiming to be a pattern IS one for the purposes of `replaceAll`'s global check. Testing
/// the slot alone would let such an object past a check the standard applies to it.
fn is_regexp(interpreter: &mut Interpreter, value: &JsValue) -> bool {
    let JsValue::Object(id) = value else { return false };
    let key = PropertyKey::Symbol(interpreter.well_known_symbol("match"));
    match interpreter.get_property(*id, &key) {
        Completion::Normal(JsValue::Undefined) => interpreter.object(*id).regexp.is_some(),
        Completion::Normal(other) => ops::to_boolean(&other),
        _ => false,
    }
}

/// The `Symbol.replace` hand-off shared by `replace` and `replaceAll`.
///
/// Answers `None` when the pattern has no hook, which is the caller's signal to run the string
/// path. The receiver goes through UNCONVERTED: the hook does its own `ToString`, and coercing here
/// would run a user `toString` before the lookup rather than after it.
fn dispatch_replace(
    interpreter: &mut Interpreter,
    this: &JsValue,
    pattern: &JsValue,
    arguments: &[JsValue],
) -> Option<Completion> {
    let JsValue::Object(id) = pattern else { return None };
    let key = PropertyKey::Symbol(interpreter.well_known_symbol("replace"));
    let method = match interpreter.get_property(*id, &key) {
        Completion::Normal(value) => value,
        abrupt => return Some(abrupt),
    };
    if matches!(method, JsValue::Undefined | JsValue::Null) {
        return None;
    }
    if !interpreter.is_callable(&method) {
        return Some(interpreter.type_error("a pattern's Symbol.replace is not callable"));
    }
    Some(interpreter.call_value(&method, pattern.clone(), vec![this.clone(), arg(arguments, 1)]))
}

fn install_number(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.object_mut(prototype).primitive = Some(JsValue::Number(0.0));
    interpreter.intrinsics.number_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "Number",
        1,
        |interpreter, _this, arguments| {
            if arguments.is_empty() {
                return Completion::Normal(JsValue::Number(0.0));
            }
            match interpreter.to_number_value(&arguments[0]) {
                Ok(number) => Completion::Normal(JsValue::Number(number)),
                Err(abrupt) => abrupt,
            }
        },
        |interpreter, _this, arguments| {
            let number = if arguments.is_empty() {
                0.0
            } else {
                match interpreter.to_number_value(&arguments[0]) {
                    Ok(number) => number,
                    Err(abrupt) => return abrupt,
                }
            };
            match interpreter.to_object(&JsValue::Number(number)) {
                Ok(id) => Completion::Normal(JsValue::Object(id)),
                Err(abrupt) => abrupt,
            }
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Number", JsValue::Object(constructor));

    for (name, value) in [
        ("MAX_SAFE_INTEGER", 9_007_199_254_740_991.0),
        ("MIN_SAFE_INTEGER", -9_007_199_254_740_991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("EPSILON", f64::EPSILON),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ] {
        interpreter.define_constant(constructor, name, JsValue::Number(value));
    }

    interpreter.define_method(constructor, "isNaN", 1, |_interpreter, _this, arguments| {
        let result = matches!(arg(arguments, 0), JsValue::Number(number) if number.is_nan());
        Completion::Normal(JsValue::Boolean(result))
    });
    interpreter.define_method(constructor, "isFinite", 1, |_interpreter, _this, arguments| {
        let result = matches!(arg(arguments, 0), JsValue::Number(number) if number.is_finite());
        Completion::Normal(JsValue::Boolean(result))
    });
    interpreter.define_method(constructor, "isInteger", 1, |_interpreter, _this, arguments| {
        let result = matches!(arg(arguments, 0), JsValue::Number(number)
            if number.is_finite() && crate::math::fract(number) == 0.0);
        Completion::Normal(JsValue::Boolean(result))
    });
    interpreter.define_method(constructor, "isSafeInteger", 1, |_interpreter, _this, arguments| {
        let result = matches!(arg(arguments, 0), JsValue::Number(number)
            if number.is_finite() && crate::math::fract(number) == 0.0 && number.abs() <= 9_007_199_254_740_991.0);
        Completion::Normal(JsValue::Boolean(result))
    });

    interpreter.define_method(prototype, "valueOf", 0, |interpreter, this, _| {
        this_number_value(interpreter, &this)
    });
    interpreter.define_method(prototype, "toString", 1, |interpreter, this, arguments| {
        let number = match this_number_value(interpreter, &this) {
            Completion::Normal(JsValue::Number(number)) => number,
            other => return other,
        };
        let radix = match arg(arguments, 0) {
            JsValue::Undefined => 10,
            value => match interpreter.to_number_value(&value) {
                Ok(radix) => {
                    let radix = to_integer(radix);
                    if !(2.0..=36.0).contains(&radix) {
                        return interpreter
                            .throw("RangeError", "radix must be between 2 and 36");
                    }
                    radix as u32
                }
                Err(abrupt) => return abrupt,
            },
        };
        if radix == 10 {
            return Completion::Normal(JsValue::string(&ops::number_to_string(number)));
        }
        Completion::Normal(JsValue::string(&number_to_radix_string(number, radix)))
    });

    interpreter.define_method(prototype, "toFixed", 1, |interpreter, this, arguments| {
        let number = match this_number_value(interpreter, &this) {
            Completion::Normal(JsValue::Number(number)) => number,
            other => return other,
        };
        let digits = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(digits) => to_integer(digits),
            Err(abrupt) => return abrupt,
        };
        if !(0.0..=100.0).contains(&digits) {
            return interpreter.throw("RangeError", "toFixed digits must be between 0 and 100");
        }
        if number.is_nan() {
            return Completion::Normal(JsValue::string("NaN"));
        }
        if number.abs() >= 1e21 {
            return Completion::Normal(JsValue::string(&ops::number_to_string(number)));
        }
        Completion::Normal(JsValue::string(&signed(number, |magnitude| {
            crate::decimal::fixed(magnitude, digits as usize)
        })))
    });

    interpreter.define_method(prototype, "toLocaleString", 0, |interpreter, this, _| {
        match this_number_value(interpreter, &this) {
            Completion::Normal(JsValue::Number(number)) => {
                Completion::Normal(JsValue::string(&ops::number_to_string(number)))
            }
            other => other,
        }
    });

    interpreter.define_method(prototype, "toExponential", 1, |interpreter, this, arguments| {
        let number = match this_number_value(interpreter, &this) {
            Completion::Normal(JsValue::Number(number)) => number,
            other => return other,
        };
        let requested = match arg(arguments, 0) {
            JsValue::Undefined => None,
            value => match interpreter.to_number_value(&value) {
                Ok(digits) => Some(to_integer(digits)),
                Err(abrupt) => return abrupt,
            },
        };
        if !number.is_finite() {
            return Completion::Normal(JsValue::string(&ops::number_to_string(number)));
        }
        if let Some(digits) = requested {
            if !(0.0..=100.0).contains(&digits) {
                return interpreter
                    .throw("RangeError", "toExponential digits must be between 0 and 100");
            }
        }
        Completion::Normal(JsValue::string(&signed(number, |magnitude| {
            let keep = match requested {
                Some(digits) => digits as usize + 1,
                None => shortest_significant(magnitude),
            };
            let (text, exponent) = crate::decimal::significant(magnitude, keep);
            let mut out = String::from(&text[..1]);
            if keep > 1 {
                out.push('.');
                out.push_str(&text[1..]);
            }
            out.push_str(&crate::decimal::exponent_suffix(exponent));
            out
        })))
    });

    interpreter.define_method(prototype, "toPrecision", 1, |interpreter, this, arguments| {
        let number = match this_number_value(interpreter, &this) {
            Completion::Normal(JsValue::Number(number)) => number,
            other => return other,
        };
        if matches!(arg(arguments, 0), JsValue::Undefined) {
            return Completion::Normal(JsValue::string(&ops::number_to_string(number)));
        }
        let precision = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(precision) => to_integer(precision),
            Err(abrupt) => return abrupt,
        };
        if !number.is_finite() {
            return Completion::Normal(JsValue::string(&ops::number_to_string(number)));
        }
        if !(1.0..=100.0).contains(&precision) {
            return interpreter
                .throw("RangeError", "toPrecision must be between 1 and 100");
        }
        let precision = precision as usize;
        Completion::Normal(JsValue::string(&signed(number, |magnitude| {
            let (text, exponent) = crate::decimal::significant(magnitude, precision);
            if exponent < -6 || exponent >= precision as i32 {
                let mut out = String::from(&text[..1]);
                if precision > 1 {
                    out.push('.');
                    out.push_str(&text[1..]);
                }
                out.push_str(&crate::decimal::exponent_suffix(exponent));
                return out;
            }
            crate::decimal::fixed(magnitude, (precision as i32 - 1 - exponent).max(0) as usize)
        })))
    });
}

/// Formats the MAGNITUDE and puts the sign back, so every routine below works on a positive value.
///
/// `-0` TAKES NO SIGN. `(-0).toFixed(2)` is `"0.00"`, not `"-0.00"` -- the standard tests
/// `x < 0`, which is false for negative zero, rather than looking at the sign bit.
fn signed(number: f64, body: impl FnOnce(f64) -> String) -> String {
    if number < 0.0 {
        format!("-{}", body(-number))
    } else {
        body(number)
    }
}

/// How many significant digits `magnitude` actually has, for `toExponential` with no argument.
///
/// The standard asks for the SMALLEST digit count that represents the value exactly, which is the
/// shortest form that reads back as the same number -- what `Number::toString` produces. Not the
/// exact expansion, which for `0.1` is fifty-five digits of noise nobody asked to see.
///
/// ZEROS AT BOTH ENDS ARE PLACEHOLDERS RATHER THAN DIGITS, and the trailing ones are the half
/// that is easy to miss: `(100).toExponential()` is `"1e+2"`, so `100` has ONE significant digit
/// here, not three. Counting them gives `"1.00e+2"` -- a correct-looking answer for every value
/// that happens not to end in a zero.
fn shortest_significant(magnitude: f64) -> usize {
    let shortest = ops::number_to_string(magnitude);
    let digits = shortest
        .bytes()
        .take_while(|byte| *byte != b'e' && *byte != b'E')
        .filter(u8::is_ascii_digit)
        .collect::<Vec<u8>>();
    let from_first = digits.iter().skip_while(|d| **d == b'0').count();
    let trailing = digits.iter().rev().take_while(|d| **d == b'0').count();
    from_first.saturating_sub(trailing).max(1)
}

fn this_number_value(interpreter: &mut Interpreter, this: &JsValue) -> Completion {
    match this {
        JsValue::Number(_) => Completion::Normal(this.clone()),
        JsValue::Object(id) => match interpreter.object(*id).primitive.clone() {
            Some(JsValue::Number(number)) => Completion::Normal(JsValue::Number(number)),
            _ => interpreter.type_error("a Number method requires a number"),
        },
        _ => interpreter.type_error("a Number method requires a number"),
    }
}


fn install_boolean(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.object_mut(prototype).primitive = Some(JsValue::Boolean(false));
    interpreter.intrinsics.boolean_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "Boolean",
        1,
        |_interpreter, _this, arguments| {
            Completion::Normal(JsValue::Boolean(ops::to_boolean(&arg(arguments, 0))))
        },
        |interpreter, _this, arguments| {
            let value = JsValue::Boolean(ops::to_boolean(&arg(arguments, 0)));
            match interpreter.to_object(&value) {
                Ok(id) => Completion::Normal(JsValue::Object(id)),
                Err(abrupt) => abrupt,
            }
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Boolean", JsValue::Object(constructor));

    interpreter.define_method(prototype, "valueOf", 0, |interpreter, this, _| {
        this_boolean_value(interpreter, &this)
    });
    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        match this_boolean_value(interpreter, &this) {
            Completion::Normal(JsValue::Boolean(value)) => {
                Completion::Normal(JsValue::string(if value { "true" } else { "false" }))
            }
            other => other,
        }
    });
}

fn this_boolean_value(interpreter: &mut Interpreter, this: &JsValue) -> Completion {
    match this {
        JsValue::Boolean(_) => Completion::Normal(this.clone()),
        JsValue::Object(id) => match interpreter.object(*id).primitive.clone() {
            Some(JsValue::Boolean(value)) => Completion::Normal(JsValue::Boolean(value)),
            _ => interpreter.type_error("a Boolean method requires a boolean"),
        },
        _ => interpreter.type_error("a Boolean method requires a boolean"),
    }
}


/// `Symbol`, its prototype, and the well-known symbols this profile carries.
///
/// # IT IS INSTALLED BEFORE `Math` AND AFTER THE PROTOTYPES, AND THE ORDER IS LOAD-BEARING
///
/// The well-known symbols must EXIST before anything can hang a method off one, so this runs before
/// any installer that wants to define `[Symbol.iterator]`. It runs after the prototypes because a
/// built-in function object needs `Function.prototype`.
fn install_symbol(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.symbol_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "Symbol",
        0,
        |interpreter, _this, arguments| {
            let description = match arg(arguments, 0) {
                JsValue::Undefined => None,
                other => match interpreter.to_string_value(&other) {
                    Ok(text) => Some(text),
                    Err(abrupt) => return abrupt,
                },
            };
            Completion::Normal(JsValue::Symbol(interpreter.allocate_symbol(description)))
        },
        |interpreter, _this, _arguments| {
            interpreter.type_error("Symbol is not a constructor")
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Symbol", JsValue::Object(constructor));

    for (index, name) in WELL_KNOWN_SYMBOLS.iter().enumerate() {
        let id = interpreter.allocate_symbol(Some(JsString::from(
            alloc::format!("Symbol.{name}").as_str(),
        )));
        interpreter.intrinsics.well_known_symbols[index] = id;
        interpreter.define_constant(constructor, name, JsValue::Symbol(id));
    }
    interpreter.intrinsics.well_known_symbols_ready = true;

    interpreter.define_to_string_tag(prototype, "Symbol");

    let to_primitive = interpreter.native_function(
        "[Symbol.toPrimitive]",
        1,
        |interpreter, this, _arguments| match this_symbol_value(interpreter, &this) {
            Ok(id) => Completion::Normal(JsValue::Symbol(id)),
            Err(abrupt) => abrupt,
        },
    );
    interpreter.define_well_known_symbol_method(prototype, "toPrimitive", to_primitive);

    interpreter.define_method(constructor, "for", 1, |interpreter, _this, arguments| {
        let key = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(key) => key,
            Err(abrupt) => return abrupt,
        };
        Completion::Normal(JsValue::Symbol(interpreter.symbol_for(&key)))
    });

    interpreter.define_method(constructor, "keyFor", 1, |interpreter, _this, arguments| {
        let JsValue::Symbol(id) = arg(arguments, 0) else {
            return interpreter.type_error("Symbol.keyFor requires a symbol");
        };
        Completion::Normal(match interpreter.symbol_key_for(id) {
            Some(key) => JsValue::String(key.clone()),
            None => JsValue::Undefined,
        })
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        match this_symbol_value(interpreter, &this) {
            Ok(id) => {
                let text = interpreter.symbol_to_display(id);
                Completion::Normal(JsValue::string(&text))
            }
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "valueOf", 0, |interpreter, this, _| {
        match this_symbol_value(interpreter, &this) {
            Ok(id) => Completion::Normal(JsValue::Symbol(id)),
            Err(abrupt) => abrupt,
        }
    });

    let getter = interpreter.native_function("get description", 0, |interpreter, this, _| {
        match this_symbol_value(interpreter, &this) {
            Ok(id) => Completion::Normal(match interpreter.symbol_description(id) {
                Some(text) => JsValue::String(text.clone()),
                None => JsValue::Undefined,
            }),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.object_mut(prototype).set_own(
        PropertyKey::from_str("description"),
        Property::accessor(Some(getter), None),
    );
}

/// `thisSymbolValue`: the receiver of a `Symbol.prototype` method is a symbol, or a wrapper boxing
/// one.
///
/// THIS SAID THERE WAS NO WRAPPER CASE BECAUSE `Object(sym)` "is not built", AND `Object(sym)`
/// WAS BUILT. It came back wearing `Boolean.prototype`, because the boxing routine matched String
/// and Number and sent everything else to Boolean through a default arm -- so the route existed,
/// produced a wrong object, and a note written by reasoning about the code rather than by running
/// it recorded the opposite. **The branch that "can never be taken" is the one to try before
/// writing that it cannot.**
fn this_symbol_value(
    interpreter: &mut Interpreter,
    this: &JsValue,
) -> Result<crate::value::SymbolId, Completion> {
    match this {
        JsValue::Symbol(id) => Ok(*id),
        JsValue::Object(object) => match interpreter.object(*object).primitive {
            Some(JsValue::Symbol(id)) => Ok(id),
            _ => Err(interpreter.type_error("a Symbol method requires a symbol")),
        },
        _ => Err(interpreter.type_error("a Symbol method requires a symbol")),
    }
}


fn install_math(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let math = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.define_global("Math", JsValue::Object(math));
    interpreter.define_to_string_tag(math, "Math");

    for (name, value) in [
        ("PI", core::f64::consts::PI),
        ("E", core::f64::consts::E),
        ("LN2", core::f64::consts::LN_2),
        ("LN10", core::f64::consts::LN_10),
        ("LOG2E", core::f64::consts::LOG2_E),
        ("LOG10E", core::f64::consts::LOG10_E),
        ("SQRT2", core::f64::consts::SQRT_2),
        ("SQRT1_2", core::f64::consts::FRAC_1_SQRT_2),
    ] {
        interpreter.define_constant(math, name, JsValue::Number(value));
    }

    interpreter.define_method(math, "pow", 2, |interpreter, _this, arguments| {
        let base = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        let exponent = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        Completion::Normal(JsValue::Number(ops::exponentiate(base, exponent)))
    });

    interpreter.define_method(math, "max", 2, |interpreter, _this, arguments| {
        extremum(interpreter, arguments, true)
    });
    interpreter.define_method(math, "min", 2, |interpreter, _this, arguments| {
        extremum(interpreter, arguments, false)
    });

    for (name, call) in UNARY_MATH {
        interpreter.define_method(math, name, 1, call);
    }

    interpreter.define_method(math, "atan2", 2, |interpreter, _this, arguments| {
        let y = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        let x = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        Completion::Normal(JsValue::Number(crate::math::atan2(y, x)))
    });

    interpreter.define_method(math, "hypot", 2, |interpreter, _this, arguments| {
        let mut sum = 0.0f64;
        let mut infinite = false;
        let mut nan = false;
        for argument in arguments {
            match interpreter.to_number_value(argument) {
                Ok(number) if number.is_infinite() => infinite = true,
                Ok(number) if number.is_nan() => nan = true,
                Ok(number) => sum += number * number,
                Err(abrupt) => return abrupt,
            }
        }
        Completion::Normal(JsValue::Number(if infinite {
            f64::INFINITY
        } else if nan {
            f64::NAN
        } else {
            crate::math::sqrt(sum)
        }))
    });

    interpreter.define_method(math, "random", 0, |interpreter, _this, _arguments| {
        interpreter.refuse(Absence::MathRandom)
    });

    interpreter.define_method(math, "imul", 2, |interpreter, _this, arguments| {
        let a = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => ops::to_int32(number),
            Err(abrupt) => return abrupt,
        };
        let b = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => ops::to_int32(number),
            Err(abrupt) => return abrupt,
        };
        Completion::Normal(JsValue::Number(f64::from(a.wrapping_mul(b))))
    });
}

/// The single-argument Math functions, each a conversion followed by one `f64` operation.
const UNARY_MATH: [(&str, crate::interpreter::NativeFn); 29] = [
    ("tan", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::tan)),
    ("asin", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::asin)),
    ("acos", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::acos)),
    ("atan", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::atan)),
    ("cbrt", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::cbrt)),
    ("log2", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::log2)),
    ("log10", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::log10)),
    ("clz32", |interpreter, _this, arguments| {
        unary_math(interpreter, arguments, |number| f64::from(ops::to_uint32(number).leading_zeros()))
    }),
    ("fround", |interpreter, _this, arguments| {
        unary_math(interpreter, arguments, |number| f64::from(number as f32))
    }),
    ("f16round", |interpreter, _this, arguments| {
        unary_math(interpreter, arguments, crate::math::f16round)
    }),
    ("abs", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::abs)),
    ("floor", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::floor)),
    ("ceil", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::ceil)),
    ("trunc", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::trunc)),
    ("sqrt", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::sqrt)),
    ("log", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::ln)),
    ("exp", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::exp)),
    ("sin", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::sin)),
    ("cos", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::cos)),
    ("round", |interpreter, _this, arguments| unary_math(interpreter, arguments, js_round)),
    ("sign", |interpreter, _this, arguments| unary_math(interpreter, arguments, js_sign)),
    ("sinh", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::sinh)),
    ("cosh", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::cosh)),
    ("tanh", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::tanh)),
    ("asinh", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::asinh)),
    ("acosh", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::acosh)),
    ("atanh", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::atanh)),
    ("log1p", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::log1p)),
    ("expm1", |interpreter, _this, arguments| unary_math(interpreter, arguments, crate::math::expm1)),
];

/// `Math.round` IS NOT `f64::round`. JavaScript rounds halves toward POSITIVE INFINITY, so
/// `Math.round(-0.5)` is `-0` while Rust's `round` gives `-1`. Every `.5` at a negative value
/// differs, which is a whole class of off-by-one nobody would look for in a rounding function.
fn js_round(number: f64) -> f64 {
    if number.is_nan() || number.is_infinite() || number == 0.0 {
        return number;
    }
    if number < 0.0 && number >= -0.5 {
        return -0.0;
    }
    if number > 0.0 && number < 0.5 {
        return 0.0;
    }
    if crate::math::abs(number) >= 4_503_599_627_370_496.0 {
        return number;
    }
    crate::math::floor(number + 0.5)
}

/// `Math.sign`, which preserves the sign of zero rather than collapsing it.
fn js_sign(number: f64) -> f64 {
    if number.is_nan() || number == 0.0 {
        number
    } else if number > 0.0 {
        1.0
    } else {
        -1.0
    }
}

fn unary_math(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
    operation: fn(f64) -> f64,
) -> Completion {
    match interpreter.to_number_value(&arg(arguments, 0)) {
        Ok(number) => Completion::Normal(JsValue::Number(operation(number))),
        Err(abrupt) => abrupt,
    }
}

fn extremum(interpreter: &mut Interpreter, arguments: &[JsValue], maximum: bool) -> Completion {
    let mut result = if maximum { f64::NEG_INFINITY } else { f64::INFINITY };
    let mut saw_nan = false;
    for argument in arguments {
        let number = match interpreter.to_number_value(argument) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        if number.is_nan() {
            saw_nan = true;
            continue;
        }
        if saw_nan {
            continue;
        }
        let replace = if maximum {
            number > result || (number == result && result.is_sign_negative())
        } else {
            number < result || (number == result && number.is_sign_negative())
        };
        if replace {
            result = number;
        }
    }
    Completion::Normal(JsValue::Number(if saw_nan { f64::NAN } else { result }))
}


fn install_global_functions(interpreter: &mut Interpreter) {
    let is_nan = interpreter.native_function("isNaN", 1, |interpreter, _this, arguments| {
        match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => Completion::Normal(JsValue::Boolean(number.is_nan())),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.define_global("isNaN", JsValue::Object(is_nan));

    let is_finite = interpreter.native_function("isFinite", 1, |interpreter, _this, arguments| {
        match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => Completion::Normal(JsValue::Boolean(number.is_finite())),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.define_global("isFinite", JsValue::Object(is_finite));

    let parse_int = interpreter.native_function("parseInt", 2, |interpreter, _this, arguments| {
        let text = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let radix = match arg(arguments, 1) {
            JsValue::Undefined => 0,
            value => match interpreter.to_number_value(&value) {
                Ok(number) => ops::to_int32(number),
                Err(abrupt) => return abrupt,
            },
        };
        Completion::Normal(JsValue::Number(parse_int_radix(&text, radix)))
    });
    interpreter.define_global("parseInt", JsValue::Object(parse_int));

    #[cfg(not(feature = "eval"))]
    let eval_fn = interpreter.native_function("eval", 1, |interpreter, _this, _arguments| {
        interpreter.refuse(Absence::Eval)
    });
    #[cfg(feature = "eval")]
    let eval_fn = interpreter.native_function("eval", 1, crate::eval::indirect_eval);
    interpreter.define_global("eval", JsValue::Object(eval_fn));

    let parse_float =
        interpreter.native_function("parseFloat", 1, |interpreter, _this, arguments| {
            let text = match interpreter.to_string_value(&arg(arguments, 0)) {
                Ok(text) => text,
                Err(abrupt) => return abrupt,
            };
            Completion::Normal(JsValue::Number(parse_leading_number(&text, false)))
        });
    interpreter.define_global("parseFloat", JsValue::Object(parse_float));

    if let Some(JsValue::Object(number)) = interpreter.global_value("Number") {
        interpreter.define_builtin(number, "parseInt", JsValue::Object(parse_int));
        interpreter.define_builtin(number, "parseFloat", JsValue::Object(parse_float));
    }
}

/// `parseInt`/`parseFloat`: the LONGEST LEADING PREFIX that parses, or NaN.
///
/// **THIS IS THE OPPOSITE OF `ToNumber`, WHICH TAKES THE WHOLE STRING OR NOTHING.** `+"12abc"` is
/// NaN and `parseInt("12abc")` is 12. Sharing one implementation between them -- the natural
/// economy -- makes one of the two wrong, and the wrongness is silent.
fn parse_leading_number(text: &JsString, integer_only: bool) -> f64 {
    let rendered = text.to_lossy_string();
    let trimmed = rendered.trim_start_matches(|c: char| c.is_whitespace());
    let bytes = trimmed.as_bytes();
    let mut end = 0usize;
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let digits_start = end;
    if !integer_only && trimmed[end..].starts_with("Infinity") {
        let negative = digits_start > 0 && bytes[0] == b'-';
        return if negative { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if !integer_only {
        if end < bytes.len() && bytes[end] == b'.' {
            end += 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
        }
        if end < bytes.len() && (bytes[end] | 0x20) == b'e' {
            let exponent_start = end;
            end += 1;
            if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
                end += 1;
            }
            if end < bytes.len() && bytes[end].is_ascii_digit() {
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
            } else {
                end = exponent_start;
            }
        }
    }
    if end == digits_start {
        return f64::NAN;
    }
    trimmed[..end].parse::<f64>().map(|value| if integer_only { crate::math::trunc(value) } else { value }).unwrap_or(f64::NAN)
}


fn install_global_this(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let global = interpreter.allocate(Object::new(Some(object_prototype)));
    for name in interpreter.global_binding_names() {
        if let Some(value) = interpreter.global_value(&name) {
            interpreter.define_builtin(global, &name, value);
        }
    }
    interpreter.define_builtin(global, "globalThis", JsValue::Object(global));
    for (name, value) in [
        ("undefined", JsValue::Undefined),
        ("NaN", JsValue::Number(f64::NAN)),
        ("Infinity", JsValue::Number(f64::INFINITY)),
    ] {
        interpreter.define_constant(global, name, value);
    }
    interpreter.define_global("globalThis", JsValue::Object(global));
}

/// Which descriptor fields were actually PRESENT, which is not the same as their values.
///
/// `{enumerable: false}` on a non-configurable property whose `enumerable` is already false is
/// LEGAL; `{enumerable: true}` on the same property is a TypeError. The rules are about CHANGES, so
/// a field that was omitted has to be told from one that was supplied with the value it already had.
#[derive(Default, Clone, Copy)]
pub(crate) struct PresentFields {
    pub(crate) value: bool,
    pub(crate) writable: bool,
    pub(crate) enumerable: bool,
    pub(crate) configurable: bool,
    pub(crate) get: bool,
    pub(crate) set: bool,
}

/// A descriptor as the program wrote it: which fields were supplied, and what they read as.
#[derive(Clone)]
pub(crate) struct RawDescriptor {
    pub(crate) present: PresentFields,
    pub(crate) enumerable: bool,
    pub(crate) configurable: bool,
    pub(crate) value: JsValue,
    pub(crate) writable: bool,
    pub(crate) get: Option<ObjectId>,
    pub(crate) set: Option<ObjectId>,
}

impl RawDescriptor {
    /// A descriptor naming NO fields, which callers fill in by setting `present` flags.
    ///
    /// Every value field is at the standard's default, so setting only a `present` flag requests
    /// exactly `false` / `undefined` -- which is what `SetIntegrityLevel` wants and is the reason
    /// this exists rather than each caller writing seven fields out.
    pub(crate) fn empty() -> Self {
        Self {
            present: PresentFields::default(),
            enumerable: false,
            configurable: false,
            value: JsValue::Undefined,
            writable: false,
            get: None,
            set: None,
        }
    }

    /// `IsAccessorDescriptor`: whether either accessor half was SUPPLIED, which is not the same as
    /// either being non-`undefined` -- `{get: undefined}` is an accessor descriptor that removes a
    /// getter.
    pub(crate) fn has_accessor_field(&self) -> bool {
        self.present.get || self.present.set
    }

    /// Whether this descriptor asks for `configurable: false`, which is the standard's
    /// `settingConfigFalse` and is a question about the FIELD rather than about the value.
    ///
    /// An omitted `configurable` is not a request for `false` even though the field defaults to
    /// `false` on a new property: the proxy invariants are about what the handler CLAIMED.
    pub(crate) fn declares_non_configurable(&self) -> bool {
        self.present.configurable && !self.configurable
    }

    /// The same question for `writable`.
    pub(crate) fn declares_non_writable(&self) -> bool {
        self.present.writable && !self.writable
    }
}

/// `FromPropertyDescriptor` for a PARTIAL descriptor: only the fields that were supplied.
///
/// IT IS NOT [`describe_property`] WITH A DIFFERENT ARGUMENT, AND THE DIFFERENCE IS OBSERVABLE.
/// `describe_property` reports a property that EXISTS, so all four of its fields are known;
/// this reports a REQUEST, where an absent field means "leave this alone". A `defineProperty` trap
/// that forwards its third argument to `Reflect.defineProperty` must not find an `enumerable: false`
/// the caller never wrote -- that would turn `Object.defineProperty(p, "x", {value: 1})` on an
/// existing enumerable property into one that hides it.
pub(crate) fn describe_raw_descriptor(
    interpreter: &mut Interpreter,
    descriptor: &RawDescriptor,
) -> crate::value::ObjectId {
    let object = interpreter.allocate(Object::new(Some(interpreter.intrinsics.object_prototype)));
    let present = descriptor.present;
    let fields: Vec<(bool, &str, JsValue)> = vec![
        (present.value, "value", descriptor.value.clone()),
        (present.writable, "writable", JsValue::Boolean(descriptor.writable)),
        (present.get, "get", descriptor.get.map_or(JsValue::Undefined, JsValue::Object)),
        (present.set, "set", descriptor.set.map_or(JsValue::Undefined, JsValue::Object)),
        (present.enumerable, "enumerable", JsValue::Boolean(descriptor.enumerable)),
        (present.configurable, "configurable", JsValue::Boolean(descriptor.configurable)),
    ];
    for (supplied, name, value) in fields {
        if supplied {
            interpreter
                .object_mut(object)
                .set_own(PropertyKey::from_str(name), Property::data(value));
        }
    }
    object
}

/// `IsCompatiblePropertyDescriptor(extensible, Desc, Current)`: whether a definition WOULD be
/// allowed, asked without performing it.
///
/// IT IS `ValidateAndApplyPropertyDescriptor` WITH NO OBJECT TO APPLY TO, which is why it shares
/// [`merge_descriptor`] and [`validate_redefinition`] with the real definition rather than
/// re-deciding. Two implementations of "may this property be redefined?" is exactly the shape where
/// a new case lands in one of them -- and here the second caller is a proxy invariant, so the two
/// would disagree about what an ordinary object permits.
pub(crate) fn is_compatible_property_descriptor(
    extensible: bool,
    descriptor: &RawDescriptor,
    current: Option<&Property>,
) -> bool {
    let Some(current) = current else { return extensible };
    let proposed = merge_descriptor(Some(current), descriptor);
    validate_redefinition(Some(current), &proposed, descriptor.present).is_ok()
}

/// The property a descriptor DESCRIBES once merged with what is already there.
///
/// An omitted field takes the EXISTING value, and on a property that does not exist yet it takes
/// the standard's default -- `undefined`, and false for all three attributes. Defaulting to the
/// ordinary-assignment attributes instead makes `Object.defineProperty(o, "x", {value: 1})` create a
/// writable, enumerable, configurable property where the standard creates a frozen one.
pub(crate) fn merge_descriptor(
    existing: Option<&Property>,
    descriptor: &RawDescriptor,
) -> Property {
    let present = descriptor.present;
    let keep = |supplied: bool, new: bool, old: bool| if supplied { new } else { old };

    let enumerable = keep(
        present.enumerable,
        descriptor.enumerable,
        existing.is_some_and(|property| property.enumerable),
    );
    let configurable = keep(
        present.configurable,
        descriptor.configurable,
        existing.is_some_and(|property| property.configurable),
    );

    let generic = !present.get && !present.set && !present.value && !present.writable;
    let kind = if generic {
        match existing.map(|property| property.kind.clone()) {
            Some(kind) => kind,
            None => PropertyKind::Data { value: JsValue::Undefined, writable: false },
        }
    } else if present.get || present.set {
        let (had_get, had_set) = match existing.map(|property| &property.kind) {
            Some(PropertyKind::Accessor { get, set }) => (*get, *set),
            _ => (None, None),
        };
        PropertyKind::Accessor {
            get: if present.get { descriptor.get } else { had_get },
            set: if present.set { descriptor.set } else { had_set },
        }
    } else {
        let value = if present.value {
            descriptor.value.clone()
        } else {
            existing
                .and_then(|property| property.data_value().cloned())
                .unwrap_or(JsValue::Undefined)
        };
        let writable =
            keep(present.writable, descriptor.writable, existing.is_some_and(Property::is_writable));
        PropertyKind::Data { value, writable }
    };
    Property { kind, enumerable, configurable }
}

/// `ToPropertyDescriptor` (ECMA-262 6.2.6.5): reads a descriptor OBJECT into the fields it supplies.
///
/// # THREE THINGS THIS FIXED, EACH INVISIBLE UNTIL A PROGRAM DEPENDED ON IT
///
/// **1. It reads with `HasProperty` and `Get`, not with `own`.** The previous version inspected the
/// descriptor's OWN properties, so `Object.create(proto, {x: Object.create({value: 1})})` saw a
/// descriptor with no fields and defined `undefined` -- **an inherited descriptor field is not an
/// edge case, it is how `Object.create`'s own corpus tests are written.** It also means a descriptor
/// field may be an ACCESSOR, and reading it RUNS CODE.
///
/// **2. Because reading runs code, the ORDER IS OBSERVABLE, and the standard fixes it**:
/// enumerable, configurable, value, writable, get, set. A counting getter is the only instrument
/// that can see this, and the corpus has them.
///
/// **3. The both-shapes check is LAST, after every field has been read.** The previous version
/// rejected `{get: f, value: 1}` before reading anything; the standard reads all six fields --
/// running any getters among them -- and only then throws. Same lesson as `map` and `slice`
/// building their result before the walk: **a check moved earlier is a different program.**
pub(crate) fn to_property_descriptor(
    interpreter: &mut Interpreter,
    descriptor: &JsValue,
) -> Result<RawDescriptor, Completion> {
    let JsValue::Object(id) = descriptor else {
        return Err(interpreter.type_error("a property descriptor must be an object"));
    };
    let id = *id;
    let mut raw = RawDescriptor {
        present: PresentFields::default(),
        enumerable: false,
        configurable: false,
        value: JsValue::Undefined,
        writable: false,
        get: None,
        set: None,
    };

    let read = |interpreter: &mut Interpreter, name: &str| -> Result<Option<JsValue>, Completion> {
        let key = PropertyKey::from_str(name);
        if !interpreter.has_property(id, &key)? {
            return Ok(None);
        }
        match interpreter.get_property(id, &key) {
            Completion::Normal(value) => Ok(Some(value)),
            abrupt => Err(abrupt),
        }
    };

    if let Some(value) = read(interpreter, "enumerable")? {
        raw.present.enumerable = true;
        raw.enumerable = ops::to_boolean(&value);
    }
    if let Some(value) = read(interpreter, "configurable")? {
        raw.present.configurable = true;
        raw.configurable = ops::to_boolean(&value);
    }
    if let Some(value) = read(interpreter, "value")? {
        raw.present.value = true;
        raw.value = value;
    }
    if let Some(value) = read(interpreter, "writable")? {
        raw.present.writable = true;
        raw.writable = ops::to_boolean(&value);
    }
    let accessor = |interpreter: &mut Interpreter,
                    value: JsValue|
     -> Result<Option<ObjectId>, Completion> {
        match value {
            JsValue::Undefined => Ok(None),
            JsValue::Object(function) if interpreter.is_callable(&JsValue::Object(function)) => {
                Ok(Some(function))
            }
            _ => Err(interpreter
                .type_error("an accessor descriptor's get and set must be callable")),
        }
    };
    if let Some(value) = read(interpreter, "get")? {
        raw.present.get = true;
        raw.get = accessor(interpreter, value)?;
    }
    if let Some(value) = read(interpreter, "set")? {
        raw.present.set = true;
        raw.set = accessor(interpreter, value)?;
    }

    if (raw.present.get || raw.present.set) && (raw.present.value || raw.present.writable) {
        return Err(interpreter
            .type_error("a descriptor may not have both an accessor and a value or writable"));
    }
    Ok(raw)
}

/// `DefinePropertyOrThrow` (7.3.4): the form that turns a refusal into a TypeError.
///
/// `Object.defineProperty` throws and `Reflect.defineProperty` answers `false`, from the same
/// algorithm. Keeping the reason as a string and choosing here is what lets them stay one rule --
/// two implementations of "may this property be redefined?" is the shape where a new case lands in
/// only one of them.
fn define_property_or_throw(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: PropertyKey,
    descriptor: &RawDescriptor,
) -> Completion {
    match define_own_property(interpreter, id, key, descriptor) {
        Ok(None) => Completion::Normal(JsValue::Object(id)),
        Ok(Some(why)) => interpreter.type_error(&why),
        Err(abrupt) => abrupt,
    }
}

/// `[[DefineOwnProperty]]` over an ordinary or array-exotic object.
///
/// `Ok(None)` defined it; `Ok(Some(reason))` is the standard's `false`, with the text a throwing
/// caller reports; `Err` is a genuinely abrupt completion -- an invalid array length, a coercion
/// that threw.
///
/// AN OMITTED FIELD DEFAULTS TO `false` ON A NEW PROPERTY AND TO THE EXISTING VALUE ON AN OLD
/// ONE. Defaulting to `true` -- the ordinary-assignment attributes -- makes
/// `Object.defineProperty(o, "x", {value: 1})` create a writable, enumerable, configurable property
/// where the standard creates a frozen one.
pub(crate) fn define_own_property(
    interpreter: &mut Interpreter,
    id: ObjectId,
    key: PropertyKey,
    descriptor: &RawDescriptor,
) -> Result<Option<String>, Completion> {
    if interpreter.object(id).proxy.is_some() {
        return crate::proxy::define_own_property(interpreter, id, key, descriptor);
    }
    let is_array_length =
        interpreter.object(id).is_array && key == PropertyKey::from_str("length");
    let mut descriptor = descriptor.clone();
    if is_array_length && descriptor.present.value {
        descriptor.value = match interpreter.canonical_array_length(&descriptor.value) {
            Ok(length) => JsValue::Number(length),
            Err(abrupt) => return Err(abrupt),
        };
    }
    let descriptor = &descriptor;

    if let Some(view) = interpreter.typed_array_view(id) {
        if let Some(index) = crate::typed_array::canonical_numeric_index(&key) {
            let valid = !matches!(
                crate::typed_array::get_element(interpreter, &view, index),
                JsValue::Undefined
            );
            if !valid {
                return Ok(Some("that index is not in this typed array".into()));
            }
            let present = descriptor.present;
            if (present.configurable && !descriptor.configurable)
                || (present.enumerable && !descriptor.enumerable)
                || present.get
                || present.set
                || (present.writable && !descriptor.writable)
            {
                return Ok(Some("a typed array's element cannot take those attributes".into()));
            }
            if present.value {
                let value = descriptor.value.clone();
                if let Err(abrupt) =
                    crate::typed_array::set_element(interpreter, &view, index, &value)
                {
                    return Err(abrupt);
                }
            }
            return Ok(None);
        }
    }

    let mapped = interpreter.parameter_alias(id, &key);
    let mut descriptor = descriptor.clone();
    if let Some((scope, name)) = &mapped {
        let data = !descriptor.present.get && !descriptor.present.set;
        if data && !descriptor.present.value && descriptor.present.writable && !descriptor.writable {
            descriptor.value = interpreter.read_parameter_alias(scope, name);
            descriptor.present.value = true;
        }
    }
    let descriptor = &descriptor;

    let existing = interpreter.object(id).own(&key).cloned();
    let present = descriptor.present;
    let proposed = merge_descriptor(existing.as_ref(), descriptor);

    let array_index = if interpreter.object(id).is_array { key.as_array_index() } else { None };
    if let Some(index) = array_index {
        let length_key = PropertyKey::from_str("length");
        let refuses = match interpreter.object(id).own(&length_key) {
            Some(length) if !length.is_writable() => match length.data_value() {
                Some(JsValue::Number(current)) => f64::from(index) >= *current,
                _ => true,
            },
            _ => false,
        };
        if refuses {
            return Ok(Some("the array's `length` is not writable".into()));
        }
    }

    if let Err(why) = validate_redefinition(existing.as_ref(), &proposed, present) {
        return Ok(Some(why.into()));
    }
    if existing.is_none() && !interpreter.object(id).extensible {
        return Ok(Some("cannot add a property to a non-extensible object".into()));
    }
    if is_array_length {
        if let (true, Some(JsValue::Number(length))) = (present.value, proposed.data_value().cloned())
        {
            let refusal = interpreter.define_array_length(id, length);
            let mut length_property = interpreter
                .object(id)
                .own(&key)
                .cloned()
                .unwrap_or_else(|| Property::builtin(JsValue::Number(length)));
            length_property.enumerable = proposed.enumerable;
            length_property.configurable = proposed.configurable;
            if let (PropertyKind::Data { writable, .. }, false) =
                (&mut length_property.kind, proposed.is_writable())
            {
                if present.writable {
                    *writable = false;
                }
            }
            interpreter.object_mut(id).set_own(key, length_property);
            return Ok(refusal);
        }
    }
    if let Some((scope, name)) = &mapped {
        if present.get || present.set {
            interpreter.unmap_parameter(id, &key);
        } else {
            if present.value {
                let value = descriptor.value.clone();
                interpreter.write_parameter_alias(scope, name, value);
            }
            if present.writable && !descriptor.writable {
                interpreter.unmap_parameter(id, &key);
            }
        }
    }
    interpreter.object_mut(id).set_own(key, proposed);
    if let Some(index) = array_index {
        interpreter.grow_array_length_past(id, index);
    }
    Ok(None)
}

/// `ValidateAndApplyPropertyDescriptor`, the part that decides whether a redefinition is allowed.
///
/// **A NON-CONFIGURABLE PROPERTY IS NOT FROZEN, IT IS NARROWED**, and the exact set of changes
/// that remain legal is the whole substance of this function. A non-configurable, WRITABLE data
/// property may still have its value changed and may still be made non-writable -- one way only.
/// Refusing every change to a non-configurable property is the obvious reading and rejects legal
/// programs; allowing them all is what this engine did and accepts forbidden ones.
fn validate_redefinition(
    existing: Option<&Property>,
    proposed: &Property,
    present: PresentFields,
) -> Result<(), &'static str> {
    let Some(existing) = existing else { return Ok(()) };
    if existing.configurable {
        return Ok(());
    }
    if present.configurable && proposed.configurable {
        return Err("cannot make a non-configurable property configurable");
    }
    if present.enumerable && proposed.enumerable != existing.enumerable {
        return Err("cannot change the enumerability of a non-configurable property");
    }
    match (&existing.kind, &proposed.kind) {
        (PropertyKind::Data { .. }, PropertyKind::Accessor { .. })
            if present.get || present.set =>
        {
            Err("cannot turn a non-configurable data property into an accessor")
        }
        (PropertyKind::Accessor { .. }, PropertyKind::Data { .. })
            if present.value || present.writable =>
        {
            Err("cannot turn a non-configurable accessor into a data property")
        }
        (
            PropertyKind::Accessor { get, set },
            PropertyKind::Accessor { get: new_get, set: new_set },
        ) => {
            if present.get && new_get != get {
                return Err("cannot change the getter of a non-configurable accessor");
            }
            if present.set && new_set != set {
                return Err("cannot change the setter of a non-configurable accessor");
            }
            Ok(())
        }
        (
            PropertyKind::Data { value, writable: false },
            PropertyKind::Data { value: new_value, writable: new_writable },
        ) => {
            if present.writable && *new_writable {
                return Err("cannot make a non-writable property writable");
            }
            if present.value && !same_value(new_value, value) {
                return Err("cannot change the value of a non-writable property");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `Object.defineProperties`: every enumerable own key of the descriptor map, in order.
fn define_properties(
    interpreter: &mut Interpreter,
    target: &JsValue,
    descriptors: &JsValue,
) -> Completion {
    let source = match interpreter.to_object(descriptors) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let keys = ok_or_abrupt!(interpreter.own_keys_of(source));
    let mut pending = Vec::new();
    for key in keys {
        let existing = ok_or_abrupt!(interpreter.own_property(source, &key));
        let Some(existing) = existing else { continue };
        if !existing.enumerable {
            continue;
        }
        let descriptor = match interpreter.get_property(source, &key) {
            Completion::Normal(descriptor) => descriptor,
            abrupt => return abrupt,
        };
        match to_property_descriptor(interpreter, &descriptor) {
            Ok(descriptor) => pending.push((key, descriptor)),
            Err(abrupt) => return abrupt,
        }
    }
    let JsValue::Object(id) = target else {
        return interpreter.type_error("Object.defineProperties called on a non-object");
    };
    for (key, descriptor) in pending {
        let completion = define_property_or_throw(interpreter, *id, key, &descriptor);
        if completion.is_abrupt() {
            return completion;
        }
    }
    Completion::Normal(JsValue::Undefined)
}

/// `preventExtensions`/`seal`/`freeze` and the three questions that read them back.
///
/// **THEY ARE THREE DIFFERENT PROPERTIES OF AN OBJECT, NOT THREE NAMES FOR ONE.**
/// `preventExtensions` stops ADDITION; `seal` also makes every existing property non-configurable;
/// `freeze` also makes every data property non-writable. And the predicates are not the actions
/// read back: `Object.isFrozen({})` on a non-extensible EMPTY object is **true**, because there is
/// nothing left that could change. Collapsing any two of these gets a whole family of tests wrong
/// in a way that looks like an off-by-one.
const EXTENSIBILITY: [(&str, crate::interpreter::NativeFn); 6] = [
    ("preventExtensions", |interpreter, _this, arguments| {
        integrity(interpreter, &arg(arguments, 0), Integrity::PreventExtensions)
    }),
    ("seal", |interpreter, _this, arguments| {
        integrity(interpreter, &arg(arguments, 0), Integrity::Seal)
    }),
    ("freeze", |interpreter, _this, arguments| {
        integrity(interpreter, &arg(arguments, 0), Integrity::Freeze)
    }),
    ("isExtensible", |interpreter, _this, arguments| {
        let JsValue::Object(id) = arg(arguments, 0) else {
            return Completion::Normal(JsValue::Boolean(false));
        };
        Completion::Normal(JsValue::Boolean(ok_or_abrupt!(interpreter.is_extensible(id))))
    }),
    ("isSealed", |interpreter, _this, arguments| {
        tests_integrity(interpreter, &arg(arguments, 0), false)
    }),
    ("isFrozen", |interpreter, _this, arguments| {
        tests_integrity(interpreter, &arg(arguments, 0), true)
    }),
];

enum Integrity {
    PreventExtensions,
    Seal,
    Freeze,
}

fn integrity(interpreter: &mut Interpreter, value: &JsValue, level: Integrity) -> Completion {
    let JsValue::Object(id) = value else {
        return Completion::Normal(value.clone());
    };
    if !ok_or_abrupt!(interpreter.prevent_extensions(*id)) {
        return interpreter.type_error("this object refused to become non-extensible");
    }
    if !matches!(level, Integrity::PreventExtensions) {
        for key in ok_or_abrupt!(interpreter.own_keys_of(*id)) {
            let mut descriptor = RawDescriptor::empty();
            descriptor.present.configurable = true;
            if matches!(level, Integrity::Freeze) {
                let existing = ok_or_abrupt!(interpreter.own_property(*id, &key));
                let Some(existing) = existing else { continue };
                if !existing.is_accessor() {
                    descriptor.present.writable = true;
                }
            }
            let completion = define_property_or_throw(interpreter, *id, key, &descriptor);
            if completion.is_abrupt() {
                return completion;
            }
        }
    }
    Completion::Normal(value.clone())
}

fn tests_integrity(interpreter: &mut Interpreter, value: &JsValue, frozen: bool) -> Completion {
    let JsValue::Object(id) = value else {
        return Completion::Normal(JsValue::Boolean(true));
    };
    if ok_or_abrupt!(interpreter.is_extensible(*id)) {
        return Completion::Normal(JsValue::Boolean(false));
    }
    if let Some(view) = interpreter.typed_array_view(*id) {
        if view.length > 0 && !interpreter.buffer_is_detached(view.buffer) {
            return Completion::Normal(JsValue::Boolean(false));
        }
    }
    for key in ok_or_abrupt!(interpreter.own_keys_of(*id)) {
        let Some(property) = ok_or_abrupt!(interpreter.own_property(*id, &key)) else { continue };
        if property.configurable {
            return Completion::Normal(JsValue::Boolean(false));
        }
        if frozen && !property.is_accessor() && property.is_writable() {
            return Completion::Normal(JsValue::Boolean(false));
        }
    }
    Completion::Normal(JsValue::Boolean(true))
}


