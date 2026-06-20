//! An RV32IM machine-code encoder for the Lamella backend's RISC-V target.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// One of the 32 RISC-V integer registers, by its number (`x0`-`x31`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reg(u8);

impl Reg {
    /// The hardwired-zero register `x0`.
    pub const ZERO: Reg = Reg(0);
    /// The return address `x1` (`ra`).
    pub const RA: Reg = Reg(1);
    /// The stack pointer `x2` (`sp`).
    pub const SP: Reg = Reg(2);
    /// Temporary `x5` (`t0`).
    pub const T0: Reg = Reg(5);
    /// Temporary `x6` (`t1`).
    pub const T1: Reg = Reg(6);
    /// Temporary `x7` (`t2`).
    pub const T2: Reg = Reg(7);
    /// Argument / return value `x10` (`a0`).
    pub const A0: Reg = Reg(10);
    /// Argument `x11` (`a1`).
    pub const A1: Reg = Reg(11);

    /// Creates a register from its number, or `None` if `number > 31`.
    #[must_use]
    pub const fn new(number: u8) -> Option<Reg> {
        if number <= 31 {
            Some(Reg(number))
        } else {
            None
        }
    }

    /// The 5-bit register number.
    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }
}

/// A location inside the image being built, resolved by the encoder in [`Encoder::finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label(u32);

/// A conditional-branch comparison, selecting the `funct3` of a B-type branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchCond {
    /// Branch if equal (`beq`).
    Eq,
    /// Branch if not equal (`bne`).
    Ne,
    /// Branch if signed less-than (`blt`).
    Lt,
    /// Branch if signed greater-or-equal (`bge`).
    Ge,
    /// Branch if unsigned less-than (`bltu`).
    LtU,
    /// Branch if unsigned greater-or-equal (`bgeu`).
    GeU,
}

impl BranchCond {
    const fn funct3(self) -> u32 {
        match self {
            BranchCond::Eq => 0,
            BranchCond::Ne => 1,
            BranchCond::Lt => 4,
            BranchCond::Ge => 5,
            BranchCond::LtU => 6,
            BranchCond::GeU => 7,
        }
    }
}

/// Why an encode could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssembleError {
    /// A [`Label`] was referenced but never bound to a position.
    UnboundLabel(Label),
    /// A branch or jump target is out of the encoding's reach.
    BranchOutOfRange,
}

/// The finished machine-code image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assembled {
    /// The little-endian RV32 byte image.
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum Fixup {
    Branch,
    Jump,
}

/// Accumulates RV32IM machine code and the label references into it.
#[derive(Debug, Clone, Default)]
pub struct Encoder {
    bytes: Vec<u8>,
    labels: Vec<Option<u32>>,
    fixups: Vec<(u32, Fixup, u32)>,
}

impl Encoder {
    /// Creates an empty encoder.
    #[must_use]
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// The current byte offset, where the next emitted instruction lands.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.bytes.len() as u32
    }

    /// Creates a fresh, unbound label.
    pub fn new_label(&mut self) -> Label {
        let id = self.labels.len() as u32;
        self.labels.push(None);
        Label(id)
    }

    /// Binds `label` to the current position.
    pub fn bind_label(&mut self, label: Label) {
        let here = self.position();
        if let Some(slot) = self.labels.get_mut(label.0 as usize) {
            *slot = Some(here);
        }
    }

    /// Appends one 32-bit instruction word, little-endian.
    pub fn emit_word(&mut self, word: u32) {
        self.bytes.extend_from_slice(&word.to_le_bytes());
    }

    fn r_type(&mut self, funct7: u32, rs2: Reg, rs1: Reg, funct3: u32, rd: Reg, opcode: u32) {
        self.emit_word(
            (funct7 << 25)
                | (u32::from(rs2.number()) << 20)
                | (u32::from(rs1.number()) << 15)
                | (funct3 << 12)
                | (u32::from(rd.number()) << 7)
                | opcode,
        );
    }

    fn i_type(&mut self, imm: i32, rs1: Reg, funct3: u32, rd: Reg, opcode: u32) {
        self.emit_word(
            ((imm as u32 & 0xfff) << 20)
                | (u32::from(rs1.number()) << 15)
                | (funct3 << 12)
                | (u32::from(rd.number()) << 7)
                | opcode,
        );
    }


    /// `add rd, rs1, rs2`.
    pub fn add(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 0, rd, 0x33);
    }
    /// `sub rd, rs1, rs2`.
    pub fn sub(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0x20, rs2, rs1, 0, rd, 0x33);
    }
    /// `and rd, rs1, rs2`.
    pub fn and(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 7, rd, 0x33);
    }
    /// `or rd, rs1, rs2`.
    pub fn or(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 6, rd, 0x33);
    }
    /// `xor rd, rs1, rs2`.
    pub fn xor(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 4, rd, 0x33);
    }
    /// `sll rd, rs1, rs2` (shift left logical by the low 5 bits of rs2).
    pub fn sll(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 1, rd, 0x33);
    }
    /// `srl rd, rs1, rs2` (shift right logical).
    pub fn srl(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 5, rd, 0x33);
    }
    /// `sra rd, rs1, rs2` (shift right arithmetic).
    pub fn sra(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0x20, rs2, rs1, 5, rd, 0x33);
    }
    /// `slt rd, rs1, rs2` (set if signed less-than, to 0/1).
    pub fn slt(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 2, rd, 0x33);
    }
    /// `sltu rd, rs1, rs2` (set if unsigned less-than).
    pub fn sltu(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(0, rs2, rs1, 3, rd, 0x33);
    }
    /// `mul rd, rs1, rs2` (the `M` extension's low-word multiply).
    pub fn mul(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(1, rs2, rs1, 0, rd, 0x33);
    }


    /// `addi rd, rs1, imm` (12-bit signed immediate).
    pub fn addi(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 0, rd, 0x13);
    }
    /// `andi rd, rs1, imm`.
    pub fn andi(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 7, rd, 0x13);
    }
    /// `xori rd, rs1, imm`.
    pub fn xori(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 4, rd, 0x13);
    }
    /// `sltiu rd, rs1, imm` (set if `rs1 < imm`, unsigned; `sltiu rd, rs, 1` is "rs == 0").
    pub fn sltiu(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 3, rd, 0x13);
    }
    /// `slli rd, rs1, shamt` (shift left by a 5-bit immediate).
    pub fn slli(&mut self, rd: Reg, rs1: Reg, shamt: u32) {
        self.i_type((shamt & 0x1f) as i32, rs1, 1, rd, 0x13);
    }
    /// `srli rd, rs1, shamt`.
    pub fn srli(&mut self, rd: Reg, rs1: Reg, shamt: u32) {
        self.i_type((shamt & 0x1f) as i32, rs1, 5, rd, 0x13);
    }
    /// `srai rd, rs1, shamt` (arithmetic; sets imm[10]).
    pub fn srai(&mut self, rd: Reg, rs1: Reg, shamt: u32) {
        self.i_type(((shamt & 0x1f) | 0x400) as i32, rs1, 5, rd, 0x13);
    }

    /// `lui rd, imm20` -- load the 20-bit immediate into rd[31:12], zeroing the low 12 bits.
    pub fn lui(&mut self, rd: Reg, imm20: u32) {
        self.emit_word(((imm20 & 0xfffff) << 12) | (u32::from(rd.number()) << 7) | 0x37);
    }

    /// `lw rd, imm(rs1)` -- load a word.
    pub fn lw(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 2, rd, 0x03);
    }
    /// `sw rs2, imm(rs1)` -- store a word.
    pub fn sw(&mut self, rs2: Reg, rs1: Reg, imm: i32) {
        let imm = imm as u32;
        self.emit_word(
            ((imm >> 5) & 0x7f) << 25
                | (u32::from(rs2.number()) << 20)
                | (u32::from(rs1.number()) << 15)
                | (2 << 12)
                | ((imm & 0x1f) << 7)
                | 0x23,
        );
    }

    /// `jalr rd, rs1, imm` -- jump to `rs1 + imm`, link into rd.
    pub fn jalr(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 0, rd, 0x67);
    }

    /// A conditional branch to `target` comparing `rs1` and `rs2`.
    pub fn branch(&mut self, cond: BranchCond, rs1: Reg, rs2: Reg, target: Label) {
        let site = self.position();
        self.fixups.push((site, Fixup::Branch, target.0));
        self.emit_word(
            (u32::from(rs2.number()) << 20)
                | (u32::from(rs1.number()) << 15)
                | (cond.funct3() << 12)
                | 0x63,
        );
    }

    /// `jal rd, target` -- jump to the label, link into rd.
    pub fn jal(&mut self, rd: Reg, target: Label) {
        let site = self.position();
        self.fixups.push((site, Fixup::Jump, target.0));
        self.emit_word((u32::from(rd.number()) << 7) | 0x6f);
    }


    /// `mv rd, rs` (`addi rd, rs, 0`).
    pub fn mv(&mut self, rd: Reg, rs: Reg) {
        self.addi(rd, rs, 0);
    }
    /// `li rd, imm` -- materialize a 32-bit constant (`addi`, or `lui`+`addi`).
    pub fn li(&mut self, rd: Reg, imm: i32) {
        if (-2048..=2047).contains(&imm) {
            self.addi(rd, Reg::ZERO, imm);
            return;
        }
        let upper = ((imm as i64 + 0x800) >> 12) as u32;
        let lower = imm - ((upper << 12) as i32);
        self.lui(rd, upper);
        if lower != 0 {
            self.addi(rd, rd, lower);
        }
    }
    /// `j target` -- unconditional jump (`jal x0, target`).
    pub fn j(&mut self, target: Label) {
        self.jal(Reg::ZERO, target);
    }
    /// `ret` -- return to the address in `ra` (`jalr x0, ra, 0`).
    pub fn ret(&mut self) {
        self.jalr(Reg::ZERO, Reg::RA, 0);
    }
    /// `ebreak` -- the environment breakpoint (used to enter semihosting/debug).
    pub fn ebreak(&mut self) {
        self.emit_word(0x0010_0073);
    }

    /// Resolves every label reference and returns the finished image, or an error if a label is
    /// unbound or a target is out of range.
    pub fn finish(mut self) -> Result<Assembled, AssembleError> {
        for &(site, fixup, label) in &self.fixups {
            let target = self
                .labels
                .get(label as usize)
                .and_then(|p| *p)
                .ok_or(AssembleError::UnboundLabel(Label(label)))?;
            let offset = target as i64 - site as i64;
            let base = u32::from_le_bytes([
                self.bytes[site as usize],
                self.bytes[site as usize + 1],
                self.bytes[site as usize + 2],
                self.bytes[site as usize + 3],
            ]);
            let imm = match fixup {
                Fixup::Branch => {
                    if !(-4096..=4094).contains(&offset) {
                        return Err(AssembleError::BranchOutOfRange);
                    }
                    let off = offset as u32;
                    ((off >> 12) & 1) << 31
                        | ((off >> 5) & 0x3f) << 25
                        | ((off >> 1) & 0xf) << 8
                        | ((off >> 11) & 1) << 7
                }
                Fixup::Jump => {
                    if !(-1_048_576..=1_048_574).contains(&offset) {
                        return Err(AssembleError::BranchOutOfRange);
                    }
                    let off = offset as u32;
                    ((off >> 20) & 1) << 31
                        | ((off >> 1) & 0x3ff) << 21
                        | ((off >> 11) & 1) << 20
                        | ((off >> 12) & 0xff) << 12
                }
            };
            let patched = (base | imm).to_le_bytes();
            self.bytes[site as usize..site as usize + 4].copy_from_slice(&patched);
        }
        Ok(Assembled { bytes: self.bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_addi_and_add() {
        let mut enc = Encoder::new();
        enc.addi(Reg::T0, Reg::ZERO, 40);
        enc.addi(Reg::T1, Reg::ZERO, 2);
        enc.add(Reg::A0, Reg::T0, Reg::T1);
        let bytes = enc.finish().unwrap().bytes;
        assert_eq!(&bytes[0..4], &0x0280_0293u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x0020_0313u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x0062_8533u32.to_le_bytes());
    }

    #[test]
    fn a_backward_branch_resolves() {
        let mut enc = Encoder::new();
        let top = enc.new_label();
        enc.bind_label(top);
        enc.addi(Reg::T0, Reg::T0, -1);
        enc.branch(BranchCond::Ne, Reg::T0, Reg::ZERO, top);
        let bytes = enc.finish().unwrap().bytes;
        assert_eq!(bytes.len(), 8);
    }
}
