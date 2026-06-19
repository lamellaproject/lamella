//! Runtime-native intrinsics: the Rust implementations a few BCL methods bind to.

use crate::interp::Vm;
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
pub fn console_write_line(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_line_empty(vm: &mut Vm, _args: &[Value]) -> Result<Option<Value>, Trap> {
    vm.write(&[NEWLINE]);
    Ok(None)
}

/// `System.Console.WriteLine(int)`: write an `int32` in decimal.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is not an `int32`.
pub fn console_write_line_int32(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_line_int64(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_line_bool(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_line_char(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_int32(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_int64(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_bool(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_char(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_line_double(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn console_write_double(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_concat(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_get_length(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_equals(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_get_chars(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_not_equals(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
    let left = string_opt(vm, args.first())?;
    let right = string_opt(vm, args.get(1))?;
    Ok(Some(Value::Int32(i32::from(left != right))))
}

/// `System.String.IsNullOrEmpty(string)`: true for a null or zero-length string.
///
/// # Errors
/// [`Trap::TypeMismatch`] if the argument is a non-string value.
pub fn string_is_null_or_empty(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_substring(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_substring_len(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
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
pub fn string_concat3(vm: &mut Vm, args: &[Value]) -> Result<Option<Value>, Trap> {
    let mut chars = string_arg_chars(vm, args.first())?;
    chars.extend_from_slice(&string_arg_chars(vm, args.get(1))?);
    chars.extend_from_slice(&string_arg_chars(vm, args.get(2))?);
    let reference = vm.heap_mut().alloc_string(&chars);
    Ok(Some(Value::Object(reference)))
}

/// `System.Object..ctor()`: the base constructor every constructor chains to. With
/// no object header to initialize here, it is a no-op (it still receives `this`).
///
/// # Errors
/// Never errors.
pub fn object_ctor(_vm: &mut Vm, _args: &[Value]) -> Result<Option<Value>, Trap> {
    Ok(None)
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

        let write_line = module.add_intrinsic(console_write_line, 1);
        let write_line_token = Token(0x0A00_0001);
        module.bind_token(write_line_token, write_line);

        let string_token = Token(0x7000_0001);
        let hello: Vec<u16> = "Hello, World".encode_utf16().collect();
        module.bind_string(string_token, &hello);

        let main = module.add_method(
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
        assert_eq!(console_write_line(&mut vm, &[Value::Null]), Ok(None));
        assert_eq!(vm.output_string(), "\n");
    }

    #[test]
    fn write_line_of_a_non_string_traps() {
        let mut vm = Vm::new();
        assert_eq!(
            console_write_line(&mut vm, &[Value::Int32(7)]),
            Err(Trap::TypeMismatch(Opcode::Call))
        );
    }
}
