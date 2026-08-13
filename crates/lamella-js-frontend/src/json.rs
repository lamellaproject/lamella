//! `JSON.parse` and `JSON.stringify`.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::object::{Object, PropertyKey};
use crate::string_value::JsString;
use crate::value::{JsValue, ObjectId};
use crate::{abstract_ops as ops, format, String, ToString, Vec};

/// Which primitive-wrapper slot an object carries -- `[[NumberData]]`, `[[StringData]]`,
/// `[[BooleanData]]`, `[[SymbolData]]` -- or `None` for anything else.
///
/// # THIS IS THE TEST AND NOT THE ANSWER
///
/// `JSON.stringify` asks it in three places, and in none of them is the slot's contents the result.
/// A `[[NumberData]]` means "run `ToNumber` on the OBJECT", which consults `Symbol.toPrimitive`,
/// `valueOf` and `toString` in the usual order; the slot is only how the standard decides that the
/// conversion is owed. Returning the slot directly is right for the overwhelmingly common case --
/// `new Number(8.5)` really does serialize as `8.5` -- and wrong the moment anything is overridden,
/// which is the shape that survives every test written against ordinary wrappers.
///
/// Each caller runs its OWN conversion, because the three steps genuinely disagree: the value
/// site mirrors the slot's type, the replacer-array site stringifies both, and `[[BooleanData]]`
/// takes the slot with no conversion at all. What they share is the question, which is why only the
/// question lives here.
fn wrapper_primitive(interpreter: &Interpreter, value: &JsValue) -> Option<JsValue> {
    match value {
        JsValue::Object(id) => interpreter.object(*id).primitive.clone(),
        _ => None,
    }
}

pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let json = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.define_global("JSON", JsValue::Object(json));
    interpreter.define_to_string_tag(json, "JSON");

    interpreter.define_method(json, "parse", 2, |interpreter, _this, arguments| {
        let text = match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(text) => text,
            Err(abrupt) => return abrupt,
        };
        let units: Vec<u16> = text.units().to_vec();
        let mut parser = Reader { units: &units, pos: 0, depth: 0 };
        parser.skip_whitespace();
        let value = match parser.value(interpreter) {
            Ok(value) => value,
            Err(message) => return interpreter.throw("SyntaxError", &message),
        };
        parser.skip_whitespace();
        if parser.pos != parser.units.len() {
            return interpreter
                .throw("SyntaxError", "unexpected trailing text after a JSON value");
        }
        let reviver = arg(arguments, 1);
        if !interpreter.is_callable(&reviver) {
            return Completion::Normal(value);
        }
        let prototype = interpreter.intrinsics.object_prototype;
        let root = interpreter.allocate(Object::new(Some(prototype)));
        let _ = interpreter.create_data_property(root, PropertyKey::from_str(""), value);
        match internalize(interpreter, root, &PropertyKey::from_str(""), &reviver) {
            Ok(value) => Completion::Normal(value),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(json, "stringify", 3, |interpreter, _this, arguments| {
        let value = arg(arguments, 0);
        let replacer = arg(arguments, 1);
        let space = arg(arguments, 2);

        let mut allow: Option<Vec<PropertyKey>> = None;
        let mut transform: Option<JsValue> = None;
        if let JsValue::Object(id) = &replacer {
            let array = match interpreter.is_array(*id) {
                Ok(array) => array,
                Err(abrupt) => return abrupt,
            };
            if interpreter.is_callable(&replacer) {
                transform = Some(replacer.clone());
            } else if array {
                let mut keys: Vec<PropertyKey> = Vec::new();
                let length = match array_length(interpreter, *id) {
                    Ok(length) => length,
                    Err(abrupt) => return abrupt,
                };
                for index in 0..length {
                    let item = match interpreter.get_property(*id, &PropertyKey::from_str(&index.to_string())) {
                        Completion::Normal(item) => item,
                        abrupt => return abrupt,
                    };
                    let item = match wrapper_primitive(interpreter, &item) {
                        Some(JsValue::Number(_) | JsValue::String(_)) => {
                            match interpreter.to_string_value(&item) {
                                Ok(text) => JsValue::String(text),
                                Err(abrupt) => return abrupt,
                            }
                        }
                        _ => item,
                    };
                    let key = match &item {
                        JsValue::String(text) => Some(PropertyKey::String(text.clone())),
                        JsValue::Number(number) => {
                            Some(PropertyKey::from_str(&ops::number_to_string(*number)))
                        }
                        _ => None,
                    };
                    if let Some(key) = key {
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                }
                allow = Some(keys);
            }
        }

        let space = match wrapper_primitive(interpreter, &space) {
            Some(JsValue::Number(_)) => match interpreter.to_number_value(&space) {
                Ok(number) => JsValue::Number(number),
                Err(abrupt) => return abrupt,
            },
            Some(JsValue::String(_)) => match interpreter.to_string_value(&space) {
                Ok(text) => JsValue::String(text),
                Err(abrupt) => return abrupt,
            },
            _ => space,
        };
        let gap = match &space {
            JsValue::Number(number) => {
                let count = if number.is_nan() { 0.0 } else { number.min(10.0).max(0.0) };
                JsString::from(" ".repeat(crate::math::trunc(count) as usize).as_str())
            }
            JsValue::String(text) => JsString::from_units(&text.units()[..text.len().min(10)]),
            _ => JsString::new(),
        };

        let mut state = Writer { gap, indent: JsString::new(), stack: Vec::new(), allow, transform };
        let prototype = interpreter.intrinsics.object_prototype;
        let holder = interpreter.allocate(Object::new(Some(prototype)));
        let _ = interpreter.create_data_property(holder, PropertyKey::from_str(""), value);
        match state.property(interpreter, holder, &PropertyKey::from_str("")) {
            Ok(Some(text)) => Completion::Normal(JsValue::String(text)),
            Ok(None) => Completion::Normal(JsValue::Undefined),
            Err(abrupt) => abrupt,
        }
    });
}


/// How deep `JSON.parse` will nest before refusing.
///
/// # AN UNBOUNDED RECURSIVE PARSER OVER UNTRUSTED INPUT IS A CRASH, NOT A SLOW PATH
///
/// `[[[[...]]]]` nested far enough exhausts the HOST stack, and a stack overflow is not a
/// catchable error -- on a device it is a reset, and this engine's whole claim is that a failure
/// is loud and bounded rather than silent. `JSON.parse` is the one entry point most likely to be
/// handed bytes nobody wrote: a config blob, a telemetry frame, an OTA payload.
///
/// The interpreter's own recursion is bounded by `stack_budget`, which is measured against a
/// real stack address. This parser recurses in RUST, below that mechanism, so it needs its own.
///
/// 256 is far beyond any document a person writes -- JSON nested past about twenty levels is
/// already pathological -- and shallow enough that the frames cost little even on a small part.
/// The bound is a REFUSAL with a SyntaxError, which a program can catch.
const MAX_DEPTH: usize = 256;

struct Reader<'a> {
    units: &'a [u16],
    pos: usize,
    depth: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u16> {
        self.units.get(self.pos).copied()
    }

    /// Descends one container. Paired with [`Reader::leave`] on EVERY exit including the error
    /// ones -- a depth counter that only decrements on success turns a document with many sibling
    /// arrays into a false "too deeply nested", which is a refusal of a legal document and reads
    /// exactly like the real thing.
    fn enter(&mut self) -> Result<(), String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!("JSON nested deeper than {MAX_DEPTH} levels"));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// JSON WHITESPACE IS EXACTLY FOUR CHARACTERS. Not Unicode whitespace, not the ECMAScript
    /// set -- a parser that reuses the language's own rule accepts documents no other JSON reader
    /// will, which defeats the point of the format.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(0x20 | 0x09 | 0x0A | 0x0D)) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, ch: u8) -> bool {
        if self.peek() == Some(u16::from(ch)) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn literal(&mut self, word: &str) -> bool {
        let units: Vec<u16> = word.encode_utf16().collect();
        if self.units[self.pos..].starts_with(&units) {
            self.pos += units.len();
            return true;
        }
        false
    }

    fn value(&mut self, interpreter: &mut Interpreter) -> Result<JsValue, String> {
        self.skip_whitespace();
        match self.peek() {
            None => Err(String::from("unexpected end of JSON input")),
            Some(0x7B) => self.object(interpreter),
            Some(0x5B) => self.array(interpreter),
            Some(0x22) => Ok(JsValue::String(self.string()?)),
            Some(0x74) if self.literal("true") => Ok(JsValue::Boolean(true)),
            Some(0x66) if self.literal("false") => Ok(JsValue::Boolean(false)),
            Some(0x6E) if self.literal("null") => Ok(JsValue::Null),
            Some(unit) => {
                if unit == 0x2D || is_digit(unit) {
                    return self.number();
                }
                Err(format!("unexpected character in JSON at position {}", self.pos))
            }
        }
    }

    fn object(&mut self, interpreter: &mut Interpreter) -> Result<JsValue, String> {
        self.enter()?;
        let result = self.object_body(interpreter);
        self.leave();
        result
    }

    fn object_body(&mut self, interpreter: &mut Interpreter) -> Result<JsValue, String> {
        self.pos += 1;
        let prototype = interpreter.intrinsics.object_prototype;
        let id = interpreter.allocate(Object::new(Some(prototype)));
        self.skip_whitespace();
        if self.eat(b'}') {
            return Ok(JsValue::Object(id));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(0x22) {
                return Err(String::from("a JSON property name must be a double-quoted string"));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if !self.eat(b':') {
                return Err(String::from("expected `:` after a JSON property name"));
            }
            let value = self.value(interpreter)?;
            let _ = interpreter.create_data_property(id, PropertyKey::String(key), value);
            self.skip_whitespace();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Ok(JsValue::Object(id));
            }
            return Err(String::from("expected `,` or `}` in a JSON object"));
        }
    }

    fn array(&mut self, interpreter: &mut Interpreter) -> Result<JsValue, String> {
        self.enter()?;
        let result = self.array_body(interpreter);
        self.leave();
        result
    }

    fn array_body(&mut self, interpreter: &mut Interpreter) -> Result<JsValue, String> {
        self.pos += 1;
        let mut items: Vec<JsValue> = Vec::new();
        self.skip_whitespace();
        if self.eat(b']') {
            return Ok(JsValue::Object(interpreter.new_array(items)));
        }
        loop {
            let value = self.value(interpreter)?;
            items.push(value);
            self.skip_whitespace();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                return Ok(JsValue::Object(interpreter.new_array(items)));
            }
            return Err(String::from("expected `,` or `]` in a JSON array"));
        }
    }

    /// A JSON string. Builds CODE UNITS, so `\uD800` survives as a lone surrogate.
    fn string(&mut self) -> Result<JsString, String> {
        self.pos += 1;
        let mut out = JsString::new();
        loop {
            let Some(unit) = self.peek() else {
                return Err(String::from("unterminated string in JSON"));
            };
            self.pos += 1;
            match unit {
                0x22 => return Ok(out),
                0x00..=0x1F => {
                    return Err(String::from("a control character must be escaped in JSON"))
                }
                0x5C => {
                    let Some(escape) = self.peek() else {
                        return Err(String::from("unterminated escape in JSON"));
                    };
                    self.pos += 1;
                    match escape {
                        0x22 => out.push_code_unit(0x22),
                        0x5C => out.push_code_unit(0x5C),
                        0x2F => out.push_code_unit(0x2F),
                        0x62 => out.push_code_unit(0x08),
                        0x66 => out.push_code_unit(0x0C),
                        0x6E => out.push_code_unit(0x0A),
                        0x72 => out.push_code_unit(0x0D),
                        0x74 => out.push_code_unit(0x09),
                        0x75 => {
                            let mut value = 0u16;
                            for _ in 0..4 {
                                let Some(digit) = self.peek() else {
                                    return Err(String::from("a `\\u` escape needs four hex digits"));
                                };
                                let Some(nibble) = hex(digit) else {
                                    return Err(String::from("a `\\u` escape needs four hex digits"));
                                };
                                value = value.wrapping_mul(16).wrapping_add(u16::from(nibble));
                                self.pos += 1;
                            }
                            out.push_code_unit(value);
                        }
                        _ => return Err(String::from("unrecognised escape in JSON")),
                    }
                }
                _ => out.push_code_unit(unit),
            }
        }
    }

    /// THE JSON NUMBER GRAMMAR IS NARROWER THAN JAVASCRIPT'S: no leading `+`, no leading zeros,
    /// no `.5`, no `5.`, no hex, no `Infinity`, no `NaN`. Reusing the language's number scanner
    /// would accept documents nothing else does.
    fn number(&mut self) -> Result<JsValue, String> {
        let start = self.pos;
        self.eat(b'-');
        if self.eat(b'0') {
            if matches!(self.peek(), Some(u) if is_digit(u)) {
                return Err(String::from("a JSON number may not have a leading zero"));
            }
        } else {
            if !matches!(self.peek(), Some(u) if is_digit(u)) {
                return Err(String::from("a JSON number needs at least one digit"));
            }
            while matches!(self.peek(), Some(u) if is_digit(u)) {
                self.pos += 1;
            }
        }
        if self.eat(b'.') {
            if !matches!(self.peek(), Some(u) if is_digit(u)) {
                return Err(String::from("a JSON fraction needs at least one digit"));
            }
            while matches!(self.peek(), Some(u) if is_digit(u)) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x2B | 0x2D)) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(u) if is_digit(u)) {
                return Err(String::from("a JSON exponent needs at least one digit"));
            }
            while matches!(self.peek(), Some(u) if is_digit(u)) {
                self.pos += 1;
            }
        }
        let text: String = self.units[start..self.pos].iter().map(|u| *u as u8 as char).collect();
        text.parse::<f64>()
            .map(JsValue::Number)
            .map_err(|_| String::from("unreadable number in JSON"))
    }
}

/// A `u16` has no `is_ascii_digit`, and going via `char::from_u32` would be wrong as well as
/// slow: a JSON number is ASCII by grammar, so the test is a range on the code unit.
fn is_digit(unit: u16) -> bool {
    (0x30..=0x39).contains(&unit)
}

fn hex(unit: u16) -> Option<u8> {
    match unit {
        0x30..=0x39 => Some((unit - 0x30) as u8),
        0x41..=0x46 => Some((unit - 0x41 + 10) as u8),
        0x61..=0x66 => Some((unit - 0x61 + 10) as u8),
        _ => None,
    }
}

/// `InternalizeJSONProperty`: the reviver walk, depth first, children before their parent.
fn internalize(
    interpreter: &mut Interpreter,
    holder: ObjectId,
    key: &PropertyKey,
    reviver: &JsValue,
) -> Result<JsValue, Completion> {
    let value = match interpreter.get_property(holder, key) {
        Completion::Normal(value) => value,
        abrupt => return Err(abrupt),
    };
    if let JsValue::Object(id) = value {
        if interpreter.is_array(id)? {
            let length = match array_length(interpreter, id) {
                Ok(length) => length,
                Err(abrupt) => return Err(abrupt),
            };
            for index in 0..length {
                let element = PropertyKey::from_str(&index.to_string());
                let revived = internalize(interpreter, id, &element, reviver)?;
                if matches!(revived, JsValue::Undefined) {
                    interpreter.object_mut(id).delete_own(&element);
                } else {
                    let _ = interpreter.create_data_property(id, element, revived)?;
                }
            }
        } else {
            for own in interpreter.own_string_keys_of(id)? {
                let revived = internalize(interpreter, id, &own, reviver)?;
                if matches!(revived, JsValue::Undefined) {
                    interpreter.object_mut(id).delete_own(&own);
                } else {
                    let _ = interpreter.create_data_property(id, own, revived)?;
                }
            }
        }
    }
    let name = JsValue::String(JsString::from(key.to_display().as_str()));
    match interpreter.call_value(reviver, JsValue::Object(holder), crate::vec![name, value]) {
        Completion::Normal(value) => Ok(value),
        abrupt => Err(abrupt),
    }
}


struct Writer {
    gap: JsString,
    indent: JsString,
    /// THE CYCLE CHECK, and it is a STACK rather than a set: an object may legitimately appear
    /// twice in one document side by side. Only an object containing ITSELF is an error.
    stack: Vec<ObjectId>,
    allow: Option<Vec<PropertyKey>>,
    transform: Option<JsValue>,
}

impl Writer {
    /// `SerializeJSONProperty`. `Ok(None)` is "this property produces no text at all".
    fn property(
        &mut self,
        interpreter: &mut Interpreter,
        holder: ObjectId,
        key: &PropertyKey,
    ) -> Result<Option<JsString>, Completion> {
        let mut value = match interpreter.get_property(holder, key) {
            Completion::Normal(value) => value,
            abrupt => return Err(abrupt),
        };
        let name = JsValue::String(JsString::from(key.to_display().as_str()));

        if let JsValue::Object(id) = value {
            let to_json = match interpreter.get_property(id, &PropertyKey::from_str("toJSON")) {
                Completion::Normal(value) => value,
                abrupt => return Err(abrupt),
            };
            if interpreter.is_callable(&to_json) {
                value = match interpreter.call_value(&to_json, value.clone(), crate::vec![name.clone()]) {
                    Completion::Normal(value) => value,
                    abrupt => return Err(abrupt),
                };
            }
        }
        if let Some(transform) = self.transform.clone() {
            value = match interpreter.call_value(
                &transform,
                JsValue::Object(holder),
                crate::vec![name, value],
            ) {
                Completion::Normal(value) => value,
                abrupt => return Err(abrupt),
            };
        }
        self.value(interpreter, value)
    }

    fn value(
        &mut self,
        interpreter: &mut Interpreter,
        value: JsValue,
    ) -> Result<Option<JsString>, Completion> {
        let value = match wrapper_primitive(interpreter, &value) {
            Some(JsValue::Number(_)) => match interpreter.to_number_value(&value) {
                Ok(number) => JsValue::Number(number),
                Err(abrupt) => return Err(abrupt),
            },
            Some(JsValue::String(_)) => match interpreter.to_string_value(&value) {
                Ok(text) => JsValue::String(text),
                Err(abrupt) => return Err(abrupt),
            },
            Some(slot @ JsValue::Boolean(_)) => slot,
            _ => value,
        };
        match value {
            JsValue::Null => Ok(Some(JsString::from("null"))),
            JsValue::Boolean(true) => Ok(Some(JsString::from("true"))),
            JsValue::Boolean(false) => Ok(Some(JsString::from("false"))),
            JsValue::String(text) => Ok(Some(quote(&text))),
            JsValue::Number(number) => Ok(Some(if number.is_finite() {
                JsString::from(ops::number_to_string(number).as_str())
            } else {
                JsString::from("null")
            })),
            JsValue::Object(id) => {
                if interpreter.is_callable(&JsValue::Object(id)) {
                    return Ok(None);
                }
                if self.stack.contains(&id) {
                    return Err(interpreter.type_error("a cyclic structure cannot be serialized"));
                }
                self.stack.push(id);
                let array = match interpreter.is_array(id) {
                    Ok(array) => array,
                    Err(abrupt) => {
                        self.stack.pop();
                        return Err(abrupt);
                    }
                };
                let result = if array {
                    self.array(interpreter, id)
                } else {
                    self.object(interpreter, id)
                };
                self.stack.pop();
                result.map(Some)
            }
            _ => Ok(None),
        }
    }

    /// The `,` / newline joining shared by both container forms.
    ///
    /// IT TAKES BOTH INDENTS, because the two ends of a container are at DIFFERENT depths: every
    /// member sits at `stepped`, and the closing bracket goes back to `outer`. Reading the current
    /// indent off `self` gives the inner one for both and produces `{\n  "a": 1\n  }` -- valid
    /// JSON, wrongly shaped, and invisible to any test that only checks it parses back.
    fn wrap(
        &self,
        parts: &[JsString],
        open: &str,
        close: &str,
        stepped: &JsString,
        outer: &JsString,
    ) -> JsString {
        let mut out = JsString::from(open);
        if parts.is_empty() {
            out.push_str(close);
            return out;
        }
        let pretty = !self.gap.is_empty();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                out.push_str(",");
            }
            if pretty {
                out.push_str("\n");
                out.extend_from(stepped);
            }
            out.extend_from(part);
        }
        if pretty {
            out.push_str("\n");
            out.extend_from(outer);
        }
        out.push_str(close);
        out
    }

    fn array(
        &mut self,
        interpreter: &mut Interpreter,
        id: ObjectId,
    ) -> Result<JsString, Completion> {
        let outer = self.indent.clone();
        let mut stepped = outer.clone();
        stepped.extend_from(&self.gap);
        self.indent = stepped.clone();

        let length = array_length(interpreter, id)?;
        let mut parts: Vec<JsString> = Vec::new();
        for index in 0..length {
            let key = PropertyKey::from_str(&index.to_string());
            let part = self.property(interpreter, id, &key)?;
            parts.push(part.unwrap_or_else(|| JsString::from("null")));
        }
        let text = self.wrap(&parts, "[", "]", &stepped, &outer);
        self.indent = outer;
        Ok(text)
    }

    fn object(
        &mut self,
        interpreter: &mut Interpreter,
        id: ObjectId,
    ) -> Result<JsString, Completion> {
        let outer = self.indent.clone();
        let mut stepped = outer.clone();
        stepped.extend_from(&self.gap);
        self.indent = stepped.clone();

        let keys: Vec<PropertyKey> = match &self.allow {
            Some(allow) => allow.clone(),
            None => interpreter.enumerable_keys_of(id)?,
        };
        let mut parts: Vec<JsString> = Vec::new();
        for key in keys {
            if let Some(text) = self.property(interpreter, id, &key)? {
                let mut part = quote(&JsString::from(key.to_display().as_str()));
                part.push_str(":");
                if !self.gap.is_empty() {
                    part.push_str(" ");
                }
                part.extend_from(&text);
                parts.push(part);
            }
        }
        let text = self.wrap(&parts, "{", "}", &stepped, &outer);
        self.indent = outer;
        Ok(text)
    }
}

/// `QuoteJSONString`, including the well-formed rule for unpaired surrogates.
fn quote(text: &JsString) -> JsString {
    let mut out = JsString::new();
    out.push_code_unit(0x22);
    let units = text.units();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        match unit {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x00..=0x1F => out.push_str(&format!("\\u{unit:04x}")),
            0xD800..=0xDBFF => {
                let paired = units
                    .get(index + 1)
                    .is_some_and(|next| (0xDC00..=0xDFFF).contains(next));
                if paired {
                    out.push_code_unit(unit);
                    out.push_code_unit(units[index + 1]);
                    index += 2;
                    continue;
                }
                out.push_str(&format!("\\u{unit:04x}"));
            }
            0xDC00..=0xDFFF => out.push_str(&format!("\\u{unit:04x}")),
            _ => out.push_code_unit(unit),
        }
        index += 1;
    }
    out.push_code_unit(0x22);
    out
}

fn array_length(interpreter: &mut Interpreter, id: ObjectId) -> Result<u32, Completion> {
    match interpreter.get_property(id, &PropertyKey::from_str("length")) {
        Completion::Normal(JsValue::Number(length)) => Ok(length as u32),
        Completion::Normal(_) => Ok(0),
        abrupt => Err(abrupt),
    }
}
