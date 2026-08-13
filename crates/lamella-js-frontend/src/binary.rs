//! `ArrayBuffer` and `DataView`: raw bytes, and a typed window onto them.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::object::{BinaryData, Object, Property, PropertyKey, PropertyKind};
use crate::value::{JsValue, ObjectId};
use crate::{Box, Vec};

pub(crate) fn install(interpreter: &mut Interpreter) {
    install_array_buffer(interpreter);
    install_data_view(interpreter);
}


fn install_array_buffer(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.array_buffer_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "ArrayBuffer",
        1,
        |interpreter, _this, _arguments| interpreter.type_error("this constructor requires `new`"),
        |interpreter, _this, arguments| {
            let length = match byte_length_argument(interpreter, &arg(arguments, 0)) {
                Ok(length) => length,
                Err(abrupt) => return abrupt,
            };
            Completion::Normal(JsValue::Object(new_buffer(interpreter, length)))
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("ArrayBuffer", JsValue::Object(constructor));
    interpreter.intrinsics.array_buffer_constructor = constructor;
    interpreter.define_species_getter(constructor);
    to_string_tag(interpreter, prototype, "ArrayBuffer");

    interpreter.define_method(constructor, "isView", 1, |interpreter, _this, arguments| {
        let is_view = match arg(arguments, 0) {
            JsValue::Object(id) => {
                matches!(interpreter.object(id).binary.as_deref(), Some(BinaryData::View { .. }))
            }
            _ => false,
        };
        Completion::Normal(JsValue::Boolean(is_view))
    });

    accessor(interpreter, prototype, "byteLength", |interpreter, this, _| {
        match buffer_bytes(interpreter, &this) {
            Ok((_, detached)) if detached => Completion::Normal(JsValue::Number(0.0)),
            Ok((length, _)) => Completion::Normal(JsValue::Number(length as f64)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "slice", 2, |interpreter, this, arguments| {
        let (length, detached) = match buffer_bytes(interpreter, &this) {
            Ok(pair) => pair,
            Err(abrupt) => return abrupt,
        };
        if detached {
            return interpreter.type_error("this ArrayBuffer is detached");
        }
        let start = match relative_index(interpreter, &arg(arguments, 0), length, 0) {
            Ok(index) => index,
            Err(abrupt) => return abrupt,
        };
        let end = match relative_index(interpreter, &arg(arguments, 1), length, length) {
            Ok(index) => index,
            Err(abrupt) => return abrupt,
        };
        let copy: Vec<u8> = {
            let JsValue::Object(id) = &this else { unreachable!("checked by buffer_bytes") };
            match interpreter.object(*id).binary.as_deref() {
                Some(BinaryData::Buffer { bytes, .. }) => {
                    bytes.get(start..end.max(start)).unwrap_or_default().to_vec()
                }
                _ => Vec::new(),
            }
        };
        let JsValue::Object(source) = &this else { unreachable!("checked by buffer_bytes") };
        let default = interpreter.intrinsics.array_buffer_constructor;
        let constructor = match interpreter.species_constructor(*source, default) {
            Ok(constructor) => constructor,
            Err(abrupt) => return abrupt,
        };
        let produced = match interpreter
            .construct(constructor, crate::vec![JsValue::Number(copy.len() as f64)])
        {
            Completion::Normal(JsValue::Object(id)) => id,
            Completion::Normal(_) => {
                return interpreter.type_error("the species constructor did not produce an object")
            }
            abrupt => return abrupt,
        };
        if produced == *source {
            return interpreter.type_error("the species constructor returned the same buffer");
        }
        let room = match interpreter.object(produced).binary.as_deref() {
            Some(BinaryData::Buffer { detached: true, .. }) => {
                return interpreter.type_error("the species constructor returned a detached buffer")
            }
            Some(BinaryData::Buffer { bytes, .. }) => bytes.len(),
            _ => return interpreter.type_error("the species constructor did not return a buffer"),
        };
        if room < copy.len() {
            return interpreter.type_error("the species constructor returned too small a buffer");
        }
        if let Some(BinaryData::Buffer { bytes, .. }) =
            interpreter.object_mut(produced).binary.as_deref_mut()
        {
            bytes[..copy.len()].copy_from_slice(&copy);
        }
        Completion::Normal(JsValue::Object(produced))
    });
}

fn new_buffer(interpreter: &mut Interpreter, length: usize) -> ObjectId {
    let prototype = interpreter.intrinsics.array_buffer_prototype;
    let mut object = Object::new(Some(prototype));
    object.binary =
        Some(Box::new(BinaryData::Buffer { bytes: crate::vec![0u8; length], detached: false }));
    interpreter.allocate(object)
}

/// `DetachArrayBuffer`, reachable only through the host harness hook.
///
/// DETACHING HAS NO SYNTAX AND NO STANDARD API. It happens as a side effect of transferring a
/// buffer to a worker, which this profile has none of -- so without a host hook the detached state
/// is UNREACHABLE, and every test about it tests nothing. The conformance suite's own interpreting
/// guide names `$262.detachArrayBuffer` for exactly this reason.
///
/// The bytes are dropped, not merely flagged. A detached buffer whose storage is still there
/// keeps the memory alive and would let a bug read it back; the flag and the release belong
/// together or the flag is a claim about something that did not happen.
pub(crate) fn detach(interpreter: &mut Interpreter, value: &JsValue) -> Completion {
    let JsValue::Object(id) = value else {
        return interpreter.type_error("detachArrayBuffer requires an ArrayBuffer");
    };
    match interpreter.object_mut(*id).binary.as_deref_mut() {
        Some(BinaryData::Buffer { bytes, detached }) => {
            *detached = true;
            bytes.clear();
            bytes.shrink_to_fit();
            Completion::Normal(JsValue::Undefined)
        }
        _ => interpreter.type_error("detachArrayBuffer requires an ArrayBuffer"),
    }
}

/// `this` as an ArrayBuffer: its length, and whether it is detached.
fn buffer_bytes(interpreter: &mut Interpreter, this: &JsValue) -> Result<(usize, bool), Completion> {
    if let JsValue::Object(id) = this {
        if let Some(BinaryData::Buffer { bytes, detached }) = interpreter.object(*id).binary.as_deref()
        {
            return Ok((bytes.len(), *detached));
        }
    }
    Err(interpreter.type_error("this method requires an ArrayBuffer"))
}


fn install_data_view(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.data_view_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "DataView",
        1,
        |interpreter, _this, _arguments| interpreter.type_error("this constructor requires `new`"),
        construct_view,
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("DataView", JsValue::Object(constructor));
    to_string_tag(interpreter, prototype, "DataView");

    accessor(interpreter, prototype, "buffer", |interpreter, this, _| {
        match view_of(interpreter, &this) {
            Ok((buffer, _, _)) => Completion::Normal(JsValue::Object(buffer)),
            Err(abrupt) => abrupt,
        }
    });
    accessor(interpreter, prototype, "byteLength", |interpreter, this, _| {
        match view_window(interpreter, &this) {
            Ok((_, _, length)) => Completion::Normal(JsValue::Number(length as f64)),
            Err(abrupt) => abrupt,
        }
    });
    accessor(interpreter, prototype, "byteOffset", |interpreter, this, _| {
        match view_window(interpreter, &this) {
            Ok((_, offset, _)) => Completion::Normal(JsValue::Number(offset as f64)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "getInt8", 1, |i, t, a| get(i, t, a, Kind::I8));
    interpreter.define_method(prototype, "getUint8", 1, |i, t, a| get(i, t, a, Kind::U8));
    interpreter.define_method(prototype, "getInt16", 1, |i, t, a| get(i, t, a, Kind::I16));
    interpreter.define_method(prototype, "getUint16", 1, |i, t, a| get(i, t, a, Kind::U16));
    interpreter.define_method(prototype, "getInt32", 1, |i, t, a| get(i, t, a, Kind::I32));
    interpreter.define_method(prototype, "getUint32", 1, |i, t, a| get(i, t, a, Kind::U32));
    interpreter.define_method(prototype, "getFloat32", 1, |i, t, a| get(i, t, a, Kind::F32));
    interpreter.define_method(prototype, "getFloat64", 1, |i, t, a| get(i, t, a, Kind::F64));
    interpreter.define_method(prototype, "setInt8", 2, |i, t, a| set(i, t, a, Kind::I8));
    interpreter.define_method(prototype, "setUint8", 2, |i, t, a| set(i, t, a, Kind::U8));
    interpreter.define_method(prototype, "setInt16", 2, |i, t, a| set(i, t, a, Kind::I16));
    interpreter.define_method(prototype, "setUint16", 2, |i, t, a| set(i, t, a, Kind::U16));
    interpreter.define_method(prototype, "setInt32", 2, |i, t, a| set(i, t, a, Kind::I32));
    interpreter.define_method(prototype, "setUint32", 2, |i, t, a| set(i, t, a, Kind::U32));
    interpreter.define_method(prototype, "setFloat32", 2, |i, t, a| set(i, t, a, Kind::F32));
    interpreter.define_method(prototype, "setFloat64", 2, |i, t, a| set(i, t, a, Kind::F64));
}

fn construct_view(
    interpreter: &mut Interpreter,
    _this: JsValue,
    arguments: &[JsValue],
) -> Completion {
    let JsValue::Object(buffer) = arg(arguments, 0) else {
        return interpreter.type_error("a DataView requires an ArrayBuffer");
    };
    if !matches!(interpreter.object(buffer).binary.as_deref(), Some(BinaryData::Buffer { .. })) {
        return interpreter.type_error("a DataView requires an ArrayBuffer");
    }

    let offset = match integer_argument(interpreter, &arg(arguments, 1)) {
        Ok(offset) => offset,
        Err(abrupt) => return abrupt,
    };
    let total = match interpreter.object(buffer).binary.as_deref() {
        Some(BinaryData::Buffer { bytes, detached: false }) => bytes.len(),
        Some(BinaryData::Buffer { detached: true, .. }) => {
            return interpreter.type_error("this ArrayBuffer is detached")
        }
        _ => return interpreter.type_error("a DataView requires an ArrayBuffer"),
    };
    if offset < 0.0 || offset > total as f64 {
        return interpreter.throw("RangeError", "the DataView offset is out of bounds");
    }
    let offset = offset as usize;

    let length = match arg(arguments, 2) {
        JsValue::Undefined => total - offset,
        value => {
            let requested = match integer_argument(interpreter, &value) {
                Ok(requested) => requested,
                Err(abrupt) => return abrupt,
            };
            if requested < 0.0 || offset as f64 + requested > total as f64 {
                return interpreter.throw("RangeError", "the DataView length is out of bounds");
            }
            requested as usize
        }
    };

    let prototype = interpreter.intrinsics.data_view_prototype;
    let mut object = Object::new(Some(prototype));
    object.binary = Some(Box::new(BinaryData::View { buffer, offset, length }));
    Completion::Normal(JsValue::Object(interpreter.allocate(object)))
}

/// `this` as a DataView: which buffer, and its window.
fn view_of(
    interpreter: &mut Interpreter,
    this: &JsValue,
) -> Result<(ObjectId, usize, usize), Completion> {
    if let JsValue::Object(id) = this {
        if let Some(BinaryData::View { buffer, offset, length }) =
            interpreter.object(*id).binary.as_deref()
        {
            return Ok((*buffer, *offset, *length));
        }
    }
    Err(interpreter.type_error("this method requires a DataView"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Kind {
    fn width(self) -> usize {
        match self {
            Kind::I8 | Kind::U8 => 1,
            Kind::I16 | Kind::U16 => 2,
            Kind::I32 | Kind::U32 | Kind::F32 => 4,
            Kind::F64 => 8,
        }
    }
}

/// The window a get/set touches, after every bounds rule. `Err` is the refusal.
fn window(
    interpreter: &mut Interpreter,
    this: &JsValue,
    arguments: &[JsValue],
    kind: Kind,
) -> Result<(ObjectId, usize), Completion> {
    let (buffer, offset, length) = view_of(interpreter, this)?;
    let index =
        match interpreter.to_index(&arg(arguments, 0), "the DataView index is not a valid index") {
            Ok(index) => index,
            Err(abrupt) => return Err(abrupt),
        };
    in_bounds(interpreter, buffer, offset, length, index, kind)
}

/// `view_of` plus the detached check, for the two accessors that refuse rather than report.
fn view_window(
    interpreter: &mut Interpreter,
    this: &JsValue,
) -> Result<(ObjectId, usize, usize), Completion> {
    let (buffer, offset, length) = view_of(interpreter, this)?;
    if interpreter.buffer_is_detached(buffer) {
        return Err(interpreter.type_error("this ArrayBuffer is detached"));
    }
    Ok((buffer, offset, length))
}

/// The detached check and the bounds check, which happen AFTER every coercion the call makes.
///
/// Separate from the coercions because `set` has one more of them and the order is observable:
/// its value coercion runs between the index's and this.
fn in_bounds(
    interpreter: &mut Interpreter,
    buffer: ObjectId,
    offset: usize,
    length: usize,
    index: usize,
    kind: Kind,
) -> Result<(ObjectId, usize), Completion> {
    let detached = matches!(
        interpreter.object(buffer).binary.as_deref(),
        Some(BinaryData::Buffer { detached: true, .. })
    );
    if detached {
        return Err(interpreter.type_error("this ArrayBuffer is detached"));
    }
    if index as f64 + kind.width() as f64 > length as f64 {
        return Err(interpreter.throw("RangeError", "the DataView access is out of bounds"));
    }
    Ok((buffer, offset + index))
}

fn get(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue], kind: Kind) -> Completion {
    let (buffer, at) = match window(interpreter, &this, arguments, kind) {
        Ok(pair) => pair,
        Err(abrupt) => return abrupt,
    };
    let little = kind.width() > 1 && crate::abstract_ops::to_boolean(&arg(arguments, 1));
    let mut raw = [0u8; 8];
    let width = kind.width();
    match interpreter.object(buffer).binary.as_deref() {
        Some(BinaryData::Buffer { bytes, .. }) => {
            raw[..width].copy_from_slice(&bytes[at..at + width]);
        }
        _ => return interpreter.type_error("this ArrayBuffer is detached"),
    }
    if !little {
        raw[..width].reverse();
    }
    let value = match kind {
        Kind::I8 => f64::from(raw[0] as i8),
        Kind::U8 => f64::from(raw[0]),
        Kind::I16 => f64::from(i16::from_le_bytes([raw[0], raw[1]])),
        Kind::U16 => f64::from(u16::from_le_bytes([raw[0], raw[1]])),
        Kind::I32 => f64::from(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Kind::U32 => f64::from(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Kind::F32 => f64::from(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Kind::F64 => f64::from_le_bytes(raw),
    };
    Completion::Normal(JsValue::Number(value))
}

fn set(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue], kind: Kind) -> Completion {
    let (buffer, offset, length) = match view_of(interpreter, &this) {
        Ok(view) => view,
        Err(abrupt) => return abrupt,
    };
    let index =
        match interpreter.to_index(&arg(arguments, 0), "the DataView index is not a valid index") {
            Ok(index) => index,
            Err(abrupt) => return abrupt,
        };
    let value = match interpreter.to_number_value(&arg(arguments, 1)) {
        Ok(value) => value,
        Err(abrupt) => return abrupt,
    };
    let (buffer, at) = match in_bounds(interpreter, buffer, offset, length, index, kind) {
        Ok(pair) => pair,
        Err(abrupt) => return abrupt,
    };
    let little = kind.width() > 1 && crate::abstract_ops::to_boolean(&arg(arguments, 2));
    let width = kind.width();
    let mut raw = [0u8; 8];
    match kind {
        Kind::I8 | Kind::U8 => raw[0] = wrap_to_u32(value) as u8,
        Kind::I16 | Kind::U16 => {
            raw[..2].copy_from_slice(&(wrap_to_u32(value) as u16).to_le_bytes())
        }
        Kind::I32 | Kind::U32 => raw[..4].copy_from_slice(&wrap_to_u32(value).to_le_bytes()),
        Kind::F32 => raw[..4].copy_from_slice(&(value as f32).to_le_bytes()),
        Kind::F64 => raw.copy_from_slice(&value.to_le_bytes()),
    }
    if !little {
        raw[..width].reverse();
    }
    match interpreter.object_mut(buffer).binary.as_deref_mut() {
        Some(BinaryData::Buffer { bytes, .. }) => {
            bytes[at..at + width].copy_from_slice(&raw[..width]);
        }
        _ => return interpreter.type_error("this ArrayBuffer is detached"),
    }
    Completion::Normal(JsValue::Undefined)
}

/// `ToUint32`: truncate toward zero, then take it modulo 2^32.
///
/// `value as u32` in Rust SATURATES -- `300.0 as u8` is 255 and `-1.0 as u32` is 0. JavaScript
/// wraps, so `setUint8(0, 300)` stores 44 and `setUint8(0, -1)` stores 255. Reaching for the cast
/// is the single easiest way to make this the wrong language.
///
/// Shared beyond this module because `ToUint32` is not a binary-data idea -- it is the standard's
/// conversion, and `String.prototype.split`'s limit needs exactly it: `split(s, -1)` must become
/// 4294967295 (unlimited), which the saturating cast would turn into 0 (the empty array).
pub(crate) fn wrap_to_u32(value: f64) -> u32 {
    if !value.is_finite() {
        return 0;
    }
    const TWO_32: f64 = 4_294_967_296.0;
    let truncated = crate::math::trunc(value);
    let wrapped = truncated - TWO_32 * crate::math::floor(truncated / TWO_32);
    wrapped as u32
}


/// A fresh zero-filled buffer of a size this part can actually hold.
///
/// THE CEILING IS THE SAME ONE `new ArrayBuffer(n)` ENFORCES, and it is here rather than at each
/// caller so that a typed array cannot go round it: `new Float64Array(2**30)` asks for 8 GB, and a
/// refusal is the only outcome available on a part with 264 KB.
pub(crate) fn new_buffer_checked(
    interpreter: &mut Interpreter,
    bytes: usize,
) -> Result<ObjectId, Completion> {
    if bytes as f64 > 64.0 * 1024.0 * 1024.0 {
        return Err(interpreter.throw("RangeError", "the requested ArrayBuffer is too large"));
    }
    Ok(new_buffer(interpreter, bytes))
}

/// `ToIndex` for a byte length: a non-negative integer that fits.
fn byte_length_argument(interpreter: &mut Interpreter, value: &JsValue) -> Result<usize, Completion> {
    if matches!(value, JsValue::Undefined) {
        return Ok(0);
    }
    let number = match interpreter.to_number_value(value) {
        Ok(number) => number,
        Err(abrupt) => return Err(abrupt),
    };
    let integer = if number.is_nan() { 0.0 } else { crate::math::trunc(number) };
    if integer < 0.0 || integer > 9_007_199_254_740_991.0 {
        return Err(interpreter.throw("RangeError", "invalid ArrayBuffer length"));
    }
    if integer > 64.0 * 1024.0 * 1024.0 {
        return Err(interpreter.throw("RangeError", "the requested ArrayBuffer is too large"));
    }
    Ok(integer as usize)
}

fn integer_argument(interpreter: &mut Interpreter, value: &JsValue) -> Result<f64, Completion> {
    if matches!(value, JsValue::Undefined) {
        return Ok(0.0);
    }
    let number = match interpreter.to_number_value(value) {
        Ok(number) => number,
        Err(abrupt) => return Err(abrupt),
    };
    Ok(if number.is_nan() { 0.0 } else { crate::math::trunc(number) })
}

/// A `start`/`end` argument: negative counts from the end, and both ends clamp.
fn relative_index(
    interpreter: &mut Interpreter,
    value: &JsValue,
    length: usize,
    default: usize,
) -> Result<usize, Completion> {
    if matches!(value, JsValue::Undefined) {
        return Ok(default);
    }
    let number = integer_argument(interpreter, value)?;
    let resolved = if number < 0.0 { length as f64 + number } else { number };
    Ok(resolved.max(0.0).min(length as f64) as usize)
}

fn accessor(
    interpreter: &mut Interpreter,
    target: ObjectId,
    name: &str,
    get: crate::interpreter::NativeFn,
) {
    let getter = interpreter.native_function(&crate::format!("get {name}"), 0, get);
    interpreter.object_mut(target).set_own(
        PropertyKey::from_str(name),
        Property {
            kind: PropertyKind::Accessor { get: Some(getter), set: None },
            enumerable: false,
            configurable: true,
        },
    );
}

fn to_string_tag(interpreter: &mut Interpreter, target: ObjectId, tag: &str) {
    interpreter.define_to_string_tag(target, tag);
}

