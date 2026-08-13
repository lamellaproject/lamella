//! TypedArrays: the nine element types, and the exotic indexing that makes them arrays.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::object::{BinaryData, Element, Object, Property, PropertyKey, PropertyKind};
use crate::string_value::JsString;
use crate::value::{JsValue, ObjectId};
use crate::{format, String, Vec};

/// What a TypedArray is, read out of its slot: everything the exotic methods need.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View {
    pub buffer: ObjectId,
    pub offset: usize,
    pub length: usize,
    pub element: Element,
}

/// `CanonicalNumericIndexString`: the numbers a property key spells, and only those.
///
/// **IT IS NOT "DOES THIS PARSE AS A NUMBER".** The test is a ROUND TRIP: the key is a canonical
/// numeric string exactly when rendering its numeric value reproduces the key. So `"1"` is one and
/// `"01"`, `"1.0"`, `" 1"` and `"0x1"` are ordinary string keys -- which is what stops
/// `ta["01"] = 5` from being swallowed as an out-of-range element write when it should create an
/// ordinary property.
///
/// `"-0"` IS ONE, and it is the reason the round trip cannot be the whole rule: `ToString(-0)` is
/// `"0"`, so `-0` would fail its own test. The standard special-cases it in the first step, and it
/// matters -- `ta[-0]` must read element 0 while `ta["-0"]` must be `undefined`, because the first
/// converts a Number key and the second is already a string.
#[must_use]
pub(crate) fn canonical_numeric_index(key: &PropertyKey) -> Option<f64> {
    let PropertyKey::String(text) = key else { return None };
    let rendered = text.to_lossy_string();
    if rendered == "-0" {
        return Some(-0.0);
    }
    let number = crate::abstract_ops::string_to_number(text);
    (crate::abstract_ops::number_to_string(number) == rendered).then_some(number)
}

/// `IsValidIntegerIndex`: whether this number addresses an element that is there.
///
/// FOUR SEPARATE REASONS TO SAY NO, and each is a different program: a detached buffer, a
/// non-integral index, a negative one (including `-0`), and one past the end. Collapsing them into
/// a range check accepts `ta[1.5]` as index 1.
#[must_use]
fn is_valid_index(interpreter: &Interpreter, view: &View, index: f64) -> bool {
    if interpreter.buffer_is_detached(view.buffer) {
        return false;
    }
    if !index.is_finite() || crate::math::trunc(index) != index {
        return false;
    }
    if index.is_sign_negative() {
        return false;
    }
    index < view.length as f64
}


/// Reads one element out of the buffer, or `undefined` when the index is not valid.
pub(crate) fn get_element(interpreter: &Interpreter, view: &View, index: f64) -> JsValue {
    if !is_valid_index(interpreter, view, index) {
        return JsValue::Undefined;
    }
    let width = view.element.width();
    let at = view.offset + (index as usize) * width;
    let mut raw = [0u8; 8];
    match interpreter.object(view.buffer).binary.as_deref() {
        Some(BinaryData::Buffer { bytes, .. }) => match bytes.get(at..at + width) {
            Some(slice) => raw[..width].copy_from_slice(slice),
            None => return JsValue::Undefined,
        },
        _ => return JsValue::Undefined,
    }
    let value = match view.element {
        Element::Int8 => f64::from(raw[0] as i8),
        Element::Uint8 | Element::Uint8Clamped => f64::from(raw[0]),
        Element::Int16 => f64::from(i16::from_le_bytes([raw[0], raw[1]])),
        Element::Uint16 => f64::from(u16::from_le_bytes([raw[0], raw[1]])),
        Element::Int32 => f64::from(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Element::Uint32 => f64::from(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Element::Float32 => f64::from(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Element::Float64 => f64::from_le_bytes(raw),
    };
    JsValue::Number(value)
}

/// `TypedArraySetElement`: coerce, THEN check the index.
///
/// THE ORDER IS OBSERVABLE AND IT IS THE OPPOSITE OF WHAT A BOUNDS CHECK SUGGESTS. `ToNumber`
/// runs first and can execute a program -- `ta[0] = { valueOf() { detach(buffer); return 1; } }` is
/// a real test -- so a validity test taken beforehand is a test of a state that no longer holds.
/// An out-of-range write then does NOTHING and reports success; it is not an error.
pub(crate) fn set_element(
    interpreter: &mut Interpreter,
    view: &View,
    index: f64,
    value: &JsValue,
) -> Result<(), Completion> {
    let number = match interpreter.to_number_value(value) {
        Ok(number) => number,
        Err(abrupt) => return Err(abrupt),
    };
    if !is_valid_index(interpreter, view, index) {
        return Ok(());
    }
    let width = view.element.width();
    let at = view.offset + (index as usize) * width;
    let mut raw = [0u8; 8];
    match view.element {
        Element::Int8 | Element::Uint8 => raw[0] = crate::binary::wrap_to_u32(number) as u8,
        Element::Uint8Clamped => raw[0] = clamp_to_u8(number),
        Element::Int16 | Element::Uint16 => {
            raw[..2].copy_from_slice(&(crate::binary::wrap_to_u32(number) as u16).to_le_bytes());
        }
        Element::Int32 | Element::Uint32 => {
            raw[..4].copy_from_slice(&crate::binary::wrap_to_u32(number).to_le_bytes());
        }
        Element::Float32 => raw[..4].copy_from_slice(&(number as f32).to_le_bytes()),
        Element::Float64 => raw.copy_from_slice(&number.to_le_bytes()),
    }
    if let Some(BinaryData::Buffer { bytes, .. }) =
        interpreter.object_mut(view.buffer).binary.as_deref_mut()
    {
        if let Some(slice) = bytes.get_mut(at..at + width) {
            slice.copy_from_slice(&raw[..width]);
        }
    }
    Ok(())
}

/// `ToUint8Clamp`: clamp to 0..255 and round HALF TO EVEN.
///
/// THE TIE RULE IS NOT "ROUND HALF UP", AND THE DIFFERENCE IS TESTED DIRECTLY. `0.5` clamps to
/// **0** and `1.5` clamps to **2** -- each to the nearer even integer -- so a `+ 0.5` and truncate
/// gets 0.5 wrong, and `f64::round` gets it wrong in the other direction. `NaN` is 0 rather than
/// being left alone, which is the one place this conversion differs from every other by being
/// total.
fn clamp_to_u8(number: f64) -> u8 {
    if number.is_nan() || number <= 0.0 {
        return 0;
    }
    if number >= 255.0 {
        return 255;
    }
    let floor = crate::math::floor(number);
    let fraction = number - floor;
    let rounded = if fraction < 0.5 {
        floor
    } else if fraction > 0.5 {
        floor + 1.0
    } else if crate::math::fract(floor / 2.0) == 0.0 {
        floor
    } else {
        floor + 1.0
    };
    rounded as u8
}


pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;

    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.typed_array_prototype = prototype;
    let constructor = interpreter.native_constructor(
        "TypedArray",
        0,
        |interpreter, _this, _arguments| {
            interpreter.type_error("this constructor requires `new`")
        },
        |interpreter, _this, _arguments| {
            interpreter.type_error("the abstract TypedArray constructor cannot be constructed")
        },
    );
    interpreter.intrinsics.typed_array_constructor = constructor;
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_species_getter(constructor);

    accessor(interpreter, prototype, "buffer", |interpreter, this, _| match view_of(
        interpreter,
        &this,
    ) {
        Ok(view) => Completion::Normal(JsValue::Object(view.buffer)),
        Err(abrupt) => abrupt,
    });
    accessor(interpreter, prototype, "byteLength", |interpreter, this, _| match view_of(
        interpreter,
        &this,
    ) {
        Ok(view) if interpreter.buffer_is_detached(view.buffer) => {
            Completion::Normal(JsValue::Number(0.0))
        }
        Ok(view) => {
            Completion::Normal(JsValue::Number((view.length * view.element.width()) as f64))
        }
        Err(abrupt) => abrupt,
    });
    accessor(interpreter, prototype, "byteOffset", |interpreter, this, _| match view_of(
        interpreter,
        &this,
    ) {
        Ok(view) if interpreter.buffer_is_detached(view.buffer) => {
            Completion::Normal(JsValue::Number(0.0))
        }
        Ok(view) => Completion::Normal(JsValue::Number(view.offset as f64)),
        Err(abrupt) => abrupt,
    });
    accessor(interpreter, prototype, "length", |interpreter, this, _| match view_of(
        interpreter,
        &this,
    ) {
        Ok(view) if interpreter.buffer_is_detached(view.buffer) => {
            Completion::Normal(JsValue::Number(0.0))
        }
        Ok(view) => Completion::Normal(JsValue::Number(view.length as f64)),
        Err(abrupt) => abrupt,
    });

    let tag_key = PropertyKey::Symbol(interpreter.well_known_symbol("toStringTag"));
    let tag = interpreter.native_function("get [Symbol.toStringTag]", 0, |interpreter, this, _| {
        let name = match &this {
            JsValue::Object(id) => interpreter.typed_array_view(*id).map(|view| view.element.name()),
            _ => None,
        };
        Completion::Normal(match name {
            Some(name) => JsValue::string(name),
            None => JsValue::Undefined,
        })
    });
    interpreter.object_mut(prototype).set_own(tag_key, Property::accessor(Some(tag), None));

    install_prototype_methods(interpreter, prototype);
    interpreter.define_method(constructor, "from", 1, from);
    interpreter.define_method(constructor, "of", 0, of);

    for element in Element::ALL {
        install_one(interpreter, element, constructor, prototype);
    }
}

/// `TypedArrayCreateFromConstructor(C, « length »)`: construct through the receiver, then check
/// that what came back is a typed array long enough to hold what is about to be written.
///
/// THE LENGTH CHECK IS THE POINT. `Construct` runs USER CODE -- a subclass constructor may
/// ignore its argument and build a shorter array, or return one whose buffer is already detached.
/// Writing into it would silently drop every element past the end, because an out-of-range write to
/// a typed array succeeds and stores nothing. The standard makes it a TypeError here instead.
fn create_from_constructor(
    interpreter: &mut Interpreter,
    constructor: &JsValue,
    length: f64,
) -> Result<ObjectId, Completion> {
    let JsValue::Object(id) = constructor else {
        return Err(interpreter.type_error("this is not a constructor"));
    };
    if !interpreter.is_constructor(*id) {
        return Err(interpreter.type_error("this is not a constructor"));
    }
    let built = match interpreter.construct(*id, crate::vec![JsValue::Number(length)]) {
        Completion::Normal(JsValue::Object(built)) => built,
        Completion::Normal(_) => {
            return Err(interpreter.type_error("that constructor did not produce an object"))
        }
        abrupt => return Err(abrupt),
    };
    let view = view_of(interpreter, &JsValue::Object(built))?;
    if interpreter.buffer_is_detached(view.buffer) {
        return Err(interpreter.type_error("this ArrayBuffer is detached"));
    }
    if (view.length as f64) < length {
        return Err(interpreter.type_error("that constructor produced too short a typed array"));
    }
    Ok(built)
}

/// `%TypedArray%.of(...items)`.
///
/// THE RECEIVER MUST BE A CONSTRUCTOR, unlike `Array.of`, which builds a plain array when it is
/// not. There is no "ordinary typed array" to fall back to -- the element type comes from the
/// constructor and nowhere else -- so `%TypedArray%.of.call(undefined, 1)` is a TypeError.
fn of(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let target = match create_from_constructor(interpreter, &this, arguments.len() as f64) {
        Ok(target) => target,
        Err(abrupt) => return abrupt,
    };
    for (index, value) in arguments.iter().enumerate() {
        let key = PropertyKey::from_str(&crate::abstract_ops::number_to_string(index as f64));
        let written = interpreter.set_property_or_throw(target, key, value.clone());
        if written.is_abrupt() {
            return written;
        }
    }
    Completion::Normal(JsValue::Object(target))
}

/// `%TypedArray%.from(source, mapfn, thisArg)`.
///
/// # THE ITERATOR PATH COLLECTS EVERYTHING **BEFORE** IT CONSTRUCTS, WHICH IS THE OPPOSITE OF
/// `Array.from`
///
/// `Array.from` builds its result first and fills it during the walk, so a refused define can close
/// the iterator that is still running. This one runs `IteratorToList` to completion and constructs
/// afterwards -- because a typed array's length is fixed at construction and cannot be known until
/// the walk is over. The two are one method name apart and the difference is deliberate in both,
/// which is why the contrast is written down rather than the shapes being made to match.
fn from(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let source = arg(arguments, 0);
    let mapper = arg(arguments, 1);
    let this_arg = arg(arguments, 2);
    match &this {
        JsValue::Object(id) if interpreter.is_constructor(*id) => {}
        _ => return interpreter.type_error("this is not a constructor"),
    }
    if !matches!(mapper, JsValue::Undefined) && !interpreter.is_callable(&mapper) {
        return interpreter.type_error("the mapping function is not a function");
    }

    let key = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));
    let using_iterator = match crate::iterator::get_method(interpreter, &source, &key) {
        Ok(method) => method,
        Err(abrupt) => return abrupt,
    };

    let mut collected: Vec<JsValue> = Vec::new();
    let mut array_like: Option<ObjectId> = None;
    let length: f64;
    if let Some(using_iterator) = using_iterator {
        let mut record =
            match crate::iterator::get_iterator_from_method(interpreter, &source, &using_iterator) {
                Ok(record) => record,
                Err(abrupt) => return abrupt,
            };
        let mut values = Vec::new();
        loop {
            let result = match crate::iterator::iterator_step(interpreter, &mut record) {
                Ok(None) => break,
                Ok(Some(result)) => result,
                Err(abrupt) => return abrupt,
            };
            match crate::iterator::iterator_value(interpreter, result) {
                Ok(value) => values.push(value),
                Err(abrupt) => return abrupt,
            }
        }
        length = values.len() as f64;
        collected = values;
    } else {
        let object = match interpreter.to_object(&source) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        length = match crate::builtins::array_length(interpreter, &JsValue::Object(object)) {
            Ok(length) => length,
            Err(abrupt) => return abrupt,
        };
        array_like = Some(object);
    }

    let target = match create_from_constructor(interpreter, &this, length) {
        Ok(target) => target,
        Err(abrupt) => return abrupt,
    };
    let mut index = 0.0f64;
    while index < length {
        let key = PropertyKey::from_str(&crate::abstract_ops::number_to_string(index));
        let value = match array_like {
            Some(object) => match interpreter.get_member(&JsValue::Object(object), &key) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            },
            None => collected[index as usize].clone(),
        };
        let value = if matches!(mapper, JsValue::Undefined) {
            value
        } else {
            match interpreter.call_value(
                &mapper,
                this_arg.clone(),
                crate::vec![value, JsValue::Number(index)],
            ) {
                Completion::Normal(mapped) => mapped,
                abrupt => return abrupt,
            }
        };
        let written = interpreter.set_property_or_throw(target, key, value);
        if written.is_abrupt() {
            return written;
        }
        index += 1.0;
    }
    Completion::Normal(JsValue::Object(target))
}

/// One of the nine, each of which is `%TypedArray%` with a width and a name.
fn install_one(
    interpreter: &mut Interpreter,
    element: Element,
    parent: ObjectId,
    parent_prototype: ObjectId,
) {
    let prototype = interpreter.allocate(Object::new(Some(parent_prototype)));
    let refuse: crate::interpreter::NativeFn = |interpreter, _this, _arguments| {
        interpreter.type_error("this constructor requires `new`")
    };
    let construct: crate::interpreter::NativeFn = |interpreter, this, arguments| {
        let element = match &this {
            JsValue::Object(id) => interpreter.typed_array_kind_of_constructor(*id),
            _ => None,
        };
        let Some(element) = element else {
            return interpreter.type_error("not a TypedArray constructor");
        };
        construct_typed_array(interpreter, element, arguments)
    };
    let constructor = interpreter.native_constructor(element.name(), 3, refuse, construct);
    interpreter.object_mut(constructor).prototype = Some(parent);
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    for target in [constructor, prototype] {
        interpreter.object_mut(target).set_own(
            PropertyKey::from_str("BYTES_PER_ELEMENT"),
            Property {
                kind: PropertyKind::Data {
                    value: JsValue::Number(element.width() as f64),
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            },
        );
    }
    interpreter.define_global(element.name(), JsValue::Object(constructor));
    interpreter.register_typed_array_constructor(constructor, prototype, element);
}

/// `new Int8Array(...)`: four shapes, told apart by the argument's TYPE.
fn construct_typed_array(
    interpreter: &mut Interpreter,
    element: Element,
    arguments: &[JsValue],
) -> Completion {
    match arg(arguments, 0) {
        JsValue::Undefined => allocate(interpreter, element, 0),
        JsValue::Object(source) => {
            let is_buffer = matches!(
                interpreter.object(source).binary.as_deref(),
                Some(BinaryData::Buffer { .. })
            );
            if is_buffer {
                return from_buffer(interpreter, element, source, arguments);
            }
            if let Some(source_view) = interpreter.typed_array_view(source) {
                if interpreter.buffer_is_detached(source_view.buffer) {
                    return interpreter.type_error("this ArrayBuffer is detached");
                }
            }
            let key = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));
            match crate::iterator::get_method(interpreter, &JsValue::Object(source), &key) {
                Ok(Some(_)) => {
                    let values = match crate::iterator::iterate_to_list(
                        interpreter,
                        &JsValue::Object(source),
                    ) {
                        Ok(values) => values,
                        Err(abrupt) => return abrupt,
                    };
                    from_values(interpreter, element, &values)
                }
                Ok(None) => from_list(interpreter, element, &JsValue::Object(source)),
                Err(abrupt) => abrupt,
            }
        }
        value => {
            let length = match interpreter.to_index(&value, "invalid typed array length") {
                Ok(length) => length,
                Err(abrupt) => return abrupt,
            };
            allocate(interpreter, element, length)
        }
    }
}

/// `new Int8Array(buffer, byteOffset, length)`: a window onto bytes somebody else owns.
fn from_buffer(
    interpreter: &mut Interpreter,
    element: Element,
    buffer: ObjectId,
    arguments: &[JsValue],
) -> Completion {
    let width = element.width();
    let offset = match interpreter.to_index(&arg(arguments, 1), "invalid typed array offset") {
        Ok(offset) => offset,
        Err(abrupt) => return abrupt,
    };
    if offset % width != 0 {
        return interpreter
            .throw("RangeError", "the offset must be a multiple of the element size");
    }
    let requested = match arg(arguments, 2) {
        JsValue::Undefined => None,
        value => match interpreter.to_index(&value, "invalid typed array length") {
            Ok(length) => Some(length),
            Err(abrupt) => return abrupt,
        },
    };
    if interpreter.buffer_is_detached(buffer) {
        return interpreter.type_error("this ArrayBuffer is detached");
    }
    let total = match interpreter.object(buffer).binary.as_deref() {
        Some(BinaryData::Buffer { bytes, .. }) => bytes.len(),
        _ => return interpreter.type_error("a typed array requires an ArrayBuffer"),
    };
    let length = match requested {
        None => {
            if total % width != 0 {
                return interpreter
                    .throw("RangeError", "the buffer length must be a multiple of the element size");
            }
            if offset > total {
                return interpreter.throw("RangeError", "the offset is past the end of the buffer");
            }
            (total - offset) / width
        }
        Some(length) => {
            if offset + length * width > total {
                return interpreter.throw("RangeError", "the view is out of the buffer's bounds");
            }
            length
        }
    };
    Completion::Normal(JsValue::Object(new_view(interpreter, element, buffer, offset, length)))
}

/// `new Int8Array([1, 2, 3])` and `new Int8Array(otherTypedArray)`.
fn from_list(interpreter: &mut Interpreter, element: Element, source: &JsValue) -> Completion {
    let values = match crate::builtins::array_like_to_vec(interpreter, source) {
        Ok(values) => values,
        Err(abrupt) => return abrupt,
    };
    from_values(interpreter, element, &values)
}

/// The half of `from_list` after the values are in hand, which the ITERABLE path also needs.
fn from_values(interpreter: &mut Interpreter, element: Element, values: &[JsValue]) -> Completion {
    let id = match allocate(interpreter, element, values.len()) {
        Completion::Normal(JsValue::Object(id)) => id,
        other => return other,
    };
    let Some(view) = interpreter.typed_array_view(id) else {
        return interpreter.internal_defect("a fresh typed array with no view");
    };
    for (index, value) in values.iter().enumerate() {
        if let Err(abrupt) = set_element(interpreter, &view, index as f64, value) {
            return abrupt;
        }
    }
    Completion::Normal(JsValue::Object(id))
}

fn allocate(interpreter: &mut Interpreter, element: Element, length: usize) -> Completion {
    let bytes = match length.checked_mul(element.width()) {
        Some(bytes) => bytes,
        None => return interpreter.throw("RangeError", "invalid typed array length"),
    };
    let buffer = match interpreter.new_array_buffer(bytes) {
        Ok(buffer) => buffer,
        Err(abrupt) => return abrupt,
    };
    Completion::Normal(JsValue::Object(new_view(interpreter, element, buffer, 0, length)))
}

/// `TypedArraySpeciesCreate(exemplar, argList)` (23.2.4.3), and the
/// `TypedArrayCreateFromConstructor` (23.2.4.2) it is written on top of.
///
/// # FOUR METHODS PERFORM IT, AND EACH ONE HAS ITS OWN WAY OF LOOKING EXEMPT
///
/// `slice`, `subarray`, `filter` and `map` each build their result through the constructor the
/// EXEMPLAR names -- `Get(O, "constructor")`, then `Get(C, @@species)` -- so a program decides what
/// comes back, its ELEMENT TYPE included. Allocating an array of the receiver's own type instead
/// is the same defect four times over, reasoned differently each time:
///
/// - `subarray`: the default constructor refuses a detached buffer, so throwing directly looks
///   equivalent. It is true of that constructor, and a species may be a function that never looks
///   at the buffer -- so the refusal belongs to `Construct`, which is a step rather than a
///   conclusion.
/// - `slice`: allocating the source's element type means `ta.constructor = Uint8Array` on an
///   `Int8Array` comes back holding the SIGNED values rather than converting them.
/// - `map` and `filter`: "`TypedArraySpeciesCreate` enforces that the result is a TypedArray,
///   unlike `ArraySpeciesCreate`" is true, and it is a fact about what the species may RETURN
///   rather than a reason not to ask for one.
///
/// The refusals are the constructor's own. A species that is not a constructor, one that returns
/// a non-TypedArray, and one that returns an array shorter than the length asked for are three
/// different TypeErrors, and none of them is spelled here: `species_constructor` raises the first,
/// [`validated_view`] the second, and step 4 below the third.
///
/// `[[ContentType]]` (step 4 of species-create) is not checked, and cannot be reached: it is
/// `bigint` only for `BigInt64Array` and `BigUint64Array`, and this profile has neither, so every
/// typed array in this realm has the same content type. A branch here would be unreachable code
/// asserting a distinction the realm cannot express.
fn species_create(
    interpreter: &mut Interpreter,
    exemplar: ObjectId,
    element: Element,
    arguments: Vec<JsValue>,
) -> Result<(ObjectId, View), Completion> {
    let default_constructor = interpreter.typed_array_constructor_for(element);
    let constructor = interpreter.species_constructor(exemplar, default_constructor)?;
    let requested = match arguments.as_slice() {
        [JsValue::Number(length)] => Some(*length),
        _ => None,
    };
    let created = match interpreter.construct(constructor, arguments) {
        Completion::Normal(value) => value,
        abrupt => return Err(abrupt),
    };
    let view = validated_view(interpreter, &created)?;
    let JsValue::Object(id) = created else {
        return Err(interpreter.internal_defect("a validated view over a non-object"));
    };
    if let Some(requested) = requested {
        if (view.length as f64) < requested {
            return Err(interpreter
                .type_error("the species constructor returned a typed array that is too short"));
        }
    }
    Ok((id, view))
}

fn new_view(
    interpreter: &mut Interpreter,
    element: Element,
    buffer: ObjectId,
    offset: usize,
    length: usize,
) -> ObjectId {
    let prototype = interpreter.typed_array_prototype_for(element);
    let mut object = Object::new(Some(prototype));
    object.binary = Some(crate::Box::new(BinaryData::TypedArray {
        buffer,
        offset,
        length,
        element,
    }));
    interpreter.allocate(object)
}

/// `this` as a TypedArray, which is the brand check every prototype method starts with.
///
/// It is a SLOT question, not a property one: `Int8Array.prototype.subarray.call({length: 1})`
/// is a TypeError *because* a plain object has no view, and reading a hidden property instead
/// would make the brand forgeable.
pub(crate) fn view_of(interpreter: &mut Interpreter, this: &JsValue) -> Result<View, Completion> {
    if let JsValue::Object(id) = this {
        if let Some(view) = interpreter.typed_array_view(*id) {
            return Ok(view);
        }
    }
    Err(interpreter.type_error("this method requires a typed array"))
}

/// `ValidateTypedArray(O, seq-cst)`: the brand check AND the detachment test, which most methods
/// open with and which [`view_of`] alone is only half of.
///
/// # A SHARED BODY IS WHERE THIS OPERATION GOES MISSING, BECAUSE THREE CALL SITES ARE TWELVE
/// METHODS
///
/// A method with a body of its own gets converted on its own and is easy to get right. The twelve
/// that share [`walk`], [`reduce`] and [`search`] are three edits, and three call sites do not look
/// like twelve. Without it a detached array reads as an array of length zero, and the failure is
/// the quiet one -- `array.forEach(f)` calls nothing and returns normally, `array.every(f)` answers
/// `true`, `array.indexOf(x)` answers -1.
///
/// **THE CALLERS OF [`view_of`] ARE THE CLAIM WORTH RE-DERIVING, NOT THEIR COUNT.** The ones
/// outside this function are `set` and `subarray`, which the text opens with `RequireInternalSlot`
/// and not with `ValidateTypedArray`, and the species-create helper, whose detachment test is a
/// separate step of its own clause. Anything else in that list is a defect.
///
/// NOT EVERY METHOD WANTS IT, which is why this is a second function rather than a check folded
/// into the first: the four accessors REPORT 0 for a detached array rather than throwing, and
/// `subarray` hands back a view without looking at the bytes. (Its refusal arrives anyway, from
/// `TypedArraySpeciesCreate` at the END -- a method that does not perform an operation can still
/// fail the way that operation fails.)
pub(crate) fn validated_view(
    interpreter: &mut Interpreter,
    this: &JsValue,
) -> Result<View, Completion> {
    let view = view_of(interpreter, this)?;
    if interpreter.buffer_is_detached(view.buffer) {
        return Err(interpreter.type_error("this ArrayBuffer is detached"));
    }
    Ok(view)
}

fn accessor(
    interpreter: &mut Interpreter,
    target: ObjectId,
    name: &str,
    call: crate::interpreter::NativeFn,
) {
    let getter = interpreter.native_function(&format!("get {name}"), 0, call);
    interpreter
        .object_mut(target)
        .set_own(PropertyKey::from_str(name), Property::accessor(Some(getter), None));
}


fn install_prototype_methods(interpreter: &mut Interpreter, prototype: ObjectId) {
    interpreter.define_method(prototype, "at", 1, |interpreter, this, arguments| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let relative = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => crate::builtins::to_integer(number),
            Err(abrupt) => return abrupt,
        };
        let length = view.length as f64;
        let index = if relative < 0.0 { length + relative } else { relative };
        if index < 0.0 || index >= length {
            return Completion::Normal(JsValue::Undefined);
        }
        Completion::Normal(get_element(interpreter, &view, index))
    });

    interpreter.define_method(prototype, "fill", 1, |interpreter, this, arguments| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let value = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => JsValue::Number(number),
            Err(abrupt) => return abrupt,
        };
        let (start, end) = match range(interpreter, arguments, 1, view.length) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        if interpreter.buffer_is_detached(view.buffer) {
            return interpreter.type_error("this ArrayBuffer is detached");
        }
        let mut index = start;
        while index < end {
            if let Err(abrupt) = set_element(interpreter, &view, index as f64, &value) {
                return abrupt;
            }
            index += 1;
        }
        Completion::Normal(this)
    });

    interpreter.define_method(prototype, "set", 1, |interpreter, this, arguments| {
        let view = match view_of(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let offset = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => crate::builtins::to_integer(number),
            Err(abrupt) => return abrupt,
        };
        if offset < 0.0 {
            return interpreter.throw("RangeError", "the offset may not be negative");
        }
        if interpreter.buffer_is_detached(view.buffer) {
            return interpreter.type_error("this ArrayBuffer is detached");
        }
        let source_view = match arg(arguments, 0) {
            JsValue::Object(source) => interpreter.typed_array_view(source),
            _ => None,
        };
        if let Some(source_view) = source_view {
            if interpreter.buffer_is_detached(source_view.buffer) {
                return interpreter.type_error("this ArrayBuffer is detached");
            }
            if offset + source_view.length as f64 > view.length as f64 {
                return interpreter.throw("RangeError", "the source is longer than the target");
            }
            let mut values = Vec::with_capacity(source_view.length);
            for index in 0..source_view.length {
                values.push(get_element(interpreter, &source_view, index as f64));
            }
            for (index, value) in values.iter().enumerate() {
                if let Err(abrupt) = set_element(interpreter, &view, offset + index as f64, value) {
                    return abrupt;
                }
            }
            return Completion::Normal(JsValue::Undefined);
        }
        let values = match crate::builtins::array_like_to_vec(interpreter, &arg(arguments, 0)) {
            Ok(values) => values,
            Err(abrupt) => return abrupt,
        };
        if offset + values.len() as f64 > view.length as f64 {
            return interpreter.throw("RangeError", "the source is longer than the target");
        }
        for (index, value) in values.iter().enumerate() {
            if let Err(abrupt) = set_element(interpreter, &view, offset + index as f64, value) {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Undefined)
    });

    interpreter.define_method(prototype, "subarray", 2, |interpreter, this, arguments| {
        let view = match view_of(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let JsValue::Object(exemplar) = this else {
            return interpreter.internal_defect("a typed array view over a non-object");
        };
        let source_length =
            if interpreter.buffer_is_detached(view.buffer) { 0 } else { view.length };
        let (start, end) = match range(interpreter, arguments, 0, source_length) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let begin_byte_offset = view.offset + start * view.element.width();
        let new_length = end.saturating_sub(start);
        let arguments = crate::vec![
            JsValue::Object(view.buffer),
            JsValue::Number(begin_byte_offset as f64),
            JsValue::Number(new_length as f64),
        ];
        match species_create(interpreter, exemplar, view.element, arguments) {
            Ok((id, _)) => Completion::Normal(JsValue::Object(id)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "slice", 2, |interpreter, this, arguments| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let (start, end) = match range(interpreter, arguments, 0, view.length) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let JsValue::Object(exemplar) = this else {
            return interpreter.internal_defect("a typed array view over a non-object");
        };
        let count = end.saturating_sub(start);
        let arguments = crate::vec![JsValue::Number(count as f64)];
        let (id, target) = match species_create(interpreter, exemplar, view.element, arguments) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        if count > 0 {
            if interpreter.buffer_is_detached(view.buffer) {
                return interpreter.type_error("this ArrayBuffer is detached");
            }
            for offset in 0..count {
                let value = get_element(interpreter, &view, (start + offset) as f64);
                if let Err(abrupt) = set_element(interpreter, &target, offset as f64, &value) {
                    return abrupt;
                }
            }
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "indexOf", 1, |interpreter, this, arguments| {
        search(interpreter, this, arguments, false, false)
    });
    interpreter.define_method(prototype, "lastIndexOf", 1, |interpreter, this, arguments| {
        search(interpreter, this, arguments, true, false)
    });
    interpreter.define_method(prototype, "includes", 1, |interpreter, this, arguments| {
        let found = match search(interpreter, this, arguments, false, true) {
            Completion::Normal(JsValue::Number(index)) => index >= 0.0,
            abrupt => return abrupt,
        };
        Completion::Normal(JsValue::Boolean(found))
    });

    interpreter.define_method(prototype, "join", 1, |interpreter, this, arguments| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let separator = match arg(arguments, 0) {
            JsValue::Undefined => String::from(","),
            value => match interpreter.to_string_value(&value) {
                Ok(text) => text.to_lossy_string(),
                Err(abrupt) => return abrupt,
            },
        };
        let mut out = String::new();
        for index in 0..view.length {
            if index > 0 {
                out.push_str(&separator);
            }
            if let JsValue::Number(number) = get_element(interpreter, &view, index as f64) {
                out.push_str(&crate::abstract_ops::number_to_string(number));
            }
        }
        Completion::Normal(JsValue::string(&out))
    });

    interpreter.define_method(prototype, "reverse", 0, |interpreter, this, _| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let mut low = 0usize;
        let mut high = view.length.saturating_sub(1);
        while low < high {
            let a = get_element(interpreter, &view, low as f64);
            let b = get_element(interpreter, &view, high as f64);
            if let Err(abrupt) = set_element(interpreter, &view, low as f64, &b) {
                return abrupt;
            }
            if let Err(abrupt) = set_element(interpreter, &view, high as f64, &a) {
                return abrupt;
            }
            low += 1;
            high -= 1;
        }
        Completion::Normal(this)
    });

    interpreter.define_method(prototype, "copyWithin", 2, |interpreter, this, arguments| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let length = view.length;
        let target = match relative(interpreter, &arg(arguments, 0), length, 0) {
            Ok(index) => index,
            Err(abrupt) => return abrupt,
        };
        let (start, end) = match range(interpreter, arguments, 1, length) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let count = end.saturating_sub(start).min(length - target);
        if count > 0 && interpreter.buffer_is_detached(view.buffer) {
            return interpreter.type_error("this ArrayBuffer is detached");
        }
        let mut taken = Vec::with_capacity(count);
        for offset in 0..count {
            taken.push(get_element(interpreter, &view, (start + offset) as f64));
        }
        for (offset, value) in taken.iter().enumerate() {
            if let Err(abrupt) = set_element(interpreter, &view, (target + offset) as f64, value) {
                return abrupt;
            }
        }
        Completion::Normal(this)
    });

    interpreter.define_method(prototype, "sort", 1, |interpreter, this, arguments| {
        let comparator = arg(arguments, 0);
        if !matches!(comparator, JsValue::Undefined) && !interpreter.is_callable(&comparator) {
            return interpreter.type_error("the comparator is not a function");
        }
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let values = match sorted_elements(interpreter, &view, &comparator) {
            Ok(values) => values,
            Err(abrupt) => return abrupt,
        };
        for (index, value) in values.iter().enumerate() {
            if let Err(abrupt) = set_element(interpreter, &view, index as f64, value) {
                return abrupt;
            }
        }
        Completion::Normal(this)
    });

    install_copy_methods(interpreter, prototype);

    for (index, name) in ITERATION_NAMES.iter().enumerate() {
        interpreter.define_method(prototype, name, 1, ITERATION_BODIES[index]);
    }
    for (index, name) in ["reduce", "reduceRight"].iter().enumerate() {
        interpreter.define_method(prototype, name, 1, REDUCE_BODIES[index]);
    }

    let values = interpreter.native_function("values", 0, |interpreter, this, _| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let _ = view;
        crate::iterator::create_array_iterator(interpreter, &this, crate::iterator::IterationKind::Value)
    });
    interpreter
        .object_mut(prototype)
        .set_own(PropertyKey::from_str("values"), Property::builtin(JsValue::Object(values)));
    let iterator_key = PropertyKey::Symbol(interpreter.well_known_symbol("iterator"));
    interpreter
        .object_mut(prototype)
        .set_own(iterator_key, Property::builtin(JsValue::Object(values)));
    interpreter.define_method(prototype, "keys", 0, |interpreter, this, _| {
        if let Err(abrupt) = validated_view(interpreter, &this) {
            return abrupt;
        }
        crate::iterator::create_array_iterator(interpreter, &this, crate::iterator::IterationKind::Key)
    });
    interpreter.define_method(prototype, "entries", 0, |interpreter, this, _| {
        if let Err(abrupt) = validated_view(interpreter, &this) {
            return abrupt;
        }
        crate::iterator::create_array_iterator(interpreter, &this, crate::iterator::IterationKind::KeyAndValue)
    });

    interpreter.define_method(prototype, "toLocaleString", 0, |interpreter, this, _| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let mut out = String::new();
        for index in 0..view.length {
            if index > 0 {
                out.push_str(",");
            }
            let element = get_element(interpreter, &view, index as f64);
            if matches!(element, JsValue::Undefined | JsValue::Null) {
                continue;
            }
            let rendered = match interpreter.invoke(&element, "toLocaleString", crate::Vec::new()) {
                Completion::Normal(value) => value,
                abrupt => return abrupt,
            };
            match interpreter.to_string_value(&rendered) {
                Ok(text) => out.push_str(&text.to_lossy_string()),
                Err(abrupt) => return abrupt,
            }
        }
        Completion::Normal(JsValue::string(&out))
    });

    let array_join = interpreter.intrinsics.array_prototype;
    if let Some(to_string) = interpreter
        .object(array_join)
        .own(&PropertyKey::from_str("toString"))
        .and_then(|property| property.data_value().cloned())
    {
        interpreter
            .object_mut(prototype)
            .set_own(PropertyKey::from_str("toString"), Property::builtin(to_string));
    }
}

/// The seven callback methods, which differ only in what they do with each result.
///
/// NONE OF THEM SKIPS ANYTHING. `Array.prototype.forEach` steps over a HOLE and a typed array
/// has none -- every index in range is present, always -- so the `HasProperty` guard that makes the
/// array versions correct would be dead code here, and copying it in would suggest a hole is
/// possible.
const ITERATION_NAMES: [&str; 9] =
    ["forEach", "map", "filter", "every", "some", "find", "findIndex", "findLast", "findLastIndex"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    ForEach,
    Map,
    Filter,
    Every,
    Some,
    Find,
    FindIndex,
    FindLast,
    FindLastIndex,
}

const ITERATION_BODIES: [crate::interpreter::NativeFn; 9] = [
    |i, t, a| walk(i, t, a, Walk::ForEach),
    |i, t, a| walk(i, t, a, Walk::Map),
    |i, t, a| walk(i, t, a, Walk::Filter),
    |i, t, a| walk(i, t, a, Walk::Every),
    |i, t, a| walk(i, t, a, Walk::Some),
    |i, t, a| walk(i, t, a, Walk::Find),
    |i, t, a| walk(i, t, a, Walk::FindIndex),
    |i, t, a| walk(i, t, a, Walk::FindLast),
    |i, t, a| walk(i, t, a, Walk::FindLastIndex),
];

const REDUCE_BODIES: [crate::interpreter::NativeFn; 2] =
    [|i, t, a| reduce(i, t, a, false), |i, t, a| reduce(i, t, a, true)];

fn walk(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    kind: Walk,
) -> Completion {
    let view = match validated_view(interpreter, &this) {
        Ok(view) => view,
        Err(abrupt) => return abrupt,
    };
    let callback = arg(arguments, 0);
    if !interpreter.is_callable(&callback) {
        return interpreter.type_error("the callback is not a function");
    }
    let receiver = arg(arguments, 1);
    let length = view.length;
    let JsValue::Object(exemplar) = this else {
        return interpreter.internal_defect("a typed array view over a non-object");
    };
    let target = match kind {
        Walk::Map => {
            let arguments = crate::vec![JsValue::Number(length as f64)];
            match species_create(interpreter, exemplar, view.element, arguments) {
                Ok((id, _)) => Some(id),
                Err(abrupt) => return abrupt,
            }
        }
        _ => None,
    };
    let backwards = matches!(kind, Walk::FindLast | Walk::FindLastIndex);
    let mut kept: Vec<JsValue> = Vec::new();
    for step in 0..length {
        let index = if backwards { length - 1 - step } else { step };
        let element = get_element(interpreter, &view, index as f64);
        let result = match interpreter.call_value(
            &callback,
            receiver.clone(),
            crate::vec![element.clone(), JsValue::Number(index as f64), this.clone()],
        ) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        let truthy = crate::abstract_ops::to_boolean(&result);
        match kind {
            Walk::ForEach => {}
            Walk::Map => {
                let Some(target) = target else { unreachable!("map allocated above") };
                let Some(out) = interpreter.typed_array_view(target) else {
                    return interpreter.internal_defect("a fresh typed array with no view");
                };
                if let Err(abrupt) = set_element(interpreter, &out, index as f64, &result) {
                    return abrupt;
                }
            }
            Walk::Filter => {
                if truthy {
                    kept.push(element);
                }
            }
            Walk::Every => {
                if !truthy {
                    return Completion::Normal(JsValue::Boolean(false));
                }
            }
            Walk::Some => {
                if truthy {
                    return Completion::Normal(JsValue::Boolean(true));
                }
            }
            Walk::Find | Walk::FindLast => {
                if truthy {
                    return Completion::Normal(element);
                }
            }
            Walk::FindIndex | Walk::FindLastIndex => {
                if truthy {
                    return Completion::Normal(JsValue::Number(index as f64));
                }
            }
        }
    }
    match kind {
        Walk::ForEach => Completion::Normal(JsValue::Undefined),
        Walk::Map => {
            Completion::Normal(JsValue::Object(target.expect("map allocated above")))
        }
        Walk::Filter => {
            let arguments = crate::vec![JsValue::Number(kept.len() as f64)];
            let (id, out) = match species_create(interpreter, exemplar, view.element, arguments) {
                Ok(pair) => pair,
                Err(abrupt) => return abrupt,
            };
            for (index, value) in kept.iter().enumerate() {
                if let Err(abrupt) = set_element(interpreter, &out, index as f64, value) {
                    return abrupt;
                }
            }
            Completion::Normal(JsValue::Object(id))
        }
        Walk::Every => Completion::Normal(JsValue::Boolean(true)),
        Walk::Some => Completion::Normal(JsValue::Boolean(false)),
        Walk::Find | Walk::FindLast => Completion::Normal(JsValue::Undefined),
        Walk::FindIndex | Walk::FindLastIndex => Completion::Normal(JsValue::Number(-1.0)),
    }
}

fn reduce(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    backwards: bool,
) -> Completion {
    let view = match validated_view(interpreter, &this) {
        Ok(view) => view,
        Err(abrupt) => return abrupt,
    };
    let callback = arg(arguments, 0);
    if !interpreter.is_callable(&callback) {
        return interpreter.type_error("the callback is not a function");
    }
    let length = view.length as isize;
    let step: isize = if backwards { -1 } else { 1 };
    let mut index: isize = if backwards { length - 1 } else { 0 };
    let mut accumulator = if arguments.len() > 1 {
        arg(arguments, 1)
    } else {
        if length == 0 {
            return interpreter.type_error("reduce of an empty typed array with no initial value");
        }
        let first = get_element(interpreter, &view, index as f64);
        index += step;
        first
    };
    while index >= 0 && index < length {
        let element = get_element(interpreter, &view, index as f64);
        accumulator = match interpreter.call_value(
            &callback,
            JsValue::Undefined,
            crate::vec![
                accumulator,
                element,
                JsValue::Number(index as f64),
                this.clone()
            ],
        ) {
            Completion::Normal(value) => value,
            abrupt => return abrupt,
        };
        index += step;
    }
    Completion::Normal(accumulator)
}

/// The number a typed array element holds. Every element of every array this module makes is one.
fn number_of(value: &JsValue) -> f64 {
    match value {
        JsValue::Number(number) => *number,
        _ => f64::NAN,
    }
}

/// `toReversed`, `toSorted` and `with`: the three methods that answer a NEW typed array.
///
/// # `TypedArrayCreateSameType`, WHICH IS NOT `TypedArraySpeciesCreate`
///
/// The result is built from the intrinsic constructor for the receiver's OWN element type,
/// deliberately ignoring `Symbol.species` -- so `new MyUint8Array([1]).toReversed()` is a plain
/// `Uint8Array`. `slice` and `subarray` consult species and these three do not, which is the one
/// difference a reader coming from the method above would not expect.
///
/// There is no `toSpliced` here. It exists on `Array.prototype` only: a typed array has a fixed
/// length, so a method that changes it has nothing to build.
fn install_copy_methods(interpreter: &mut Interpreter, prototype: ObjectId) {
    interpreter.define_method(prototype, "toReversed", 0, |interpreter, this, _| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let (id, target) = match same_type_result(interpreter, &view) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        for index in 0..view.length {
            let value = get_element(interpreter, &view, (view.length - index - 1) as f64);
            if let Err(abrupt) = set_element(interpreter, &target, index as f64, &value) {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "toSorted", 1, |interpreter, this, arguments| {
        let comparator = arg(arguments, 0);
        if !matches!(comparator, JsValue::Undefined) && !interpreter.is_callable(&comparator) {
            return interpreter.type_error("the comparator is not a function");
        }
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let (id, target) = match same_type_result(interpreter, &view) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        let values = match sorted_elements(interpreter, &view, &comparator) {
            Ok(values) => values,
            Err(abrupt) => return abrupt,
        };
        for (index, value) in values.iter().enumerate() {
            if let Err(abrupt) = set_element(interpreter, &target, index as f64, value) {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Object(id))
    });

    interpreter.define_method(prototype, "with", 2, |interpreter, this, arguments| {
        let view = match validated_view(interpreter, &this) {
            Ok(view) => view,
            Err(abrupt) => return abrupt,
        };
        let length = view.length as f64;
        let relative = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => crate::builtins::to_integer(number),
            Err(abrupt) => return abrupt,
        };
        let at = if relative >= 0.0 { relative } else { length + relative };
        let replacement = match interpreter.to_number_value(&arg(arguments, 1)) {
            Ok(number) => JsValue::Number(number),
            Err(abrupt) => return abrupt,
        };
        if at < 0.0 || at >= length || interpreter.buffer_is_detached(view.buffer) {
            return interpreter.throw("RangeError", "that index is outside the typed array");
        }
        let (id, target) = match same_type_result(interpreter, &view) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        for index in 0..view.length {
            let value = if index as f64 == at {
                replacement.clone()
            } else {
                get_element(interpreter, &view, index as f64)
            };
            if let Err(abrupt) = set_element(interpreter, &target, index as f64, &value) {
                return abrupt;
            }
        }
        Completion::Normal(JsValue::Object(id))
    });
}

/// `TypedArrayCreateSameType(exemplar, length)` with the fresh array's view already resolved, which
/// is the pair every caller here needs.
fn same_type_result(
    interpreter: &mut Interpreter,
    view: &View,
) -> Result<(ObjectId, View), Completion> {
    let id = match allocate(interpreter, view.element, view.length) {
        Completion::Normal(JsValue::Object(id)) => id,
        other => return Err(other),
    };
    match interpreter.typed_array_view(id) {
        Some(target) => Ok((id, target)),
        None => Err(interpreter.internal_defect("a fresh typed array with no view")),
    }
}

/// `SortIndexedProperties` over a typed array: every element read out in order and sorted, with
/// nothing written back.
///
/// # `sort` AND `toSorted` DIFFER ONLY IN WHERE THEY PUT THE ANSWER
///
/// One edits the receiver and the other fills a fresh array of the same type. The COMPARISON is the
/// same rule and is therefore one function -- and this comparison is the one that is easy to get
/// wrong twice, because:
///
/// THE DEFAULT ORDER IS **NUMERIC**, NOT THE STRING ORDER `Array.prototype.sort` USES.
/// `new Int8Array([10, 9]).sort()` is `[9, 10]` and `[10, 9].sort()` is `[10, 9]`, from methods with
/// the same name. Reusing the array comparison here is the single easiest way to make this the wrong
/// function -- and writing it out a second time for `toSorted` is the second easiest.
fn sorted_elements(
    interpreter: &mut Interpreter,
    view: &View,
    comparator: &JsValue,
) -> Result<Vec<JsValue>, Completion> {
    let mut values = Vec::with_capacity(view.length);
    for index in 0..view.length {
        values.push(get_element(interpreter, view, index as f64));
    }
    if matches!(comparator, JsValue::Undefined) {
        values.sort_by(|a, b| {
            let (a, b) = (number_of(a), number_of(b));
            match (a.is_nan(), b.is_nan()) {
                (true, true) => core::cmp::Ordering::Equal,
                (true, false) => core::cmp::Ordering::Greater,
                (false, true) => core::cmp::Ordering::Less,
                (false, false) => {
                    if a < b {
                        core::cmp::Ordering::Less
                    } else if a > b {
                        core::cmp::Ordering::Greater
                    } else if a.is_sign_negative() && !b.is_sign_negative() {
                        core::cmp::Ordering::Less
                    } else if !a.is_sign_negative() && b.is_sign_negative() {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Equal
                    }
                }
            }
        });
    } else {
        sort_with(interpreter, &mut values, comparator)?;
    }
    Ok(values)
}

/// An insertion sort driven by a user comparator.
///
/// INSERTION RATHER THAN A LIBRARY SORT, because the comparator is a PROGRAM: it may throw, and
/// Rust's `sort_by` has no way to carry an abrupt completion out of a closure. It is also stable,
/// which the standard requires.
fn sort_with(
    interpreter: &mut Interpreter,
    values: &mut [JsValue],
    comparator: &JsValue,
) -> Result<(), Completion> {
    for i in 1..values.len() {
        let mut j = i;
        while j > 0 {
            let ordering = match interpreter.call_value(
                comparator,
                JsValue::Undefined,
                crate::vec![values[j - 1].clone(), values[j].clone()],
            ) {
                Completion::Normal(value) => interpreter.to_number_value(&value)?,
                abrupt => return Err(abrupt),
            };
            if !(ordering > 0.0) {
                break;
            }
            values.swap(j - 1, j);
            j -= 1;
        }
    }
    Ok(())
}

/// One relative index, clamped into `0..=length`.
fn relative(
    interpreter: &mut Interpreter,
    value: &JsValue,
    length: usize,
    fallback: usize,
) -> Result<usize, Completion> {
    if matches!(value, JsValue::Undefined) {
        return Ok(fallback);
    }
    let number = interpreter.to_number_value(value)?;
    let integer = crate::builtins::to_integer(number);
    Ok(if integer < 0.0 {
        (length as f64 + integer).max(0.0) as usize
    } else {
        integer.min(length as f64) as usize
    })
}

/// `[start, end)` from a pair of relative arguments -- the shape `fill`, `slice` and `subarray`
/// share.
fn range(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
    first: usize,
    length: usize,
) -> Result<(usize, usize), Completion> {
    let bound = |interpreter: &mut Interpreter, value: JsValue, fallback: f64| match value {
        JsValue::Undefined => Ok(fallback),
        value => {
            let number = interpreter.to_number_value(&value)?;
            let relative = crate::builtins::to_integer(number);
            Ok(if relative < 0.0 {
                (length as f64 + relative).max(0.0)
            } else {
                relative.min(length as f64)
            })
        }
    };
    let start = bound(interpreter, arg(arguments, first), 0.0)?;
    let end = bound(interpreter, arg(arguments, first + 1), length as f64)?;
    Ok((start as usize, end as usize))
}

/// `includes` COMPARES WITH SameValueZero AND THE TWO `indexOf`s WITH STRICT EQUALITY, which is
/// the ONE thing that separates them: `new Float64Array([NaN]).includes(NaN)` is `true` and
/// `.indexOf(NaN)` is -1. Building `includes` on the search that answers an index made the two agree
/// on NaN, and they are only ever asked about it by a test that knows they must not.
fn search(
    interpreter: &mut Interpreter,
    this: JsValue,
    arguments: &[JsValue],
    backwards: bool,
    same_value_zero: bool,
) -> Completion {
    let view = match validated_view(interpreter, &this) {
        Ok(view) => view,
        Err(abrupt) => return abrupt,
    };
    let target = arg(arguments, 0);
    let length = view.length as f64;
    if length == 0.0 {
        return Completion::Normal(JsValue::Number(-1.0));
    }
    let mut index = match arguments.get(1) {
        Some(value) => match interpreter.to_number_value(value) {
            Ok(number) => {
                let n = crate::builtins::to_integer(number);
                if backwards {
                    if n >= 0.0 {
                        n.min(length - 1.0)
                    } else {
                        length + n
                    }
                } else if n < 0.0 {
                    (length + n).max(0.0)
                } else {
                    n
                }
            }
            Err(abrupt) => return abrupt,
        },
        None => {
            if backwards {
                length - 1.0
            } else {
                0.0
            }
        }
    };
    while if backwards { index >= 0.0 } else { index < length } {
        let element = get_element(interpreter, &view, index);
        let found = if same_value_zero {
            crate::abstract_ops::same_value_zero(&element, &target)
        } else {
            crate::abstract_ops::strict_equals(&element, &target)
        };
        if found {
            return Completion::Normal(JsValue::Number(index));
        }
        index += if backwards { -1.0 } else { 1.0 };
    }
    Completion::Normal(JsValue::Number(-1.0))
}

/// The own keys of a TypedArray: every in-bounds index, ascending, then the ordinary ones.
///
/// THE INDICES ARE SYNTHESIZED rather than read from the property store, because they are not in
/// it -- they are bytes in a buffer. A detached array has none at all, which is why this needs the
/// interpreter and cannot live on `Object`.
pub(crate) fn own_keys(interpreter: &Interpreter, id: ObjectId, view: &View) -> Vec<PropertyKey> {
    let mut keys = Vec::new();
    if !interpreter.buffer_is_detached(view.buffer) {
        for index in 0..view.length {
            keys.push(PropertyKey::String(JsString::from(
                crate::abstract_ops::number_to_string(index as f64).as_str(),
            )));
        }
    }
    for key in interpreter.object(id).own_keys() {
        if canonical_numeric_index(&key).is_none() {
            keys.push(key);
        }
    }
    keys
}
