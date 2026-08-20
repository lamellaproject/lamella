//! `RegExp`, its prototype, and the `lastIndex` protocol.

use lamella_regexp::js::{compile_pattern, Compiled, Flags, Search};
use lamella_regexp::Fuel;

use crate::interpreter::{Completion, Interpreter};
use crate::object::{Object, Property, PropertyKey, PropertyKind};
use crate::string_value::JsString;
use crate::value::{JsValue, ObjectId};
use crate::{String, Vec};

/// What a `RegExp` object holds that a program can neither read, write nor forge.
#[derive(Debug, Clone)]
pub struct RegExpData {
    pub compiled: Compiled,
    /// The pattern text as `source` reports it, which is the ESCAPED form rather than the original.
    pub source: JsString,
    pub flags: String,
}

/// The budget a host match runs under.
///
/// The host tier is unbounded so that conformance measures the standard rather than this engine's
/// patience. A device profile is expected to bound it, and the knob for that is owed rather than
/// present -- so a device build today inherits the host's answer, which is the honest statement of
/// where this stands.
const HOST_FUEL: Fuel = Fuel::UNLIMITED;

pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.regexp_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "RegExp",
        2,
        |interpreter, _this, arguments| construct(interpreter, arguments),
        |interpreter, _this, arguments| construct(interpreter, arguments),
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("RegExp", JsValue::Object(constructor));
    interpreter.intrinsics.regexp_constructor = constructor;

    interpreter.define_method(prototype, "exec", 1, |interpreter, this, arguments| {
        let subject = match string_argument(interpreter, arguments) {
            Ok(subject) => subject,
            Err(abrupt) => return abrupt,
        };
        exec(interpreter, &this, &subject)
    });

    interpreter.define_method(prototype, "test", 1, |interpreter, this, arguments| {
        let subject = match string_argument(interpreter, arguments) {
            Ok(subject) => subject,
            Err(abrupt) => return abrupt,
        };
        match exec(interpreter, &this, &subject) {
            Completion::Normal(JsValue::Null) => Completion::Normal(JsValue::Boolean(false)),
            Completion::Normal(_) => Completion::Normal(JsValue::Boolean(true)),
            other => other,
        }
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        let JsValue::Object(id) = this else {
            return interpreter.type_error("RegExp.prototype.toString requires an object");
        };
        let source = match interpreter.get_property(id, &PropertyKey::from_str("source")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        let flags = match interpreter.get_property(id, &PropertyKey::from_str("flags")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        let mut text = JsString::from("/");
        match interpreter.to_string_value(&source) {
            Ok(value) => text.extend_from(&value),
            Err(abrupt) => return abrupt,
        }
        text.push_str("/");
        match interpreter.to_string_value(&flags) {
            Ok(value) => text.extend_from(&value),
            Err(abrupt) => return abrupt,
        }
        Completion::Normal(JsValue::String(text))
    });

    install_flag_accessors(interpreter, prototype);
    install_protocol_methods(interpreter, prototype);
    install_split(interpreter, prototype);
    install_match_all(interpreter, prototype);
}

/// The eight flag accessors and `source`, each an accessor rather than a data property.
///
/// They are separate functions because a built-in is a bare function pointer and cannot capture
/// which flag it was installed for. The alternative -- one function reading its own name -- would
/// make the name load-bearing, which is worse.
fn install_flag_accessors(interpreter: &mut Interpreter, prototype: ObjectId) {
    fn accessor(
        interpreter: &mut Interpreter,
        prototype: ObjectId,
        name: &'static str,
        getter_name: &'static str,
        get: crate::interpreter::NativeFn,
    ) {
        let getter = interpreter.native_function(getter_name, 0, get);
        interpreter.object_mut(prototype).set_own(
            PropertyKey::from_str(name),
            Property {
                kind: PropertyKind::Accessor { get: Some(getter), set: None },
                enumerable: false,
                configurable: true,
            },
        );
    }

    accessor(interpreter, prototype, "source", "get source", |interpreter, this, _| {
        match data(interpreter, &this) {
            Some(data) => Completion::Normal(JsValue::String(data.source.clone())),
            None if is_regexp_prototype(interpreter, &this) => {
                Completion::Normal(JsValue::string("(?:)"))
            }
            None => interpreter.type_error("RegExp.prototype.source requires a regular expression"),
        }
    });

    accessor(interpreter, prototype, "flags", "get flags", |interpreter, this, _| {
        match data(interpreter, &this) {
            Some(data) => Completion::Normal(JsValue::String(JsString::from(data.flags.as_str()))),
            None if is_regexp_prototype(interpreter, &this) => {
                Completion::Normal(JsValue::string(""))
            }
            None => interpreter.type_error("RegExp.prototype.flags requires a regular expression"),
        }
    });

    accessor(interpreter, prototype, "hasIndices", "get hasIndices", |i, t, _| flag(i, &t, 'd'));
    accessor(interpreter, prototype, "global", "get global", |i, t, _| flag(i, &t, 'g'));
    accessor(interpreter, prototype, "ignoreCase", "get ignoreCase", |i, t, _| flag(i, &t, 'i'));
    accessor(interpreter, prototype, "multiline", "get multiline", |i, t, _| flag(i, &t, 'm'));
    accessor(interpreter, prototype, "dotAll", "get dotAll", |i, t, _| flag(i, &t, 's'));
    accessor(interpreter, prototype, "unicode", "get unicode", |i, t, _| flag(i, &t, 'u'));
    accessor(interpreter, prototype, "unicodeSets", "get unicodeSets", |i, t, _| flag(i, &t, 'v'));
    accessor(interpreter, prototype, "sticky", "get sticky", |i, t, _| flag(i, &t, 'y'));
}

/// One flag's accessor. On `RegExp.prototype` itself the answer is `undefined` rather than a
/// TypeError, which is the standard distinguishing "the prototype, which has no pattern" from
/// "some other object, which has no business answering".
fn flag(interpreter: &mut Interpreter, this: &JsValue, letter: char) -> Completion {
    match data(interpreter, this) {
        Some(data) => Completion::Normal(JsValue::Boolean(data.flags.contains(letter))),
        None if is_regexp_prototype(interpreter, this) => {
            Completion::Normal(JsValue::Undefined)
        }
        None => interpreter.type_error("a RegExp flag accessor requires a regular expression"),
    }
}

fn is_regexp_prototype(interpreter: &Interpreter, this: &JsValue) -> bool {
    matches!(this, JsValue::Object(id) if *id == interpreter.intrinsics.regexp_prototype)
}

/// The compiled pattern a RegExp object holds, or `None` when the value is not one.
fn data(interpreter: &Interpreter, this: &JsValue) -> Option<RegExpData> {
    match this {
        JsValue::Object(id) => interpreter.object(*id).regexp.as_deref().cloned(),
        _ => None,
    }
}

/// `new RegExp(pattern, flags)`.
fn construct(interpreter: &mut Interpreter, arguments: &[JsValue]) -> Completion {
    let pattern_argument = arguments.first().cloned().unwrap_or(JsValue::Undefined);
    let flags_argument = arguments.get(1).cloned().unwrap_or(JsValue::Undefined);

    let (source, flags_text) = match (&pattern_argument, data(interpreter, &pattern_argument)) {
        (_, Some(existing)) => {
            let flags = match &flags_argument {
                JsValue::Undefined => JsString::from(existing.flags.as_str()),
                other => match interpreter.to_string_value(other) {
                    Ok(value) => value,
                    Err(abrupt) => return abrupt,
                },
            };
            (existing.source.clone(), flags)
        }
        _ => {
            let source = match &pattern_argument {
                JsValue::Undefined => JsString::new(),
                other => match interpreter.to_string_value(other) {
                    Ok(value) => value,
                    Err(abrupt) => return abrupt,
                },
            };
            let flags = match &flags_argument {
                JsValue::Undefined => JsString::new(),
                other => match interpreter.to_string_value(other) {
                    Ok(value) => value,
                    Err(abrupt) => return abrupt,
                },
            };
            (source, flags)
        }
    };

    match create(interpreter, &source, &flags_text) {
        Ok(id) => Completion::Normal(JsValue::Object(id)),
        Err(abrupt) => abrupt,
    }
}

/// Builds a RegExp object, or an abrupt completion carrying the SyntaxError the standard requires.
///
/// A malformed pattern is a SyntaxError THROWN AT RUN TIME here, not a parse error, because
/// `new RegExp(s)` compiles a string that did not exist when the program was parsed. A literal is
/// different and is checked earlier; both end up in this function so there is one compiler.
pub(crate) fn create(
    interpreter: &mut Interpreter,
    source: &JsString,
    flags_text: &JsString,
) -> Result<ObjectId, Completion> {
    let flags_string = flags_text.to_lossy_string();
    let flags = match Flags::parse(&flags_string) {
        Ok(flags) => flags,
        Err(error) => return Err(interpreter.throw("SyntaxError", &error.kind.message())),
    };

    let source_string = source.to_lossy_string();
    let compiled = match compile_pattern(&source_string, flags) {
        Ok(compiled) => compiled,
        Err(error) => return Err(interpreter.throw("SyntaxError", &error.kind.message())),
    };

    let prototype = interpreter.intrinsics.regexp_prototype;
    let id = interpreter.allocate(Object::new(Some(prototype)));
    interpreter.object_mut(id).regexp = Some(crate::Box::new(RegExpData {
        compiled,
        source: escaped_source(source),
        flags: flags_string,
    }));

    interpreter.object_mut(id).set_own(
        PropertyKey::from_str("lastIndex"),
        Property {
            kind: PropertyKind::Data { value: JsValue::Number(0.0), writable: true },
            enumerable: false,
            configurable: false,
        },
    );

    Ok(id)
}

/// What `source` reports, which is not the text that was compiled.
///
/// The standard requires the result to be a pattern that round-trips through `eval` as the same
/// regular expression, so an empty pattern reports `(?:)` and a `/` inside the pattern is escaped.
/// Reporting the raw text would produce `//`, which is a line comment rather than a regular
/// expression.
fn escaped_source(source: &JsString) -> JsString {
    if source.is_empty() {
        return JsString::from("(?:)");
    }

    let mut out = JsString::new();
    let units = source.units();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        match unit {
            0x2F => out.push_str("\\/"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x2028 => out.push_str("\\u2028"),
            0x2029 => out.push_str("\\u2029"),
            0x5C => {
                out.push_code_unit(0x5C);
                index += 1;
                if let Some(next) = units.get(index) {
                    out.push_code_unit(*next);
                }
            }
            other => out.push_code_unit(other),
        }
        index += 1;
    }
    out
}

fn string_argument(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
) -> Result<JsString, Completion> {
    let value = arguments.first().cloned().unwrap_or(JsValue::Undefined);
    interpreter.to_string_value(&value)
}

/// `RegExpBuiltinExec`: the search, and the `lastIndex` bookkeeping around it.
pub(crate) fn exec(
    interpreter: &mut Interpreter,
    this: &JsValue,
    subject: &JsString,
) -> Completion {
    let JsValue::Object(id) = this else {
        return interpreter.type_error("RegExp.prototype.exec requires a regular expression");
    };
    let id = *id;
    let Some(data) = interpreter.object(id).regexp.as_deref().cloned() else {
        return interpreter.type_error("RegExp.prototype.exec requires a regular expression");
    };

    let global = data.compiled.flags.global;
    let sticky = data.compiled.flags.sticky;

    let start = if global || sticky {
        let value = match interpreter.get_property(id, &PropertyKey::from_str("lastIndex")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        match interpreter.to_number_value(&value) {
            Ok(number) => to_length(number),
            Err(abrupt) => return abrupt,
        }
    } else {
        0
    };

    let units = subject.units();
    let outcome = if start > units.len() {
        Search::NotFound
    } else {
        data.compiled.find(units, start, HOST_FUEL)
    };

    let slots = match outcome {
        Search::Found(slots) => slots,
        Search::NotFound => {
            if global || sticky {
                if let Completion::Throw(error) = interpreter.set_property_or_throw(
                    id,
                    PropertyKey::from_str("lastIndex"),
                    JsValue::Number(0.0),
                ) {
                    return Completion::Throw(error);
                }
            }
            return Completion::Normal(JsValue::Null);
        }
        Search::Fuel => {
            return interpreter
                .throw("RangeError", "this regular expression exceeded the match step budget")
        }
        Search::TooDeep => {
            return interpreter
                .throw("SyntaxError", "this regular expression nests lookarounds too deeply")
        }
    };

    let (Some(match_start), Some(match_end)) = (slots[0], slots[1]) else {
        return interpreter.host_error("a match reported no span");
    };

    if global || sticky {
        if let Completion::Throw(error) = interpreter.set_property_or_throw(
            id,
            PropertyKey::from_str("lastIndex"),
            JsValue::Number(match_end as f64),
        ) {
            return Completion::Throw(error);
        }
    }

    Completion::Normal(JsValue::Object(build_result(
        interpreter,
        &data,
        subject,
        &slots,
        match_start,
        match_end,
    )))
}

/// The array `exec` answers, with the `index`, `input` and `groups` properties on it.
fn build_result(
    interpreter: &mut Interpreter,
    data: &RegExpData,
    subject: &JsString,
    slots: &[Option<usize>],
    match_start: usize,
    match_end: usize,
) -> ObjectId {
    let units = subject.units();
    let mut elements = Vec::new();
    elements.push(JsValue::String(JsString::from_units(&units[match_start..match_end])));

    for group in 1..=data.compiled.groups as usize {
        let value = match (slots.get(group * 2).copied().flatten(), slots.get(group * 2 + 1).copied().flatten())
        {
            (Some(start), Some(end)) => {
                JsValue::String(JsString::from_units(&units[start..end]))
            }
            _ => JsValue::Undefined,
        };
        elements.push(value);
    }

    let array = interpreter.new_array(elements);
    set_own(interpreter, array, "index", JsValue::Number(match_start as f64));
    set_own(interpreter, array, "input", JsValue::String(subject.clone()));

    let groups = if data.compiled.names.is_empty() {
        JsValue::Undefined
    } else {
        let holder = interpreter.allocate(Object::new(None));
        for (name, index) in &data.compiled.names {
            let value = match (
                slots.get(*index as usize * 2).copied().flatten(),
                slots.get(*index as usize * 2 + 1).copied().flatten(),
            ) {
                (Some(start), Some(end)) => {
                    JsValue::String(JsString::from_units(&units[start..end]))
                }
                _ => JsValue::Undefined,
            };
            set_own(interpreter, holder, name.as_str(), value);
        }
        JsValue::Object(holder)
    };
    set_own(interpreter, array, "groups", groups);

    array
}

/// An ordinary own data property: writable, enumerable and configurable, which is what the result
/// array's own fields are.
fn set_own(interpreter: &mut Interpreter, id: ObjectId, name: &str, value: JsValue) {
    interpreter
        .object_mut(id)
        .set_own(PropertyKey::from_str(name), Property::data(value));
}

/// `ToLength`: clamped to a non-negative integer no larger than the maximum array index.
fn to_length(number: f64) -> usize {
    if number.is_nan() || number <= 0.0 {
        return 0;
    }
    let clamped = crate::math::trunc(number).min(9_007_199_254_740_991.0);
    clamped as usize
}

/// `SameValue` over the shapes a cursor takes, which is all this file compares.
///
/// It is not `==`: a cursor a program set to a string is a different value from the number it
/// coerces to, and the restore has to notice that it changed.
fn same_value(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Number(left), JsValue::Number(right)) => {
            left == right || (left.is_nan() && right.is_nan())
        }
        (JsValue::Undefined, JsValue::Undefined) | (JsValue::Null, JsValue::Null) => true,
        (JsValue::String(left), JsValue::String(right)) => left == right,
        (JsValue::Boolean(left), JsValue::Boolean(right)) => left == right,
        _ => false,
    }
}

/// `RegExpExec`: the extensibility hook the whole `Symbol.*` family goes through.
///
/// A subclass may override `exec`, and every operation that searches -- `@@match`, `@@search`,
/// `@@replace`, `@@split` -- is specified to consult that override rather than the built-in.
/// Calling [`exec`] directly from those would make the override invisible, which is the same class
/// of defect as a well-known symbol that is defined and never read.
///
/// A user `exec` that answers something that is neither an object nor null is a TypeError HERE,
/// where it can be attributed, rather than a confusing failure later when a caller reads `index`
/// off a number.
pub(crate) fn regexp_exec(
    interpreter: &mut Interpreter,
    receiver: &JsValue,
    subject: &JsString,
) -> Completion {
    let JsValue::Object(id) = receiver else {
        return interpreter.type_error("a regular expression operation requires an object");
    };
    let hook = match interpreter.get_property(*id, &PropertyKey::from_str("exec")) {
        Completion::Normal(value) => value,
        abrupt => return abrupt,
    };
    if interpreter.is_callable(&hook) {
        let result = interpreter.call_value(
            &hook,
            receiver.clone(),
            crate::vec![JsValue::String(subject.clone())],
        );
        return match result {
            Completion::Normal(value) => match value {
                JsValue::Object(_) | JsValue::Null => Completion::Normal(value),
                _ => interpreter.type_error("a regular expression `exec` answered a non-object"),
            },
            abrupt => abrupt,
        };
    }
    exec(interpreter, receiver, subject)
}

/// Reads a property and coerces it to a string, which the protocol methods do constantly.
fn get_string(
    interpreter: &mut Interpreter,
    id: ObjectId,
    name: &str,
) -> Result<JsString, Completion> {
    match interpreter.get_property(id, &PropertyKey::from_str(name)) {
        Completion::Normal(value) => interpreter.to_string_value(&value),
        abrupt => Err(abrupt),
    }
}

/// How far a failed or empty match moves on, which is one CHARACTER in the mode's own unit.
///
/// `AdvanceStringIndex`. Under `u` a zero-length match inside a surrogate pair must step past the
/// whole pair, or a global search over an astral character reports a match between its halves.
fn advance(subject: &JsString, index: usize, code_point_mode: bool) -> usize {
    if !code_point_mode {
        return index + 1;
    }
    let units = subject.units();
    match units.get(index) {
        Some(&unit) if (0xD800..0xDC00).contains(&unit) => match units.get(index + 1) {
            Some(&next) if (0xDC00..0xE000).contains(&next) => index + 2,
            _ => index + 1,
        },
        _ => index + 1,
    }
}

/// The flags string as a PROPERTY read, because a subclass may override the getter.
fn flags_of(interpreter: &mut Interpreter, id: ObjectId) -> Result<String, Completion> {
    Ok(get_string(interpreter, id, "flags")?.to_lossy_string())
}

pub(crate) fn install_protocol_methods(interpreter: &mut Interpreter, prototype: ObjectId) {
    let matcher = interpreter.native_function("[Symbol.match]", 1, |interpreter, this, arguments| {
        let JsValue::Object(id) = this else {
            return interpreter.type_error("Symbol.match requires an object");
        };
        let subject = match string_argument(interpreter, arguments) {
            Ok(subject) => subject,
            Err(abrupt) => return abrupt,
        };
        let flags = match flags_of(interpreter, id) {
            Ok(flags) => flags,
            Err(abrupt) => return abrupt,
        };

        if !flags.contains('g') {
            return regexp_exec(interpreter, &JsValue::Object(id), &subject);
        }

        let code_point_mode = flags.contains('u') || flags.contains('v');
        if let Completion::Throw(error) = interpreter.set_property_or_throw(
            id,
            PropertyKey::from_str("lastIndex"),
            JsValue::Number(0.0),
        ) {
            return Completion::Throw(error);
        }

        let mut found: Vec<JsValue> = Vec::new();
        loop {
            let result = match regexp_exec(interpreter, &JsValue::Object(id), &subject) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            let JsValue::Object(result) = result else {
                return if found.is_empty() {
                    Completion::Normal(JsValue::Null)
                } else {
                    Completion::Normal(JsValue::Object(interpreter.new_array(found)))
                };
            };

            let matched = match get_string(interpreter, result, "0") {
                Ok(text) => text,
                Err(abrupt) => return abrupt,
            };
            let empty = matched.is_empty();
            found.push(JsValue::String(matched));

            if empty {
                let at = match interpreter.get_property(id, &PropertyKey::from_str("lastIndex")) {
                    Completion::Normal(value) => match interpreter.to_number_value(&value) {
                        Ok(number) => to_length(number),
                        Err(abrupt) => return abrupt,
                    },
                    abrupt => return abrupt,
                };
                if let Completion::Throw(error) = interpreter.set_property_or_throw(
                    id,
                    PropertyKey::from_str("lastIndex"),
                    JsValue::Number(advance(&subject, at, code_point_mode) as f64),
                ) {
                    return Completion::Throw(error);
                }
            }
        }
    });
    interpreter.define_well_known_symbol_method(prototype, "match", matcher);

    let search = interpreter.native_function("[Symbol.search]", 1, |interpreter, this, arguments| {
        let JsValue::Object(id) = this else {
            return interpreter.type_error("Symbol.search requires an object");
        };
        let subject = match string_argument(interpreter, arguments) {
            Ok(subject) => subject,
            Err(abrupt) => return abrupt,
        };

        let previous = match interpreter.get_property(id, &PropertyKey::from_str("lastIndex")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        let was_zero = matches!(&previous, JsValue::Number(number) if *number == 0.0);
        if !was_zero {
            if let Completion::Throw(error) = interpreter.set_property_or_throw(
                id,
                PropertyKey::from_str("lastIndex"),
                JsValue::Number(0.0),
            ) {
                return Completion::Throw(error);
            }
        }

        let result = match regexp_exec(interpreter, &JsValue::Object(id), &subject) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };

        let current = match interpreter.get_property(id, &PropertyKey::from_str("lastIndex")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        if !same_value(&current, &previous) {
            if let Completion::Throw(error) = interpreter.set_property_or_throw(
                id,
                PropertyKey::from_str("lastIndex"),
                previous,
            ) {
                return Completion::Throw(error);
            }
        }

        match result {
            JsValue::Object(result) => {
                interpreter.get_property(result, &PropertyKey::from_str("index"))
            }
            _ => Completion::Normal(JsValue::Number(-1.0)),
        }
    });
    interpreter.define_well_known_symbol_method(prototype, "search", search);

    let replace =
        interpreter.native_function("[Symbol.replace]", 2, |interpreter, this, arguments| {
            let JsValue::Object(id) = this else {
                return interpreter.type_error("Symbol.replace requires an object");
            };
            let subject = match string_argument(interpreter, arguments) {
                Ok(subject) => subject,
                Err(abrupt) => return abrupt,
            };
            let replacement = arguments.get(1).cloned().unwrap_or(JsValue::Undefined);
            replace_all_matches(interpreter, id, &subject, replacement)
        });
    interpreter.define_well_known_symbol_method(prototype, "replace", replace);
}

/// The body of `@@replace`, out of line because it is long and the closure above is not the place
/// to read it.
fn replace_all_matches(
    interpreter: &mut Interpreter,
    id: ObjectId,
    subject: &JsString,
    replacement: JsValue,
) -> Completion {
    let functional = interpreter.is_callable(&replacement);

    let literal = if functional {
        None
    } else {
        match interpreter.to_string_value(&replacement) {
            Ok(text) => Some(text),
            Err(abrupt) => return abrupt,
        }
    };

    let flags = match flags_of(interpreter, id) {
        Ok(flags) => flags,
        Err(abrupt) => return abrupt,
    };
    let global = flags.contains('g');
    let code_point_mode = flags.contains('u') || flags.contains('v');

    if global {
        if let Completion::Throw(error) = interpreter.set_property_or_throw(
            id,
            PropertyKey::from_str("lastIndex"),
            JsValue::Number(0.0),
        ) {
            return Completion::Throw(error);
        }
    }

    let mut matches: Vec<ObjectId> = Vec::new();
    loop {
        let result = match regexp_exec(interpreter, &JsValue::Object(id), subject) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        let JsValue::Object(result) = result else { break };
        matches.push(result);
        if !global {
            break;
        }
        let matched = match get_string(interpreter, result, "0") {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        if matched.is_empty() {
            let at = match interpreter.get_property(id, &PropertyKey::from_str("lastIndex")) {
                Completion::Normal(value) => match interpreter.to_number_value(&value) {
                    Ok(number) => to_length(number),
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            };
            if let Completion::Throw(error) = interpreter.set_property_or_throw(
                id,
                PropertyKey::from_str("lastIndex"),
                JsValue::Number(advance(subject, at, code_point_mode) as f64),
            ) {
                return Completion::Throw(error);
            }
        }
    }

    let units = subject.units().to_vec();
    let mut out = JsString::new();
    let mut copied = 0usize;

    for result in matches {
        let length = match interpreter.get_property(result, &PropertyKey::from_str("length")) {
            Completion::Normal(value) => match interpreter.to_number_value(&value) {
                Ok(number) => to_length(number),
                Err(abrupt) => return abrupt,
            },
            abrupt => return abrupt,
        };
        let captures_count = length.saturating_sub(1);

        let matched = match get_string(interpreter, result, "0") {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };

        let position = match interpreter.get_property(result, &PropertyKey::from_str("index")) {
            Completion::Normal(value) => match interpreter.to_number_value(&value) {
                Ok(number) => to_length(number).min(units.len()),
                Err(abrupt) => return abrupt,
            },
            abrupt => return abrupt,
        };

        let mut captures: Vec<Option<JsString>> = Vec::new();
        for index in 1..=captures_count {
            let mut key = String::from("");
            key.push_str(&crate::format!("{index}"));
            let value = match interpreter.get_property(result, &PropertyKey::from_str(&key)) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            captures.push(match value {
                JsValue::Undefined => None,
                other => match interpreter.to_string_value(&other) {
                    Ok(text) => Some(text),
                    Err(abrupt) => return abrupt,
                },
            });
        }

        let named = match interpreter.get_property(result, &PropertyKey::from_str("groups")) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };

        let replaced = if functional {
            let mut call_arguments = crate::vec![JsValue::String(matched.clone())];
            for capture in &captures {
                call_arguments.push(match capture {
                    Some(text) => JsValue::String(text.clone()),
                    None => JsValue::Undefined,
                });
            }
            call_arguments.push(JsValue::Number(position as f64));
            call_arguments.push(JsValue::String(subject.clone()));
            if !matches!(named, JsValue::Undefined) {
                call_arguments.push(named.clone());
            }
            match interpreter.call_value(&replacement, JsValue::Undefined, call_arguments) {
                Completion::Normal(value) => match interpreter.to_string_value(&value) {
                    Ok(text) => text,
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            }
        } else {
            let literal = literal.clone().unwrap_or_default();
            match get_substitution(
                interpreter,
                &matched,
                subject,
                position,
                &captures,
                &named,
                &literal,
            ) {
                Ok(text) => text,
                Err(abrupt) => return abrupt,
            }
        };

        if position >= copied {
            out.extend_from(&JsString::from_units(&units[copied..position]));
            out.extend_from(&replaced);
            copied = position + matched.len();
        }
    }

    if copied < units.len() {
        out.extend_from(&JsString::from_units(&units[copied..]));
    }
    Completion::Normal(JsValue::String(out))
}

/// `GetSubstitution`: the `$` language inside a replacement string.
///
/// # ONE FUNCTION, THREE CALLERS, AND THE STRING FORM WAS MISSING IT ENTIRELY
///
/// `String.prototype.replace`, `String.prototype.replaceAll` and `RegExp.prototype[@@replace]` all
/// end here, and the standard routes all three through this operation. The two string forms were
/// not doing it at all: `"abc".replace("b", "[$&]")` answered `a[$&]c` where every conforming
/// engine answers `a[b]c`. That is a silent wrong answer on surface no absence list mentioned,
/// which is the category this profile exists to refuse -- and it was invisible because the string
/// form looks like plain text concatenation.
///
/// The string callers pass no captures and no named groups, which is not a special case: it is this
/// function with two empty inputs, and `$1` with no captures is then left alone exactly as the
/// standard says.
///
/// The recognized forms are `$$`, `$&`, `` $` ``, `$'`, `$n`, `$nn` and `$<name>`. **Anything else
/// after a `$` is kept verbatim**, including a `$` at the end of the string and a group number past
/// what the pattern has -- so a replacement containing a literal price stays a price.
pub(crate) fn get_substitution(
    interpreter: &mut Interpreter,
    matched: &JsString,
    subject: &JsString,
    position: usize,
    captures: &[Option<JsString>],
    named: &JsValue,
    template: &JsString,
) -> Result<JsString, Completion> {
    let template_units = template.units();
    let subject_units = subject.units();
    let tail = position + matched.len();

    let mut out = JsString::new();
    let mut index = 0usize;

    while index < template_units.len() {
        let unit = template_units[index];
        if unit != 0x24 {
            out.push_code_unit(unit);
            index += 1;
            continue;
        }

        let next = template_units.get(index + 1).copied();
        match next {
            Some(0x24) => {
                out.push_code_unit(0x24);
                index += 2;
            }
            Some(0x26) => {
                out.extend_from(matched);
                index += 2;
            }
            Some(0x60) => {
                out.extend_from(&JsString::from_units(&subject_units[..position]));
                index += 2;
            }
            Some(0x27) => {
                let from = tail.min(subject_units.len());
                out.extend_from(&JsString::from_units(&subject_units[from..]));
                index += 2;
            }
            Some(0x3C) => {
                match named {
                    JsValue::Undefined => {
                        out.push_code_unit(0x24);
                        index += 1;
                    }
                    _ => {
                        let close = template_units[index + 2..]
                            .iter()
                            .position(|&unit| unit == 0x3E)
                            .map(|offset| index + 2 + offset);
                        match close {
                            None => {
                                out.push_code_unit(0x24);
                                index += 1;
                            }
                            Some(close) => {
                                let name =
                                    JsString::from_units(&template_units[index + 2..close]);
                                let value = match named {
                                    JsValue::Object(holder) => match interpreter.get_property(
                                        *holder,
                                        &PropertyKey::from_str(&name.to_lossy_string()),
                                    ) {
                                        Completion::Normal(value) => value,
                                        abrupt => return Err(abrupt),
                                    },
                                    _ => JsValue::Undefined,
                                };
                                if !matches!(value, JsValue::Undefined) {
                                    let text = interpreter.to_string_value(&value)?;
                                    out.extend_from(&text);
                                }
                                index = close + 1;
                            }
                        }
                    }
                }
            }
            Some(digit) if (0x30..=0x39).contains(&digit) => {
                let first = (digit - 0x30) as usize;
                let second = template_units.get(index + 2).copied().filter(|unit| {
                    (0x30..=0x39).contains(unit)
                });

                let two_digit = second.map(|second| first * 10 + (second - 0x30) as usize);
                let (group, width) = match two_digit {
                    Some(value) if value >= 1 && value <= captures.len() => (value, 3),
                    _ if first >= 1 && first <= captures.len() => (first, 2),
                    _ => {
                        out.push_code_unit(0x24);
                        index += 1;
                        continue;
                    }
                };
                if let Some(Some(text)) = captures.get(group - 1) {
                    out.extend_from(text);
                }
                index += width;
            }
            _ => {
                out.push_code_unit(0x24);
                index += 1;
            }
        }
    }

    Ok(out)
}

/// `RegExp.prototype[@@split]`.
///
/// # IT SPLITS WITH A STICKY CLONE, NOT WITH THE PATTERN IT WAS GIVEN
///
/// The standard builds a second regular expression carrying the original's flags plus `y`, and
/// drives that. The reason is that splitting has to ask "does the separator start exactly here",
/// which is what sticky means, and reusing the caller's pattern would also trample the caller's
/// `lastIndex`. The clone goes through the SPECIES constructor, so a subclass gets its own type
/// back.
///
/// # THE CAPTURES OF THE SEPARATOR ARE PART OF THE RESULT
///
/// `"a1b".split(/(\d)/)` is `["a", "1", "b"]` -- the separator's groups are spliced in between the
/// pieces. An implementation that only emitted the pieces would be right for every pattern without
/// groups and quietly lossy for every pattern with them.
///
/// # AN EMPTY SUBJECT IS ITS OWN CASE, AND IT INVERTS
///
/// `"".split(/x/)` is `[""]` and `"".split(/(?:)/)` is `[]`. The empty string yields one empty piece
/// unless the separator matches it, in which case it yields nothing at all.
pub(crate) fn install_split(interpreter: &mut Interpreter, prototype: ObjectId) {
    let split = interpreter.native_function("[Symbol.split]", 2, |interpreter, this, arguments| {
        let JsValue::Object(id) = this else {
            return interpreter.type_error("Symbol.split requires an object");
        };
        let subject = match string_argument(interpreter, arguments) {
            Ok(subject) => subject,
            Err(abrupt) => return abrupt,
        };
        let limit = match arguments.get(1) {
            Some(JsValue::Undefined) | None => u32::MAX as usize,
            Some(value) => match interpreter.to_number_value(value) {
                Ok(number) => to_uint32(number) as usize,
                Err(abrupt) => return abrupt,
            },
        };

        let flags = match flags_of(interpreter, id) {
            Ok(flags) => flags,
            Err(abrupt) => return abrupt,
        };
        let code_point_mode = flags.contains('u') || flags.contains('v');
        let sticky_flags = if flags.contains('y') {
            flags.clone()
        } else {
            let mut text = flags.clone();
            text.push('y');
            text
        };

        let default = interpreter.intrinsics.regexp_constructor;
        let constructor = match interpreter.species_constructor(id, default) {
            Ok(constructor) => constructor,
            Err(abrupt) => return abrupt,
        };
        let splitter = match interpreter.construct(
            constructor,
            crate::vec![
                JsValue::Object(id),
                JsValue::String(JsString::from(sticky_flags.as_str())),
            ],
        ) {
            Completion::Normal(JsValue::Object(splitter)) => splitter,
            Completion::Normal(_) => {
                return interpreter.type_error("a split constructor answered a non-object")
            }
            abrupt => return abrupt,
        };

        let mut pieces: Vec<JsValue> = Vec::new();
        if limit == 0 {
            return Completion::Normal(JsValue::Object(interpreter.new_array(pieces)));
        }

        let units = subject.units().to_vec();
        if units.is_empty() {
            let probe = match regexp_exec(interpreter, &JsValue::Object(splitter), &subject) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            if !matches!(probe, JsValue::Null) {
                return Completion::Normal(JsValue::Object(interpreter.new_array(pieces)));
            }
            pieces.push(JsValue::String(subject.clone()));
            return Completion::Normal(JsValue::Object(interpreter.new_array(pieces)));
        }

        let mut start = 0usize;
        let mut cursor = 0usize;
        while cursor < units.len() {
            if let Completion::Throw(error) = interpreter.set_property_or_throw(
                splitter,
                PropertyKey::from_str("lastIndex"),
                JsValue::Number(cursor as f64),
            ) {
                return Completion::Throw(error);
            }

            let found = match regexp_exec(interpreter, &JsValue::Object(splitter), &subject) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            let JsValue::Object(found) = found else {
                cursor = advance(&subject, cursor, code_point_mode);
                continue;
            };

            let end = match interpreter
                .get_property(splitter, &PropertyKey::from_str("lastIndex"))
            {
                Completion::Normal(value) => match interpreter.to_number_value(&value) {
                    Ok(number) => to_length(number).min(units.len()),
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            };

            if end == start {
                cursor = advance(&subject, cursor, code_point_mode);
                continue;
            }

            pieces.push(JsValue::String(JsString::from_units(&units[start..cursor])));
            if pieces.len() == limit {
                return Completion::Normal(JsValue::Object(interpreter.new_array(pieces)));
            }
            start = end;

            let captures = match interpreter.get_property(found, &PropertyKey::from_str("length"))
            {
                Completion::Normal(value) => match interpreter.to_number_value(&value) {
                    Ok(number) => to_length(number).saturating_sub(1),
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            };
            for index in 1..=captures {
                let key = crate::format!("{index}");
                let value = match interpreter.get_property(found, &PropertyKey::from_str(&key)) {
                    Completion::Normal(value) => value,
                    abrupt => return abrupt,
                };
                pieces.push(value);
                if pieces.len() == limit {
                    return Completion::Normal(JsValue::Object(interpreter.new_array(pieces)));
                }
            }
            cursor = start;
        }

        pieces.push(JsValue::String(JsString::from_units(&units[start..])));
        Completion::Normal(JsValue::Object(interpreter.new_array(pieces)))
    });
    interpreter.define_well_known_symbol_method(prototype, "split", split);
}

/// `ToUint32`, which is what a split limit is coerced by.
///
/// It is not `ToInteger`: a negative limit wraps to a very large one rather than clamping to zero,
/// so `split(s, -1)` is effectively unlimited and is NOT an error.
fn to_uint32(number: f64) -> u32 {
    if !number.is_finite() || number == 0.0 {
        return 0;
    }
    let truncated = crate::math::trunc(number);
    let wrapped = truncated % 4_294_967_296.0;
    let wrapped = if wrapped < 0.0 { wrapped + 4_294_967_296.0 } else { wrapped };
    wrapped as u32
}

/// `%RegExpStringIteratorPrototype%`, `@@matchAll`, and the iterator between them.
///
/// # THE ITERATOR IS WHY THIS ARRIVED AFTER THE OTHER THREE
///
/// `match`, `search` and `replace` are each a dispatch through a symbol onto work the matcher
/// already did. `matchAll` is that plus an object with state of its own that survives between
/// calls, and there was no such object here. Wiring the dispatch without it would have answered
/// `undefined` from a method whose entire purpose is to be iterated.
///
/// # IT WALKS A CLONE, SO THE CALLER'S CURSOR IS ITS OWN
///
/// The iterator is built over a fresh pattern carrying the original's flags, seeded with the
/// original's `lastIndex`. Iterating therefore does not move the caller's cursor, and two
/// concurrent walks over one pattern do not interfere -- which is the difference between
/// `matchAll` and a hand-written `while (re.exec(s))` loop, and the reason the method exists.
pub(crate) fn install_match_all(interpreter: &mut Interpreter, prototype: ObjectId) {
    let iterator_prototype = interpreter.intrinsics.iterator_prototype;
    let iterator = interpreter.allocate(Object::new(Some(iterator_prototype)));
    interpreter.intrinsics.regexp_string_iterator_prototype = iterator;
    interpreter.define_method(iterator, "next", 0, regexp_string_iterator_next);
    crate::iterator::define_to_string_tag(interpreter, iterator, "RegExp String Iterator");

    let match_all =
        interpreter.native_function("[Symbol.matchAll]", 1, |interpreter, this, arguments| {
            let JsValue::Object(id) = this else {
                return interpreter.type_error("Symbol.matchAll requires an object");
            };
            let subject = match string_argument(interpreter, arguments) {
                Ok(subject) => subject,
                Err(abrupt) => return abrupt,
            };
            let flags = match flags_of(interpreter, id) {
                Ok(flags) => flags,
                Err(abrupt) => return abrupt,
            };

            let default = interpreter.intrinsics.regexp_constructor;
            let constructor = match interpreter.species_constructor(id, default) {
                Ok(constructor) => constructor,
                Err(abrupt) => return abrupt,
            };
            let walker = match interpreter.construct(
                constructor,
                crate::vec![
                    JsValue::Object(id),
                    JsValue::String(JsString::from(flags.as_str())),
                ],
            ) {
                Completion::Normal(JsValue::Object(walker)) => walker,
                Completion::Normal(_) => {
                    return interpreter.type_error("a matchAll constructor answered a non-object")
                }
                abrupt => return abrupt,
            };

            let start = match interpreter.get_property(id, &PropertyKey::from_str("lastIndex")) {
                Completion::Normal(value) => match interpreter.to_number_value(&value) {
                    Ok(number) => to_length(number),
                    Err(abrupt) => return abrupt,
                },
                abrupt => return abrupt,
            };
            if let Completion::Throw(error) = interpreter.set_property_or_throw(
                walker,
                PropertyKey::from_str("lastIndex"),
                JsValue::Number(start as f64),
            ) {
                return Completion::Throw(error);
            }

            let prototype = interpreter.intrinsics.regexp_string_iterator_prototype;
            let mut object = Object::new(Some(prototype));
            object.iterator_state =
                Some(crate::Box::new(crate::iterator::IteratorState::RegExpString {
                    pattern: Some(walker),
                    text: subject,
                    global: flags.contains('g'),
                    code_point_mode: flags.contains('u') || flags.contains('v'),
                }));
            Completion::Normal(JsValue::Object(interpreter.allocate(object)))
        });
    interpreter.define_well_known_symbol_method(prototype, "matchAll", match_all);
}

fn regexp_string_iterator_next(
    interpreter: &mut Interpreter,
    this: JsValue,
    _arguments: &[JsValue],
) -> Completion {
    let JsValue::Object(id) = this else {
        return interpreter.type_error("next() requires a RegExp String Iterator");
    };
    let Some(crate::iterator::IteratorState::RegExpString {
        pattern,
        text,
        global,
        code_point_mode,
    }) = interpreter.object(id).iterator_state.as_deref().cloned()
    else {
        return interpreter.type_error("next() requires a RegExp String Iterator");
    };

    let Some(pattern) = pattern else {
        return Completion::Normal(crate::iterator::iter_result(
            interpreter,
            JsValue::Undefined,
            true,
        ));
    };

    let found = match regexp_exec(interpreter, &JsValue::Object(pattern), &text) {
        Completion::Normal(value) => value,
        abrupt => return abrupt,
    };
    let JsValue::Object(found) = found else {
        set_iterator_done(interpreter, id);
        return Completion::Normal(crate::iterator::iter_result(
            interpreter,
            JsValue::Undefined,
            true,
        ));
    };

    if !global {
        set_iterator_done(interpreter, id);
        return Completion::Normal(crate::iterator::iter_result(
            interpreter,
            JsValue::Object(found),
            false,
        ));
    }

    let matched = match interpreter.get_property(found, &PropertyKey::from_str("0")) {
        Completion::Normal(value) => match interpreter.to_string_value(&value) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        },
        abrupt => return abrupt,
    };
    if matched.is_empty() {
        let at = match interpreter.get_property(pattern, &PropertyKey::from_str("lastIndex")) {
            Completion::Normal(value) => match interpreter.to_number_value(&value) {
                Ok(number) => to_length(number),
                Err(abrupt) => return abrupt,
            },
            abrupt => return abrupt,
        };
        if let Completion::Throw(error) = interpreter.set_property_or_throw(
            pattern,
            PropertyKey::from_str("lastIndex"),
            JsValue::Number(advance(&text, at, code_point_mode) as f64),
        ) {
            return Completion::Throw(error);
        }
    }

    Completion::Normal(crate::iterator::iter_result(
        interpreter,
        JsValue::Object(found),
        false,
    ))
}

/// Latches the iterator, by dropping the pattern it was walking.
fn set_iterator_done(interpreter: &mut Interpreter, id: ObjectId) {
    if let Some(crate::iterator::IteratorState::RegExpString { pattern, .. }) =
        interpreter.object_mut(id).iterator_state.as_deref_mut()
    {
        *pattern = None;
    }
}
