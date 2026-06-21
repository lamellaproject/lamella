#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! A WebAssembly 1.0 (MVP) binary-module encoder for the Lamella backend's WASM target.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// A WebAssembly number type -- the value types of WASM 1.0 (core spec 2.3.1). Reference types
/// (`funcref`/`externref`) and vector types arrive with later proposals and are out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValType {
    /// A 32-bit integer.
    I32,
    /// A 64-bit integer.
    I64,
    /// A 32-bit IEEE-754 float.
    F32,
    /// A 64-bit IEEE-754 float.
    F64,
}

impl ValType {
    /// The byte that encodes this value type in the binary format (core spec 5.3.1).
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            ValType::I32 => 0x7F,
            ValType::I64 => 0x7E,
            ValType::F32 => 0x7D,
            ValType::F64 => 0x7C,
        }
    }
}

/// A function type: the value types consumed and produced (core spec 2.3.3). WASM 1.0 admits at
/// most one result; the encoder accepts a vector so the multi-value proposal slots in unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FuncType {
    /// The parameter types, in order.
    pub params: Vec<ValType>,
    /// The result types, in order (zero or one in WASM 1.0).
    pub results: Vec<ValType>,
}

/// The result signature of a `block`/`loop`/`if` region (core spec 5.3.3). WASM 1.0 allows the
/// empty type or a single value type inline; a region producing more is a multi-value addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    /// No result: the region leaves the stack as it found it (encoded `0x40`).
    Empty,
    /// A single result value of the given type.
    Value(ValType),
}

impl BlockType {
    fn write(self, out: &mut Vec<u8>) {
        match self {
            BlockType::Empty => out.push(0x40),
            BlockType::Value(t) => out.push(t.byte()),
        }
    }
}

/// What an export refers to (core spec 5.5.10). The backend exports functions and the linear
/// memory; tables and globals are added when a lowering needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportKind {
    Func,
    Memory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Export {
    name: String,
    kind: ExportKind,
    index: u32,
}


/// Appends `value` as an unsigned LEB128 integer (used for indices, counts, sizes, and the
/// alignment/offset of a memory access).
pub fn write_var_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Appends `value` as a signed LEB128 integer (used for `i32.const`).
pub fn write_var_i32(out: &mut Vec<u8>, value: i32) {
    write_var_i64(out, value as i64);
}

/// Appends `value` as a signed LEB128 integer (used for `i64.const` and any 64-bit immediate).
pub fn write_var_i64(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value as u8) & 0x7F;
        value >>= 7;
        let sign_set = byte & 0x40 != 0;
        if (value == 0 && !sign_set) || (value == -1 && sign_set) {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// The immediate of a memory access (`align`, `offset`) -- the static alignment as a power of two
/// (its log2) and a byte offset added to the dynamic address (core spec 5.4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemArg {
    /// The alignment hint as a base-2 logarithm: 0 for a byte, 1 for 2 bytes, 2 for 4, 3 for 8.
    pub align_log2: u32,
    /// The constant byte offset added to the address operand.
    pub offset: u32,
}

impl MemArg {
    /// A naturally aligned access of `bytes` width at the given constant `offset` (`bytes` is a
    /// power of two: 1/2/4/8).
    #[must_use]
    pub fn new(bytes: u32, offset: u32) -> MemArg {
        MemArg {
            align_log2: bytes.trailing_zeros(),
            offset,
        }
    }

    fn write(self, out: &mut Vec<u8>) {
        write_var_u32(out, self.align_log2);
        write_var_u32(out, self.offset);
    }
}

/// A function body under construction: its declared locals and the byte stream of its
/// instructions. The lowering pushes instructions in order and closes the body with [`Func::end`]
/// (the trailing `end` every body requires). The operand stack is implicit -- the lowering is
/// responsible for emitting a well-typed sequence, exactly as a stack machine demands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Func {
    param_count: u32,
    locals: Vec<ValType>,
    code: Vec<u8>,
}

impl Func {
    /// Starts a body for a function whose type has `param_count` parameters. Parameters occupy
    /// local indices `0..param_count`; [`Func::add_local`] hands out the indices above them.
    #[must_use]
    pub fn new(param_count: u32) -> Func {
        Func {
            param_count,
            locals: Vec::new(),
            code: Vec::new(),
        }
    }

    /// Declares a new local of type `ty` and returns its index (which follows the parameters and
    /// any earlier locals).
    pub fn add_local(&mut self, ty: ValType) -> u32 {
        let index = self.param_count + self.locals.len() as u32;
        self.locals.push(ty);
        index
    }

    /// The raw instruction bytes emitted so far -- for tests and inspection.
    #[must_use]
    pub fn code(&self) -> &[u8] {
        &self.code
    }

    fn op(&mut self, opcode: u8) {
        self.code.push(opcode);
    }


    /// `i32.const value`.
    pub fn i32_const(&mut self, value: i32) {
        self.op(0x41);
        write_var_i32(&mut self.code, value);
    }
    /// `i64.const value`.
    pub fn i64_const(&mut self, value: i64) {
        self.op(0x42);
        write_var_i64(&mut self.code, value);
    }
    /// `f32.const` from the float's raw IEEE-754 bit pattern (four little-endian bytes).
    pub fn f32_const_bits(&mut self, bits: u32) {
        self.op(0x43);
        self.code.extend_from_slice(&bits.to_le_bytes());
    }
    /// `f64.const` from the double's raw IEEE-754 bit pattern (eight little-endian bytes).
    pub fn f64_const_bits(&mut self, bits: u64) {
        self.op(0x44);
        self.code.extend_from_slice(&bits.to_le_bytes());
    }


    /// `local.get index`.
    pub fn local_get(&mut self, index: u32) {
        self.op(0x20);
        write_var_u32(&mut self.code, index);
    }
    /// `local.set index`.
    pub fn local_set(&mut self, index: u32) {
        self.op(0x21);
        write_var_u32(&mut self.code, index);
    }
    /// `local.tee index` (set, but leave the value on the stack).
    pub fn local_tee(&mut self, index: u32) {
        self.op(0x22);
        write_var_u32(&mut self.code, index);
    }
    /// `global.get index`.
    pub fn global_get(&mut self, index: u32) {
        self.op(0x23);
        write_var_u32(&mut self.code, index);
    }
    /// `global.set index`.
    pub fn global_set(&mut self, index: u32) {
        self.op(0x24);
        write_var_u32(&mut self.code, index);
    }


    /// `unreachable` -- an unconditional trap.
    pub fn unreachable(&mut self) {
        self.op(0x00);
    }
    /// `nop`.
    pub fn nop(&mut self) {
        self.op(0x01);
    }
    /// `block bt` -- open a region branched out of by targeting its depth (a forward exit).
    pub fn block(&mut self, bt: BlockType) {
        self.op(0x02);
        bt.write(&mut self.code);
    }
    /// `loop bt` -- open a region whose label is its top (a branch to it is a back-edge).
    pub fn loop_(&mut self, bt: BlockType) {
        self.op(0x03);
        bt.write(&mut self.code);
    }
    /// `if bt` -- open a region entered when the popped condition is non-zero.
    pub fn if_(&mut self, bt: BlockType) {
        self.op(0x04);
        bt.write(&mut self.code);
    }
    /// `else` -- begin the alternative arm of the open `if`.
    pub fn else_(&mut self) {
        self.op(0x05);
    }
    /// `end` -- close a region (or the function body).
    pub fn end(&mut self) {
        self.op(0x0B);
    }
    /// `br depth` -- branch to the region `depth` levels out (0 is the innermost).
    pub fn br(&mut self, depth: u32) {
        self.op(0x0C);
        write_var_u32(&mut self.code, depth);
    }
    /// `br_if depth` -- branch to `depth` when the popped condition is non-zero.
    pub fn br_if(&mut self, depth: u32) {
        self.op(0x0D);
        write_var_u32(&mut self.code, depth);
    }
    /// `br_table targets default` -- index the popped value into `targets`, falling back to
    /// `default` (the jump-table branch; core spec 5.4.1).
    pub fn br_table(&mut self, targets: &[u32], default: u32) {
        self.op(0x0E);
        write_var_u32(&mut self.code, targets.len() as u32);
        for &t in targets {
            write_var_u32(&mut self.code, t);
        }
        write_var_u32(&mut self.code, default);
    }
    /// `return` -- return from the function with the values on the stack.
    pub fn return_(&mut self) {
        self.op(0x0F);
    }
    /// `call index` -- call the function at index `index` (imports first, then defined funcs).
    pub fn call(&mut self, index: u32) {
        self.op(0x10);
        write_var_u32(&mut self.code, index);
    }


    /// `drop` -- discard the top stack value. (Named `drop_` to avoid clashing with the `Drop`
    /// trait's method.)
    pub fn drop_(&mut self) {
        self.op(0x1A);
    }
    /// `select` -- pop a condition and two values, push the second if the condition is zero.
    pub fn select(&mut self) {
        self.op(0x1B);
    }


    /// `i32.load`.
    pub fn i32_load(&mut self, m: MemArg) {
        self.mem(0x28, m);
    }
    /// `i64.load`.
    pub fn i64_load(&mut self, m: MemArg) {
        self.mem(0x29, m);
    }
    /// `f32.load`.
    pub fn f32_load(&mut self, m: MemArg) {
        self.mem(0x2A, m);
    }
    /// `f64.load`.
    pub fn f64_load(&mut self, m: MemArg) {
        self.mem(0x2B, m);
    }
    /// `i32.load8_s` -- load a byte, sign-extended to i32.
    pub fn i32_load8_s(&mut self, m: MemArg) {
        self.mem(0x2C, m);
    }
    /// `i32.load8_u` -- load a byte, zero-extended to i32.
    pub fn i32_load8_u(&mut self, m: MemArg) {
        self.mem(0x2D, m);
    }
    /// `i32.load16_s` -- load a halfword, sign-extended to i32.
    pub fn i32_load16_s(&mut self, m: MemArg) {
        self.mem(0x2E, m);
    }
    /// `i32.load16_u` -- load a halfword, zero-extended to i32.
    pub fn i32_load16_u(&mut self, m: MemArg) {
        self.mem(0x2F, m);
    }
    /// `i64.load8_s`.
    pub fn i64_load8_s(&mut self, m: MemArg) {
        self.mem(0x30, m);
    }
    /// `i64.load8_u`.
    pub fn i64_load8_u(&mut self, m: MemArg) {
        self.mem(0x31, m);
    }
    /// `i64.load16_s`.
    pub fn i64_load16_s(&mut self, m: MemArg) {
        self.mem(0x32, m);
    }
    /// `i64.load16_u`.
    pub fn i64_load16_u(&mut self, m: MemArg) {
        self.mem(0x33, m);
    }
    /// `i64.load32_s`.
    pub fn i64_load32_s(&mut self, m: MemArg) {
        self.mem(0x34, m);
    }
    /// `i64.load32_u`.
    pub fn i64_load32_u(&mut self, m: MemArg) {
        self.mem(0x35, m);
    }
    /// `i32.store`.
    pub fn i32_store(&mut self, m: MemArg) {
        self.mem(0x36, m);
    }
    /// `i64.store`.
    pub fn i64_store(&mut self, m: MemArg) {
        self.mem(0x37, m);
    }
    /// `f32.store`.
    pub fn f32_store(&mut self, m: MemArg) {
        self.mem(0x38, m);
    }
    /// `f64.store`.
    pub fn f64_store(&mut self, m: MemArg) {
        self.mem(0x39, m);
    }
    /// `i32.store8` -- store the low byte.
    pub fn i32_store8(&mut self, m: MemArg) {
        self.mem(0x3A, m);
    }
    /// `i32.store16` -- store the low halfword.
    pub fn i32_store16(&mut self, m: MemArg) {
        self.mem(0x3B, m);
    }
    /// `i64.store8`.
    pub fn i64_store8(&mut self, m: MemArg) {
        self.mem(0x3C, m);
    }
    /// `i64.store16`.
    pub fn i64_store16(&mut self, m: MemArg) {
        self.mem(0x3D, m);
    }
    /// `i64.store32`.
    pub fn i64_store32(&mut self, m: MemArg) {
        self.mem(0x3E, m);
    }
    /// `memory.size` -- the current size in 64 KiB pages.
    pub fn memory_size(&mut self) {
        self.op(0x3F);
        self.code.push(0x00);
    }
    /// `memory.grow` -- grow by the popped page count, pushing the old size (or -1 on failure).
    pub fn memory_grow(&mut self) {
        self.op(0x40);
        self.code.push(0x00);
    }

    fn mem(&mut self, opcode: u8, m: MemArg) {
        self.op(opcode);
        m.write(&mut self.code);
    }


    /// `i32.eqz` (compare to zero).
    pub fn i32_eqz(&mut self) {
        self.op(0x45);
    }
    /// `i32.eq`.
    pub fn i32_eq(&mut self) {
        self.op(0x46);
    }
    /// `i32.ne`.
    pub fn i32_ne(&mut self) {
        self.op(0x47);
    }
    /// `i32.lt_s`.
    pub fn i32_lt_s(&mut self) {
        self.op(0x48);
    }
    /// `i32.lt_u`.
    pub fn i32_lt_u(&mut self) {
        self.op(0x49);
    }
    /// `i32.gt_s`.
    pub fn i32_gt_s(&mut self) {
        self.op(0x4A);
    }
    /// `i32.gt_u`.
    pub fn i32_gt_u(&mut self) {
        self.op(0x4B);
    }
    /// `i32.le_s`.
    pub fn i32_le_s(&mut self) {
        self.op(0x4C);
    }
    /// `i32.le_u`.
    pub fn i32_le_u(&mut self) {
        self.op(0x4D);
    }
    /// `i32.ge_s`.
    pub fn i32_ge_s(&mut self) {
        self.op(0x4E);
    }
    /// `i32.ge_u`.
    pub fn i32_ge_u(&mut self) {
        self.op(0x4F);
    }


    /// `i64.eqz`.
    pub fn i64_eqz(&mut self) {
        self.op(0x50);
    }
    /// `i64.eq`.
    pub fn i64_eq(&mut self) {
        self.op(0x51);
    }
    /// `i64.ne`.
    pub fn i64_ne(&mut self) {
        self.op(0x52);
    }
    /// `i64.lt_s`.
    pub fn i64_lt_s(&mut self) {
        self.op(0x53);
    }
    /// `i64.lt_u`.
    pub fn i64_lt_u(&mut self) {
        self.op(0x54);
    }
    /// `i64.gt_s`.
    pub fn i64_gt_s(&mut self) {
        self.op(0x55);
    }
    /// `i64.gt_u`.
    pub fn i64_gt_u(&mut self) {
        self.op(0x56);
    }
    /// `i64.le_s`.
    pub fn i64_le_s(&mut self) {
        self.op(0x57);
    }
    /// `i64.le_u`.
    pub fn i64_le_u(&mut self) {
        self.op(0x58);
    }
    /// `i64.ge_s`.
    pub fn i64_ge_s(&mut self) {
        self.op(0x59);
    }
    /// `i64.ge_u`.
    pub fn i64_ge_u(&mut self) {
        self.op(0x5A);
    }


    /// `i32.clz`.
    pub fn i32_clz(&mut self) {
        self.op(0x67);
    }
    /// `i32.ctz`.
    pub fn i32_ctz(&mut self) {
        self.op(0x68);
    }
    /// `i32.popcnt`.
    pub fn i32_popcnt(&mut self) {
        self.op(0x69);
    }
    /// `i32.add`.
    pub fn i32_add(&mut self) {
        self.op(0x6A);
    }
    /// `i32.sub`.
    pub fn i32_sub(&mut self) {
        self.op(0x6B);
    }
    /// `i32.mul`.
    pub fn i32_mul(&mut self) {
        self.op(0x6C);
    }
    /// `i32.div_s`.
    pub fn i32_div_s(&mut self) {
        self.op(0x6D);
    }
    /// `i32.div_u`.
    pub fn i32_div_u(&mut self) {
        self.op(0x6E);
    }
    /// `i32.rem_s`.
    pub fn i32_rem_s(&mut self) {
        self.op(0x6F);
    }
    /// `i32.rem_u`.
    pub fn i32_rem_u(&mut self) {
        self.op(0x70);
    }
    /// `i32.and`.
    pub fn i32_and(&mut self) {
        self.op(0x71);
    }
    /// `i32.or`.
    pub fn i32_or(&mut self) {
        self.op(0x72);
    }
    /// `i32.xor`.
    pub fn i32_xor(&mut self) {
        self.op(0x73);
    }
    /// `i32.shl`.
    pub fn i32_shl(&mut self) {
        self.op(0x74);
    }
    /// `i32.shr_s`.
    pub fn i32_shr_s(&mut self) {
        self.op(0x75);
    }
    /// `i32.shr_u`.
    pub fn i32_shr_u(&mut self) {
        self.op(0x76);
    }
    /// `i32.rotl`.
    pub fn i32_rotl(&mut self) {
        self.op(0x77);
    }
    /// `i32.rotr`.
    pub fn i32_rotr(&mut self) {
        self.op(0x78);
    }


    /// `i64.add`.
    pub fn i64_add(&mut self) {
        self.op(0x7C);
    }
    /// `i64.sub`.
    pub fn i64_sub(&mut self) {
        self.op(0x7D);
    }
    /// `i64.mul`.
    pub fn i64_mul(&mut self) {
        self.op(0x7E);
    }
    /// `i64.div_s`.
    pub fn i64_div_s(&mut self) {
        self.op(0x7F);
    }
    /// `i64.div_u`.
    pub fn i64_div_u(&mut self) {
        self.op(0x80);
    }
    /// `i64.rem_s`.
    pub fn i64_rem_s(&mut self) {
        self.op(0x81);
    }
    /// `i64.rem_u`.
    pub fn i64_rem_u(&mut self) {
        self.op(0x82);
    }
    /// `i64.and`.
    pub fn i64_and(&mut self) {
        self.op(0x83);
    }
    /// `i64.or`.
    pub fn i64_or(&mut self) {
        self.op(0x84);
    }
    /// `i64.xor`.
    pub fn i64_xor(&mut self) {
        self.op(0x85);
    }
    /// `i64.shl`.
    pub fn i64_shl(&mut self) {
        self.op(0x86);
    }
    /// `i64.shr_s`.
    pub fn i64_shr_s(&mut self) {
        self.op(0x87);
    }
    /// `i64.shr_u`.
    pub fn i64_shr_u(&mut self) {
        self.op(0x88);
    }
    /// `i64.rotl`.
    pub fn i64_rotl(&mut self) {
        self.op(0x89);
    }
    /// `i64.rotr`.
    pub fn i64_rotr(&mut self) {
        self.op(0x8A);
    }


    /// `i32.wrap_i64` -- truncate an i64 to its low 32 bits.
    pub fn i32_wrap_i64(&mut self) {
        self.op(0xA7);
    }
    /// `i64.extend_i32_s` -- sign-extend an i32 to i64.
    pub fn i64_extend_i32_s(&mut self) {
        self.op(0xAC);
    }
    /// `i64.extend_i32_u` -- zero-extend an i32 to i64.
    pub fn i64_extend_i32_u(&mut self) {
        self.op(0xAD);
    }


    /// `f32.eq`.
    pub fn f32_eq(&mut self) {
        self.op(0x5B);
    }
    /// `f32.ne`.
    pub fn f32_ne(&mut self) {
        self.op(0x5C);
    }
    /// `f32.lt`.
    pub fn f32_lt(&mut self) {
        self.op(0x5D);
    }
    /// `f32.gt`.
    pub fn f32_gt(&mut self) {
        self.op(0x5E);
    }
    /// `f32.le`.
    pub fn f32_le(&mut self) {
        self.op(0x5F);
    }
    /// `f32.ge`.
    pub fn f32_ge(&mut self) {
        self.op(0x60);
    }
    /// `f64.eq`.
    pub fn f64_eq(&mut self) {
        self.op(0x61);
    }
    /// `f64.ne`.
    pub fn f64_ne(&mut self) {
        self.op(0x62);
    }
    /// `f64.lt`.
    pub fn f64_lt(&mut self) {
        self.op(0x63);
    }
    /// `f64.gt`.
    pub fn f64_gt(&mut self) {
        self.op(0x64);
    }
    /// `f64.le`.
    pub fn f64_le(&mut self) {
        self.op(0x65);
    }
    /// `f64.ge`.
    pub fn f64_ge(&mut self) {
        self.op(0x66);
    }


    /// `f32.abs`.
    pub fn f32_abs(&mut self) {
        self.op(0x8B);
    }
    /// `f32.neg`.
    pub fn f32_neg(&mut self) {
        self.op(0x8C);
    }
    /// `f32.sqrt`.
    pub fn f32_sqrt(&mut self) {
        self.op(0x91);
    }
    /// `f32.add`.
    pub fn f32_add(&mut self) {
        self.op(0x92);
    }
    /// `f32.sub`.
    pub fn f32_sub(&mut self) {
        self.op(0x93);
    }
    /// `f32.mul`.
    pub fn f32_mul(&mut self) {
        self.op(0x94);
    }
    /// `f32.div`.
    pub fn f32_div(&mut self) {
        self.op(0x95);
    }
    /// `f64.abs`.
    pub fn f64_abs(&mut self) {
        self.op(0x99);
    }
    /// `f64.neg`.
    pub fn f64_neg(&mut self) {
        self.op(0x9A);
    }
    /// `f64.sqrt`.
    pub fn f64_sqrt(&mut self) {
        self.op(0x9F);
    }
    /// `f64.add`.
    pub fn f64_add(&mut self) {
        self.op(0xA0);
    }
    /// `f64.sub`.
    pub fn f64_sub(&mut self) {
        self.op(0xA1);
    }
    /// `f64.mul`.
    pub fn f64_mul(&mut self) {
        self.op(0xA2);
    }
    /// `f64.div`.
    pub fn f64_div(&mut self) {
        self.op(0xA3);
    }


    /// `i32.trunc_f32_s` -- truncate an f32 to a signed i32.
    pub fn i32_trunc_f32_s(&mut self) {
        self.op(0xA8);
    }
    /// `i32.trunc_f64_s` -- truncate an f64 to a signed i32.
    pub fn i32_trunc_f64_s(&mut self) {
        self.op(0xAA);
    }
    /// `f32.convert_i32_s` -- convert a signed i32 to an f32.
    pub fn f32_convert_i32_s(&mut self) {
        self.op(0xB2);
    }
    /// `f32.demote_f64` -- narrow an f64 to an f32.
    pub fn f32_demote_f64(&mut self) {
        self.op(0xB6);
    }
    /// `f64.convert_i32_s` -- convert a signed i32 to an f64.
    pub fn f64_convert_i32_s(&mut self) {
        self.op(0xB7);
    }
    /// `f64.promote_f32` -- widen an f32 to an f64.
    pub fn f64_promote_f32(&mut self) {
        self.op(0xBB);
    }

    /// Encodes this body as a code-section entry: `[size][locals vec][code]` (core spec 5.5.13).
    /// `code` must already be closed with a trailing [`Func::end`].
    fn encode_entry(&self, out: &mut Vec<u8>) {
        let mut body = Vec::new();
        let mut runs: Vec<(u32, ValType)> = Vec::new();
        for &ty in &self.locals {
            match runs.last_mut() {
                Some((count, t)) if *t == ty => *count += 1,
                _ => runs.push((1, ty)),
            }
        }
        write_var_u32(&mut body, runs.len() as u32);
        for (count, ty) in runs {
            write_var_u32(&mut body, count);
            body.push(ty.byte());
        }
        body.extend_from_slice(&self.code);
        write_var_u32(out, body.len() as u32);
        out.extend_from_slice(&body);
    }
}

/// A linear-memory definition: a minimum and optional maximum size in 64 KiB pages (core spec
/// 2.3.8 / 5.3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The minimum size in pages (the memory starts at this size).
    pub min_pages: u32,
    /// The maximum size in pages, or `None` for unbounded.
    pub max_pages: Option<u32>,
}

/// A module global: a typed, optionally mutable cell with a constant initializer (core spec 2.3.9 /
/// 5.5.9). Used by the backend for the bump-allocator heap pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Global {
    ty: ValType,
    mutable: bool,
    /// The constant initializer, interpreted at `ty` (the bit pattern for the float types).
    init: i64,
}

/// An active data segment: `bytes` copied into the single linear memory at `offset` when the module
/// is instantiated (core spec 2.5.13 / 5.5.14). Used for read-only string-literal blobs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DataSegment {
    offset: u32,
    bytes: Vec<u8>,
}

/// A WebAssembly module under construction: its function types, imported and defined functions,
/// an optional linear memory, exports, and the defined functions' bodies. [`Module::finish`]
/// serializes it to the binary format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Module {
    types: Vec<FuncType>,
    imported_funcs: Vec<(String, String, u32)>,
    defined_funcs: Vec<u32>,
    bodies: Vec<Func>,
    memory: Option<Limits>,
    globals: Vec<Global>,
    exports: Vec<Export>,
    data: Vec<DataSegment>,
}

impl Module {
    /// Creates an empty module.
    #[must_use]
    pub fn new() -> Module {
        Module::default()
    }

    /// Interns `ty` in the type section, returning its index (deduplicating equal types).
    pub fn add_type(&mut self, ty: FuncType) -> u32 {
        if let Some(i) = self.types.iter().position(|t| *t == ty) {
            return i as u32;
        }
        self.types.push(ty);
        (self.types.len() - 1) as u32
    }

    /// Declares an imported function `module.name` of type `type_index`, returning its function
    /// index. Imported functions occupy the low indices, so every import must be declared before
    /// the defined functions whose indices follow them.
    pub fn add_import_func(&mut self, module: &str, name: &str, type_index: u32) -> u32 {
        let index = self.imported_funcs.len() as u32;
        self.imported_funcs
            .push((String::from(module), String::from(name), type_index));
        index
    }

    /// Adds a defined function of type `type_index` with the given `body`, returning its function
    /// index (which follows the imported functions).
    pub fn add_function(&mut self, type_index: u32, body: Func) -> u32 {
        let index = self.imported_funcs.len() as u32 + self.defined_funcs.len() as u32;
        self.defined_funcs.push(type_index);
        self.bodies.push(body);
        index
    }

    /// Defines the module's single linear memory.
    pub fn set_memory(&mut self, limits: Limits) {
        self.memory = Some(limits);
    }

    /// Adds a global cell of type `ty` (mutable or not) with a constant initializer, returning its
    /// global index. For a float type, `init` is the IEEE-754 bit pattern.
    pub fn add_global(&mut self, ty: ValType, mutable: bool, init: i64) -> u32 {
        let index = self.globals.len() as u32;
        self.globals.push(Global { ty, mutable, init });
        index
    }

    /// Adds an active data segment: `bytes` are copied into linear memory at `offset` on
    /// instantiation. Used for read-only string-literal blobs.
    pub fn add_data(&mut self, offset: u32, bytes: Vec<u8>) {
        self.data.push(DataSegment { offset, bytes });
    }

    /// Exports the function at `func_index` under `name`.
    pub fn export_func(&mut self, name: &str, func_index: u32) {
        self.exports.push(Export {
            name: String::from(name),
            kind: ExportKind::Func,
            index: func_index,
        });
    }

    /// Exports the linear memory under `name`.
    pub fn export_memory(&mut self, name: &str) {
        self.exports.push(Export {
            name: String::from(name),
            kind: ExportKind::Memory,
            index: 0,
        });
    }

    /// Serializes the module to its WebAssembly binary form: the magic and version, then each
    /// non-empty section in ascending id order (core spec 5.5).
    #[must_use]
    pub fn finish(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0x00, 0x61, 0x73, 0x6D]);
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);

        if !self.types.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.types.len() as u32);
            for ty in &self.types {
                s.push(0x60);
                write_var_u32(&mut s, ty.params.len() as u32);
                for p in &ty.params {
                    s.push(p.byte());
                }
                write_var_u32(&mut s, ty.results.len() as u32);
                for r in &ty.results {
                    s.push(r.byte());
                }
            }
            write_section(&mut out, 1, &s);
        }

        if !self.imported_funcs.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.imported_funcs.len() as u32);
            for (module, name, type_index) in &self.imported_funcs {
                write_name(&mut s, module);
                write_name(&mut s, name);
                s.push(0x00);
                write_var_u32(&mut s, *type_index);
            }
            write_section(&mut out, 2, &s);
        }

        if !self.defined_funcs.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.defined_funcs.len() as u32);
            for &type_index in &self.defined_funcs {
                write_var_u32(&mut s, type_index);
            }
            write_section(&mut out, 3, &s);
        }

        if let Some(limits) = self.memory {
            let mut s = Vec::new();
            write_var_u32(&mut s, 1);
            write_limits(&mut s, limits);
            write_section(&mut out, 5, &s);
        }

        if !self.globals.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.globals.len() as u32);
            for g in &self.globals {
                s.push(g.ty.byte());
                s.push(u8::from(g.mutable));
                match g.ty {
                    ValType::I32 => {
                        s.push(0x41);
                        write_var_i32(&mut s, g.init as i32);
                    }
                    ValType::I64 => {
                        s.push(0x42);
                        write_var_i64(&mut s, g.init);
                    }
                    ValType::F32 => {
                        s.push(0x43);
                        s.extend_from_slice(&(g.init as u32).to_le_bytes());
                    }
                    ValType::F64 => {
                        s.push(0x44);
                        s.extend_from_slice(&(g.init as u64).to_le_bytes());
                    }
                }
                s.push(0x0B);
            }
            write_section(&mut out, 6, &s);
        }

        if !self.exports.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.exports.len() as u32);
            for export in &self.exports {
                write_name(&mut s, &export.name);
                let kind = match export.kind {
                    ExportKind::Func => 0x00,
                    ExportKind::Memory => 0x02,
                };
                s.push(kind);
                write_var_u32(&mut s, export.index);
            }
            write_section(&mut out, 7, &s);
        }

        if !self.bodies.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.bodies.len() as u32);
            for body in &self.bodies {
                body.encode_entry(&mut s);
            }
            write_section(&mut out, 10, &s);
        }

        if !self.data.is_empty() {
            let mut s = Vec::new();
            write_var_u32(&mut s, self.data.len() as u32);
            for seg in &self.data {
                s.push(0x00);
                s.push(0x41);
                write_var_i32(&mut s, seg.offset as i32);
                s.push(0x0B);
                write_var_u32(&mut s, seg.bytes.len() as u32);
                s.extend_from_slice(&seg.bytes);
            }
            write_section(&mut out, 11, &s);
        }

        out
    }
}

fn write_section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
    out.push(id);
    write_var_u32(out, contents.len() as u32);
    out.extend_from_slice(contents);
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    write_var_u32(out, name.len() as u32);
    out.extend_from_slice(name.as_bytes());
}

fn write_limits(out: &mut Vec<u8>, limits: Limits) {
    match limits.max_pages {
        None => {
            out.push(0x00);
            write_var_u32(out, limits.min_pages);
        }
        Some(max) => {
            out.push(0x01);
            write_var_u32(out, limits.min_pages);
            write_var_u32(out, max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn unsigned_leb128_matches_known_encodings() {
        let cases: &[(u32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (624_485, &[0xE5, 0x8E, 0x26]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            write_var_u32(&mut out, *value);
            assert_eq!(&out, expected, "u32 LEB128 of {value}");
        }
    }

    #[test]
    fn signed_leb128_matches_known_encodings() {
        let cases: &[(i64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (-1, &[0x7F]),
            (63, &[0x3F]),
            (64, &[0xC0, 0x00]),
            (-64, &[0x40]),
            (-65, &[0xBF, 0x7F]),
            (-123_456, &[0xC0, 0xBB, 0x78]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            write_var_i64(&mut out, *value);
            assert_eq!(&out, expected, "i64 LEB128 of {value}");
        }
    }

    /// The canonical first milestone: `fn main() -> i32 { 40 + 2 }`, exported as `main`. The
    /// expected bytes are hand-derived from the binary format so this pins the whole module
    /// layout (magic, version, type/function/export/code sections), not just that it is non-empty.
    #[test]
    fn encodes_the_add_module_byte_for_byte() {
        let mut module = Module::new();
        let ty = module.add_type(FuncType {
            params: vec![],
            results: vec![ValType::I32],
        });
        let mut body = Func::new(0);
        body.i32_const(40);
        body.i32_const(2);
        body.i32_add();
        body.end();
        let f = module.add_function(ty, body);
        module.export_func("main", f);
        let bytes = module.finish();

        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
            0x03, 0x02, 0x01, 0x00,
            0x07, 0x08, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x00, 0x00,
            0x0A, 0x09, 0x01, 0x07, 0x00, 0x41, 0x28, 0x41, 0x02, 0x6A, 0x0B,
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn locals_compress_into_runs() {
        let mut f = Func::new(1);
        assert_eq!(f.add_local(ValType::I32), 1);
        assert_eq!(f.add_local(ValType::I32), 2);
        assert_eq!(f.add_local(ValType::I64), 3);
        f.end();
        let mut out = Vec::new();
        f.encode_entry(&mut out);
        assert_eq!(&out[1..], &[0x02, 0x02, 0x7F, 0x01, 0x7E, 0x0B]);
    }
}
