//! Runtime-native intrinsics: the Rust implementations a few BCL methods bind to.

use crate::interp::Vm;
use crate::module::Module;
use crate::object::{Object, decode_string};
use crate::trap::Trap;
use crate::value::Value;
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
/// [`Trap::TypeMismatch`] for bad argument types; [`Trap::IndexOutOfRange`] if
/// `startIndex` is negative or past the end.
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
        .ok_or(Trap::IndexOutOfRange(start))?;
    let reference = vm.heap_mut().alloc_string(&chars[start..]);
    Ok(Some(Value::Object(reference)))
}

/// `System.String.Substring(int, int)`: `length` units from `startIndex`.
///
/// # Errors
/// [`Trap::TypeMismatch`] for bad argument types; [`Trap::IndexOutOfRange`] if the
/// range falls outside the string.
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
    let start = usize::try_from(start).map_err(|_| Trap::IndexOutOfRange(start))?;
    let count = usize::try_from(length).map_err(|_| Trap::IndexOutOfRange(length))?;
    let end = start
        .checked_add(count)
        .filter(|&end| end <= chars.len())
        .ok_or(Trap::IndexOutOfRange(length))?;
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

/// The additional .NET Micro Framework v4.4 BCL surface, beyond the ECMA-335 Kernel
/// Profile. Gated by `NETMFv4_4` so a Kernel-only build omits it entirely; its public
/// intrinsics are re-exported below so `crate::intrinsics::*` paths are unchanged.
#[cfg(feature = "NETMFv4_4")]
mod netmf {
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

    /// Appends `units` to the string builder at `reference`.
    fn string_builder_extend(vm: &mut Vm, reference: ObjectRef, units: &[u16]) -> Result<(), Trap> {
        match vm.heap_mut().string_builder_buf_mut(reference) {
            Some(buffer) => {
                buffer.extend_from_slice(units);
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
pub use netmf::*;

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
            Some(Object::StringBuilder(buffer)) => String::from_utf16_lossy(buffer),
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
/// constant of that enum), otherwise the underlying value's text. The boxed `type_token`
/// already carries its owning assembly (folded in at the `box` site), so the enum maps are
/// queried with assembly 0.
fn boxed_text(module: &Module, type_token: u32, value: &Value) -> String {
    if let Some(integer) = enum_underlying(value) {
        if let Some(name) = module.enum_value_name(0, type_token, integer) {
            return String::from(name);
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
                .enum_value_by_name(0, token, name, ignore_case)
                .or_else(|| name.parse::<i64>().ok())
        })
        .ok_or(Trap::InvalidArgument)?;
    let boxed_value = if module.enum_is_wide(0, token) {
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
                .enum_value_by_name(
                    0,
                    token,
                    &String::from_utf16_lossy(&decode_string(chars)),
                    false,
                )
                .is_some(),
            Some(Object::Boxed { value, .. }) => enum_underlying(value)
                .is_some_and(|n| module.enum_value_name(0, token, n).is_some()),
            _ => false,
        },
        Some(other) => {
            enum_underlying(other).is_some_and(|n| module.enum_value_name(0, token, n).is_some())
        }
        None => false,
    };
    Ok(Some(Value::Int32(i32::from(defined))))
}

/// The type token a `RuntimeTypeHandle` / `Type` argument carries (it is modeled as a
/// native-int handle holding the token).
fn type_handle_token(arg: Option<&Value>) -> u32 {
    match arg {
        Some(Value::NativeInt(handle)) => *handle as u32,
        _ => 0,
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
    _module: &Module,
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
        value => Value::Object(vm.heap_mut().alloc_boxed(0, value)),
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
}
