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
    /// A 32-bit data word holding `S + A - P` -- a signed relative reference to a symbol, WITHOUT the
    /// Thumb-bit forcing an absolute code pointer carries. A vtable slot uses it: the value is a
    /// method's entry relative to its type descriptor, resolved by the linker so it survives
    /// `--gc-sections` re-layout (a baked [`Encoder::data_word_diff`] would not). See
    /// [`Encoder::data_word_symbol_reldesc`].
    RelDesc32,
    /// A 32-bit `B.W` unconditional branch (encoding T4): the SAME S:I1:I2:imm10:imm11 swizzle as
    /// [`RelocKind::ThumbCall`], but hw2 bit 14 is clear (a branch, not a link) -- reach about
    /// +/-16 MB. A far [`RelocKind::ThumbBranch11`] grows into this on a Mainline (wide-Thumb-2)
    /// target, where ARMv6-M would instead need a literal-pool veneer.
    ThumbBranch24,
    /// A 32-bit `ADR.W` (`ADD`/`SUB Rd, PC, #imm12`, encoding T3/T2): the wide form of a PC-relative
    /// address, reach about +/-4 KB from `Align(PC, 4)`. A far `adr` (a string blob) grows into this
    /// on a Mainline target instead of hard-erroring at the ~1 KB [`RelocKind::ThumbLdrLit8`] reach.
    ThumbAdrWide,
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
    /// The relocation addend `A` in the link-step calculation. Zero for the call/absolute forms (whose
    /// addend a `SHT_REL`-style consumer takes from the instruction field); a [`RelocKind::RelDesc32`]
    /// carries its constant here (the slot's fixed distance from its type descriptor).
    pub addend: i32,
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
    /// Label ids of literal-pool words emitted via [`Encoder::pool_word`] /
    /// [`Encoder::pool_word_symbol`] -- each a self-contained 4-byte datum a PC-relative `ldr`
    /// loads. If such a word ends up beyond its load's ~1 KB reach, [`Encoder::finish`] relocates a
    /// copy into a nearer branch-over island (see [`Encoder::island_far_literal`]). Other
    /// `ThumbLdrLit8` targets -- an `adr` to a string or a multi-word descriptor -- are NOT listed
    /// here and are never split this way.
    pool_literals: Vec<u32>,
    /// Marked data BLOBs: `(label id, byte length)` for each self-contained datum registered via
    /// [`Encoder::mark_blob`] -- a string laid at its function's end, referenced by an `adr`. On a
    /// Mainline target, a marked blob beyond even the widened `ADR.W`'s +/-4 KB reach relocates a
    /// copy into a nearer branch-over island (see [`Encoder::island_far_blob`]); an UNMARKED far
    /// `adr` target (a descriptor, whose interior labels a byte copy would not carry) still
    /// hard-errors in [`Encoder::finish`].
    blobs: Vec<(u32, u32)>,
    /// Whether the target is a Mainline (wide-Thumb-2) profile -- ARMv7-M / ARMv8-M Mainline, e.g. the
    /// RP2350's Cortex-M33. When set, a far unconditional branch relaxes to `B.W` and a far `adr` to
    /// `ADR.W` (see [`Encoder::relax`]) rather than the ARMv6-M literal-pool veneer. The object build
    /// sets it from the target's [`crate::target::Profile`]; the default `false` keeps every existing
    /// ARMv6-M path byte-identical.
    wide: bool,
}

use crate::cond::Cond;
use crate::register::Reg;

impl Encoder {
    /// Creates an empty encoder.
    #[must_use]
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// Marks the target as a Mainline (wide-Thumb-2) profile, so [`Encoder::finish`] relaxes a far
    /// unconditional branch to `B.W` and a far `adr` to `ADR.W` instead of hard-erroring at the
    /// ARMv6-M reach. Call before emitting; leave it off (the default) for an ARMv6-M target.
    pub fn set_wide_thumb2(&mut self, enabled: bool) {
        self.wide = enabled;
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

    /// `ADD Rdm, SP, Rdm` -- add the stack pointer into a LOW register in place. 16-bit encoding
    /// T1 of ADD (SP plus register) (A6.7.4): `0100 0100 DM 1101 Rdm` with `Rm = SP`. The
    /// big-frame slot addressing builds a byte offset in `rdm` and rebases it onto SP with this.
    pub fn add_sp_reg(&mut self, rdm: Reg) -> Result<(), AssembleError> {
        if !rdm.is_low() {
            return Err(AssembleError::UnencodableOperand);
        }
        self.emit_u16(0x4468 | u16::from(rdm.number()));
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

    /// Reserve a frame too large for one `SUB SP,#imm` (T1 caps at 508) by chunking into
    /// consecutive 508-byte-max decrements. `imm` a multiple of 4. Thumb-1 SP-relative
    /// slot loads/stores reach 1020, so a spilled frame stays within that; this only
    /// spans the 508..=1020 gap the single instruction cannot.
    pub fn sub_sp_far(&mut self, imm: u16) -> Result<(), AssembleError> {
        if imm % 4 != 0 {
            return Err(AssembleError::UnencodableOperand);
        }
        let mut left = imm;
        while left > 508 {
            self.sub_sp(508)?;
            left -= 508;
        }
        if left > 0 {
            self.sub_sp(left)?;
        }
        Ok(())
    }

    /// Release a frame reserved by [`Encoder::sub_sp_far`] -- the `ADD SP,#imm` counterpart,
    /// chunked the same way.
    pub fn add_sp_far(&mut self, imm: u16) -> Result<(), AssembleError> {
        if imm % 4 != 0 {
            return Err(AssembleError::UnencodableOperand);
        }
        let mut left = imm;
        while left > 508 {
            self.add_sp(508)?;
            left -= 508;
        }
        if left > 0 {
            self.add_sp(left)?;
        }
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
        while self.position() % 4 != 0 {
            if self.position() % 2 == 0 {
                self.nop();
            } else {
                self.bytes.push(0);
            }
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
            addend: 0,
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
            addend: 0,
        });
        self.bytes.extend_from_slice(&[0; 4]);
    }

    /// Emits a 32-bit data word holding `symbol + addend - here` -- a linker-resolved relative reference
    /// ([`RelocKind::RelDesc32`]). Unlike [`Encoder::data_word_diff`] (baked at [`Encoder::finish`], so
    /// wrong once `--gc-sections` re-lays-out the object) the offset is a real relocation the linker
    /// applies to the FINAL layout. A vtable slot uses it to point at a method relative to its type
    /// descriptor, with `addend` = the slot's fixed distance from that descriptor, so the stored value
    /// resolves to `method_entry - type_desc` wherever the two land.
    pub fn data_word_symbol_reldesc(&mut self, symbol: u32, addend: i32) {
        let at = self.position();
        self.relocs.push(Reloc {
            at,
            kind: RelocKind::RelDesc32,
            symbol,
            addend,
        });
        self.bytes.extend_from_slice(&[0; 4]);
    }

    /// Emits a 4-byte literal-pool word bound to `label` and holding `value`, marked ISLANDABLE: if
    /// `label` ends up beyond the ~1 KB reach of its PC-relative `ldr`, [`Encoder::finish`] moves a
    /// copy of the word into a nearer branch-over island and re-points the load. This is the bind +
    /// [`Encoder::emit_word`] a literal pool already does, plus the islandability record -- so a
    /// large function's early loads still reach their constants instead of hard-erroring.
    pub fn pool_word(&mut self, label: Label, value: u32) {
        self.bind_label(label);
        self.pool_literals.push(label.0);
        self.emit_word(value);
    }

    /// Like [`Encoder::pool_word`] but the word holds the address of an external `symbol` (an
    /// `Abs32` reloc the link step fills), for a function-pointer or type-descriptor pool entry. The
    /// word is islandable too; islanding replicates the relocation onto the relocated copy.
    pub fn pool_word_symbol(&mut self, label: Label, symbol: u32) {
        self.pool_word_symbol_addend(label, symbol, 0);
    }

    /// Like [`Encoder::pool_word_symbol`] but the word resolves to `symbol + addend` -- a static
    /// field's slot within its assembly's region symbol, or any other offset-from-symbol datum.
    /// Islanding copies the whole relocation, addend included.
    pub fn pool_word_symbol_addend(&mut self, label: Label, symbol: u32, addend: i32) {
        self.bind_label(label);
        self.pool_literals.push(label.0);
        let at = self.position();
        self.relocs.push(Reloc {
            at,
            kind: RelocKind::Abs32,
            symbol,
            addend,
        });
        self.bytes.extend_from_slice(&[0; 4]);
    }

    /// Marks `label` as the start of a self-contained data BLOB of `len` bytes -- a string laid at
    /// its function's end -- making it islandable BY COPY: on a Mainline target, if a (widened)
    /// `adr` to it still lands beyond `ADR.W`'s +/-4 KB reach, [`Encoder::finish`] moves a copy of
    /// the whole blob into a branch-over island beside the `adr` and re-points the label (see
    /// [`Encoder::island_far_blob`]). Call after the blob's bytes are emitted, once `len` is known.
    /// An unmarked `adr` target -- a type descriptor, whose interior labels and diffs a byte copy
    /// would not carry -- is never copied and still hard-errors out of reach.
    pub fn mark_blob(&mut self, label: Label, len: u32) {
        self.blobs.push((label.0, len));
    }

    /// Inserts `insert` bytes at byte offset `at`, shifting every later reference -- any bound
    /// label, fixup, relocation, or diff at offset >= `at` -- forward by `insert.len()`. The shared
    /// building block for growing the image mid-stream: branch relaxation splices a halfword-pair,
    /// literal islanding splices a branch-over datum, and both must move everything after the seam.
    fn splice_in(&mut self, at: u32, insert: &[u8]) {
        let grow = insert.len() as u32;
        let pos = at as usize;
        self.bytes.splice(pos..pos, insert.iter().copied());
        for slot in self.labels.iter_mut().flatten() {
            if *slot >= at {
                *slot += grow;
            }
        }
        for (fixup_at, _, _) in &mut self.fixups {
            if *fixup_at >= at {
                *fixup_at += grow;
            }
        }
        for reloc in &mut self.relocs {
            if reloc.at >= at {
                reloc.at += grow;
            }
        }
        for (diff_at, _, _) in &mut self.diffs {
            if *diff_at >= at {
                *diff_at += grow;
            }
        }
    }

    /// Runs branch relaxation and literal-pool islanding to a JOINT fixpoint before the fixups are
    /// resolved. Each grows the image -- a widened conditional branch splices a halfword-pair, an
    /// islanded literal splices a branch-over datum -- which shifts later references and can push
    /// another branch or load out of reach, so the steps co-iterate until nothing must move. Each
    /// conditional branch widens at most once (it then has the wider reach), each pool word islands
    /// at most once (its copy then sits a few bytes from the load), and each far blob islands at
    /// most once per referencing `adr` (its copy then sits beside the `ADR.W`), so this terminates.
    fn relax(&mut self) -> Result<(), AssembleError> {
        loop {
            if self.widen_far_conditional_branch()? {
                continue;
            }
            if self.wide && self.widen_far_unconditional_branch()? {
                continue;
            }
            if self.wide && self.widen_far_adr()? {
                continue;
            }
            if self.wide && self.island_far_blob()? {
                continue;
            }
            if self.island_far_literal()? {
                continue;
            }
            return Ok(());
        }
    }

    /// Grows the first far unconditional branch (`ThumbBranch11`, its +/-2 KB reach exceeded) into a
    /// 32-bit `B.W` (+/-16 MB) -- a Mainline-only relaxation. Splices the second halfword and re-kinds
    /// the fixup; the caller re-checks from the top because the spliced halfword shifts later refs.
    /// (An ARMv6-M target, which has no wide branch, would instead need a literal-pool veneer.)
    fn widen_far_unconditional_branch(&mut self) -> Result<bool, AssembleError> {
        for idx in 0..self.fixups.len() {
            let (at, kind, label_id) = self.fixups[idx];
            if kind != RelocKind::ThumbBranch11 {
                continue;
            }
            let target = match self.labels.get(label_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(label_id))),
            };
            let offset = i64::from(target) - (i64::from(at) + 4);
            if (-2048..=2046).contains(&offset) && offset % 2 == 0 {
                continue;
            }
            self.splice_in(at + 2, &[0x00, 0x00, 0x00, 0xBF]);
            self.fixups[idx].1 = RelocKind::ThumbBranch24;
            return Ok(true);
        }
        Ok(false)
    }

    /// Grows the first far `adr` (a `ThumbLdrLit8` fixup whose instruction is an `ADR`, its ~1 KB
    /// reach exceeded) into a 32-bit `ADR.W` (+/-4 KB) -- a Mainline-only relaxation, the alternative
    /// to blob-islanding a far string on a v6-M target. Only the `ADR` opcode (`0xA0xx`) grows here; a
    /// pool WORD (an `LDR` literal, `0x48xx`) still relocates through [`Encoder::island_far_literal`].
    fn widen_far_adr(&mut self) -> Result<bool, AssembleError> {
        for idx in 0..self.fixups.len() {
            let (at, kind, label_id) = self.fixups[idx];
            if kind != RelocKind::ThumbLdrLit8 {
                continue;
            }
            let is_adr = self
                .bytes
                .get(at as usize..at as usize + 2)
                .is_some_and(|b| u16::from_le_bytes([b[0], b[1]]) & 0xF800 == 0xA000);
            if !is_adr {
                continue;
            }
            let target = match self.labels.get(label_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(label_id))),
            };
            let pc = (at + 4) & !3u32;
            if target >= pc && target - pc <= 1020 && (target - pc) % 4 == 0 {
                continue;
            }
            self.splice_in(at + 2, &[0x00, 0x00, 0x00, 0xBF]);
            self.fixups[idx].1 = RelocKind::ThumbAdrWide;
            return Ok(true);
        }
        Ok(false)
    }

    /// Grows the first conditional branch whose +/-256-byte reach is exceeded into the two-halfword
    /// inverted-skip form (ARMv6-M has no wide `B<c>`): `B<!cond>` over a following `B`, which
    /// reaches +/-2 KB. Returns whether one grew; the caller re-checks from the top because the
    /// spliced halfword shifts every later reference and can push another branch out of range.
    fn widen_far_conditional_branch(&mut self) -> Result<bool, AssembleError> {
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
            self.splice_in(at + 2, &[0x00, 0xE0, 0x00, 0xBF]);
            self.fixups[idx].1 = RelocKind::ThumbBranchCond8Long;
            return Ok(true);
        }
        Ok(false)
    }

    /// Relocates the first islandable literal-pool word (see [`Encoder::pool_word`]) that lies
    /// beyond its PC-relative load's ~1 KB reach into a branch-over island placed right after the
    /// load -- a point execution reaches, from which the copy is a few bytes ahead and so back in
    /// range -- and re-points the load's label at the copy. Returns whether one moved.
    ///
    /// The island is `B over` (which skips the datum), word-align padding, then the 4-byte copy.
    /// The original word is left where it was as harmless dead data: nothing loads it any more (its
    /// label now names the copy), and a relocation that rode on it is replicated onto the live copy
    /// while the orphaned one merely patches unread bytes. Only marked words move -- an `adr` to a
    /// string or multi-word descriptor is never split this way and still hard-errors in
    /// [`Encoder::finish`] if it is out of reach.
    fn island_far_literal(&mut self) -> Result<bool, AssembleError> {
        for idx in 0..self.fixups.len() {
            let (at, kind, label_id) = self.fixups[idx];
            if kind != RelocKind::ThumbLdrLit8 || !self.pool_literals.contains(&label_id) {
                continue;
            }
            let target = match self.labels.get(label_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(label_id))),
            };
            let pc = (at + 4) & !3u32;
            if target >= pc && target - pc <= 1020 {
                continue;
            }
            let word = match self.bytes.get(target as usize..target as usize + 4) {
                Some(b) => [b[0], b[1], b[2], b[3]],
                None => return Err(AssembleError::BranchOutOfRange { at }),
            };
            let carried = self.relocs.iter().find(|r| r.at == target).copied();
            let ins = at + 2;
            let pad = (4 - ((ins + 2) & 3)) & 3;
            let mut island = Vec::with_capacity(8);
            island.extend_from_slice(&0xE000u16.to_le_bytes());
            island.resize(island.len() + pad as usize, 0);
            island.extend_from_slice(&word);
            let word_site = ins + 2 + pad;
            let trailing = (4 - (island.len() as u32 & 3)) & 3;
            island.resize(island.len() + trailing as usize, 0);
            let over = ins + island.len() as u32;
            self.splice_in(ins, &island);
            self.labels[label_id as usize] = Some(word_site);
            let over_label = self.new_label();
            self.labels[over_label.0 as usize] = Some(over);
            self.fixups.push((ins, RelocKind::ThumbBranch11, over_label.0));
            if let Some(mut r) = carried {
                r.at = word_site;
                self.relocs.push(r);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Relocates the first marked blob (see [`Encoder::mark_blob`]) that lies beyond its widened
    /// `adr`'s +/-4 KB `ADR.W` reach into a branch-over island placed right after that `ADR.W` --
    /// the multi-byte twin of [`Encoder::island_far_literal`], and Mainline-only: it fires on a
    /// [`RelocKind::ThumbAdrWide`] fixup, which only [`Encoder::widen_far_adr`] mints (widening is
    /// tried first, so the 4-byte `ADR.W` rescues the 1 KB..4 KB band and a copy is spliced only
    /// past that). Returns whether one moved.
    ///
    /// The island is `B over` (which skips the datum), word-align padding, the blob copy, then
    /// trailing padding to a MULTIPLE OF 4 -- the splice preserves every later `ldr [pc, #imm*4]`
    /// pool word's 4-byte alignment, exactly as [`Encoder::island_far_literal`] does. The original
    /// blob stays where it was as harmless dead data (its label now names the copy); a relocation
    /// inside its range is replicated onto the live copy at the same interior offset, while the
    /// orphaned one merely patches unread bytes. A blob longer than the skip branch's +/-2 KB reach
    /// pushes `B over` out of range; the joint fixpoint then widens that skip to `B.W` on a later
    /// pass (`wide` is always set when this fires).
    fn island_far_blob(&mut self) -> Result<bool, AssembleError> {
        for idx in 0..self.fixups.len() {
            let (at, kind, label_id) = self.fixups[idx];
            if kind != RelocKind::ThumbAdrWide {
                continue;
            }
            let blob_len = match self.blobs.iter().find(|(id, _)| *id == label_id) {
                Some(&(_, len)) => len,
                None => continue,
            };
            let target = match self.labels.get(label_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(label_id))),
            };
            let pc = i64::from((at + 4) & !3u32);
            let delta = i64::from(target) - pc;
            if (-4095..=4095).contains(&delta) {
                continue;
            }
            let blob = match self
                .bytes
                .get(target as usize..(target + blob_len) as usize)
            {
                Some(b) => b.to_vec(),
                None => return Err(AssembleError::BranchOutOfRange { at }),
            };
            let carried: Vec<Reloc> = self
                .relocs
                .iter()
                .filter(|r| r.at >= target && r.at < target + blob_len)
                .copied()
                .collect();
            let ins = at + 4;
            let pad = (4 - ((ins + 2) & 3)) & 3;
            let mut island = Vec::with_capacity(8 + blob.len());
            island.extend_from_slice(&0xE000u16.to_le_bytes());
            island.resize(island.len() + pad as usize, 0);
            let copy_site = ins + 2 + pad;
            island.extend_from_slice(&blob);
            let trailing = (4 - (island.len() as u32 & 3)) & 3;
            island.resize(island.len() + trailing as usize, 0);
            let over = ins + island.len() as u32;
            self.splice_in(ins, &island);
            self.labels[label_id as usize] = Some(copy_site);
            let over_label = self.new_label();
            self.labels[over_label.0 as usize] = Some(over);
            self.fixups.push((ins, RelocKind::ThumbBranch11, over_label.0));
            for mut r in carried {
                r.at = copy_site + (r.at - target);
                self.relocs.push(r);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolves every internal label reference and returns the finished image
    /// plus the external relocations the link step must still apply.
    ///
    /// The resolved value of a label is its byte offset within this image, which
    /// stands in for a load address until the AOT driver assigns sections.
    /// Returns [`AssembleError::UnboundLabel`] if any referenced label was never
    /// bound.
    pub fn finish(mut self) -> Result<Assembled, AssembleError> {
        self.relax()?;
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
                RelocKind::RelDesc32 => {}
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
                RelocKind::ThumbBranch24 => {
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
                    let hw2 = 0x9000 | (j1 << 13) | (j2 << 11) | imm11;
                    if let Some(slot) = self.bytes.get_mut(site..site + 4) {
                        slot[0..2].copy_from_slice(&hw1.to_le_bytes());
                        slot[2..4].copy_from_slice(&hw2.to_le_bytes());
                    }
                }
                RelocKind::ThumbAdrWide => {
                    let rd = self
                        .bytes
                        .get(site..site + 2)
                        .map_or(0, |b| (u16::from_le_bytes([b[0], b[1]]) >> 8) & 7);
                    let pc = i64::from((*at + 4) & !3u32);
                    let delta = i64::from(target) - pc;
                    let (add, mag) = if delta >= 0 { (true, delta) } else { (false, -delta) };
                    if !(0..=4095).contains(&mag) {
                        return Err(AssembleError::BranchOutOfRange { at: *at });
                    }
                    let mag = mag as u16;
                    let i = (mag >> 11) & 1;
                    let imm3 = (mag >> 8) & 7;
                    let imm8 = mag & 0xFF;
                    let hw1 = (if add { 0xF20F } else { 0xF2AF }) | (i << 10);
                    let hw2 = (imm3 << 12) | (rd << 8) | imm8;
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

    /// Lays the image out exactly as [`Encoder::finish`] does -- running branch relaxation and literal
    /// islanding to a joint fixpoint -- then returns each requested label's POST-RELAXATION byte offset
    /// (`None` for a never-bound label), WITHOUT resolving fixups. `finish` consumes the encoder and, on
    /// [`AssembleError::BranchOutOfRange`], reports only the failing site's post-relaxation byte offset; a
    /// caller that must map that site back to a bound region -- e.g. the function whose body owns an
    /// unencodable instruction, to stub it and rebuild the object -- clones the encoder first and reads
    /// true offsets here against the SAME layout `finish` computes (relaxation is deterministic). A
    /// relaxation error (rare: an unreadable marked pool word) surfaces here just as it would in `finish`.
    pub fn relaxed_positions(mut self, labels: &[Label]) -> Result<Vec<Option<u32>>, AssembleError> {
        self.relax()?;
        Ok(labels
            .iter()
            .map(|l| self.labels.get(l.0 as usize).copied().flatten())
            .collect())
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
    fn align_to_word_recovers_from_an_odd_offset() {
        let mut enc = Encoder::new();
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        enc.emit_bytes(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        enc.align_to_word();
        assert_eq!(
            enc.position() % 4,
            0,
            "must reach a word boundary from an odd offset"
        );
        enc.bind_label(blob);
        enc.emit_word(0xdead_beef);
        enc.finish()
            .expect("the ADR resolves to a word-aligned target");
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

    /// The offset a resolved `LDR` (literal) at `site` reads from -- `Align(site + 4, 4) + imm8 * 4`.
    fn ldr_literal_target(bytes: &[u8], site: usize) -> usize {
        let instr = u16::from_le_bytes([bytes[site], bytes[site + 1]]);
        assert_eq!(instr & 0xF800, 0x4800, "the site is an LDR (literal)");
        let imm8 = (instr & 0x00FF) as usize;
        assert!(imm8 * 4 <= 1020, "the resolved load is within its ~1 KB reach");
        ((site + 4) & !3) + imm8 * 4
    }

    #[test]
    fn a_far_literal_pool_word_islands_within_reach() {
        let mut enc = Encoder::new();
        let pool = enc.new_label();
        enc.ldr_literal(Reg::R0, pool).unwrap();
        for _ in 0..600 {
            enc.nop();
        }
        enc.align_to_word();
        enc.pool_word(pool, 0xDEAD_BEEF);
        let out = enc.finish().expect("the far pool word islands rather than erroring");
        let addr = ldr_literal_target(&out.bytes, 0);
        assert_eq!(
            &out.bytes[addr..addr + 4],
            &0xDEAD_BEEFu32.to_le_bytes(),
            "the load resolves to an in-reach copy holding the constant"
        );
    }

    #[test]
    fn a_far_symbol_pool_word_islands_and_replicates_its_relocation() {
        let mut enc = Encoder::new();
        let pool = enc.new_label();
        enc.ldr_literal(Reg::R0, pool).unwrap();
        for _ in 0..600 {
            enc.nop();
        }
        enc.align_to_word();
        enc.pool_word_symbol(pool, 7);
        let out = enc.finish().expect("the far symbol pool word islands");
        let addr = ldr_literal_target(&out.bytes, 0) as u32;
        assert!(
            out.relocs
                .iter()
                .any(|r| r.at == addr && r.symbol == 7 && r.kind == RelocKind::Abs32),
            "the relocation is replicated onto the islanded copy the load reads"
        );
    }

    #[test]
    fn islanding_and_branch_relaxation_reach_a_joint_fixpoint() {
        let mut enc = Encoder::new();
        let pool = enc.new_label();
        let target = enc.new_label();
        enc.ldr_literal(Reg::R0, pool).unwrap();
        enc.b_cond(Cond::Eq, target);
        for _ in 0..600 {
            enc.nop();
        }
        enc.bind_label(target);
        enc.align_to_word();
        enc.pool_word(pool, 0x1234_5678);
        let out = enc
            .finish()
            .expect("the branch relaxes and the literal islands together");
        let addr = ldr_literal_target(&out.bytes, 0);
        assert_eq!(
            &out.bytes[addr..addr + 4],
            &0x1234_5678u32.to_le_bytes(),
            "the load still resolves to its constant after both grew the image"
        );
    }

    #[test]
    fn an_island_preserves_word_alignment_of_later_literal_loads() {
        let mut enc = Encoder::new();
        let pool_a = enc.new_label();
        enc.ldr_literal(Reg::R0, pool_a).unwrap();
        for _ in 0..600 {
            enc.nop();
        }
        enc.align_to_word();
        enc.pool_word(pool_a, 0xAAAA_AAAA);
        enc.align_to_word();
        let pool_b = enc.new_label();
        enc.ldr_literal(Reg::R1, pool_b).unwrap();
        enc.align_to_word();
        enc.pool_word(pool_b, 0xBBBB_BBBB);
        let out = enc
            .finish()
            .expect("region B's word-aligned literal load survives region A's island");
        let pos = out.label_position(pool_b).expect("pool_b bound") as usize;
        assert_eq!(
            &out.bytes[pos..pos + 4],
            &0xBBBB_BBBBu32.to_le_bytes(),
            "region B's load still resolves to its constant after the island shifted it"
        );
        assert_eq!(pos % 4, 0, "the islanded pool word stays word-aligned");
    }

    #[test]
    fn a_far_unconditional_branch_widens_to_bw_on_a_wide_target() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let target = enc.new_label();
        enc.b(target);
        for _ in 0..1500 {
            enc.nop();
        }
        enc.bind_label(target);
        enc.nop();
        let out = enc
            .finish()
            .expect("a far unconditional branch widens to B.W on a wide target");
        let hw1 = u16::from_le_bytes([out.bytes[0], out.bytes[1]]);
        let hw2 = u16::from_le_bytes([out.bytes[2], out.bytes[3]]);
        assert_eq!(hw1 & 0xF800, 0xF000, "B.W first halfword");
        assert_eq!(hw2 & 0xD000, 0x9000, "B.W second halfword (a branch, not the BL 0xD000)");
        assert_eq!((hw1 >> 10) & 1, 0, "a forward branch has S clear");
        let i1 = 1 - u32::from((hw2 >> 13) & 1);
        let i2 = 1 - u32::from((hw2 >> 11) & 1);
        let off = (i1 << 23) | (i2 << 22) | (u32::from(hw1 & 0x3FF) << 12) | (u32::from(hw2 & 0x7FF) << 1);
        assert_eq!(4 + off, out.label_position(target).unwrap(), "B.W lands on the target");
    }

    #[test]
    fn a_far_adr_widens_to_adrw_on_a_wide_target() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        for _ in 0..800 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        enc.emit_word(0x1234_5678);
        let out = enc
            .finish()
            .expect("a far adr widens to ADR.W on a wide target");
        let hw1 = u16::from_le_bytes([out.bytes[0], out.bytes[1]]);
        let hw2 = u16::from_le_bytes([out.bytes[2], out.bytes[3]]);
        assert_eq!(hw1 & 0xFBFF, 0xF20F, "ADR.W ADD form (Rd, PC, #imm)");
        assert_eq!((hw2 >> 8) & 0xF, 0, "ADR.W targets R0");
        let imm = (u32::from((hw1 >> 10) & 1) << 11) | (u32::from((hw2 >> 12) & 7) << 8) | u32::from(hw2 & 0xFF);
        assert_eq!(4 + imm, out.label_position(blob).unwrap(), "ADR.W points at the blob");
    }

    #[test]
    fn a_far_unconditional_branch_hard_errors_without_the_wide_capability() {
        let mut enc = Encoder::new();
        let target = enc.new_label();
        enc.b(target);
        for _ in 0..1500 {
            enc.nop();
        }
        enc.bind_label(target);
        enc.nop();
        assert!(matches!(
            enc.finish(),
            Err(AssembleError::BranchOutOfRange { .. })
        ));
    }

    #[test]
    fn a_near_branch_encodes_identically_with_or_without_the_wide_capability() {
        let build = |wide: bool| {
            let mut enc = Encoder::new();
            enc.set_wide_thumb2(wide);
            let t = enc.new_label();
            enc.b(t);
            enc.nop();
            enc.bind_label(t);
            enc.nop();
            enc.finish().unwrap().bytes
        };
        assert_eq!(
            build(false),
            build(true),
            "a near branch encodes identically with or without the wide capability"
        );
    }

    #[test]
    fn a_widening_between_a_load_and_its_pool_word_keeps_the_word_aligned() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let pool = enc.new_label();
        let far = enc.new_label();
        enc.ldr_literal(Reg::R0, pool).unwrap();
        enc.b(far);
        enc.align_to_word();
        enc.pool_word(pool, 0xCAFE_F00D);
        for _ in 0..1500 {
            enc.nop();
        }
        enc.bind_label(far);
        enc.nop();
        let out = enc
            .finish()
            .expect("a widening between a load and its in-reach word keeps the word 4-aligned");
        let pos = out.label_position(pool).unwrap() as usize;
        assert_eq!(
            &out.bytes[pos..pos + 4],
            &0xCAFE_F00Du32.to_le_bytes(),
            "the load's pool word is intact past the widened branch"
        );
        assert_eq!(pos % 4, 0, "the pool word stayed 4-aligned across the widening");
    }

    #[test]
    fn a_far_blob_islands_beside_its_wide_adr() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        for _ in 0..2200 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        let start = enc.position();
        enc.emit_word(2);
        enc.emit_u16(0x0041);
        enc.emit_u16(0x0042);
        enc.mark_blob(blob, enc.position() - start);
        let out = enc.finish().expect("a far blob islands beside its wide adr");
        let hw1 = u16::from_le_bytes([out.bytes[0], out.bytes[1]]);
        let hw2 = u16::from_le_bytes([out.bytes[2], out.bytes[3]]);
        assert_eq!(hw1 & 0xFBFF, 0xF20F, "the adr widened to ADR.W ADD form first");
        let imm = (u32::from((hw1 >> 10) & 1) << 11)
            | (u32::from((hw2 >> 12) & 7) << 8)
            | u32::from(hw2 & 0xFF);
        let copy = out.label_position(blob).expect("the blob label was re-bound");
        assert_eq!(4 + imm, copy, "the ADR.W points at the ISLANDED copy");
        assert_eq!(copy, 8, "the copy sits right beside the adr, not at the original site");
        assert_eq!(
            &out.bytes[copy as usize..copy as usize + 8],
            &[2, 0, 0, 0, 0x41, 0, 0x42, 0],
            "the copy carries the whole blob -- count word and units"
        );
        let skip = u16::from_le_bytes([out.bytes[4], out.bytes[5]]);
        assert_eq!(skip, 0xE004, "a 16-bit B skips the 12-byte island to the shifted code");
    }

    #[test]
    fn an_adr_w_reach_blob_widens_without_islanding() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        for _ in 0..800 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        let start = enc.position();
        enc.emit_word(1);
        enc.emit_u16(0x0041);
        enc.mark_blob(blob, enc.position() - start);
        let out = enc.finish().expect("an ADR.W-reach blob needs no island");
        let pos = out.label_position(blob).expect("blob bound");
        assert!(pos > 1600, "the blob stayed at its original function-end site");
    }

    #[test]
    fn a_far_marked_blob_hard_errors_without_the_wide_capability() {
        let mut enc = Encoder::new();
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        for _ in 0..800 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        let start = enc.position();
        enc.emit_word(1);
        enc.emit_u16(0x0041);
        enc.mark_blob(blob, enc.position() - start);
        assert!(matches!(
            enc.finish(),
            Err(AssembleError::BranchOutOfRange { .. })
        ));
    }

    #[test]
    fn a_blob_island_between_a_load_and_its_pool_word_keeps_the_word_aligned() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let blob = enc.new_label();
        let pool = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        enc.ldr_literal(Reg::R1, pool).unwrap();
        enc.align_to_word();
        enc.pool_word(pool, 0xCAFE_F00D);
        for _ in 0..2200 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        let start = enc.position();
        enc.emit_bytes(b"hello, world\0");
        enc.mark_blob(blob, enc.position() - start);
        let out = enc
            .finish()
            .expect("a blob island between a load and its word keeps the word 4-aligned");
        let pos = out.label_position(pool).unwrap() as usize;
        assert_eq!(
            &out.bytes[pos..pos + 4],
            &0xCAFE_F00Du32.to_le_bytes(),
            "the pool word is intact past the island"
        );
        assert_eq!(pos % 4, 0, "the pool word stayed 4-aligned across the island splice");
        let copy = out.label_position(blob).unwrap() as usize;
        assert_eq!(&out.bytes[copy..copy + 13], b"hello, world\0", "the copy carries the text");
    }

    #[test]
    fn a_relocation_inside_an_islanded_blob_replicates_onto_the_copy() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        for _ in 0..2200 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        let start = enc.position();
        enc.emit_word(1);
        enc.data_word_symbol(7);
        enc.mark_blob(blob, enc.position() - start);
        let out = enc.finish().expect("a reloc-carrying far blob islands");
        let copy = out.label_position(blob).unwrap();
        assert!(
            out.relocs
                .iter()
                .any(|r| r.at == copy + 4 && r.symbol == 7 && r.kind == RelocKind::Abs32),
            "the interior relocation is replicated at the copy's matching offset"
        );
    }

    #[test]
    fn a_blob_longer_than_the_skip_branch_reach_widens_the_skip_to_bw() {
        let mut enc = Encoder::new();
        enc.set_wide_thumb2(true);
        let blob = enc.new_label();
        enc.adr(Reg::R0, blob).unwrap();
        for _ in 0..2200 {
            enc.nop();
        }
        enc.align_to_word();
        enc.bind_label(blob);
        let start = enc.position();
        enc.emit_word(1200);
        for i in 0..1200u16 {
            enc.emit_u16(i);
        }
        enc.mark_blob(blob, enc.position() - start);
        let out = enc
            .finish()
            .expect("the island's skip branch widens to B.W around a huge blob");
        let copy = out.label_position(blob).unwrap() as usize;
        assert_eq!(copy % 4, 0, "the copy stays word-aligned across the skip's widening");
        assert_eq!(&out.bytes[copy..copy + 4], &1200u32.to_le_bytes(), "count word intact");
        assert_eq!(&out.bytes[copy + 4..copy + 8], &[0, 0, 1, 0], "units 0 and 1 intact");
    }

    #[test]
    fn relaxed_positions_returns_bound_offsets_when_nothing_relaxes() {
        let mut enc = Encoder::new();
        let a = enc.new_label();
        enc.bind_label(a);
        enc.nop();
        let b = enc.new_label();
        enc.bind_label(b);
        enc.nop();
        let pos = enc.relaxed_positions(&[a, b]).unwrap();
        assert_eq!(pos.len(), 2);
        assert_eq!(pos[0], Some(0));
        assert_eq!(pos[1], Some(2), "an unrelaxed image reports its bound offsets verbatim");
    }

    #[test]
    fn relaxed_positions_reports_the_post_islanding_layout_finish_uses() {
        let mut enc = Encoder::new();
        let pool = enc.new_label();
        let marker = enc.new_label();
        enc.ldr_literal(Reg::R0, pool).unwrap();
        for _ in 0..600 {
            enc.nop();
        }
        enc.bind_label(marker);
        enc.align_to_word();
        enc.pool_word(pool, 0xDEAD_BEEF);
        let via_probe = enc.clone().relaxed_positions(&[marker, pool]).unwrap();
        let out = enc.finish().expect("the far pool word islands rather than erroring");
        assert_eq!(
            via_probe[0],
            out.label_position(marker),
            "relaxed_positions tracks the island's forward shift exactly as finish bakes it"
        );
        assert_eq!(
            via_probe[1],
            out.label_position(pool),
            "including the load's label being re-pointed at the in-reach island copy"
        );
        assert!(
            via_probe[0].unwrap() > 2 + 600 * 2,
            "the marker moved forward: an island was spliced before it (not a no-op agreement)"
        );
    }

    #[test]
    fn sub_sp_far_chunks_a_frame_past_the_single_instruction_reach() {
        let mut enc = Encoder::new();
        enc.sub_sp_far(1020).unwrap();
        assert_eq!(enc.as_bytes(), &[0xFF, 0xB0, 0xFF, 0xB0, 0x81, 0xB0]);
        let mut one = Encoder::new();
        one.sub_sp_far(500).unwrap();
        assert_eq!(one.as_bytes(), &[0xFD, 0xB0]);
        assert_eq!(
            Encoder::new().sub_sp_far(510),
            Err(AssembleError::UnencodableOperand)
        );
    }

    #[test]
    fn add_sp_reg_encodes_the_sp_plus_register_form() {
        let mut enc = Encoder::new();
        enc.add_sp_reg(Reg::R0).unwrap();
        enc.add_sp_reg(Reg::R7).unwrap();
        assert_eq!(enc.as_bytes(), &[0x68, 0x44, 0x6F, 0x44]);
        assert_eq!(
            Encoder::new().add_sp_reg(Reg::R8),
            Err(AssembleError::UnencodableOperand)
        );
    }

    #[test]
    fn add_sp_far_mirrors_sub_sp_far() {
        let mut enc = Encoder::new();
        enc.add_sp_far(1020).unwrap();
        assert_eq!(enc.as_bytes(), &[0x7F, 0xB0, 0x7F, 0xB0, 0x01, 0xB0]);
        let (mut sub, mut add) = (Encoder::new(), Encoder::new());
        sub.sub_sp_far(1016).unwrap();
        add.add_sp_far(1016).unwrap();
        assert_eq!(sub.as_bytes().len(), add.as_bytes().len());
        assert_eq!(
            Encoder::new().add_sp_far(2),
            Err(AssembleError::UnencodableOperand)
        );
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
                symbol: 42,
                addend: 0
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
                symbol: 7,
                addend: 0
            }]
        );
    }

    #[test]
    fn reldesc_word_carries_its_addend() {
        let mut enc = Encoder::new();
        enc.data_word_symbol_reldesc(3, -8);
        let out = enc.finish().unwrap();
        assert_eq!(out.bytes, [0, 0, 0, 0]);
        assert_eq!(
            out.relocs,
            [Reloc {
                at: 0,
                kind: RelocKind::RelDesc32,
                symbol: 3,
                addend: -8
            }]
        );
    }
}
