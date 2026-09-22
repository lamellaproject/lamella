//! An on-device implementation of [`DebugBackend`]: it drives a halted Cortex-M target
//! over the Lamella CMSIS-DAP stack, so the `lamella-dap` adapter -- and through it VS
//! Code -- can debug AOT-compiled code running on real hardware, the same protocol layer
//! that drives the interpreter.

mod innermost;
pub mod program;
mod thumb;

/// Whether a client's document names the file a row recorded.
///
/// **NOT STRING EQUALITY, because the two ends spell a path differently and neither is wrong.** A
/// producer records what its command line was given -- often relative, and with the separator of
/// the host it ran on -- while a DAP client sends the absolute path it opened. Comparing the file
/// NAME and requiring the recorded path to be a suffix of the client's is what makes
/// `Blink.swift`, `Sources/Blink.swift` and `C:\work\Sources\Blink.swift` the same file, without
/// making two different `Blink.swift` files in separate directories the same one.
fn paths_match(recorded: &str, document: &str) -> bool {
    let normalize = |path: &str| path.replace('\\', "/").to_ascii_lowercase();
    let (recorded, document) = (normalize(recorded), normalize(document));
    recorded == document
        || document.ends_with(&format!("/{recorded}"))
        || recorded.ends_with(&format!("/{document}"))
}

use core::cell::RefCell;

use lamella_probe_core::TargetAccess;
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, SourceLocation, Stop, Variable,
};

/// The stack pointer's DWARF register number on ARM, which is not its ADIv5 selector by accident:
/// both are 13, and every other register in this file is addressed by one or the other. Named so a
/// reader can tell which numbering a call site meant.
const SP_DWARF_REGISTER: u64 = 13;

/// One row of the native-offset to source map.
///
/// # EVERY ROW CARRIES ITS OWN FILE
///
/// One file held beside the whole table is enough for a single-source program, and an AOT C#
/// program is usually that. **A Swift image is not**: a compilation unit can declare fourteen
/// files, and the rows interleave at INSTRUCTION granularity -- within a thirty-byte span of one
/// function, six rows can come from four different files. Resolved against one filename, a stop at
/// such an address yields a real line number against the wrong file, which is **a plausible answer
/// the user cannot check** rather than a failure anyone would notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRow {
    /// Distance from [`DeviceBackend`]'s base, which is what a PC minus that base gives.
    pub offset: u32,
    /// The 1-based source line, or 0 for an address the producer attributed to no line.
    pub line: u32,
    /// Which entry of the backend's file table this row came from.
    pub file: u32,
}

/// Drives a Cortex-M target over ANY probe as a [`DebugBackend`]. The trait's inspection methods
/// take `&self` (suited to the interpreter's in-memory state), so the probe sits behind a
/// `RefCell` for the I/O those methods must perform.
///
/// **GENERIC OVER [`TargetAccess`], NOT OVER ONE PROBE'S PACKET TRANSPORT.** The surface it uses is
/// twelve methods -- `connect`, `init_mem`, `read_idcode`, `read_word`, `write_word`,
/// `read_core_reg`, `write_core_reg`, `step`, `resume`, `halt`, `is_halted` and
/// `set_breakpoints` -- and every one is on `TargetAccess`. Any probe implementing that trait
/// drives this backend: CMSIS-DAP and `lamella_stlink::StLink` alike.
pub struct DeviceBackend<A: TargetAccess> {
    probe: RefCell<A>,
    /// The source map for the loaded program, ascending by offset.
    lines: Vec<LineRow>,
    /// The loaded method's flash base, subtracted from a PC to index `lines`.
    base: u32,
    /// Per-method `(image offset, end offset, name)`, ascending by offset -- the frame name at a PC.
    names: Vec<(u32, u32, String)>,
    /// The files the rows index. Empty when no source map was composed.
    files: Vec<String>,
    /// Semihosting output captured from the target, drained by `take_output`.
    output: String,
    /// The user's hardware breakpoints (the code addresses last set), kept so a step-over can
    /// re-arm them around its temporary return-address breakpoint.
    breakpoints: Vec<u32>,
    /// How many instruction address comparators the target's breakpoint unit reports, read from
    /// `FP_CTRL` at launch. `None` before launch, and when that read fails.
    comparators: Option<usize>,
    /// The entry method's `Type.Method` name. Stepping out of it means "continue" -- the entry
    /// has no caller within the program (its return is the startup trampoline), so there is no
    /// frame to return to.
    entry: String,
    /// The image's `.debug_frame`, from which a call stack is computed. Empty for a program that
    /// carries none, and then a stop reports the one frame it can see.
    ///
    /// **HELD AS BYTES AND PARSED PER STOP, ON PURPOSE.** The reader borrows the section, so an
    /// owned table would be a self-reference; and a stop is a human-scale event where the parse is
    /// bounded by the section rather than by the program's running time.
    frame_section: Vec<u8>,
    /// The sections a variables pane is read from, and the image's executable ranges. Empty until
    /// [`Self::with_locals`] is called, and then a session reports no variables -- which is what it
    /// did before there was a reader for them.
    locals: crate::program::LocalSections,
    /// The image's Arm exception-handling tables, consulted for a frame `.debug_frame` has no row
    /// for. Empty until [`Self::with_unwind_tables`] is called, and then such a frame ends the walk
    /// -- which is what it did before there was a reader for them.
    unwind: crate::program::UnwindTables,
}

impl<A: TargetAccess> DeviceBackend<A> {
    /// Wraps a probe with the loaded method's line table (native offset -> source line),
    /// its flash `base`, per-method `names`, the source `file` it came from, and the `entry`
    /// method's name (stepping out of which continues, having no in-program caller).
    pub fn new(
        probe: A,
        lines: Vec<LineRow>,
        base: u32,
        names: Vec<(u32, u32, String)>,
        files: Vec<String>,
        entry: String,
        frame_section: Vec<u8>,
    ) -> Self {
        DeviceBackend {
            probe: RefCell::new(probe),
            lines,
            base,
            names,
            files,
            output: String::new(),
            breakpoints: Vec::new(),
            comparators: None,
            entry,
            frame_section,
            locals: crate::program::LocalSections::default(),
            unwind: crate::program::UnwindTables::default(),
        }
    }

    /// Every frame, each with THE STACK POINTER THAT FRAME HAD.
    ///
    /// The second value is what a local's frame offset resolves against, and it is only the core
    /// register for the innermost frame -- above that it is the callee's canonical frame address,
    /// which is the caller's stack pointer by definition. Computing it here rather than twice is
    /// what stops a variables pane and a call stack disagreeing about where a frame is.
    fn walk_frames(&self) -> Vec<(Frame, u32)> {
        let live = self.live_registers();
        let mut sections = lamella_dwarf::Sections::default();
        sections.set(".debug_frame", &self.frame_section);
        let frames = lamella_dwarf::FrameTable::parse(&sections).ok();

        let mut out = Vec::new();
        let (mut pc, mut sp, mut lr) = (
            live[15].unwrap_or(0),
            live[13].unwrap_or(0),
            live[14].unwrap_or(0),
        );
        let mut registers = live;
        for depth in 0..32u32 {
            let lookup = if depth == 0 { pc } else { pc.saturating_sub(1) };
            let offset = self.image_offset(lookup);
            out.push((
                Frame {
                    address: u64::from(pc),
                    name: self.method_name_at(offset),
                    line: self.source_line_at(offset),
                },
                sp,
            ));

            let row = frames.as_ref().map(|table| table.unwind(u64::from(lookup)));
            let caller = match row {
                Some(Ok(Some(rules))) => {
                    registers = [None; 16];
                    self.caller_from_frame_table(&rules, depth, pc, sp, lr)
                }
                Some(Err(_)) => None,
                Some(Ok(None)) | None => self.caller_from_index_table(depth, lookup, &mut registers),
            };
            let Some((return_address, cfa)) = caller else {
                break;
            };

            let next = return_address & !1;
            if next == 0 || next == pc || cfa < sp {
                break;
            }
            lr = return_address;
            pc = next;
            sp = cfa;
            registers[13] = Some(cfa);
            registers[14] = Some(return_address);
            registers[15] = Some(next);
        }
        out
    }

    /// The caller of the frame at `pc` from its `.debug_frame` row: the return address, and the
    /// caller's stack pointer, which is the row's canonical frame address.
    fn caller_from_frame_table(
        &self,
        rules: &lamella_dwarf::UnwindRow<'_>,
        depth: u32,
        pc: u32,
        sp: u32,
        lr: u32,
    ) -> Option<(u32, u32)> {
        if rules.truncated_at.is_some() {
            return None;
        }
        if !rules.describes_a_possible_frame(SP_DWARF_REGISTER) {
            return None;
        }
        let lamella_dwarf::CfaRule::RegisterOffset { register, offset } = rules.cfa else {
            return None;
        };
        let cfa_base = match register {
            13 => sp,
            14 => lr,
            15 => pc,
            other => {
                let other = u8::try_from(other).ok()?;
                match self.probe.borrow_mut().read_core_reg(other) {
                    Ok(value) if depth == 0 => value,
                    _ => return None,
                }
            }
        };
        let cfa = cfa_base.checked_add_signed(i32::try_from(offset).ok()?)?;

        let return_address = match rules.return_address() {
            lamella_dwarf::RegisterRule::Undefined if depth > 0 => return None,
            lamella_dwarf::RegisterRule::Undefined | lamella_dwarf::RegisterRule::SameValue => {
                let (start, end) = rules.function;
                let candidate = u64::from(lr & !1);
                if candidate >= start && candidate < end {
                    return None;
                }
                if !self.follows_a_call(lr & !1) {
                    return None;
                }
                lr
            }
            lamella_dwarf::RegisterRule::Offset(at) => {
                let address = cfa.checked_add_signed(i32::try_from(at).ok()?)?;
                let bytes = self.read_memory(u64::from(address), 4);
                if bytes.len() < 4 {
                    return None;
                }
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            lamella_dwarf::RegisterRule::Register(number) => {
                let number = u8::try_from(number).ok()?;
                match self.probe.borrow_mut().read_core_reg(number) {
                    Ok(value) if depth == 0 => value,
                    _ => return None,
                }
            }
            lamella_dwarf::RegisterRule::ValOffset(_)
            | lamella_dwarf::RegisterRule::Expression(_)
            | lamella_dwarf::RegisterRule::ValExpression(_) => return None,
        };
        Some((return_address, cfa))
    }

    /// The caller of the frame at `lookup` from the image's index tables, where `.debug_frame` has no
    /// row for its function: the return address, and the caller's stack pointer.
    ///
    /// `registers` are that frame's registers as far as the walk knows them; on success they become
    /// the caller's.
    fn caller_from_index_table(
        &self,
        depth: u32,
        lookup: u32,
        registers: &mut lamella_elf::ehabi::Registers,
    ) -> Option<(u32, u32)> {
        let described = self.unwind.describe(lookup)?;
        let mut caller = *registers;
        if depth == 0 {
            let xpsr = self.probe.borrow_mut().read_core_reg(16).ok()?;
            let placement = crate::innermost::place(
                described.entry.function,
                registers,
                xpsr,
                &described.description,
                |address| self.halfword(address),
                |address| self.probe.borrow_mut().read_word(address).ok(),
            );
            match placement {
                crate::innermost::Placement::Caller(placed) => caller = placed,
                crate::innermost::Placement::Body => {
                    self.unwind_through(&described.description, &mut caller)?;
                }
                crate::innermost::Placement::Undecided => return None,
            }
        } else {
            self.unwind_through(&described.description, &mut caller)?;
        }
        let return_address = caller[15]?;
        let caller_stack_pointer = caller[13]?;
        if !self.follows_a_call(return_address & !1) {
            return None;
        }
        *registers = caller;
        Some((return_address, caller_stack_pointer))
    }

    /// Runs an index entry's `description` over `registers`, reading the stack through the probe.
    ///
    /// A register a called function need not preserve is unknown afterwards unless the frame restored
    /// it: the value left in it belongs to the callee.
    fn unwind_through(
        &self,
        description: &lamella_elf::ehabi::Description,
        registers: &mut lamella_elf::ehabi::Registers,
    ) -> Option<()> {
        let restored = lamella_elf::ehabi::unwind(description, registers, |address| {
            self.probe.borrow_mut().read_word(address).ok()
        })
        .ok()?;
        for register in crate::innermost::NOT_PRESERVED {
            if restored & (1 << register) == 0 {
                registers[register] = None;
            }
        }
        Some(())
    }

    /// The halted core's registers, r0 to r15, each `None` where the probe could not read it.
    ///
    /// For an image with no index tables only the stack pointer, the link register and the program
    /// counter are read, because nothing else is used and each read is a round trip to the probe.
    fn live_registers(&self) -> lamella_elf::ehabi::Registers {
        let mut registers = [None; 16];
        let mut probe = self.probe.borrow_mut();
        for (number, register) in (0u8..).zip(registers.iter_mut()) {
            if number < 13 && self.unwind.is_empty() {
                continue;
            }
            *register = probe.read_core_reg(number).ok();
        }
        registers
    }

    /// The Thumb halfword at `address`, read through the whole word that contains it.
    ///
    /// **A WORD IS ONLY EVER ASKED FOR AT A MULTIPLE OF FOUR.** A Thumb instruction starts at any
    /// even address, and reading a word at one that is not aligned would make the answer depend on
    /// what each probe does with an address between two words.
    fn halfword(&self, address: u32) -> Option<u16> {
        let word = self.probe.borrow_mut().read_word(address & !3).ok()?;
        Some(if address & 2 != 0 { (word >> 16) as u16 } else { word as u16 })
    }

    /// The stack pointer frame `index` had, which is what its locals' frame offsets count from.
    fn stack_pointer_of_frame(&self, index: usize) -> Option<u32> {
        self.walk_frames().get(index).map(|&(_, sp)| sp)
    }

    /// Renders one local as a value and a type, reading the target where the place says to.
    ///
    /// # THE TYPE COLUMN SAYS WHERE THE VALUE CAME FROM, NOT WHAT THE VARIABLE IS DECLARED AS
    ///
    /// Rendering a Swift `Int` as an integer needs the type graph in `.debug_info`, which this does
    /// not read yet -- so every value is shown as four raw bytes and the column says which place
    /// they were read from. **That is a smaller lie than a decoded value would be**: a person can
    /// see that `r4` is being reported and check it against the register view, where "42" for a
    /// variable that is actually a `Float` is indistinguishable from the truth.
    fn render(
        &self,
        local: &lamella_dwarf::Local<'_>,
        address: u64,
        frame_pointer: u32,
        frame_base: &lamella_dwarf::FrameBase<'_>,
        depth: usize,
    ) -> (String, String) {
        let word = |bytes: &[u8]| -> String {
            if bytes.len() < 4 {
                "<unreadable>".to_string()
            } else {
                format!("0x{:08x}", u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
        };
        match local.place_at(address) {
            lamella_dwarf::Place::FrameOffset(offset) => {
                let lamella_dwarf::FrameBase::Register(register) = frame_base else {
                    return ("<frame base is not a register>".to_string(), "unsupported".to_string());
                };
                let Ok(offset) = i32::try_from(offset) else {
                    return ("<offset out of range>".to_string(), "unsupported".to_string());
                };
                let (base, spelling) = if *register == SP_DWARF_REGISTER {
                    (frame_pointer, "sp".to_string())
                } else {
                    if depth != 0 {
                        return (
                            "<frame-pointer base, not recovered for an outer frame>".to_string(),
                            "unsupported".to_string(),
                        );
                    }
                    let Ok(number) = u8::try_from(*register) else {
                        return ("<register out of range>".to_string(), "unsupported".to_string());
                    };
                    match self.probe.borrow_mut().read_core_reg(number) {
                        Ok(value) => (value, format!("r{register}")),
                        Err(_) => return ("<unreadable>".to_string(), "unsupported".to_string()),
                    }
                };
                let Some(at) = base.checked_add_signed(offset) else {
                    return ("<offset off the frame>".to_string(), "unsupported".to_string());
                };
                (word(&self.read_memory(u64::from(at), 4)), format!("[{spelling}{offset:+}]"))
            }
            lamella_dwarf::Place::Register(register) => {
                let Ok(number) = u8::try_from(register) else {
                    return ("<register out of range>".to_string(), "unsupported".to_string());
                };
                match self.probe.borrow_mut().read_core_reg(number) {
                    Ok(value) => (format!("0x{value:08x}"), format!("r{register}")),
                    Err(_) => ("<unreadable>".to_string(), format!("r{register}")),
                }
            }
            lamella_dwarf::Place::RegisterOffset { register, offset } => {
                let (Ok(number), Ok(offset)) = (u8::try_from(register), i32::try_from(offset))
                else {
                    return ("<register out of range>".to_string(), "unsupported".to_string());
                };
                let read = self.probe.borrow_mut().read_core_reg(number);
                let Ok(base) = read else {
                    return ("<unreadable>".to_string(), format!("[r{register}{offset:+}]"));
                };
                let Some(at) = base.checked_add_signed(offset) else {
                    return ("<offset off the register>".to_string(), "unsupported".to_string());
                };
                (word(&self.read_memory(u64::from(at), 4)), format!("[r{register}{offset:+}]"))
            }
            lamella_dwarf::Place::Address(at) => {
                (word(&self.read_memory(at, 4)), format!("[0x{at:08x}]"))
            }
            lamella_dwarf::Place::Expression(_) => {
                ("<expression>".to_string(), "unsupported".to_string())
            }
            lamella_dwarf::Place::Nowhere => {
                ("<not live here>".to_string(), "optimized".to_string())
            }
        }
    }

    /// Gives the session what it needs to answer `variables`.
    ///
    #[must_use]
    pub fn with_locals(mut self, locals: crate::program::LocalSections) -> Self {
        self.locals = locals;
        self
    }

    /// Gives the walk the image's Arm exception-handling tables, for the frames `.debug_frame` does
    /// not describe.
    #[must_use]
    pub fn with_unwind_tables(mut self, unwind: crate::program::UnwindTables) -> Self {
        self.unwind = unwind;
        self
    }

    /// The 1-based source line whose native code contains `offset` (the last entry at or
    /// before it), or 0 if unknown.
    fn source_line_at(&self, offset: u32) -> u32 {
        self.row_at(offset).map_or(0, |row| row.line)
    }

    /// Whether the core is locked up, read from `DHCSR.S_LOCKUP` rather than guessed from the PC.
    ///
    /// # THE BIT, NOT THE ADDRESS
    ///
    /// A locked-up Cortex-M0 fetches from `0xFFFFFFFE` and a halt lands the PC there (Armv6-M ARM
    /// DDI 0419E, B1.5), which makes that value tempting to test for. It is not portable: Armv7-M
    /// fetches from "the Lockup address, determined by the nature of the fault" (Armv7-M ARM
    /// DDI 0403E.d, B1.5.15), so the address is a property of the fault on that profile and of the
    /// architecture only on this one. [`S_LOCKUP`] means the same thing on all of them.
    ///
    /// A read that fails answers false: a probe that cannot reach `DHCSR` has not established that
    /// the part is locked up, and the caller's own error path covers a transport that is not
    /// answering.
    fn locked_up(&mut self) -> bool {
        self.probe
            .get_mut()
            .read_word(DHCSR)
            .is_ok_and(|status| status & S_LOCKUP != 0)
    }

    /// The image offset of a target `address`, for the lookups that take one.
    ///
    /// # AN ADDRESS BELOW THE IMAGE MUST NOT BECOME ITS FIRST BYTE
    ///
    /// Saturating the subtraction would put every address below the image at offset 0 -- inside the
    /// first method, on the first line -- so a PC, an LR or a breakpoint address from outside the
    /// program would be named and located as the program's own first statement.
    ///
    /// An address below the base has no offset in this image, and this answers [`u32::MAX`] for it:
    /// no method range can contain it (they are half-open, so `MAX < end` is false whatever `end`
    /// is) and no row's offset equals it, so every bounded lookup here reports "outside" on its own
    /// terms -- no name, no line, no row -- without a caller having to test for it.
    fn image_offset(&self, address: u32) -> u32 {
        address.checked_sub(self.base).unwrap_or(u32::MAX)
    }

    /// The row whose native code contains `offset` -- the last one at or before it, and only where
    /// the program has code at `offset` at all.
    ///
    /// # THE BOUND IS THE POINT, BECAUSE A BACKWARD SEARCH ALONE CLAMPS
    ///
    /// The rows carry no end, so "the last row at or before the offset" is the right rule *inside*
    /// the program and answers the FINAL row for every address above it. An address far outside the
    /// image therefore came back as the program's last line, and the further wrong the address was,
    /// the more definite the answer looked. [`Self::method_name_at`] has always been bounded -- it
    /// answers `?` for code no subprogram covers -- so the two lookups disagreed about the same
    /// address, and a frame could carry a line with no name.
    ///
    /// The method table is the bound: an offset inside no method has no row, which is
    /// [`program::name_containing`]'s own test. A program whose table is empty keeps the unbounded
    /// search, because then nothing here knows where its code ends.
    fn row_at(&self, offset: u32) -> Option<LineRow> {
        if !self.names.is_empty() && program::name_containing(&self.names, offset).is_none() {
            return None;
        }
        self.lines.iter().rev().find(|row| row.offset <= offset).copied()
    }

    /// The name of the function whose code CONTAINS `offset`, or `?` when none does.
    ///
    /// **NOT THE NEAREST PRECEDING NAME.** Most of an image has no subprogram entry -- a C floor
    /// built without debug information, a compiler-generated thunk, an assembly stub -- and the
    /// nearest-preceding rule labels every one of those with the last function that happened to
    /// end before it. Measured on a Swift image: 15 names over 1,524 bytes of code, so a stop in
    /// the SERCOM driver reported itself in `appMain`.
    fn method_name_at(&self, offset: u32) -> String {
        program::name_containing(&self.names, offset)
            .map_or_else(|| String::from("?"), |(_, _, name)| name.clone())
    }

    /// Whether `address` is preceded by a call, which is what makes it a return address.
    ///
    /// **THE ONLY CHECK THAT SEPARATES A LIVE LINK REGISTER FROM A LEFTOVER ONE.** Where the frame
    /// table gives no rule for the return address, the convention is that it is still in the link
    /// register -- and that is true of a leaf, false of a function that has since called out
    /// without saving it, and indistinguishable from the table. The value is a plausible code
    /// address either way. Reading the instruction before it is what decides: Thumb reaches a
    /// function by `BL` (32-bit, the halfword pair at -4) or by `BLX` on a register (16-bit, at
    /// -2), and a return address always sits immediately after one of the two.
    ///
    /// A read that fails answers false: an address whose memory the probe cannot reach is not one
    /// to build a frame on.
    fn follows_a_call(&self, address: u32) -> bool {
        let halfword = |back: u32| address.checked_sub(back).and_then(|at| self.halfword(at));
        let (Some(first), Some(second)) = (halfword(4), halfword(2)) else {
            return false;
        };
        let bl = (first & 0xF800) == 0xF000 && (second & 0xD000) == 0xD000;
        let blx = (second & 0xFF80) == 0x4780;
        bl || blx
    }

    /// Services a halt: if the core stopped at a semihosting `BKPT 0xAB`, captures a
    /// `SYS_WRITE0` string into the output buffer, steps past it, resumes, and reports
    /// `Some(true)` (keep running); a non-semihosting halt is `Some(false)` (a real
    /// stop); a probe error is `None`.
    fn service_semihosting(&mut self) -> Option<bool> {
        let string_bytes = {
            let probe = self.probe.get_mut();
            let pc = probe.read_core_reg(15).ok()?;
            let word = probe.read_word(pc & !3).ok()?;
            let halfword = if pc & 2 != 0 {
                (word >> 16) as u16
            } else {
                word as u16
            };
            if halfword != 0xBEAB {
                return Some(false);
            }
            let bytes = if probe.read_core_reg(0).ok()? == 0x04 {
                let mut addr = probe.read_core_reg(1).ok()?;
                let mut collected = Vec::new();
                while collected.len() < 4096 {
                    let w = probe.read_word(addr & !3).ok()?;
                    let byte = (w >> ((addr & 3) * 8)) as u8;
                    if byte == 0 {
                        break;
                    }
                    collected.push(byte);
                    addr = addr.wrapping_add(1);
                }
                Some(collected)
            } else {
                None
            };
            probe.write_core_reg(15, pc.wrapping_add(2)).ok()?;
            probe.resume().ok()?;
            bytes
        };
        if let Some(bytes) = string_bytes {
            self.output.push_str(&String::from_utf8_lossy(&bytes));
        }
        Some(true)
    }
}

impl<A: TargetAccess> DeviceBackend<A> {
    /// Runs the target at full speed to `target` -- a return address -- arming it as a temporary
    /// breakpoint; reports `Step` on arrival or `Breakpoint` if a user breakpoint intervened. Used
    /// by step-over (run a callee to the live LR) and step-out (run the frame to its saved return).
    /// Comparators: keep all user breakpoints and add `target` if a slot is free or `target` is
    /// already one; else BORROW the comparator of the breakpoint on `target`'s call-site line (the
    /// instruction before it) -- that code cannot run before we reach `target`, so disarming it for
    /// the duration misses nothing; else single-step with a bound (never hanging), stopping on any
    /// user breakpoint (never missing).
    fn run_to_address(&mut self, target: u32) -> Stop {
        let free = self
            .comparators
            .is_some_and(|count| self.breakpoints.len() < count);
        let armed: Option<Vec<u32>> = if free || self.breakpoints.contains(&target) {
            let mut a = self.breakpoints.clone();
            if !a.contains(&target) {
                a.push(target);
            }
            Some(a)
        } else {
            let call_line = self.source_line_at(self.image_offset(target.saturating_sub(4)));
            let borrow = (call_line != 0)
                .then(|| {
                    self.breakpoints
                        .iter()
                        .position(|&bp| self.source_line_at(self.image_offset(bp)) == call_line)
                })
                .flatten();
            borrow.map(|index| {
                let mut a = self.breakpoints.clone();
                a[index] = target;
                a
            })
        };

        if let Some(armed) = armed {
            let probe = self.probe.get_mut();
            if probe.set_breakpoints(&armed).is_err() {
                return Stop::Fault("arm return breakpoint".into());
            }
            if probe.resume().is_err() {
                return Stop::Fault("resume into call".into());
            }
            let mut halted = false;
            for _ in 0..1_000_000u32 {
                match probe.is_halted() {
                    Ok(true) => {
                        halted = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(_) => return Stop::Fault("poll halt".into()),
                }
            }
            let _ = probe.set_breakpoints(&self.breakpoints);
            if !halted {
                let _ = probe.halt();
                return Stop::Fault("call did not return".into());
            }
            let pc = probe.read_core_reg(15).unwrap_or(0) & !1;
            return if self.breakpoints.contains(&pc) {
                Stop::Breakpoint
            } else {
                Stop::Step
            };
        }

        const FALLBACK_STEP_LIMIT: u32 = 2048;
        for _ in 0..FALLBACK_STEP_LIMIT {
            if self.probe.get_mut().step().is_err() {
                return Stop::Fault("step in call".into());
            }
            let pc = self.probe.get_mut().read_core_reg(15).unwrap_or(0) & !1;
            if pc == target {
                return Stop::Step;
            }
            if self.breakpoints.contains(&pc) {
                return Stop::Breakpoint;
            }
        }
        self.output.push_str(
            "[lamella] Step stopped inside a long-running call: all hardware breakpoints are in \
             use, so it could not be run at full speed. Free a breakpoint, or set one past the \
             call and Continue.\n",
        );
        Stop::Breakpoint
    }

    /// The current frame's return address, recovered by reading the saved LR off the stack -- so
    /// step-out works from a NON-LEAF frame, where the live LR is the frame's own internal return
    /// (set by a call it already made), not its caller's. The AOT prologue for a non-leaf method is
    /// `push {<callee-saved>, lr}` then an optional `sub sp, #frame`, so the saved LR is the topmost
    /// pushed word, at `sp + frame + 4*saved_count` (SP sits `frame` below the push through the
    /// body). Decodes those two Thumb instructions at the method's entry. Returns `None` if the
    /// prologue is not that shape (e.g. a leaf that never saved LR), so the caller can fall back.
    fn frame_return_address(&mut self, pc: u32) -> Option<u32> {
        let off = self.image_offset(pc);
        let method_off = program::name_containing(&self.names, off).map(|&(start, _, _)| start)?;
        let method_start = self.base + method_off;
        let probe = self.probe.get_mut();
        let w0 = probe.read_word(method_start & !3).ok()?;
        let push = if method_start & 2 != 0 {
            (w0 >> 16) as u16
        } else {
            w0 as u16
        };
        if push & 0xFE00 != 0xB400 || push & 0x0100 == 0 {
            return None;
        }
        let saved_count = u32::from(push & 0x00FF).count_ones();
        let after = method_start + 2;
        let w1 = probe.read_word(after & !3).ok()?;
        let next = if after & 2 != 0 {
            (w1 >> 16) as u16
        } else {
            w1 as u16
        };
        let frame = if next & 0xFF80 == 0xB080 {
            u32::from(next & 0x7F) * 4
        } else {
            0
        };
        let sp = probe.read_core_reg(13).ok()?;
        let saved_lr = probe.read_word((sp + frame + 4 * saved_count) & !3).ok()?;
        Some(saved_lr & !1)
    }
}

/// The Debug Halting Control and Status Register: Armv7-M ARM (DDI 0403E.d) C1.6.2, and Armv8-M ARM
/// (DDI 0553B.y) D1.2.39.
const DHCSR: u32 = 0xE000_EDF0;
/// `DHCSR.DBGKEY`, bits 31:16: a write to the register's lower half takes effect only with this key.
const DBGKEY: u32 = 0xA05F_0000;
/// `DHCSR.S_LOCKUP`, bit 19 on every profile this project targets -- Armv6-M ARM (DDI 0419E)
/// Table C1-13, Armv7-M ARM (DDI 0403E.d) B1.5.15, Armv8-M ARM (DDI 0553B.y) D1.2.36: the processor
/// is locked up because of an unrecoverable exception.
///
/// **IT READS 1 WHILE THE PROCESSOR IS RUNNING, WHICH IS THE WHOLE REASON THIS CONSTANT EXISTS.**
/// Armv6-M ARM Table C1-13: the bit "can only read as 1 when accessed by a remote debugger using the
/// DAP. The value of 1 indicates that the processor is running but locked up." So `S_HALT` is clear
/// and a poll that asks only "is it halted?" answers "still running" for the rest of the session.
const S_LOCKUP: u32 = 1 << 19;

/// The breakpoint unit's control register, at one address on every Cortex-M: `BP_CTRL` in Armv6-M ARM
/// (DDI 0419E) C1.8.2, and `FP_CTRL` in Armv7-M ARM (DDI 0403E.d) C1.11.3 and Armv8-M ARM (DDI
/// 0553B.y) D1.2.109. Its NUM_CODE field counts the unit's instruction address comparators.
const FP_CTRL: u32 = 0xE000_2000;

impl<A: TargetAccess> DebugBackend for DeviceBackend<A> {
    fn launch(&mut self) -> Result<(), String> {
        let probe = self.probe.get_mut();
        probe
            .connect()
            .map_err(|error| format!("could not connect to the target: {error}"))?;
        probe
            .read_idcode()
            .map_err(|error| format!("could not read the debug port's IDCODE: {error}"))?;
        probe
            .init_mem()
            .map_err(|error| format!("could not reach the target's memory: {error}"))?;
        probe
            .halt()
            .map_err(|error| format!("could not halt the core: {error}"))?;
        self.comparators = probe
            .read_word(FP_CTRL)
            .ok()
            .map(|word| lamella_probe_core::cortex_m::fpb_num_code(word) as usize);
        Ok(())
    }

    fn resume(&mut self) -> Stop {
        let bps = self.breakpoints.clone();
        let probe = self.probe.get_mut();
        let _ = probe.set_breakpoints(&bps);
        if let Ok(pc) = probe.read_core_reg(15) {
            if bps.contains(&(pc & !1)) {
                let _ = probe.step();
            }
        }
        match probe.resume() {
            Ok(()) => Stop::Running,
            Err(_) => Stop::Fault("resume failed".into()),
        }
    }

    fn pause(&mut self) -> bool {
        self.probe.get_mut().halt().is_ok()
    }

    /// Removes every breakpoint, lets a halted core run on, and turns halting debug off, so the part
    /// behaves as it would with no debugger attached: a debug event that would have halted it -- a
    /// breakpoint instruction, a vector catch -- does what it does on a board nobody is debugging.
    fn release(&mut self) -> Result<(), String> {
        self.breakpoints.clear();
        let probe = self.probe.get_mut();
        probe
            .set_breakpoints(&[])
            .map_err(|error| format!("could not remove the breakpoints: {error}"))?;
        let halted = probe
            .is_halted()
            .map_err(|error| format!("could not read whether the core is halted: {error}"))?;
        if halted {
            match self.service_semihosting() {
                Some(true) => {}
                Some(false) => self
                    .probe
                    .get_mut()
                    .resume()
                    .map_err(|error| format!("could not resume the core: {error}"))?,
                None => {
                    return Err("could not read the instruction the core is stopped at".to_owned());
                }
            }
        }
        let probe = self.probe.get_mut();
        probe
            .write_word(DHCSR, DBGKEY)
            .map_err(|error| format!("could not turn halting debug off: {error}"))?;
        match probe.is_halted() {
            Ok(false) => Ok(()),
            Ok(true) => {
                Err("the core stopped again before halting debug was turned off".to_owned())
            }
            Err(error) => Err(format!(
                "could not read whether the core is halted: {error}"
            )),
        }
    }

    fn run_to_return(&mut self) -> Stop {
        let lr = match self.probe.get_mut().read_core_reg(14) {
            Ok(lr) => lr & !1,
            Err(_) => return Stop::Fault("read LR".into()),
        };
        self.run_to_address(lr)
    }

    fn step_out(&mut self) -> Option<Stop> {
        let (pc, lr) = {
            let probe = self.probe.get_mut();
            (
                probe.read_core_reg(15).unwrap_or(0) & !1,
                probe.read_core_reg(14).unwrap_or(0) & !1,
            )
        };
        let here = self.method_name_at(self.image_offset(pc));
        if here == self.entry {
            return Some(self.resume());
        }
        if self.method_name_at(self.image_offset(lr)) == here {
            return Some(match self.frame_return_address(pc) {
                Some(ret) => self.run_to_address(ret),
                None => Stop::Step,
            });
        }
        Some(self.run_to_return())
    }

    fn poll(&mut self) -> Stop {
        match self.probe.get_mut().is_halted() {
            Ok(false) if self.locked_up() => Stop::Fault(String::from(
                "the target is locked up: an unrecoverable exception was taken and the core is \
                 fetching from the lockup address instead of running the program. Only a reset or \
                 an NMI leaves that state, so the session cannot continue. DHCSR.S_LOCKUP reads 1",
            )),
            Ok(false) => Stop::Running,
            Ok(true) => match self.service_semihosting() {
                Some(true) => Stop::Running,
                Some(false) => Stop::Breakpoint,
                None => Stop::Fault("semihosting service failed".into()),
            },
            Err(_) => Stop::Fault("could not read halt status".into()),
        }
    }

    fn step(&mut self) -> Stop {
        match self.probe.get_mut().step() {
            Ok(()) => Stop::Step,
            Err(_) => Stop::Fault("step failed".into()),
        }
    }

    fn depth(&self) -> usize {
        self.probe
            .borrow_mut()
            .read_core_reg(13)
            .map_or(0, |sp| sp.wrapping_neg() as usize)
    }

    fn set_breakpoints(&mut self, addresses: &[u64]) -> Result<(), String> {
        let fitting: Vec<u32> = addresses
            .iter()
            .filter_map(|&address| u32::try_from(address).ok())
            .collect();
        let dropped = addresses.len() - fitting.len();

        let words: Vec<u32> = fitting
            .into_iter()
            .take(self.comparators.unwrap_or(usize::MAX))
            .collect();
        self.breakpoints = words.clone();

        self.probe
            .get_mut()
            .set_breakpoints(&words)
            .map_err(|error| format!("could not arm breakpoints on the target: {error}"))?;

        if dropped > 0 {
            return Err(format!(
                "{dropped} of {} breakpoints are outside this target's 32-bit address space and were not armed",
                addresses.len()
            ));
        }
        Ok(())
    }

    fn max_breakpoints(&self) -> Option<usize> {
        self.comparators
    }

    fn stack(&self) -> Vec<Frame> {
        self.walk_frames().into_iter().map(|(frame, _)| frame).collect()
    }

    fn resolve_source_breakpoint(&self, document: &str, line: u32) -> Option<u64> {
        let wanted = self.files.iter().position(|file| paths_match(file, document));
        self.lines
            .iter()
            .find(|row| row.line == line && wanted.is_none_or(|index| row.file as usize == index))
            .map(|row| u64::from(self.base + row.offset))
    }

    fn source_location(&self, address: u64) -> Option<SourceLocation> {
        let row = self.row_at(self.image_offset(address as u32))?;
        if row.line == 0 {
            return None;
        }
        let file = self.files.get(row.file as usize)?;
        if file.is_empty() {
            return None;
        }
        Some(SourceLocation {
            file: file.clone(),
            line: row.line,
            column: 1,
            end_line: row.line,
            end_column: 1,
        })
    }

    fn has_source(&self) -> bool {
        !self.lines.is_empty()
    }

    fn at_source_boundary(&self) -> bool {
        let pc = self.probe.borrow_mut().read_core_reg(15).unwrap_or(0);
        let offset = self.image_offset(pc);
        match self.lines.iter().position(|row| row.offset == offset) {
            Some(0) => true,
            Some(i) => {
                self.lines[i].line != self.lines[i - 1].line
                    || self.lines[i].file != self.lines[i - 1].file
            }
            None => false,
        }
    }

    fn variables(&self, frame: usize, scope: Scope) -> Vec<Variable> {
        if matches!(scope, Scope::Stack) || self.locals.is_empty() {
            return Vec::new();
        }
        let frames = self.stack();
        let Some(entry) = frames.get(frame) else {
            return Vec::new();
        };
        let pc = entry.address;
        let Some(frame_pointer) = self.stack_pointer_of_frame(frame) else {
            return Vec::new();
        };

        let sections = self.locals.view();
        let Ok(locals) = lamella_dwarf::Locals::parse_within(&sections, &self.locals.code) else {
            return Vec::new();
        };
        let lookup = if frame == 0 { pc } else { pc.saturating_sub(1) };
        let Some(program) = locals.at(lookup) else {
            return Vec::new();
        };

        let wanted_parameters = matches!(scope, Scope::Arguments);
        program
            .locals
            .iter()
            .filter(|local| local.is_parameter == wanted_parameters)
            .map(|local| {
                let name = local
                    .name
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    .unwrap_or_else(|| "<unnamed>".to_string());
                let (value, kind) =
                    self.render(local, lookup, frame_pointer, &program.frame_base, frame);
                Variable { name, value, kind }
            })
            .collect()
    }

    fn read_memory(&self, address: u64, len: usize) -> Vec<u8> {
        let mut probe = self.probe.borrow_mut();
        let mut out = Vec::with_capacity(len);
        let mut addr = address as u32;
        while out.len() < len {
            match probe.read_word(addr) {
                Ok(word) => out.extend_from_slice(&word.to_le_bytes()),
                Err(_) => break,
            }
            addr = addr.wrapping_add(4);
        }
        out.truncate(len);
        out
    }

    fn read_registers(&self) -> Vec<Register> {
        const NAMES: [&str; 17] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
            "lr", "pc", "xpsr",
        ];
        let mut probe = self.probe.borrow_mut();
        NAMES
            .iter()
            .enumerate()
            .filter_map(|(sel, name)| {
                probe.read_core_reg(sel as u8).ok().map(|value| Register {
                    name: (*name).into(),
                    value: u64::from(value),
                })
            })
            .collect()
    }

    fn disassemble(&self, address: u64, offset: i64, count: usize) -> Vec<Disassembled> {
        let mut probe = self.probe.borrow_mut();
        let start = (address as i64).wrapping_add(offset * 2) as u32;
        (0..count)
            .map(|i| {
                let addr = start.wrapping_add((i as u32) * 2);
                let text = match probe.read_word(addr & !3) {
                    Ok(word) => {
                        let half = if addr & 2 != 0 {
                            word >> 16
                        } else {
                            word & 0xffff
                        };
                        format!("{half:04x}")
                    }
                    Err(_) => "????".into(),
                };
                Disassembled {
                    address: u64::from(addr),
                    text,
                }
            })
            .collect()
    }

    fn take_output(&mut self) -> Option<String> {
        if self.output.is_empty() {
            None
        } else {
            Some(core::mem::take(&mut self.output))
        }
    }
}
