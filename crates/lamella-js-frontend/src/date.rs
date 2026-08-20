//! `Date`: the calendar, and the one number underneath it.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::object::Object;
use crate::value::{JsValue, ObjectId};
use crate::{format, String, Vec};

/// Milliseconds in a day, and the pieces of one.
const MS_PER_SECOND: f64 = 1000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;

/// THE RANGE IS 8.64e15 EITHER SIDE OF THE EPOCH, AND ANYTHING BEYOND IT IS `NaN` -- not a
/// clamp and not an error. `TimeClip` is what makes `new Date(8.64e15 + 1)` an Invalid Date, and an
/// implementation that clamps instead reports a real date for a value the standard says has none.
const MAX_TIME: f64 = 8.64e15;

pub(crate) fn install(interpreter: &mut Interpreter) {
    let object_prototype = interpreter.intrinsics.object_prototype;
    let prototype = interpreter.allocate(Object::new(Some(object_prototype)));
    interpreter.intrinsics.date_prototype = prototype;

    let constructor = interpreter.native_constructor(
        "Date",
        7,
        |interpreter, _this, _arguments| {
            let now = host_now(interpreter);
            Completion::Normal(JsValue::string(&render(now)))
        },
        |interpreter, _this, arguments| {
            let time = match arguments.len() {
                0 => host_now(interpreter),
                1 => match &arguments[0] {
                    JsValue::Object(id) if interpreter.object(*id).date.is_some() => {
                        interpreter.object(*id).date.unwrap_or(f64::NAN)
                    }
                    value => {
                        match interpreter
                            .to_primitive(value, crate::abstract_ops::Hint::Default)
                        {
                            Ok(JsValue::String(text)) => parse_date(&text.to_lossy_string()),
                            Ok(other) => match interpreter.to_number_value(&other) {
                                Ok(number) => time_clip(number),
                                Err(abrupt) => return abrupt,
                            },
                            Err(abrupt) => return abrupt,
                        }
                    }
                },
                _ => match components_from_arguments(interpreter, arguments) {
                    Ok(time) => time,
                    Err(abrupt) => return abrupt,
                },
            };
            Completion::Normal(JsValue::Object(new_date(interpreter, time)))
        },
    );
    interpreter.define_constant(constructor, "prototype", JsValue::Object(prototype));
    interpreter.define_builtin(prototype, "constructor", JsValue::Object(constructor));
    interpreter.define_global("Date", JsValue::Object(constructor));

    interpreter.define_method(constructor, "now", 0, |interpreter, _this, _arguments| {
        Completion::Normal(JsValue::Number(host_now(interpreter)))
    });

    interpreter.define_method(constructor, "UTC", 7, |interpreter, _this, arguments| {
        if arguments.is_empty() {
            return Completion::Normal(JsValue::Number(f64::NAN));
        }
        match components_from_arguments(interpreter, arguments) {
            Ok(time) => Completion::Normal(JsValue::Number(time)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(constructor, "parse", 1, |interpreter, _this, arguments| {
        match interpreter.to_string_value(&arg(arguments, 0)) {
            Ok(text) => Completion::Normal(JsValue::Number(parse_date(&text.to_lossy_string()))),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "valueOf", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) => Completion::Normal(JsValue::Number(time)),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.define_method(prototype, "getTime", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) => Completion::Normal(JsValue::Number(time)),
            Err(abrupt) => abrupt,
        }
    });

    for (index, name) in FIELD_NAMES.iter().enumerate() {
        interpreter.define_method(prototype, name, 0, FIELD_GETTERS[index]);
        interpreter.define_method(prototype, UTC_FIELD_NAMES[index], 0, FIELD_GETTERS[index]);
    }

    interpreter.define_method(prototype, "getTimezoneOffset", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::Number(f64::NAN)),
            Ok(_) => Completion::Normal(JsValue::Number(0.0)),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "setTime", 1, |interpreter, this, arguments| {
        let id = match this_date_object(interpreter, &this) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let time = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => time_clip(number),
            Err(abrupt) => return abrupt,
        };
        interpreter.object_mut(id).date = Some(time);
        Completion::Normal(JsValue::Number(time))
    });

    interpreter.define_method(prototype, "toISOString", 0, |interpreter, this, _| {
        let time = match this_time(interpreter, &this) {
            Ok(time) => time,
            Err(abrupt) => return abrupt,
        };
        if time.is_nan() {
            return interpreter.throw("RangeError", "an invalid date has no ISO representation");
        }
        Completion::Normal(JsValue::string(&iso_string(time)))
    });

    interpreter.define_method(prototype, "toJSON", 1, |interpreter, this, _| {
        let object = match interpreter.to_object(&this) {
            Ok(id) => JsValue::Object(id),
            Err(abrupt) => return abrupt,
        };
        match interpreter.to_primitive(&object, crate::abstract_ops::Hint::Number) {
            Ok(JsValue::Number(number)) if !number.is_finite() => {
                return Completion::Normal(JsValue::Null)
            }
            Ok(_) => {}
            Err(abrupt) => return abrupt,
        }
        interpreter.invoke(&object, "toISOString", Vec::new())
    });

    for (index, name) in SETTER_NAMES.iter().enumerate() {
        interpreter.define_method(prototype, name, SETTER_LENGTHS[index], SETTERS[index]);
        interpreter.define_method(
            prototype,
            UTC_SETTER_NAMES[index],
            SETTER_LENGTHS[index],
            SETTERS[index],
        );
    }

    interpreter.define_method(prototype, "getYear", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::Number(f64::NAN)),
            Ok(time) => Completion::Normal(JsValue::Number(components_of(time)[0] - 1900.0)),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.define_method(prototype, "setYear", 1, |interpreter, this, arguments| {
        let id = match this_date_object(interpreter, &this) {
            Ok(id) => id,
            Err(abrupt) => return abrupt,
        };
        let time = interpreter.object(id).date.unwrap_or(f64::NAN);
        let time = if time.is_nan() { 0.0 } else { time };
        let year = match interpreter.to_number_value(&arg(arguments, 0)) {
            Ok(number) => number,
            Err(abrupt) => return abrupt,
        };
        if year.is_nan() {
            interpreter.object_mut(id).date = Some(f64::NAN);
            return Completion::Normal(JsValue::Number(f64::NAN));
        }
        let truncated = crate::builtins::to_integer(year);
        let full = if (0.0..=99.0).contains(&truncated) { 1900.0 + truncated } else { year };
        let held = components_of(time);
        let composed = make_date(
            make_day(full, held[1], held[2]),
            make_time(held[3], held[4], held[5], held[6]),
        );
        let clipped = time_clip(composed);
        interpreter.object_mut(id).date = Some(clipped);
        Completion::Normal(JsValue::Number(clipped))
    });

    interpreter.define_method(prototype, "toString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) => Completion::Normal(JsValue::string(&render(time))),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "toUTCString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::string("Invalid Date")),
            Ok(time) => Completion::Normal(JsValue::string(&utc_string(time))),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "toDateString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::string("Invalid Date")),
            Ok(time) => Completion::Normal(JsValue::string(&date_string(time))),
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "toTimeString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::string("Invalid Date")),
            Ok(time) => {
                Completion::Normal(JsValue::string(&format!("{}{TIME_ZONE}", time_string(time))))
            }
            Err(abrupt) => abrupt,
        }
    });

    interpreter.define_method(prototype, "toLocaleString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) => Completion::Normal(JsValue::string(&render(time))),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.define_method(prototype, "toLocaleDateString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::string("Invalid Date")),
            Ok(time) => Completion::Normal(JsValue::string(&date_string(time))),
            Err(abrupt) => abrupt,
        }
    });
    interpreter.define_method(prototype, "toLocaleTimeString", 0, |interpreter, this, _| {
        match this_time(interpreter, &this) {
            Ok(time) if time.is_nan() => Completion::Normal(JsValue::string("Invalid Date")),
            Ok(time) => {
                Completion::Normal(JsValue::string(&format!("{}{TIME_ZONE}", time_string(time))))
            }
            Err(abrupt) => abrupt,
        }
    });

    let to_primitive = interpreter.native_function(
        "[Symbol.toPrimitive]",
        1,
        |interpreter, this, arguments| {
            if !this.is_object() {
                return interpreter.type_error("Symbol.toPrimitive requires an object");
            }
            let first = match arg(arguments, 0) {
                JsValue::String(text) => match text.to_lossy_string().as_str() {
                    "string" | "default" => crate::abstract_ops::Hint::String,
                    "number" => crate::abstract_ops::Hint::Number,
                    _ => return interpreter.type_error("an invalid hint for Symbol.toPrimitive"),
                },
                _ => return interpreter.type_error("an invalid hint for Symbol.toPrimitive"),
            };
            match interpreter.ordinary_to_primitive(&this, first) {
                Ok(value) => Completion::Normal(value),
                Err(abrupt) => abrupt,
            }
        },
    );
    interpreter.define_well_known_symbol_method(prototype, "toPrimitive", to_primitive);
}

const FIELD_NAMES: [&str; 8] = [
    "getFullYear",
    "getMonth",
    "getDate",
    "getDay",
    "getHours",
    "getMinutes",
    "getSeconds",
    "getMilliseconds",
];
const UTC_FIELD_NAMES: [&str; 8] = [
    "getUTCFullYear",
    "getUTCMonth",
    "getUTCDate",
    "getUTCDay",
    "getUTCHours",
    "getUTCMinutes",
    "getUTCSeconds",
    "getUTCMilliseconds",
];

/// A `fn` pointer carries no captured state, so the field cannot be closed over -- a table
/// parallel to the names, the same shape `ERROR_MAKERS` uses.
const FIELD_GETTERS: [crate::interpreter::NativeFn; 8] = [
    |i, t, _| field(i, &t, Field::Year),
    |i, t, _| field(i, &t, Field::Month),
    |i, t, _| field(i, &t, Field::DayOfMonth),
    |i, t, _| field(i, &t, Field::WeekDay),
    |i, t, _| field(i, &t, Field::Hours),
    |i, t, _| field(i, &t, Field::Minutes),
    |i, t, _| field(i, &t, Field::Seconds),
    |i, t, _| field(i, &t, Field::Milliseconds),
];

/// THE DISCRIMINANTS INDEX [`components_of`] AND ARE NOT DECORATION. `WeekDay` sits past the end
/// on purpose: a weekday is derived rather than stored, and nothing can set one.
#[derive(Clone, Copy)]
enum Field {
    Year = 0,
    Month = 1,
    DayOfMonth = 2,
    Hours = 3,
    Minutes = 4,
    Seconds = 5,
    Milliseconds = 6,
    WeekDay = 7,
}

fn field(interpreter: &mut Interpreter, this: &JsValue, which: Field) -> Completion {
    let time = match this_time(interpreter, this) {
        Ok(time) => time,
        Err(abrupt) => return abrupt,
    };
    if time.is_nan() {
        return Completion::Normal(JsValue::Number(f64::NAN));
    }
    let value = match which {
        Field::WeekDay => euclid_mod(day_number(time) + 4.0, 7.0),
        component => components_of(time)[component as usize],
    };
    Completion::Normal(JsValue::Number(value))
}

/// The seven components the calendar decomposes into, in the order the setters take them.
///
/// ONE DECOMPOSITION SERVES BOTH DIRECTIONS. The getters and the setters ask the identical
/// question -- what year, month, day, hour, minute, second and millisecond is this time value? -- and
/// a second copy of that answer is a list that can disagree with itself while each half looks
/// complete. The setters need the components they are NOT writing, which is exactly the reason a
/// second copy would otherwise have been written here.
fn components_of(time: f64) -> [f64; 7] {
    let (year, month, day_of_month) = civil_from_days(day_number(time));
    [
        f64::from(year),
        f64::from(month),
        f64::from(day_of_month),
        euclid_mod(crate::math::floor(time / MS_PER_HOUR), 24.0),
        euclid_mod(crate::math::floor(time / MS_PER_MINUTE), 60.0),
        euclid_mod(crate::math::floor(time / MS_PER_SECOND), 60.0),
        euclid_mod(time, MS_PER_SECOND),
    ]
}

const SETTER_NAMES: [&str; 7] = [
    "setFullYear",
    "setMonth",
    "setDate",
    "setHours",
    "setMinutes",
    "setSeconds",
    "setMilliseconds",
];
const UTC_SETTER_NAMES: [&str; 7] = [
    "setUTCFullYear",
    "setUTCMonth",
    "setUTCDate",
    "setUTCHours",
    "setUTCMinutes",
    "setUTCSeconds",
    "setUTCMilliseconds",
];

/// THE `length` IS HOW MANY COMPONENTS THE SETTER CAN STILL WRITE, and it is not 1. `setHours`
/// is 4 and `setMilliseconds` is 1; the suite checks the property on every one of them, so a
/// uniform 1 is wrong for five of the seven and looks like the oversight it is.
const SETTER_LENGTHS: [u32; 7] = [3, 2, 1, 4, 3, 2, 1];

/// Each setter, keyed by the [`components_of`] index it starts writing at.
const SETTERS: [crate::interpreter::NativeFn; 7] = [
    |i, t, a| set_components(i, &t, a, 0),
    |i, t, a| set_components(i, &t, a, 1),
    |i, t, a| set_components(i, &t, a, 2),
    |i, t, a| set_components(i, &t, a, 3),
    |i, t, a| set_components(i, &t, a, 4),
    |i, t, a| set_components(i, &t, a, 5),
    |i, t, a| set_components(i, &t, a, 6),
];

/// Writes a run of components starting at `start`, re-composing the time value around them.
fn set_components(
    interpreter: &mut Interpreter,
    this: &JsValue,
    arguments: &[JsValue],
    start: usize,
) -> Completion {
    let id = match this_date_object(interpreter, this) {
        Ok(id) => id,
        Err(abrupt) => return abrupt,
    };
    let mut time = interpreter.object(id).date.unwrap_or(f64::NAN);
    let last = if start <= 2 { 2 } else { 6 };

    let mut given: [Option<f64>; 7] = [None; 7];
    for slot in start..=last {
        let offset = slot - start;
        if offset > 0 && arguments.len() <= offset {
            continue;
        }
        match interpreter.to_number_value(&arg(arguments, offset)) {
            Ok(number) => given[slot] = Some(number),
            Err(abrupt) => return abrupt,
        }
    }

    if time.is_nan() {
        if start != 0 {
            return Completion::Normal(JsValue::Number(f64::NAN));
        }
        time = 0.0;
    }

    let held = components_of(time);
    let parts: [f64; 7] = core::array::from_fn(|index| given[index].unwrap_or(held[index]));
    let composed = make_date(
        make_day(parts[0], parts[1], parts[2]),
        make_time(parts[3], parts[4], parts[5], parts[6]),
    );
    let clipped = time_clip(composed);
    interpreter.object_mut(id).date = Some(clipped);
    Completion::Normal(JsValue::Number(clipped))
}

/// A fresh `Date` object holding a time value.
fn new_date(interpreter: &mut Interpreter, time: f64) -> ObjectId {
    let prototype = interpreter.intrinsics.date_prototype;
    let mut object = Object::new(Some(prototype));
    object.date = Some(time_clip(time));
    interpreter.allocate(object)
}

/// THE TIME VALUE IS A SLOT, for the reason `primitive` is: it is state a program can neither
/// read, write nor forge, and `Date.prototype.getTime.call({})` is a TypeError BECAUSE a plain
/// object has none. Hidden properties would make the brand check forgeable.
fn this_time(interpreter: &mut Interpreter, this: &JsValue) -> Result<f64, Completion> {
    if let JsValue::Object(id) = this {
        if let Some(time) = interpreter.object(*id).date {
            return Ok(time);
        }
    }
    Err(interpreter.type_error("this method requires a Date"))
}

fn this_date_object(interpreter: &mut Interpreter, this: &JsValue) -> Result<ObjectId, Completion> {
    if let JsValue::Object(id) = this {
        if interpreter.object(*id).date.is_some() {
            return Ok(*id);
        }
    }
    Err(interpreter.type_error("this method requires a Date"))
}

/// `Date.now()`: the anchor plus however much has elapsed since it was taken.
///
/// WITH NO HOST SEAM THIS IS THE EPOCH AND DOES NOT ADVANCE, which is C#'s third case and is the
/// honest answer for a build whose embedder installed nothing: the engine cannot invent elapsed time
/// it has no source for. An embedder that installs only the monotonic half gets a clock that starts
/// at 1970 and counts correctly, which is the case every board takes today.
fn host_now(interpreter: &mut Interpreter) -> f64 {
    interpreter.host_now_millis()
}

fn time_clip(time: f64) -> f64 {
    if !time.is_finite() || crate::math::abs(time) > MAX_TIME {
        return f64::NAN;
    }
    crate::math::trunc(time) + 0.0
}

fn day_number(time: f64) -> f64 {
    crate::math::floor(time / MS_PER_DAY)
}

/// A modulus that is always non-negative, which `%` is not for a negative left operand.
///
/// `-1 % 7` is `-1` in Rust and in JavaScript, and a weekday of -1 is not a weekday. Every field
/// below the day needs this, because a time value BEFORE the epoch is negative and every one of its
/// fields would come out negative with the ordinary operator.
fn euclid_mod(value: f64, modulus: f64) -> f64 {
    let remainder = value - modulus * crate::math::floor(value / modulus);
    remainder
}

/// Days since the epoch to (year, month 0-11, day 1-31), by the proleptic Gregorian calendar.
///
/// THE CALENDAR IS PROLEPTIC AND UNLIMITED IN BOTH DIRECTIONS. ECMAScript dates run to roughly
/// +/-273,790 years, so a table of month lengths per year does not work and the leap rule has to be
/// arithmetic. This is the standard shift-the-epoch-to-March technique: with March as month 0 the
/// leap day lands at the END of the year and stops being a special case in the middle of it.
fn civil_from_days(days: f64) -> (i32, u32, u32) {
    let z = days + 719_468.0;
    let era = crate::math::floor(z / 146_097.0);
    let day_of_era = z - era * 146_097.0;
    let year_of_era = crate::math::floor(
        (day_of_era - crate::math::floor(day_of_era / 1460.0)
            + crate::math::floor(day_of_era / 36_524.0)
            - crate::math::floor(day_of_era / 146_096.0))
            / 365.0,
    );
    let year = year_of_era + era * 400.0;
    let day_of_year = day_of_era
        - (365.0 * year_of_era
            + crate::math::floor(year_of_era / 4.0)
            - crate::math::floor(year_of_era / 100.0));
    let shifted_month = crate::math::floor((5.0 * day_of_year + 2.0) / 153.0);
    let day = day_of_year - crate::math::floor((153.0 * shifted_month + 2.0) / 5.0) + 1.0;
    let month = if shifted_month < 10.0 { shifted_month + 2.0 } else { shifted_month - 10.0 };
    let year = if month < 2.0 { year + 1.0 } else { year };
    (year as i32, month as u32, day as u32)
}

/// The inverse: (year, month 0-11, day) to days since the epoch.
fn days_from_civil(year: f64, month: f64, day: f64) -> f64 {
    let year = year + crate::math::floor(month / 12.0);
    let month = euclid_mod(month, 12.0);
    let year = if month < 2.0 { year - 1.0 } else { year };
    let era = crate::math::floor(year / 400.0);
    let year_of_era = year - era * 400.0;
    let shifted_month = if month >= 2.0 { month - 2.0 } else { month + 10.0 };
    let day_of_year = crate::math::floor((153.0 * shifted_month + 2.0) / 5.0) + day - 1.0;
    let day_of_era = year_of_era * 365.0 + crate::math::floor(year_of_era / 4.0)
        - crate::math::floor(year_of_era / 100.0)
        + day_of_year;
    era * 146_097.0 + day_of_era - 719_468.0
}

/// `MakeTime`: four numbers to a count of milliseconds within a day.
///
/// THE COMPONENTS ARE TRUNCATED, NOT ROUNDED, and none of them is range-checked. `MakeTime(25, 0,
/// 0, 0)` is a whole day plus an hour, which is what makes `d.setHours(25)` roll into tomorrow.
fn make_time(hour: f64, minute: f64, second: f64, millisecond: f64) -> f64 {
    if !hour.is_finite() || !minute.is_finite() || !second.is_finite() || !millisecond.is_finite() {
        return f64::NAN;
    }
    crate::math::trunc(hour) * MS_PER_HOUR
        + crate::math::trunc(minute) * MS_PER_MINUTE
        + crate::math::trunc(second) * MS_PER_SECOND
        + crate::math::trunc(millisecond)
}

/// `MakeDay`: a year, a month and a day of the month to a count of days since the epoch.
///
/// AN OUT-OF-RANGE YEAR CANNOT PRODUCE A PLAUSIBLE SMALL ANSWER HERE, and that is worth stating
/// because the standard's step 7 -- "if this is not possible, return NaN" -- has no explicit bound.
/// A day count only survives `TimeClip` below 1e11, and the arithmetic in [`days_from_civil`] is
/// exact in `f64` out to roughly 9e15; so a year too large to represent overshoots the clip by five
/// orders of magnitude before it could lose a digit, and lands on NaN by the route the standard
/// takes rather than by luck.
fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let (year, month, date) =
        (crate::math::trunc(year), crate::math::trunc(month), crate::math::trunc(date));
    let rolled_year = year + crate::math::floor(month / 12.0);
    if !rolled_year.is_finite() {
        return f64::NAN;
    }
    days_from_civil(rolled_year, euclid_mod(month, 12.0), 1.0) + date - 1.0
}

/// `MakeDate`: a day count and a time within the day to a time value.
fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        return f64::NAN;
    }
    let composed = day * MS_PER_DAY + time;
    if !composed.is_finite() {
        return f64::NAN;
    }
    composed
}

/// `MakeFullYear`.
///
/// A TWO-DIGIT YEAR MEANS 19xx, and only in the COMPONENT form. `new Date(99, 0)` is 1999;
/// `new Date(99)` is 99 milliseconds after the epoch. Legacy, required, and the one place the
/// constructor's two forms disagree about what a small number means. The SETTERS do not use it:
/// `d.setFullYear(99)` is the year 99, which is the difference the "full" in the name is carrying.
fn make_full_year(year: f64) -> f64 {
    if !year.is_finite() {
        return f64::NAN;
    }
    let truncated = crate::math::trunc(year);
    if (0.0..=99.0).contains(&truncated) {
        return 1900.0 + truncated;
    }
    truncated
}

/// The constructor's and `Date.UTC`'s component form.
fn components_from_arguments(
    interpreter: &mut Interpreter,
    arguments: &[JsValue],
) -> Result<f64, Completion> {
    let mut parts = [0.0f64; 7];
    parts[2] = 1.0;
    for (index, slot) in parts.iter_mut().enumerate() {
        if let Some(value) = arguments.get(index) {
            match interpreter.to_number_value(value) {
                Ok(number) => *slot = number,
                Err(abrupt) => return Err(abrupt),
            }
        }
    }
    Ok(time_clip(make_date(
        make_day(make_full_year(parts[0]), parts[1], parts[2]),
        make_time(parts[3], parts[4], parts[5], parts[6]),
    )))
}

/// `Date.parse`: the ISO format the standard requires, and the two renderings this engine itself
/// produces.
///
/// # THE NARROWING IS "NO HEURISTICS", NOT "NO OTHER FORMAT"
///
/// The standard requires the Date Time String Format and leaves everything else
/// implementation-defined; engines then accept a large, undocumented and mutually incompatible set,
/// and accepting more here would make a program that relies on it non-portable AT THIS ENGINE'S
/// ENCOURAGEMENT. Every unrecognised string is `NaN`, which is the specified answer.
///
/// BUT `toString` AND `toUTCString` ARE NOT "ANOTHER FORMAT" -- THIS ENGINE WROTE THEM, and the
/// standard names both: `Date.parse(x.toString())` and `Date.parse(x.toUTCString())` must produce
/// `x.valueOf()`. Refusing them is not narrowness, it is a realm that cannot read what it wrote.
/// So the two [`parse_rendered`] arms accept exactly the strings [`render`] and [`utc_string`]
/// emit, and nothing that looks like them.
fn parse_date(text: &str) -> f64 {
    let bytes = text.as_bytes();
    let digits = |from: usize, count: usize| -> Option<f64> {
        if from + count > bytes.len() {
            return None;
        }
        let mut value = 0.0f64;
        for byte in &bytes[from..from + count] {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10.0 + f64::from(byte - b'0');
        }
        Some(value)
    };
    let (year, rest) = match bytes.first() {
        Some(&b'+') | Some(&b'-') => {
            let Some(magnitude) = digits(1, 6) else { return parse_rendered(text) };
            if bytes[0] == b'-' && magnitude == 0.0 {
                return f64::NAN;
            }
            (if bytes[0] == b'-' { -magnitude } else { magnitude }, 7)
        }
        _ => match digits(0, 4) {
            Some(year) => (year, 4),
            None => return parse_rendered(text),
        },
    };
    if bytes.len() == rest {
        return time_clip(days_from_civil(year, 0.0, 1.0) * MS_PER_DAY);
    }
    if bytes.get(rest) != Some(&b'-') {
        return parse_rendered(text);
    }
    let Some(month) = digits(rest + 1, 2).filter(|m| (1.0..=12.0).contains(m)) else {
        return f64::NAN;
    };
    if bytes.len() == rest + 3 {
        return time_clip(days_from_civil(year, month - 1.0, 1.0) * MS_PER_DAY);
    }
    if bytes.get(rest + 3) != Some(&b'-') {
        return f64::NAN;
    }
    let Some(day) = digits(rest + 4, 2).filter(|d| (1.0..=31.0).contains(d)) else {
        return f64::NAN;
    };
    let date_only = days_from_civil(year, month - 1.0, day) * MS_PER_DAY;
    if bytes.len() == rest + 6 {
        return time_clip(date_only);
    }
    if bytes.get(rest + 6) != Some(&b'T') {
        return f64::NAN;
    }
    let (Some(hours), Some(minutes)) = (
        digits(rest + 7, 2).filter(|h| (0.0..=24.0).contains(h)),
        digits(rest + 10, 2).filter(|m| (0.0..=59.0).contains(m)),
    ) else {
        return f64::NAN;
    };
    if bytes.get(rest + 9) != Some(&b':') {
        return f64::NAN;
    }
    let mut time = date_only + hours * MS_PER_HOUR + minutes * MS_PER_MINUTE;
    let mut at = rest + 12;
    if bytes.get(at) == Some(&b':') {
        let Some(seconds) = digits(at + 1, 2).filter(|s| (0.0..=59.0).contains(s)) else {
            return f64::NAN;
        };
        time += seconds * MS_PER_SECOND;
        at += 3;
        if bytes.get(at) == Some(&b'.') {
            let Some(millis) = digits(at + 1, 3) else { return f64::NAN };
            time += millis;
            at += 4;
        }
    }
    if hours == 24.0 && time != date_only + 24.0 * MS_PER_HOUR {
        return f64::NAN;
    }
    match bytes.get(at) {
        None => time_clip(time),
        Some(&b'Z') if at + 1 == bytes.len() => time_clip(time),
        Some(&sign) if sign == b'+' || sign == b'-' => {
            if bytes.len() != at + 6 || bytes.get(at + 3) != Some(&b':') {
                return f64::NAN;
            }
            let (Some(offset_hours), Some(offset_minutes)) = (
                digits(at + 1, 2).filter(|h| (0.0..=23.0).contains(h)),
                digits(at + 4, 2).filter(|m| (0.0..=59.0).contains(m)),
            ) else {
                return f64::NAN;
            };
            let offset = offset_hours * MS_PER_HOUR + offset_minutes * MS_PER_MINUTE;
            time_clip(if sign == b'+' { time - offset } else { time + offset })
        }
        _ => f64::NAN,
    }
}

/// The inverse of [`render`] and [`utc_string`], and of nothing else.
///
/// # IT IS WRITTEN AGAINST THOSE TWO FUNCTIONS, NOT AGAINST A FAMILY OF HUMAN DATE STRINGS
///
/// `Date.parse(x.toString())` and `Date.parse(x.toUTCString())` are required to answer
/// `x.valueOf()`. That is a round trip through this engine's own output, so the honest
/// implementation reads exactly the two shapes above it in this file:
///
/// ```text
///   toString     Www Mmm DD [-]YYYY hh:mm:ss GMT+0000 (Coordinated Universal Time)
///   toUTCString  Www, DD Mmm [-]YYYY hh:mm:ss GMT
/// ```
///
/// The weekday is NOT checked against the date. It is output, not input: the standard's round trip
/// is about the time value, and a string whose weekday disagrees with its date is a string this
/// engine never wrote. Reading it and answering the date is what every engine does.
///
/// The trailing timezone text is required to be EXACTLY what `TIME_ZONE` holds, so this does not
/// quietly become a general offset parser -- an offset the realm cannot honour must be `NaN` rather
/// than read as UTC, which is the same rule the ISO arm follows.
fn parse_rendered(text: &str) -> f64 {
    let mut parts = text.split(' ');
    let (Some(first), Some(second), Some(third), Some(fourth), Some(clock)) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return f64::NAN;
    };
    let utc = first.ends_with(',');
    let weekday = first.trim_end_matches(',');
    if !WEEKDAYS.contains(&weekday) {
        return f64::NAN;
    }
    let (month_name, day_text) = if utc { (third, second) } else { (second, third) };
    let Some(month) = MONTHS.iter().position(|name| *name == month_name) else { return f64::NAN };
    let Some(day) = two_digits(day_text) else { return f64::NAN };
    let (year_text, negative) =
        fourth.strip_prefix('-').map_or((fourth, false), |rest| (rest, true));
    if year_text.len() < 4 || !year_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return f64::NAN;
    }
    let Ok(magnitude) = year_text.parse::<f64>() else { return f64::NAN };
    let year = if negative { -magnitude } else { magnitude };
    let mut clock_parts = clock.split(':');
    let (Some(hours), Some(minutes), Some(seconds), None) = (
        clock_parts.next().and_then(two_digits),
        clock_parts.next().and_then(two_digits),
        clock_parts.next().and_then(two_digits),
        clock_parts.next(),
    ) else {
        return f64::NAN;
    };
    let tail: crate::Vec<&str> = parts.collect();
    let expected = if utc { "GMT" } else { "GMT+0000" };
    if parts_tail_mismatches(&tail, expected, utc) {
        return f64::NAN;
    }
    time_clip(
        days_from_civil(year, month as f64, day) * MS_PER_DAY
            + hours * MS_PER_HOUR
            + minutes * MS_PER_MINUTE
            + seconds * MS_PER_SECOND,
    )
}

/// Exactly two ASCII digits, which is what every fixed-width field in the two renderings is.
fn two_digits(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(f64::from(bytes[0] - b'0') * 10.0 + f64::from(bytes[1] - b'0'))
}

/// Whether what follows the clock is NOT the zone text the matching renderer emits.
fn parts_tail_mismatches(tail: &[&str], expected: &str, utc: bool) -> bool {
    match tail {
        [zone] if utc => *zone != expected,
        [zone, rest @ ..] if !utc => {
            *zone != expected || rest.join(" ") != "(Coordinated Universal Time)"
        }
        _ => true,
    }
}

fn iso_string(time: f64) -> String {
    let (year, month, day) = civil_from_days(day_number(time));
    let hours = euclid_mod(crate::math::floor(time / MS_PER_HOUR), 24.0);
    let minutes = euclid_mod(crate::math::floor(time / MS_PER_MINUTE), 60.0);
    let seconds = euclid_mod(crate::math::floor(time / MS_PER_SECOND), 60.0);
    let millis = euclid_mod(time, MS_PER_SECOND);
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year > 9999 {
        format!("+{year:06}")
    } else {
        format!("-{:06}", -year)
    };
    format!(
        "{year_text}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        month + 1,
        day,
        hours as u32,
        minutes as u32,
        seconds as u32,
        millis as u32
    )
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `TimeZoneString`.
///
/// ALWAYS `+0000`, AND THAT IS THE PUBLISHED LOCAL-TIME DEVIATION SPEAKING rather than a stub: the
/// offset is zero because local time IS UTC here. The trailing name is the implementation-defined
/// half the standard permits, and it is the one every engine fills in.
const TIME_ZONE: &str = "+0000 (Coordinated Universal Time)";

/// `DateString`: the calendar half of `toString`, and all of `toDateString`.
///
/// THE YEAR IS ZERO-PADDED TO FOUR DIGITS WITH A SEPARATE SIGN. Year 5 is `0005` and year -5 is
/// `-0005`, not `5` and `-5`. An unpadded year is right for every date anybody spot-checks and wrong
/// for the whole first millennium -- and `Date.parse` reading back its own output is what finds it.
fn date_string(time: f64) -> String {
    let (year, month, day_of_month) = civil_from_days(day_number(time));
    let weekday = euclid_mod(day_number(time) + 4.0, 7.0) as usize;
    let sign = if year < 0 { "-" } else { "" };
    format!(
        "{} {} {day_of_month:02} {sign}{:04}",
        WEEKDAYS[weekday.min(6)],
        MONTHS[(month as usize).min(11)],
        year.unsigned_abs()
    )
}

/// `TimeString`: `hh:mm:ss GMT`.
///
/// THE `GMT` BELONGS TO THIS OPERATION AND THE OFFSET TO `TimeZoneString`, which is why both
/// `toString` and `toTimeString` read `... GMT+0000` with nothing between the two.
fn time_string(time: f64) -> String {
    let parts = components_of(time);
    format!("{:02}:{:02}:{:02} GMT", parts[3] as u32, parts[4] as u32, parts[5] as u32)
}

/// `ToDateString`: what `toString` answers.
///
/// The exact text is implementation-defined; the SHAPE is not. 21.4.4.41.4 composes it from the
/// three operations above, and "Invalid Date" is required.
fn render(time: f64) -> String {
    if time.is_nan() {
        return String::from("Invalid Date");
    }
    format!("{} {}{TIME_ZONE}", date_string(time), time_string(time))
}

/// `toUTCString`: HTTP-date from RFC 7231, generalized to the full range of a time value.
///
/// THIS IS A DIFFERENT FORMAT FROM `toString` AND IT IS REQUIRED TEXT. The day comes BEFORE the
/// month, a comma follows the weekday, and there is no offset at the end. Both renderings are
/// human-readable dates that look right; only a parser can tell them apart, which is precisely why
/// one renderer serving both was a wrong answer rather than a shortcut.
fn utc_string(time: f64) -> String {
    let (year, month, day_of_month) = civil_from_days(day_number(time));
    let weekday = euclid_mod(day_number(time) + 4.0, 7.0) as usize;
    let sign = if year < 0 { "-" } else { "" };
    format!(
        "{}, {day_of_month:02} {} {sign}{:04} {}",
        WEEKDAYS[weekday.min(6)],
        MONTHS[(month as usize).min(11)],
        year.unsigned_abs(),
        time_string(time)
    )
}


