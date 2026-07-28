//! Numeric-op evaluation: every [`NumOp`] with its exact spec semantics.

use crate::exec::{Trap, Value};
use crate::ops::NumOp;
use alloc::vec::Vec;

#[inline]
fn pop(stack: &mut Vec<Value>) -> Result<Value, Trap> {
    stack.pop().ok_or(Trap::ModuleInvalid)
}

#[inline]
fn pop_i32(stack: &mut Vec<Value>) -> Result<u32, Trap> {
    match pop(stack)? {
        Value::I32(v) => Ok(v),
        _ => Err(Trap::ModuleInvalid),
    }
}

#[inline]
fn pop_i64(stack: &mut Vec<Value>) -> Result<u64, Trap> {
    match pop(stack)? {
        Value::I64(v) => Ok(v),
        _ => Err(Trap::ModuleInvalid),
    }
}

#[inline]
fn pop_f32(stack: &mut Vec<Value>) -> Result<f32, Trap> {
    match pop(stack)? {
        Value::F32(bits) => Ok(f32::from_bits(bits)),
        _ => Err(Trap::ModuleInvalid),
    }
}

#[inline]
fn pop_f64(stack: &mut Vec<Value>) -> Result<f64, Trap> {
    match pop(stack)? {
        Value::F64(bits) => Ok(f64::from_bits(bits)),
        _ => Err(Trap::ModuleInvalid),
    }
}

/// Pushes without a cap check: evaluation never grows the stack (every op pops at least as
/// many values as it pushes), so the executor's per-push budget can't be exceeded here.
#[inline]
fn push(stack: &mut Vec<Value>, v: Value) {
    stack.push(v);
}

#[inline]
fn push_bool(stack: &mut Vec<Value>, b: bool) {
    push(stack, Value::I32(u32::from(b)));
}

macro_rules! bin_i32 {
    ($stack:expr, $f:expr) => {{
        let b = pop_i32($stack)?;
        let a = pop_i32($stack)?;
        push($stack, Value::I32($f(a, b)));
    }};
}

macro_rules! bin_i64 {
    ($stack:expr, $f:expr) => {{
        let b = pop_i64($stack)?;
        let a = pop_i64($stack)?;
        push($stack, Value::I64($f(a, b)));
    }};
}

macro_rules! cmp_i32 {
    ($stack:expr, $f:expr) => {{
        let b = pop_i32($stack)?;
        let a = pop_i32($stack)?;
        push_bool($stack, $f(a, b));
    }};
}

macro_rules! cmp_i64 {
    ($stack:expr, $f:expr) => {{
        let b = pop_i64($stack)?;
        let a = pop_i64($stack)?;
        push_bool($stack, $f(a, b));
    }};
}

macro_rules! cmp_f32 {
    ($stack:expr, $f:expr) => {{
        let b = pop_f32($stack)?;
        let a = pop_f32($stack)?;
        push_bool($stack, $f(a, b));
    }};
}

macro_rules! cmp_f64 {
    ($stack:expr, $f:expr) => {{
        let b = pop_f64($stack)?;
        let a = pop_f64($stack)?;
        push_bool($stack, $f(a, b));
    }};
}

macro_rules! un_f32 {
    ($stack:expr, $f:expr) => {{
        let a = pop_f32($stack)?;
        push($stack, Value::F32($f(a).to_bits()));
    }};
}

macro_rules! un_f64 {
    ($stack:expr, $f:expr) => {{
        let a = pop_f64($stack)?;
        push($stack, Value::F64($f(a).to_bits()));
    }};
}

macro_rules! bin_f32 {
    ($stack:expr, $f:expr) => {{
        let b = pop_f32($stack)?;
        let a = pop_f32($stack)?;
        push($stack, Value::F32($f(a, b).to_bits()));
    }};
}

macro_rules! bin_f64 {
    ($stack:expr, $f:expr) => {{
        let b = pop_f64($stack)?;
        let a = pop_f64($stack)?;
        push($stack, Value::F64($f(a, b).to_bits()));
    }};
}

/// wasm `min`: NaN propagates (canonical), and `-0 < +0` (the bit-or trick on equal zeros).
fn min32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::from_bits(0x7FC0_0000);
    }
    if a == b {
        return f32::from_bits(a.to_bits() | b.to_bits());
    }
    if a < b { a } else { b }
}

/// wasm `max`: NaN propagates (canonical), and `+0 > -0` (the bit-and trick on equal zeros).
fn max32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::from_bits(0x7FC0_0000);
    }
    if a == b {
        return f32::from_bits(a.to_bits() & b.to_bits());
    }
    if a > b { a } else { b }
}

fn min64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::from_bits(0x7FF8_0000_0000_0000);
    }
    if a == b {
        return f64::from_bits(a.to_bits() | b.to_bits());
    }
    if a < b { a } else { b }
}

fn max64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::from_bits(0x7FF8_0000_0000_0000);
    }
    if a == b {
        return f64::from_bits(a.to_bits() & b.to_bits());
    }
    if a > b { a } else { b }
}

/// A trapping float-to-int truncation: NaN and out-of-range are traps, never a silent
/// saturation (a bare Rust `as` would saturate -- the exact bug the design doc warns about).
/// `lo`/`hi` are the EXCLUSIVE bounds of the valid truncated range as exact floats.
macro_rules! trunc_checked {
    ($stack:expr, $pop:ident, $truncf:path, $lo:expr, $hi:expr, $cast:ty, $wrap:expr) => {{
        let x = $pop($stack)?;
        if x.is_nan() {
            return Err(Trap::InvalidConversion);
        }
        let t = $truncf(x);
        if t <= $lo || t >= $hi {
            return Err(Trap::IntOverflow);
        }
        push($stack, $wrap(t as $cast));
    }};
}

/// Evaluates one numeric op against the operand stack.
#[allow(clippy::too_many_lines)]
pub(crate) fn eval(op: NumOp, stack: &mut Vec<Value>) -> Result<(), Trap> {
    match op {
        NumOp::I32Eqz => {
            let a = pop_i32(stack)?;
            push_bool(stack, a == 0);
        }
        NumOp::I32Eq => cmp_i32!(stack, |a, b| a == b),
        NumOp::I32Ne => cmp_i32!(stack, |a, b| a != b),
        NumOp::I32LtS => cmp_i32!(stack, |a, b| (a as i32) < (b as i32)),
        NumOp::I32LtU => cmp_i32!(stack, |a, b| a < b),
        NumOp::I32GtS => cmp_i32!(stack, |a, b| (a as i32) > (b as i32)),
        NumOp::I32GtU => cmp_i32!(stack, |a, b| a > b),
        NumOp::I32LeS => cmp_i32!(stack, |a, b| (a as i32) <= (b as i32)),
        NumOp::I32LeU => cmp_i32!(stack, |a, b| a <= b),
        NumOp::I32GeS => cmp_i32!(stack, |a, b| (a as i32) >= (b as i32)),
        NumOp::I32GeU => cmp_i32!(stack, |a, b| a >= b),
        NumOp::I64Eqz => {
            let a = pop_i64(stack)?;
            push_bool(stack, a == 0);
        }
        NumOp::I64Eq => cmp_i64!(stack, |a, b| a == b),
        NumOp::I64Ne => cmp_i64!(stack, |a, b| a != b),
        NumOp::I64LtS => cmp_i64!(stack, |a, b| (a as i64) < (b as i64)),
        NumOp::I64LtU => cmp_i64!(stack, |a, b| a < b),
        NumOp::I64GtS => cmp_i64!(stack, |a, b| (a as i64) > (b as i64)),
        NumOp::I64GtU => cmp_i64!(stack, |a, b| a > b),
        NumOp::I64LeS => cmp_i64!(stack, |a, b| (a as i64) <= (b as i64)),
        NumOp::I64LeU => cmp_i64!(stack, |a, b| a <= b),
        NumOp::I64GeS => cmp_i64!(stack, |a, b| (a as i64) >= (b as i64)),
        NumOp::I64GeU => cmp_i64!(stack, |a, b| a >= b),
        NumOp::F32Eq => cmp_f32!(stack, |a, b| a == b),
        NumOp::F32Ne => cmp_f32!(stack, |a, b| a != b),
        NumOp::F32Lt => cmp_f32!(stack, |a, b| a < b),
        NumOp::F32Gt => cmp_f32!(stack, |a, b| a > b),
        NumOp::F32Le => cmp_f32!(stack, |a, b| a <= b),
        NumOp::F32Ge => cmp_f32!(stack, |a, b| a >= b),
        NumOp::F64Eq => cmp_f64!(stack, |a, b| a == b),
        NumOp::F64Ne => cmp_f64!(stack, |a, b| a != b),
        NumOp::F64Lt => cmp_f64!(stack, |a, b| a < b),
        NumOp::F64Gt => cmp_f64!(stack, |a, b| a > b),
        NumOp::F64Le => cmp_f64!(stack, |a, b| a <= b),
        NumOp::F64Ge => cmp_f64!(stack, |a, b| a >= b),
        NumOp::I32Clz => {
            let a = pop_i32(stack)?;
            push(stack, Value::I32(a.leading_zeros()));
        }
        NumOp::I32Ctz => {
            let a = pop_i32(stack)?;
            push(stack, Value::I32(a.trailing_zeros()));
        }
        NumOp::I32Popcnt => {
            let a = pop_i32(stack)?;
            push(stack, Value::I32(a.count_ones()));
        }
        NumOp::I32Add => bin_i32!(stack, u32::wrapping_add),
        NumOp::I32Sub => bin_i32!(stack, u32::wrapping_sub),
        NumOp::I32Mul => bin_i32!(stack, u32::wrapping_mul),
        NumOp::I32DivS => {
            let b = pop_i32(stack)? as i32;
            let a = pop_i32(stack)? as i32;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            if a == i32::MIN && b == -1 {
                return Err(Trap::IntOverflow);
            }
            push(stack, Value::I32(a.wrapping_div(b) as u32));
        }
        NumOp::I32DivU => {
            let b = pop_i32(stack)?;
            let a = pop_i32(stack)?;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            push(stack, Value::I32(a / b));
        }
        NumOp::I32RemS => {
            let b = pop_i32(stack)? as i32;
            let a = pop_i32(stack)? as i32;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            push(stack, Value::I32(a.wrapping_rem(b) as u32));
        }
        NumOp::I32RemU => {
            let b = pop_i32(stack)?;
            let a = pop_i32(stack)?;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            push(stack, Value::I32(a % b));
        }
        NumOp::I32And => bin_i32!(stack, |a, b| a & b),
        NumOp::I32Or => bin_i32!(stack, |a, b| a | b),
        NumOp::I32Xor => bin_i32!(stack, |a, b| a ^ b),
        NumOp::I32Shl => bin_i32!(stack, u32::wrapping_shl),
        NumOp::I32ShrS => bin_i32!(stack, |a, b| (a as i32).wrapping_shr(b) as u32),
        NumOp::I32ShrU => bin_i32!(stack, u32::wrapping_shr),
        NumOp::I32Rotl => bin_i32!(stack, u32::rotate_left),
        NumOp::I32Rotr => bin_i32!(stack, u32::rotate_right),
        NumOp::I64Clz => {
            let a = pop_i64(stack)?;
            push(stack, Value::I64(u64::from(a.leading_zeros())));
        }
        NumOp::I64Ctz => {
            let a = pop_i64(stack)?;
            push(stack, Value::I64(u64::from(a.trailing_zeros())));
        }
        NumOp::I64Popcnt => {
            let a = pop_i64(stack)?;
            push(stack, Value::I64(u64::from(a.count_ones())));
        }
        NumOp::I64Add => bin_i64!(stack, u64::wrapping_add),
        NumOp::I64Sub => bin_i64!(stack, u64::wrapping_sub),
        NumOp::I64Mul => bin_i64!(stack, u64::wrapping_mul),
        NumOp::I64DivS => {
            let b = pop_i64(stack)? as i64;
            let a = pop_i64(stack)? as i64;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            if a == i64::MIN && b == -1 {
                return Err(Trap::IntOverflow);
            }
            push(stack, Value::I64(a.wrapping_div(b) as u64));
        }
        NumOp::I64DivU => {
            let b = pop_i64(stack)?;
            let a = pop_i64(stack)?;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            push(stack, Value::I64(a / b));
        }
        NumOp::I64RemS => {
            let b = pop_i64(stack)? as i64;
            let a = pop_i64(stack)? as i64;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            push(stack, Value::I64(a.wrapping_rem(b) as u64));
        }
        NumOp::I64RemU => {
            let b = pop_i64(stack)?;
            let a = pop_i64(stack)?;
            if b == 0 {
                return Err(Trap::DivByZero);
            }
            push(stack, Value::I64(a % b));
        }
        NumOp::I64And => bin_i64!(stack, |a, b| a & b),
        NumOp::I64Or => bin_i64!(stack, |a, b| a | b),
        NumOp::I64Xor => bin_i64!(stack, |a, b| a ^ b),
        NumOp::I64Shl => bin_i64!(stack, |a: u64, b: u64| a.wrapping_shl(b as u32)),
        NumOp::I64ShrS => {
            bin_i64!(stack, |a: u64, b: u64| (a as i64).wrapping_shr(b as u32) as u64);
        }
        NumOp::I64ShrU => bin_i64!(stack, |a: u64, b: u64| a.wrapping_shr(b as u32)),
        NumOp::I64Rotl => bin_i64!(stack, |a: u64, b: u64| a.rotate_left(b as u32 & 63)),
        NumOp::I64Rotr => bin_i64!(stack, |a: u64, b: u64| a.rotate_right(b as u32 & 63)),
        NumOp::F32Abs => un_f32!(stack, libm::fabsf),
        NumOp::F32Neg => un_f32!(stack, |a: f32| -a),
        NumOp::F32Ceil => un_f32!(stack, libm::ceilf),
        NumOp::F32Floor => un_f32!(stack, libm::floorf),
        NumOp::F32Trunc => un_f32!(stack, libm::truncf),
        NumOp::F32Nearest => un_f32!(stack, libm::rintf),
        NumOp::F32Sqrt => un_f32!(stack, libm::sqrtf),
        NumOp::F32Add => bin_f32!(stack, |a, b| a + b),
        NumOp::F32Sub => bin_f32!(stack, |a, b| a - b),
        NumOp::F32Mul => bin_f32!(stack, |a, b| a * b),
        NumOp::F32Div => bin_f32!(stack, |a, b| a / b),
        NumOp::F32Min => bin_f32!(stack, min32),
        NumOp::F32Max => bin_f32!(stack, max32),
        NumOp::F32Copysign => bin_f32!(stack, libm::copysignf),
        NumOp::F64Abs => un_f64!(stack, libm::fabs),
        NumOp::F64Neg => un_f64!(stack, |a: f64| -a),
        NumOp::F64Ceil => un_f64!(stack, libm::ceil),
        NumOp::F64Floor => un_f64!(stack, libm::floor),
        NumOp::F64Trunc => un_f64!(stack, libm::trunc),
        NumOp::F64Nearest => un_f64!(stack, libm::rint),
        NumOp::F64Sqrt => un_f64!(stack, libm::sqrt),
        NumOp::F64Add => bin_f64!(stack, |a, b| a + b),
        NumOp::F64Sub => bin_f64!(stack, |a, b| a - b),
        NumOp::F64Mul => bin_f64!(stack, |a, b| a * b),
        NumOp::F64Div => bin_f64!(stack, |a, b| a / b),
        NumOp::F64Min => bin_f64!(stack, min64),
        NumOp::F64Max => bin_f64!(stack, max64),
        NumOp::F64Copysign => bin_f64!(stack, libm::copysign),
        NumOp::I32WrapI64 => {
            let a = pop_i64(stack)?;
            push(stack, Value::I32(a as u32));
        }
        NumOp::I32TruncF32S => trunc_checked!(
            stack,
            pop_f32,
            libm::truncf,
            -2_147_483_904.0f32,
            2_147_483_648.0f32,
            i32,
            |v: i32| Value::I32(v as u32)
        ),
        NumOp::I32TruncF32U => trunc_checked!(
            stack,
            pop_f32,
            libm::truncf,
            -1.0f32,
            4_294_967_296.0f32,
            u32,
            Value::I32
        ),
        NumOp::I32TruncF64S => trunc_checked!(
            stack,
            pop_f64,
            libm::trunc,
            -2_147_483_649.0f64,
            2_147_483_648.0f64,
            i32,
            |v: i32| Value::I32(v as u32)
        ),
        NumOp::I32TruncF64U => trunc_checked!(
            stack,
            pop_f64,
            libm::trunc,
            -1.0f64,
            4_294_967_296.0f64,
            u32,
            Value::I32
        ),
        NumOp::I64ExtendI32S => {
            let a = pop_i32(stack)?;
            push(stack, Value::I64(a as i32 as i64 as u64));
        }
        NumOp::I64ExtendI32U => {
            let a = pop_i32(stack)?;
            push(stack, Value::I64(u64::from(a)));
        }
        NumOp::I64TruncF32S => trunc_checked!(
            stack,
            pop_f32,
            libm::truncf,
            -9_223_373_136_366_403_584.0f32,
            9_223_372_036_854_775_808.0f32,
            i64,
            |v: i64| Value::I64(v as u64)
        ),
        NumOp::I64TruncF32U => trunc_checked!(
            stack,
            pop_f32,
            libm::truncf,
            -1.0f32,
            18_446_744_073_709_551_616.0f32,
            u64,
            Value::I64
        ),
        NumOp::I64TruncF64S => trunc_checked!(
            stack,
            pop_f64,
            libm::trunc,
            -9_223_372_036_854_777_856.0f64,
            9_223_372_036_854_775_808.0f64,
            i64,
            |v: i64| Value::I64(v as u64)
        ),
        NumOp::I64TruncF64U => trunc_checked!(
            stack,
            pop_f64,
            libm::trunc,
            -1.0f64,
            18_446_744_073_709_551_616.0f64,
            u64,
            Value::I64
        ),
        NumOp::F32ConvertI32S => {
            let a = pop_i32(stack)?;
            push(stack, Value::F32((a as i32 as f32).to_bits()));
        }
        NumOp::F32ConvertI32U => {
            let a = pop_i32(stack)?;
            push(stack, Value::F32((a as f32).to_bits()));
        }
        NumOp::F32ConvertI64S => {
            let a = pop_i64(stack)?;
            push(stack, Value::F32((a as i64 as f32).to_bits()));
        }
        NumOp::F32ConvertI64U => {
            let a = pop_i64(stack)?;
            push(stack, Value::F32((a as f32).to_bits()));
        }
        NumOp::F32DemoteF64 => {
            let a = pop_f64(stack)?;
            push(stack, Value::F32((a as f32).to_bits()));
        }
        NumOp::F64ConvertI32S => {
            let a = pop_i32(stack)?;
            push(stack, Value::F64(f64::from(a as i32).to_bits()));
        }
        NumOp::F64ConvertI32U => {
            let a = pop_i32(stack)?;
            push(stack, Value::F64(f64::from(a).to_bits()));
        }
        NumOp::F64ConvertI64S => {
            let a = pop_i64(stack)?;
            push(stack, Value::F64((a as i64 as f64).to_bits()));
        }
        NumOp::F64ConvertI64U => {
            let a = pop_i64(stack)?;
            push(stack, Value::F64((a as f64).to_bits()));
        }
        NumOp::F64PromoteF32 => {
            let a = pop_f32(stack)?;
            push(stack, Value::F64(f64::from(a).to_bits()));
        }
        NumOp::I32ReinterpretF32 => {
            let v = match pop(stack)? {
                Value::F32(bits) => bits,
                _ => return Err(Trap::ModuleInvalid),
            };
            push(stack, Value::I32(v));
        }
        NumOp::I64ReinterpretF64 => {
            let v = match pop(stack)? {
                Value::F64(bits) => bits,
                _ => return Err(Trap::ModuleInvalid),
            };
            push(stack, Value::I64(v));
        }
        NumOp::F32ReinterpretI32 => {
            let v = pop_i32(stack)?;
            push(stack, Value::F32(v));
        }
        NumOp::F64ReinterpretI64 => {
            let v = pop_i64(stack)?;
            push(stack, Value::F64(v));
        }
        NumOp::I32Extend8S => {
            let a = pop_i32(stack)?;
            push(stack, Value::I32(a as u8 as i8 as i32 as u32));
        }
        NumOp::I32Extend16S => {
            let a = pop_i32(stack)?;
            push(stack, Value::I32(a as u16 as i16 as i32 as u32));
        }
        NumOp::I64Extend8S => {
            let a = pop_i64(stack)?;
            push(stack, Value::I64(a as u8 as i8 as i64 as u64));
        }
        NumOp::I64Extend16S => {
            let a = pop_i64(stack)?;
            push(stack, Value::I64(a as u16 as i16 as i64 as u64));
        }
        NumOp::I64Extend32S => {
            let a = pop_i64(stack)?;
            push(stack, Value::I64(a as u32 as i32 as i64 as u64));
        }
        NumOp::I32TruncSatF32S => {
            let a = pop_f32(stack)?;
            push(stack, Value::I32(a as i32 as u32));
        }
        NumOp::I32TruncSatF32U => {
            let a = pop_f32(stack)?;
            push(stack, Value::I32(a as u32));
        }
        NumOp::I32TruncSatF64S => {
            let a = pop_f64(stack)?;
            push(stack, Value::I32(a as i32 as u32));
        }
        NumOp::I32TruncSatF64U => {
            let a = pop_f64(stack)?;
            push(stack, Value::I32(a as u32));
        }
        NumOp::I64TruncSatF32S => {
            let a = pop_f32(stack)?;
            push(stack, Value::I64(a as i64 as u64));
        }
        NumOp::I64TruncSatF32U => {
            let a = pop_f32(stack)?;
            push(stack, Value::I64(a as u64));
        }
        NumOp::I64TruncSatF64S => {
            let a = pop_f64(stack)?;
            push(stack, Value::I64(a as i64 as u64));
        }
        NumOp::I64TruncSatF64U => {
            let a = pop_f64(stack)?;
            push(stack, Value::I64(a as u64));
        }
    }
    Ok(())
}
