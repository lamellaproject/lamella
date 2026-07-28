//! The binary decoder: bytes to a [`Module`] with every control target resolved.

use crate::ops::{LabelKind, NumOp, Op};
use crate::{
    ConstExpr, DataSegment, DecodeError, DecodeErrorKind, ElemSegment, Export, ExportKind,
    FuncBody, FuncType, Global, Import, ImportKind, Limits, MemoryType, Module, ValType,
};
use alloc::string::String;
use alloc::vec::Vec;

/// The deepest allowed structured-control nesting inside one function body.
const MAX_CONTROL_DEPTH: usize = 1024;
/// The most locals (parameters included) one function may declare.
const MAX_LOCALS: u64 = 50_000;
/// The op-index placeholder a decode bug would leave behind; asserted absent before return.
const PATCH: u32 = u32::MAX;

/// A bounds-checked byte reader positioned inside the module image; `base + pos` is the
/// absolute offset every error carries.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    base: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], base: usize) -> Reader<'a> {
        Reader { bytes, pos: 0, base }
    }

    fn err(&self, kind: DecodeErrorKind) -> DecodeError {
        DecodeError { offset: self.base + self.pos, kind }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        let b = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| self.err(DecodeErrorKind::UnexpectedEof))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(self.err(DecodeErrorKind::UnexpectedEof));
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Splits off the next `n` bytes as their own reader (a section or code body), so size
    /// accounting is by construction: the parent continues after them regardless of how far
    /// the child read.
    fn sub(&mut self, n: usize) -> Result<Reader<'a>, DecodeError> {
        let base = self.base + self.pos;
        Ok(Reader::new(self.take(n)?, base))
    }

    /// An unsigned LEB128 u32: at most five bytes, value bits past 31 rejected, non-canonical
    /// (padded) encodings accepted.
    fn u32_leb(&mut self) -> Result<u32, DecodeError> {
        let mut result: u32 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift == 28 && (byte & 0x70) != 0 {
                return Err(self.err(DecodeErrorKind::LebOverflow));
            }
            result |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift > 28 {
                return Err(self.err(DecodeErrorKind::LebOverflow));
            }
        }
    }

    /// A signed LEB128 i32 (`i32.const`): at most five bytes, the final byte's spare bits
    /// must match the sign.
    fn i32_leb(&mut self) -> Result<i32, DecodeError> {
        let mut result: i32 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift == 28 {
                let spare = byte & 0x70;
                let sign = byte & 0x08 != 0;
                if (sign && spare != 0x70) || (!sign && spare != 0) || byte & 0x80 != 0 {
                    return Err(self.err(DecodeErrorKind::LebOverflow));
                }
                result |= (i32::from(byte & 0x0F)) << 28;
                return Ok(result);
            }
            result |= i32::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    result |= -1i32 << shift;
                }
                return Ok(result);
            }
        }
    }

    /// A signed LEB128 i64 (`i64.const`): at most ten bytes, final-byte spare bits
    /// sign-checked.
    fn i64_leb(&mut self) -> Result<i64, DecodeError> {
        let mut result: i64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift == 63 {
                let spare = byte & 0x7E;
                let sign = byte & 0x01 != 0;
                if (sign && spare != 0x7E) || (!sign && spare != 0) || byte & 0x80 != 0 {
                    return Err(self.err(DecodeErrorKind::LebOverflow));
                }
                result |= i64::from(byte & 0x01) << 63;
                return Ok(result);
            }
            result |= i64::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
    }

    /// A vector count, sanity-bounded by the bytes left (every element takes at least one).
    fn count(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        let n = self.u32_leb()?;
        if n as usize > self.remaining() {
            return Err(self.err(DecodeErrorKind::TooMany(what)));
        }
        Ok(n)
    }

    /// A UTF-8 name (core spec 5.2.4).
    fn name(&mut self) -> Result<String, DecodeError> {
        let len = self.count("name bytes")? as usize;
        let at = self.pos;
        let bytes = self.take(len)?;
        match core::str::from_utf8(bytes) {
            Ok(s) => Ok(String::from(s)),
            Err(_) => Err(DecodeError { offset: self.base + at, kind: DecodeErrorKind::BadUtf8 }),
        }
    }

    fn val_type(&mut self) -> Result<ValType, DecodeError> {
        let at = self.pos;
        let b = self.u8()?;
        val_type_byte(b).ok_or(DecodeError {
            offset: self.base + at,
            kind: DecodeErrorKind::BadValType(b),
        })
    }

    /// A limits record (core spec 5.3.6): flags 0x00 (min) or 0x01 (min+max); anything else
    /// (shared memories) is out of scope.
    fn limits(&mut self) -> Result<Limits, DecodeError> {
        let flags = self.u8()?;
        match flags {
            0x00 => Ok(Limits { min: self.u32_leb()?, max: None }),
            0x01 => {
                let min = self.u32_leb()?;
                let max = self.u32_leb()?;
                Ok(Limits { min, max: Some(max) })
            }
            _ => Err(self.err(DecodeErrorKind::BadFlags("limits"))),
        }
    }

    /// A memory type: the limits record plus the `custom-page-sizes` extension (phase-3
    /// proposal) -- flags BIT 3 (0x08) marks a trailing u32 log2(page size), whose only
    /// legal values today are 0 (1-byte pages) and 16 (the classic 64 KiB). The structure
    /// always decodes; whether a non-default page size is ADMITTED is the instantiation
    /// gate's call (the experiment knob), same split as import kinds.
    fn memory_type(&mut self) -> Result<MemoryType, DecodeError> {
        let flags = self.u8()?;
        if flags & !0x09 != 0 {
            return Err(self.err(DecodeErrorKind::BadFlags("memory limits")));
        }
        let min = self.u32_leb()?;
        let max = if flags & 0x01 != 0 { Some(self.u32_leb()?) } else { None };
        let page_size_log2 = if flags & 0x08 != 0 {
            match self.u32_leb()? {
                0 => 0,
                16 => 16,
                _ => return Err(self.err(DecodeErrorKind::UnsupportedFeature("page size"))),
            }
        } else {
            16
        };
        Ok(MemoryType { limits: Limits { min, max }, page_size_log2 })
    }

    /// An MVP constant expression: one const-shaped instruction, then `end`.
    fn const_expr(&mut self) -> Result<ConstExpr, DecodeError> {
        let expr = match self.u8()? {
            0x41 => ConstExpr::I32(self.i32_leb()? as u32),
            0x42 => ConstExpr::I64(self.i64_leb()? as u64),
            0x43 => ConstExpr::F32(u32::from_le_bytes(fixed4(self.take(4)?))),
            0x44 => ConstExpr::F64(u64::from_le_bytes(fixed8(self.take(8)?))),
            0x23 => ConstExpr::GlobalGet(self.u32_leb()?),
            _ => return Err(self.err(DecodeErrorKind::BadConstExpr)),
        };
        if self.u8()? != 0x0B {
            return Err(self.err(DecodeErrorKind::BadConstExpr));
        }
        Ok(expr)
    }
}

fn val_type_byte(b: u8) -> Option<ValType> {
    match b {
        0x7F => Some(ValType::I32),
        0x7E => Some(ValType::I64),
        0x7D => Some(ValType::F32),
        0x7C => Some(ValType::F64),
        _ => None,
    }
}

fn fixed4(s: &[u8]) -> [u8; 4] {
    let mut a = [0u8; 4];
    a.copy_from_slice(s);
    a
}

fn fixed8(s: &[u8]) -> [u8; 8] {
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    a
}

/// Decodes a binary module. The result has every structured-control target resolved and every
/// index it was cheap to check verified; what remains unverified is exactly the crate's
/// stated validation-lite contract -- a type error a full validator would reject up front
/// instead surfaces as a [`Trap`](crate::Trap) at run time, never as undefined behavior.
pub fn decode(bytes: &[u8]) -> Result<Module, DecodeError> {
    let mut r = Reader::new(bytes, 0);
    if r.take(4)? != b"\0asm" {
        return Err(DecodeError { offset: 0, kind: DecodeErrorKind::BadMagic });
    }
    if r.take(4)? != [1, 0, 0, 0] {
        return Err(DecodeError { offset: 4, kind: DecodeErrorKind::BadVersion });
    }

    let mut module = Module::default();
    let mut last_id = 0u8;
    while r.remaining() > 0 {
        let at = r.pos;
        let id = r.u8()?;
        let size = r.u32_leb()? as usize;
        let mut body = r.sub(size)?;
        if id == 0 {
            continue;
        }
        if id > 11 {
            return Err(DecodeError { offset: at, kind: DecodeErrorKind::BadSectionId(id) });
        }
        if id <= last_id {
            return Err(DecodeError { offset: at, kind: DecodeErrorKind::SectionOrder(id) });
        }
        last_id = id;
        match id {
            1 => decode_types(&mut body, &mut module)?,
            2 => decode_imports(&mut body, &mut module)?,
            3 => decode_functions(&mut body, &mut module)?,
            4 => decode_tables(&mut body, &mut module)?,
            5 => decode_memories(&mut body, &mut module)?,
            6 => decode_globals(&mut body, &mut module)?,
            7 => decode_exports(&mut body, &mut module)?,
            8 => {
                let index = body.u32_leb()?;
                check_func_index(&body, &module, index)?;
                module.start = Some(index);
            }
            9 => decode_elements(&mut body, &mut module)?,
            10 => decode_code(&mut body, &mut module)?,
            11 => decode_data(&mut body, &mut module)?,
            _ => unreachable!(),
        }
        if body.remaining() != 0 {
            return Err(body.err(DecodeErrorKind::SectionSize));
        }
    }
    if module.functions.len() != module.code.len() {
        return Err(DecodeError {
            offset: bytes.len(),
            kind: DecodeErrorKind::FuncCodeMismatch,
        });
    }
    Ok(module)
}

fn decode_types(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("types")?;
    for _ in 0..count {
        if r.u8()? != 0x60 {
            return Err(r.err(DecodeErrorKind::BadFlags("function type tag")));
        }
        let mut ty = FuncType::default();
        for _ in 0..r.count("parameters")? {
            ty.params.push(r.val_type()?);
        }
        let results = r.count("results")?;
        if results > 1 {
            return Err(r.err(DecodeErrorKind::UnsupportedFeature("multi-value results")));
        }
        for _ in 0..results {
            ty.results.push(r.val_type()?);
        }
        module.types.push(ty);
    }
    Ok(())
}

fn decode_imports(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("imports")?;
    for _ in 0..count {
        let module_name = r.name()?;
        let name = r.name()?;
        let kind = match r.u8()? {
            0x00 => {
                let type_index = r.u32_leb()?;
                if type_index as usize >= module.types.len() {
                    return Err(r.err(DecodeErrorKind::IndexOutOfRange("type")));
                }
                ImportKind::Func { type_index }
            }
            0x01 => {
                if r.u8()? != 0x70 {
                    return Err(r.err(DecodeErrorKind::BadFlags("table element type")));
                }
                r.limits()?;
                ImportKind::Table
            }
            0x02 => {
                r.memory_type()?;
                ImportKind::Memory
            }
            0x03 => {
                r.val_type()?;
                let mutable = r.u8()?;
                if mutable > 1 {
                    return Err(r.err(DecodeErrorKind::BadFlags("global mutability")));
                }
                ImportKind::Global
            }
            _ => return Err(r.err(DecodeErrorKind::BadFlags("import kind"))),
        };
        module.imports.push(Import { module: module_name, name, kind });
    }
    Ok(())
}

fn decode_functions(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("functions")?;
    for _ in 0..count {
        let type_index = r.u32_leb()?;
        if type_index as usize >= module.types.len() {
            return Err(r.err(DecodeErrorKind::IndexOutOfRange("type")));
        }
        module.functions.push(type_index);
    }
    Ok(())
}

fn decode_tables(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("tables")?;
    if count > 1 {
        return Err(r.err(DecodeErrorKind::UnsupportedFeature("multiple tables")));
    }
    for _ in 0..count {
        if r.u8()? != 0x70 {
            return Err(r.err(DecodeErrorKind::BadFlags("table element type")));
        }
        module.table = Some(r.limits()?);
    }
    Ok(())
}

fn decode_memories(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("memories")?;
    if count > 1 {
        return Err(r.err(DecodeErrorKind::UnsupportedFeature("multiple memories")));
    }
    for _ in 0..count {
        module.memory = Some(r.memory_type()?);
    }
    Ok(())
}

fn decode_globals(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("globals")?;
    for _ in 0..count {
        let ty = r.val_type()?;
        let mutable = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(r.err(DecodeErrorKind::BadFlags("global mutability"))),
        };
        let init = r.const_expr()?;
        module.globals.push(Global { ty, mutable, init });
    }
    Ok(())
}

fn imported_global_count(module: &Module) -> u32 {
    module.imports.iter().filter(|i| matches!(i.kind, ImportKind::Global)).count() as u32
}

fn total_func_count(module: &Module) -> u32 {
    module.imported_func_count() + module.functions.len() as u32
}

fn check_func_index(r: &Reader<'_>, module: &Module, index: u32) -> Result<(), DecodeError> {
    if index < total_func_count(module) {
        Ok(())
    } else {
        Err(r.err(DecodeErrorKind::IndexOutOfRange("function")))
    }
}

fn decode_exports(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("exports")?;
    for _ in 0..count {
        let name = r.name()?;
        let kind_byte = r.u8()?;
        let index = r.u32_leb()?;
        let kind = match kind_byte {
            0x00 => {
                check_func_index(r, module, index)?;
                ExportKind::Func(index)
            }
            0x01 => ExportKind::Table(index),
            0x02 => ExportKind::Memory(index),
            0x03 => {
                let total = imported_global_count(module) + module.globals.len() as u32;
                if index >= total {
                    return Err(r.err(DecodeErrorKind::IndexOutOfRange("global")));
                }
                ExportKind::Global(index)
            }
            _ => return Err(r.err(DecodeErrorKind::BadFlags("export kind"))),
        };
        module.exports.push(Export { name, kind });
    }
    Ok(())
}

fn decode_elements(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("element segments")?;
    for _ in 0..count {
        if r.u32_leb()? != 0 {
            return Err(r.err(DecodeErrorKind::UnsupportedFeature("element segment flags")));
        }
        let offset = r.const_expr()?;
        let mut funcs = Vec::new();
        for _ in 0..r.count("element entries")? {
            let index = r.u32_leb()?;
            check_func_index(r, module, index)?;
            funcs.push(index);
        }
        module.elements.push(ElemSegment { offset, funcs });
    }
    Ok(())
}

fn decode_data(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("data segments")?;
    for _ in 0..count {
        if r.u32_leb()? != 0 {
            return Err(r.err(DecodeErrorKind::UnsupportedFeature("data segment flags")));
        }
        let offset = r.const_expr()?;
        let len = r.count("data bytes")? as usize;
        let bytes = Vec::from(r.take(len)?);
        module.data.push(DataSegment { offset, bytes });
    }
    Ok(())
}

fn decode_code(r: &mut Reader<'_>, module: &mut Module) -> Result<(), DecodeError> {
    let count = r.count("code bodies")?;
    if count as usize != module.functions.len() {
        return Err(r.err(DecodeErrorKind::FuncCodeMismatch));
    }
    let imported = module.imported_func_count();
    for i in 0..count {
        let size = r.u32_leb()? as usize;
        let mut body = r.sub(size)?;
        let type_index = module.functions[i as usize];
        let func_type = &module.types[type_index as usize];
        let param_count = func_type.params.len() as u64;
        let ret_arity = func_type.results.len() as u8;

        let mut locals = Vec::new();
        for _ in 0..body.count("local declarations")? {
            let n = body.u32_leb()?;
            let ty = body.val_type()?;
            if param_count + locals.len() as u64 + u64::from(n) > MAX_LOCALS {
                return Err(body.err(DecodeErrorKind::TooMany("locals")));
            }
            for _ in 0..n {
                locals.push(ty);
            }
        }
        let local_total = param_count as u32 + locals.len() as u32;
        let ops = decode_expr(&mut body, module, imported, local_total, ret_arity)?;
        if body.remaining() != 0 {
            return Err(body.err(DecodeErrorKind::SectionSize));
        }
        module.code.push(FuncBody { locals, ops });
    }
    Ok(())
}

/// Decode-time bookkeeping for one open structured region.
struct CtrlFrame {
    kind: FrameKind,
    keep: u8,
    /// The region's [`Op::PushLabel`] index, patched at `end` for forward labels.
    push_ix: usize,
    /// An `if`'s pending false edge (its [`Op::BrIfZero`]), patched at `else` or `end`.
    false_edge: Option<usize>,
    /// Then-arm [`Op::Goto`]s awaiting the `end` position.
    goto_ixs: Vec<usize>,
    has_else: bool,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum FrameKind {
    Func,
    Block,
    Loop,
    If,
}

/// The result arity a `block`/`loop`/`if` declares: none, or one value type. A type-index
/// block type is the multi-value proposal and is refused.
fn block_arity(r: &mut Reader<'_>) -> Result<u8, DecodeError> {
    let at = r.pos;
    let b = r.u8()?;
    if b == 0x40 {
        return Ok(0);
    }
    if val_type_byte(b).is_some() {
        return Ok(1);
    }
    Err(DecodeError {
        offset: r.base + at,
        kind: DecodeErrorKind::UnsupportedFeature("type-index block type"),
    })
}

/// Translates one function body's expression to the internal op stream, resolving every
/// structural target. The function's own body is
/// frame zero: a synthetic Block label whose branch target is the trailing [`Op::Return`].
fn decode_expr(
    r: &mut Reader<'_>,
    module: &Module,
    imported_funcs: u32,
    local_total: u32,
    ret_arity: u8,
) -> Result<Vec<Op>, DecodeError> {
    let func_total = imported_funcs + module.functions.len() as u32;
    let global_total = imported_global_count(module) + module.globals.len() as u32;
    let mut ops: Vec<Op> = Vec::new();
    let mut ctrl: Vec<CtrlFrame> = Vec::new();

    ops.push(Op::PushLabel { kind: LabelKind::Block, keep: ret_arity, target: PATCH });
    ctrl.push(CtrlFrame {
        kind: FrameKind::Func,
        keep: ret_arity,
        push_ix: 0,
        false_edge: None,
        goto_ixs: Vec::new(),
        has_else: false,
    });

    macro_rules! check_index {
        ($index:expr, $bound:expr, $what:literal) => {
            if $index >= $bound {
                return Err(r.err(DecodeErrorKind::IndexOutOfRange($what)));
            }
        };
    }

    loop {
        let opcode_at = r.base + r.pos;
        let opcode = r.u8()?;
        match opcode {
            0x00 => ops.push(Op::Unreachable),
            0x01 => ops.push(Op::Nop),
            0x02 => {
                if ctrl.len() >= MAX_CONTROL_DEPTH {
                    return Err(r.err(DecodeErrorKind::TooMany("control nesting")));
                }
                let keep = block_arity(r)?;
                let push_ix = ops.len();
                ops.push(Op::PushLabel { kind: LabelKind::Block, keep, target: PATCH });
                ctrl.push(CtrlFrame {
                    kind: FrameKind::Block,
                    keep,
                    push_ix,
                    false_edge: None,
                    goto_ixs: Vec::new(),
                    has_else: false,
                });
            }
            0x03 => {
                if ctrl.len() >= MAX_CONTROL_DEPTH {
                    return Err(r.err(DecodeErrorKind::TooMany("control nesting")));
                }
                let keep = block_arity(r)?;
                let push_ix = ops.len();
                ops.push(Op::PushLabel {
                    kind: LabelKind::Loop,
                    keep: 0,
                    target: push_ix as u32 + 1,
                });
                ctrl.push(CtrlFrame {
                    kind: FrameKind::Loop,
                    keep,
                    push_ix,
                    false_edge: None,
                    goto_ixs: Vec::new(),
                    has_else: false,
                });
            }
            0x04 => {
                if ctrl.len() >= MAX_CONTROL_DEPTH {
                    return Err(r.err(DecodeErrorKind::TooMany("control nesting")));
                }
                let keep = block_arity(r)?;
                let push_ix = ops.len();
                ops.push(Op::PushLabel { kind: LabelKind::Block, keep, target: PATCH });
                let false_edge = ops.len();
                ops.push(Op::BrIfZero { target: PATCH });
                ctrl.push(CtrlFrame {
                    kind: FrameKind::If,
                    keep,
                    push_ix,
                    false_edge: Some(false_edge),
                    goto_ixs: Vec::new(),
                    has_else: false,
                });
            }
            0x05 => {
                let frame = ctrl.last_mut().ok_or_else(|| {
                    r.err(DecodeErrorKind::BadElseContext)
                })?;
                if frame.kind != FrameKind::If || frame.has_else {
                    return Err(r.err(DecodeErrorKind::BadElseContext));
                }
                frame.has_else = true;
                let goto_ix = ops.len();
                ops.push(Op::Goto { target: PATCH });
                frame.goto_ixs.push(goto_ix);
                let false_edge = frame.false_edge.take().unwrap_or(0);
                let else_start = ops.len() as u32;
                if let Some(Op::BrIfZero { target }) = ops.get_mut(false_edge) {
                    *target = else_start;
                }
            }
            0x0B => {
                let frame = ctrl.pop().ok_or_else(|| {
                    r.err(DecodeErrorKind::ControlUnderflow)
                })?;
                let pop_ix = ops.len() as u32;
                if let Some(false_edge) = frame.false_edge {
                    if !frame.has_else && frame.keep != 0 {
                        return Err(r.err(DecodeErrorKind::IfResultWithoutElse));
                    }
                    if let Some(Op::BrIfZero { target }) = ops.get_mut(false_edge) {
                        if *target == PATCH {
                            *target = pop_ix;
                        }
                    }
                }
                for goto_ix in &frame.goto_ixs {
                    if let Some(Op::Goto { target }) = ops.get_mut(*goto_ix) {
                        *target = pop_ix;
                    }
                }
                ops.push(Op::PopLabel);
                let after = ops.len() as u32;
                if frame.kind != FrameKind::Loop {
                    if let Some(Op::PushLabel { target, .. }) = ops.get_mut(frame.push_ix) {
                        *target = after;
                    }
                }
                if frame.kind == FrameKind::Func {
                    ops.push(Op::Return);
                    break;
                }
            }
            0x0C => {
                let depth = r.u32_leb()?;
                check_index!(depth as usize, ctrl.len(), "label");
                ops.push(Op::Br { depth });
            }
            0x0D => {
                let depth = r.u32_leb()?;
                check_index!(depth as usize, ctrl.len(), "label");
                ops.push(Op::BrIf { depth });
            }
            0x0E => {
                let count = r.count("br_table targets")?;
                let mut depths = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let depth = r.u32_leb()?;
                    check_index!(depth as usize, ctrl.len(), "label");
                    depths.push(depth);
                }
                let default = r.u32_leb()?;
                check_index!(default as usize, ctrl.len(), "label");
                ops.push(Op::BrTable { depths: depths.into_boxed_slice(), default });
            }
            0x0F => ops.push(Op::Return),
            0x10 => {
                let func = r.u32_leb()?;
                check_index!(func, func_total, "function");
                ops.push(Op::Call { func });
            }
            0x11 => {
                let type_index = r.u32_leb()?;
                check_index!(type_index as usize, module.types.len(), "type");
                if r.u32_leb()? != 0 {
                    return Err(r.err(DecodeErrorKind::UnsupportedFeature("multiple tables")));
                }
                ops.push(Op::CallIndirect { type_index });
            }
            0x1A => ops.push(Op::Drop),
            0x1B => ops.push(Op::Select),
            0x20..=0x22 => {
                let index = r.u32_leb()?;
                check_index!(index, local_total, "local");
                ops.push(match opcode {
                    0x20 => Op::LocalGet(index),
                    0x21 => Op::LocalSet(index),
                    _ => Op::LocalTee(index),
                });
            }
            0x23 | 0x24 => {
                let index = r.u32_leb()?;
                check_index!(index, global_total, "global");
                ops.push(if opcode == 0x23 {
                    Op::GlobalGet(index)
                } else {
                    Op::GlobalSet(index)
                });
            }
            0x28..=0x35 => {
                let (ty, width, signed) = LOAD_SHAPES[(opcode - 0x28) as usize];
                let _align = r.u32_leb()?;
                let offset = r.u32_leb()?;
                ops.push(Op::Load { ty, width, signed, offset });
            }
            0x36..=0x3E => {
                let (ty, width) = STORE_SHAPES[(opcode - 0x36) as usize];
                let _align = r.u32_leb()?;
                let offset = r.u32_leb()?;
                ops.push(Op::Store { ty, width, offset });
            }
            0x3F | 0x40 => {
                if r.u8()? != 0 {
                    return Err(r.err(DecodeErrorKind::BadFlags("memory index")));
                }
                ops.push(if opcode == 0x3F { Op::MemorySize } else { Op::MemoryGrow });
            }
            0x41 => {
                let v = r.i32_leb()?;
                ops.push(Op::I32Const(v as u32));
            }
            0x42 => {
                let v = r.i64_leb()?;
                ops.push(Op::I64Const(v as u64));
            }
            0x43 => ops.push(Op::F32Const(u32::from_le_bytes(fixed4(r.take(4)?)))),
            0x44 => ops.push(Op::F64Const(u64::from_le_bytes(fixed8(r.take(8)?)))),
            0x45..=0xC4 => {
                ops.push(Op::Num(NUM_OPS[(opcode - 0x45) as usize]));
            }
            0xFC => {
                let sub = r.u32_leb()?;
                match sub {
                    0..=7 => ops.push(Op::Num(TRUNC_SAT_OPS[sub as usize])),
                    10 => {
                        if r.u8()? != 0 || r.u8()? != 0 {
                            return Err(r.err(DecodeErrorKind::BadFlags("memory index")));
                        }
                        ops.push(Op::MemoryCopy);
                    }
                    11 => {
                        if r.u8()? != 0 {
                            return Err(r.err(DecodeErrorKind::BadFlags("memory index")));
                        }
                        ops.push(Op::MemoryFill);
                    }
                    _ => return Err(r.err(DecodeErrorKind::BadPrefixOpcode(sub))),
                }
            }
            _ => {
                return Err(DecodeError {
                    offset: opcode_at,
                    kind: DecodeErrorKind::BadOpcode(opcode),
                });
            }
        }
    }

    debug_assert!(!ops.iter().any(|op| matches!(
        op,
        Op::PushLabel { target: PATCH, .. }
            | Op::BrIfZero { target: PATCH }
            | Op::Goto { target: PATCH }
    )));
    Ok(ops)
}

/// The load opcodes 0x28..=0x35 in order: result type, width, sign-extension.
const LOAD_SHAPES: [(ValType, u8, bool); 14] = [
    (ValType::I32, 4, false),
    (ValType::I64, 8, false),
    (ValType::F32, 4, false),
    (ValType::F64, 8, false),
    (ValType::I32, 1, true),
    (ValType::I32, 1, false),
    (ValType::I32, 2, true),
    (ValType::I32, 2, false),
    (ValType::I64, 1, true),
    (ValType::I64, 1, false),
    (ValType::I64, 2, true),
    (ValType::I64, 2, false),
    (ValType::I64, 4, true),
    (ValType::I64, 4, false),
];

/// The store opcodes 0x36..=0x3E in order: operand type, width.
const STORE_SHAPES: [(ValType, u8); 9] = [
    (ValType::I32, 4),
    (ValType::I64, 8),
    (ValType::F32, 4),
    (ValType::F64, 8),
    (ValType::I32, 1),
    (ValType::I32, 2),
    (ValType::I64, 1),
    (ValType::I64, 2),
    (ValType::I64, 4),
];

/// The plain numeric opcodes 0x45..=0xC4 in spec order.
const NUM_OPS: [NumOp; 128] = [
    NumOp::I32Eqz,
    NumOp::I32Eq,
    NumOp::I32Ne,
    NumOp::I32LtS,
    NumOp::I32LtU,
    NumOp::I32GtS,
    NumOp::I32GtU,
    NumOp::I32LeS,
    NumOp::I32LeU,
    NumOp::I32GeS,
    NumOp::I32GeU,
    NumOp::I64Eqz,
    NumOp::I64Eq,
    NumOp::I64Ne,
    NumOp::I64LtS,
    NumOp::I64LtU,
    NumOp::I64GtS,
    NumOp::I64GtU,
    NumOp::I64LeS,
    NumOp::I64LeU,
    NumOp::I64GeS,
    NumOp::I64GeU,
    NumOp::F32Eq,
    NumOp::F32Ne,
    NumOp::F32Lt,
    NumOp::F32Gt,
    NumOp::F32Le,
    NumOp::F32Ge,
    NumOp::F64Eq,
    NumOp::F64Ne,
    NumOp::F64Lt,
    NumOp::F64Gt,
    NumOp::F64Le,
    NumOp::F64Ge,
    NumOp::I32Clz,
    NumOp::I32Ctz,
    NumOp::I32Popcnt,
    NumOp::I32Add,
    NumOp::I32Sub,
    NumOp::I32Mul,
    NumOp::I32DivS,
    NumOp::I32DivU,
    NumOp::I32RemS,
    NumOp::I32RemU,
    NumOp::I32And,
    NumOp::I32Or,
    NumOp::I32Xor,
    NumOp::I32Shl,
    NumOp::I32ShrS,
    NumOp::I32ShrU,
    NumOp::I32Rotl,
    NumOp::I32Rotr,
    NumOp::I64Clz,
    NumOp::I64Ctz,
    NumOp::I64Popcnt,
    NumOp::I64Add,
    NumOp::I64Sub,
    NumOp::I64Mul,
    NumOp::I64DivS,
    NumOp::I64DivU,
    NumOp::I64RemS,
    NumOp::I64RemU,
    NumOp::I64And,
    NumOp::I64Or,
    NumOp::I64Xor,
    NumOp::I64Shl,
    NumOp::I64ShrS,
    NumOp::I64ShrU,
    NumOp::I64Rotl,
    NumOp::I64Rotr,
    NumOp::F32Abs,
    NumOp::F32Neg,
    NumOp::F32Ceil,
    NumOp::F32Floor,
    NumOp::F32Trunc,
    NumOp::F32Nearest,
    NumOp::F32Sqrt,
    NumOp::F32Add,
    NumOp::F32Sub,
    NumOp::F32Mul,
    NumOp::F32Div,
    NumOp::F32Min,
    NumOp::F32Max,
    NumOp::F32Copysign,
    NumOp::F64Abs,
    NumOp::F64Neg,
    NumOp::F64Ceil,
    NumOp::F64Floor,
    NumOp::F64Trunc,
    NumOp::F64Nearest,
    NumOp::F64Sqrt,
    NumOp::F64Add,
    NumOp::F64Sub,
    NumOp::F64Mul,
    NumOp::F64Div,
    NumOp::F64Min,
    NumOp::F64Max,
    NumOp::F64Copysign,
    NumOp::I32WrapI64,
    NumOp::I32TruncF32S,
    NumOp::I32TruncF32U,
    NumOp::I32TruncF64S,
    NumOp::I32TruncF64U,
    NumOp::I64ExtendI32S,
    NumOp::I64ExtendI32U,
    NumOp::I64TruncF32S,
    NumOp::I64TruncF32U,
    NumOp::I64TruncF64S,
    NumOp::I64TruncF64U,
    NumOp::F32ConvertI32S,
    NumOp::F32ConvertI32U,
    NumOp::F32ConvertI64S,
    NumOp::F32ConvertI64U,
    NumOp::F32DemoteF64,
    NumOp::F64ConvertI32S,
    NumOp::F64ConvertI32U,
    NumOp::F64ConvertI64S,
    NumOp::F64ConvertI64U,
    NumOp::F64PromoteF32,
    NumOp::I32ReinterpretF32,
    NumOp::I64ReinterpretF64,
    NumOp::F32ReinterpretI32,
    NumOp::F64ReinterpretI64,
    NumOp::I32Extend8S,
    NumOp::I32Extend16S,
    NumOp::I64Extend8S,
    NumOp::I64Extend16S,
    NumOp::I64Extend32S,
];

/// The `0xFC` 0..=7 saturating truncations in spec order.
const TRUNC_SAT_OPS: [NumOp; 8] = [
    NumOp::I32TruncSatF32S,
    NumOp::I32TruncSatF32U,
    NumOp::I32TruncSatF64S,
    NumOp::I32TruncSatF64U,
    NumOp::I64TruncSatF32S,
    NumOp::I64TruncSatF32U,
    NumOp::I64TruncSatF64S,
    NumOp::I64TruncSatF64U,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(bytes: &[u8]) -> Result<u32, DecodeError> {
        Reader::new(bytes, 0).u32_leb()
    }

    fn read_i32(bytes: &[u8]) -> Result<i32, DecodeError> {
        Reader::new(bytes, 0).i32_leb()
    }

    fn read_i64(bytes: &[u8]) -> Result<i64, DecodeError> {
        Reader::new(bytes, 0).i64_leb()
    }

    #[test]
    fn u32_leb_canonical_and_padded() {
        assert_eq!(read_u32(&[0x00]).unwrap(), 0);
        assert_eq!(read_u32(&[0x7F]).unwrap(), 127);
        assert_eq!(read_u32(&[0x80, 0x01]).unwrap(), 128);
        assert_eq!(read_u32(&[0x80, 0x80, 0x80, 0x80, 0x00]).unwrap(), 0);
        assert_eq!(read_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).unwrap(), u32::MAX);
    }

    #[test]
    fn u32_leb_overflow_rejected() {
        assert!(read_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]).is_err());
        assert!(read_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).is_err());
        assert!(read_u32(&[0x80]).is_err());
    }

    #[test]
    fn i32_leb_signs_and_bounds() {
        assert_eq!(read_i32(&[0x00]).unwrap(), 0);
        assert_eq!(read_i32(&[0x7F]).unwrap(), -1);
        assert_eq!(read_i32(&[0x40]).unwrap(), -64);
        assert_eq!(read_i32(&[0xC0, 0x00]).unwrap(), 64);
        assert_eq!(read_i32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07]).unwrap(), i32::MAX);
        assert_eq!(read_i32(&[0x80, 0x80, 0x80, 0x80, 0x78]).unwrap(), i32::MIN);
        assert_eq!(read_i32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).unwrap(), -1);
        assert!(read_i32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x4F]).is_err());
        assert!(read_i32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).is_err());
    }

    #[test]
    fn i64_leb_signs_and_bounds() {
        assert_eq!(read_i64(&[0x00]).unwrap(), 0);
        assert_eq!(read_i64(&[0x7F]).unwrap(), -1);
        assert_eq!(
            read_i64(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]).unwrap(),
            i64::MAX
        );
        assert_eq!(
            read_i64(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7F]).unwrap(),
            i64::MIN
        );
        assert_eq!(
            read_i64(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).unwrap(),
            -1
        );
        assert!(read_i64(&[0xFF; 11]).is_err());
    }

    #[test]
    fn empty_module_decodes() {
        let m = decode(&[0x00, b'a', b's', b'm', 1, 0, 0, 0]).unwrap();
        assert_eq!(m, Module::default());
    }

    #[test]
    fn bad_magic_and_version() {
        assert!(matches!(
            decode(b"\0asX\x01\0\0\0").unwrap_err().kind,
            DecodeErrorKind::BadMagic
        ));
        assert!(matches!(
            decode(b"\0asm\x02\0\0\0").unwrap_err().kind,
            DecodeErrorKind::BadVersion
        ));
    }
}
