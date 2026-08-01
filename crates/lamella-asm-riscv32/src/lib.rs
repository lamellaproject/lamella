//! An RV32IM machine-code encoder for the Lamella backend's RISC-V target.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// The machine CSR numbers this encoder's callers need, for [`Encoder::csrr`] / [`Encoder::csrw`].
///
/// A trap handler is the reason these exist: without an `mtvec` a fault jumps to address zero and
/// keeps faulting, which on QEMU is an image that HANGS rather than one that reports. Naming the
/// four here keeps the magic numbers out of every harness that installs one.
pub mod csr {
    /// `mstatus` -- machine status.
    pub const MSTATUS: u16 = 0x300;
    /// `mtvec` -- the machine trap-vector base. The low two bits are the MODE field, so a handler
    /// address written here must be 4-byte aligned and MODE 0 means "direct: every trap goes to
    /// BASE".
    pub const MTVEC: u16 = 0x305;
    /// `mepc` -- the PC the trap was taken on.
    pub const MEPC: u16 = 0x341;
    /// `mcause` -- why the trap was taken (2 = illegal instruction, 5 = load access fault,
    /// 7 = store access fault, and the high bit set means an interrupt rather than an exception).
    pub const MCAUSE: u16 = 0x342;
}

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
    /// Argument / syscall number `x17` (`a7`) -- the syscall id for an `ecall`.
    pub const A7: Reg = Reg(17);

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
    /// A branch or jump target is out of the encoding's reach, WITH THE SITE AND THE DISTANCE.
    ///
    /// A bare marker was survivable while nothing relaxed: the whole image simply refused. It is not
    /// survivable while diagnosing WHICH branch overflows and by how much -- a conditional branch
    /// reaches +/-4 KB and a jump +/-1 MB, so "out of range" alone cannot even say which tier was
    /// asked for. The ARM encoder learned this first and names its site too.
    BranchOutOfRange {
        /// The byte offset of the instruction whose target could not be reached.
        at: u32,
        /// The distance it needed to cover, in bytes, signed (negative = backwards).
        offset: i64,
        /// The reach the encoding actually has, in bytes, as `+/-limit`.
        limit: i64,
    },
}

/// The finished machine-code image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assembled {
    /// The little-endian RV32 byte image.
    pub bytes: Vec<u8>,
    /// The insertions branch relaxation made, `(position, bytes grown)`, ascending by position.
    /// EMPTY when nothing relaxed, which is the ordinary case. Feed a pre-relaxation offset through
    /// [`shift_position`] to find where it landed.
    pub shifts: Vec<(u32, u32)>,
}

/// `jal x0, 0` -- an unconditional jump with its offset still to patch (opcode `0x6f`, rd = x0),
/// the exact word [`Encoder::jal`] emits for [`Encoder::j`]. Spliced in to widen a far branch.
const JAL_X0_PLACEHOLDER: u32 = 0x0000_006f;

#[derive(Debug, Clone, Copy)]
enum Fixup {
    Branch,
    Jump,
    /// `la rd, label`: the auipc (hi20) at the site and the addi (lo12) at site+4 of a PC-relative
    /// address load, patched together as one pair.
    PcRel,
}

/// Accumulates RV32IM machine code and the label references into it.
#[derive(Debug, Clone, Default)]
pub struct Encoder {
    bytes: Vec<u8>,
    labels: Vec<Option<u32>>,
    fixups: Vec<(u32, Fixup, u32)>,
    /// `emit_word_diff` sites: `(word offset, base label, target label)` patched to `target - base`.
    diffs: Vec<(u32, u32, u32)>,
    /// Every insertion branch relaxation made, as `(position, bytes grown)`, in the order made.
    ///
    /// The encoder shifts what it OWNS (labels, fixups, diffs) itself. It cannot shift what it does
    /// not own, and this backend's caller records descriptor/call relocations against
    /// `position()` in its own vectors -- so relaxation would move every one of them off its
    /// instruction. Handing the insertions back lets the caller re-point them
    /// ([`Assembled::shifts`], applied with [`shift_position`]).
    shifts: Vec<(u32, u32)>,
}

/// Re-points a byte offset recorded BEFORE relaxation to where it sits AFTER, given the insertions
/// [`Assembled::shifts`] reports. An offset moves by the total inserted at or before it.
///
/// A site exactly AT an insertion point moves too: the relaxation splices the unconditional jump
/// AFTER the branch it widens, so anything recorded at that address belonged to the following
/// instruction and has to travel with it.
#[must_use]
pub fn shift_position(shifts: &[(u32, u32)], position: u32) -> u32 {
    shifts
        .iter()
        .filter(|(at, _)| *at <= position)
        .map(|(_, grow)| *grow)
        .sum::<u32>()
        + position
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

    /// Appends raw, already-assembled bytes (e.g. an embedded stub) into the stream. Their internal
    /// references must be self-contained -- this encoder's fixups do not reach inside them.
    pub fn emit_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Emits a 4-byte word holding the signed byte difference `target - base` of two labels, resolved
    /// at [`finish`]. Position-independent: a `base`-relative vtable slot (`method_entry - descriptor`)
    /// stays correct wherever the image loads, so dispatch adds the difference back to the descriptor
    /// address it reads at run time.
    pub fn emit_word_diff(&mut self, base: Label, target: Label) {
        let site = self.position();
        self.diffs.push((site, base.0, target.0));
        self.emit_word(0);
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
    /// `mulhu rd, rs1, rs2` (the `M` extension's high word of an UNSIGNED 32x32 product) -- the carry
    /// into the high half of a 64-bit multiply.
    pub fn mulhu(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(1, rs2, rs1, 3, rd, 0x33);
    }
    /// `div rd, rs1, rs2` (the `M` extension's signed division, truncating toward zero). RV32M
    /// semantics: division by zero yields all-ones (-1) and the MIN/-1 overflow yields MIN, neither
    /// traps -- a checked-context exception is a separate follow-up (as on ARM).
    pub fn div(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(1, rs2, rs1, 4, rd, 0x33);
    }
    /// `divu rd, rs1, rs2` (the `M` extension's unsigned division). Division by zero yields all-ones.
    pub fn divu(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(1, rs2, rs1, 5, rd, 0x33);
    }
    /// `rem rd, rs1, rs2` (the `M` extension's signed remainder, with the sign of the dividend).
    /// Remainder by zero yields the dividend.
    pub fn rem(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(1, rs2, rs1, 6, rd, 0x33);
    }
    /// `remu rd, rs1, rs2` (the `M` extension's unsigned remainder). Remainder by zero yields the dividend.
    pub fn remu(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.r_type(1, rs2, rs1, 7, rd, 0x33);
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

    /// `auipc rd, imm20` -- rd = pc + (imm20 << 12); the PC-relative high half of an address load.
    pub fn auipc(&mut self, rd: Reg, imm20: u32) {
        self.emit_word(((imm20 & 0xfffff) << 12) | (u32::from(rd.number()) << 7) | 0x17);
    }

    /// `la rd, label` -- load the (PC-relative) address of `label` into `rd`, as the standard
    /// `auipc rd, %pcrel_hi(label)` + `addi rd, rd, %pcrel_lo(label)` pair (resolved at `finish`).
    /// Position-independent, so it addresses an in-image datum (e.g. a GC TypeDesc) wherever the
    /// image loads.
    pub fn la(&mut self, rd: Reg, label: Label) {
        let site = self.position();
        self.auipc(rd, 0);
        self.fixups.push((site, Fixup::PcRel, label.0));
        self.i_type(0, rd, 0, rd, 0x13);
    }

    /// `lw rd, imm(rs1)` -- load a word.
    pub fn lw(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 2, rd, 0x03);
    }
    /// An S-type store (`STORE` opcode `0x23`): `rs2 -> imm(rs1)`, the `funct3` selecting the width
    /// (`sb`=0, `sh`=1, `sw`=2).
    fn s_type(&mut self, imm: i32, rs2: Reg, rs1: Reg, funct3: u32) {
        let imm = imm as u32;
        self.emit_word(
            ((imm >> 5) & 0x7f) << 25
                | (u32::from(rs2.number()) << 20)
                | (u32::from(rs1.number()) << 15)
                | (funct3 << 12)
                | ((imm & 0x1f) << 7)
                | 0x23,
        );
    }

    /// `lb rd, imm(rs1)` -- load a sign-extended byte.
    pub fn lb(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 0, rd, 0x03);
    }
    /// `lh rd, imm(rs1)` -- load a sign-extended halfword.
    pub fn lh(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 1, rd, 0x03);
    }
    /// `lbu rd, imm(rs1)` -- load a zero-extended byte.
    pub fn lbu(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 4, rd, 0x03);
    }
    /// `lhu rd, imm(rs1)` -- load a zero-extended halfword.
    pub fn lhu(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.i_type(imm, rs1, 5, rd, 0x03);
    }
    /// `sb rs2, imm(rs1)` -- store a byte.
    pub fn sb(&mut self, rs2: Reg, rs1: Reg, imm: i32) {
        self.s_type(imm, rs2, rs1, 0);
    }
    /// `sh rs2, imm(rs1)` -- store a halfword.
    pub fn sh(&mut self, rs2: Reg, rs1: Reg, imm: i32) {
        self.s_type(imm, rs2, rs1, 1);
    }
    /// `sw rs2, imm(rs1)` -- store a word.
    pub fn sw(&mut self, rs2: Reg, rs1: Reg, imm: i32) {
        self.s_type(imm, rs2, rs1, 2);
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
        let lower = imm.wrapping_sub((upper << 12) as i32);
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


    /// `csrrw rd, csr, rs1` -- atomically write `rs1` into `csr` and read the OLD value into `rd`.
    /// With `rd = x0` the read is suppressed, which is the `csrw` pseudo-instruction
    /// ([`Encoder::csrw`]).
    pub fn csrrw(&mut self, rd: Reg, csr: u16, rs1: Reg) {
        self.i_type(i32::from(csr), rs1, 1, rd, 0x73);
    }

    /// `csrrs rd, csr, rs1` -- read `csr` into `rd` and SET the bits `rs1` names. With `rs1 = x0`
    /// nothing is written, which is the `csrr` pseudo-instruction ([`Encoder::csrr`]).
    pub fn csrrs(&mut self, rd: Reg, csr: u16, rs1: Reg) {
        self.i_type(i32::from(csr), rs1, 2, rd, 0x73);
    }

    /// `csrw csr, rs` -- write a CSR, discarding its old value (`csrrw x0, csr, rs`).
    pub fn csrw(&mut self, csr: u16, rs: Reg) {
        self.csrrw(Reg::ZERO, csr, rs);
    }

    /// `csrr rd, csr` -- read a CSR without writing it (`csrrs rd, csr, x0`).
    pub fn csrr(&mut self, rd: Reg, csr: u16) {
        self.csrrs(rd, csr, Reg::ZERO);
    }

    /// Inserts `insert` at `at`, carrying everything the encoder owns past it: a label, fixup site
    /// or diff site AT OR AFTER the insertion belongs to code that just moved. Records the growth so
    /// the caller can carry ITS relocations too (see [`Encoder::shifts`]).
    fn splice_in(&mut self, at: u32, insert: &[u8]) {
        let grow = insert.len() as u32;
        let pos = at as usize;
        self.bytes.splice(pos..pos, insert.iter().copied());
        for slot in self.labels.iter_mut().flatten() {
            if *slot >= at {
                *slot += grow;
            }
        }
        for (site, _, _) in &mut self.fixups {
            if *site >= at {
                *site += grow;
            }
        }
        for (site, _, _) in &mut self.diffs {
            if *site >= at {
                *site += grow;
            }
        }
        self.shifts.push((at, grow));
        self.shifts.sort_unstable();
    }

    /// Widens ONE conditional branch that cannot reach its target, and reports whether it did.
    ///
    /// A B-type branch reaches +/-4 KB; a J-type jump reaches +/-1 MB. So a far branch becomes the
    /// INVERTED branch over an unconditional jump to the real target: the taken path falls into the
    /// jump, the not-taken path skips it. Inverting is one bit -- `beq`/`bne`, `blt`/`bge`,
    /// `bltu`/`bgeu` differ only in funct3 bit 0.
    ///
    /// One per call, because the splice moves every later site and a second far branch has to be
    /// re-measured against the new layout rather than the old. [`Encoder::finish`] loops to a
    /// fixpoint.
    fn widen_far_conditional_branch(&mut self) -> Result<bool, AssembleError> {
        for idx in 0..self.fixups.len() {
            let (site, fixup, label_id) = self.fixups[idx];
            if !matches!(fixup, Fixup::Branch) {
                continue;
            }
            let target = match self.labels.get(label_id as usize) {
                Some(Some(offset)) => *offset,
                _ => return Err(AssembleError::UnboundLabel(Label(label_id))),
            };
            if (-4096..=4094).contains(&(target as i64 - site as i64)) {
                continue;
            }
            self.splice_in(site + 4, &JAL_X0_PLACEHOLDER.to_le_bytes());
            let s = site as usize;
            let word = u32::from_le_bytes([
                self.bytes[s],
                self.bytes[s + 1],
                self.bytes[s + 2],
                self.bytes[s + 3],
            ]);
            self.bytes[s..s + 4].copy_from_slice(&(word ^ (1 << 12)).to_le_bytes());
            let skip = self.new_label();
            self.labels[skip.0 as usize] = Some(site + 8);
            self.fixups[idx].2 = skip.0;
            self.fixups.push((site + 4, Fixup::Jump, label_id));
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolves every label reference and returns the finished image, or an error if a label is
    /// unbound or a target is out of range.
    pub fn finish(mut self) -> Result<Assembled, AssembleError> {
        while self.widen_far_conditional_branch()? {}
        for &(site, base_label, target_label) in &self.diffs {
            let base = self
                .labels
                .get(base_label as usize)
                .and_then(|p| *p)
                .ok_or(AssembleError::UnboundLabel(Label(base_label)))?;
            let target = self
                .labels
                .get(target_label as usize)
                .and_then(|p| *p)
                .ok_or(AssembleError::UnboundLabel(Label(target_label)))?;
            let diff = (target as i64 - base as i64) as u32;
            self.bytes[site as usize..site as usize + 4].copy_from_slice(&diff.to_le_bytes());
        }
        for &(site, fixup, label) in &self.fixups {
            let target = self
                .labels
                .get(label as usize)
                .and_then(|p| *p)
                .ok_or(AssembleError::UnboundLabel(Label(label)))?;
            let offset = target as i64 - site as i64;
            if let Fixup::PcRel = fixup {
                let lo12 = (offset & 0xfff) as u32;
                let hi20 = (((offset + 0x800) >> 12) & 0xfffff) as u32;
                let s = site as usize;
                let auipc = u32::from_le_bytes([
                    self.bytes[s],
                    self.bytes[s + 1],
                    self.bytes[s + 2],
                    self.bytes[s + 3],
                ]) | (hi20 << 12);
                self.bytes[s..s + 4].copy_from_slice(&auipc.to_le_bytes());
                let a = s + 4;
                let addi = u32::from_le_bytes([
                    self.bytes[a],
                    self.bytes[a + 1],
                    self.bytes[a + 2],
                    self.bytes[a + 3],
                ]) | (lo12 << 20);
                self.bytes[a..a + 4].copy_from_slice(&addi.to_le_bytes());
                continue;
            }
            let base = u32::from_le_bytes([
                self.bytes[site as usize],
                self.bytes[site as usize + 1],
                self.bytes[site as usize + 2],
                self.bytes[site as usize + 3],
            ]);
            let imm = match fixup {
                Fixup::Branch => {
                    if !(-4096..=4094).contains(&offset) {
                        return Err(AssembleError::BranchOutOfRange {
                            at: site,
                            offset,
                            limit: 4096,
                        });
                    }
                    let off = offset as u32;
                    ((off >> 12) & 1) << 31
                        | ((off >> 5) & 0x3f) << 25
                        | ((off >> 1) & 0xf) << 8
                        | ((off >> 11) & 1) << 7
                }
                Fixup::Jump => {
                    if !(-1_048_576..=1_048_574).contains(&offset) {
                        return Err(AssembleError::BranchOutOfRange {
                            at: site,
                            offset,
                            limit: 1_048_576,
                        });
                    }
                    let off = offset as u32;
                    ((off >> 20) & 1) << 31
                        | ((off >> 1) & 0x3ff) << 21
                        | ((off >> 11) & 1) << 20
                        | ((off >> 12) & 0xff) << 12
                }
                Fixup::PcRel => unreachable!("PcRel is patched before this match"),
            };
            let patched = (base | imm).to_le_bytes();
            self.bytes[site as usize..site as usize + 4].copy_from_slice(&patched);
        }
        Ok(Assembled {
            bytes: self.bytes,
            shifts: self.shifts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_far_conditional_branch_widens_into_an_inverted_branch_over_a_jump() {
        let mut enc = Encoder::new();
        let far = enc.new_label();
        enc.branch(BranchCond::Eq, Reg::T0, Reg::T1, far);
        for _ in 0..2000 {
            enc.addi(Reg::ZERO, Reg::ZERO, 0);
        }
        enc.bind_label(far);
        let out = enc.finish().expect("a far branch widens instead of refusing");
        let word = |i: usize| {
            u32::from_le_bytes([
                out.bytes[i],
                out.bytes[i + 1],
                out.bytes[i + 2],
                out.bytes[i + 3],
            ])
        };
        assert_eq!(word(0) & 0x7f, 0x63, "the first instruction is still a branch");
        assert_eq!(
            (word(0) >> 12) & 7,
            1,
            "beq must have inverted to bne, so the NOT-taken path skips the jump"
        );
        assert_eq!(word(4) & 0x7f, 0x6f, "a jal must follow it");
        assert_eq!((word(4) >> 7) & 0x1f, 0, "the jal links into x0 -- an unconditional jump");
        let imm = ((word(0) >> 8) & 0xf) << 1 | ((word(0) >> 25) & 0x3f) << 5 | ((word(0) >> 7) & 1) << 11;
        assert_eq!(imm, 8, "the inverted branch lands just past the jump");
        assert_eq!(
            out.shifts.as_slice(),
            &[(4u32, 4u32)][..],
            "one 4-byte insertion, reported to the caller"
        );
    }

    #[test]
    fn a_reachable_branch_is_left_exactly_as_it_was() {
        let mut enc = Encoder::new();
        let near = enc.new_label();
        enc.branch(BranchCond::Eq, Reg::T0, Reg::T1, near);
        enc.addi(Reg::ZERO, Reg::ZERO, 0);
        enc.bind_label(near);
        let out = enc.finish().expect("a near branch assembles");
        assert_eq!(out.bytes.len(), 8, "no instruction was inserted");
        assert!(out.shifts.is_empty(), "nothing moved, so nothing to re-point");
        let w = u32::from_le_bytes([out.bytes[0], out.bytes[1], out.bytes[2], out.bytes[3]]);
        assert_eq!((w >> 12) & 7, 0, "beq stays beq");
    }

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
    fn csr_access_matches_the_reference_assembler() {
        let mut enc = Encoder::new();
        enc.csrw(csr::MTVEC, Reg::T0);
        enc.csrr(Reg::A0, csr::MCAUSE);
        enc.csrr(Reg::A0, csr::MEPC);
        let bytes = enc.finish().unwrap().bytes;
        assert_eq!(&bytes[0..4], &0x3052_9073u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x3420_2573u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x3410_2573u32.to_le_bytes());
    }

    #[test]
    fn li_materializes_extreme_constants() {
        fn decode_li(bytes: &[u8]) -> i32 {
            let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
            let mut acc = 0i32;
            let mut i = 0;
            while i + 4 <= bytes.len() {
                let w = word(i);
                match w & 0x7f {
                    0x37 => acc = (w & 0xFFFF_F000) as i32,
                    0x13 => acc = acc.wrapping_add((w as i32) >> 20),
                    other => panic!("unexpected opcode {other:#x} in li sequence"),
                }
                i += 4;
            }
            acc
        }
        for imm in [
            0,
            1,
            -1,
            2047,
            -2048,
            2048,
            i32::MAX,
            i32::MIN,
            0x7FFF_F800u32 as i32,
            0x1234_5000,
            0x1234_5678,
            -0x1234_5678,
        ] {
            let mut enc = Encoder::new();
            enc.li(Reg::A0, imm);
            let bytes = enc.finish().unwrap().bytes;
            assert_eq!(decode_li(&bytes), imm, "li({imm:#x}) round-trips");
        }
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

    #[test]
    fn encodes_the_m_extension_division() {
        let mut enc = Encoder::new();
        enc.div(Reg::A0, Reg::T0, Reg::T1);
        enc.divu(Reg::A0, Reg::T0, Reg::T1);
        enc.rem(Reg::A0, Reg::T0, Reg::T1);
        enc.remu(Reg::A0, Reg::T0, Reg::T1);
        let bytes = enc.finish().unwrap().bytes;
        assert_eq!(&bytes[0..4], &0x0262_c533u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x0262_d533u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x0262_e533u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &0x0262_f533u32.to_le_bytes());
    }

    #[test]
    fn emit_word_diff_holds_the_signed_label_difference() {
        let mut enc = Encoder::new();
        let base = enc.new_label();
        let target = enc.new_label();
        let back = enc.new_label();
        enc.bind_label(back);
        enc.emit_word(0xDEAD_BEEF);
        enc.bind_label(base);
        enc.emit_word_diff(base, target);
        enc.emit_word_diff(base, back);
        enc.bind_label(target);
        let bytes = enc.finish().unwrap().bytes;
        assert_eq!(&bytes[4..8], &8i32.to_le_bytes());
        assert_eq!(&bytes[8..12], &(-4i32).to_le_bytes());
    }

    #[test]
    fn la_loads_a_pc_relative_label_address() {
        let mut enc = Encoder::new();
        let data = enc.new_label();
        enc.la(Reg::A0, data);
        enc.ret();
        enc.bind_label(data);
        enc.emit_word(0xDEAD_BEEF);
        let bytes = enc.finish().unwrap().bytes;
        assert_eq!(&bytes[0..4], &0x0000_0517u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x00c5_0513u32.to_le_bytes());
    }
}
