//! The Thumb-2 instruction encoder and its relocation model.

use alloc::vec::Vec;

/// A location inside the image being built, resolved by the encoder itself.
///
/// Mint one with [`Encoder::new_label`], fix its position with
/// [`Encoder::bind_label`] when the target instruction is emitted, and reference
/// it from an instruction or data directive. Labels left unbound at
/// [`Encoder::finish`] are reported, never silently zeroed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(u32);

/// The shape of a patch site: how the bytes at a reference encode their target.
///
/// Only the absolute 32-bit data word is modelled so far; the Thumb branch and
/// `MOVW`/`MOVT` forms, whose immediates are split across the encoding, are
/// added as their instructions are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelocKind {
    /// A full 32-bit little-endian word holding the target address.
    Abs32,
    /// A 16-bit unconditional Thumb branch (`B` encoding T2): its low 11 bits
    /// take the halfword-scaled PC-relative offset to the target (Armv6-M ARM
    /// (DDI 0419E), A6.7.10), a reach of about +/-2 KB.
    ThumbBranch11,
    /// A 16-bit conditional Thumb branch (`B<c>` encoding T1): its low 8 bits
    /// take the halfword-scaled PC-relative offset (A6.7.10), a reach of about
    /// +/-256 bytes.
    ThumbBranchCond8,
    /// A relaxed conditional branch occupying TWO halfwords: an inverted `B<!c>`
    /// over a following `B` (encoding T2). [`Encoder::finish`] grows a
    /// [`RelocKind::ThumbBranchCond8`] into this when its +/-256-byte reach is
    /// exceeded -- ARMv6-M has no wide conditional branch, so the condition is
    /// inverted to skip an unconditional `B` with the wider +/-2 KB reach.
    ThumbBranchCond8Long,
    /// A PC-relative literal load (`LDR` (literal), encoding T1): the low 8 bits
    /// take the word-scaled distance from `Align(PC, 4)` to the pool entry
    /// (Armv6-M ARM (DDI 0419E), A6.7.27), which must lie ahead within about 1 KB.
    ThumbLdrLit8,
    /// A 32-bit `BL` call (encoding T1): a 24-bit signed, halfword-scaled
    /// PC-relative offset split as S:I1:I2:imm10:imm11 with the J1/J2 swizzle
    /// (Armv6-M ARM (DDI 0419E), A6.7.13), reach about +/-16 MB.
    ThumbCall,
}

/// A reference to an externally defined symbol, left for the link step.
///
/// The `symbol` is an opaque index the backend's link step maps to a concrete
/// address; this crate does not interpret it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reloc {
    /// Byte offset of the patch site within the finished image.
    pub at: u32,
    /// How the target address is encoded at the site.
    pub kind: RelocKind,
    /// The backend-assigned symbol the site refers to.
    pub symbol: u32,
}

/// Why an encode could not be completed.
///
/// Every encoder either succeeds or returns one of these; none panic on a
/// request the caller can legitimately make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssembleError {
    /// A [`Label`] was referenced but never bound to a position.
    UnboundLabel(Label),
    /// An operand cannot be represented in the chosen encoding, such as a high
    /// register where a 16-bit Thumb form admits only R0-R7.
    UnencodableOperand,
    /// A branch's target is too far away, or misaligned, for its encoding.
    BranchOutOfRange {
        /// Byte offset of the branch instruction that cannot reach its target.
        at: u32,
    },
}

/// The finished output: the machine-code bytes and any unresolved relocations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assembled {
    /// The little-endian Thumb byte image.
    pub bytes: Vec<u8>,
    /// References to external symbols the link step must still resolve.
    pub relocs: Vec<Reloc>,
    /// Each label's FINAL bound offset (after branch relaxation), or `None` if it was never bound --
    /// so a caller that captured `Label`s (e.g. one per function) can read the post-relaxation layout.
    labels: Vec<Option<u32>>,
}

impl Assembled {
    /// The final byte offset of `label` in [`Assembled::bytes`], after relaxation; `None` if the label
    /// was never bound. A caller binds a label at each region of interest, then reads the true layout
    /// here -- correct even when relaxation grew the image (unlike an offset captured during emission).
    #[must_use]
    pub fn label_position(&self, label: Label) -> Option<u32> {
        self.labels.get(label.0 as usize).copied().flatten()
    }

    /// The final position of a label by its raw id (from [`Encoder::safepoint_label`]) -- for resolving
    /// a stack-map entry's `return_pc`, stored as a label id during lowering, after relaxation.
    #[must_use]
    pub fn label_position_by_id(&self, id: u32) -> Option<u32> {
        self.label_position(Label(id))
    }
}

/// Accumulates Thumb machine code and the references into it.
#[derive(Debug, Clone, Default)]
pub struct Encoder {
    bytes: Vec<u8>,
    /// `labels[i]` is the bound byte offset of label `i`, or `None` until bound.
    labels: Vec<Option<u32>>,
    /// Internal references to patch in `finish`: `(site, kind, label index)`.
    fixups: Vec<(u32, RelocKind, u32)>,
    /// Position-independent data words to patch in `finish`: `(site, from label, to label)`, each
    /// filled with `to_offset - from_offset` -- a placement-invariant relative reference (a vtable
    /// entry relative to its type descriptor, so the image works wherever it is loaded).
    diffs: Vec<(u32, u32, u32)>,
    relocs: Vec<Reloc>,
}

use crate::cond::Cond;
use crate::register::Reg;

impl Encoder {
    /// Creates an empty encoder.
    #[must_use]
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// The current byte offset, i.e. where the next emitted byte lands.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.bytes.len() as u32
    }

    /// The bytes emitted so far, before relocations are resolved.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Creates a fresh, unbound label.
    pub fn new_label(&mut self) -> Label {
        let id = self.labels.len() as u32;
        self.labels.push(None);
        Label(id)
    }

    /// Binds `label` to the current position. A label bound more than once keeps
    /// its latest position; a label from another encoder is ignored rather than
    /// allowed to panic.
    pub fn bind_label(&mut self, label: Label) {
        let here = self.position();
        if let Some(slot) = self.labels.get_mut(label.0 as usize) {
            *slot = Some(here);
        }
    }

    /// Binds a fresh label at the current position and returns its raw id -- for recording a SAFEPOINT
    /// return address that must survive branch relaxation. The id is stored in a stack-map entry's
    /// `return_pc` during lowering and resolved to the final offset via [`Assembled::label_position`]
    /// after [`Encoder::finish`]; a bare `position()` would capture a pre-relaxation offset.
    pub fn safepoint_label(&mut self) -> u32 {
        let label = self.new_label();
        self.bind_label(label);
        label.0
    }

    /// Appends one 16-bit halfword, low byte first.
    pub fn emit_u16(&mut self, halfword: u16) {
        self.bytes.extend_from_slice(&halfword.to_le_bytes());
    }

    /// Appends a 32-bit Thumb instruction as its two halfwords, `hw1` (the
    /// lower address) first, each low byte first (Armv6-M ARM (DDI 0419E), A5.3).
    pub fn emit_thumb32(&mut self, hw1: u16, hw2: u16) {
        self.emit_u16(hw1);
        self.emit_u16(hw2);
    }

    /// `BX Rm` -- branch and exchange to the address in `Rm`; `BX LR` is the
    /// canonical return. 16-bit encoding T1 (Armv6-M ARM (DDI 0419E), A6.7.15).
    pub fn bx(&mut self, rm: Reg) {
        self.emit_u16(0x4700 | (u16::from(rm.number()) << 3));
    }

    /// `NOP` -- the hint that does nothing. 16-bit encoding T1 (A6.7.47).
    pub fn nop(&mut self) {
        self.emit_u16(0xBF00);
    }

    /// `PUSH {LR}` -- the leaf-call prologue, saving the return address. 16-bit
    /// encoding T1 with the M bit set (A6.7.50).
    pub fn push_lr(&mut self) {
        self.emit_u16(0xB500);
    }

    /// `POP {PC}` -- the matching epilogue, returning by loading the saved
    /// address into the program counter. 16-bit encoding T1 with the P bit set
    /// (A6.7.49).
    pub fn pop_pc(&mut self) {
        self.emit_u16(0xBD00);
    }

    /// `PUSH {registers}` -- push the given low registers, and LR when `lr` is set.
    /// 16-bit encoding T1 (A6.7.50); `registers` is a bitmask of R0-R7.
    pub fn push_registers(&mut self, registers: u8, lr: bool) {
        self.emit_u16(0xB400 | (u16::from(lr) << 8) | u16::from(registers));
    }

    /// `POP {registers}` -- pop the given low registers, and PC when `pc` is set.
    /// 16-bit encoding T1 (A6.7.49); `registers` is a bitmask of R0-R7.
    pub fn pop_registers(&mut self, registers: u8, pc: bool) {
        self.emit_u16(0xBC00 | (u16::from(pc) << 8) | u16::from(registers));
    }

    /// `ADDS Rd, Rn, Rm` -- add two registers, setting flags. 16-bit encoding T1
    /// (A6.7.3), which admits only the low registers R0-R7; a high register
    /// yields [`AssembleError::UnencodableOperand`].
    pub fn adds(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rd.is_low() && rn.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        let encoding = 0x1800
            | (u16::from(rm.number()) << 6)
            | (u16::from(rn.number()) << 3)
            | u16::from(rd.number());
        self.emit_u16(encoding);
        Ok(())
    }

    /// `MOVS Rd, #imm8` -- move an 8-bit immediate into a low register, setting
    /// flags. 16-bit encoding T1 (Armv6-M ARM (DDI 0419E), A6.7.39); `Rd` is a
    /// 3-bit field, so only R0-R7 encode.
    pub fn movs_imm(&mut self, rd: Reg, imm8: u8) -> Result<(), AssembleError> {
        if !rd.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x2000 | (u16::from(rd.number()) << 8) | u16::from(imm8));
        Ok(())
    }

    /// `MOV Rd, Rm` -- copy a register without setting flags; either register may
    /// be high. 16-bit encoding T1 (A6.7.40), where the destination's high bit
    /// rides in bit 7 (`d = D:Rd`).
    pub fn mov_reg(&mut self, rd: Reg, rm: Reg) {
        let high = u16::from(rd.number() >> 3) & 1;
        let rd_low = u16::from(rd.number() & 0x7);
        self.emit_u16(0x4600 | (high << 7) | (u16::from(rm.number()) << 3) | rd_low);
    }

    /// `MOVS Rd, Rm` -- copy a low register and set flags. 16-bit encoding T2
    /// (A6.7.40), which shares the shift encoding (it is `LSLS Rd, Rm, #0`) and so
    /// admits only R0-R7.
    pub fn movs_reg(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rd.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16((u16::from(rm.number()) << 3) | u16::from(rd.number()));
        Ok(())
    }

    /// `ADDS Rd, Rn, #imm3` -- add a 3-bit immediate, setting flags. 16-bit
    /// encoding T1 (A6.7.2); low registers only, `imm3` in 0..=7.
    pub fn adds_imm3(&mut self, rd: Reg, rn: Reg, imm3: u8) -> Result<(), AssembleError> {
        if !(rd.is_low() && rn.is_low()) || imm3 > 7 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x1C00
                | (u16::from(imm3) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rd.number()),
        );
        Ok(())
    }

    /// `ADDS Rdn, #imm8` -- add an 8-bit immediate to a low register, setting
    /// flags. 16-bit encoding T2 (A6.7.2); `imm8` in 0..=255.
    pub fn adds_imm8(&mut self, rdn: Reg, imm8: u8) -> Result<(), AssembleError> {
        if !rdn.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x3000 | (u16::from(rdn.number()) << 8) | u16::from(imm8));
        Ok(())
    }

    /// `SUBS Rd, Rn, #imm3` -- subtract a 3-bit immediate, setting flags. 16-bit
    /// encoding T1 (A6.7.65); low registers only, `imm3` in 0..=7.
    pub fn subs_imm3(&mut self, rd: Reg, rn: Reg, imm3: u8) -> Result<(), AssembleError> {
        if !(rd.is_low() && rn.is_low()) || imm3 > 7 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x1E00
                | (u16::from(imm3) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rd.number()),
        );
        Ok(())
    }

    /// `SUBS Rdn, #imm8` -- subtract an 8-bit immediate from a low register,
    /// setting flags. 16-bit encoding T2 (A6.7.65); `imm8` in 0..=255.
    pub fn subs_imm8(&mut self, rdn: Reg, imm8: u8) -> Result<(), AssembleError> {
        if !rdn.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x3800 | (u16::from(rdn.number()) << 8) | u16::from(imm8));
        Ok(())
    }

    /// `CMP Rn, #imm8` -- compare a low register with an 8-bit immediate, setting
    /// flags from `Rn - imm8` and discarding the result. 16-bit encoding T1
    /// (A6.7.17); `imm8` in 0..=255.
    pub fn cmp_imm(&mut self, rn: Reg, imm8: u8) -> Result<(), AssembleError> {
        if !rn.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x2800 | (u16::from(rn.number()) << 8) | u16::from(imm8));
        Ok(())
    }

    /// `CMP Rn, Rm` -- compare two low registers, setting flags from `Rn - Rm`.
    /// 16-bit encoding T1 (A6.7.18, low-register form).
    pub fn cmp_reg(&mut self, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rn.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x4280 | (u16::from(rm.number()) << 3) | u16::from(rn.number()));
        Ok(())
    }

    /// `LSLS Rd, Rm, #imm5` -- logical shift left by an immediate, setting flags.
    /// 16-bit encoding T1 (A6.7.35); low registers, `imm5` in 0..=31 (a shift of
    /// 0 coincides with the `MOV (register)` encoding).
    pub fn lsls_imm(&mut self, rd: Reg, rm: Reg, imm5: u8) -> Result<(), AssembleError> {
        if !(rd.is_low() && rm.is_low()) || imm5 > 31 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            (u16::from(imm5) << 6) | (u16::from(rm.number()) << 3) | u16::from(rd.number()),
        );
        Ok(())
    }

    /// `LSRS Rd, Rm, #imm5` -- logical (zero-filling) shift right by an immediate. 16-bit
    /// encoding T1 (ARMv6-M ARM, LSR (immediate)): `0000 1 imm5 Rm Rd`, i.e. `LSLS` with bit
    /// 11 set. Low registers; `imm5` in 1..=31 (the ARM encoding reads 0 as a shift of 32,
    /// which this lowering never emits).
    pub fn lsrs_imm(&mut self, rd: Reg, rm: Reg, imm5: u8) -> Result<(), AssembleError> {
        if !(rd.is_low() && rm.is_low()) || imm5 > 31 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x0800
                | (u16::from(imm5) << 6)
                | (u16::from(rm.number()) << 3)
                | u16::from(rd.number()),
        );
        Ok(())
    }

    /// `LDR Rt, [Rn, #imm]` -- load a word from `Rn + imm`. 16-bit encoding T1
    /// (A6.7.26); low registers, `imm` a multiple of 4 in 0..=124.
    pub fn ldr_imm(&mut self, rt: Reg, rn: Reg, imm: u16) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low()) || imm % 4 != 0 || imm > 124 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x6800 | ((imm / 4) << 6) | (u16::from(rn.number()) << 3) | u16::from(rt.number()),
        );
        Ok(())
    }

    /// `STR Rt, [Rn, #imm]` -- store a word to `Rn + imm`. 16-bit encoding T1
    /// (STR (immediate); A5.2.4, the load/store group), which is `LDR` with bit 11
    /// clear. Low registers, `imm` a multiple of 4 in 0..=124.
    pub fn str_imm(&mut self, rt: Reg, rn: Reg, imm: u16) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low()) || imm % 4 != 0 || imm > 124 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x6000 | ((imm / 4) << 6) | (u16::from(rn.number()) << 3) | u16::from(rt.number()),
        );
        Ok(())
    }

    /// `LDR Rt, [SP, #imm]` -- load a word relative to the stack pointer. 16-bit
    /// encoding T2 (A6.7.26); low register, `imm` a multiple of 4 in 0..=1020.
    pub fn ldr_sp(&mut self, rt: Reg, imm: u16) -> Result<(), AssembleError> {
        if !rt.is_low() || imm % 4 != 0 || imm > 1020 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x9800 | (u16::from(rt.number()) << 8) | (imm / 4));
        Ok(())
    }

    /// `STR Rt, [SP, #imm]` -- store a word relative to the stack pointer. 16-bit
    /// encoding T2 (STR (immediate); A5.2.4), `LDR` with bit 11 clear. Low
    /// register, `imm` a multiple of 4 in 0..=1020.
    pub fn str_sp(&mut self, rt: Reg, imm: u16) -> Result<(), AssembleError> {
        if !rt.is_low() || imm % 4 != 0 || imm > 1020 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x9000 | (u16::from(rt.number()) << 8) | (imm / 4));
        Ok(())
    }

    /// The 16-bit register-offset load/store form, `0101 opB Rm Rn Rt` (Armv6-M
    /// ARM (DDI 0419E), A5.2.4, Table A5-5). All three registers must be low.
    fn ldst_reg(&mut self, opb: u16, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x5000
                | (opb << 9)
                | (u16::from(rm.number()) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rt.number()),
        );
        Ok(())
    }

    /// `STR Rt, [Rn, Rm]` -- store a word (Table A5-5, opB 000).
    pub fn str_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b000, rt, rn, rm)
    }

    /// `STRH Rt, [Rn, Rm]` -- store a halfword (Table A5-5, opB 001).
    pub fn strh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b001, rt, rn, rm)
    }

    /// `STRB Rt, [Rn, Rm]` -- store a byte (Table A5-5, opB 010).
    pub fn strb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b010, rt, rn, rm)
    }

    /// `LDRSB Rt, [Rn, Rm]` -- load a sign-extended byte (Table A5-5, opB 011).
    pub fn ldrsb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b011, rt, rn, rm)
    }

    /// `LDR Rt, [Rn, Rm]` -- load a word (Table A5-5, opB 100).
    pub fn ldr_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b100, rt, rn, rm)
    }

    /// `LDRH Rt, [Rn, Rm]` -- load a zero-extended halfword (Table A5-5, opB 101).
    pub fn ldrh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b101, rt, rn, rm)
    }

    /// `LDRB Rt, [Rn, Rm]` -- load a zero-extended byte (Table A5-5, opB 110).
    pub fn ldrb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b110, rt, rn, rm)
    }

    /// `LDRSH Rt, [Rn, Rm]` -- load a sign-extended halfword (Table A5-5, opB 111).
    pub fn ldrsh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.ldst_reg(0b111, rt, rn, rm)
    }

    /// `STRB Rt, [Rn, #imm5]` -- store a byte. 16-bit encoding T1 (Table A5-5, opA
    /// 0111); low registers, `imm5` in 0..=31.
    pub fn strb_imm(&mut self, rt: Reg, rn: Reg, imm5: u8) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low()) || imm5 > 31 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x7000
                | (u16::from(imm5) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rt.number()),
        );
        Ok(())
    }

    /// `LDRB Rt, [Rn, #imm5]` -- load a zero-extended byte. 16-bit encoding T1
    /// (Table A5-5, opA 0111); low registers, `imm5` in 0..=31.
    pub fn ldrb_imm(&mut self, rt: Reg, rn: Reg, imm5: u8) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low()) || imm5 > 31 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x7800
                | (u16::from(imm5) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rt.number()),
        );
        Ok(())
    }

    /// `STRH Rt, [Rn, #imm]` -- store a halfword. 16-bit encoding T1 (Table A5-5,
    /// opA 1000); low registers, `imm` even in 0..=62.
    pub fn strh_imm(&mut self, rt: Reg, rn: Reg, imm: u8) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low()) || imm % 2 != 0 || imm > 62 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x8000
                | (u16::from(imm / 2) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rt.number()),
        );
        Ok(())
    }

    /// `LDRH Rt, [Rn, #imm]` -- load a zero-extended halfword. 16-bit encoding T1
    /// (Table A5-5, opA 1000); low registers, `imm` even in 0..=62.
    pub fn ldrh_imm(&mut self, rt: Reg, rn: Reg, imm: u8) -> Result<(), AssembleError> {
        if !(rt.is_low() && rn.is_low()) || imm % 2 != 0 || imm > 62 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x8800
                | (u16::from(imm / 2) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rt.number()),
        );
        Ok(())
    }

    /// The 16-bit sign/zero-extend form, `1011 0010 op2 Rm Rd` (Armv6-M ARM
    /// (DDI 0419E), the extend instructions); low registers only.
    fn extend(&mut self, op2: u16, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rd.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0xB200 | (op2 << 6) | (u16::from(rm.number()) << 3) | u16::from(rd.number()));
        Ok(())
    }

    /// `SXTH Rd, Rm` -- sign-extend the low halfword to 32 bits (op2 00).
    pub fn sxth(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.extend(0b00, rd, rm)
    }

    /// `SXTB Rd, Rm` -- sign-extend the low byte to 32 bits (op2 01).
    pub fn sxtb(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.extend(0b01, rd, rm)
    }

    /// `UXTH Rd, Rm` -- zero-extend the low halfword to 32 bits (op2 10).
    pub fn uxth(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.extend(0b10, rd, rm)
    }

    /// `UXTB Rd, Rm` -- zero-extend the low byte to 32 bits (op2 11).
    pub fn uxtb(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.extend(0b11, rd, rm)
    }

    /// The 16-bit byte-reverse form, `1011 1010 op2 Rm Rd` (the REV instructions);
    /// low registers only.
    fn reverse(&mut self, op2: u16, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rd.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0xBA00 | (op2 << 6) | (u16::from(rm.number()) << 3) | u16::from(rd.number()));
        Ok(())
    }

    /// `REV Rd, Rm` -- reverse the byte order of a word (op2 00).
    pub fn rev(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.reverse(0b00, rd, rm)
    }

    /// `REV16 Rd, Rm` -- reverse the byte order within each halfword (op2 01).
    pub fn rev16(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.reverse(0b01, rd, rm)
    }

    /// `REVSH Rd, Rm` -- reverse the low halfword's bytes and sign-extend (op2 11).
    pub fn revsh(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.reverse(0b11, rd, rm)
    }

    /// `ADD Rdn, Rm` -- add two registers without setting flags, either of which
    /// may be high. 16-bit encoding T2 (A6.7.3); the destination's high bit is DN.
    pub fn add_high(&mut self, rdn: Reg, rm: Reg) {
        let dn = u16::from(rdn.number() >> 3) & 1;
        self.emit_u16(
            0x4400 | (dn << 7) | (u16::from(rm.number()) << 3) | u16::from(rdn.number() & 7),
        );
    }

    /// `CMP Rn, Rm` -- compare two registers, either of which may be high. 16-bit
    /// encoding T2 (A6.7.18); `Rn`'s high bit is N.
    pub fn cmp_high(&mut self, rn: Reg, rm: Reg) {
        let n = u16::from(rn.number() >> 3) & 1;
        self.emit_u16(
            0x4500 | (n << 7) | (u16::from(rm.number()) << 3) | u16::from(rn.number() & 7),
        );
    }

    /// The 16-bit data-processing register form, `0100 00 op Rm Rdn` (Armv6-M ARM
    /// (DDI 0419E), A5.2.2, Table A5-3). `a` occupies bits 2..0 and `b` bits 5..3;
    /// both must be low registers.
    fn dp_reg(&mut self, opcode: u16, a: Reg, b: Reg) -> Result<(), AssembleError> {
        if !(a.is_low() && b.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x4000 | (opcode << 6) | (u16::from(b.number()) << 3) | u16::from(a.number()),
        );
        Ok(())
    }

    /// `ANDS Rdn, Rm` -- bitwise AND, setting flags (opcode 0000).
    pub fn ands(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0000, rdn, rm)
    }

    /// `EORS Rdn, Rm` -- bitwise exclusive OR, setting flags (opcode 0001).
    pub fn eors(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0001, rdn, rm)
    }

    /// `LSLS Rdn, Rm` -- logical shift left by a register, setting flags (0010).
    pub fn lsls_reg(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0010, rdn, rm)
    }

    /// `LSRS Rdn, Rm` -- logical shift right by a register, setting flags (0011).
    pub fn lsrs_reg(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0011, rdn, rm)
    }

    /// `ASRS Rdn, Rm` -- arithmetic shift right by a register, flags (0100).
    pub fn asrs_reg(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0100, rdn, rm)
    }

    /// `ASRS Rd, Rm, #imm5` -- arithmetic shift right by 1-31 (used to spread the sign
    /// bit across the high word of an `int64`). 16-bit T1 (A6.7.9).
    pub fn asrs_imm(&mut self, rd: Reg, rm: Reg, imm5: u8) -> Result<(), AssembleError> {
        if !rd.is_low() || !rm.is_low() || imm5 == 0 || imm5 > 31 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x1000
                | (u16::from(imm5) << 6)
                | (u16::from(rm.number()) << 3)
                | u16::from(rd.number()),
        );
        Ok(())
    }

    /// `ADCS Rdn, Rm` -- add with carry, setting flags (opcode 0101).
    pub fn adcs(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0101, rdn, rm)
    }

    /// `SBCS Rdn, Rm` -- subtract with carry, setting flags (opcode 0110).
    pub fn sbcs(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0110, rdn, rm)
    }

    /// `RORS Rdn, Rm` -- rotate right by a register, setting flags (opcode 0111).
    pub fn rors(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b0111, rdn, rm)
    }

    /// `TST Rn, Rm` -- set flags on a bitwise AND, discarding the result (1000).
    pub fn tst(&mut self, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1000, rn, rm)
    }

    /// `RSBS Rd, Rn, #0` -- negate `Rn` into `Rd`, setting flags (opcode 1001).
    pub fn rsbs(&mut self, rd: Reg, rn: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1001, rd, rn)
    }

    /// `CMN Rn, Rm` -- compare negative, setting flags from `Rn + Rm` (1011).
    pub fn cmn(&mut self, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1011, rn, rm)
    }

    /// `ORRS Rdn, Rm` -- bitwise OR, setting flags (opcode 1100).
    pub fn orrs(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1100, rdn, rm)
    }

    /// `MULS Rdm, Rn, Rdm` -- multiply `Rdm` by `Rn` into `Rdm`, flags (1101).
    pub fn muls(&mut self, rdm: Reg, rn: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1101, rdm, rn)
    }

    /// `BICS Rdn, Rm` -- bit clear (`Rdn AND NOT Rm`), setting flags (1110).
    pub fn bics(&mut self, rdn: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1110, rdn, rm)
    }

    /// `MVNS Rd, Rm` -- bitwise NOT of `Rm` into `Rd`, setting flags (opcode 1111).
    pub fn mvns(&mut self, rd: Reg, rm: Reg) -> Result<(), AssembleError> {
        self.dp_reg(0b1111, rd, rm)
    }

    /// `SUBS Rd, Rn, Rm` -- subtract registers, setting flags. 16-bit encoding T1
    /// (A6.7.66; A5.2.1, the add/subtract group), `ADDS` register with bit 9 set.
    /// Low registers only.
    pub fn subs(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), AssembleError> {
        if !(rd.is_low() && rn.is_low() && rm.is_low()) {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(
            0x1A00
                | (u16::from(rm.number()) << 6)
                | (u16::from(rn.number()) << 3)
                | u16::from(rd.number()),
        );
        Ok(())
    }

    /// `ADD Rd, SP, #imm` -- compute a stack-relative address into a low register.
    /// 16-bit encoding T1 (A6.7.4); `imm` a multiple of 4 in 0..=1020.
    pub fn add_sp_imm(&mut self, rd: Reg, imm: u16) -> Result<(), AssembleError> {
        if !rd.is_low() || imm % 4 != 0 || imm > 1020 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0xA800 | (u16::from(rd.number()) << 8) | (imm / 4));
        Ok(())
    }

    /// `ADD SP, SP, #imm` -- raise the stack pointer (release a frame). 16-bit
    /// encoding T2 (A6.7.4); `imm` a multiple of 4 in 0..=508.
    pub fn add_sp(&mut self, imm: u16) -> Result<(), AssembleError> {
        if imm % 4 != 0 || imm > 508 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0xB000 | (imm / 4));
        Ok(())
    }

    /// `SUB SP, SP, #imm` -- lower the stack pointer (reserve a frame). 16-bit
    /// encoding T1 (SUB (SP minus immediate), A6.7.67); `imm` a multiple of 4 in
    /// 0..=508.
    pub fn sub_sp(&mut self, imm: u16) -> Result<(), AssembleError> {
        if imm % 4 != 0 || imm > 508 {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0xB080 | (imm / 4));
        Ok(())
    }

    /// `BKPT #imm8` -- breakpoint. With `imm8 == 0xAB` it is the semihosting
    /// request a debugger or QEMU intercepts. 16-bit encoding T1 (A6.7.12).
    pub fn bkpt(&mut self, imm8: u8) {
        self.emit_u16(0xBE00 | u16::from(imm8));
    }

    /// `UDF #imm8` -- permanently undefined instruction, used as a trap; executing
    /// it raises an undefined-instruction fault. 16-bit encoding T1 (A6.7.26 area;
    /// the conditional-branch `cond == 0b1110` slot).
    pub fn udf(&mut self, imm8: u8) {
        self.emit_u16(0xDE00 | u16::from(imm8));
    }

    /// Emits a literal 32-bit little-endian word -- a vector-table entry, an
    /// inline constant, or a literal-pool datum.
    pub fn emit_word(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Appends raw, already-encoded bytes -- for example a separately lowered
    /// function body -- to the image.
    pub fn emit_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Pads with a `NOP` if the next emission would not be 4-byte aligned, which
    /// a literal pool requires (the PC for a literal load is `Align(PC, 4)`).
    pub fn align_to_word(&mut self) {
        if self.position() % 4 != 0 {
            self.nop();
        }
    }

    /// `LDR Rt, <label>` -- load a 32-bit word from a PC-relative literal pool
    /// entry. 16-bit encoding T1 (A6.7.27); the entry must lie ahead of the load,
    /// 4-byte aligned, within about 1 KB. The offset is resolved in
    /// [`Encoder::finish`].
    pub fn ldr_literal(&mut self, rt: Reg, target: Label) -> Result<(), AssembleError> {
        if !rt.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        let at = self.position();
        self.fixups.push((at, RelocKind::ThumbLdrLit8, target.0));
        self.emit_u16(0x4800 | (u16::from(rt.number()) << 8));
        Ok(())
    }

    /// `ADR Rd, <label>` -- form the PC-relative address of a label (`ADD Rd, PC, #imm`).
    /// 16-bit encoding T1 (A6.7.7); the label must lie ahead, 4-byte aligned, within about
    /// 1 KB. Resolved in [`Encoder::finish`], reusing the literal-pool relocation -- `ADR`
    /// and a literal `LDR` share the PC-relative form, differing only in the opcode bits.
    pub fn adr(&mut self, rd: Reg, target: Label) -> Result<(), AssembleError> {
        if !rd.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        let at = self.position();
        self.fixups.push((at, RelocKind::ThumbLdrLit8, target.0));
        self.emit_u16(0xA000 | (u16::from(rd.number()) << 8));
        Ok(())
    }

    /// `B <label>` -- unconditional branch to a bound label. 16-bit encoding T2
    /// (A6.7.10); the PC-relative offset is resolved in [`Encoder::finish`],
    /// reaching about +/-2 KB ([`AssembleError::BranchOutOfRange`] otherwise).
    pub fn b(&mut self, target: Label) {
        let at = self.position();
        self.fixups.push((at, RelocKind::ThumbBranch11, target.0));
        self.emit_u16(0xE000);
    }

    /// `B<cond> <label>` -- conditional branch to a bound label. 16-bit encoding
    /// T1 (A6.7.10); reach about +/-256 bytes. The condition occupies bits 11..8
    /// and the offset is resolved in [`Encoder::finish`].
    pub fn b_cond(&mut self, cond: Cond, target: Label) {
        let at = self.position();
        self.fixups
            .push((at, RelocKind::ThumbBranchCond8, target.0));
        self.emit_u16(0xD000 | (u16::from(cond.encoding()) << 8));
    }

    /// `BL <label>` -- branch with link (a call to a bound label). 32-bit encoding
    /// T1 (A6.7.13); the J1/J2-swizzled, PC-relative offset (reach about +/-16 MB)
    /// is resolved in [`Encoder::finish`].
    pub fn bl(&mut self, target: Label) {
        let at = self.position();
        self.fixups.push((at, RelocKind::ThumbCall, target.0));
        self.emit_thumb32(0xF000, 0xD000);
    }

    /// `BL <external symbol>` -- a call (32-bit `BL`, encoding T1) to a symbol defined elsewhere,
    /// recorded as a [`Reloc`] ([`RelocKind::ThumbCall`]) for the link step rather than resolved
    /// here. The placeholder halfwords are overwritten by the linker (`R_ARM_THM_CALL`), so an
    /// object emitter (`arm32::lower_object`) uses this for a cross-object/intra-module call it wants
    /// the linker to see -- the BL twin of [`Encoder::data_word_symbol`].
    pub fn bl_symbol(&mut self, symbol: u32) {
        let at = self.position();
        self.relocs.push(Reloc {
            at,
            kind: RelocKind::ThumbCall,
            symbol,
        });
        self.emit_thumb32(0xF000, 0xD000);
    }

    /// `BLX Rm` -- branch with link and exchange to the address in `Rm` (an
    /// indirect call). 16-bit encoding T1 (A6.7.14).
    pub fn blx(&mut self, rm: Reg) {
        self.emit_u16(0x4780 | (u16::from(rm.number()) << 3));
    }

    /// Emits a 32-bit data word holding the address of `label`, to be patched in
    /// [`Encoder::finish`].
    pub fn data_word(&mut self, label: Label) {
        let at = self.position();
        self.fixups.push((at, RelocKind::Abs32, label.0));
        self.bytes.extend_from_slice(&[0; 4]);
    }

    /// Emits a 32-bit data word holding `to`'s offset minus `from`'s -- a placement-invariant relative
    /// reference, patched in [`Encoder::finish`]. A vtable entry uses this (the method's address
    /// relative to its type descriptor) so dispatch is correct wherever the image is loaded.
    pub fn data_word_diff(&mut self, from: Label, to: Label) {
        let at = self.position();
        self.diffs.push((at, from.0, to.0));
        self.bytes.extend_from_slice(&[0; 4]);
    }

    /// Emits a 32-bit data word referring to an external `symbol`, recorded as a
    /// [`Reloc`] for the link step.
    pub fn data_word_symbol(&mut self, symbol: u32) {
        let at = self.position();
        self.relocs.push(Reloc {
            at,
            kind: RelocKind::Abs32,
            symbol,
        });
        self.bytes.extend_from_slice(&[0; 4]);
    }

    /// Grows any conditional branch whose +/-256-byte reach is exceeded into the two-halfword
    /// inverted-skip form (ARMv6-M has no wide `B<c>`): `B<!cond>` over a following `B`, which
    /// reaches +/-2 KB. Inserting the extra halfword shifts every later label and reference, so a
    /// grown branch can push another out of range -- the pass repeats until all are in range. Each
    /// conditional branch grows at most once (it then has the wider reach), so this terminates.
    fn relax_conditional_branches(&mut self) -> Result<(), AssembleError> {
        loop {
            let mut grew = false;
            for idx in 0..self.fixups.len() {
                let (at, kind, label_id) = self.fixups[idx];
                if kind != RelocKind::ThumbBranchCond8 {
                    continue;
                }
                let target = match self.labels.get(label_id as usize) {
                    Some(Some(offset)) => *offset,
                    _ => return Err(AssembleError::UnboundLabel(Label(label_id))),
                };
                let offset = i64::from(target) - (i64::from(at) + 4);
                if (-256..=254).contains(&offset) && offset % 2 == 0 {
                    continue;
                }
                let insert = (at + 2) as usize;
                self.bytes.splice(insert..insert, [0x00, 0xE0, 0x00, 0xBF]);
                for slot in self.labels.iter_mut().flatten() {
                    if *slot >= at + 2 {
                        *slot += 4;
                    }
                }
                for (fixup_at, _, _) in &mut self.fixups {
                    if *fixup_at >= at + 2 {
                        *fixup_at += 4;
                    }
                }
                for reloc in &mut self.relocs {
                    if reloc.at >= at + 2 {
                        reloc.at += 4;
                    }
                }
                self.fixups[idx].1 = RelocKind::ThumbBranchCond8Long;
                grew = true;
                break;
            }
            if !grew {
                return Ok(());
            }
        }
    }

    /// Resolves every internal label reference and returns the finished image
    /// plus the external relocations the link step must still apply.
    ///
    /// The resolved value of a label is its byte offset within this image, which
    /// stands in for a load address until the AOT driver assigns sections.
    /// Returns [`AssembleError::UnboundLabel`] if any referenced label was never
    /// bound.
    pub fn finish(mut self) -> Result<Assembled, AssembleError> {
        self.relax_conditional_branches()?;
        let branch_offset =
            |at: u32, target: u32, min: i64, max: i64| -> Result<u16, AssembleError> {
                let offset = i64::from(target) - (i64::from(at) + 4);
                if offset % 2 != 0 || offset < min || offset > max {
                    return Err(AssembleError::BranchOutOfRange { at });
                }
                Ok((offset >> 1) as u16)
            };
        for (at, kind, label_id) in &self.fixups {
            let target = match self.labels.get(*label_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(*label_id))),
            };
            let site = *at as usize;
            match kind {
                RelocKind::Abs32 => {
                    if let Some(slot) = self.bytes.get_mut(site..site + 4) {
                        slot.copy_from_slice(&target.to_le_bytes());
                    }
                }
                RelocKind::ThumbBranch11 => {
                    let imm = branch_offset(*at, target, -2048, 2046)?;
                    if let Some(slot) = self.bytes.get_mut(site..site + 2) {
                        slot.copy_from_slice(&(0xE000 | (imm & 0x07FF)).to_le_bytes());
                    }
                }
                RelocKind::ThumbBranchCond8 => {
                    let imm = branch_offset(*at, target, -256, 254)?;
                    if let Some(slot) = self.bytes.get_mut(site..site + 2) {
                        let base = u16::from_le_bytes([slot[0], slot[1]]) & 0xFF00;
                        slot.copy_from_slice(&(base | (imm & 0x00FF)).to_le_bytes());
                    }
                }
                RelocKind::ThumbBranchCond8Long => {
                    let cond = self
                        .bytes
                        .get(site..site + 2)
                        .map_or(0, |s| (u16::from_le_bytes([s[0], s[1]]) >> 8) & 0xF);
                    let inverted = cond ^ 1;
                    if let Some(slot) = self.bytes.get_mut(site..site + 2) {
                        slot.copy_from_slice(&(0xD000 | (inverted << 8) | 1).to_le_bytes());
                    }
                    let imm = branch_offset(*at + 2, target, -2048, 2046)?;
                    if let Some(slot) = self.bytes.get_mut(site + 2..site + 4) {
                        slot.copy_from_slice(&(0xE000 | (imm & 0x07FF)).to_le_bytes());
                    }
                }
                RelocKind::ThumbLdrLit8 => {
                    let pc = i64::from((*at + 4) & !3u32);
                    let offset = i64::from(target) - pc;
                    if !(0..=1020).contains(&offset) || offset % 4 != 0 {
                        return Err(AssembleError::BranchOutOfRange { at: *at });
                    }
                    let imm8 = (offset / 4) as u16;
                    if let Some(slot) = self.bytes.get_mut(site..site + 2) {
                        let base = u16::from_le_bytes([slot[0], slot[1]]) & 0xFF00;
                        slot.copy_from_slice(&(base | (imm8 & 0x00FF)).to_le_bytes());
                    }
                }
                RelocKind::ThumbCall => {
                    let off = i64::from(target) - (i64::from(*at) + 4);
                    if off % 2 != 0 || !(-16_777_216..=16_777_214).contains(&off) {
                        return Err(AssembleError::BranchOutOfRange { at: *at });
                    }
                    let s = ((off >> 24) & 1) as u16;
                    let i1 = ((off >> 23) & 1) as u16;
                    let i2 = ((off >> 22) & 1) as u16;
                    let imm10 = ((off >> 12) & 0x3FF) as u16;
                    let imm11 = ((off >> 1) & 0x7FF) as u16;
                    let j1 = (!(i1 ^ s)) & 1;
                    let j2 = (!(i2 ^ s)) & 1;
                    let hw1 = 0xF000 | (s << 10) | imm10;
                    let hw2 = 0xD000 | (j1 << 13) | (j2 << 11) | imm11;
                    if let Some(slot) = self.bytes.get_mut(site..site + 4) {
                        slot[0..2].copy_from_slice(&hw1.to_le_bytes());
                        slot[2..4].copy_from_slice(&hw2.to_le_bytes());
                    }
                }
            }
        }
        for &(at, from_id, to_id) in &self.diffs {
            let from = match self.labels.get(from_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(from_id))),
            };
            let to = match self.labels.get(to_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(to_id))),
            };
            let diff = (to as i32).wrapping_sub(from as i32) as u32;
            let site = at as usize;
            if let Some(slot) = self.bytes.get_mut(site..site + 4) {
                slot.copy_from_slice(&diff.to_le_bytes());
            }
        }
        Ok(Assembled {
            bytes: self.bytes,
            relocs: self.relocs,
            labels: self.labels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes one instruction and returns its bytes.
    fn one(emit: impl FnOnce(&mut Encoder)) -> Vec<u8> {
        let mut enc = Encoder::new();
        emit(&mut enc);
        enc.as_bytes().to_vec()
    }

    #[test]
    fn bx_lr_is_the_canonical_return() {
        assert_eq!(one(|e| e.bx(Reg::LR)), [0x70, 0x47]);
    }

    #[test]
    fn fixed_sixteen_bit_encodings() {
        assert_eq!(one(Encoder::nop), [0x00, 0xBF]);
        assert_eq!(one(Encoder::push_lr), [0x00, 0xB5]);
        assert_eq!(one(Encoder::pop_pc), [0x00, 0xBD]);
    }

    #[test]
    fn adds_low_registers() {
        assert_eq!(
            one(|e| e.adds(Reg::R0, Reg::R1, Reg::R2).unwrap()),
            [0x88, 0x18]
        );
    }

    #[test]
    fn adds_rejects_high_registers_without_panicking() {
        let mut enc = Encoder::new();
        assert_eq!(
            enc.adds(Reg::R8, Reg::R0, Reg::R0),
            Err(AssembleError::UnencodableOperand)
        );
        assert!(
            enc.as_bytes().is_empty(),
            "a rejected encode must emit nothing"
        );
    }

    #[test]
    fn movs_immediate() {
        assert_eq!(one(|e| e.movs_imm(Reg::R0, 0x2A).unwrap()), [0x2A, 0x20]);
    }

    #[test]
    fn mov_register_allows_high() {
        assert_eq!(one(|e| e.mov_reg(Reg::R1, Reg::R8)), [0x41, 0x46]);
    }

    #[test]
    fn movs_register_low_only() {
        assert_eq!(one(|e| e.movs_reg(Reg::R2, Reg::R3).unwrap()), [0x1A, 0x00]);
    }

    #[test]
    fn adds_immediate_three_bit() {
        assert_eq!(
            one(|e| e.adds_imm3(Reg::R0, Reg::R1, 5).unwrap()),
            [0x48, 0x1D]
        );
    }

    #[test]
    fn adds_immediate_eight_bit() {
        assert_eq!(one(|e| e.adds_imm8(Reg::R3, 0x10).unwrap()), [0x10, 0x33]);
    }

    #[test]
    fn subs_immediate() {
        assert_eq!(
            one(|e| e.subs_imm3(Reg::R0, Reg::R1, 5).unwrap()),
            [0x48, 0x1F]
        );
        assert_eq!(one(|e| e.subs_imm8(Reg::R3, 0x10).unwrap()), [0x10, 0x3B]);
    }

    #[test]
    fn cmp_immediate_and_register() {
        assert_eq!(one(|e| e.cmp_imm(Reg::R4, 0xFF).unwrap()), [0xFF, 0x2C]);
        assert_eq!(one(|e| e.cmp_reg(Reg::R5, Reg::R6).unwrap()), [0xB5, 0x42]);
    }

    #[test]
    fn lsls_immediate() {
        assert_eq!(
            one(|e| e.lsls_imm(Reg::R0, Reg::R1, 3).unwrap()),
            [0xC8, 0x00]
        );
    }

    #[test]
    fn lsrs_immediate() {
        assert_eq!(
            one(|e| e.lsrs_imm(Reg::R0, Reg::R1, 3).unwrap()),
            [0xC8, 0x08]
        );
    }

    #[test]
    fn data_processing_register_group() {
        assert_eq!(one(|e| e.ands(Reg::R0, Reg::R1).unwrap()), [0x08, 0x40]);
        assert_eq!(one(|e| e.orrs(Reg::R2, Reg::R3).unwrap()), [0x1A, 0x43]);
        assert_eq!(one(|e| e.muls(Reg::R4, Reg::R5).unwrap()), [0x6C, 0x43]);
        assert_eq!(one(|e| e.mvns(Reg::R0, Reg::R1).unwrap()), [0xC8, 0x43]);
        assert_eq!(one(|e| e.lsls_reg(Reg::R2, Reg::R3).unwrap()), [0x9A, 0x40]);
        assert_eq!(one(|e| e.rsbs(Reg::R0, Reg::R1).unwrap()), [0x48, 0x42]);
    }

    #[test]
    fn load_store_register_offset() {
        assert_eq!(
            one(|e| e.ldr_imm(Reg::R0, Reg::R1, 4).unwrap()),
            [0x48, 0x68]
        );
        assert_eq!(
            one(|e| e.str_imm(Reg::R2, Reg::R3, 8).unwrap()),
            [0x9A, 0x60]
        );
    }

    #[test]
    fn load_store_sp_relative() {
        assert_eq!(one(|e| e.ldr_sp(Reg::R0, 16).unwrap()), [0x04, 0x98]);
        assert_eq!(one(|e| e.str_sp(Reg::R1, 20).unwrap()), [0x05, 0x91]);
    }

    #[test]
    fn unconditional_branch_resolves_backward() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.bind_label(target);
        enc.nop();
        enc.nop();
        enc.b(target);
        let out = enc.finish().unwrap();
        assert_eq!(&out.bytes[4..6], &[0xFC, 0xE7]);
    }

    #[test]
    fn conditional_branch_resolves_backward() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.bind_label(target);
        enc.nop();
        enc.nop();
        enc.b(target);
        enc.b_cond(Cond::Ne, target);
        let out = enc.finish().unwrap();
        assert_eq!(&out.bytes[4..6], &[0xFC, 0xE7]);
        assert_eq!(&out.bytes[6..8], &[0xFB, 0xD1]);
    }

    #[test]
    fn branch_out_of_range_is_a_controlled_error() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.b(target);
        for _ in 0..2500 {
            enc.nop();
        }
        enc.bind_label(target);
        assert_eq!(enc.finish(), Err(AssembleError::BranchOutOfRange { at: 0 }));
    }

    #[test]
    fn conditional_branch_relaxes_when_out_of_range() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.b_cond(Cond::Eq, target);
        for _ in 0..400 {
            enc.nop();
        }
        enc.bind_label(target);
        let out = enc.finish().expect("relaxed, not rejected");
        assert_eq!(out.bytes.len(), 6 + 400 * 2);
        assert_eq!(&out.bytes[0..2], &[0x01, 0xD1]);
        assert_eq!(&out.bytes[2..4], &[0x90, 0xE1]);
        assert_eq!(&out.bytes[4..6], &[0x00, 0xBF]);
    }

    #[test]
    fn subtract_register_and_stack_pointer_adjust() {
        assert_eq!(
            one(|e| e.subs(Reg::R0, Reg::R1, Reg::R2).unwrap()),
            [0x88, 0x1A]
        );
        assert_eq!(one(|e| e.add_sp_imm(Reg::R0, 16).unwrap()), [0x04, 0xA8]);
        assert_eq!(one(|e| e.add_sp(8).unwrap()), [0x02, 0xB0]);
        assert_eq!(one(|e| e.sub_sp(8).unwrap()), [0x82, 0xB0]);
    }

    #[test]
    fn breakpoint_and_data_word() {
        assert_eq!(one(|e| e.bkpt(0xAB)), [0xAB, 0xBE]);
        assert_eq!(one(|e| e.emit_word(0x2000_4000)), [0x00, 0x40, 0x00, 0x20]);
    }

    #[test]
    fn ldr_literal_resolves_to_pool() {
        let mut enc = Encoder::new();
        let pool = enc.new_label();
        enc.ldr_literal(Reg::R0, pool).unwrap();
        enc.nop();
        enc.bind_label(pool);
        enc.emit_word(0xDEAD_BEEF);
        let out = enc.finish().unwrap();
        assert_eq!(&out.bytes[0..2], &[0x00, 0x48]);
        assert_eq!(&out.bytes[4..8], &0xDEAD_BEEFu32.to_le_bytes());
    }

    #[test]
    fn adr_resolves_to_a_pc_relative_address() {
        let mut enc = Encoder::new();
        let label = enc.new_label();
        enc.adr(Reg::R1, label).unwrap();
        enc.nop();
        enc.bind_label(label);
        enc.emit_word(0xDEAD_BEEF);
        let out = enc.finish().unwrap();
        assert_eq!(&out.bytes[0..2], &[0x00, 0xA1]);
    }

    #[test]
    fn bl_call_resolves_backward() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.bind_label(target);
        enc.nop();
        enc.nop();
        enc.bl(target);
        let out = enc.finish().unwrap();
        assert_eq!(&out.bytes[4..8], &[0xFF, 0xF7, 0xFC, 0xFF]);
    }

    #[test]
    fn blx_register() {
        assert_eq!(one(|e| e.blx(Reg::R3)), [0x98, 0x47]);
    }

    #[test]
    fn sub_word_loads_and_stores() {
        assert_eq!(
            one(|e| e.ldr_reg(Reg::R0, Reg::R1, Reg::R2).unwrap()),
            [0x88, 0x58]
        );
        assert_eq!(
            one(|e| e.str_reg(Reg::R0, Reg::R1, Reg::R2).unwrap()),
            [0x88, 0x50]
        );
        assert_eq!(
            one(|e| e.ldrsb_reg(Reg::R0, Reg::R1, Reg::R2).unwrap()),
            [0x88, 0x56]
        );
        assert_eq!(
            one(|e| e.ldrb_reg(Reg::R3, Reg::R4, Reg::R5).unwrap()),
            [0x63, 0x5D]
        );
        assert_eq!(
            one(|e| e.ldrb_imm(Reg::R0, Reg::R1, 5).unwrap()),
            [0x48, 0x79]
        );
        assert_eq!(
            one(|e| e.ldrh_imm(Reg::R0, Reg::R1, 6).unwrap()),
            [0xC8, 0x88]
        );
    }

    #[test]
    fn push_pop_register_lists() {
        assert_eq!(one(|e| e.push_registers(0x30, false)), [0x30, 0xB4]);
        assert_eq!(one(|e| e.pop_registers(0x30, false)), [0x30, 0xBC]);
        assert_eq!(one(|e| e.push_registers(0x10, true)), [0x10, 0xB5]);
    }

    #[test]
    fn udf_trap() {
        assert_eq!(one(|e| e.udf(0)), [0x00, 0xDE]);
    }

    #[test]
    fn extend_reverse_and_high_registers() {
        assert_eq!(one(|e| e.sxtb(Reg::R0, Reg::R1).unwrap()), [0x48, 0xB2]);
        assert_eq!(one(|e| e.uxtb(Reg::R2, Reg::R3).unwrap()), [0xDA, 0xB2]);
        assert_eq!(one(|e| e.rev(Reg::R0, Reg::R1).unwrap()), [0x08, 0xBA]);
        assert_eq!(one(|e| e.revsh(Reg::R0, Reg::R1).unwrap()), [0xC8, 0xBA]);
        assert_eq!(one(|e| e.add_high(Reg::R8, Reg::R1)), [0x88, 0x44]);
        assert_eq!(one(|e| e.cmp_high(Reg::R10, Reg::R3)), [0x9A, 0x45]);
    }

    #[test]
    fn encoders_never_panic_over_all_registers_and_immediates() {
        for rn in 0..=15u8 {
            let a = Reg::new(rn).unwrap();
            for rm in 0..=15u8 {
                let b = Reg::new(rm).unwrap();
                let mut e = Encoder::new();
                let _ = e.adds(a, b, b);
                let _ = e.subs(a, b, b);
                let _ = e.cmp_reg(a, b);
                let _ = e.ands(a, b);
                let _ = e.ldr_reg(a, b, b);
                e.mov_reg(a, b);
                e.add_high(a, b);
                let _ = e.sxtb(a, b);
            }
            for imm in [0u8, 1, 7, 8, 31, 32, 64, 255] {
                let mut e = Encoder::new();
                let _ = e.movs_imm(a, imm);
                let _ = e.adds_imm8(a, imm);
                let _ = e.cmp_imm(a, imm);
                let _ = e.ldrb_imm(a, a, imm);
                let _ = e.strh_imm(a, a, imm);
            }
        }
    }

    #[test]
    fn finish_never_panics_on_bad_fixups() {
        let mut e = Encoder::new();
        let l = e.new_label();
        e.b(l);
        assert!(matches!(e.finish(), Err(AssembleError::UnboundLabel(_))));

        let mut e = Encoder::new();
        let l = e.new_label();
        e.b(l);
        for _ in 0..2500 {
            e.nop();
        }
        e.bind_label(l);
        assert!(matches!(
            e.finish(),
            Err(AssembleError::BranchOutOfRange { .. })
        ));
    }

    #[test]
    fn thumb32_orders_halfwords_then_bytes() {
        assert_eq!(
            one(|e| e.emit_thumb32(0xABCD, 0x1234)),
            [0xCD, 0xAB, 0x34, 0x12]
        );
    }

    #[test]
    fn label_reference_is_patched_at_finish() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.data_word(target);
        enc.nop();
        enc.bind_label(target);
        let out = enc.finish().unwrap();
        assert_eq!(&out.bytes[0..4], &6u32.to_le_bytes());
    }

    #[test]
    fn unbound_label_is_a_controlled_error() {
        let mut enc = Encoder::new();
        let dangling = enc.new_label();
        enc.data_word(dangling);
        assert_eq!(enc.finish(), Err(AssembleError::UnboundLabel(dangling)));
    }

    #[test]
    fn external_symbol_becomes_a_relocation() {
        let mut enc = Encoder::new();
        enc.data_word_symbol(42);
        let out = enc.finish().unwrap();
        assert_eq!(out.bytes, [0, 0, 0, 0]);
        assert_eq!(
            out.relocs,
            [Reloc {
                at: 0,
                kind: RelocKind::Abs32,
                symbol: 42
            }]
        );
    }

    #[test]
    fn bl_symbol_becomes_a_thumb_call_relocation() {
        let mut enc = Encoder::new();
        enc.bx(Reg::LR);
        enc.bl_symbol(7);
        let out = enc.finish().unwrap();
        assert_eq!(out.bytes, [0x70, 0x47, 0x00, 0xF0, 0x00, 0xD0]);
        assert_eq!(
            out.relocs,
            [Reloc {
                at: 2,
                kind: RelocKind::ThumbCall,
                symbol: 7
            }]
        );
    }
}
