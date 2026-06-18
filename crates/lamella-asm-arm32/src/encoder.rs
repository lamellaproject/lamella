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
    /// A PC-relative literal load (`LDR` (literal), encoding T1): the low 8 bits
    /// take the word-scaled distance from `Align(PC, 4)` to the pool entry
    /// (Armv6-M ARM (DDI 0419E), A6.7.27), which must lie ahead within about 1 KB.
    ThumbLdrLit8,
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
}

/// Accumulates Thumb machine code and the references into it.
#[derive(Debug, Clone, Default)]
pub struct Encoder {
    bytes: Vec<u8>,
    /// `labels[i]` is the bound byte offset of label `i`, or `None` until bound.
    labels: Vec<Option<u32>>,
    /// Internal references to patch in `finish`: `(site, kind, label index)`.
    fixups: Vec<(u32, RelocKind, u32)>,
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

    /// Emits a 32-bit data word holding the address of `label`, to be patched in
    /// [`Encoder::finish`].
    pub fn data_word(&mut self, label: Label) {
        let at = self.position();
        self.fixups.push((at, RelocKind::Abs32, label.0));
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

    /// Resolves every internal label reference and returns the finished image
    /// plus the external relocations the link step must still apply.
    ///
    /// The resolved value of a label is its byte offset within this image, which
    /// stands in for a load address until the AOT driver assigns sections.
    /// Returns [`AssembleError::UnboundLabel`] if any referenced label was never
    /// bound.
    pub fn finish(mut self) -> Result<Assembled, AssembleError> {
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
            }
        }
        Ok(Assembled {
            bytes: self.bytes,
            relocs: self.relocs,
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
        enc.b_cond(Cond::Eq, target);
        for _ in 0..400 {
            enc.nop();
        }
        enc.bind_label(target);
        assert_eq!(enc.finish(), Err(AssembleError::BranchOutOfRange { at: 0 }));
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
}
