//! Generated MIR helper builders shared by the WASM, ARM, and RISC-V backends: the bodies that the
//! `StringConcat`, `IntToString`, and `StringEquals` marker instructions lower to. Each is an
//! ordinary verified [`Function`] of pure [`lamella_ir`] MIR (no target specifics) -- a string is the
//! array layout `[u32 unit_count][u16 units]`, so the helpers build/read their results with
//! `AllocArray` (element size 2), array loads/stores, `FieldLoad` of the count word, and the integer
//! Div/Rem. A backend rewrites the marker to a call to the appended helper and lowers it through its
//! usual path. Kept out of the feature-gated WASM module so the always-compiled ARM + RISC-V backends
//! can use them too.

use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{BasicBlock, BinOp, BlockId, CmpOp, Function, Inst, MirType, Terminator, ValueId};

use crate::resolver::ELEMENT_KIND_UTF16_UNIT;

pub(crate) use crate::resolver::STORAGE_IS_BYTES;

/// The literal blob for `utf16`, in this build's storage encoding -- the ONE place the AOT's string
/// layout is written down, so the three backends emit the same bytes by construction rather than by
/// three transcriptions agreeing.
///
/// - default (UTF-16): `[u32 unit_count][u16 units...]`
/// - `string-utf8` / `string-utf8-wtf8`: `[u32 unit_count][u32 byte_len][bytes...]`
///
/// `unit_count` leads in BOTH, because `String.Length` is the UTF-16 unit count in every tier -- the
/// encoding changes the storage, never the managed semantics.
pub(crate) fn string_blob_bytes(utf16: &[u16]) -> Result<Vec<u8>, UnencodableUnit> {
    let mut blob = Vec::new();
    blob.extend_from_slice(&(utf16.len() as u32).to_le_bytes());
    if STORAGE_IS_BYTES {
        let bytes = encode_string_bytes(utf16)?;
        blob.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        blob.extend_from_slice(&bytes);
    } else {
        for &unit in utf16 {
            blob.extend_from_slice(&unit.to_le_bytes());
        }
    }
    Ok(blob)
}

/// A UTF-16 code unit this build's string storage cannot represent, with its index -- the two facts
/// .NET's own `EncoderFallbackException` message carries, and the index counts UTF-16 CODE UNITS,
/// not scalars, exactly as .NET's does.
///
/// Only strict `string-utf8` can produce one, and only for a LONE surrogate: the default tier stores
/// the units themselves, and `string-utf8-wtf8` exists precisely so that a lone surrogate survives.
/// Two of the three tiers can never construct this, which is the seam's shape rather than a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnencodableUnit {
    /// The offending UTF-16 code unit.
    pub(crate) unit: u16,
    /// Its index in the literal, in UTF-16 code units.
    pub(crate) index: u32,
}

/// Encodes UTF-16 units to this build's UTF-8 storage bytes: standard UTF-8 for `string-utf8`, and
/// surrogate-preserving WTF-8 for `string-utf8-wtf8`, where a lone surrogate keeps its own three-byte
/// encoding. Returns the UTF-16 bytes unchanged when this build stores units, so callers need no
/// branch.
///
/// A well-formed surrogate PAIR combines into one four-byte code point in BOTH -- encoding it as two
/// three-byte surrogates instead is CESU-8, which passes every vector that does not contain a pair.
///
/// UNDER STRICT `string-utf8` A LONE SURROGATE IS REFUSED, not replaced with U+FFFD. It used to be
/// replaced, which matched `Encoding.UTF8`'s default fallback and was the wrong rule for this
/// operation: this is string CONSTRUCTION, which never loses data, and the interpreter now raises
/// `EncoderFallbackException` rather than substituting. A REFUSAL rather than a mirror of that throw,
/// because the AOT knows the offending unit at COMPILE time -- every unit reaching this encoder comes
/// from a literal in the metadata, since the built-string helpers join storage bytes and never
/// re-encode -- so there is nothing to defer to run time. That keeps the two pointing the same way and
/// makes the AOT the louder of the two: a program the interpreter would refuse to run past is a
/// program the compiler refuses to build, rather than a device image that quietly differs from its
/// preview.
///
/// The two byte tiers now share ONE scan. They did not: the strict tier called `from_utf16_lossy`,
/// a second implementation of the same encoding, which is how the lossy rule outlived the decision to
/// drop it.
pub(crate) fn encode_string_bytes(units: &[u16]) -> Result<Vec<u8>, UnencodableUnit> {
    if !STORAGE_IS_BYTES {
        return Ok(units.iter().flat_map(|u| u.to_le_bytes()).collect());
    }
    let strict = cfg!(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")));
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let u = u32::from(units[i]);
        let index = i as u32;
        let code = if (0xD800..=0xDBFF).contains(&u)
            && i + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&u32::from(units[i + 1]))
        {
            let lo = u32::from(units[i + 1]);
            i += 2;
            0x1_0000 + ((u - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            i += 1;
            u
        };
        if strict && (0xD800..=0xDFFF).contains(&code) {
            return Err(UnencodableUnit {
                unit: code as u16,
                index,
            });
        }
        if code < 0x80 {
            out.push(code as u8);
        } else if code < 0x800 {
            out.push(0xC0 | (code >> 6) as u8);
            out.push(0x80 | (code & 0x3F) as u8);
        } else if code < 0x1_0000 {
            out.push(0xE0 | (code >> 12) as u8);
            out.push(0x80 | ((code >> 6) & 0x3F) as u8);
            out.push(0x80 | (code & 0x3F) as u8);
        } else {
            out.push(0xF0 | (code >> 18) as u8);
            out.push(0x80 | ((code >> 12) & 0x3F) as u8);
            out.push(0x80 | ((code >> 6) & 0x3F) as u8);
            out.push(0x80 | (code & 0x3F) as u8);
        }
    }
    Ok(out)
}

/// The per-target console sink every backend routes `Debug.WriteLine` / `Console.WriteLine` through:
/// `lamella_console_write_bytes(ptr: *const u8, len: usize)`, implemented once per target in that
/// target's `runtime-support` crate (ARM over semihosting `SYS_WRITEC`, RISC-V over the board UART).
/// Byte-oriented because every console is a byte channel, which keeps the UTF-16 -> byte conversion
/// out of every per-target crate and puts formatting in the front end -- so a NEW ISA profile is a
/// new implementation of this ONE symbol and zero front-end change.
///
/// Distinct from the older UTF-16 `lamella_console_write(*const u32)`, which still backs the corlib
/// `Console.Write(string)` path and is deliberately left alone.
pub(crate) const CONSOLE_WRITE_BYTES: &str = "lamella_console_write_bytes";

/// Rewrites each `StringConcat` to a call to a generated `__string_concat` helper appended to the
/// program, so string concatenation reuses the normal call + structuring path on every backend.
///
/// `string` is `System.String`'s handle where this build can name the type -- see
/// [`string_concat_mir`] for what it buys and why the flat path passes `None`.
pub(crate) fn lower_string_concat(program: &mut Vec<Function>, string: Option<u32>) {
    let has_concat = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::StringConcat { .. }));
    if !has_concat {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::StringConcat { lhs, rhs } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*lhs, *rhs],
                    };
                }
            }
        }
    }
    program.push(string_concat_mir(string));
}

/// Rewrites each `IntToString` to a call to a generated `__int_to_string` helper (appended after the
/// string helpers, if any), so integer formatting reuses the normal call + structuring path.
///
/// `string` is `System.String`'s handle where this build can name the type; see
/// [`string_concat_mir`].
pub(crate) fn lower_int_to_string(program: &mut Vec<Function>, string: Option<u32>) {
    let has = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::IntToString { .. }));
    if !has {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::IntToString { value } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*value],
                    };
                }
            }
        }
    }
    program.push(int_to_string_mir(string));
}

/// Rewrites each `WriteInt` to a call to a generated `__write_int` helper, so `Console.WriteLine(int)`
/// (and the Python front end's `print` of an int) formats in SHARED MIR and hands the bytes to the
/// per-target console seam -- rather than each backend carrying its own hand-encoded itoa. ARM had
/// one; RISC-V had none, which is why `Console.WriteLine(int)` did not compile there at all.
///
/// OBJECT PATH ONLY, by the caller's choice: the helper ends in a `PInvoke` of
/// [`CONSOLE_WRITE_BYTES`], and a flat image has no linker to resolve an extern against. The flat
/// path keeps each backend's self-contained inline form (ARM's `emit_write_int`; RISC-V rejects, as
/// it did before).
pub(crate) fn lower_write_int(program: &mut Vec<Function>) {
    let has = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::WriteInt { .. }));
    if !has {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::WriteInt { value } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*value],
                    };
                }
            }
        }
    }
    program.push(write_int_mir());
}

/// The stack buffer `__write_int` formats into, in bytes. The widest output is `-2147483648\n` = 12
/// bytes, so 16 leaves slack and keeps the slot word-sized.
const WRITE_INT_BUF: i64 = 16;

/// The `__write_int(v)` helper: formats a signed i32 as decimal + a newline into a STACK buffer and
/// hands `(pointer, length)` to the per-target console seam ([`CONSOLE_WRITE_BYTES`]). The sign split
/// is the branchless one [`int_to_string_mir`] uses (`mask = v >> 31`; `mag = (v ^ mask) - mask`;
/// `sign = mask & 1`), and the magnitude is consumed with UNSIGNED div/rem so `i32::MIN` -- whose
/// negation does not fit in an i32 -- formats correctly as the bit pattern 0x8000_0000.
///
/// Digits are written back-to-front from the end of the buffer, so nothing needs to be counted first
/// and nothing is allocated: this runs with no heap and no GC, which a console primitive must.
///
/// Two deliberate shape choices:
///  - the buffer POINTER is typed `I32`, not `ManagedPtr`. A stack address must NEVER be enumerated
///    as a garbage-collector root: the root walk keys on the slot's type, and the collector treats a
///    non-null root as a heap payload with no range check, so typing it as a pointer would hand the
///    collector a stack address to trace.
///  - the loop carries `(magnitude, index)` as HEADER BLOCK PARAMETERS fed by `Jump` edges, with the
///    bodies reading them by dominance. `Terminator::Branch` edges cannot carry block arguments on
///    either backend, so the exit index reaches the tail the same way -- by dominance -- and the
///    start offset is computed arithmetically (`i + 1 - sign`) instead of merged through a phi.
fn write_int_mir() -> Function {
    let i32t = MirType::I32;
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let bin = |op, lhs, rhs| Inst::Binary { op, lhs, rhs };
    let cmp = |op, lhs, rhs| Inst::Compare { op, lhs, rhs };
    let put = |address, value| Inst::Store {
        address,
        value,
        width: 1,
    };
    let v = ValueId;
    let branch = |cond, t: u32, f: u32| Terminator::Branch {
        cond,
        if_true: BlockId(t),
        true_args: Vec::new(),
        if_false: BlockId(f),
        false_args: Vec::new(),
    };
    let jump = |t: u32, args: Vec<ValueId>| Terminator::Jump {
        target: BlockId(t),
        args,
    };
    let buf = MirType::ValueType {
        handle: lamella_ir::TypeHandle(0),
        size: WRITE_INT_BUF as u32,
    };
    Function {
        params: vec![i32t],
        ret: Some(i32t),
        value_types: vec![
            i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, buf, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![v(0)],
                insts: vec![
                    (v(1), c(0)),
                    (v(2), c(1)),
                    (v(3), c(10)),
                    (v(4), c(31)),
                    (v(5), c(i64::from(b'0'))),
                    (v(6), c(i64::from(b'-'))),
                    (v(7), c(i64::from(b'\n'))),
                    (v(8), c(WRITE_INT_BUF - 1)),
                    (v(9), c(WRITE_INT_BUF)),
                    (v(10), Inst::InitStruct),
                    (
                        v(11),
                        Inst::FieldAddr {
                            base: v(10),
                            offset: 0,
                        },
                    ),
                    (v(12), bin(BinOp::ShrSigned, v(0), v(4))),
                    (v(13), bin(BinOp::Xor, v(0), v(12))),
                    (v(14), bin(BinOp::Sub, v(13), v(12))),
                    (v(15), bin(BinOp::And, v(12), v(2))),
                    (v(16), bin(BinOp::Add, v(11), v(8))),
                    (v(17), put(v(16), v(7))),
                    (v(18), bin(BinOp::RemUnsigned, v(14), v(3))),
                    (v(19), bin(BinOp::Add, v(18), v(5))),
                    (v(20), bin(BinOp::Sub, v(8), v(2))),
                    (v(21), bin(BinOp::Add, v(11), v(20))),
                    (v(22), put(v(21), v(19))),
                    (v(23), bin(BinOp::DivUnsigned, v(14), v(3))),
                    (v(24), bin(BinOp::Sub, v(20), v(2))),
                ],
                terminator: Some(jump(1, vec![v(23), v(24)])),
            },
            BasicBlock {
                params: vec![v(25), v(26)],
                insts: vec![(v(27), cmp(CmpOp::Ne, v(25), v(1)))],
                terminator: Some(branch(v(27), 2, 3)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(28), bin(BinOp::RemUnsigned, v(25), v(3))),
                    (v(29), bin(BinOp::Add, v(28), v(5))),
                    (v(30), bin(BinOp::Add, v(11), v(26))),
                    (v(31), put(v(30), v(29))),
                    (v(32), bin(BinOp::DivUnsigned, v(25), v(3))),
                    (v(33), bin(BinOp::Sub, v(26), v(2))),
                ],
                terminator: Some(jump(1, vec![v(32), v(33)])),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(34), cmp(CmpOp::Ne, v(15), v(1)))],
                terminator: Some(branch(v(34), 4, 5)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(35), bin(BinOp::Add, v(11), v(26))),
                    (v(36), put(v(35), v(6))),
                ],
                terminator: Some(jump(5, Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(37), bin(BinOp::Add, v(26), v(2))),
                    (v(38), bin(BinOp::Sub, v(37), v(15))),
                    (v(39), bin(BinOp::Add, v(11), v(38))),
                    (v(40), bin(BinOp::Sub, v(9), v(38))),
                    (
                        v(41),
                        Inst::PInvoke {
                            import: CONSOLE_WRITE_BYTES.into(),
                            args: vec![v(39), v(40)],
                        },
                    ),
                    (v(42), c(0)),
                ],
                terminator: Some(Terminator::Return(Some(v(42)))),
            },
        ],
    }
}

/// The ADDRESS of a string's BYTE storage: `RefToInt(s) + 8`, past the unit-count and byte-length
/// words. Byte storage is addressed RAW rather than through `ArrayLoad`/`ArrayStore`, and that is
/// FORCED rather than stylistic.
///
/// An array access is BOUNDS-CHECKED against the word at offset 0, which for a string is its UTF-16
/// UNIT count -- while a byte index runs to its BYTE length and, with the payload starting at 8, is
/// biased by four on top of that. So an array access biased into byte storage traps on any string
/// whose biased byte index reaches the unit count, which for ASCII is every string of more than four
/// units. Whether that trap is observable depends on the backend emitting the bounds check, so the
/// raw addressing is what makes the helper correct on every backend rather than on the ones that
/// happen not to check.
///
/// Nothing is given up by addressing directly, because each loop's bound IS the byte length -- the
/// check was never the thing keeping these copies in range.
///
/// GC: the caller must convert AFTER the last safepoint in the body. A raw address does not survive a
/// collection, where the reference it came from does.
fn storage_address(insts: &mut Vec<(ValueId, Inst)>, next: &mut u32, s: ValueId) -> ValueId {
    let i32t = MirType::I32;
    let mut fresh = || {
        let id = ValueId(*next);
        *next += 1;
        id
    };
    let raw = fresh();
    insts.push((
        raw,
        Inst::Convert {
            value: s,
            kind: lamella_ir::ConvKind::RefToInt,
        },
    ));
    let header = fresh();
    insts.push((header, Inst::ConstInt { ty: i32t, value: 8 }));
    let start = fresh();
    insts.push((
        start,
        Inst::Binary {
            op: BinOp::Add,
            lhs: raw,
            rhs: header,
        },
    ));
    start
}

/// Emits a runtime-BUILT string's allocation, binding `result` to a fresh `System.String` object of
/// `unit_count` UTF-16 units, and writes the storage header word(s) the tier defines.
///
/// This is the RUN-TIME twin of [`string_blob_bytes`], which lays a literal's header at build time --
/// and it is the only other place the AOT decides a string's layout, so the two answer the same
/// question the same way: the unit count leads in every tier, and a byte tier carries a second
/// `byte_len` word before its storage.
///
/// `AllocDescribed` rather than `AllocArray`, and that is the whole point rather than a detail: an
/// `AllocArray` names the type by a SYNTHETIC array handle, so a built string carried an ARRAY's
/// descriptor -- `s.ToString()` dispatched through `System.Array`'s vtable and `o is string` on one
/// answered FALSE. Only `AllocDescribed` can both name `System.String` (through `TypeDescAddr`, which
/// lays the type's ONE canonical descriptor) and take a RUN-TIME payload size, which the byte tiers
/// need because their storage length is not a function of the unit count.
///
/// `byte_len` is `Some` exactly under a byte tier, where it is the storage length -- the payload is
/// then `8 + byte_len` and both header words are written; `None` under the default tier, where the
/// payload is `4 + unit_count * 2` and there is only the count word.
///
/// Every value appended is an `i32`; `next` advances past them so the caller extends `value_types` by
/// the same count. Values are numbered in EMISSION order, so a definition never follows its use.
fn emit_string_alloc(
    insts: &mut Vec<(ValueId, Inst)>,
    next: &mut u32,
    result: ValueId,
    string: u32,
    unit_count: ValueId,
    byte_len: Option<ValueId>,
) {
    let i32t = MirType::I32;
    let mut fresh = || {
        let id = ValueId(*next);
        *next += 1;
        id
    };
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };
    let payload = match byte_len {
        Some(bytes) => {
            let eight = fresh();
            insts.push((eight, Inst::ConstInt { ty: i32t, value: 8 }));
            let payload = fresh();
            insts.push((payload, add(bytes, eight)));
            payload
        }
        None => {
            let two = fresh();
            insts.push((two, Inst::ConstInt { ty: i32t, value: 2 }));
            let scaled = fresh();
            insts.push((
                scaled,
                Inst::Binary {
                    op: BinOp::Mul,
                    lhs: unit_count,
                    rhs: two,
                },
            ));
            let four = fresh();
            insts.push((four, Inst::ConstInt { ty: i32t, value: 4 }));
            let payload = fresh();
            insts.push((payload, add(scaled, four)));
            payload
        }
    };
    let desc = fresh();
    insts.push((
        desc,
        Inst::TypeDescAddr {
            handle: lamella_ir::TypeHandle(string),
        },
    ));
    insts.push((
        result,
        Inst::AllocDescribed {
            descriptor: desc,
            payload_size: payload,
        },
    ));
    let unit_store = fresh();
    insts.push((
        unit_store,
        Inst::FieldStore {
            base: result,
            offset: 0,
            value: unit_count,
        },
    ));
    if let Some(bytes) = byte_len {
        let byte_store = fresh();
        insts.push((
            byte_store,
            Inst::FieldStore {
                base: result,
                offset: 4,
                value: bytes,
            },
        ));
    }
}

/// The `__int_to_string(v) -> ObjectRef` helper: formats a signed i32 as decimal. Branchlessly splits
/// `v` into magnitude + sign (`mask = v >> 31`; `mag = (v ^ mask) - mask`; `sign = mask & 1`), counts
/// the decimal digits (a `/10` loop), allocates a `[u32 unit_count][u16 units]` blob of digits + the
/// optional `-`, fills the digits back-to-front (`%10` + `/10`), then writes a leading `-` if negative.
/// Built as MIR so it reloops + lowers like any function (uses Div/Rem).
///
/// `string` is `System.String`'s handle where this build can name the type; see
/// [`string_concat_mir`] for what that changes and why `None` keeps the old body verbatim.
fn int_to_string_mir(string: Option<u32>) -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let bin = |op, lhs, rhs| Inst::Binary { op, lhs, rhs };
    let cmp = |op, lhs, rhs| Inst::Compare { op, lhs, rhs };
    let put = |array, index, value| Inst::ArrayStore {
        array,
        index,
        value,
        element_size: 2,
    };
    let v = ValueId;
    let branch = |cond, t: u32, f: u32, ta: Vec<ValueId>, fa: Vec<ValueId>| Terminator::Branch {
        cond,
        if_true: BlockId(t),
        true_args: ta,
        if_false: BlockId(f),
        false_args: fa,
    };
    let jump = |t: u32, args: Vec<ValueId>| Terminator::Jump {
        target: BlockId(t),
        args,
    };
    let mut f = Function {
        params: vec![objt],
        ret: Some(objt),
        value_types: vec![
            i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, objt, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![v(0)],
                insts: vec![
                    (v(1), c(0)),
                    (v(2), c(1)),
                    (v(3), c(10)),
                    (v(4), c(i64::from(b'0'))),
                    (v(5), c(i64::from(b'-'))),
                    (v(6), c(31)),
                    (v(7), bin(BinOp::ShrSigned, v(0), v(6))),
                    (v(8), bin(BinOp::Xor, v(0), v(7))),
                    (v(9), bin(BinOp::Sub, v(8), v(7))),
                    (v(10), bin(BinOp::And, v(7), v(2))),
                    (v(11), bin(BinOp::DivUnsigned, v(9), v(3))),
                ],
                terminator: Some(jump(1, vec![v(11), v(2)])),
            },
            BasicBlock {
                params: vec![v(12), v(13)],
                insts: vec![(v(14), cmp(CmpOp::Ne, v(12), v(1)))],
                terminator: Some(branch(v(14), 2, 3, Vec::new(), Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(15), bin(BinOp::Add, v(13), v(2))),
                    (v(16), bin(BinOp::DivUnsigned, v(12), v(3))),
                ],
                terminator: Some(jump(1, vec![v(16), v(15)])),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(18), bin(BinOp::Add, v(13), v(10))),
                    (
                        v(19),
                        Inst::AllocArray {
                            handle: lamella_ir::synthetic_array_handle(ELEMENT_KIND_UTF16_UNIT),
                            element: None,
                            length: v(18),
                            element_size: 2,
                            element_kind: ELEMENT_KIND_UTF16_UNIT,
                        },
                    ),
                    (v(20), bin(BinOp::Sub, v(18), v(2))),
                ],
                terminator: Some(jump(4, vec![v(20), v(9)])),
            },
            BasicBlock {
                params: vec![v(21), v(22)],
                insts: vec![(v(23), cmp(CmpOp::SignedGe, v(21), v(10)))],
                terminator: Some(branch(v(23), 5, 6, Vec::new(), Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(24), bin(BinOp::RemUnsigned, v(22), v(3))),
                    (v(25), bin(BinOp::Add, v(24), v(4))),
                    (v(26), put(v(19), v(21), v(25))),
                    (v(27), bin(BinOp::DivUnsigned, v(22), v(3))),
                    (v(28), bin(BinOp::Sub, v(21), v(2))),
                ],
                terminator: Some(jump(4, vec![v(28), v(27)])),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(29), cmp(CmpOp::Ne, v(10), v(1)))],
                terminator: Some(branch(v(29), 7, 8, Vec::new(), Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(30), put(v(19), v(1), v(5)))],
                terminator: Some(jump(8, Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(Some(v(19)))),
            },
        ],
    };
    if let Some(handle) = string {
        patch_int_to_string_for_string(&mut f, handle);
    }
    f
}

/// Rewrites [`int_to_string_mir`]'s allocation to name `System.String`, and under a byte tier its
/// storage to the byte layout, in place.
///
/// Block 3 is the only block that must change under the default tier -- the storage there is already
/// byte-identical to a literal's, so ONLY the descriptor moves and every fill store stands. Under a
/// byte tier the fills move too, and the conversion is free rather than an encode: every unit this
/// helper writes is an ASCII digit or `-`, so the byte length EQUALS the unit count and one UTF-16 unit
/// is one storage byte. That is why `byte_len` is the same value as the unit count here, where
/// [`patch_string_concat_for_string`] has to read a second word.
fn patch_int_to_string_for_string(f: &mut Function, handle: u32) {
    let i32t = MirType::I32;
    let mut next = f.value_types.len() as u32;
    let (total, sign, one, dash, result) =
        (ValueId(18), ValueId(10), ValueId(2), ValueId(5), ValueId(19));
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };

    let mut insts = vec![(
        total,
        Inst::Binary {
            op: BinOp::Add,
            lhs: ValueId(13),
            rhs: sign,
        },
    )];
    emit_string_alloc(
        &mut insts,
        &mut next,
        result,
        handle,
        total,
        STORAGE_IS_BYTES.then_some(total),
    );
    let mut storage = ValueId(next);
    if STORAGE_IS_BYTES {
        storage = storage_address(&mut insts, &mut next, result);
    }
    insts.push((
        ValueId(20),
        Inst::Binary {
            op: BinOp::Sub,
            lhs: total,
            rhs: one,
        },
    ));
    f.blocks[3].insts = insts;

    if STORAGE_IS_BYTES {
        let at = ValueId(next);
        next += 1;
        f.blocks[5].insts = vec![
            (
                ValueId(24),
                Inst::Binary {
                    op: BinOp::RemUnsigned,
                    lhs: ValueId(22),
                    rhs: ValueId(3),
                },
            ),
            (ValueId(25), add(ValueId(24), ValueId(4))),
            (at, add(storage, ValueId(21))),
            (
                ValueId(26),
                Inst::Store {
                    address: at,
                    value: ValueId(25),
                    width: 1,
                },
            ),
            (
                ValueId(27),
                Inst::Binary {
                    op: BinOp::DivUnsigned,
                    lhs: ValueId(22),
                    rhs: ValueId(3),
                },
            ),
            (
                ValueId(28),
                Inst::Binary {
                    op: BinOp::Sub,
                    lhs: ValueId(21),
                    rhs: one,
                },
            ),
        ];
        f.blocks[7].insts = vec![(
            ValueId(30),
            Inst::Store {
                address: storage,
                value: dash,
                width: 1,
            },
        )];
    }
    f.value_types
        .extend(core::iter::repeat_n(i32t, (next as usize) - f.value_types.len()));
}

/// The `__string_concat(a, b) -> ObjectRef` helper: allocates a `[u32 unit_count][u16 units]` blob of
/// `a.length + b.length` units (an `AllocArray` of element size 2, which stores the count word) and
/// copies a's then b's units in with two length-2 array-copy loops. (Non-null operands; null handling
/// is a follow-up.)
///
/// `string` is `System.String`'s handle where the build can name the type. With it the result is a real
/// `System.String` object rather than a synthetic UTF-16-unit array (see [`emit_string_alloc`]), and
/// under a byte tier the storage is the byte layout as well. `None` -- the FLAT/monolithic path, which
/// has no linker, lays no canonical descriptors and gives a literal no object header either -- keeps
/// this body exactly as it was: the two travel together because both need a nameable `System.String`,
/// and under a byte tier the descriptor says "cannot stride", so unit storage under it would be a heap
/// object nothing can size.
fn string_concat_mir(string: Option<u32>) -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let ci = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let len = |s| Inst::FieldLoad { base: s, offset: 0 };
    let unit = |array, index| Inst::ArrayLoad {
        array,
        index,
        element_size: 2,
        signed: false,
    };
    let put = |array, index, value| Inst::ArrayStore {
        array,
        index,
        value,
        element_size: 2,
    };
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };
    let lt = |lhs, rhs| Inst::Compare {
        op: CmpOp::SignedLt,
        lhs,
        rhs,
    };
    let v = ValueId;
    let mut f = Function {
        params: vec![objt, objt],
        ret: Some(objt),
        value_types: vec![
            objt, objt, i32t, i32t, i32t, objt, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, i32t, i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![v(0), v(1)],
                insts: vec![
                    (v(2), len(v(0))),
                    (v(3), len(v(1))),
                    (v(4), add(v(2), v(3))),
                    (
                        v(5),
                        Inst::AllocArray {
                            handle: lamella_ir::synthetic_array_handle(ELEMENT_KIND_UTF16_UNIT),
                            element: None,
                            length: v(4),
                            element_size: 2,
                            element_kind: ELEMENT_KIND_UTF16_UNIT,
                        },
                    ),
                    (v(6), ci(0)),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(1),
                    args: vec![v(6)],
                }),
            },
            BasicBlock {
                params: vec![v(7)],
                insts: vec![(v(8), lt(v(7), v(2)))],
                terminator: Some(Terminator::Branch {
                    cond: v(8),
                    if_true: BlockId(2),
                    true_args: Vec::new(),
                    if_false: BlockId(3),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(9), unit(v(0), v(7))),
                    (v(10), put(v(5), v(7), v(9))),
                    (v(11), ci(1)),
                    (v(12), add(v(7), v(11))),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(1),
                    args: vec![v(12)],
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(13), ci(0))],
                terminator: Some(Terminator::Jump {
                    target: BlockId(4),
                    args: vec![v(13)],
                }),
            },
            BasicBlock {
                params: vec![v(14)],
                insts: vec![(v(15), lt(v(14), v(3)))],
                terminator: Some(Terminator::Branch {
                    cond: v(15),
                    if_true: BlockId(5),
                    true_args: Vec::new(),
                    if_false: BlockId(6),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(16), unit(v(1), v(14))),
                    (v(17), add(v(2), v(14))),
                    (v(18), put(v(5), v(17), v(16))),
                    (v(19), ci(1)),
                    (v(20), add(v(14), v(19))),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(4),
                    args: vec![v(20)],
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(Some(v(5)))),
            },
        ],
    };
    if let Some(handle) = string {
        patch_string_concat_for_string(&mut f, handle);
    }
    f
}

/// Rewrites [`string_concat_mir`]'s allocation to name `System.String`, and under a byte tier its
/// copies to the byte layout, in place.
///
/// Under the DEFAULT tier only block 0 changes: the storage is already byte-identically a literal's,
/// so the descriptor moves and both unit-copy loops stand untouched.
///
/// Under a BYTE tier the copies move too, and each edit is forced by the layout rather than chosen:
/// the blob is `[unit_count][byte_len][bytes]`, so (1) the result's unit count is still the sum of the
/// operands' unit counts -- `String.Length` is UTF-16 units in every tier -- while its PAYLOAD is
/// sized from the sum of their BYTE lengths, read from word 1; (2) each loop's bound is its operand's
/// byte length, not its unit count; (3) the copies move one BYTE at a time; and (4) they address the
/// storage DIRECTLY, for the bounds-check reason [`storage_address`] gives.
///
/// A byte-for-byte join is EXACT under `string-utf8`, where no ill-formed sequence can be in storage
/// to begin with. Under `string-utf8-wtf8` there is one case it does not cover and it is named rather
/// than hidden: a string ending in a lone HIGH surrogate joined to one starting with a lone LOW
/// surrogate is well-formed WTF-8 only if the two three-byte forms COMBINE into the pair's single
/// four-byte form, and this does not combine them. The interpreter does (it joins in UTF-16 and
/// re-encodes), so the two disagree on the storage bytes -- and therefore on `==`, which compares
/// them -- for that one input. Sizing, tracing and `Length` are unaffected. Combining would need a
/// seam test over the last and first three bytes; it is a separate change and `string-utf8-wtf8` is
/// enabled by nothing in the tree.
fn patch_string_concat_for_string(f: &mut Function, handle: u32) {
    let i32t = MirType::I32;
    let mut next = f.value_types.len() as u32;
    let (a, b, la, lb, units, result) = (
        ValueId(0),
        ValueId(1),
        ValueId(2),
        ValueId(3),
        ValueId(4),
        ValueId(5),
    );
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };
    let count = |s| Inst::FieldLoad { base: s, offset: 0 };

    let mut insts = vec![
        (la, count(a)),
        (lb, count(b)),
        (units, add(la, lb)),
    ];
    let (bytes_a, bytes_b) = (ValueId(next), ValueId(next + 1));
    let total_bytes = ValueId(next + 2);
    let byte_len = if STORAGE_IS_BYTES {
        next += 3;
        insts.push((bytes_a, Inst::FieldLoad { base: a, offset: 4 }));
        insts.push((bytes_b, Inst::FieldLoad { base: b, offset: 4 }));
        insts.push((total_bytes, add(bytes_a, bytes_b)));
        Some(total_bytes)
    } else {
        None
    };
    emit_string_alloc(&mut insts, &mut next, result, handle, units, byte_len);
    let (mut from_a, mut from_b, mut into) = (ValueId(next), ValueId(next), ValueId(next));
    if STORAGE_IS_BYTES {
        from_a = storage_address(&mut insts, &mut next, a);
        from_b = storage_address(&mut insts, &mut next, b);
        into = storage_address(&mut insts, &mut next, result);
    }
    insts.push((ValueId(6), Inst::ConstInt { ty: i32t, value: 0 }));
    f.blocks[0].insts = insts;

    if STORAGE_IS_BYTES {
        let one = |id| (id, Inst::ConstInt { ty: i32t, value: 1 });
        let below = |lhs, rhs| Inst::Compare {
            op: CmpOp::SignedLt,
            lhs,
            rhs,
        };
        let byte_at = |address| Inst::Load {
            address,
            width: 1,
            signed: false,
        };
        f.blocks[1].insts = vec![(ValueId(8), below(ValueId(7), bytes_a))];
        f.blocks[4].insts = vec![(ValueId(15), below(ValueId(14), bytes_b))];
        let (src, dst) = (ValueId(next), ValueId(next + 1));
        next += 2;
        f.blocks[2].insts = vec![
            (src, add(from_a, ValueId(7))),
            (ValueId(9), byte_at(src)),
            (dst, add(into, ValueId(7))),
            (
                ValueId(10),
                Inst::Store {
                    address: dst,
                    value: ValueId(9),
                    width: 1,
                },
            ),
            one(ValueId(11)),
            (ValueId(12), add(ValueId(7), ValueId(11))),
        ];
        let (src, seam) = (ValueId(next), ValueId(next + 1));
        next += 2;
        f.blocks[5].insts = vec![
            (src, add(from_b, ValueId(14))),
            (ValueId(16), byte_at(src)),
            (seam, add(into, bytes_a)),
            (ValueId(17), add(seam, ValueId(14))),
            (
                ValueId(18),
                Inst::Store {
                    address: ValueId(17),
                    value: ValueId(16),
                    width: 1,
                },
            ),
            one(ValueId(19)),
            (ValueId(20), add(ValueId(14), ValueId(19))),
        ];
    }
    f.value_types
        .extend(core::iter::repeat_n(i32t, (next as usize) - f.value_types.len()));
}

/// Rewrites each `StringEquals` to a call to a generated `__string_eq` helper appended to the
/// program, so ordinal string comparison reuses the normal call + structuring path. The WASM and
/// RISC-V backends use this; ARM lowers `StringEquals` inline, so it is gated to those two features.
#[cfg(any(feature = "wasm", feature = "riscv32"))]
pub(crate) fn lower_string_equals(program: &mut Vec<Function>) {
    let has_string_equals = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::StringEquals { .. }));
    if !has_string_equals {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::StringEquals { lhs, rhs } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*lhs, *rhs],
                    };
                }
            }
        }
    }
    program.push(string_eq_mir());
}

/// The `__string_eq(a, b) -> i32` helper: ordinal UTF-16 string equality matching the runtime's
/// contract -- two nulls are equal, null and non-null are not, otherwise length-then-content. The
/// string blob is the array layout `[u32 length][u16 units]`, so the content loop reads units with a
/// length-2 array load. Built as MIR so it goes through the same verifier + structurer as any
/// function (the loop and branches relooped, the reference null-checks lowered as i32 compares).
#[cfg(any(feature = "wasm", feature = "riscv32"))]
fn string_eq_mir() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let ci = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let cmp = |op, lhs, rhs| Inst::Compare { op, lhs, rhs };
    let unit = |array, index| Inst::ArrayLoad {
        array,
        index,
        element_size: 2,
        signed: false,
    };
    let branch = |cond, if_true: u32, if_false: u32| Terminator::Branch {
        cond,
        if_true: BlockId(if_true),
        true_args: Vec::new(),
        if_false: BlockId(if_false),
        false_args: Vec::new(),
    };
    let ret = |v| Some(Terminator::Return(Some(v)));
    let mut f = Function {
        params: vec![objt, objt],
        ret: Some(i32t),
        value_types: vec![
            objt, objt, objt, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, i32t, i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![
                    (ValueId(2), Inst::ConstInt { ty: objt, value: 0 }),
                    (ValueId(3), cmp(CmpOp::Eq, ValueId(0), ValueId(2))),
                ],
                terminator: Some(branch(ValueId(3), 1, 2)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(4), cmp(CmpOp::Eq, ValueId(1), ValueId(2)))],
                terminator: ret(ValueId(4)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(5), cmp(CmpOp::Eq, ValueId(1), ValueId(2)))],
                terminator: Some(branch(ValueId(5), 3, 4)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(6), ci(0))],
                terminator: ret(ValueId(6)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(7),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::FieldLoad {
                            base: ValueId(1),
                            offset: 0,
                        },
                    ),
                    (ValueId(9), cmp(CmpOp::Ne, ValueId(7), ValueId(8))),
                ],
                terminator: Some(branch(ValueId(9), 5, 6)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(10), ci(0))],
                terminator: ret(ValueId(10)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(11), ci(0))],
                terminator: Some(Terminator::Jump {
                    target: BlockId(7),
                    args: vec![ValueId(11)],
                }),
            },
            BasicBlock {
                params: vec![ValueId(12)],
                insts: vec![(ValueId(13), cmp(CmpOp::UnsignedGe, ValueId(12), ValueId(7)))],
                terminator: Some(branch(ValueId(13), 8, 9)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(14), ci(1))],
                terminator: ret(ValueId(14)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(15), unit(ValueId(0), ValueId(12))),
                    (ValueId(16), unit(ValueId(1), ValueId(12))),
                    (ValueId(17), cmp(CmpOp::Ne, ValueId(15), ValueId(16))),
                ],
                terminator: Some(branch(ValueId(17), 10, 11)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(18), ci(0))],
                terminator: ret(ValueId(18)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(19), ci(1)),
                    (
                        ValueId(20),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(12),
                            rhs: ValueId(19),
                        },
                    ),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(7),
                    args: vec![ValueId(20)],
                }),
            },
        ],
    };
    if STORAGE_IS_BYTES {
        patch_string_eq_for_bytes(&mut f);
    }
    f
}

/// Rewrites [`string_eq_mir`]'s UTF-16 comparison into the byte-storage one, in place.
///
/// Three edits, and each is forced by the layout rather than chosen: the blob is
/// `[unit_count][byte_len][bytes]`, so (1) equal unit counts no longer imply equal storage -- two
/// strings can hold the same number of UTF-16 units in a different number of bytes -- so the byte
/// length is compared too; (2) the loop runs over BYTES, so its bound is the byte length; and (3) the
/// payload starts at offset 8 rather than 4, which is expressed by biasing the index by four rather
/// than by a new instruction; and (3) it addresses the payload DIRECTLY, because an array access is
/// bounds-checked against the UNIT count while this loop's index runs to the BYTE length -- see
/// [`storage_address`], which is where that was measured. RISC-V and WASM (this helper's only
/// consumers) emit no such check, so the earlier biased form was never a live fault there; it became
/// one the moment the same idiom was reused on ARM, which is how it was found.
///
/// A PATCH rather than a second hand-numbered function, because the two would otherwise drift: every
/// block this does not touch is shared by construction. The new block is APPENDED, so no existing
/// block index moves and the untouched terminators stay correct.
///
/// Built only for the backends whose `StringEquals` lowering goes through this helper.
#[cfg(any(feature = "wasm", feature = "riscv32"))]
fn patch_string_eq_for_bytes(f: &mut Function) {
    let i32t = MirType::I32;
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };
    let byte_at = |address| Inst::Load {
        address,
        width: 1,
        signed: false,
    };
    let (byte_len_a, byte_len_b, lens_differ) = (ValueId(21), ValueId(22), ValueId(23));
    f.value_types.extend_from_slice(&[i32t, i32t, i32t]);
    let mut next = f.value_types.len() as u32;
    let byte_check = BlockId(f.blocks.len() as u32);

    f.blocks[4].terminator = Some(Terminator::Branch {
        cond: ValueId(9),
        if_true: BlockId(5),
        true_args: Vec::new(),
        if_false: byte_check,
        false_args: Vec::new(),
    });
    let mut insts = vec![(ValueId(11), Inst::ConstInt { ty: i32t, value: 0 })];
    let from_a = storage_address(&mut insts, &mut next, ValueId(0));
    let from_b = storage_address(&mut insts, &mut next, ValueId(1));
    f.blocks[6].insts = insts;
    f.blocks[7].insts = vec![(
        ValueId(13),
        Inst::Compare {
            op: CmpOp::UnsignedGe,
            lhs: ValueId(12),
            rhs: byte_len_a,
        },
    )];
    let (at_a, at_b) = (ValueId(next), ValueId(next + 1));
    next += 2;
    f.blocks[9].insts = vec![
        (at_a, add(from_a, ValueId(12))),
        (ValueId(15), byte_at(at_a)),
        (at_b, add(from_b, ValueId(12))),
        (ValueId(16), byte_at(at_b)),
        (
            ValueId(17),
            Inst::Compare {
                op: CmpOp::Ne,
                lhs: ValueId(15),
                rhs: ValueId(16),
            },
        ),
    ];
    f.blocks.push(BasicBlock {
        params: Vec::new(),
        insts: vec![
            (
                byte_len_a,
                Inst::FieldLoad {
                    base: ValueId(0),
                    offset: 4,
                },
            ),
            (
                byte_len_b,
                Inst::FieldLoad {
                    base: ValueId(1),
                    offset: 4,
                },
            ),
            (
                lens_differ,
                Inst::Compare {
                    op: CmpOp::Ne,
                    lhs: byte_len_a,
                    rhs: byte_len_b,
                },
            ),
        ],
        terminator: Some(Terminator::Branch {
            cond: lens_differ,
            if_true: BlockId(5),
            true_args: Vec::new(),
            if_false: BlockId(6),
            false_args: Vec::new(),
        }),
    });
    f.value_types
        .extend(core::iter::repeat_n(i32t, (next as usize) - f.value_types.len()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in `System.String` handle for the tests: a plain TypeDef-shaped token, so nothing here
    /// depends on a real assembly's numbering.
    const STRING: u32 = 0x0200_0011;

    /// Every generated helper must be well-formed MIR: these are hand-built block graphs, so a bad
    /// value number or a block-argument arity slip would otherwise surface as a backend error on
    /// whatever program happened to call one.
    ///
    /// BOTH SHAPES of the two allocating helpers, because each is built by PATCHING the block graph and
    /// a patch is exactly what the `__string_eq` note below was written about: verifying only the shape
    /// this build happens to lower would leave the other one unchecked, and under a byte tier the patch
    /// rewrites five blocks.
    #[test]
    fn generated_helpers_verify() {
        assert!(lamella_ir::verify(&write_int_mir()).is_ok(), "__write_int");
        for string in [None, Some(STRING)] {
            assert!(
                lamella_ir::verify(&int_to_string_mir(string)).is_ok(),
                "__int_to_string, string handle {string:?}"
            );
            assert!(
                lamella_ir::verify(&string_concat_mir(string)).is_ok(),
                "__string_concat, string handle {string:?}"
            );
        }
        #[cfg(any(feature = "wasm", feature = "riscv32"))]
        assert!(lamella_ir::verify(&string_eq_mir()).is_ok(), "__string_eq");
    }

    /// A RUNTIME-BUILT string must be allocated against `System.String` itself, not a synthetic
    /// UTF-16-unit array -- otherwise `s.ToString()` on one dispatches through `System.Array`'s vtable
    /// and `o is string` on one answers FALSE, which is what this whole shape is here to fix.
    ///
    /// Follows the ALLOCATION'S OPERAND to the instruction that defines it rather than merely finding a
    /// `TypeDescAddr` somewhere in the body: an unrelated descriptor address in the same function would
    /// satisfy the weaker check while the allocation still named an array. The absence of `AllocArray`
    /// is asserted too, since leaving one behind is how half a conversion would read as a whole one.
    #[test]
    fn a_built_string_allocates_against_system_string() {
        for (name, f) in [
            ("__string_concat", string_concat_mir(Some(STRING))),
            ("__int_to_string", int_to_string_mir(Some(STRING))),
        ] {
            let insts: Vec<&Inst> = f.blocks.iter().flat_map(|b| &b.insts).map(|(_, i)| i).collect();
            assert!(
                !insts.iter().any(|i| matches!(i, Inst::AllocArray { .. })),
                "{name} must no longer allocate an array"
            );
            let descriptor = f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find_map(|(_, i)| match i {
                    Inst::AllocDescribed { descriptor, .. } => Some(*descriptor),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} must allocate with AllocDescribed"));
            let defines = f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|(id, _)| *id == descriptor)
                .map(|(_, i)| i)
                .unwrap_or_else(|| panic!("{name}'s descriptor operand has no definition"));
            assert!(
                matches!(defines, Inst::TypeDescAddr { handle } if handle.0 == STRING),
                "{name} must allocate against System.String's own descriptor, got {defines:?}"
            );
        }
    }

    /// The built string's HEADER and COPY WIDTHS must match the tier this build stores, exactly as
    /// [`string_blob_bytes`] does for a literal -- the two are the same layout decision made at run time
    /// and at build time, and a build where they disagree produces strings that cannot be compared to
    /// their own literals.
    ///
    /// Reads the MIR rather than the source, and keyed on the `FieldStore` OFFSETS (the header words)
    /// and the `ArrayLoad`/`ArrayStore` widths (the storage), which is what a consumer actually sees.
    #[test]
    fn the_built_string_header_matches_this_builds_storage_tier() {
        let f = string_concat_mir(Some(STRING));
        let stores: Vec<u32> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::FieldStore { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        let byte_len_reads = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|(_, i)| matches!(i, Inst::FieldLoad { offset: 4, .. }))
            .count();
        let elements: Vec<u32> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::ArrayLoad { element_size, .. } | Inst::ArrayStore { element_size, .. } => {
                    Some(*element_size)
                }
                _ => None,
            })
            .collect();
        let direct: Vec<u32> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::Load { width, .. } | Inst::Store { width, .. } => Some(*width),
                _ => None,
            })
            .collect();
        if STORAGE_IS_BYTES {
            assert_eq!(stores, vec![0, 4], "byte storage carries BOTH header words");
            assert_eq!(
                byte_len_reads, 2,
                "the payload is sized from the operands' BYTE lengths -- a unit count cannot give it, \
                 which is the whole reason AllocArray could not express this tier"
            );
            assert_eq!(direct, vec![1, 1, 1, 1], "byte storage copies BYTES");
            assert!(
                elements.is_empty(),
                "byte storage must NOT be reached through a bounds-checked array access"
            );
        } else {
            assert_eq!(stores, vec![0], "unit storage has only the count word");
            assert_eq!(byte_len_reads, 0, "there is no byte-length word in the unit tier");
            assert_eq!(elements, vec![2, 2, 2, 2], "unit storage copies UTF-16 UNITS");
            assert!(
                direct.is_empty(),
                "unit storage indexes by UNIT, where the array bound is exactly right"
            );
        }
    }

    /// Without a nameable `System.String` -- the FLAT path -- both helpers keep the body they have
    /// always had, synthetic array handle included. Pinned because the flat path has no linker and no
    /// canonical descriptors, so "improving" it would allocate against a descriptor that is not there.
    #[test]
    fn no_string_handle_leaves_the_helpers_as_they_were() {
        for (name, f) in [
            ("__string_concat", string_concat_mir(None)),
            ("__int_to_string", int_to_string_mir(None)),
        ] {
            let handles: Vec<u32> = f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .filter_map(|(_, i)| match i {
                    Inst::AllocArray { handle, .. } => Some(handle.0),
                    _ => None,
                })
                .collect();
            assert_eq!(
                handles,
                vec![lamella_ir::synthetic_array_handle(ELEMENT_KIND_UTF16_UNIT).0],
                "{name} without a string handle"
            );
            assert!(
                !f.blocks
                    .iter()
                    .flat_map(|b| &b.insts)
                    .any(|(_, i)| matches!(i, Inst::AllocDescribed { .. })),
                "{name} must not allocate against a descriptor the flat path never lays"
            );
        }
    }

    /// The storage layout, asserted per tier IN the tier -- so the three differ by a test rather than
    /// by prose, and a build that silently ignores its feature fails here rather than on silicon.
    ///
    /// `"A\u{1F600}"` is chosen because it separates all three: one BMP unit plus a surrogate PAIR,
    /// which must combine into ONE four-byte code point under both UTF-8 tiers. Encoding the pair as
    /// two three-byte surrogates instead is CESU-8, and it passes every vector without a pair.
    #[test]
    fn the_string_blob_matches_this_builds_storage_tier() {
        let units = [0x0041u16, 0xD83D, 0xDE00];
        let blob = string_blob_bytes(&units).expect("a well-formed literal encodes in every tier");
        assert_eq!(&blob[0..4], &3u32.to_le_bytes(), "unit count leads every tier");
        if STORAGE_IS_BYTES {
            let expected: &[u8] = &[0x41, 0xF0, 0x9F, 0x98, 0x80];
            assert_eq!(
                &blob[4..8],
                &(expected.len() as u32).to_le_bytes(),
                "the byte length is the second word"
            );
            assert_eq!(&blob[8..], expected, "a surrogate PAIR is one 4-byte code point, not CESU-8");
        } else {
            assert_eq!(blob.len(), 4 + units.len() * 2);
            assert_eq!(&blob[4..], &[0x41, 0x00, 0x3D, 0xD8, 0x00, 0xDE]);
        }
    }

    /// Where the three tiers actually differ: a LONE surrogate. WTF-8 preserves it as its own
    /// three-byte encoding, which is the whole reason that tier exists; the UTF-16 tier stores the unit
    /// itself; and strict UTF-8 REFUSES it. Asserted in all three directions so none can quietly become
    /// another.
    ///
    /// The strict arm used to assert `[0xEF, 0xBF, 0xBD]` -- U+FFFD, the silent replacement. That is
    /// the assertion this change inverts: substituting is `Encoding.UTF8`'s default fallback and the
    /// wrong rule for string CONSTRUCTION, which never loses data.
    #[test]
    fn a_lone_surrogate_separates_the_three_tiers() {
        let encoded = encode_string_bytes(&[0xD800]);
        if cfg!(feature = "string-utf8-wtf8") {
            assert_eq!(
                encoded,
                Ok(alloc::vec![0xED, 0xA0, 0x80]),
                "WTF-8 preserves a lone surrogate"
            );
        } else if cfg!(feature = "string-utf8") {
            assert_eq!(
                encoded,
                Err(UnencodableUnit {
                    unit: 0xD800,
                    index: 0
                }),
                "strict UTF-8 REFUSES a lone surrogate -- it does not replace it with U+FFFD"
            );
        } else {
            assert_eq!(
                encoded,
                Ok(alloc::vec![0x00, 0xD8]),
                "the UTF-16 tier stores the unit itself"
            );
        }
        if STORAGE_IS_BYTES {
            assert_eq!(
                encode_string_bytes(&[0xD834, 0xDD1E]),
                Ok(alloc::vec![0xF0, 0x9D, 0x84, 0x9E])
            );
        }
    }

    /// The REPORTED INDEX counts UTF-16 CODE UNITS, not scalars -- the same distinction the
    /// interpreter's `EncoderFallbackException.Index` had to get right, and for the same reason: the
    /// two numbers only diverge once a SUPPLEMENTARY character precedes the offending unit, so the
    /// obvious one-surrogate vector cannot tell them apart.
    ///
    /// `[D83D, DE00, D800]` is that vector: one pair (two units, ONE scalar) then a lone high
    /// surrogate. Units say 2; scalars would say 1.
    #[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
    #[test]
    fn the_refusal_reports_a_unit_index_not_a_scalar_index() {
        assert_eq!(
            encode_string_bytes(&[0xD83D, 0xDE00, 0xD800]),
            Err(UnencodableUnit {
                unit: 0xD800,
                index: 2
            }),
            "the index is the offending unit's own position in UTF-16 units"
        );
    }

    /// A refusal must reach the CALLER as a blob failure, not be swallowed into a shorter blob: the
    /// whole point is that the build stops. Keyed on the blob entry point rather than the encoder, since
    /// that is what the backends call.
    #[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
    #[test]
    fn an_unencodable_literal_refuses_the_blob() {
        assert!(
            string_blob_bytes(&[0x0041, 0xDC00]).is_err(),
            "a literal with a lone surrogate has no blob under strict UTF-8"
        );
        assert!(
            string_blob_bytes(&[0x0041, 0xD83D, 0xDE00]).is_ok(),
            "a well-formed pair still encodes"
        );
    }

    /// The byte tiers compare storage BYTES and must check the byte length, because equal unit counts
    /// no longer imply equal storage. Pins the patch's three edits through the MIR rather than by
    /// reading the source.
    #[cfg(any(feature = "wasm", feature = "riscv32"))]
    #[test]
    fn string_equality_reads_the_layout_this_build_stores() {
        let f = string_eq_mir();
        let loads: Vec<u32> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::ArrayLoad { element_size, .. } => Some(element_size),
                _ => None,
            })
            .copied()
            .collect();
        let byte_len_reads = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|(_, i)| matches!(i, Inst::FieldLoad { offset: 4, .. }))
            .count();
        let direct: Vec<u32> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::Load { width, .. } => Some(*width),
                _ => None,
            })
            .collect();
        if STORAGE_IS_BYTES {
            assert_eq!(direct, vec![1, 1], "byte storage compares BYTES");
            assert!(
                loads.is_empty(),
                "byte storage must NOT be reached through a bounds-checked array access -- the check \
                 is against the UNIT count while this loop's index runs to the BYTE length"
            );
            assert_eq!(
                byte_len_reads, 2,
                "both byte lengths must be read and compared -- equal unit counts do not imply \
                 equal storage once a code point can span a variable number of bytes"
            );
        } else {
            assert_eq!(loads, vec![2, 2], "unit storage compares UTF-16 UNITS");
            assert!(direct.is_empty(), "unit storage indexes by UNIT");
            assert_eq!(byte_len_reads, 0, "there is no byte-length word in the unit tier");
        }
    }

    /// `__write_int` must reach the console through the seam and nothing else -- if the `PInvoke`
    /// were dropped the helper would still verify, still lower, and silently print nothing.
    #[test]
    fn write_int_helper_calls_the_console_seam() {
        let f = write_int_mir();
        let calls: Vec<&str> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::PInvoke { import, .. } => Some(&**import),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![CONSOLE_WRITE_BYTES]);
    }

    /// The buffer pointer must NOT be a garbage-collector root type. The root walk keys on a slot's
    /// `MirType`, and the collector treats a non-null root as a heap payload with no range check --
    /// so typing the stack-buffer address as `ManagedPtr`/`ObjectRef` would hand it a stack address
    /// to trace. Pinned as a test because the failure would be a rare, data-dependent heap corruption.
    #[test]
    fn write_int_holds_no_gc_root_slots() {
        let f = write_int_mir();
        assert!(
            !f.value_types.iter().any(|t| t.is_gc_reference()),
            "no slot in __write_int is enumerated as a GC root"
        );
    }

    /// The rewrite replaces `WriteInt` with a call to the APPENDED helper, and leaves no marker
    /// behind for a backend to reject.
    #[test]
    fn lower_write_int_rewrites_to_the_appended_helper() {
        let caller = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 42,
                        },
                    ),
                    (ValueId(1), Inst::WriteInt { value: ValueId(0) }),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let mut program = vec![caller];
        lower_write_int(&mut program);
        assert_eq!(program.len(), 2, "the helper is appended");
        assert!(
            matches!(
                program[0].blocks[0].insts[1].1,
                Inst::Call { callee: 1, .. }
            ),
            "the WriteInt became a call to the helper at index 1"
        );
        assert!(
            !program
                .iter()
                .flat_map(|f| &f.blocks)
                .flat_map(|b| &b.insts)
                .any(|(_, i)| matches!(i, Inst::WriteInt { .. })),
            "no WriteInt marker survives the rewrite"
        );
    }

    /// A program with no `WriteInt` is left completely alone -- no helper, no renumbering.
    #[test]
    fn lower_write_int_is_a_no_op_without_a_write_int() {
        let mut program = vec![Function {
            params: Vec::new(),
            ret: None,
            value_types: Vec::new(),
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(None)),
            }],
        }];
        lower_write_int(&mut program);
        assert_eq!(program.len(), 1);
    }
}
