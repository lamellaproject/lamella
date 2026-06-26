//! Runtime-native intrinsics: the Rust implementations a few BCL methods bind to.

use crate::interp::{Session, Vm};
use crate::module::{AttrValue, Module};
use crate::net::{Interest, NetResult};
use crate::tls::{TlsStack, VerifyMode};
use crate::object::{Object, ObjectRef, decode_string};
use crate::trap::Trap;
use crate::value::Value;
#[cfg(feature = "float")]
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_cil::Opcode;

/// The line terminator the console writes.
const NEWLINE: u16 = b'\n' as u16;

/// `System.Console.WriteLine(System.String)`: write the string's characters
/// followed by a line terminator. `WriteLine(null)` writes just the terminator.
///
/// # Errors
/// Returns [`Trap::TypeMismatch`] if the argument is not a string or null
/// reference.
pub fn console_write_line(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    match args.first() {
        Some(Value::Object(reference)) => {
            let chars: Vec<u16> = vm
                .heap()
                .as_string(*reference)
                .ok_or(Trap::TypeMismatch(Opcode::Call))?
                .to_vec();
            vm.write(&chars);
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(Trap::TypeMismatch(Opcode::Call)),
    }
    vm.write(&[NEWLINE]);
    Ok(None)
}

/// Writes `text` (UTF-16 encoded).
fn write_text(vm: &mut Vm, text: &str) {
    let chars: Vec<u16> = text.encode_utf16().collect();
    vm.write(&chars);
}

/// Writes `text` (UTF-16 encoded) followed by the line terminator.
fn write_line_text(vm: &mut Vm, text: &str) {
    write_text(vm, text);
    vm.write(&[NEWLINE]);
}

/// `System.Console.WriteLine()`: write just a line terminator.
///
/// # Errors
/// Never; the signature matches the intrinsic ABI.
pub fn console_write_line_empty(
    vm: &mut Vm,
    _module: &Module,
    _args: &[Value],
) -> Result<Option<Value>, Trap> {
    vm.write(&[NEWLINE]);
    Ok(None)
}

/// `System.Console.WriteLine(int)`: write an `int32` in decimal.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_line_int32(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, &value.to_string());
    Ok(None)
}

/// `System.Console.WriteLine(long)`: write an `int64` in decimal.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int64`.
pub fn console_write_line_int64(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int64(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, &value.to_string());
    Ok(None)
}

/// `System.Console.WriteLine(uint)`: write a `uint32` in decimal -- UNSIGNED, the full
/// magnitude with no sign. A `uint` rides the evaluation stack as an `int32` whose bits ARE
/// the unsigned value, so it is reinterpreted (`as u32`) before formatting.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_line_uint32(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, &(value as u32).to_string());
    Ok(None)
}

/// `System.Console.WriteLine(ulong)`: write a `uint64` in decimal -- UNSIGNED. A `ulong` rides
/// the stack as an `int64` whose bits ARE the unsigned value (reinterpreted `as u64`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int64`.
pub fn console_write_line_uint64(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int64(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, &(value as u64).to_string());
    Ok(None)
}

/// `System.Console.WriteLine(bool)`: write `True` or `False`. A `bool` is an
/// `int32` on the evaluation stack.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_line_bool(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, if value != 0 { "True" } else { "False" });
    Ok(None)
}

/// `System.Console.WriteLine(char)`: write a single UTF-16 code unit. A `char` is
/// an `int32` on the evaluation stack.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_line_char(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    vm.write(&[value as u16, NEWLINE]);
    Ok(None)
}

/// `System.Console.Write(string)`: write the string's characters, no terminator.
/// `Write(null)` writes nothing.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a string or null reference.
pub fn console_write(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    match args.first() {
        Some(Value::Object(reference)) => {
            let chars: Vec<u16> = vm
                .heap()
                .as_string(*reference)
                .ok_or(Trap::TypeMismatch(Opcode::Call))?
                .to_vec();
            vm.write(&chars);
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(Trap::TypeMismatch(Opcode::Call)),
    }
    Ok(None)
}

/// `System.Diagnostics.DefaultTraceListener` debug sink: write a string's characters to
/// the runtime DEBUG channel (the host renders it to STDERR), no terminator. The single
/// argument is a string or null reference; null writes nothing. This is the
/// `[RuntimeProvided]` primitive the managed `DefaultTraceListener` calls -- the
/// "debug channel" a developer sees from a bare `Debug.WriteLine` with no listener config,
/// conceptually distinct from `Console.Out`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a string or null reference.
pub fn debug_write(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    match args.first() {
        Some(Value::Object(reference)) => {
            let chars: Vec<u16> = vm
                .heap()
                .as_string(*reference)
                .ok_or(Trap::TypeMismatch(Opcode::Call))?
                .to_vec();
            vm.debug_write(&chars);
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(Trap::TypeMismatch(Opcode::Call)),
    }
    Ok(None)
}

/// `System.Console.Write(int)`: write an `int32` in decimal, no terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_int32(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_text(vm, &value.to_string());
    Ok(None)
}

/// `System.Console.Write(long)`: write an `int64` in decimal, no terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int64`.
pub fn console_write_int64(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int64(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_text(vm, &value.to_string());
    Ok(None)
}

/// `System.Console.Write(bool)`: write `True` or `False`, no terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_bool(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_text(vm, if value != 0 { "True" } else { "False" });
    Ok(None)
}

/// `System.Console.Write(char)`: write a single UTF-16 code unit, no terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_char(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    vm.write(&[value as u16]);
    Ok(None)
}

/// Formats an `f64` as .NET's `double.ToString()` does for the common cases:
/// shortest round-trippable for finite values (Rust matches .NET here), and
/// `Infinity` / `-Infinity` / `NaN` for the specials. Exponent formatting of very
/// large or small magnitudes still differs from .NET -- a stage-4-oracle refinement.
#[cfg(feature = "float")]
fn format_double(value: f64) -> String {
    if value.is_infinite() {
        return String::from(if value < 0.0 { "-Infinity" } else { "Infinity" });
    }
    value.to_string()
}

/// Formats an `f32` as .NET's `Single.ToString()` does: the shortest round-trippable text for a
/// finite value (Rust's `f32` formatter chooses the same fewest digits .NET does), and
/// `Infinity` / `-Infinity` / `NaN` for the specials. The f32 path is what makes a Single render
/// to its own (shorter) digits rather than the f64-widened decimal -- e.g. `0.1f` is "0.1", and a
/// single-precision sum like `0.1f + 0.2f` is "0.3", where the double is "0.30000000000000004".
#[cfg(feature = "float")]
fn format_single(value: f32) -> String {
    if value.is_infinite() {
        return String::from(if value < 0.0 { "-Infinity" } else { "Infinity" });
    }
    value.to_string()
}

/// `System.Console.WriteLine(double)`: write a double, then a line terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a floating-point value.
#[cfg(feature = "float")]
pub fn console_write_line_double(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Float(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, &format_double(value));
    Ok(None)
}

/// `System.Console.Write(double)`: write a double, no terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a floating-point value.
#[cfg(feature = "float")]
pub fn console_write_double(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Float(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_text(vm, &format_double(value));
    Ok(None)
}

/// `System.Console.WriteLine(float)`: write a single, then a line terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a single-precision value.
#[cfg(feature = "float")]
pub fn console_write_line_single(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Single(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_line_text(vm, &format_single(value));
    Ok(None)
}

/// `System.Console.Write(float)`: write a single, no terminator.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a single-precision value.
#[cfg(feature = "float")]
pub fn console_write_single(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Single(value)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    write_text(vm, &format_single(value));
    Ok(None)
}

/// The UTF-16 characters of a string-or-null argument; a null or missing argument
/// is the empty string.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is some non-string value.
fn string_arg_chars(vm: &Vm, arg: Option<&Value>) -> Result<Vec<u16>, Trap> {
    match arg {
        Some(Value::Object(reference)) => Ok(vm
            .heap()
            .as_string(*reference)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?
            .to_vec()),
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(_) => Err(Trap::TypeMismatch(Opcode::Call)),
    }
}

/// `System.String.Concat(string, string)`: concatenate two strings into a new one
/// (a null argument is the empty string), returning the new string reference.
///
/// # Errors
/// [`Trap::TypeMismatch`] if either argument is a non-string value.
pub fn string_concat(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let mut chars = string_arg_chars(vm, args.first())?;
    chars.extend_from_slice(&string_arg_chars(vm, args.get(1))?);
    let reference = vm.heap_mut().alloc_string(&chars);
    Ok(Some(Value::Object(reference)))
}

/// `System.String.get_Length` (the `Length` property): the number of UTF-16 code
/// units. The string is the implicit `this`, the only argument.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a string.
pub fn string_get_length(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(Value::Object(reference)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let length = vm
        .heap()
        .as_string(*reference)
        .ok_or(Trap::TypeMismatch(Opcode::Call))?
        .len();
    Ok(Some(Value::Int32(
        i32::try_from(length).unwrap_or(i32::MAX),
    )))
}

/// The UTF-16 characters of a string argument, or `None` for a null reference --
/// kept distinct from the empty string so equality is correct.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is some non-string value.
fn string_opt(vm: &Vm, arg: Option<&Value>) -> Result<Option<Vec<u16>>, Trap> {
    match arg {
        Some(Value::Object(reference)) => Ok(Some(
            vm.heap()
                .as_string(*reference)
                .ok_or(Trap::TypeMismatch(Opcode::Call))?
                .to_vec(),
        )),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(Trap::TypeMismatch(Opcode::Call)),
    }
}

/// `System.String.op_Equality(string, string)`: ordinal equality as a `bool` (an
/// `int32` 0/1). Two nulls are equal; a null and a string are not.
///
/// # Errors
/// [`Trap::TypeMismatch`] if either argument is a non-string value.
pub fn string_equals(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let left = string_opt(vm, args.first())?;
    let right = string_opt(vm, args.get(1))?;
    Ok(Some(Value::Int32(i32::from(left == right))))
}

/// `System.String.get_Chars(int)` (the indexer `s[i]`): the UTF-16 code unit at
/// `index`, as the `int32` a `char` is on the stack. The string is `this`, the
/// index the second argument.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a string or the index not an
/// `int32`; [`Trap::ArgumentOutOfRange`] if the index is out of bounds.
pub fn string_get_chars(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(Value::Object(reference)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(index)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let chars = vm
        .heap()
        .as_string(*reference)
        .ok_or(Trap::TypeMismatch(Opcode::Call))?;
    let unit = usize::try_from(index)
        .ok()
        .and_then(|index| chars.get(index))
        .ok_or(Trap::IndexOutOfRange(index))?;
    Ok(Some(Value::Int32(i32::from(*unit))))
}

/// `System.String.op_Inequality(string, string)`: ordinal inequality as a `bool`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if either argument is a non-string value.
pub fn string_not_equals(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let left = string_opt(vm, args.first())?;
    let right = string_opt(vm, args.get(1))?;
    Ok(Some(Value::Int32(i32::from(left != right))))
}

/// `System.String.IsNullOrEmpty(string)`: true for a null or zero-length string.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is a non-string value.
pub fn string_is_null_or_empty(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = string_opt(vm, args.first())?;
    let empty = value.is_none_or(|chars| chars.is_empty());
    Ok(Some(Value::Int32(i32::from(empty))))
}

/// `System.String.Substring(int)`: the tail from `startIndex` (which may equal the
/// length, giving the empty string). The string is `this`.
///
/// # Errors
/// [`Trap::TypeMismatch`] for bad argument types; [`Trap::ArgumentOutOfRange`] if
/// `startIndex` is negative or past the end -- the `ArgumentOutOfRangeException` site, matching
/// .NET's `String.Substring` (its out-of-range exception is `ArgumentOutOfRangeException`, not
/// the array indexer's `IndexOutOfRangeException`).
pub fn string_substring(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let chars = string_arg_chars(vm, args.first())?;
    let Some(&Value::Int32(start)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let start = usize::try_from(start)
        .ok()
        .filter(|&start| start <= chars.len())
        .ok_or(Trap::ArgumentOutOfRange(0))?;
    let reference = vm.heap_mut().alloc_string(&chars[start..]);
    Ok(Some(Value::Object(reference)))
}

/// `System.String.Substring(int, int)`: `length` units from `startIndex`.
///
/// # Errors
/// [`Trap::TypeMismatch`] for bad argument types; [`Trap::ArgumentOutOfRange`] if the range
/// falls outside the string (the `ArgumentOutOfRangeException` site, as in .NET).
pub fn string_substring_len(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let chars = string_arg_chars(vm, args.first())?;
    let Some(&Value::Int32(start)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(length)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let start = usize::try_from(start).map_err(|_| Trap::ArgumentOutOfRange(0))?;
    let count = usize::try_from(length).map_err(|_| Trap::ArgumentOutOfRange(1))?;
    let end = start
        .checked_add(count)
        .filter(|&end| end <= chars.len())
        .ok_or(Trap::ArgumentOutOfRange(1))?;
    let reference = vm.heap_mut().alloc_string(&chars[start..end]);
    Ok(Some(Value::Object(reference)))
}

/// `System.String.Concat(string, string, string)`: join three strings into a new
/// one (a null argument is the empty string).
///
/// # Errors
/// [`Trap::TypeMismatch`] if any argument is a non-string value.
pub fn string_concat3(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let mut chars = string_arg_chars(vm, args.first())?;
    chars.extend_from_slice(&string_arg_chars(vm, args.get(1))?);
    chars.extend_from_slice(&string_arg_chars(vm, args.get(2))?);
    let reference = vm.heap_mut().alloc_string(&chars);
    Ok(Some(Value::Object(reference)))
}

/// The host clock seam: the current time as 100-nanosecond ticks since the .NET epoch
/// (0001-01-01 00:00:00), read from the [`Vm`]. Backs the managed `DateTime.Now` /
/// `DateTime.UtcNow` (which wrap it in `new DateTime(ticks)`). The interpreter core is
/// no_std and keeps no clock of its own -- the embedder sets the value via
/// [`Vm::set_now_ticks`] (the host from `std::time`, a device from its RTC), defaulting
/// to 0 (the epoch). For v1 there is no timezone, so `Now` and `UtcNow` report the same
/// UTC-based ticks.
///
/// # Errors
/// Never; the signature matches the intrinsic ABI (it takes no arguments).
pub fn datetime_now_ticks(
    vm: &mut Vm,
    _module: &Module,
    _args: &[Value],
) -> Result<Option<Value>, Trap> {
    Ok(Some(Value::Int64(vm.now_ticks())))
}

/// The additional NETMFv4_4-profile BCL surface, beyond the ECMA-335 Kernel
/// Profile. Gated by `NETMFv4_4` so a Kernel-only build omits it entirely; its public
/// intrinsics are re-exported below so `crate::intrinsics::*` paths are unchanged.
#[cfg(feature = "NETMFv4_4")]
mod extended {
    use super::{scalar_text, string_arg_chars};
    use crate::interp::Vm;
    use crate::module::Module;
    use crate::object::{Object, ObjectRef};
    use crate::trap::Trap;
    use crate::value::Value;
    use alloc::vec::Vec;
    use lamella_cil::Opcode;

    /// `-1` for no match, else the index as an `int32` -- the .NET convention for the
    /// `IndexOf` family.
    fn match_index(index: Option<usize>) -> Value {
        Value::Int32(
            index
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        )
    }

    /// The index of the first ordinal occurrence of `needle` in `haystack`. The empty
    /// needle matches at 0, as .NET's ordinal search does.
    fn find_subsequence(haystack: &[u16], needle: &[u16]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// ASCII upper-casing of one UTF-16 code unit (`a..z` -> `A..Z`); others are unchanged.
    /// Full Unicode/culture casing is a later refinement.
    fn ascii_upper(unit: u16) -> u16 {
        if (b'a' as u16..=b'z' as u16).contains(&unit) {
            unit - 32
        } else {
            unit
        }
    }

    /// ASCII lower-casing of one UTF-16 code unit (`A..Z` -> `a..z`); others are unchanged.
    fn ascii_lower(unit: u16) -> u16 {
        if (b'A' as u16..=b'Z' as u16).contains(&unit) {
            unit + 32
        } else {
            unit
        }
    }

    /// Whether a UTF-16 code unit is one of the ASCII whitespace characters `Trim` removes
    /// (space, tab, LF, VT, FF, CR). Unicode whitespace is a later refinement.
    fn is_ascii_space(unit: u16) -> bool {
        matches!(unit, 0x20 | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D)
    }

    /// `System.String.IndexOf(char)`: the index of the first occurrence of a code unit, or
    /// `-1`. The string is `this`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string or the argument not a char.
    pub fn string_index_of_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(target)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let target = target as u16;
        Ok(Some(match_index(chars.iter().position(|&c| c == target))))
    }

    /// `System.String.IndexOf(string)`: the index of the first ordinal occurrence of a
    /// substring, or `-1`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if a receiver/argument is a non-string value.
    pub fn string_index_of_string(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let needle = string_arg_chars(vm, args.get(1))?;
        Ok(Some(match_index(find_subsequence(&chars, &needle))))
    }

    /// `System.String.LastIndexOf(char)`: the index of the last occurrence, or `-1`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string or the argument not a char.
    pub fn string_last_index_of_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(target)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let target = target as u16;
        Ok(Some(match_index(chars.iter().rposition(|&c| c == target))))
    }

    /// `System.String.StartsWith(string)` (ordinal): does the string begin with `value`?
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if a receiver/argument is a non-string value.
    pub fn string_starts_with(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let value = string_arg_chars(vm, args.get(1))?;
        Ok(Some(Value::Int32(i32::from(chars.starts_with(&value)))))
    }

    /// `System.String.EndsWith(string)` (ordinal): does the string end with `value`?
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if a receiver/argument is a non-string value.
    pub fn string_ends_with(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let value = string_arg_chars(vm, args.get(1))?;
        Ok(Some(Value::Int32(i32::from(chars.ends_with(&value)))))
    }

    /// `System.String.Contains(string)` (ordinal): does the string contain `value`?
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if a receiver/argument is a non-string value.
    pub fn string_contains(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let value = string_arg_chars(vm, args.get(1))?;
        Ok(Some(Value::Int32(i32::from(
            find_subsequence(&chars, &value).is_some(),
        ))))
    }

    /// `System.String.ToUpper()`: an ASCII upper-cased copy.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string.
    pub fn string_to_upper(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let upper: Vec<u16> = string_arg_chars(vm, args.first())?
            .iter()
            .map(|&unit| ascii_upper(unit))
            .collect();
        let reference = vm.heap_mut().alloc_string(&upper);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.ToLower()`: an ASCII lower-cased copy.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string.
    pub fn string_to_lower(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let lower: Vec<u16> = string_arg_chars(vm, args.first())?
            .iter()
            .map(|&unit| ascii_lower(unit))
            .collect();
        let reference = vm.heap_mut().alloc_string(&lower);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.Trim()`: a copy with leading and trailing ASCII whitespace removed.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string.
    pub fn string_trim(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let trimmed: &[u16] = match chars.iter().position(|&c| !is_ascii_space(c)) {
            Some(start) => {
                let end = chars
                    .iter()
                    .rposition(|&c| !is_ascii_space(c))
                    .unwrap_or(start);
                &chars[start..=end]
            }
            None => &[],
        };
        let reference = vm.heap_mut().alloc_string(trimmed);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.Replace(char, char)`: every `from` replaced by `to`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for bad argument types.
    pub fn string_replace_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(from)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let Some(&Value::Int32(to)) = args.get(2) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let (from, to) = (from as u16, to as u16);
        let replaced: Vec<u16> = chars
            .iter()
            .map(|&unit| if unit == from { to } else { unit })
            .collect();
        let reference = vm.heap_mut().alloc_string(&replaced);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.Replace(string, string)`: every non-overlapping ordinal occurrence of
    /// `old` replaced by `new`. An empty `old` leaves the string unchanged (.NET throws
    /// `ArgumentException`; the interpreter returns the original rather than trapping).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if a receiver/argument is a non-string value.
    pub fn string_replace_string(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let old = string_arg_chars(vm, args.get(1))?;
        let new = string_arg_chars(vm, args.get(2))?;
        let replaced = if old.is_empty() {
            chars
        } else {
            let mut out = Vec::with_capacity(chars.len());
            let mut index = 0;
            while index < chars.len() {
                if chars[index..].starts_with(&old) {
                    out.extend_from_slice(&new);
                    index += old.len();
                } else {
                    out.push(chars[index]);
                    index += 1;
                }
            }
            out
        };
        let reference = vm.heap_mut().alloc_string(&replaced);
        Ok(Some(Value::Object(reference)))
    }

    /// The single `int32` argument of a numeric intrinsic.
    fn arg_int32(args: &[Value]) -> Result<i32, Trap> {
        match args.first() {
            Some(&Value::Int32(value)) => Ok(value),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// The single `int64` argument of a numeric intrinsic.
    fn arg_int64(args: &[Value]) -> Result<i64, Trap> {
        match args.first() {
            Some(&Value::Int64(value)) => Ok(value),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// The two `int32` arguments of a binary numeric intrinsic.
    fn two_int32(args: &[Value]) -> Result<(i32, i32), Trap> {
        match (args.first(), args.get(1)) {
            (Some(&Value::Int32(left)), Some(&Value::Int32(right))) => Ok((left, right)),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// The two `int64` arguments of a binary numeric intrinsic.
    fn two_int64(args: &[Value]) -> Result<(i64, i64), Trap> {
        match (args.first(), args.get(1)) {
            (Some(&Value::Int64(left)), Some(&Value::Int64(right))) => Ok((left, right)),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.Math.Abs(int)`: the absolute value; throws `OverflowException` for
    /// `int.MinValue`, whose magnitude is unrepresentable (matching .NET).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-int argument; [`Trap::Overflow`] for `int.MinValue`.
    pub fn math_abs_int32(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        arg_int32(args)?
            .checked_abs()
            .map(|abs| Some(Value::Int32(abs)))
            .ok_or(Trap::Overflow)
    }

    /// `System.Math.Abs(long)`: the absolute value; `OverflowException` for `long.MinValue`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-long argument; [`Trap::Overflow`] for `long.MinValue`.
    pub fn math_abs_int64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        arg_int64(args)?
            .checked_abs()
            .map(|abs| Some(Value::Int64(abs)))
            .ok_or(Trap::Overflow)
    }

    /// `System.Math.Max(int, int)`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-int arguments.
    pub fn math_max_int32(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (left, right) = two_int32(args)?;
        Ok(Some(Value::Int32(left.max(right))))
    }

    /// `System.Math.Min(int, int)`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-int arguments.
    pub fn math_min_int32(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (left, right) = two_int32(args)?;
        Ok(Some(Value::Int32(left.min(right))))
    }

    /// `System.Math.Max(long, long)`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-long arguments.
    pub fn math_max_int64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (left, right) = two_int64(args)?;
        Ok(Some(Value::Int64(left.max(right))))
    }

    /// `System.Math.Min(long, long)`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-long arguments.
    pub fn math_min_int64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (left, right) = two_int64(args)?;
        Ok(Some(Value::Int64(left.min(right))))
    }

    /// `System.Math.Sign(int)`: -1, 0, or 1.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-int argument.
    pub fn math_sign_int32(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(arg_int32(args)?.signum())))
    }

    /// `System.Math.Sign(long)`: -1, 0, or 1 (returned as an `int`).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-long argument.
    pub fn math_sign_int64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(arg_int64(args)?.signum() as i32)))
    }

    /// The single `f64` argument of a `Math` intrinsic.
    #[cfg(feature = "float")]
    fn arg_f64(args: &[Value]) -> Result<f64, Trap> {
        match args.first() {
            Some(&Value::Float(value)) => Ok(value),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// The two `f64` arguments of a binary `Math` intrinsic.
    #[cfg(feature = "float")]
    fn two_f64(args: &[Value]) -> Result<(f64, f64), Trap> {
        match (args.first(), args.get(1)) {
            (Some(&Value::Float(left)), Some(&Value::Float(right))) => Ok((left, right)),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.Math.Abs(double)`: the magnitude, by clearing the sign bit (no libm needed).
    /// NaN stays NaN; -0.0 becomes +0.0 -- matching .NET.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn math_abs_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let bits = arg_f64(args)?.to_bits() & 0x7FFF_FFFF_FFFF_FFFF;
        Ok(Some(Value::Float(f64::from_bits(bits))))
    }

    /// `System.Math.Max(double, double)`: the larger, or NaN if either is NaN. (The
    /// signed-zero tie-break of .NET is not modeled; -0.0/+0.0 compare equal here.)
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-double arguments.
    #[cfg(feature = "float")]
    pub fn math_max_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (left, right) = two_f64(args)?;
        let result = if left.is_nan() || right.is_nan() {
            f64::NAN
        } else if left >= right {
            left
        } else {
            right
        };
        Ok(Some(Value::Float(result)))
    }

    /// `System.Math.Min(double, double)`: the smaller, or NaN if either is NaN.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-double arguments.
    #[cfg(feature = "float")]
    pub fn math_min_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (left, right) = two_f64(args)?;
        let result = if left.is_nan() || right.is_nan() {
            f64::NAN
        } else if left <= right {
            left
        } else {
            right
        };
        Ok(Some(Value::Float(result)))
    }

    /// `System.Math.Sign(double)`: -1, 0, or 1 (returned as an `int`).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument; [`Trap::InvalidArgument`] for NaN
    /// (.NET throws `ArithmeticException`).
    #[cfg(feature = "float")]
    pub fn math_sign_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = arg_f64(args)?;
        if value.is_nan() {
            return Err(Trap::InvalidArgument);
        }
        let sign = if value > 0.0 {
            1
        } else if value < 0.0 {
            -1
        } else {
            0
        };
        Ok(Some(Value::Int32(sign)))
    }

    /// Truncates `value` toward zero (no libm): an already-integral magnitude (>= 2^52, where
    /// a double has no fractional bits) is returned as-is, else round-tripped through `i64`.
    #[cfg(feature = "float")]
    fn trunc_f64(value: f64) -> f64 {
        if value.is_finite() && value.abs() < 4_503_599_627_370_496.0 {
            (value as i64) as f64
        } else {
            value
        }
    }

    /// The largest integer not greater than `value` (no libm).
    #[cfg(feature = "float")]
    fn floor_f64(value: f64) -> f64 {
        let truncated = trunc_f64(value);
        if truncated > value {
            truncated - 1.0
        } else {
            truncated
        }
    }

    /// `System.Math.Truncate(double)`: the integer part, toward zero.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn math_truncate_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(trunc_f64(arg_f64(args)?))))
    }

    /// `System.Math.Floor(double)`: the largest integer not greater than the argument.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn math_floor_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(floor_f64(arg_f64(args)?))))
    }

    /// `System.Math.Ceiling(double)`: the smallest integer not less than the argument.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn math_ceiling_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = arg_f64(args)?;
        let truncated = trunc_f64(value);
        let ceiling = if truncated < value {
            truncated + 1.0
        } else {
            truncated
        };
        Ok(Some(Value::Float(ceiling)))
    }

    /// `System.Math.Round(double)`: round half to even (banker's rounding), matching .NET's
    /// default `MidpointRounding.ToEven`. No libm.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn math_round_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = arg_f64(args)?;
        let result = if !value.is_finite() || value.abs() >= 4_503_599_627_370_496.0 {
            value
        } else {
            let floor = floor_f64(value);
            let fraction = value - floor;
            if fraction < 0.5 {
                floor
            } else if fraction > 0.5 {
                floor + 1.0
            } else if (floor as i64) % 2 == 0 {
                floor
            } else {
                floor + 1.0
            }
        };
        Ok(Some(Value::Float(result)))
    }

    /// `System.Convert.ToInt32(double)`: the nearest integer (round half to even, like .NET).
    ///
    /// # Errors
    /// [`Trap::Overflow`] if out of `int` range; [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn convert_to_int32_double(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = arg_f64(args)?;
        if !value.is_finite() {
            return Err(Trap::Overflow);
        }
        let floor = floor_f64(value);
        let fraction = value - floor;
        let rounded = if fraction < 0.5 {
            floor
        } else if fraction > 0.5 {
            floor + 1.0
        } else if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        };
        if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
            return Err(Trap::Overflow);
        }
        Ok(Some(Value::Int32(rounded as i32)))
    }

    /// `System.Convert.ToChar(int)`: the character with the given UTF-16 code (0..=65535).
    ///
    /// # Errors
    /// [`Trap::Overflow`] outside the code-unit range; [`Trap::TypeMismatch`] for a non-int.
    pub fn convert_to_char_int(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        match args.first() {
            Some(&Value::Int32(value)) if (0..=0xFFFF).contains(&value) => {
                Ok(Some(Value::Int32(value)))
            }
            Some(&Value::Int32(_)) => Err(Trap::Overflow),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.Convert.ToByte(int)`: the byte value (0..=255).
    ///
    /// # Errors
    /// [`Trap::Overflow`] outside 0..=255; [`Trap::TypeMismatch`] for a non-int.
    pub fn convert_to_byte_int(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        match args.first() {
            Some(&Value::Int32(value)) if (0..=255).contains(&value) => {
                Ok(Some(Value::Int32(value)))
            }
            Some(&Value::Int32(_)) => Err(Trap::Overflow),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.Convert.ToBoolean(int)`: false for zero, true otherwise.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-int argument.
    pub fn convert_to_boolean_int(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        match args.first() {
            Some(&Value::Int32(value)) => Ok(Some(Value::Int32(i32::from(value != 0)))),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.String.Split(char[, StringSplitOptions])`: splits on the separator character,
    /// keeping empty entries (the `StringSplitOptions` argument is not honored). Returns a
    /// `string[]`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-string receiver or non-char separator.
    pub fn string_split_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let separator = match args.get(1) {
            Some(&Value::Int32(code)) => code as u16,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let mut parts: Vec<Value> = Vec::new();
        let mut start = 0;
        for (index, &unit) in chars.iter().enumerate() {
            if unit == separator {
                let part = vm.heap_mut().alloc_string(&chars[start..index]);
                parts.push(Value::Object(part));
                start = index + 1;
            }
        }
        let last = vm.heap_mut().alloc_string(&chars[start..]);
        parts.push(Value::Object(last));
        let array = vm.heap_mut().alloc_array(parts);
        Ok(Some(Value::Object(array)))
    }

    /// `System.String.Join(string, string[])`: concatenates the array's strings with the
    /// separator between them (a null element contributes nothing).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the separator is not a string or the second argument not an array.
    pub fn string_join(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let separator = string_arg_chars(vm, args.first())?;
        let array = match args.get(1) {
            Some(&Value::Object(reference)) => reference,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let len = vm
            .heap()
            .array_len(array)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        let mut result: Vec<u16> = Vec::new();
        for index in 0..len {
            if index > 0 {
                result.extend_from_slice(&separator);
            }
            if let Some(Value::Object(element)) = vm.heap().array_get(array, index) {
                if let Some(chars) = vm.heap().as_string(element) {
                    result.extend_from_slice(&chars);
                }
            }
        }
        let joined = vm.heap_mut().alloc_string(&result);
        Ok(Some(Value::Object(joined)))
    }

    /// `System.Math.Sqrt(double)`: the square root, via `libm` (IEEE correctly-rounded).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_sqrt_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::sqrt(arg_f64(args)?))))
    }

    /// `System.Math.Pow(double, double)`: base raised to the exponent, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for non-double arguments.
    #[cfg(feature = "math-transcendental")]
    pub fn math_pow_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (base, exponent) = two_f64(args)?;
        Ok(Some(Value::Float(libm::pow(base, exponent))))
    }

    /// `System.Math.Sin(double)`: the sine of an angle in radians, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_sin_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::sin(arg_f64(args)?))))
    }

    /// `System.Math.Cos(double)`: the cosine of an angle in radians, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_cos_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::cos(arg_f64(args)?))))
    }

    /// `System.Math.Tan(double)`: the tangent of an angle in radians, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_tan_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::tan(arg_f64(args)?))))
    }

    /// `System.Math.Log(double)`: the natural logarithm, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_log_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::log(arg_f64(args)?))))
    }

    /// `System.Math.Log10(double)`: the base-10 logarithm, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_log10_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::log10(arg_f64(args)?))))
    }

    /// `System.Math.Exp(double)`: e raised to the argument, via `libm`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "math-transcendental")]
    pub fn math_exp_f64(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Float(libm::exp(arg_f64(args)?))))
    }

    /// Whether a UTF-16 code unit is an ASCII decimal digit (`0..9`).
    fn is_ascii_digit_unit(unit: u16) -> bool {
        (b'0' as u16..=b'9' as u16).contains(&unit)
    }

    /// Whether a UTF-16 code unit is an ASCII letter (`A..Z` or `a..z`).
    fn is_ascii_letter_unit(unit: u16) -> bool {
        (b'A' as u16..=b'Z' as u16).contains(&unit) || (b'a' as u16..=b'z' as u16).contains(&unit)
    }

    /// The `char` argument of a `System.Char` intrinsic, as its UTF-16 code unit (a `char`
    /// is an `int32` on the stack).
    fn arg_char(args: &[Value]) -> Result<u16, Trap> {
        Ok(arg_int32(args)? as u16)
    }

    /// `System.Char.IsDigit(char)` (ASCII classification).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_is_digit(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(i32::from(is_ascii_digit_unit(
            arg_char(args)?,
        )))))
    }

    /// `System.Char.IsLetter(char)` (ASCII classification).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_is_letter(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(i32::from(is_ascii_letter_unit(
            arg_char(args)?,
        )))))
    }

    /// `System.Char.IsLetterOrDigit(char)` (ASCII classification).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_is_letter_or_digit(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let unit = arg_char(args)?;
        Ok(Some(Value::Int32(i32::from(
            is_ascii_letter_unit(unit) || is_ascii_digit_unit(unit),
        ))))
    }

    /// `System.Char.IsWhiteSpace(char)` (ASCII whitespace).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_is_white_space(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(i32::from(is_ascii_space(arg_char(
            args,
        )?)))))
    }

    /// `System.Char.IsUpper(char)` (ASCII).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_is_upper(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let unit = arg_char(args)?;
        Ok(Some(Value::Int32(i32::from(
            (b'A' as u16..=b'Z' as u16).contains(&unit),
        ))))
    }

    /// `System.Char.IsLower(char)` (ASCII).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_is_lower(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let unit = arg_char(args)?;
        Ok(Some(Value::Int32(i32::from(
            (b'a' as u16..=b'z' as u16).contains(&unit),
        ))))
    }

    /// `System.Char.ToUpper(char)`: the ASCII upper-case of a code unit (as a `char`).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_to_upper(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(i32::from(ascii_upper(arg_char(args)?)))))
    }

    /// `System.Char.ToLower(char)`: the ASCII lower-case of a code unit (as a `char`).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-char argument.
    pub fn char_to_lower(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        Ok(Some(Value::Int32(i32::from(ascii_lower(arg_char(args)?)))))
    }

    /// The `chars` slice with leading and trailing ASCII whitespace removed.
    fn trim_ascii(chars: &[u16]) -> &[u16] {
        match chars.iter().position(|&unit| !is_ascii_space(unit)) {
            Some(start) => {
                let end = chars
                    .iter()
                    .rposition(|&unit| !is_ascii_space(unit))
                    .unwrap_or(start);
                &chars[start..=end]
            }
            None => &[],
        }
    }

    /// Parses a base-10 integer from UTF-16 chars (an `i128` accumulator): optional ASCII
    /// whitespace, an optional `+`/`-` sign, then ASCII digits. `None` for malformed input
    /// or magnitude overflow of the accumulator.
    fn parse_decimal(chars: &[u16]) -> Option<i128> {
        let body = trim_ascii(chars);
        let (negative, digits) = match body.first() {
            Some(&unit) if unit == b'-' as u16 => (true, &body[1..]),
            Some(&unit) if unit == b'+' as u16 => (false, &body[1..]),
            _ => (false, body),
        };
        if digits.is_empty() {
            return None;
        }
        let mut value: i128 = 0;
        for &unit in digits {
            if !is_ascii_digit_unit(unit) {
                return None;
            }
            value = value
                .checked_mul(10)?
                .checked_add(i128::from(unit - b'0' as u16))?;
        }
        Some(if negative { -value } else { value })
    }

    /// `System.Int32.Parse(string)`: a base-10 parse.
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] (the FormatException site) for malformed input;
    /// [`Trap::Overflow`] if the value does not fit an `int32`; [`Trap::TypeMismatch`] for a
    /// non-string argument.
    pub fn int32_parse(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let value = parse_decimal(&chars).ok_or(Trap::InvalidArgument)?;
        i32::try_from(value)
            .map(|value| Some(Value::Int32(value)))
            .map_err(|_| Trap::Overflow)
    }

    /// `System.Int64.Parse(string)`: a base-10 parse.
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] for malformed input; [`Trap::Overflow`] if the value does
    /// not fit an `int64`; [`Trap::TypeMismatch`] for a non-string argument.
    pub fn int64_parse(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let value = parse_decimal(&chars).ok_or(Trap::InvalidArgument)?;
        i64::try_from(value)
            .map(|value| Some(Value::Int64(value)))
            .map_err(|_| Trap::Overflow)
    }

    /// `System.Boolean.Parse(string)`: case-insensitive `True` / `False` after trimming.
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] for anything other than (trimmed, case-insensitive)
    /// `true`/`false`; [`Trap::TypeMismatch`] for a non-string argument.
    pub fn boolean_parse(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let lowered: Vec<u16> = trim_ascii(&chars)
            .iter()
            .map(|&unit| ascii_lower(unit))
            .collect();
        let truthy: Vec<u16> = "true".encode_utf16().collect();
        let falsy: Vec<u16> = "false".encode_utf16().collect();
        if lowered == truthy {
            Ok(Some(Value::Int32(1)))
        } else if lowered == falsy {
            Ok(Some(Value::Int32(0)))
        } else {
            Err(Trap::InvalidArgument)
        }
    }

    /// The pad character from `args[2]` (a `char`), or a space when the one-argument
    /// overload is called.
    fn pad_char(args: &[Value]) -> Result<u16, Trap> {
        match args.get(2) {
            Some(&Value::Int32(unit)) => Ok(unit as u16),
            None => Ok(b' ' as u16),
            Some(_) => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.String.PadLeft(int [, char])`: right-justify in a field `width` wide, padding
    /// on the left; the original is returned when it is already at least that wide.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for bad argument types.
    pub fn string_pad_left(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(width)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let pad = pad_char(args)?;
        let width = usize::try_from(width).unwrap_or(0);
        let result = if chars.len() >= width {
            chars
        } else {
            let mut out = alloc::vec![pad; width - chars.len()];
            out.extend_from_slice(&chars);
            out
        };
        let reference = vm.heap_mut().alloc_string(&result);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.PadRight(int [, char])`: left-justify in a field `width` wide, padding
    /// on the right.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for bad argument types.
    pub fn string_pad_right(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let mut chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(width)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let pad = pad_char(args)?;
        let width = usize::try_from(width).unwrap_or(0);
        if chars.len() < width {
            chars.resize(width, pad);
        }
        let reference = vm.heap_mut().alloc_string(&chars);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.Insert(int startIndex, string value)`: a copy with `value` inserted at
    /// `startIndex`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for bad argument types; [`Trap::ArgumentOutOfRange`] if
    /// `startIndex` is negative or past the end.
    pub fn string_insert(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(index)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let value = string_arg_chars(vm, args.get(2))?;
        let index = usize::try_from(index)
            .ok()
            .filter(|&index| index <= chars.len())
            .ok_or(Trap::ArgumentOutOfRange(0))?;
        let mut out = Vec::with_capacity(chars.len() + value.len());
        out.extend_from_slice(&chars[..index]);
        out.extend_from_slice(&value);
        out.extend_from_slice(&chars[index..]);
        let reference = vm.heap_mut().alloc_string(&out);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.Remove(int startIndex [, int count])`: a copy with `count` units (or
    /// the tail, for the one-argument overload) removed at `startIndex`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for bad argument types; [`Trap::ArgumentOutOfRange`] if the
    /// range falls outside the string.
    pub fn string_remove(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.first())?;
        let Some(&Value::Int32(start)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let start = usize::try_from(start)
            .ok()
            .filter(|&start| start <= chars.len())
            .ok_or(Trap::ArgumentOutOfRange(0))?;
        let mut out = chars[..start].to_vec();
        match args.get(2) {
            Some(&Value::Int32(count)) => {
                let count = usize::try_from(count).map_err(|_| Trap::ArgumentOutOfRange(1))?;
                let end = start
                    .checked_add(count)
                    .filter(|&end| end <= chars.len())
                    .ok_or(Trap::ArgumentOutOfRange(1))?;
                out.extend_from_slice(&chars[end..]);
            }
            None => {}
            Some(_) => return Err(Trap::TypeMismatch(Opcode::Call)),
        }
        let reference = vm.heap_mut().alloc_string(&out);
        Ok(Some(Value::Object(reference)))
    }

    /// `System.String.ToCharArray()`: a fresh `char[]` of the string's UTF-16 code units.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string.
    pub fn string_to_char_array(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let elements: Vec<Value> = string_arg_chars(vm, args.first())?
            .iter()
            .map(|&unit| Value::Int32(i32::from(unit)))
            .collect();
        let reference = vm.heap_mut().alloc_array(elements);
        Ok(Some(Value::Object(reference)))
    }

    /// The `this` receiver reference (the first argument) of an instance intrinsic.
    fn receiver_ref(args: &[Value]) -> Result<ObjectRef, Trap> {
        match args.first() {
            Some(&Value::Object(reference)) => Ok(reference),
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// Appends `units` to the string builder at `reference`, growing its tracked
    /// `Capacity` by .NET's rule when the result outgrows it.
    fn string_builder_extend(vm: &mut Vm, reference: ObjectRef, units: &[u16]) -> Result<(), Trap> {
        match vm.heap_mut().string_builder_buf_mut(reference) {
            Some(buffer) => {
                buffer.extend_from_slice(units);
                let length = buffer.len();
                vm.heap_mut().string_builder_grow_capacity(reference, length);
                Ok(())
            }
            None => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.Text.StringBuilder.Append(string)`: appends the argument's code units and
    /// returns the builder, so `Append` calls chain.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string builder.
    pub fn string_builder_append_string(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.get(1))?;
        let this = receiver_ref(args)?;
        string_builder_extend(vm, this, &chars)?;
        Ok(Some(Value::Object(this)))
    }

    /// `StringBuilder.Append(char)`: appends one UTF-16 code unit and returns the builder.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-builder receiver or non-char argument.
    pub fn string_builder_append_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let unit = match args.get(1) {
            Some(&Value::Int32(code)) => code as u16,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let this = receiver_ref(args)?;
        string_builder_extend(vm, this, &[unit])?;
        Ok(Some(Value::Object(this)))
    }

    /// `StringBuilder.Append(int)`: appends the integer's decimal text and returns the builder.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-builder receiver or non-int argument.
    pub fn string_builder_append_int(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = match args.get(1) {
            Some(&Value::Int32(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let units: Vec<u16> = scalar_text(&Value::Int32(value)).encode_utf16().collect();
        let this = receiver_ref(args)?;
        string_builder_extend(vm, this, &units)?;
        Ok(Some(Value::Object(this)))
    }

    /// `StringBuilder.ToString()`: a fresh `System.String` of the accumulated code units.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string builder.
    pub fn string_builder_to_string(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let units = vm
            .heap()
            .string_builder_buf(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?
            .to_vec();
        let reference = vm.heap_mut().alloc_string(&units);
        Ok(Some(Value::Object(reference)))
    }

    /// `StringBuilder.Length` getter: the accumulated code-unit count.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string builder.
    pub fn string_builder_get_length(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let length = vm
            .heap()
            .string_builder_buf(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?
            .len();
        Ok(Some(Value::Int32(length as i32)))
    }

    /// `System.Text.StringBuilder.Insert(int, string)`: inserts the string's code units at the
    /// index and returns the builder.
    ///
    /// # Errors
    /// [`Trap::ArgumentOutOfRange`] if the index is past the end; [`Trap::TypeMismatch`] for a
    /// non-builder receiver or non-int index.
    pub fn string_builder_insert(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let chars = string_arg_chars(vm, args.get(2))?;
        let index = match args.get(1) {
            Some(&Value::Int32(index)) => index,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let this = receiver_ref(args)?;
        match vm.heap_mut().string_builder_buf_mut(this) {
            Some(buffer) => {
                let index = usize::try_from(index).unwrap_or(usize::MAX);
                if index > buffer.len() {
                    return Err(Trap::ArgumentOutOfRange(1));
                }
                buffer.splice(index..index, chars.iter().copied());
                Ok(Some(Value::Object(this)))
            }
            None => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `StringBuilder.Remove(int start, int length)`: removes a range and returns the builder.
    ///
    /// # Errors
    /// [`Trap::ArgumentOutOfRange`] if the range is out of bounds; [`Trap::TypeMismatch`] for a
    /// non-builder receiver or non-int arguments.
    pub fn string_builder_remove(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let start = match args.get(1) {
            Some(&Value::Int32(start)) => start,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let length = match args.get(2) {
            Some(&Value::Int32(length)) => length,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let this = receiver_ref(args)?;
        match vm.heap_mut().string_builder_buf_mut(this) {
            Some(buffer) => {
                let start = usize::try_from(start).map_err(|_| Trap::ArgumentOutOfRange(1))?;
                let length = usize::try_from(length).map_err(|_| Trap::ArgumentOutOfRange(2))?;
                match start.checked_add(length).filter(|end| *end <= buffer.len()) {
                    Some(end) => {
                        buffer.drain(start..end);
                        Ok(Some(Value::Object(this)))
                    }
                    None => Err(Trap::ArgumentOutOfRange(2)),
                }
            }
            None => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `StringBuilder.Replace(char, char)`: replaces every occurrence and returns the builder.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-builder receiver or non-char arguments.
    pub fn string_builder_replace_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (old, new) = match (args.get(1), args.get(2)) {
            (Some(&Value::Int32(old)), Some(&Value::Int32(new))) => (old as u16, new as u16),
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let this = receiver_ref(args)?;
        match vm.heap_mut().string_builder_buf_mut(this) {
            Some(buffer) => {
                for unit in buffer.iter_mut() {
                    if *unit == old {
                        *unit = new;
                    }
                }
                Ok(Some(Value::Object(this)))
            }
            None => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `StringBuilder.Length` setter core (`SetLengthCore`): truncates the buffer when
    /// `value` is below the current length, or extends it with NUL (`\0`) code units when
    /// above -- exactly .NET's `Length` set, which pads the grown tail with `'\0'`. The
    /// observable `Capacity` grows by the usual rule when the new length outgrows it. The
    /// public managed `Length` setter rejects a negative value (a catchable
    /// `ArgumentOutOfRangeException`) before this runs; a stray negative here is also
    /// rejected, defensively, as out of range.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-builder receiver or non-int argument;
    /// [`Trap::ArgumentOutOfRange`] for a negative length.
    pub fn string_builder_set_length(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let Some(&Value::Int32(value)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let length = usize::try_from(value).map_err(|_| Trap::ArgumentOutOfRange(1))?;
        let this = receiver_ref(args)?;
        match vm.heap_mut().string_builder_buf_mut(this) {
            Some(buffer) => {
                buffer.resize(length, 0u16);
                vm.heap_mut().string_builder_grow_capacity(this, length);
                Ok(None)
            }
            None => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `StringBuilder.Capacity` getter: the tracked capacity (>= `Length`).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not a string builder.
    pub fn string_builder_get_capacity(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let capacity = vm
            .heap()
            .string_builder_capacity(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        Ok(Some(Value::Int32(capacity as i32)))
    }

    /// `StringBuilder.this[int]` getter (`get_Chars`): the code unit at `index`. .NET's
    /// indexer getter raises `IndexOutOfRangeException` (NOT `ArgumentOutOfRangeException`,
    /// which is what its *setter* raises) outside `[0, Length)`, so the bound is checked
    /// here and surfaces as [`Trap::IndexOutOfRange`] -- the trap that presents as
    /// `IndexOutOfRangeException` to a managed `catch`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-builder receiver or non-int index;
    /// [`Trap::IndexOutOfRange`] if `index` is outside the live content.
    pub fn string_builder_get_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let Some(&Value::Int32(index)) = args.get(1) else {
            return Err(Trap::TypeMismatch(Opcode::Call));
        };
        let this = receiver_ref(args)?;
        let buffer = vm
            .heap()
            .string_builder_buf(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        let unit = usize::try_from(index)
            .ok()
            .and_then(|i| buffer.get(i))
            .copied()
            .ok_or(Trap::IndexOutOfRange(index))?;
        Ok(Some(Value::Int32(i32::from(unit))))
    }

    /// `StringBuilder.this[int]` setter core (`SetCharCore`): stores `value` at `index`.
    /// The public managed setter performs the `[0, Length)` bound check (raising a catchable
    /// `ArgumentOutOfRangeException`, matching .NET's indexer setter) before this runs; an
    /// out-of-range index here is rejected defensively as [`Trap::IndexOutOfRange`].
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-builder receiver or bad argument types;
    /// [`Trap::IndexOutOfRange`] if `index` is outside the live content.
    pub fn string_builder_set_char(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let (index, unit) = match (args.get(1), args.get(2)) {
            (Some(&Value::Int32(index)), Some(&Value::Int32(unit))) => (index, unit as u16),
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        let this = receiver_ref(args)?;
        let buffer = vm
            .heap_mut()
            .string_builder_buf_mut(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        let slot = usize::try_from(index)
            .ok()
            .and_then(|i| buffer.get_mut(i))
            .ok_or(Trap::IndexOutOfRange(index))?;
        *slot = unit;
        Ok(None)
    }

    /// `System.BitConverter.DoubleToInt64Bits(double)`: the IEEE-754 bit pattern of the
    /// double as an `Int64`, the building block the managed `BitConverter.GetBytes(double)` /
    /// `ToDouble` use (pure managed C# cannot reinterpret a `double`'s bits without `unsafe`).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-double argument.
    #[cfg(feature = "float")]
    pub fn bitconverter_double_to_int64_bits(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = match args.first() {
            Some(&Value::Float(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        Ok(Some(Value::Int64(value.to_bits() as i64)))
    }

    /// `System.BitConverter.Int64BitsToDouble(long)`: the `double` whose IEEE-754 bit
    /// pattern is `value`. The inverse of [`bitconverter_double_to_int64_bits`].
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-long argument.
    #[cfg(feature = "float")]
    pub fn bitconverter_int64_bits_to_double(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let bits = match args.first() {
            Some(&Value::Int64(bits)) => bits,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        Ok(Some(Value::Float(f64::from_bits(bits as u64))))
    }

    /// `System.BitConverter.SingleToInt32Bits(float)`: the IEEE-754 bit pattern of the value
    /// as a single-precision `Int32`. The VM holds a `float` as a true `f32`, so the bits are
    /// read directly -- matching .NET's 4-byte `GetBytes(float)`.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-single argument.
    #[cfg(feature = "float")]
    pub fn bitconverter_single_to_int32_bits(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = match args.first() {
            Some(&Value::Single(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        Ok(Some(Value::Int32(value.to_bits() as i32)))
    }

    /// `System.BitConverter.Int32BitsToSingle(int)`: the `float` whose single-precision
    /// IEEE-754 bit pattern is `value`. The inverse of [`bitconverter_single_to_int32_bits`].
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] for a non-int argument.
    #[cfg(feature = "float")]
    pub fn bitconverter_int32_bits_to_single(
        _vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let bits = match args.first() {
            Some(&Value::Int32(bits)) => bits,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        };
        Ok(Some(Value::Single(f32::from_bits(bits as u32))))
    }

    /// An `i32` index argument as a `usize` (a negative index is out of range).
    fn list_index(arg: Option<&Value>) -> Result<usize, Trap> {
        match arg {
            Some(&Value::Int32(index)) => {
                usize::try_from(index).map_err(|_| Trap::IndexOutOfRange(index))
            }
            _ => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `System.Collections.ArrayList.Add(object)`: appends the value, returning its index.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not an array-backed list.
    pub fn list_add(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
        let value = args.get(1).cloned().unwrap_or(Value::Null);
        let this = receiver_ref(args)?;
        match vm.heap_mut().array_push(this, value) {
            Some(index) => Ok(Some(Value::Int32(index as i32))),
            None => Err(Trap::TypeMismatch(Opcode::Call)),
        }
    }

    /// `ArrayList.get_Item(int)`: the element at the index.
    ///
    /// # Errors
    /// [`Trap::IndexOutOfRange`] if out of range; [`Trap::TypeMismatch`] for a non-list receiver.
    pub fn list_get_item(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let index = list_index(args.get(1))?;
        vm.heap()
            .array_get(this, index)
            .map(Some)
            .ok_or(Trap::IndexOutOfRange(index as i32))
    }

    /// `ArrayList.set_Item(int, object)`: stores the value at the index.
    ///
    /// # Errors
    /// [`Trap::IndexOutOfRange`] if the index is out of range.
    pub fn list_set_item(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let index = list_index(args.get(1))?;
        let value = args.get(2).cloned().unwrap_or(Value::Null);
        if vm.heap_mut().array_set(this, index, value) {
            Ok(None)
        } else {
            Err(Trap::IndexOutOfRange(index as i32))
        }
    }

    /// `ArrayList.get_Count()`: the element count.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not an array-backed list.
    pub fn list_get_count(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let count = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        Ok(Some(Value::Int32(count as i32)))
    }

    /// `ArrayList.Clear()`: removes every element.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not an array-backed list.
    pub fn list_clear(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        if vm.heap_mut().array_clear(this) {
            Ok(None)
        } else {
            Err(Trap::TypeMismatch(Opcode::Call))
        }
    }

    /// `ArrayList.RemoveAt(int)`: removes the element at the index.
    ///
    /// # Errors
    /// [`Trap::IndexOutOfRange`] if the index is out of range.
    pub fn list_remove_at(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let index = list_index(args.get(1))?;
        if vm.heap_mut().array_remove_at(this, index) {
            Ok(None)
        } else {
            Err(Trap::IndexOutOfRange(index as i32))
        }
    }

    /// `ArrayList.Insert(int, object)`: inserts the value before the index.
    ///
    /// # Errors
    /// [`Trap::IndexOutOfRange`] if the index is past the end.
    pub fn list_insert(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let index = list_index(args.get(1))?;
        let value = args.get(2).cloned().unwrap_or(Value::Null);
        if vm.heap_mut().array_insert(this, index, value) {
            Ok(None)
        } else {
            Err(Trap::IndexOutOfRange(index as i32))
        }
    }

    /// `Object.Equals` key semantics for a `Hashtable`: two nulls are equal; two strings by
    /// their code units; two boxed value types by their boxed value; otherwise reference
    /// identity. (Keys are scanned linearly -- no hashing yet.)
    fn value_key_equals(vm: &Vm, left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Null, Value::Null) => true,
            (Value::Object(a), Value::Object(b)) => {
                if a == b {
                    return true;
                }
                match (vm.heap().get(*a), vm.heap().get(*b)) {
                    (Some(Object::Str(_)), Some(Object::Str(_))) => {
                        vm.heap().as_string(*a).as_deref() == vm.heap().as_string(*b).as_deref()
                    }
                    (
                        Some(Object::Boxed { value: va, .. }),
                        Some(Object::Boxed { value: vb, .. }),
                    ) => va == vb,
                    _ => false,
                }
            }
            _ => left == right,
        }
    }

    /// The slot index of `key` in the flattened `[k, v, k, v, ...]` pair list, if present.
    fn map_find(vm: &Vm, this: ObjectRef, key: &Value) -> Option<usize> {
        let len = vm.heap().array_len(this)?;
        let mut slot = 0;
        while slot < len {
            let stored = vm.heap().array_get(this, slot).unwrap_or(Value::Null);
            if value_key_equals(vm, key, &stored) {
                return Some(slot);
            }
            slot += 2;
        }
        None
    }

    /// `System.Collections.Hashtable.Add(object key, object value)`: appends the pair. A
    /// duplicate key is not rejected (the earlier entry shadows it on lookup).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn map_add(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let key = args.get(1).cloned().unwrap_or(Value::Null);
        let value = args.get(2).cloned().unwrap_or(Value::Null);
        if vm.heap_mut().array_push(this, key).is_none() {
            return Err(Trap::TypeMismatch(Opcode::Call));
        }
        vm.heap_mut().array_push(this, value);
        Ok(None)
    }

    /// `Hashtable.get_Item(object key)`: the value for `key`, or null if absent (no throw).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn map_get_item(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let key = args.get(1).cloned().unwrap_or(Value::Null);
        match map_find(vm, this, &key) {
            Some(slot) => Ok(Some(
                vm.heap().array_get(this, slot + 1).unwrap_or(Value::Null),
            )),
            None => Ok(Some(Value::Null)),
        }
    }

    /// `Hashtable.set_Item(object key, object value)`: updates `key`'s value, or adds the pair.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn map_set_item(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let key = args.get(1).cloned().unwrap_or(Value::Null);
        let value = args.get(2).cloned().unwrap_or(Value::Null);
        match map_find(vm, this, &key) {
            Some(slot) => {
                vm.heap_mut().array_set(this, slot + 1, value);
            }
            None => {
                vm.heap_mut().array_push(this, key);
                vm.heap_mut().array_push(this, value);
            }
        }
        Ok(None)
    }

    /// `Hashtable.get_Count()`: the number of key/value pairs.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn map_get_count(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let len = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        Ok(Some(Value::Int32((len / 2) as i32)))
    }

    /// `Hashtable.Contains(object key)` / `ContainsKey(object key)`: whether `key` is present.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn map_contains(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let key = args.get(1).cloned().unwrap_or(Value::Null);
        let present = map_find(vm, this, &key).is_some();
        Ok(Some(Value::Int32(i32::from(present))))
    }

    /// `Hashtable.Remove(object key)`: removes `key`'s pair if present.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn map_remove(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let key = args.get(1).cloned().unwrap_or(Value::Null);
        if let Some(slot) = map_find(vm, this, &key) {
            vm.heap_mut().array_remove_at(this, slot + 1);
            vm.heap_mut().array_remove_at(this, slot);
        }
        Ok(None)
    }

    /// `Stack.Push(object)` / `Queue.Enqueue(object)`: appends the value to the backing list.
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn collection_push(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let value = args.get(1).cloned().unwrap_or(Value::Null);
        let this = receiver_ref(args)?;
        if vm.heap_mut().array_push(this, value).is_none() {
            return Err(Trap::TypeMismatch(Opcode::Call));
        }
        Ok(None)
    }

    /// `Stack.Pop()`: removes and returns the top (the last element).
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] if the stack is empty (.NET throws `InvalidOperationException`).
    pub fn stack_pop(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let len = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        if len == 0 {
            return Err(Trap::InvalidArgument);
        }
        let value = vm.heap().array_get(this, len - 1).unwrap_or(Value::Null);
        vm.heap_mut().array_remove_at(this, len - 1);
        Ok(Some(value))
    }

    /// `Stack.Peek()`: returns the top (the last element) without removing it.
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] if the stack is empty.
    pub fn stack_peek(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let len = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        if len == 0 {
            return Err(Trap::InvalidArgument);
        }
        Ok(Some(
            vm.heap().array_get(this, len - 1).unwrap_or(Value::Null),
        ))
    }

    /// `Queue.Dequeue()`: removes and returns the front (the first element).
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] if the queue is empty.
    pub fn queue_dequeue(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let len = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        if len == 0 {
            return Err(Trap::InvalidArgument);
        }
        let value = vm.heap().array_get(this, 0).unwrap_or(Value::Null);
        vm.heap_mut().array_remove_at(this, 0);
        Ok(Some(value))
    }

    /// `Queue.Peek()`: returns the front (the first element) without removing it.
    ///
    /// # Errors
    /// [`Trap::InvalidArgument`] if the queue is empty.
    pub fn queue_peek(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let len = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        if len == 0 {
            return Err(Trap::InvalidArgument);
        }
        Ok(Some(vm.heap().array_get(this, 0).unwrap_or(Value::Null)))
    }

    /// `Stack.Contains(object)` / `Queue.Contains(object)`: whether an equal element is present
    /// (by `Object.Equals` semantics; see [`value_key_equals`]).
    ///
    /// # Errors
    /// [`Trap::TypeMismatch`] if the receiver is not array-backed.
    pub fn collection_contains(
        vm: &mut Vm,
        _module: &Module,
        args: &[Value],
    ) -> Result<Option<Value>, Trap> {
        let this = receiver_ref(args)?;
        let needle = args.get(1).cloned().unwrap_or(Value::Null);
        let len = vm
            .heap()
            .array_len(this)
            .ok_or(Trap::TypeMismatch(Opcode::Call))?;
        let mut present = false;
        let mut index = 0;
        while index < len {
            let element = vm.heap().array_get(this, index).unwrap_or(Value::Null);
            if value_key_equals(vm, &needle, &element) {
                present = true;
                break;
            }
            index += 1;
        }
        Ok(Some(Value::Int32(i32::from(present))))
    }
}

#[cfg(feature = "NETMFv4_4")]
pub use extended::*;

/// `System.Object.ReferenceEquals(object, object)`: reference identity (two nulls are
/// equal; a null and an object are not). Value-type arguments arrive boxed, so distinct
/// boxes compare unequal -- matching .NET. A Kernel-Profile member.
///
/// # Errors
/// Never; the signature matches the intrinsic ABI.
pub fn object_reference_equals(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let same = match (args.first(), args.get(1)) {
        (Some(Value::Object(left)), Some(Value::Object(right))) => left == right,
        (Some(Value::Null), Some(Value::Null)) => true,
        _ => false,
    };
    Ok(Some(Value::Int32(i32::from(same))))
}

/// `System.Object..ctor()`: the base constructor every constructor chains to. With
/// no object header to initialize here, it is a no-op (it still receives `this`).
///
/// # Errors
/// Never errors.
pub fn object_ctor(_vm: &mut Vm, _module: &Module, _args: &[Value]) -> Result<Option<Value>, Trap> {
    Ok(None)
}

/// `System.Exception..ctor()` / `.ctor(string)` / `.ctor(string, Exception)`: records
/// the message argument (if a string is present) as the exception's message; the inner
/// exception is dropped for now. `this` is the exception object (arg 0).
///
/// # Errors
/// Never errors (an absent or non-string message is simply not recorded).
pub fn exception_ctor(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    if let (Some(&Value::Object(this)), Some(&Value::Object(message))) = (args.first(), args.get(1))
    {
        vm.set_exception_message(this, message);
    }
    Ok(None)
}

/// `System.Exception.get_Message`: the stored message string, or an empty string if
/// none was given (`Message` is conventionally non-null). `this` is the exception.
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not an object reference.
pub fn exception_get_message(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(this)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let message = match vm.exception_message(this) {
        Some(message) => message,
        None => vm.heap_mut().alloc_string(&[]),
    };
    Ok(Some(Value::Object(message)))
}

/// Allocates a `System.String` holding `text` and returns it as a value.
fn alloc_str(vm: &mut Vm, text: &str) -> Value {
    let chars: Vec<u16> = text.encode_utf16().collect();
    Value::Object(vm.heap_mut().alloc_string(&chars))
}

/// The `this` of a 32-bit value-type `ToString`: the `Int32` directly (a managed
/// pointer the caller has dereferenced) or a boxed one.
fn int32_self(vm: &Vm, args: &[Value]) -> Option<i32> {
    match args.first()? {
        Value::Int32(value) => Some(*value),
        Value::Object(reference) => match vm.heap().boxed_value(*reference) {
            Some(Value::Int32(value)) => Some(value),
            _ => None,
        },
        _ => None,
    }
}

/// `System.Int32.ToString()`: the value's decimal text.
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not an `Int32` (or a boxed one).
pub fn int32_to_string(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = int32_self(vm, args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    Ok(Some(alloc_str(vm, &value.to_string())))
}

/// `System.Boolean.ToString()`: "True" or "False".
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not a boolean (an `Int32` 0/1).
pub fn boolean_to_string(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = int32_self(vm, args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    Ok(Some(alloc_str(
        vm,
        if value != 0 { "True" } else { "False" },
    )))
}

/// `System.Char.ToString()`: the single character.
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not a char (an `Int32` code unit).
pub fn char_to_string(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = int32_self(vm, args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    let reference = vm.heap_mut().alloc_string(&[value as u16]);
    Ok(Some(Value::Object(reference)))
}

/// `System.Int64.ToString()`: the value's decimal text.
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not an `Int64`.
pub fn int64_to_string(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(Value::Int64(value) | Value::NativeInt(value)) => *value,
        Some(Value::Object(reference)) => match vm.heap().boxed_value(*reference) {
            Some(Value::Int64(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    Ok(Some(alloc_str(vm, &value.to_string())))
}

/// `System.Double.ToString()`: the value's text (Infinity / NaN spelled out).
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not a `Double`.
#[cfg(feature = "float")]
pub fn double_to_string(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(Value::Float(value)) => *value,
        Some(Value::Object(reference)) => match vm.heap().boxed_value(*reference) {
            Some(Value::Float(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    Ok(Some(alloc_str(vm, &format_double(value))))
}

/// `System.Single.ToString()`: the value's shortest round-trippable text (Infinity / NaN spelled
/// out), rendered at f32 precision so a Single prints its own digits, not the f64-widened decimal.
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not a `Single`.
#[cfg(feature = "float")]
pub fn single_to_string(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(Value::Single(value)) => *value,
        Some(Value::Object(reference)) => match vm.heap().boxed_value(*reference) {
            Some(Value::Single(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    Ok(Some(alloc_str(vm, &format_single(value))))
}

/// `System.Single.ToFixed(float, int)`: the value rendered with EXACTLY `decimals` fractional
/// digits (.NET's "F" formatter for a Single), rounding the exact decimal value of the IEEE-754
/// single half-to-even. The Single is widened to `f64` only AFTER it already carries the
/// single-rounded value, so the fixed-point digits are those of the f32 -- matching .NET.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the value is not a single or the precision is not an `Int32`.
#[cfg(feature = "float")]
pub fn single_to_fixed(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(&Value::Single(value)) => value,
        Some(&Value::Object(reference)) => match vm.heap().boxed_value(reference) {
            Some(Value::Single(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let decimals = match args.get(1) {
        Some(&Value::Int32(decimals)) => decimals.max(0) as usize,
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let text = if value.is_nan() {
        String::from("NaN")
    } else if value.is_infinite() {
        String::from(if value < 0.0 { "-Infinity" } else { "Infinity" })
    } else {
        format!("{:.*}", decimals, f64::from(value))
    };
    Ok(Some(alloc_str(vm, &text)))
}

/// `System.Double.ToFixed(double, int)`: the value rendered with EXACTLY `decimals`
/// fractional digits (.NET's "F" / "N"-without-grouping body). Rust's core float
/// formatter (`{:.*}`) rounds the EXACT decimal value of the IEEE-754 double
/// half-to-even, byte-for-byte the way .NET's fixed-point formatter does -- so
/// `(2.005).ToString("F2")` is "2.00" (not "2.01") and `(9.995).ToString("F2")` is
/// "9.99", matching .NET. The specials carry .NET's spelling (`{:.*}` would print
/// "inf"/"NaN" without a sign on -inf), so they are handled here rather than delegated.
/// Negative precision is treated as zero. The managed `Double.ToString` calls this for
/// the digits, then does any thousands grouping ("N") in managed code.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the value is not a double or the precision is not an `Int32`.
#[cfg(feature = "float")]
pub fn double_to_fixed(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(&Value::Float(value)) => value,
        Some(&Value::Object(reference)) => match vm.heap().boxed_value(reference) {
            Some(Value::Float(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let decimals = match args.get(1) {
        Some(&Value::Int32(decimals)) => decimals.max(0) as usize,
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let text = if value.is_nan() {
        String::from("NaN")
    } else if value.is_infinite() {
        String::from(if value < 0.0 { "-Infinity" } else { "Infinity" })
    } else {
        format!("{value:.decimals$}")
    };
    Ok(Some(alloc_str(vm, &text)))
}

/// Renders `value` in .NET exponential ("E"/"e") format: a normalized mantissa carrying `precision`
/// fractional digits, then the `E`/`e` letter, a mandatory sign, and an exponent of at least three
/// digits (e.g. `1.234500E+003`). Rust's `{:e}` supplies the value-accurate normalized digits (the
/// same float engine the F/G formatters use, which match .NET); only the exponent's sign-and-width
/// layout is reshaped to .NET's. Specials spell out as `NaN` / `Infinity` / `-Infinity`.
#[cfg(feature = "float")]
fn format_exponential(value: f64, precision: usize, upper: bool) -> String {
    if value.is_nan() {
        return String::from("NaN");
    }
    if value.is_infinite() {
        return String::from(if value < 0.0 { "-Infinity" } else { "Infinity" });
    }
    let raw = format!("{value:.precision$e}");
    let Some((mantissa, exponent)) = raw.split_once('e') else {
        return raw;
    };
    let exp = exponent.parse::<i32>().unwrap_or(0);
    let exp_char = if upper { 'E' } else { 'e' };
    let exp_sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}{exp_char}{exp_sign}{:03}", exp.unsigned_abs())
}

/// `System.Double.ToExponential(double, int, bool)`: the "E"/"e" exponential rendering (the managed
/// formatter's exponential body; `upper` selects the `E`/`e` letter case).
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not a `Double`.
#[cfg(feature = "float")]
pub fn double_to_exponential(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(&Value::Float(value)) => value,
        Some(&Value::Object(reference)) => match vm.heap().boxed_value(reference) {
            Some(Value::Float(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let precision = match args.get(1) {
        Some(&Value::Int32(precision)) => precision.max(0) as usize,
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let upper = matches!(args.get(2), Some(&Value::Int32(flag)) if flag != 0);
    Ok(Some(alloc_str(vm, &format_exponential(value, precision, upper))))
}

/// `System.Single.ToExponential(float, int, bool)`: the "E"/"e" exponential rendering for a Single
/// (widened to f64 exactly, like the Single "F" body).
///
/// # Errors
/// [`Trap::TypeMismatch`] if `this` is not a `Single`.
#[cfg(feature = "float")]
pub fn single_to_exponential(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(&Value::Single(value)) => value,
        Some(&Value::Object(reference)) => match vm.heap().boxed_value(reference) {
            Some(Value::Single(value)) => value,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let precision = match args.get(1) {
        Some(&Value::Int32(precision)) => precision.max(0) as usize,
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let upper = matches!(args.get(2), Some(&Value::Int32(flag)) if flag != 0);
    Ok(Some(alloc_str(vm, &format_exponential(f64::from(value), precision, upper))))
}

/// `System.Single::ParseValid(string)`: the numeric conversion behind the managed `Single.Parse` /
/// `TryParse`, which have ALREADY validated the format (so the only work left is the
/// decimal-to-nearest-single rounding managed C# cannot do without `unsafe`). Recognizes the .NET
/// specials (`NaN` / `[+-]Infinity`, case-insensitively, after trimming) and otherwise rounds the
/// decimal with Rust's `f32` parser -- matching .NET's invariant rounding. Malformed input cannot
/// reach here (the managed validator gates it), but a stray case still traps rather than guessing.
///
/// # Errors
/// [`Trap::InvalidArgument`] if the (already-validated) text somehow does not parse;
/// [`Trap::TypeMismatch`] for a non-string argument.
#[cfg(feature = "float")]
pub fn single_parse(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let chars = string_arg_chars(vm, args.first())?;
    let text = String::from_utf16(&chars).map_err(|_| Trap::InvalidArgument)?;
    let trimmed = text.trim();
    let value = match trimmed.to_ascii_lowercase().as_str() {
        "nan" => f32::NAN,
        "infinity" | "+infinity" => f32::INFINITY,
        "-infinity" => f32::NEG_INFINITY,
        _ => trimmed.parse::<f32>().map_err(|_| Trap::InvalidArgument)?,
    };
    Ok(Some(Value::Single(value)))
}

/// `System.Object.ToString()`: a value's display text -- a boxed value type by its
/// representation, a string verbatim, anything else as "object".
///
/// # Errors
/// Never errors.
pub fn object_to_string(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let text = object_text(vm, module, args.first());
    Ok(Some(alloc_str(vm, &text)))
}

/// `System.Console.WriteLine(object)`: the object's text, then a line terminator.
///
/// # Errors
/// Never errors.
pub fn console_write_line_object(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let text = object_text(vm, module, args.first());
    write_line_text(vm, &text);
    Ok(None)
}

/// Renders an object for `Object.ToString` / `WriteLine(object)`: a string verbatim, a
/// boxed value type by its representation, null/absent as empty, else "object".
fn object_text(vm: &Vm, module: &Module, value: Option<&Value>) -> String {
    match value {
        Some(Value::Object(reference)) => match vm.heap().get(*reference) {
            Some(Object::Str(chars)) => String::from_utf16_lossy(&decode_string(chars)),
            Some(Object::StringBuilder { buf, .. }) => String::from_utf16_lossy(buf),
            Some(Object::Boxed { type_token, value }) => boxed_text(module, *type_token, value),
            _ => String::from("object"),
        },
        Some(Value::Null) | None => String::new(),
        Some(other) => scalar_text(other),
    }
}

/// `System.String.Concat(object, object)`: each argument rendered by `ToString` (null as
/// empty) and joined -- the form `string + value` lowers to. A Kernel-Profile member.
///
/// # Errors
/// Never; a non-string argument renders via its boxed representation.
pub fn string_concat_object2(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let mut text = object_text(vm, module, args.first());
    text.push_str(&object_text(vm, module, args.get(1)));
    let units: Vec<u16> = text.encode_utf16().collect();
    let reference = vm.heap_mut().alloc_string(&units);
    Ok(Some(Value::Object(reference)))
}

/// `System.String.Concat(object, object, object)`: three arguments rendered by `ToString`
/// and joined (the flattened `+` chain `a + b + c` over mixed types). A Kernel member.
///
/// # Errors
/// Never; a non-string argument renders via its boxed representation.
pub fn string_concat_object3(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let mut text = object_text(vm, module, args.first());
    text.push_str(&object_text(vm, module, args.get(1)));
    text.push_str(&object_text(vm, module, args.get(2)));
    let units: Vec<u16> = text.encode_utf16().collect();
    let reference = vm.heap_mut().alloc_string(&units);
    Ok(Some(Value::Object(reference)))
}

/// Renders a boxed value type: an enum as its constant name (when the value is a known
/// constant of that enum), otherwise the underlying value's text. The boxed `type_token` is
/// the asm-folded handle (the assembly folded in at the `box` site), so the enum maps are
/// queried by that handle directly.
fn boxed_text(module: &Module, type_token: u64, value: &Value) -> String {
    if let Some(integer) = enum_underlying(value) {
        if let Some(text) = module.enum_name_or_flags(type_token, integer, false) {
            return text;
        }
    }
    scalar_text(value)
}

/// The underlying integer of an enum value, for the constant-name lookup.
fn enum_underlying(value: &Value) -> Option<i64> {
    match value {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        _ => None,
    }
}

/// Renders a scalar value (the numeric kinds) as text; anything else as "object".
fn scalar_text(value: &Value) -> String {
    match value {
        Value::Int32(value) => value.to_string(),
        Value::Int64(value) | Value::NativeInt(value) => value.to_string(),
        #[cfg(feature = "float")]
        Value::Float(value) => format_double(*value),
        #[cfg(feature = "float")]
        Value::Single(value) => format_single(*value),
        _ => String::from("object"),
    }
}

/// `System.Delegate.Combine(a, b)`: a delegate whose invocation list is a's followed by
/// b's (multicast, the `+=` operator). A null operand contributes nothing.
///
/// # Errors
/// Never errors (a non-delegate operand contributes no invocations).
pub fn delegate_combine(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let mut invocations = delegate_list(vm, args.first());
    invocations.extend(delegate_list(vm, args.get(1)));
    if invocations.is_empty() {
        return Ok(Some(Value::Null));
    }
    let reference = vm.heap_mut().alloc_multicast(invocations);
    Ok(Some(Value::Object(reference)))
}

/// `System.Delegate.Remove(source, value)`: `source`'s invocation list with `value`'s
/// invocations removed (the `-=` operator); null if nothing remains.
///
/// # Errors
/// Never errors.
pub fn delegate_remove(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let removed = delegate_list(vm, args.get(1));
    let invocations: Vec<(Value, u32)> = delegate_list(vm, args.first())
        .into_iter()
        .filter(|entry| !removed.contains(entry))
        .collect();
    if invocations.is_empty() {
        return Ok(Some(Value::Null));
    }
    let reference = vm.heap_mut().alloc_multicast(invocations);
    Ok(Some(Value::Object(reference)))
}

/// `System.Threading.Interlocked.CompareExchange<T>(ref T location, T value, T comparand)`:
/// if `*location` equals `comparand`, set `*location = value`; return the original `*location`
/// either way. csc lowers a field-like event's `+=`/`-=` to a compare-and-swap retry loop
/// around this. The interpreter is single-threaded, so the compare-and-set is a plain
/// (non-atomic) one -- with no other thread to race, the swap always observes the comparand
/// and the loop completes on its first iteration.
///
/// The first argument arrives as the raw managed pointer (`&`, un-dereferenced -- the call
/// dispatch keeps it so for this intrinsic). Only a heap-reachable pointer is supported (a
/// field, array element, static, or box -- which is what `ldflda` on an event's backing field
/// yields); the comparison is by stack-value identity (reference identity for the delegate /
/// object references compared here, matching `Interlocked`'s reference semantics).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the first argument is not a managed pointer the heap can reach.
pub fn interlocked_compare_exchange(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let location = match args.first() {
        Some(Value::ByRef(location)) => location.clone(),
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let value = args.get(1).cloned().unwrap_or(Value::Null);
    let comparand = args.get(2).cloned().unwrap_or(Value::Null);
    let original = read_vm_location(vm, &location)?;
    if original == comparand {
        write_vm_location(vm, &location, value)?;
    }
    Ok(Some(original))
}

/// Reads the value at a heap-reachable managed pointer using only the `Vm` an intrinsic holds
/// (it has no call frames, so a pointer into a frame local/argument is out of reach).
fn read_vm_location(vm: &Vm, location: &crate::value::Location) -> Result<Value, Trap> {
    use crate::value::Location;
    let value = match *location {
        Location::Field { object, slot } => vm.heap().instance_field(object, slot),
        Location::Element { array, index, .. } => vm.heap().array_get(array, index),
        Location::Static { slot } => vm.static_field(slot),
        Location::Boxed { object } => vm.heap().boxed_value(object),
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    Ok(value.unwrap_or(Value::Null))
}

/// Writes `value` at a heap-reachable managed pointer using only the `Vm` (see
/// [`read_vm_location`]).
fn write_vm_location(
    vm: &mut Vm,
    location: &crate::value::Location,
    value: Value,
) -> Result<(), Trap> {
    use crate::value::Location;
    let ok = match *location {
        Location::Field { object, slot } => vm.heap_mut().set_instance_field(object, slot, value),
        Location::Element { array, index, .. } => vm.heap_mut().array_set(array, index, value),
        Location::Static { slot } => {
            vm.set_static_field(slot, value);
            true
        }
        Location::Boxed { object } => vm.heap_mut().set_boxed_value(object, value),
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    if ok {
        Ok(())
    } else {
        Err(Trap::NullReference)
    }
}

/// The invocation list of a delegate value (empty for null or a non-delegate).
fn delegate_list(vm: &Vm, value: Option<&Value>) -> Vec<(Value, u32)> {
    match value {
        Some(Value::Object(reference)) => vm
            .heap()
            .delegate_invocations(*reference)
            .map(<[(Value, u32)]>::to_vec)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// `System.GC.SuppressFinalize(object)`: cancels the object's finalization -- the
/// deterministic-cleanup (Dispose) pattern. Present only with the `finalizers` feature; a
/// build without it omits the `System.GC` finalization surface entirely.
///
/// # Errors
/// Never errors (a non-object argument is ignored).
#[cfg(feature = "finalizers")]
pub fn suppress_finalize(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    if let Some(&Value::Object(object)) = args.first() {
        vm.heap_mut().suppress_finalizer(object);
    }
    Ok(None)
}

/// `System.GC.ReRegisterForFinalize(object)`: re-arms the object's finalization after a
/// prior suppression. Present only with the `finalizers` feature.
///
/// # Errors
/// Never errors (a non-object argument is ignored).
#[cfg(feature = "finalizers")]
pub fn reregister_finalize(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    if let Some(&Value::Object(object)) = args.first() {
        vm.heap_mut().register_finalizer(object);
    }
    Ok(None)
}

/// `System.GC.Collect()`: requests a collection at the next safepoint. Present only with
/// the `gc` feature; a no-GC build omits the `System.GC` class entirely.
///
/// # Errors
/// Never errors.
#[cfg(feature = "gc")]
pub fn gc_collect(vm: &mut Vm, _module: &Module, _args: &[Value]) -> Result<Option<Value>, Trap> {
    vm.request_collect();
    Ok(None)
}

/// `System.WeakReference.MakeWeakCell(object)`: allocates a weak cell holding `target` and returns
/// a reference to it. `System.WeakReference` stores the cell by a STRONG reference, so the cell
/// lives with the WeakReference; the collector treats the cell's target weakly. Present only with
/// the `gc` feature (a weak reference is meaningless without a collector).
///
/// # Errors
/// Never errors (a missing argument is treated as a null target).
#[cfg(feature = "gc")]
pub fn weak_make_cell(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let target = args.first().cloned().unwrap_or(Value::Null);
    let cell = vm.heap_mut().alloc_weak(target);
    Ok(Some(Value::Object(cell)))
}

/// `System.WeakReference.ReadWeakCell(object cell)`: the weak cell's current target -- `Null` once
/// the target has been reclaimed (which is what makes `IsAlive` go false).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a weak-cell reference.
#[cfg(feature = "gc")]
pub fn weak_read_cell(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(cell)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    Ok(Some(vm.heap().weak_cell_target(cell).unwrap_or(Value::Null)))
}

/// `System.WeakReference.WriteWeakCell(object cell, object target)`: re-points the weak cell at a
/// new target (the `WeakReference.Target` setter).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the first argument is not a weak-cell reference.
#[cfg(feature = "gc")]
pub fn weak_write_cell(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(cell)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let target = args.get(1).cloned().unwrap_or(Value::Null);
    vm.heap_mut().set_weak_cell_target(cell, target);
    Ok(None)
}

/// `System.Threading.Thread.StartThread(ThreadStart)`: reads the delegate's bound `(target,
/// method)`, reserves a green-thread id, and asks the scheduler to spawn a thread running it (a
/// thread IS a reified `Session`); returns the id, which the managed `Thread` stores for `Join`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a single-cast delegate.
pub fn thread_start(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(delegate)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let background = matches!(args.get(1), Some(&Value::Int32(flag)) if flag != 0);
    let invocations = vm
        .heap()
        .delegate_invocations(delegate)
        .ok_or(Trap::TypeMismatch(Opcode::Call))?
        .to_vec();
    let bound = invocations.first().ok_or(Trap::TypeMismatch(Opcode::Call))?;
    let target = bound.0.clone();
    let method = bound.1;
    let id = vm.alloc_thread_id();
    vm.request_spawn(id, method, target, background);
    Ok(Some(Value::Int32(id as i32)))
}

/// `System.Threading.Thread.JoinThread(int)`: blocks the running thread until thread `id` finishes.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an int.
pub fn thread_join(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let id = match args.first() {
        Some(&Value::Int32(id)) => id as u32,
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    vm.request_join(id);
    Ok(None)
}

/// `System.Threading.Thread.YieldThread()`: cooperatively yields to the scheduler (the body of
/// `Thread.Yield`).
///
/// # Errors
/// Never errors.
pub fn thread_yield(
    vm: &mut Vm,
    _module: &Module,
    _args: &[Value],
) -> Result<Option<Value>, Trap> {
    vm.request_yield();
    Ok(None)
}

/// `System.Threading.Thread.SleepThread(int)`: blocks the running thread for `millisecondsTimeout`
/// milliseconds (`Thread.Sleep`). Other green threads run meanwhile and the scheduler idle-sleeps the
/// OS thread to the nearest deadline; without a host clock it degrades to a cooperative yield (no
/// real delay). A negative timeout is clamped to 0.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an int.
pub fn thread_sleep(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(millis)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    vm.request_sleep(millis.max(0) as u64);
    Ok(None)
}


/// Returned when a socket op parked the thread; the managed caller re-invokes it on wake.
const WOULD_BLOCK: i32 = -1;
/// Returned when a socket op failed; the managed caller throws a `SocketException`.
const SOCK_ERROR: i32 = -2;

/// Decodes a socket-handle argument (a non-negative `int`) at `args[index]`.
fn socket_arg(args: &[Value], index: usize) -> Result<u32, Trap> {
    match args.get(index) {
        Some(&Value::Int32(handle)) => Ok(handle as u32),
        _ => Err(Trap::TypeMismatch(Opcode::Call)),
    }
}

/// Reads a managed `byte[]` IP address (4 octets IPv4 / 16 IPv6, network order) into a byte vector
/// for the seam, which picks the address family from the length.
fn read_addr_bytes(vm: &mut Vm, array: ObjectRef) -> alloc::vec::Vec<u8> {
    let len = vm.heap_mut().array_len(array).unwrap_or(0);
    let mut bytes = alloc::vec![0u8; len];
    for (i, slot) in bytes.iter_mut().enumerate() {
        if let Some(Value::Int32(octet)) = vm.heap_mut().array_get(array, i) {
            *slot = octet as u8;
        }
    }
    bytes
}

/// `Socket.ConnectStart(int addr, int port)`: opens a TCP socket and begins connecting (IPv4, host
/// byte order). Returns the socket handle (>= 0) immediately, or `SOCK_ERROR`; the managed caller then
/// loops on `ConnectPoll` until the connection completes.
pub fn socket_connect_start(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(addr_array)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(port)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let addr = read_addr_bytes(vm, addr_array);
    let result = match vm.net_backend() {
        Some(backend) => backend.tcp_connect(&addr, port as u16),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(handle) => handle as i32,
        _ => SOCK_ERROR,
    })))
}

/// `Socket.ConnectPoll(int handle)`: `0` connected, `WOULD_BLOCK` still connecting (the thread parks
/// for writability), `SOCK_ERROR` the connect failed.
pub fn socket_connect_poll(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let result = match vm.net_backend() {
        Some(backend) => backend.connect_check(handle),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(()) => 0,
        NetResult::WouldBlock => {
            vm.request_block_on_io(handle, Interest::Write);
            WOULD_BLOCK
        }
        NetResult::Error => SOCK_ERROR,
    })))
}

/// `Socket.ListenStart(int addr, int port, int backlog)`: binds a TCP listener (port 0 = ephemeral)
/// and begins listening. Returns the listener handle (>= 0) or `SOCK_ERROR`.
pub fn socket_listen(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(addr_array)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(port)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(backlog)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let addr = read_addr_bytes(vm, addr_array);
    let result = match vm.net_backend() {
        Some(backend) => backend.tcp_listen(&addr, port as u16, backlog),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(handle) => handle as i32,
        _ => SOCK_ERROR,
    })))
}

/// `Socket.AcceptPoll(int listener)`: a newly accepted connection's handle (>= 0), `WOULD_BLOCK` (no
/// connection pending -- the thread parks for readability), or `SOCK_ERROR`.
pub fn socket_accept(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let listener = socket_arg(args, 0)?;
    let result = match vm.net_backend() {
        Some(backend) => backend.accept(listener),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(handle) => handle as i32,
        NetResult::WouldBlock => {
            vm.request_block_on_io(listener, Interest::Read);
            WOULD_BLOCK
        }
        NetResult::Error => SOCK_ERROR,
    })))
}

/// `Socket.SendPoll(int handle, byte[] buffer, int offset, int count)`: bytes sent (>= 0),
/// `WOULD_BLOCK` (the send buffer is full -- the thread parks for writability), or `SOCK_ERROR`.
pub fn socket_send(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let mut buf = alloc::vec![0u8; count];
    for (i, slot) in buf.iter_mut().enumerate() {
        if let Some(Value::Int32(byte)) = vm.heap_mut().array_get(array, offset + i) {
            *slot = byte as u8;
        }
    }
    let result = match vm.net_backend() {
        Some(backend) => backend.send(handle, &buf),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(n) => n as i32,
        NetResult::WouldBlock => {
            vm.request_block_on_io(handle, Interest::Write);
            WOULD_BLOCK
        }
        NetResult::Error => SOCK_ERROR,
    })))
}

/// `Socket.ReceivePoll(int handle, byte[] buffer, int offset, int count)`: bytes received (>= 0; `0`
/// = the peer closed cleanly), `WOULD_BLOCK` (no data yet -- the thread parks for readability), or
/// `SOCK_ERROR`.
pub fn socket_recv(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let mut buf = alloc::vec![0u8; count];
    let result = match vm.net_backend() {
        Some(backend) => backend.recv(handle, &mut buf),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    match result {
        NetResult::Ready(n) => {
            for i in 0..n {
                vm.heap_mut()
                    .array_set(array, offset + i, Value::Int32(i32::from(buf[i])));
            }
            Ok(Some(Value::Int32(n as i32)))
        }
        NetResult::WouldBlock => {
            vm.request_block_on_io(handle, Interest::Read);
            Ok(Some(Value::Int32(WOULD_BLOCK)))
        }
        NetResult::Error => Ok(Some(Value::Int32(SOCK_ERROR))),
    }
}

/// `Socket.LocalPort(int handle)`: the local port the socket/listener is bound to, or `-1`.
pub fn socket_local_port(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let port = match vm.net_backend() {
        Some(backend) => backend.local_port(handle),
        None => None,
    };
    Ok(Some(Value::Int32(port.map_or(-1, i32::from))))
}

/// `Socket.CloseSocket(int handle)`: closes a socket or listener and releases its handle.
pub fn socket_close(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    if let Some(backend) = vm.net_backend() {
        backend.close(handle);
    }
    Ok(None)
}

/// `Socket.UdpBind(byte[] addr, int port)`: opens a UDP socket bound to `addr:port`. Returns the
/// handle (>= 0) or `SOCK_ERROR`.
pub fn socket_udp_bind(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(addr_array)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(port)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let addr = read_addr_bytes(vm, addr_array);
    let result = match vm.net_backend() {
        Some(backend) => backend.udp_bind(&addr, port as u16),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(handle) => handle as i32,
        _ => SOCK_ERROR,
    })))
}

/// `Socket.UdpSendTo(int handle, byte[] buffer, int offset, int count, byte[] addr, int port)`: sends
/// one datagram. Returns bytes sent (>= 0), `WOULD_BLOCK`, or `SOCK_ERROR`.
pub fn socket_udp_send_to(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Object(addr_array)) = args.get(4) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(port)) = args.get(5) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let mut buf = alloc::vec![0u8; count];
    for (i, slot) in buf.iter_mut().enumerate() {
        if let Some(Value::Int32(byte)) = vm.heap_mut().array_get(array, offset + i) {
            *slot = byte as u8;
        }
    }
    let addr = read_addr_bytes(vm, addr_array);
    let result = match vm.net_backend() {
        Some(backend) => backend.udp_send_to(handle, &buf, &addr, port as u16),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    Ok(Some(Value::Int32(match result {
        NetResult::Ready(n) => n as i32,
        NetResult::WouldBlock => {
            vm.request_block_on_io(handle, Interest::Write);
            WOULD_BLOCK
        }
        NetResult::Error => SOCK_ERROR,
    })))
}

/// `Socket.UdpReceiveFrom(int handle, byte[] buffer, int offset, int count, byte[] senderAddr,
/// int[] senderMeta)`: receives one datagram, writing the sender's address octets into `senderAddr`
/// and `[addrLen, port]` into `senderMeta`. Returns bytes received (>= 0; the datagram is truncated to
/// `count`), `WOULD_BLOCK`, or `SOCK_ERROR`.
pub fn socket_udp_recv_from(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Object(sender_addr_array)) = args.get(4) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Object(sender_meta_array)) = args.get(5) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let mut buf = alloc::vec![0u8; count];
    let mut sender = alloc::vec![0u8; 16];
    let result = match vm.net_backend() {
        Some(backend) => backend.udp_recv_from(handle, &mut buf, &mut sender),
        None => return Ok(Some(Value::Int32(SOCK_ERROR))),
    };
    match result {
        NetResult::Ready((n, addr_len, port)) => {
            for i in 0..n {
                vm.heap_mut()
                    .array_set(array, offset + i, Value::Int32(i32::from(buf[i])));
            }
            for i in 0..addr_len {
                vm.heap_mut()
                    .array_set(sender_addr_array, i, Value::Int32(i32::from(sender[i])));
            }
            vm.heap_mut()
                .array_set(sender_meta_array, 0, Value::Int32(addr_len as i32));
            vm.heap_mut()
                .array_set(sender_meta_array, 1, Value::Int32(i32::from(port)));
            Ok(Some(Value::Int32(n as i32)))
        }
        NetResult::WouldBlock => {
            vm.request_block_on_io(handle, Interest::Read);
            Ok(Some(Value::Int32(WOULD_BLOCK)))
        }
        NetResult::Error => Ok(Some(Value::Int32(SOCK_ERROR))),
    }
}

/// `Dns.ResolveHost(string host, byte[] buffer, int[] lengths)`: resolves `host` to its IP addresses,
/// writing address `i`'s network-order bytes at `buffer[i*16 ..]` and its byte length (4 = IPv4 /
/// 16 = IPv6) into `lengths[i]`, and returning the address count (>= 1) or -1 on failure.
/// `lengths.Length` caps the count (the managed side sizes both arrays at 16 bytes per slot). The
/// managed `Dns` builds an `IPAddress[]`, so BOTH families and multiple addresses surface.
///
/// # Errors
/// [`Trap::TypeMismatch`] if `host` is not a string or the buffers are not arrays.
pub fn dns_resolve_host(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let host = string_value(vm, args.first()).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    let Some(&Value::Object(buffer)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Object(lengths)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let addresses = match vm.net_backend() {
        Some(backend) => backend.resolve(&host),
        None => alloc::vec::Vec::new(),
    };
    if addresses.is_empty() {
        return Ok(Some(Value::Int32(-1)));
    }
    let capacity = vm.heap_mut().array_len(lengths).unwrap_or(0);
    let count = addresses.len().min(capacity);
    for (i, address) in addresses.iter().take(count).enumerate() {
        for (j, &octet) in address.iter().enumerate() {
            vm.heap_mut()
                .array_set(buffer, i * 16 + j, Value::Int32(i32::from(octet)));
        }
        vm.heap_mut()
            .array_set(lengths, i, Value::Int32(address.len() as i32));
    }
    Ok(Some(Value::Int32(count as i32)))
}


/// Returned by a TLS intrinsic when the operation failed or no backend is installed.
const TLS_ERROR: i32 = -1;
/// Returned by `tls_read_plain` when the peer has closed (no more plaintext).
const TLS_CLOSED: i32 = -2;

/// Reads an entire managed `byte[]` into a Rust vector for the seam.
fn read_whole_array(vm: &mut Vm, array: ObjectRef) -> alloc::vec::Vec<u8> {
    let len = vm.heap_mut().array_len(array).unwrap_or(0);
    let mut bytes = alloc::vec![0u8; len];
    for (i, slot) in bytes.iter_mut().enumerate() {
        if let Some(Value::Int32(byte)) = vm.heap_mut().array_get(array, i) {
            *slot = byte as u8;
        }
    }
    bytes
}

/// Reads a managed `byte[]` segment `[offset, offset+count)` into a Rust vector for the seam.
fn read_byte_segment(
    vm: &mut Vm,
    array: ObjectRef,
    offset: usize,
    count: usize,
) -> alloc::vec::Vec<u8> {
    let mut buf = alloc::vec![0u8; count];
    for (i, slot) in buf.iter_mut().enumerate() {
        if let Some(Value::Int32(byte)) = vm.heap_mut().array_get(array, offset + i) {
            *slot = byte as u8;
        }
    }
    buf
}

/// Writes `bytes` into a managed `byte[]` starting at `offset`.
fn write_byte_segment(vm: &mut Vm, array: ObjectRef, offset: usize, bytes: &[u8]) {
    for (i, &byte) in bytes.iter().enumerate() {
        vm.heap_mut()
            .array_set(array, offset + i, Value::Int32(i32::from(byte)));
    }
}

/// `TlsNative.ClientConfig(int stack, int verifyMode, byte[] rootsPem)`: builds a client config
/// (engine + trust policy; `rootsPem` may be null). Returns the config handle (>= 0) or `TLS_ERROR`.
pub fn tls_client_config(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(stack)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(verify)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let roots = match args.get(2) {
        Some(&Value::Object(array)) => Some(read_whole_array(vm, array)),
        _ => None,
    };
    let result = match vm.tls_backend() {
        Some(backend) => backend.client_config(
            TlsStack::from_i32(stack),
            VerifyMode::from_i32(verify),
            roots.as_deref(),
        ),
        None => return Ok(Some(Value::Int32(TLS_ERROR))),
    };
    Ok(Some(Value::Int32(result.map_or(TLS_ERROR, |h| h as i32))))
}

/// `TlsNative.ServerConfig(int stack, byte[] pfx, string password)`: builds a server config from a
/// PKCS#12 identity. Returns the config handle (>= 0) or `TLS_ERROR`.
pub fn tls_server_config(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(stack)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Object(pfx_array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let password = string_value(vm, args.get(2)).unwrap_or_default();
    let pfx = read_whole_array(vm, pfx_array);
    let result = match vm.tls_backend() {
        Some(backend) => backend.server_config(TlsStack::from_i32(stack), &pfx, &password),
        None => return Ok(Some(Value::Int32(TLS_ERROR))),
    };
    Ok(Some(Value::Int32(result.map_or(TLS_ERROR, |h| h as i32))))
}

/// `TlsNative.ClientNew(int config, string hostname)`: starts a client session (SNI = hostname).
/// Returns the session handle (>= 0) or `TLS_ERROR`.
pub fn tls_client_new(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(config)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let hostname = string_value(vm, args.get(1)).unwrap_or_default();
    let result = match vm.tls_backend() {
        Some(backend) => backend.client_new(config as u32, &hostname),
        None => return Ok(Some(Value::Int32(TLS_ERROR))),
    };
    Ok(Some(Value::Int32(result.map_or(TLS_ERROR, |h| h as i32))))
}

/// `TlsNative.ServerNew(int config)`: starts a server session. Returns the handle (>= 0) or `TLS_ERROR`.
pub fn tls_server_new(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Int32(config)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let result = match vm.tls_backend() {
        Some(backend) => backend.server_new(config as u32),
        None => return Ok(Some(Value::Int32(TLS_ERROR))),
    };
    Ok(Some(Value::Int32(result.map_or(TLS_ERROR, |h| h as i32))))
}

/// `TlsNative.Process(int tls)`: advances the session state machine. Returns the state
/// (`0` handshaking, `1` established, `2` closed, `3` error), or `TLS_ERROR` with no backend.
pub fn tls_process(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let state = match vm.tls_backend() {
        Some(backend) => backend.process(handle).as_i32(),
        None => TLS_ERROR,
    };
    Ok(Some(Value::Int32(state)))
}

/// `TlsNative.WantsWrite(int tls)`: `1` if outgoing ciphertext is queued, else `0`.
pub fn tls_wants_write(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let wants = match vm.tls_backend() {
        Some(backend) => backend.wants_write(handle),
        None => false,
    };
    Ok(Some(Value::Int32(i32::from(wants))))
}

/// `TlsNative.WriteTls(int tls, byte[] buf, int offset, int count)`: drains up to `count` bytes of
/// outgoing ciphertext into `buf` at `offset`. Returns the byte count written.
pub fn tls_write_tls(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let mut out = alloc::vec![0u8; count];
    let n = match vm.tls_backend() {
        Some(backend) => backend.write_tls(handle, &mut out),
        None => return Ok(Some(Value::Int32(0))),
    };
    write_byte_segment(vm, array, offset, &out[..n]);
    Ok(Some(Value::Int32(n as i32)))
}

/// `TlsNative.ReadTls(int tls, byte[] buf, int offset, int count)`: feeds `count` bytes of received
/// ciphertext from `buf` at `offset`. Returns how many bytes the engine consumed.
pub fn tls_read_tls(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let input = read_byte_segment(vm, array, offset, count);
    let n = match vm.tls_backend() {
        Some(backend) => backend.read_tls(handle, &input),
        None => return Ok(Some(Value::Int32(0))),
    };
    Ok(Some(Value::Int32(n as i32)))
}

/// `TlsNative.ReadPlain(int tls, byte[] buf, int offset, int count)`: reads up to `count` bytes of
/// decrypted application data into `buf` at `offset`. Returns the byte count (`0` = none yet),
/// `TLS_CLOSED` when the peer closed, or `TLS_ERROR` with no backend.
pub fn tls_read_plain(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let mut out = alloc::vec![0u8; count];
    let result = match vm.tls_backend() {
        Some(backend) => backend.read_plain(handle, &mut out),
        None => return Ok(Some(Value::Int32(TLS_ERROR))),
    };
    match result {
        Some(n) => {
            write_byte_segment(vm, array, offset, &out[..n]);
            Ok(Some(Value::Int32(n as i32)))
        }
        None => Ok(Some(Value::Int32(TLS_CLOSED))),
    }
}

/// `TlsNative.WritePlain(int tls, byte[] buf, int offset, int count)`: queues `count` bytes of
/// application data to encrypt. Returns how many bytes were accepted.
pub fn tls_write_plain(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(offset)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let Some(&Value::Int32(count)) = args.get(3) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let (offset, count) = (offset.max(0) as usize, count.max(0) as usize);
    let input = read_byte_segment(vm, array, offset, count);
    let n = match vm.tls_backend() {
        Some(backend) => backend.write_plain(handle, &input),
        None => return Ok(Some(Value::Int32(0))),
    };
    Ok(Some(Value::Int32(n as i32)))
}

/// `TlsNative.PeerCert(int tls, byte[] buf)`: writes the peer's end-entity certificate (DER) into
/// `buf` when it fits, returning its full DER length (`0` = none). A caller probes with a large
/// buffer; if the return exceeds it, re-call with a bigger one (the engine wrote nothing).
pub fn tls_peer_cert(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    let Some(&Value::Object(array)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let cap = vm.heap_mut().array_len(array).unwrap_or(0);
    let mut out = alloc::vec![0u8; cap];
    let der_len = match vm.tls_backend() {
        Some(backend) => backend.peer_cert(handle, &mut out),
        None => return Ok(Some(Value::Int32(0))),
    };
    if der_len > 0 && der_len <= cap {
        write_byte_segment(vm, array, 0, &out[..der_len]);
    }
    Ok(Some(Value::Int32(der_len as i32)))
}

/// `TlsNative.CloseTls(int tls)`: closes the session and releases its handle.
pub fn tls_close(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let handle = socket_arg(args, 0)?;
    if let Some(backend) = vm.tls_backend() {
        backend.close(handle);
    }
    Ok(None)
}

/// `TlsNative.DefaultStack()`: the TLS stack the managed `SslStream` selects when a program does not
/// request one (`0` = rustls, `1` = mbedTLS) -- the host's runtime choice. `0` with no backend.
pub fn tls_default_stack(
    vm: &mut Vm,
    _module: &Module,
    _args: &[Value],
) -> Result<Option<Value>, Trap> {
    let stack = match vm.tls_backend() {
        Some(backend) => backend.default_stack(),
        None => 0,
    };
    Ok(Some(Value::Int32(stack)))
}

/// `System.Threading.Monitor.EnterLock(object)`: acquires the per-object lock for the running
/// thread. If the object is free or already owned by this thread (a recursive `lock`) the thread
/// holds it and proceeds into the critical section. On contention the thread is queued in the lock's
/// waiters and asked to BLOCK ([`Vm::request_block_on_lock`]); it resumes here -- already holding the
/// lock -- only once the owner releases and hands it over, so it never re-tries.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an object reference.
pub fn monitor_enter(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(obj)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let thread = vm.current_thread_id();
    if !vm.lock_acquire(obj.0, thread) {
        vm.request_block_on_lock(None);
    }
    Ok(None)
}

/// `System.Threading.Monitor.ExitLock(object)`: releases one level of the running thread's lock on
/// the object. When the outermost level is released and a thread is queued, the lock is handed to
/// the first waiter, which is woken ([`Vm::request_wake`]).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an object reference.
pub fn monitor_exit(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(obj)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let thread = vm.current_thread_id();
    if let Some(woken) = vm.lock_release(obj.0, thread) {
        vm.request_wake(woken);
    }
    Ok(None)
}

/// `System.Threading.Monitor.TryEnterLock(object)`: tries to acquire the per-object lock WITHOUT
/// blocking, returning `true` (an `int32` 1) if it is now held by this thread (the object was free
/// or already owned by it) or `false` (0) if another thread owns it. Backs `Monitor.TryEnter`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an object reference.
pub fn monitor_try_enter(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(obj)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let thread = vm.current_thread_id();
    Ok(Some(Value::Int32(i32::from(vm.lock_try(obj.0, thread)))))
}

/// `System.Threading.Monitor.WaitLock(object)`: the running thread (which MUST own the lock)
/// FULLY releases it, parks in the object's condition wait-set, and blocks until a `Pulse`/`PulseAll`
/// moves it to the acquire-queue and a later release hands it the lock -- at which point it resumes
/// here holding the lock again at its original recursion depth. Backs `Monitor.Wait`. If the release
/// handed the lock to a contender, that thread is woken in the same step.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an object reference; [`Trap::SynchronizationLock`]
/// if the running thread does not own the lock (the `SynchronizationLockException` site).
pub fn monitor_wait(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(obj)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let thread = vm.current_thread_id();
    if !vm.lock_is_owner(obj.0, thread) {
        return Err(Trap::SynchronizationLock);
    }
    let woken = vm.lock_wait(obj.0, thread);
    vm.request_block_on_lock(woken);
    Ok(None)
}

/// `System.Threading.Monitor.PulseLock(object)`: moves ONE thread blocked in `Monitor.Wait` on the
/// object into the lock's acquire-queue (it is handed the lock when the running thread later
/// releases it). The running thread must own the lock; a no-op if none are waiting. Backs
/// `Monitor.Pulse`.
///
/// # Errors
/// [`Trap::TypeMismatch`] for a non-object argument; [`Trap::SynchronizationLock`] if the running
/// thread does not own the lock.
pub fn monitor_pulse(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(obj)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let thread = vm.current_thread_id();
    if !vm.lock_is_owner(obj.0, thread) {
        return Err(Trap::SynchronizationLock);
    }
    vm.lock_pulse(obj.0, true);
    Ok(None)
}

/// `System.Threading.Monitor.PulseAllLock(object)`: like [`monitor_pulse`] but moves ALL threads
/// blocked in `Monitor.Wait` on the object into the acquire-queue. Backs `Monitor.PulseAll`.
///
/// # Errors
/// [`Trap::TypeMismatch`] for a non-object argument; [`Trap::SynchronizationLock`] if the running
/// thread does not own the lock.
pub fn monitor_pulse_all(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(obj)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let thread = vm.current_thread_id();
    if !vm.lock_is_owner(obj.0, thread) {
        return Err(Trap::SynchronizationLock);
    }
    vm.lock_pulse(obj.0, false);
    Ok(None)
}

/// `System.GC.WaitForPendingFinalizers()`: a no-op -- finalizers run inline during the
/// collection, so there is nothing to wait for. Present only with the `finalizers` feature.
///
/// # Errors
/// Never errors.
#[cfg(feature = "finalizers")]
pub fn wait_for_pending_finalizers(
    _vm: &mut Vm,
    _module: &Module,
    _args: &[Value],
) -> Result<Option<Value>, Trap> {
    Ok(None)
}

/// `System.Type.GetTypeFromHandle(RuntimeTypeHandle)`: a type handle IS the Type in this
/// runtime (the type's token); identity.
///
/// # Errors
/// Never errors.
pub fn type_from_handle(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    Ok(Some(args.first().cloned().unwrap_or(Value::Null)))
}

/// `System.Object.GetType()`: the receiver's runtime `Type`, modeled (like `typeof`) as the
/// type's asm-folded token so a following `.Name` resolves through the same path. csc lowers
/// `i.GetType()` on a value type by `box`-ing the receiver first, so the `this` here is a
/// boxed value whose tag already carries the value type's token (Int32, Boolean, ...); a
/// heap string yields `System.String`'s handle, and a reference instance its own type's.
///
/// # Errors
/// [`Trap::NullReference`] on a null receiver; [`Trap::TypeMismatch`] if the receiver is not
/// an object or its type has no recorded handle.
pub fn object_get_type(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let reference = match args.first() {
        Some(Value::Object(reference)) => *reference,
        Some(Value::Null) => return Err(Trap::NullReference),
        _ => return Err(Trap::TypeMismatch(Opcode::Callvirt)),
    };
    let handle = vm
        .heap()
        .boxed_type_token(reference)
        .or_else(|| {
            vm.heap()
                .type_of(reference)
                .and_then(|type_id| module.type_handle_of(type_id))
        })
        .or_else(|| {
            vm.heap()
                .is_string(reference)
                .then(|| module.string_type_id().and_then(|id| module.type_handle_of(id)))
                .flatten()
        })
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    Ok(Some(Value::NativeInt(handle as i64)))
}

/// `System.Enum.Parse(Type, string)`: the enum constant named by the string, boxed.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the name is not a constant of the enum.
pub fn enum_parse(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let token = type_handle_token(args.first());
    let ignore_case = matches!(args.get(2), Some(&Value::Int32(flag)) if flag != 0);
    let value = string_value(vm, args.get(1))
        .and_then(|raw| {
            let name = raw.trim();
            module
                .enum_value_by_name_handle(token, name, ignore_case)
                .or_else(|| name.parse::<i64>().ok())
        })
        .ok_or(Trap::InvalidArgument)?;
    let boxed_value = if module.enum_is_wide_by_handle(token) {
        Value::Int64(value)
    } else {
        Value::Int32(value as i32)
    };
    let boxed = vm.heap_mut().alloc_boxed(token, boxed_value);
    Ok(Some(Value::Object(boxed)))
}

/// `System.Enum.IsDefined(Type, object)`: whether `object` (a constant name, or an
/// underlying value) is defined in the enum.
///
/// # Errors
/// Never errors.
pub fn enum_is_defined(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let token = type_handle_token(args.first());
    let defined = match args.get(1) {
        Some(Value::Object(reference)) => match vm.heap().get(*reference) {
            Some(Object::Str(chars)) => module
                .enum_value_by_name_handle(
                    token,
                    &String::from_utf16_lossy(&decode_string(chars)),
                    false,
                )
                .is_some(),
            Some(Object::Boxed { value, .. }) => enum_underlying(value)
                .is_some_and(|n| module.enum_value_name_by_handle(token, n).is_some()),
            _ => false,
        },
        Some(other) => enum_underlying(other)
            .is_some_and(|n| module.enum_value_name_by_handle(token, n).is_some()),
        None => false,
    };
    Ok(Some(Value::Int32(i32::from(defined))))
}

/// The underlying integer carried by an `Enum` argument -- a boxed enum value (the common
/// `(object)value` / value passed where an `object` is expected) or a bare numeric value.
fn enum_arg_value(vm: &Vm, arg: Option<&Value>) -> Option<i64> {
    match arg {
        Some(Value::Object(reference)) => match vm.heap().get(*reference) {
            Some(Object::Boxed { value, .. }) => enum_underlying(value),
            _ => None,
        },
        other => enum_underlying(other?),
    }
}

/// `System.Enum.GetName(Type, object)`: the name of the member whose underlying value equals
/// `object`, or null. Not flags-aware -- only an exactly-named member yields a name (matching
/// .NET's `GetName`, which never decomposes).
///
/// # Errors
/// Never errors (an unrecognized value or type yields null).
pub fn enum_get_name(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let token = type_handle_token(args.first());
    let name = enum_arg_value(vm, args.get(1))
        .and_then(|value| module.enum_value_name_resolved(token, value));
    match name {
        Some(name) => {
            let chars: Vec<u16> = name.encode_utf16().collect();
            Ok(Some(Value::Object(vm.heap_mut().alloc_string(&chars))))
        }
        None => Ok(Some(Value::Null)),
    }
}

/// `System.Enum.GetNames(Type)`: a `string[]` of the member names, ascending by underlying
/// value (the order .NET reports).
///
/// # Errors
/// Never errors (an unknown type yields an empty array).
pub fn enum_get_names(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let token = type_handle_token(args.first());
    let members = module.enum_members_by_handle(token).unwrap_or_default();
    let mut elements: Vec<Value> = Vec::with_capacity(members.len());
    for (_, name) in members {
        let chars: Vec<u16> = name.encode_utf16().collect();
        elements.push(Value::Object(vm.heap_mut().alloc_string(&chars)));
    }
    let array = vm.heap_mut().alloc_array(elements);
    Ok(Some(Value::Object(array)))
}

/// `System.Enum.GetValues(Type)`: an array of the enum's underlying values, ascending by value
/// (the order .NET reports). Each element is a boxed enum value tagged with the enum's type, so
/// an `Enum.GetValues(t)` element unboxes to its declared enum type (and casts to its underlying
/// integer) -- matching the `Array` of the enum type .NET returns.
///
/// # Errors
/// Never errors (an unknown type yields an empty array).
pub fn enum_get_values(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let token = type_handle_token(args.first());
    let wide = module.enum_is_wide_by_handle(token);
    let members = module.enum_members_by_handle(token).unwrap_or_default();
    let mut elements: Vec<Value> = Vec::with_capacity(members.len());
    for (value, _) in members {
        let boxed_value = if wide {
            Value::Int64(value)
        } else {
            Value::Int32(value as i32)
        };
        let boxed = vm.heap_mut().alloc_boxed(token, boxed_value);
        elements.push(Value::Object(boxed));
    }
    let array = vm.heap_mut().alloc_array(elements);
    Ok(Some(Value::Object(array)))
}

/// `System.Enum.Format(Type, object, string)`: renders the enum value per the format string
/// ("G"/"D"/"X"/"F"), matching .NET byte-for-byte. "G" = the member name (flags-decomposed for a
/// `[Flags]` enum) or the decimal number; "D" = the decimal underlying value; "X" = the
/// underlying value in UPPERCASE hex, zero-padded to the underlying width; "F" = flags-style
/// decomposition regardless of `[Flags]`, else the number.
///
/// # Errors
/// Never errors (an unrecognized format renders as "G"; an unknown value falls back to the number).
pub fn enum_format(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let token = type_handle_token(args.first());
    let value = enum_arg_value(vm, args.get(1)).unwrap_or(0);
    let format = string_value(vm, args.get(2)).unwrap_or_default();
    let text = format_enum(module, token, value, &format);
    let chars: Vec<u16> = text.encode_utf16().collect();
    Ok(Some(Value::Object(vm.heap_mut().alloc_string(&chars))))
}

/// Renders `value` of the enum named by `token` per a single-letter `format` ("G"/"D"/"X"/"F",
/// case-insensitive), defaulting to "G" for anything else -- the shared engine behind
/// `Enum.Format` and `Enum.ToString(string)`.
fn format_enum(module: &Module, token: u64, value: i64, format: &str) -> String {
    let kind = format.chars().next().unwrap_or('G').to_ascii_uppercase();
    match kind {
        'D' => value.to_string(),
        'X' => format_enum_hex(module.enum_width_by_handle(token), value),
        'F' => module
            .enum_name_or_flags(token, value, true)
            .unwrap_or_else(|| value.to_string()),
        _ => module
            .enum_name_or_flags(token, value, false)
            .unwrap_or_else(|| value.to_string()),
    }
}

/// The "X" enum format: `value`'s low `width` bytes as UPPERCASE hex, zero-padded to `width * 2`
/// digits (.NET pads to the underlying type's width and uppercases even for the lowercase "x").
fn format_enum_hex(width: u8, value: i64) -> String {
    let digits = (width as usize) * 2;
    let mask = if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (width as u32 * 8)) - 1
    };
    let bits = (value as u64) & mask;
    alloc::format!("{bits:0digits$X}")
}

/// The asm-folded type handle a `RuntimeTypeHandle` / `Type` argument carries (it is modeled as
/// a native-int holding the folded token: the assembly in the high 32 bits, the token in the
/// low 32). Read back bit-for-bit as a u64 so the asm id survives.
fn type_handle_token(arg: Option<&Value>) -> u64 {
    match arg {
        Some(Value::NativeInt(handle)) => *handle as u64,
        _ => 0,
    }
}

/// `System.Array.Empty<T>()`: a fresh empty array -- what `params T[]` lowers a no-argument
/// call to (a zero-length array literal). The element type is irrelevant for an empty array.
///
/// # Errors
/// Never errors.
pub fn array_empty(vm: &mut Vm, _module: &Module, _args: &[Value]) -> Result<Option<Value>, Trap> {
    let array = vm.heap_mut().alloc_array(Vec::new());
    Ok(Some(Value::Object(array)))
}

/// `System.Type.get_Name` (the `Name` property): the type's simple (unqualified) name --
/// "Int32", "String", "Program". The receiver `this` is the `Type`, modeled as the type's
/// asm-folded token; the loader recorded each `typeof`'d type's name under that key.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
pub fn type_get_name(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let name = module
        .type_name_by_handle(handle as u64)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    Ok(Some(alloc_str(vm, name)))
}

/// The reflection metadata the loader recorded for the receiver `Type` (the first argument, a
/// native int holding the type's asm-folded token). Shared by the `System.Type` introspection
/// intrinsics below.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle or names no recorded type.
#[cfg(feature = "NETMFv4_4")]
fn reflect_type_of<'m>(
    module: &'m Module,
    args: &[Value],
) -> Result<&'m crate::module::ReflectType, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    module
        .reflect_type(handle as u64)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))
}

/// Reads a boolean kind bit from the receiver `Type`'s recorded reflection metadata (a C# `bool`
/// is a 0/1 `int32` on the stack).
#[cfg(feature = "NETMFv4_4")]
fn type_kind_bit(
    module: &Module,
    args: &[Value],
    pick: impl Fn(&crate::module::ReflectType) -> bool,
) -> Result<Option<Value>, Trap> {
    let info = reflect_type_of(module, args)?;
    Ok(Some(Value::Int32(i32::from(pick(info)))))
}

/// `System.Type.get_FullName` (`Type.FullName`): the type's `namespace.name`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_full_name(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let full = reflect_type_of(module, args)?.full_name.clone();
    Ok(Some(alloc_str(vm, &full)))
}

/// `System.Type.get_Namespace` (`Type.Namespace`): the type's namespace, or null for a type in
/// the global namespace (matching .NET).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_namespace(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let info = reflect_type_of(module, args)?;
    if info.namespace.is_empty() {
        Ok(Some(Value::Null))
    } else {
        let namespace = info.namespace.clone();
        Ok(Some(alloc_str(vm, &namespace)))
    }
}

/// `System.Type.get_IsEnum` (`Type.IsEnum`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_enum(_vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| info.is_enum)
}

/// `System.Type.get_IsValueType` (`Type.IsValueType`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_value_type(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| info.is_value_type)
}

/// `System.Type.get_IsClass` (`Type.IsClass`): neither an interface nor a value type (.NET's rule).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_class(_vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| !info.is_interface && !info.is_value_type)
}

/// `System.Type.get_IsInterface` (`Type.IsInterface`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_interface(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| info.is_interface)
}

/// `System.Type.get_IsAbstract` (`Type.IsAbstract`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_abstract(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| info.is_abstract)
}

/// `System.Type.get_IsPublic` (`Type.IsPublic`): public top-level visibility.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_public(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| info.is_public)
}

/// `System.Type.get_IsNotPublic` (`Type.IsNotPublic`): the complement of `IsPublic` for a
/// top-level type.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_not_public(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    type_kind_bit(module, args, |info| !info.is_public)
}

/// `System.Type.get_IsArray` (`Type.IsArray`): a `TypeDef` handle never names an array type
/// (array types are `TypeSpec`s), so false.
///
/// # Errors
/// Never errors.
#[cfg(feature = "NETMFv4_4")]
pub fn type_is_array(
    _vm: &mut Vm,
    _module: &Module,
    _args: &[Value],
) -> Result<Option<Value>, Trap> {
    Ok(Some(Value::Int32(0)))
}

/// `System.Type.get_Assembly` (`Type.Assembly`): the declaring assembly, modeled as the
/// asm-folded handle with a zero token (the assembly/module itself). The handle's high 32 bits
/// carry the assembly id the type was folded with.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_assembly(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let assembly_handle = (handle as u64) & 0xFFFF_FFFF_0000_0000;
    Ok(Some(Value::NativeInt(assembly_handle as i64)))
}

/// `System.Type.GetFields(BindingFlags)` (and the parameterless overload): the type's fields that
/// match the binding flags, as a `FieldInfo[]` whose elements are the field handles. Only DECLARED
/// fields are modeled (the loader records each type's own fields), so a caller wanting .NET parity
/// on a derived type passes `BindingFlags.DeclaredOnly` (a class extending only `Object` needs
/// nothing, having no inherited fields).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_fields(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let flags = match args.get(1) {
        Some(&Value::Int32(value)) => value,
        _ => 0x04 | 0x08 | 0x10,
    };
    let elements: Vec<Value> = module
        .type_fields(handle as u64)
        .iter()
        .filter(|field| binding_flags_match(flags, field.is_static, field.is_public))
        .map(|field| Value::NativeInt(field.handle as i64))
        .collect();
    let array = vm.heap_mut().alloc_array(elements);
    Ok(Some(Value::Object(array)))
}

/// Whether a member with the given static / visibility bits matches the `BindingFlags` value
/// (an int-backed enum). Shared by `GetFields` / `GetMethods`. The parameterless overloads default
/// to `Public | Instance | Static`.
#[cfg(feature = "NETMFv4_4")]
fn binding_flags_match(flags: i32, is_static: bool, is_public: bool) -> bool {
    const INSTANCE: i32 = 0x04;
    const STATIC: i32 = 0x08;
    const PUBLIC: i32 = 0x10;
    const NON_PUBLIC: i32 = 0x20;
    let scope = if is_static {
        flags & STATIC != 0
    } else {
        flags & INSTANCE != 0
    };
    let visibility = if is_public {
        flags & PUBLIC != 0
    } else {
        flags & NON_PUBLIC != 0
    };
    scope && visibility
}

/// `System.Type.GetMethods(BindingFlags)` (and the parameterless overload): the type's methods
/// (constructors excluded, matching .NET) that match the binding flags, as a `MethodInfo[]` of
/// method handles. Only DECLARED methods are modeled, so a caller wanting .NET parity on a derived
/// type passes `BindingFlags.DeclaredOnly`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_methods(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let flags = match args.get(1) {
        Some(&Value::Int32(value)) => value,
        _ => 0x04 | 0x08 | 0x10,
    };
    let elements: Vec<Value> = module
        .type_methods(handle as u64)
        .iter()
        .filter(|method| binding_flags_match(flags, method.is_static, method.is_public))
        .map(|method| Value::NativeInt(method.handle as i64))
        .collect();
    let array = vm.heap_mut().alloc_array(elements);
    Ok(Some(Value::Object(array)))
}

/// `System.Reflection.FieldInfo.get_FieldType` (`FieldInfo.FieldType`) and
/// `System.Reflection.MethodInfo.get_ReturnType` (`MethodInfo.ReturnType`): the member's type as a
/// `Type` (its asm-folded handle). The loader recorded each member's type at load. Null when the
/// type was not resolvable as a `Type` handle (an array / pointer / unresolved cross-assembly type
/// -- not modeled yet); the common primitive / string / object / same-assembly class+struct cases
/// resolve.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a member handle.
#[cfg(feature = "NETMFv4_4")]
pub fn member_get_type(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    Ok(Some(match module.member_type(handle as u64) {
        Some(type_handle) => Value::NativeInt(type_handle as i64),
        None => Value::Null,
    }))
}

/// `System.Type.get_BaseType` (`Type.BaseType`): the type's base class as a `Type`, or null for an
/// interface or `System.Object`.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a recorded type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_base_type(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let info = reflect_type_of(module, args)?;
    Ok(Some(if info.base_handle == 0 {
        Value::Null
    } else {
        Value::NativeInt(info.base_handle as i64)
    }))
}

/// Reads a `MethodBase.Is*` predicate from the receiver method's recorded `MethodAttributes`.
#[cfg(feature = "NETMFv4_4")]
fn method_attr_bit(
    module: &Module,
    args: &[Value],
    pick: impl Fn(u32) -> bool,
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let attrs = module.method_attrs(handle as u64).unwrap_or(0);
    Ok(Some(Value::Int32(i32::from(pick(attrs)))))
}

/// `System.Reflection.MethodBase.get_IsPublic` (`MethodAttributes` access == `Public`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a method handle.
#[cfg(feature = "NETMFv4_4")]
pub fn method_is_public(_vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    method_attr_bit(module, args, |attrs| attrs & 0x0007 == 0x0006)
}

/// `System.Reflection.MethodBase.get_IsStatic` (`MethodAttributes.Static`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a method handle.
#[cfg(feature = "NETMFv4_4")]
pub fn method_is_static(_vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    method_attr_bit(module, args, |attrs| attrs & 0x0010 != 0)
}

/// `System.Reflection.MethodBase.get_IsFinal` (`MethodAttributes.Final`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a method handle.
#[cfg(feature = "NETMFv4_4")]
pub fn method_is_final(_vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    method_attr_bit(module, args, |attrs| attrs & 0x0020 != 0)
}

/// `System.Reflection.MethodBase.get_IsVirtual` (`MethodAttributes.Virtual`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a method handle.
#[cfg(feature = "NETMFv4_4")]
pub fn method_is_virtual(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    method_attr_bit(module, args, |attrs| attrs & 0x0040 != 0)
}

/// `System.Reflection.MethodBase.get_IsAbstract` (`MethodAttributes.Abstract`).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a method handle.
#[cfg(feature = "NETMFv4_4")]
pub fn method_is_abstract(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    method_attr_bit(module, args, |attrs| attrs & 0x0400 != 0)
}

/// `System.Reflection.Assembly.GetType(string)`: the type named `name` (a full `namespace.name`)
/// as a `Type` handle, or null if no such type is recorded. The assembly receiver is not scoped --
/// the name resolves across the loaded assemblies (corlib first) -- which suffices for the single
/// program + corlib model.
///
/// # Errors
/// Never errors (a missing type or non-string argument yields null).
#[cfg(feature = "NETMFv4_4")]
pub fn assembly_get_type(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(name) = string_value(vm, args.get(1)) else {
        return Ok(Some(Value::Null));
    };
    Ok(Some(match module.type_handle_by_name(&name) {
        Some(handle) => Value::NativeInt(handle as i64),
        None => Value::Null,
    }))
}

/// The asm-folded handle a `Type` / `Assembly` reference argument carries (a native int), or 0 for
/// null. Reflection references are token-only handles, so reference identity is handle equality.
#[cfg(feature = "NETMFv4_4")]
fn reflect_handle(arg: Option<&Value>) -> i64 {
    match arg {
        Some(&Value::NativeInt(handle)) => handle,
        _ => 0,
    }
}

/// Canonicalizes a reflection handle for `==`/`!=`. A `Type` reference can arrive via different
/// tokens for the SAME type (a `TypeRef` vs the defining `TypeDef`, across assemblies -- e.g. a
/// program's `typeof(int)` vs the corlib `Int32` a field's `FieldType` / a boxed value's `GetType`
/// resolves to). Resolving each handle to the type's defining-`TypeDef` handle makes `Type ==`
/// exact type identity, matching .NET's reference equality. A non-type handle (a member's `Field`/
/// `MethodDef` token, an `Assembly`, an untracked array/pointer `TypeSpec`) has no `TypeId` and is
/// compared by its own token -- which is exactly the member/assembly identity reflection wants.
#[cfg(feature = "NETMFv4_4")]
fn canonical_reflect_handle(module: &Module, handle: i64) -> i64 {
    module
        .type_id_by_handle(handle as u64)
        .and_then(|type_id| module.type_handle_of(type_id))
        .map_or(handle, |canonical| canonical as i64)
}

/// `System.Type.op_Equality` / `System.Reflection.{MemberInfo,FieldInfo,MethodInfo,Assembly}
/// .op_Equality` (.NET overloads `==`/`!=` on all of them): reference identity over the canonical
/// handle.
///
/// # Errors
/// Never errors.
#[cfg(feature = "NETMFv4_4")]
pub fn reflect_handle_equals(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let left = canonical_reflect_handle(module, reflect_handle(args.first()));
    let right = canonical_reflect_handle(module, reflect_handle(args.get(1)));
    Ok(Some(Value::Int32(i32::from(left == right))))
}

/// `op_Inequality` for the reflection references (the complement of [`reflect_handle_equals`]).
///
/// # Errors
/// Never errors.
#[cfg(feature = "NETMFv4_4")]
pub fn reflect_handle_not_equals(
    _vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let left = canonical_reflect_handle(module, reflect_handle(args.first()));
    let right = canonical_reflect_handle(module, reflect_handle(args.get(1)));
    Ok(Some(Value::Int32(i32::from(left != right))))
}

/// `System.Reflection.FieldInfo.GetValue(object obj)`: the value of this field on `obj` (or the
/// static slot, ignoring `obj`), boxed to `object`. A reference-typed field returns its reference
/// directly; a primitive value type is boxed by its runtime type token. (Boxing a `bool`/`char`/
/// enum field by its DECLARED type needs recorded field types -- a follow-up; the runtime-value
/// boxing here is exact for `int`/`long`/`nint`/`float`/reference fields.)
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a field handle; [`Trap::NullReference`] for a
/// null target on an instance field.
#[cfg(feature = "NETMFv4_4")]
pub fn field_get_value(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let handle = handle as u64;
    let value = if let Some(slot) = module.field_slot_by_handle(handle) {
        let target = match args.get(1) {
            Some(Value::Object(reference)) => *reference,
            _ => return Err(Trap::NullReference),
        };
        vm.heap()
            .instance_field(target, slot)
            .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?
    } else if let Some(slot) = module.static_field_slot_by_handle(handle) {
        vm.static_field(slot)
            .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?
    } else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let boxed = match module.primitive_type_token(&value) {
        Some(token) => Value::Object(vm.heap_mut().alloc_boxed(token, value)),
        None => value,
    };
    Ok(Some(boxed))
}

/// `System.Reflection.FieldInfo.SetValue(object obj, object value)`: stores `value` into this field
/// on `obj` (or the static slot). A boxed primitive is unboxed to its underlying value; a reference
/// (or null) is stored as-is. Returns void.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a field handle; [`Trap::NullReference`] for a
/// null target on an instance field.
#[cfg(feature = "NETMFv4_4")]
pub fn field_set_value(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let handle = handle as u64;
    let incoming = match args.get(2) {
        Some(&Value::Object(reference)) => vm
            .heap()
            .boxed_value(reference)
            .unwrap_or(Value::Object(reference)),
        Some(value) => value.clone(),
        None => Value::Null,
    };
    if let Some(slot) = module.field_slot_by_handle(handle) {
        let target = match args.get(1) {
            Some(Value::Object(reference)) => *reference,
            _ => return Err(Trap::NullReference),
        };
        vm.heap_mut().set_instance_field(target, slot, incoming);
    } else if let Some(slot) = module.static_field_slot_by_handle(handle) {
        vm.set_static_field(slot, incoming);
    } else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    }
    Ok(None)
}

/// Unboxes a reflection argument or result: a boxed value type yields its underlying value; a real
/// reference (or null) passes through unchanged.
#[cfg(feature = "NETMFv4_4")]
fn unbox_reflect_arg(vm: &Vm, value: Value) -> Value {
    match value {
        Value::Object(reference) => vm
            .heap()
            .boxed_value(reference)
            .unwrap_or(Value::Object(reference)),
        other => other,
    }
}

/// `System.Reflection.MethodBase.Invoke(object obj, object[] parameters)`: invokes the method on
/// `obj` (null for a static method) with the boxed `parameters`, returning the result boxed to
/// `object` (null for a void method). Instance vs static is inferred from the method's arg count
/// (an instance method's count includes `this`). A primitive value type is unboxed on the way in
/// and boxed by its runtime type on the way out; references pass through.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a method handle; propagates any [`Trap`] from
/// running the invoked method.
#[cfg(feature = "NETMFv4_4")]
pub fn method_invoke(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let method_id = module
        .resolve_by_handle(handle as u64)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    let arg_count = module
        .method(method_id)
        .map(|method| method.arg_count() as usize)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    let mut params: Vec<Value> = Vec::new();
    if let Some(&Value::Object(array)) = args.get(2) {
        let len = vm.heap().array_len(array).unwrap_or(0);
        for index in 0..len {
            let element = vm.heap().array_get(array, index).unwrap_or(Value::Null);
            params.push(unbox_reflect_arg(vm, element));
        }
    }
    let mut full_args = Vec::with_capacity(arg_count);
    if arg_count == params.len() + 1 {
        full_args.push(args.get(1).cloned().unwrap_or(Value::Null));
    }
    full_args.extend(params);
    let result = Session::new(module, method_id, full_args)?.run(module, vm)?;
    let boxed = match result {
        Some(value) => match module.primitive_type_token(&value) {
            Some(token) => Value::Object(vm.heap_mut().alloc_boxed(token, value)),
            None => value,
        },
        None => Value::Null,
    };
    Ok(Some(boxed))
}

/// `System.Activator.CreateInstance(Type type)`: allocates an instance of `type` (fields
/// zero-initialized) and runs its parameterless constructor, returning the new object -- the
/// reflection analogue of `newobj` of a default constructor. Collection is suspended across the
/// allocation + constructor run so the fresh instance (held only in a Rust local) is not relocated.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not a type handle with a recorded layout; propagates a
/// [`Trap`] from running the constructor.
#[cfg(feature = "NETMFv4_4")]
pub fn activator_create_instance(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let handle = handle as u64;
    let type_id = module
        .type_id_by_handle(handle)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    let defaults = module
        .type_field_defaults(type_id)
        .map(|fields| fields.to_vec())
        .unwrap_or_default();
    #[cfg(feature = "gc")]
    vm.suspend_collection();
    let instance = vm.heap_mut().alloc_instance(type_id, defaults);
    let outcome = match module.type_ctor(handle) {
        Some(ctor) => {
            let mut ctor_args = Vec::with_capacity(1);
            ctor_args.push(Value::Object(instance));
            Session::new(module, ctor, ctor_args)
                .and_then(|mut session| session.run(module, vm))
                .map(|_| ())
        }
        None => Ok(()),
    };
    #[cfg(feature = "gc")]
    vm.resume_collection();
    outcome.map(|()| Some(Value::Object(instance)))
}

/// `System.Type.GetConstructor(Type[])`: the instance constructor whose parameter count matches the
/// length of the `Type[]`, as a `ConstructorInfo` handle, or null. Matched by ARITY (a type rarely
/// has two constructors of the same arity; exact per-parameter type matching is deferred -- it would
/// need cross-assembly type identity).
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
#[cfg(feature = "NETMFv4_4")]
pub fn type_get_constructor(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let arity = match args.get(1) {
        Some(&Value::Object(array)) => vm.heap().array_len(array).unwrap_or(0),
        _ => 0,
    };
    let ctor = module
        .type_ctors_list(handle as u64)
        .iter()
        .find(|(_, count)| *count == arity)
        .map(|(ctor_handle, _)| *ctor_handle);
    Ok(Some(match ctor {
        Some(ctor_handle) => Value::NativeInt(ctor_handle as i64),
        None => Value::Null,
    }))
}

/// `System.Reflection.ConstructorInfo.Invoke(object[])`: allocates a new instance of the
/// constructor's declaring type and runs the constructor with the (unboxed) parameters, returning
/// the new object. Like `Activator.CreateInstance` but for a specific constructor with arguments.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a constructor handle with a recorded layout;
/// propagates a [`Trap`] from running the constructor.
#[cfg(feature = "NETMFv4_4")]
pub fn constructor_invoke(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let method_id = module
        .resolve_by_handle(handle as u64)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    let type_id = module
        .method_type(method_id)
        .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?;
    let defaults = module
        .type_field_defaults(type_id)
        .map(|fields| fields.to_vec())
        .unwrap_or_default();
    let mut params: Vec<Value> = Vec::new();
    if let Some(&Value::Object(array)) = args.get(1) {
        let len = vm.heap().array_len(array).unwrap_or(0);
        for index in 0..len {
            let element = vm.heap().array_get(array, index).unwrap_or(Value::Null);
            params.push(unbox_reflect_arg(vm, element));
        }
    }
    #[cfg(feature = "gc")]
    vm.suspend_collection();
    let instance = vm.heap_mut().alloc_instance(type_id, defaults);
    let mut ctor_args = Vec::with_capacity(params.len() + 1);
    ctor_args.push(Value::Object(instance));
    ctor_args.extend(params);
    let outcome = Session::new(module, method_id, ctor_args)
        .and_then(|mut session| session.run(module, vm))
        .map(|_| ());
    #[cfg(feature = "gc")]
    vm.resume_collection();
    outcome.map(|()| Some(Value::Object(instance)))
}

/// Materializes a decoded custom-attribute argument ([`AttrValue`]) into a runtime [`Value`]:
/// an integer at its width, a heap string, a `Type` handle (the asm-folded token in a native
/// int, the representation `typeof` yields), or null. The float arms are present only with the
/// `float` feature; an `R4`/`R8` argument on a no-float build materializes as null (no corpus
/// uses one).
fn materialize_attr_value(vm: &mut Vm, value: &AttrValue) -> Value {
    match value {
        AttrValue::Int { value, wide } => {
            if *wide {
                Value::Int64(*value)
            } else {
                Value::Int32(*value as i32)
            }
        }
        #[cfg(feature = "float")]
        AttrValue::R4(number) => Value::Single(*number),
        #[cfg(feature = "float")]
        AttrValue::R8(number) => Value::Float(*number),
        #[cfg(not(feature = "float"))]
        AttrValue::R4(_) | AttrValue::R8(_) => Value::Null,
        AttrValue::Str(units) => Value::Object(vm.heap_mut().alloc_string(units)),
        AttrValue::Type(handle) => Value::NativeInt(*handle as i64),
        AttrValue::Null => Value::Null,
    }
}

/// Instantiates one custom attribute: allocates the attribute type's instance, runs its
/// constructor with the decoded positional arguments (a nested interpreter run, so a non-trivial
/// ctor body executes exactly as it would normally), then assigns each decoded named field. The
/// resulting object reference is what `GetCustomAttributes` returns to the array. Collection is
/// assumed suspended by the caller for the lifetime of the returned reference.
fn instantiate_attribute(
    vm: &mut Vm,
    module: &Module,
    attribute: &crate::module::LoadedAttribute,
) -> Result<ObjectRef, Trap> {
    let defaults = module
        .type_field_defaults(attribute.type_id)
        .ok_or(Trap::NoSuchMethod(attribute.ctor))?
        .to_vec();
    let instance = vm.heap_mut().alloc_instance(attribute.type_id, defaults);
    let mut ctor_args = Vec::with_capacity(attribute.positional.len() + 1);
    ctor_args.push(Value::Object(instance));
    for argument in &attribute.positional {
        ctor_args.push(materialize_attr_value(vm, argument));
    }
    Session::new(module, attribute.ctor, ctor_args)?.run(module, vm)?;
    for (slot, value) in &attribute.named_fields {
        let materialized = materialize_attr_value(vm, value);
        vm.heap_mut()
            .set_instance_field(instance, *slot, materialized);
    }
    for (setter, value) in &attribute.named_properties {
        let materialized = materialize_attr_value(vm, value);
        Session::new(module, *setter, alloc::vec![Value::Object(instance), materialized])?
            .run(module, vm)?;
    }
    Ok(instance)
}

/// `System.Reflection.MemberInfo.GetCustomAttributes(bool)` (the base method `Type`,
/// `FieldInfo`, `MethodInfo`, and `PropertyInfo` all inherit, which is what csc emits the call
/// against): an `object[]` of the attribute INSTANCES applied to the receiver. The receiver
/// `this` is the asm-folded handle of the target (a `Type`'s `TypeDef` token, or the `Field` /
/// `MethodDef` / `Property` token a `Type.GetField`/`GetMethod`/`GetProperty` returned); the
/// loader decoded each applied attribute's constructor + argument values under that handle. Each
/// attribute is instantiated (its ctor run, its named fields set); the `inherit` flag is accepted
/// and ignored (the attribute surface here is the directly-applied set -- the corpus does not
/// rely on inherited attributes). An array of length 0 when the target has no recorded attributes.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a handle; propagates a [`Trap`] from running an
/// attribute's constructor.
pub fn get_custom_attributes(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::NativeInt(handle)) = args.first() else {
        return Err(Trap::TypeMismatch(Opcode::Callvirt));
    };
    let attributes = module.custom_attributes_of(handle as u64);
    #[cfg(feature = "gc")]
    vm.suspend_collection();
    let mut elements: Vec<Value> = Vec::with_capacity(attributes.len());
    let mut outcome = Ok(());
    for attribute in attributes {
        match instantiate_attribute(vm, module, attribute) {
            Ok(instance) => elements.push(Value::Object(instance)),
            Err(trap) => {
                outcome = Err(trap);
                break;
            }
        }
    }
    let result = outcome.map(|()| {
        let array = vm.heap_mut().alloc_array(elements);
        Some(Value::Object(array))
    });
    #[cfg(feature = "gc")]
    vm.resume_collection();
    result
}

/// `System.Type.GetField(string)`: the `FieldInfo` for the named field of the type the receiver
/// handle identifies, modeled (like `Type`) as the field's asm-folded `Field` token in a native
/// int -- the handle its `GetCustomAttributes` then reads. The loader recorded each declared
/// field's name under its declaring type's handle. `null` if the type has no such field.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
pub fn type_get_field(vm: &mut Vm, module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    member_lookup(vm, args, |module, handle, name| {
        module.type_field_handle(handle, name)
    })(module)
}

/// `System.Type.GetMethod(string)`: the `MethodInfo` for the named method, modeled as the
/// method's asm-folded `MethodDef` token in a native int. `null` if the type has no such method.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
pub fn type_get_method(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    member_lookup(vm, args, |module, handle, name| {
        module.type_method_handle(handle, name)
    })(module)
}

/// `System.Type.GetProperty(string)`: the `PropertyInfo` for the named property, modeled as the
/// property's asm-folded `Property` token in a native int. `null` if the type has no such
/// property.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the receiver is not a type handle.
pub fn type_get_property(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    member_lookup(vm, args, |module, handle, name| {
        module.type_property_handle(handle, name)
    })(module)
}

/// The shared body of `Type.GetField`/`GetMethod`/`GetProperty`: read the type handle (the
/// receiver) and the member-name string argument, then resolve the member through `resolve` to
/// its asm-folded token, returned as a native-int handle (or null if absent). Returns a closure
/// taking the module so each public entry point reads as a one-liner; `vm` and `args` are
/// captured for the string lookup.
fn member_lookup<'a>(
    vm: &'a mut Vm,
    args: &'a [Value],
    resolve: impl Fn(&Module, u64, &str) -> Option<u64> + 'a,
) -> impl FnOnce(&Module) -> Result<Option<Value>, Trap> + 'a {
    move |module| {
        let Some(&Value::NativeInt(handle)) = args.first() else {
            return Err(Trap::TypeMismatch(Opcode::Callvirt));
        };
        let Some(name) = string_value(vm, args.get(1)) else {
            return Ok(Some(Value::Null));
        };
        Ok(Some(match resolve(module, handle as u64, &name) {
            Some(member_handle) => Value::NativeInt(member_handle as i64),
            None => Value::Null,
        }))
    }
}

/// `System.Runtime.CompilerServices.RuntimeHelpers.InitializeArray(Array, RuntimeFieldHandle)`:
/// fills the array's elements from the field's raw little-endian RVA initializer bytes (a
/// constant primitive array literal, `T[] a = {...}`). The field handle is the asm-folded
/// field token; its data blob was recorded at load. Each element is read at its width.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the first argument is not an array; the field-handle / blob
/// being absent is a no-op (the array keeps its zero elements) rather than a fault.
pub fn initialize_array(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let array = match args.first() {
        Some(&Value::Object(reference)) => reference,
        Some(Value::Null) => return Err(Trap::NullReference),
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let handle = type_handle_token(args.get(1));
    let Some(data) = module.field_rva_by_handle(handle) else {
        return Ok(None);
    };
    let Some(length) = vm.heap().array_len(array) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    if length == 0 {
        return Ok(None);
    }
    let width_bytes = data.len() / length;
    for index in 0..length {
        let element = vm.heap().array_get(array, index).unwrap_or(Value::Int32(0));
        let Some(filled) = element_from_bytes(&element, data, index, width_bytes) else {
            break;
        };
        vm.heap_mut().array_set(array, index, filled);
    }
    Ok(None)
}

/// One array element read from the little-endian initializer blob at `index`, taking the
/// width from `width_bytes` (the array's element type width). `None` once the blob is
/// exhausted, so a too-short blob leaves the remaining elements at their zero value.
fn element_from_bytes(zero: &Value, data: &[u8], index: usize, width_bytes: usize) -> Option<Value> {
    let start = index.checked_mul(width_bytes)?;
    let bytes = data.get(start..start.checked_add(width_bytes)?)?;
    Some(match zero {
        Value::Int32(_) => Value::Int32(read_le_int(bytes) as i32),
        Value::Int64(_) => Value::Int64(read_le_int(bytes)),
        Value::NativeInt(_) => Value::NativeInt(read_le_int(bytes)),
        #[cfg(feature = "float")]
        Value::Single(_) => Value::Single(f32::from_le_bytes(bytes.try_into().ok()?)),
        #[cfg(feature = "float")]
        Value::Float(_) => Value::Float(f64::from_le_bytes(bytes.try_into().ok()?)),
        _ => return None,
    })
}

/// A little-endian integer of `bytes.len()` (1, 2, 4, or 8) bytes, sign-extended to i64.
fn read_le_int(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    buf[..bytes.len()].copy_from_slice(bytes);
    let unsigned = u64::from_le_bytes(buf);
    let bits = bytes.len() * 8;
    if bits < 64 && unsigned & (1 << (bits - 1)) != 0 {
        (unsigned | (!0u64 << bits)) as i64
    } else {
        unsigned as i64
    }
}

/// The text of a string argument (None if absent or not a string).
fn string_value(vm: &Vm, arg: Option<&Value>) -> Option<String> {
    match arg {
        Some(Value::Object(reference)) => vm
            .heap()
            .as_string(*reference)
            .map(|chars| String::from_utf16_lossy(&chars)),
        _ => None,
    }
}

/// `T[,]::Get(i0, i1, ...)`: the element of a multi-dimensional array at the given indices.
///
/// # Errors
/// [`Trap::NullReference`] for a null array; [`Trap::IndexOutOfRange`] if an index is out of
/// range (or the rank does not match).
pub fn md_array_get(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let indices = int_indices(&args[1..]);
    vm.heap()
        .md_array_get(array, &indices)
        .map(Some)
        .ok_or_else(|| Trap::IndexOutOfRange(indices.first().copied().unwrap_or(0)))
}

/// `T[,]::Set(i0, i1, ..., value)`: stores `value` at the given indices.
///
/// # Errors
/// [`Trap::NullReference`] for a null array; [`Trap::IndexOutOfRange`] if an index is out of
/// range (or the rank does not match).
pub fn md_array_set(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let split = args.len().saturating_sub(1);
    let indices = int_indices(&args[1..split]);
    let value = args.get(split).cloned().unwrap_or(Value::Null);
    if vm.heap_mut().md_array_set(array, &indices, value) {
        Ok(None)
    } else {
        Err(Trap::IndexOutOfRange(indices.first().copied().unwrap_or(0)))
    }
}

/// `T[,]::Address(i0, i1, ...)`: a managed pointer to the element of a multi-dimensional
/// array at the given indices (what C# emits for `ref a[i,j]`). The element lives at the
/// row-major flat index into the array's storage -- computed exactly as `md_array_get`
/// does -- and is addressed by a `Location::Element` carrying that flat index, the same
/// element-pointer form `ldelema` yields for a single-dimensional array.
///
/// # Errors
/// [`Trap::NullReference`] for a null array; [`Trap::IndexOutOfRange`] if an index is out of
/// range (or the rank does not match).
pub fn md_array_address(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let indices = int_indices(&args[1..]);
    let flat = vm
        .heap()
        .md_array_flat_index(array, &indices)
        .ok_or_else(|| Trap::IndexOutOfRange(indices.first().copied().unwrap_or(0)))?;
    Ok(Some(Value::ByRef(crate::value::Location::Element {
        array,
        index: flat,
        byte_offset: 0,
    })))
}

/// The integer indices of a multi-dimensional array access (int32 / native-int values).
fn int_indices(values: &[Value]) -> Vec<i32> {
    values
        .iter()
        .map(|value| match value {
            Value::Int32(n) => *n,
            Value::Int64(n) | Value::NativeInt(n) => *n as i32,
            _ => 0,
        })
        .collect()
}

/// `Array::get_Length` (the `Length` property): the total number of elements.
///
/// # Errors
/// [`Trap::NullReference`] for a null array.
pub fn md_array_length(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let length = i32::try_from(vm.heap().array_len(array).unwrap_or(0)).unwrap_or(i32::MAX);
    Ok(Some(Value::Int32(length)))
}

/// `Array::GetLength(dim)`: the length of the given dimension.
///
/// # Errors
/// [`Trap::NullReference`] for a null array.
pub fn md_array_get_length(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let dim = match args.get(1) {
        Some(Value::Int32(n)) => *n,
        _ => 0,
    };
    Ok(Some(Value::Int32(
        vm.heap().array_dimension(array, dim).unwrap_or(0),
    )))
}

/// `System.Array.GetValue(int)`: the element at `index` as an `object`. A reference
/// element (or null) is returned as-is; a value-type element is boxed (III.4.1), so the
/// untyped accessor always yields an object reference, matching .NET.
///
/// # Errors
/// [`Trap::NullReference`] for a null array; [`Trap::IndexOutOfRange`] if `index` is out
/// of range.
pub fn array_get_value(
    vm: &mut Vm,
    module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let Some(&Value::Int32(index)) = args.get(1) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let element = usize::try_from(index)
        .ok()
        .and_then(|index| vm.heap().array_get(array, index))
        .ok_or(Trap::IndexOutOfRange(index))?;
    let boxed = match element {
        reference @ (Value::Object(_) | Value::Null) => reference,
        value => {
            let token = module.primitive_type_token(&value).unwrap_or(0);
            Value::Object(vm.heap_mut().alloc_boxed(token, value))
        }
    };
    Ok(Some(boxed))
}

/// `System.Array.SetValue(object value, int index)`: stores `value` at `index`. A
/// reference-element array stores the reference directly; a value-type-element array
/// unboxes `value` first (III.4.1), recovering the value-type value from its box.
///
/// # Errors
/// [`Trap::NullReference`] for a null array; [`Trap::IndexOutOfRange`] if `index` is out
/// of range; [`Trap::TypeMismatch`] if a value-type array's `value` is not a box.
pub fn array_set_value(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let value = args.get(1).cloned().unwrap_or(Value::Null);
    let Some(&Value::Int32(index)) = args.get(2) else {
        return Err(Trap::TypeMismatch(Opcode::Call));
    };
    let slot = usize::try_from(index).map_err(|_| Trap::IndexOutOfRange(index))?;
    let current = vm
        .heap()
        .array_get(array, slot)
        .ok_or(Trap::IndexOutOfRange(index))?;
    let to_store = match current {
        Value::Object(_) | Value::Null => value,
        _ => match value {
            Value::Object(boxed) => vm
                .heap()
                .boxed_value(boxed)
                .ok_or(Trap::TypeMismatch(Opcode::Call))?,
            _ => return Err(Trap::TypeMismatch(Opcode::Call)),
        },
    };
    if vm.heap_mut().array_set(array, slot, to_store) {
        Ok(None)
    } else {
        Err(Trap::IndexOutOfRange(index))
    }
}

/// `System.Array.Clone()` (the `ICloneable.Clone` implementation): a SHALLOW copy -- a new
/// array of the same shape whose elements are the same values. Reference elements are
/// shared, value-type elements copied, matching .NET. The element-type/dimension info is
/// carried by the heap array itself, so the managed `Array` base (which cannot name its
/// element type) crosses to this runtime primitive, the same way `GetValue`/`SetValue` do.
///
/// # Errors
/// [`Trap::NullReference`] if `this` is null or not an array.
pub fn array_clone(vm: &mut Vm, _module: &Module, args: &[Value]) -> Result<Option<Value>, Trap> {
    let Some(&Value::Object(array)) = args.first() else {
        return Err(Trap::NullReference);
    };
    let clone = vm.heap_mut().clone_array(array).ok_or(Trap::NullReference)?;
    Ok(Some(Value::Object(clone)))
}

/// `System.Buffer.ByteLength(Array)`: the array's total byte length (element count times the
/// element type's byte width). Returns `-1` -- the sentinel the managed `Buffer.ByteLength`
/// wrapper turns into `ArgumentException` -- for an array whose element type is NOT a primitive
/// (a reference / value-type array), which `System.Buffer` does not accept. A null receiver is
/// rejected by the managed wrapper before this is reached.
///
/// # Errors
/// [`Trap::NullReference`] if the argument is null; [`Trap::TypeMismatch`] if it is not an object.
pub fn buffer_byte_length(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let array = match args.first() {
        Some(&Value::Object(reference)) => reference,
        Some(Value::Null) => return Err(Trap::NullReference),
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    let length = match vm.heap().buffer_byte_length(array) {
        Some(bytes) => i32::try_from(bytes).unwrap_or(i32::MAX),
        None => -1,
    };
    Ok(Some(Value::Int32(length)))
}

/// `System.Buffer.BlockCopy(Array src, int srcOffset, Array dst, int dstOffset, int count)`:
/// copies `count` bytes from `src`'s flat little-endian byte image (at byte `srcOffset`) into
/// `dst`'s (at byte `dstOffset`). The managed wrapper has already validated the arguments (null,
/// negative offset/count, non-primitive arrays, and ranges within `ByteLength`), so a `false`
/// result from the heap here is a logic error rather than a user-facing exception.
///
/// # Errors
/// [`Trap::NullReference`] if `src` or `dst` is null; [`Trap::TypeMismatch`] for a non-object
/// reference or non-`int` offset/count; [`Trap::InvalidArgument`] if the (pre-validated) copy is
/// somehow out of range or over a non-primitive array.
pub fn buffer_block_copy(
    vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let src = buffer_array_arg(args.first())?;
    let src_offset = buffer_count_arg(args.get(1))?;
    let dst = buffer_array_arg(args.get(2))?;
    let dst_offset = buffer_count_arg(args.get(3))?;
    let count = buffer_count_arg(args.get(4))?;
    if vm
        .heap_mut()
        .buffer_block_copy(src, src_offset, dst, dst_offset, count)
    {
        Ok(None)
    } else {
        Err(Trap::InvalidArgument)
    }
}

/// An array-reference argument of a `Buffer` intrinsic.
fn buffer_array_arg(arg: Option<&Value>) -> Result<crate::object::ObjectRef, Trap> {
    match arg {
        Some(&Value::Object(reference)) => Ok(reference),
        Some(Value::Null) => Err(Trap::NullReference),
        _ => Err(Trap::TypeMismatch(Opcode::Call)),
    }
}

/// A non-negative `int` byte offset / count argument of a `Buffer` intrinsic, as a `usize`. A
/// negative value cannot occur (the managed wrapper rejects it first) but maps to a type mismatch
/// rather than silently wrapping.
fn buffer_count_arg(arg: Option<&Value>) -> Result<usize, Trap> {
    match arg {
        Some(&Value::Int32(value)) => {
            usize::try_from(value).map_err(|_| Trap::TypeMismatch(Opcode::Call))
        }
        _ => Err(Trap::TypeMismatch(Opcode::Call)),
    }
}

/// The 128-bit `System.Decimal` value type, implemented over the same `lo`/`mid`/`hi`/`flags`
/// layout .NET uses: a 96-bit unsigned mantissa (the three 32-bit words) scaled by a power of
/// ten in `[0, 28]` (the scale lives in `flags` bits 16..23) with a sign in `flags` bit 31.
/// The decimal value is `(-1)^sign * mantissa * 10^-scale`.
///
/// The managed corlib (`corlib/System/Decimal.cs`) owns everything that only needs 64-bit
/// integer math -- the constructors, `ToString`, `Parse`, the integer conversions, hashing --
/// and reaches HERE for the operations whose intermediates can exceed 96 bits (and so cannot
/// be done with the C# corlib's `long`/`ulong`): the five arithmetic operators, comparison,
/// and the floating-point conversions. Each works on a fixed-width `[u32]` magnitude so a
/// 96x96 product (up to 192 bits) and a scale alignment (multiplying a 96-bit mantissa by a
/// power of ten) are exact, then rescales the result half-to-even into the 96-bit/scale<=28
/// range the way .NET's `DecCalc` does -- so `0.1m + 0.2m == 0.3m`, `1m / 3m` carries 28
/// significant digits, and an out-of-range result raises `OverflowException`.
mod decimal_ops {
    use crate::value::Value;
    use alloc::boxed::Box;

    /// The maximum scale (number of fractional decimal digits) a `Decimal` can carry.
    const MAX_SCALE: u32 = 28;
    /// The number of 32-bit limbs in the working magnitude: enough for a 96x96 product (192
    /// bits = 6 limbs) plus headroom for a rounding carry and intermediate scale-up shifts.
    const LIMBS: usize = 8;

    /// A `Decimal` decoded into its parts: a 96-bit magnitude (held in the low three limbs of a
    /// wider buffer), the base-ten scale, and the sign.
    #[derive(Clone, Copy)]
    pub struct Dec {
        /// The mantissa magnitude, little-endian 32-bit limbs (only the low three are the value;
        /// the rest are working headroom, zero on a decoded value).
        mag: [u32; LIMBS],
        /// The base-ten scale (`0..=28`): the value is `mag * 10^-scale`.
        scale: u32,
        /// The sign: true for negative.
        negative: bool,
    }

    impl Dec {
        /// Flips the sign (used to turn subtraction into addition).
        pub fn negate(&mut self) {
            if !is_zero(&self.mag) {
                self.negative = !self.negative;
            }
        }

        /// Whether the value is zero (a zero divisor check, before the sign matters).
        pub fn is_zero_value(&self) -> bool {
            is_zero(&self.mag)
        }
    }

    /// Decodes `(lo, mid, hi, flags)` into a [`Dec`]. The scale and sign come from `flags`
    /// (bits 16..23 and bit 31). An out-of-spec scale (> 28) is clamped defensively, though
    /// a well-formed `Decimal` never carries one.
    fn decode(lo: i32, mid: i32, hi: i32, flags: i32) -> Dec {
        let mut mag = [0u32; LIMBS];
        mag[0] = lo as u32;
        mag[1] = mid as u32;
        mag[2] = hi as u32;
        let scale = ((flags as u32) >> 16) & 0xFF;
        let negative = (flags as u32) & 0x8000_0000 != 0;
        Dec {
            mag,
            scale: scale.min(MAX_SCALE),
            negative,
        }
    }

    /// Reads a [`Dec`] from a `Value::Struct` argument (the inline value-type form a `Decimal`
    /// takes on the stack): its four field slots are `lo, mid, hi, flags`, matching the field
    /// declaration order in the managed `corlib/System/Decimal.cs`. The four-field width is
    /// exactly what a `Decimal` instance carries, so any other shape is rejected.
    fn dec_arg(args: &[Value], index: usize) -> Option<Dec> {
        let fields = match args.get(index) {
            Some(Value::Struct(fields)) if fields.len() == 4 => fields,
            _ => return None,
        };
        let word = |i: usize| match fields[i] {
            Value::Int32(value) => Some(value),
            _ => None,
        };
        Some(decode(word(0)?, word(1)?, word(2)?, word(3)?))
    }

    /// Whether the magnitude is zero.
    fn is_zero(mag: &[u32; LIMBS]) -> bool {
        mag.iter().all(|&w| w == 0)
    }

    /// Whether the magnitude exceeds 96 bits (any limb above the low three is set).
    fn exceeds_96(mag: &[u32; LIMBS]) -> bool {
        mag[3..].iter().any(|&w| w != 0)
    }

    /// `mag += other` (limb-wise with carry). Returns the carry out of the top limb (nonzero
    /// only on a true overflow past the buffer width, which the callers size against).
    fn add_into(mag: &mut [u32; LIMBS], other: &[u32; LIMBS]) -> u32 {
        let mut carry = 0u64;
        for i in 0..LIMBS {
            let sum = u64::from(mag[i]) + u64::from(other[i]) + carry;
            mag[i] = sum as u32;
            carry = sum >> 32;
        }
        carry as u32
    }

    /// `mag -= other`, assuming `mag >= other` (the caller orders the operands by magnitude).
    fn sub_into(mag: &mut [u32; LIMBS], other: &[u32; LIMBS]) {
        let mut borrow = 0i64;
        for i in 0..LIMBS {
            let diff = i64::from(mag[i]) - i64::from(other[i]) - borrow;
            if diff < 0 {
                mag[i] = (diff + (1i64 << 32)) as u32;
                borrow = 1;
            } else {
                mag[i] = diff as u32;
                borrow = 0;
            }
        }
    }

    /// Compares two magnitudes (`-1`/`0`/`1`), high limb first.
    fn cmp_mag(a: &[u32; LIMBS], b: &[u32; LIMBS]) -> i32 {
        for i in (0..LIMBS).rev() {
            if a[i] != b[i] {
                return if a[i] > b[i] { 1 } else { -1 };
            }
        }
        0
    }

    /// `mag *= factor` (a single 32-bit multiplier). Returns the carry out of the top limb.
    fn mul_small(mag: &mut [u32; LIMBS], factor: u32) -> u32 {
        let mut carry = 0u64;
        for limb in mag.iter_mut() {
            let product = u64::from(*limb) * u64::from(factor) + carry;
            *limb = product as u32;
            carry = product >> 32;
        }
        carry as u32
    }

    /// `mag /= 10`, returning the remainder (`0..=9`). High limb first so the running remainder
    /// threads down through the limbs.
    fn div10(mag: &mut [u32; LIMBS]) -> u32 {
        let mut remainder = 0u64;
        for i in (0..LIMBS).rev() {
            let cur = (remainder << 32) | u64::from(mag[i]);
            mag[i] = (cur / 10) as u32;
            remainder = cur % 10;
        }
        remainder as u32
    }

    /// Powers of ten that fit one 32-bit limb (`10^0 .. 10^9`), for scaling a magnitude up by a
    /// known number of decimal places in chunks.
    const POW10_U32: [u32; 10] = [
        1,
        10,
        100,
        1000,
        10000,
        100_000,
        1_000_000,
        10_000_000,
        100_000_000,
        1_000_000_000,
    ];

    /// Multiplies `mag` by `10^power` in single-limb chunks. Returns false if the product
    /// overflows the working buffer (the caller treats that as out of range).
    fn scale_up(mag: &mut [u32; LIMBS], mut power: u32) -> bool {
        while power > 0 {
            let chunk = power.min(9);
            if mul_small(mag, POW10_U32[chunk as usize]) != 0 {
                return false;
            }
            power -= chunk;
        }
        true
    }

    /// Rounds a magnitude down by `drop` decimal places, half-to-even (banker's rounding, what
    /// .NET's `DecCalc` uses): divide by ten `drop` times, tracking whether anything below the
    /// final digit was nonzero so a tie is broken to even. Returns the carry of a round-up that
    /// could grow the magnitude (e.g. 9.5 -> 10).
    fn round_off(mag: &mut [u32; LIMBS], drop: u32) {
        if drop == 0 {
            return;
        }
        let mut sticky = false;
        let mut last = 0u32;
        for _ in 0..drop {
            sticky |= last != 0;
            last = div10(mag);
        }
        let round_up = last > 5 || (last == 5 && (sticky || mag[0] & 1 == 1));
        if round_up {
            let mut one = [0u32; LIMBS];
            one[0] = 1;
            add_into(mag, &one);
        }
    }

    /// Builds the result `Value::Struct([lo, mid, hi, flags])` from a magnitude, scale, and sign,
    /// after rescaling it into the 96-bit / `scale<=28` range half-to-even. A magnitude that
    /// still will not fit, or a scale that cannot be reduced enough, is an overflow (`None`).
    /// A zero magnitude normalizes to a clean positive zero at the requested scale (matching
    /// .NET, which keeps a zero's scale but not its sign).
    fn finish(mut mag: [u32; LIMBS], mut scale: u32, negative: bool) -> Option<Value> {
        while exceeds_96(&mag) || scale > MAX_SCALE {
            if scale == 0 {
                return None;
            }
            let drop = if scale > MAX_SCALE { scale - MAX_SCALE } else { 1 };
            round_off(&mut mag, drop);
            scale -= drop;
        }
        if is_zero(&mag) {
            return Some(encode(&mag, scale, false));
        }
        Some(encode(&mag, scale, negative))
    }

    /// Packs a fit magnitude (low three limbs) + scale + sign into the `Value::Struct` form.
    fn encode(mag: &[u32; LIMBS], scale: u32, negative: bool) -> Value {
        let flags = (scale << 16) | if negative { 0x8000_0000 } else { 0 };
        Value::Struct(Box::new([
            Value::Int32(mag[0] as i32),
            Value::Int32(mag[1] as i32),
            Value::Int32(mag[2] as i32),
            Value::Int32(flags as i32),
        ]))
    }

    /// Aligns two decoded decimals to a common scale by scaling the lower-scale magnitude up.
    /// Returns the common scale, or `None` if scaling up overflowed the working buffer (the
    /// callers map that to `OverflowException`).
    fn align(a: &mut Dec, b: &mut Dec) -> Option<u32> {
        if a.scale < b.scale {
            if !scale_up(&mut a.mag, b.scale - a.scale) {
                return None;
            }
            a.scale = b.scale;
        } else if b.scale < a.scale {
            if !scale_up(&mut b.mag, a.scale - b.scale) {
                return None;
            }
            b.scale = a.scale;
        }
        Some(a.scale)
    }

    /// The signed sum `a + b` (used for both addition and subtraction; the caller flips `b`'s
    /// sign for subtraction). Aligns scales, then adds same-sign magnitudes or subtracts the
    /// smaller from the larger for opposite signs, and finishes (rescaling/rounding) the result.
    pub fn add(mut a: Dec, mut b: Dec) -> Option<Value> {
        let scale = align(&mut a, &mut b)?;
        if a.negative == b.negative {
            let mut mag = a.mag;
            add_into(&mut mag, &b.mag);
            finish(mag, scale, a.negative)
        } else {
            match cmp_mag(&a.mag, &b.mag) {
                0 => finish([0u32; LIMBS], scale, false),
                1 => {
                    let mut mag = a.mag;
                    sub_into(&mut mag, &b.mag);
                    finish(mag, scale, a.negative)
                }
                _ => {
                    let mut mag = b.mag;
                    sub_into(&mut mag, &a.mag);
                    finish(mag, scale, b.negative)
                }
            }
        }
    }

    /// The product `a * b`: the magnitudes multiply (a full 192-bit schoolbook product over the
    /// working buffer) and the scales add; `finish` then rescales/rounds into range. .NET caps
    /// the product scale at 28, rounding away extra fractional digits, which `finish` does.
    pub fn mul(a: Dec, b: Dec) -> Option<Value> {
        let mut product = [0u32; LIMBS];
        for i in 0..3 {
            if a.mag[i] == 0 {
                continue;
            }
            let mut carry = 0u64;
            for j in 0..3 {
                let pos = i + j;
                let cur = u64::from(a.mag[i]) * u64::from(b.mag[j])
                    + u64::from(product[pos])
                    + carry;
                product[pos] = cur as u32;
                carry = cur >> 32;
            }
            let mut pos = i + 3;
            while carry != 0 && pos < LIMBS {
                let cur = u64::from(product[pos]) + carry;
                product[pos] = cur as u32;
                carry = cur >> 32;
                pos += 1;
            }
            if carry != 0 {
                return None;
            }
        }
        let negative = a.negative != b.negative;
        finish(product, a.scale + b.scale, negative)
    }

    /// The quotient `a / b`: align both to the same scale (so the quotient is the integer ratio
    /// of the magnitudes), then long-divide, extending the dividend by extra factors of ten to
    /// generate fractional digits up to .NET's 28-29 significant-digit limit, rounding the last
    /// digit half-to-even. Division by zero is `None` (the managed wrapper raises
    /// `DivideByZeroException`); the algorithm matches .NET's `VarDecDiv` results.
    pub fn div(mut a: Dec, mut b: Dec) -> Option<Value> {
        if is_zero(&b.mag) {
            return None;
        }
        if is_zero(&a.mag) {
            return finish([0u32; LIMBS], 0, false);
        }
        let common = a.scale.max(b.scale);
        if !scale_up(&mut a.mag, common - a.scale) {
            return None;
        }
        if !scale_up(&mut b.mag, common - b.scale) {
            return None;
        }
        let divisor = b.mag;
        let (mut quotient, mut remainder) = divmod(&a.mag, &divisor);
        if exceeds_96(&quotient) {
            return None;
        }
        let mut result_scale = 0u32;
        while !is_zero(&remainder) && result_scale < MAX_SCALE {
            if mul_small(&mut remainder, 10) != 0 {
                break;
            }
            let (digit, r) = divmod(&remainder, &divisor);
            let mut shifted = quotient;
            if mul_small(&mut shifted, 10) != 0 {
                break;
            }
            add_into(&mut shifted, &digit);
            if exceeds_96(&shifted) {
                break;
            }
            quotient = shifted;
            remainder = r;
            result_scale += 1;
        }
        if !is_zero(&remainder) {
            let mut twice = remainder;
            let overflow = mul_small(&mut twice, 2) != 0;
            let round_up = if overflow {
                true
            } else {
                let cmp = cmp_mag(&twice, &divisor);
                cmp > 0 || (cmp == 0 && quotient[0] & 1 == 1)
            };
            if round_up {
                let mut one = [0u32; LIMBS];
                one[0] = 1;
                add_into(&mut quotient, &one);
                if exceeds_96(&quotient) {
                    return None;
                }
            }
        }
        let negative = a.negative != b.negative;
        finish(quotient, result_scale, negative)
    }

    /// The remainder `a % b` (.NET's `Decimal.Remainder`): the result has the sign of the
    /// dividend and `|a % b| < |b|`. Computed by aligning scales and taking the magnitude
    /// remainder of the integer division. `None` on a zero divisor.
    pub fn rem(mut a: Dec, mut b: Dec) -> Option<Value> {
        if is_zero(&b.mag) {
            return None;
        }
        let scale = align(&mut a, &mut b)?;
        let (_, r) = divmod(&a.mag, &b.mag);
        finish(r, scale, a.negative)
    }

    /// Compares two decimals by value (`-1`/`0`/`1`), scale-independent: a zero is equal
    /// regardless of sign, opposite signs order by sign, and same signs align scales then
    /// compare magnitudes (the sign flips the order for negatives). Returns `None` only if a
    /// scale alignment overflows the working buffer, which the caller surfaces as a fault.
    pub fn compare(mut a: Dec, mut b: Dec) -> Option<i32> {
        let a_zero = is_zero(&a.mag);
        let b_zero = is_zero(&b.mag);
        if a_zero && b_zero {
            return Some(0);
        }
        if a_zero {
            return Some(if b.negative { 1 } else { -1 });
        }
        if b_zero {
            return Some(if a.negative { -1 } else { 1 });
        }
        if a.negative != b.negative {
            return Some(if a.negative { -1 } else { 1 });
        }
        align(&mut a, &mut b)?;
        let mag_cmp = cmp_mag(&a.mag, &b.mag);
        Some(if a.negative { -mag_cmp } else { mag_cmp })
    }

    /// Integer division of magnitudes: returns `(quotient, remainder)` with
    /// `dividend = quotient*divisor + remainder` and `remainder < divisor`. A restoring
    /// bit-at-a-time long division over the working buffer -- exact and simple (the operands are
    /// small enough that performance is a non-issue for the differential corpus).
    fn divmod(dividend: &[u32; LIMBS], divisor: &[u32; LIMBS]) -> ([u32; LIMBS], [u32; LIMBS]) {
        let mut quotient = [0u32; LIMBS];
        let mut remainder = [0u32; LIMBS];
        let total_bits = LIMBS * 32;
        for bit in (0..total_bits).rev() {
            shl1(&mut remainder);
            let word = bit / 32;
            let off = bit % 32;
            if (dividend[word] >> off) & 1 == 1 {
                remainder[0] |= 1;
            }
            if cmp_mag(&remainder, divisor) >= 0 {
                sub_into(&mut remainder, divisor);
                quotient[word] |= 1 << off;
            }
        }
        (quotient, remainder)
    }

    /// `mag <<= 1`.
    fn shl1(mag: &mut [u32; LIMBS]) {
        let mut carry = 0u32;
        for limb in mag.iter_mut() {
            let new_carry = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = new_carry;
        }
    }

    /// Converts an `f64` to a [`Dec`] the way .NET's `Decimal(double)` ctor does: round the
    /// value to 15 significant decimal digits (double's reliable precision), then express that as
    /// a 96-bit mantissa with the matching scale. Returns `None` for NaN / infinity / a
    /// magnitude outside the Decimal range (the managed ctor raises `OverflowException`).
    #[cfg(feature = "float")]
    pub fn from_double(value: f64) -> Option<Value> {
        if !value.is_finite() {
            return None;
        }
        if value == 0.0 {
            return finish([0u32; LIMBS], 0, false);
        }
        let negative = value < 0.0;
        let magnitude = value.abs();
        let mut exp = floor_log10(magnitude);
        let mut digits15 = magnitude / pow10_f64(exp - 14);
        if digits15 >= 1e15 {
            exp += 1;
            digits15 = magnitude / pow10_f64(exp - 14);
        } else if digits15 < 1e14 {
            exp -= 1;
            digits15 = magnitude / pow10_f64(exp - 14);
        }
        let rounded = round_half_even_f64(digits15);
        let mut int_digits = rounded as u128;
        if int_digits == 0 {
            return finish([0u32; LIMBS], 0, false);
        }
        let mut scale_pow = exp - 14;
        while int_digits % 10 == 0 {
            int_digits /= 10;
            scale_pow += 1;
        }
        let mut mag = [0u32; LIMBS];
        mag[0] = int_digits as u32;
        mag[1] = (int_digits >> 32) as u32;
        mag[2] = (int_digits >> 64) as u32;
        if scale_pow >= 0 {
            if !scale_up(&mut mag, scale_pow as u32) {
                return None;
            }
            finish(mag, 0, negative)
        } else {
            finish(mag, (-scale_pow) as u32, negative)
        }
    }

    /// `floor(log10(x))` for a finite positive `x`, without a math library: bracket the value
    /// between consecutive integer powers of ten (the range of magnitudes is small).
    #[cfg(feature = "float")]
    fn floor_log10(x: f64) -> i32 {
        let mut exp = 0i32;
        let mut v = x;
        while v >= 10.0 {
            v /= 10.0;
            exp += 1;
        }
        while v < 1.0 {
            v *= 10.0;
            exp -= 1;
        }
        exp
    }

    /// `10^n` as an `f64` for a small signed exponent, by repeated multiply/divide (exact for the
    /// |n| <= ~22 range where 10^n is representable exactly in a double; good enough beyond).
    #[cfg(feature = "float")]
    fn pow10_f64(n: i32) -> f64 {
        let mut result = 1.0f64;
        let mut k = n.abs();
        while k > 0 {
            result *= 10.0;
            k -= 1;
        }
        if n < 0 { 1.0 / result } else { result }
    }

    /// Rounds an `f64` to the nearest integer, ties to even.
    #[cfg(feature = "float")]
    fn round_half_even_f64(x: f64) -> f64 {
        let floor = floor_f64(x);
        let frac = x - floor;
        if frac < 0.5 {
            floor
        } else if frac > 0.5 {
            floor + 1.0
        } else if (floor as i64) & 1 == 0 {
            floor
        } else {
            floor + 1.0
        }
    }

    /// `floor(x)` for a non-negative `x` within the i64 range (the 15-digit integers here),
    /// without a math library.
    #[cfg(feature = "float")]
    fn floor_f64(x: f64) -> f64 {
        let truncated = x as i64 as f64;
        if truncated > x { truncated - 1.0 } else { truncated }
    }

    /// Converts a decoded decimal to the nearest `f64` (.NET's `(double)dec` operator): the
    /// 96-bit mantissa as a float divided by `10^scale`. Double's rounding gives the same
    /// result as .NET here for the values the corpus exercises.
    #[cfg(feature = "float")]
    pub fn to_double(a: Dec) -> f64 {
        let mantissa =
            u128::from(a.mag[0]) | (u128::from(a.mag[1]) << 32) | (u128::from(a.mag[2]) << 64);
        let mut value = mantissa as f64;
        value /= pow10_f64(a.scale as i32);
        if a.negative {
            -value
        } else {
            value
        }
    }

    /// Decodes the two-`Decimal` argument form (two `Value::Struct`s): `op_Addition(a, b)` etc.
    pub fn two(args: &[Value]) -> Option<(Dec, Dec)> {
        Some((dec_arg(args, 0)?, dec_arg(args, 1)?))
    }

    /// Decodes the single-`Decimal` argument form (one `Value::Struct`): the `this` of a unary
    /// conversion like `op_Explicit(decimal) -> double`.
    pub fn one(args: &[Value]) -> Option<Dec> {
        dec_arg(args, 0)
    }
}

/// `System.Decimal::DecAdd(lo1,mid1,hi1,flags1, lo2,mid2,hi2,flags2)`: the exact sum, returned
/// as the four `Decimal` words (`Value::Struct`). Backs the managed `op_Addition`. See
/// [`decimal_ops`] for the algorithm.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the eight arguments are not `Int32`; [`Trap::Overflow`] if the
/// sum is outside the `Decimal` range.
pub fn decimal_add(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let (a, b) = decimal_ops::two(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    decimal_ops::add(a, b)
        .ok_or(Trap::Overflow)
        .map(Some)
}

/// `System.Decimal::DecSub(...)`: the exact difference (backs `op_Subtraction`).
///
/// # Errors
/// [`Trap::TypeMismatch`] for non-`Int32` arguments; [`Trap::Overflow`] on range overflow.
pub fn decimal_subtract(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let (a, mut b) = decimal_ops::two(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    b.negate();
    decimal_ops::add(a, b)
        .ok_or(Trap::Overflow)
        .map(Some)
}

/// `System.Decimal::DecMul(...)`: the exact product (backs `op_Multiply`).
///
/// # Errors
/// [`Trap::TypeMismatch`] for non-`Int32` arguments; [`Trap::Overflow`] on range overflow.
pub fn decimal_multiply(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let (a, b) = decimal_ops::two(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    decimal_ops::mul(a, b)
        .ok_or(Trap::Overflow)
        .map(Some)
}

/// `System.Decimal::DecDiv(...)`: the quotient at full Decimal precision (backs `op_Division`).
///
/// # Errors
/// [`Trap::TypeMismatch`] for non-`Int32` arguments; [`Trap::DivideByZero`] if the divisor is
/// zero; [`Trap::Overflow`] on range overflow.
pub fn decimal_divide(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let (a, b) = decimal_ops::two(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    if b.is_zero_value() {
        return Err(Trap::DivideByZero);
    }
    decimal_ops::div(a, b)
        .ok_or(Trap::Overflow)
        .map(Some)
}

/// `System.Decimal::DecRem(...)`: the remainder with the dividend's sign (backs `op_Modulus`).
///
/// # Errors
/// [`Trap::TypeMismatch`] for non-`Int32` arguments; [`Trap::DivideByZero`] for a zero divisor;
/// [`Trap::Overflow`] on range overflow.
pub fn decimal_remainder(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let (a, b) = decimal_ops::two(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    if b.is_zero_value() {
        return Err(Trap::DivideByZero);
    }
    decimal_ops::rem(a, b)
        .ok_or(Trap::Overflow)
        .map(Some)
}

/// `System.Decimal::DecCompare(...)`: `-1`/`0`/`1` by value (scale-independent). Backs
/// `CompareTo`, `Equals`, and every relational operator.
///
/// # Errors
/// [`Trap::TypeMismatch`] for non-`Int32` arguments.
pub fn decimal_compare(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let (a, b) = decimal_ops::two(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    let ordering = decimal_ops::compare(a, b).ok_or(Trap::Overflow)?;
    Ok(Some(Value::Int32(ordering)))
}

/// `System.Decimal::FromDouble(double)`: the `Decimal(double)` ctor result as the four words.
///
/// # Errors
/// [`Trap::TypeMismatch`] for a non-`double` argument; [`Trap::Overflow`] for NaN / infinity /
/// out-of-range.
#[cfg(feature = "float")]
pub fn decimal_from_double(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let value = match args.first() {
        Some(&Value::Float(value)) => value,
        _ => return Err(Trap::TypeMismatch(Opcode::Call)),
    };
    decimal_ops::from_double(value)
        .ok_or(Trap::Overflow)
        .map(Some)
}

/// `System.Decimal::ToDouble(lo,mid,hi,flags)`: the `(double)dec` operator.
///
/// # Errors
/// [`Trap::TypeMismatch`] for non-`Int32` arguments.
#[cfg(feature = "float")]
pub fn decimal_to_double(
    _vm: &mut Vm,
    _module: &Module,
    args: &[Value],
) -> Result<Option<Value>, Trap> {
    let a = decimal_ops::one(args).ok_or(Trap::TypeMismatch(Opcode::Call))?;
    Ok(Some(Value::Float(decimal_ops::to_double(a))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::Module;
    use crate::{Vm, run};
    use alloc::boxed::Box;
    use alloc::vec;
    use lamella_cil::{Instruction, MethodBodyImage, Operand};
    use lamella_token::Token;

    #[test]
    fn hello_world_from_a_hand_built_assembly() {
        let mut module = Module::new();

        let write_line = module.add_intrinsic(0, console_write_line, 1);
        let write_line_token = Token(0x0A00_0001);
        module.bind_token(0, write_line_token, write_line);

        let string_token = Token(0x7000_0001);
        let hello: Vec<u16> = "Hello, World".encode_utf16().collect();
        module.bind_string(0, string_token, &hello);

        let main = module.add_method(
            0,
            MethodBodyImage {
                max_stack: 8,
                init_locals: false,
                local_var_sig: None,
                code: vec![
                    Instruction::new(Opcode::Ldstr, Operand::Token(string_token)),
                    Instruction::new(Opcode::Call, Operand::Token(write_line_token)),
                    Instruction::simple(Opcode::Ret),
                ]
                .into_boxed_slice(),
                handlers: Box::new([]),
            },
            0,
        );

        let mut vm = Vm::new();
        let result = run(&module, &mut vm, main, Vec::new());

        assert_eq!(result, Ok(None));
        assert_eq!(vm.output_string(), "Hello, World\n");
    }

    #[test]
    fn write_line_of_null_is_a_blank_line() {
        let mut vm = Vm::new();
        assert_eq!(
            console_write_line(&mut vm, &Module::new(), &[Value::Null]),
            Ok(None)
        );
        assert_eq!(vm.output_string(), "\n");
    }

    #[test]
    fn write_line_of_a_non_string_traps() {
        let mut vm = Vm::new();
        assert_eq!(
            console_write_line(&mut vm, &Module::new(), &[Value::Int32(7)]),
            Err(Trap::TypeMismatch(Opcode::Call))
        );
    }

    /// Encodes a `Decimal` value (`mantissa * 10^-scale`, mantissa <= 2^96-1) as the inline
    /// `Value::Struct` an operand arrives as: four field slots `lo, mid, hi, flags`.
    #[cfg(test)]
    fn dec_words(mantissa: u128, scale: u32, negative: bool) -> Vec<Value> {
        let flags = (scale << 16) | if negative { 0x8000_0000 } else { 0 };
        alloc::vec![Value::Struct(Box::new([
            Value::Int32(mantissa as u32 as i32),
            Value::Int32((mantissa >> 32) as u32 as i32),
            Value::Int32((mantissa >> 64) as u32 as i32),
            Value::Int32(flags as i32),
        ]))]
    }

    /// Decodes a result `Value::Struct` back into `(mantissa, scale, negative)` for assertions.
    #[cfg(test)]
    fn dec_parts(value: &Value) -> (u128, u32, bool) {
        let Value::Struct(fields) = value else {
            panic!("expected a Decimal struct, got {value:?}");
        };
        let word = |i: usize| match fields[i] {
            Value::Int32(v) => v as u32 as u128,
            _ => panic!("non-int field"),
        };
        let mant = word(0) | (word(1) << 32) | (word(2) << 64);
        let flags = match fields[3] {
            Value::Int32(v) => v as u32,
            _ => panic!("non-int flags"),
        };
        (mant, (flags >> 16) & 0xFF, flags & 0x8000_0000 != 0)
    }

    /// Builds the eight-argument vector for a two-operand decimal intrinsic.
    #[cfg(test)]
    fn two_args(a: (u128, u32, bool), b: (u128, u32, bool)) -> Vec<Value> {
        let mut args = Vec::new();
        args.extend(dec_words(a.0, a.1, a.2));
        args.extend(dec_words(b.0, b.1, b.2));
        args
    }

    #[test]
    fn decimal_add_aligns_scales_exactly() {
        let mut vm = Vm::new();
        let r = decimal_add(&mut vm, &Module::new(), &two_args((1, 1, false), (2, 1, false)))
            .unwrap()
            .unwrap();
        assert_eq!(dec_parts(&r), (3, 1, false));
    }

    #[test]
    fn decimal_add_keeps_larger_scale() {
        let mut vm = Vm::new();
        let r = decimal_add(
            &mut vm,
            &Module::new(),
            &two_args((10000, 2, false), (1, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (10100, 2, false));
    }

    #[test]
    fn decimal_subtract_opposite_magnitudes() {
        let mut vm = Vm::new();
        let r = decimal_subtract(
            &mut vm,
            &Module::new(),
            &two_args((1, 0, false), (9, 1, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (1, 1, false));
    }

    #[test]
    fn decimal_multiply_adds_scales() {
        let mut vm = Vm::new();
        let r = decimal_multiply(
            &mut vm,
            &Module::new(),
            &two_args((15, 1, false), (2, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (30, 1, false));
        let r = decimal_multiply(
            &mut vm,
            &Module::new(),
            &two_args((12345678, 4, false), (1000, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (12_345_678_000, 4, false));
    }

    #[test]
    fn decimal_divide_full_precision_half_even() {
        let mut vm = Vm::new();
        let r = decimal_divide(
            &mut vm,
            &Module::new(),
            &two_args((1, 0, false), (3, 0, false)),
        )
        .unwrap()
        .unwrap();
        let (mant, scale, neg) = dec_parts(&r);
        assert_eq!((scale, neg), (28, false));
        assert_eq!(mant, 3_333_333_333_333_333_333_333_333_333u128);
        let r = decimal_divide(
            &mut vm,
            &Module::new(),
            &two_args((1, 0, false), (7, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r).0, 1_428_571_428_571_428_571_428_571_429u128);
        let r = decimal_divide(
            &mut vm,
            &Module::new(),
            &two_args((1, 0, false), (8, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (125, 3, false));
        let r = decimal_divide(
            &mut vm,
            &Module::new(),
            &two_args((6, 0, false), (2, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (3, 0, false));
    }

    #[test]
    fn decimal_remainder_keeps_dividend_sign() {
        let mut vm = Vm::new();
        let r = decimal_remainder(
            &mut vm,
            &Module::new(),
            &two_args((10, 0, false), (3, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (1, 0, false));
        let r = decimal_remainder(
            &mut vm,
            &Module::new(),
            &two_args((10, 0, true), (3, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec_parts(&r), (1, 0, true));
    }

    #[test]
    fn decimal_compare_is_scale_independent() {
        let mut vm = Vm::new();
        let r = decimal_compare(
            &mut vm,
            &Module::new(),
            &two_args((250, 2, false), (25, 1, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int32(0));
        let r = decimal_compare(
            &mut vm,
            &Module::new(),
            &two_args((5, 0, false), (3, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int32(1));
        let r = decimal_compare(
            &mut vm,
            &Module::new(),
            &two_args((1, 0, true), (0, 0, false)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int32(-1));
    }

    #[test]
    fn decimal_add_overflow_is_a_trap() {
        let mut vm = Vm::new();
        let max = (1u128 << 96) - 1;
        let r = decimal_add(
            &mut vm,
            &Module::new(),
            &two_args((max, 0, false), (1, 0, false)),
        );
        assert_eq!(r, Err(Trap::Overflow));
    }

    #[test]
    fn decimal_divide_by_zero_traps() {
        let mut vm = Vm::new();
        let r = decimal_divide(
            &mut vm,
            &Module::new(),
            &two_args((5, 0, false), (0, 0, false)),
        );
        assert_eq!(r, Err(Trap::DivideByZero));
    }

    #[cfg(feature = "float")]
    #[test]
    fn decimal_from_double_rounds_to_fifteen_digits() {
        let mut vm = Vm::new();
        let r = decimal_from_double(&mut vm, &Module::new(), &[Value::Float(0.1)])
            .unwrap()
            .unwrap();
        assert_eq!(dec_parts(&r), (1, 1, false));
        let r = decimal_from_double(&mut vm, &Module::new(), &[Value::Float(1.5)])
            .unwrap()
            .unwrap();
        assert_eq!(dec_parts(&r), (15, 1, false));
        let r = decimal_from_double(&mut vm, &Module::new(), &[Value::Float(1_000_000.0)])
            .unwrap()
            .unwrap();
        assert_eq!(dec_parts(&r), (1_000_000, 0, false));
        let r = decimal_from_double(&mut vm, &Module::new(), &[Value::Float(1.0 / 3.0)])
            .unwrap()
            .unwrap();
        assert_eq!(dec_parts(&r), (333_333_333_333_333, 15, false));
    }

    #[cfg(feature = "float")]
    #[test]
    fn decimal_to_double_round_trips_simple_values() {
        let mut vm = Vm::new();
        let r = decimal_to_double(&mut vm, &Module::new(), &dec_words(15, 1, false))
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Float(1.5));
    }
}
