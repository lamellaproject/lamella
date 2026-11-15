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
    /// The start of a UTF-8 character the target has only partly written.
    ///
    /// **A CONSOLE WRITES ONE BYTE PER CALL**, so a character outside ASCII arrives over two to four
    /// calls. Decoding each byte as it arrived would print every such character as replacement
    /// marks; these bytes wait here until the character is whole.
    partial_character: Vec<u8>,
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
    /// The program's console ring, followed while it runs: what the program prints joins `output`,
    /// and the end the ring reports ends the session. `None` until [`Self::with_console`] is called,
    /// and for a program that keeps no ring.
    console: Option<lamella_probe_console::RingReader>,
    /// Lines for the debug console about the program's console -- bytes it lost before they could be
    /// read, a restart, a ring that could no longer be read -- drained by `take_debug_output`.
    notes: String,
    /// An end the console ring reported between polls, which the next poll reports.
    ended: Option<lamella_probe_console::ProgramEnd>,
    /// The code the program returned, as its console ring reported it; 0 until then.
    exit_code: i32,
    /// The code the board gave for a program it aborted, as its console ring reported it; `None`
    /// until then.
    aborted_code: Option<i32>,
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
            partial_character: Vec::new(),
            breakpoints: Vec::new(),
            comparators: None,
            entry,
            frame_section,
            locals: crate::program::LocalSections::default(),
            unwind: crate::program::UnwindTables::default(),
            console: None,
            notes: String::new(),
            ended: None,
            exit_code: 0,
            aborted_code: None,
        }
    }

    /// Every frame, each with THE STACK POINTER THAT FRAME HAD, and its registers as far as the walk
    /// recovered them.
    ///
    /// The second value is what a local's frame offset resolves against, and it is only the core
    /// register for the innermost frame -- above that it is the callee's canonical frame address,
    /// which is the caller's stack pointer by definition. Computing it here rather than twice is
    /// what stops a variables pane and a call stack disagreeing about where a frame is.
    ///
    /// The registers are the core's for the innermost frame, read where the walk needs them, and
    /// above it the ones the frames below restored or left alone: `None` for any register whose
    /// value in that frame is not known.
    fn walk_frames(&self) -> Vec<(Frame, u32, lamella_elf::ehabi::Registers)> {
        self.walk(32)
    }

    /// [`Self::walk_frames`], stopped after `limit` frames: a caller that needs only the innermost
    /// frame's caller reads no more of the stack than that.
    fn walk(&self, limit: u32) -> Vec<(Frame, u32, lamella_elf::ehabi::Registers)> {
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
        for depth in 0..limit {
            let lookup = if depth == 0 { pc } else { pc.saturating_sub(1) };
            let offset = self.image_offset(lookup);
            out.push((
                Frame {
                    address: u64::from(pc),
                    name: self.method_name_at(offset),
                    line: self.source_line_at(offset),
                },
                sp,
                registers,
            ));
            if depth + 1 >= limit {
                break;
            }

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

    /// The value at `address`: the bytes of `scalar`, a type this reads, decoded as .NET's debugger
    /// shows them ([`show_scalar`]), and otherwise four bytes in hexadecimal, as every value was
    /// before the type graph was read.
    fn value_at(&self, address: u64, scalar: Option<lamella_dwarf::ScalarType<'_>>) -> String {
        let width = scalar
            .and_then(|scalar| usize::try_from(scalar.byte_size).ok())
            .filter(|width| (1..=8).contains(width));
        let bytes = self.read_memory(address, width.unwrap_or(4));
        scalar
            .and_then(|scalar| show_scalar(&bytes, &scalar))
            .unwrap_or_else(|| hex_word(&bytes))
    }

    /// Renders one local as a value and a type, reading the target where the place says to.
    ///
    /// # THE TYPE COLUMN SAYS WHERE THE VALUE CAME FROM, NOT WHAT THE VARIABLE IS DECLARED AS
    ///
    /// A value is decoded where the type graph names its declared type a scalar -- a base type, or
    /// an enumeration as its underlying type: `scalar`, from [`lamella_dwarf::scalar_type`] -- and
    /// shown as four raw bytes otherwise: a pointer, an object, a structure. Either way the column
    /// says which place the bytes were read from, so a person can check a value against the
    /// register view. A guess at an undeclared type would be the larger lie: "42" for a variable
    /// that is actually a `Float` is indistinguishable from the truth. A value the debug information
    /// gives outright, with no place, is decoded the same way, and the column says `constant`:
    /// nothing was read from the target.
    ///
    /// # A REGISTER IS READ AS THE FRAME HAD IT
    ///
    /// In the frame the core is stopped in, that is the core's own register; above it, it is the
    /// value the walk recovered for the frame, `registers`. A register no frame below restored holds
    /// what the innermost frame left in it, and a local read from it would be a plausible number from
    /// the wrong frame, so it is shown as not recovered.
    fn render(
        &self,
        local: &lamella_dwarf::Local<'_>,
        address: u64,
        frame_pointer: u32,
        frame_base: &lamella_dwarf::FrameBase<'_>,
        depth: usize,
        registers: &lamella_elf::ehabi::Registers,
        scalar: Option<lamella_dwarf::ScalarType<'_>>,
    ) -> (String, String) {
        let register_of_frame = |number: u8| -> Result<u32, &'static str> {
            if depth == 0 {
                self.probe.borrow_mut().read_core_reg(number).map_err(|_| "<unreadable>")
            } else {
                registers
                    .get(usize::from(number))
                    .copied()
                    .flatten()
                    .ok_or("<not recovered for an outer frame>")
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
                    let Ok(number) = u8::try_from(*register) else {
                        return ("<register out of range>".to_string(), "unsupported".to_string());
                    };
                    match register_of_frame(number) {
                        Ok(value) => (value, format!("r{register}")),
                        Err(_) if depth != 0 => {
                            return (
                                "<frame-pointer base, not recovered for an outer frame>".to_string(),
                                "unsupported".to_string(),
                            );
                        }
                        Err(why) => return (why.to_string(), "unsupported".to_string()),
                    }
                };
                let Some(at) = base.checked_add_signed(offset) else {
                    return ("<offset off the frame>".to_string(), "unsupported".to_string());
                };
                (self.value_at(u64::from(at), scalar), format!("[{spelling}{offset:+}]"))
            }
            lamella_dwarf::Place::Register(register) => {
                let Ok(number) = u8::try_from(register) else {
                    return ("<register out of range>".to_string(), "unsupported".to_string());
                };
                match register_of_frame(number) {
                    Ok(value) => (in_register(value, scalar), format!("r{register}")),
                    Err(why) => (why.to_string(), format!("r{register}")),
                }
            }
            lamella_dwarf::Place::RegisterOffset { register, offset } => {
                let (Ok(number), Ok(offset)) = (u8::try_from(register), i32::try_from(offset))
                else {
                    return ("<register out of range>".to_string(), "unsupported".to_string());
                };
                let base = match register_of_frame(number) {
                    Ok(base) => base,
                    Err(why) => return (why.to_string(), format!("[r{register}{offset:+}]")),
                };
                let Some(at) = base.checked_add_signed(offset) else {
                    return ("<offset off the register>".to_string(), "unsupported".to_string());
                };
                (self.value_at(u64::from(at), scalar), format!("[r{register}{offset:+}]"))
            }
            lamella_dwarf::Place::Address(at) => {
                (self.value_at(at, scalar), format!("[0x{at:08x}]"))
            }
            lamella_dwarf::Place::Constant(value) => {
                (constant(value, scalar), "constant".to_string())
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

    /// Follows the program's console ring at `ring`, the block of RAM its image names
    /// ([`crate::program::DebugProgram::console`]), as `lamella run` follows it: what the program
    /// prints joins the session's output while it runs, and the end the ring reports ends the
    /// session, with the program's code.
    ///
    /// **A RING NOT SET UP YET IS NO OUTPUT YET, NEVER AN ERROR.** A session holds a program before
    /// its startup sets the ring up for as long as its user likes, so the ring is waited for without
    /// end. A ring a run before this one left in RAM is the writer's to clear: see
    /// [`clear_console_ring`].
    #[must_use]
    pub fn with_console(mut self, ring: crate::program::DataObject) -> Self {
        self.console = Some(lamella_probe_console::RingReader::new(
            ring,
            lamella_probe_console::END_MAGICS,
            u32::MAX,
        ));
        self
    }

    /// The stop that ends the session where the debugger could not do `what`, a phrase such as "step
    /// the program": in the probe's own words where it gave up on its link, so a session whose probe
    /// stopped answering says that once, rather than naming whichever call failed next.
    fn fault(&mut self, what: &str) -> Stop {
        self.fault_in_words(format!("The debugger could not {what}, so the session ends here. Start it again."))
    }

    /// The stop that ends the session with `words`, or in the probe's own where it gave up on its link.
    fn fault_in_words(&mut self, words: String) -> Stop {
        Stop::Fault(self.probe.get_mut().stopped_answering().unwrap_or(words))
    }

    /// `stop`, unless the probe gave up on its link while it was reached: then the stop that ends
    /// the session in its words ([`fault`](Self::fault)).
    fn unless_the_probe_stopped(&mut self, stop: Stop) -> Stop {
        match self.probe.get_mut().stopped_answering() {
            Some(words) => Stop::Fault(words),
            None => stop,
        }
    }

    /// Looks at the program's console ring, if it keeps one: what it printed since the last look
    /// joins the session's output, and the end it reports is returned.
    ///
    /// What the ring lost and a restart are noted for the debug console in the words `lamella run`
    /// uses. A ring that cannot be read is noted once and then let go: the next look would fail the
    /// same way, and a note at every look would bury the console. A probe that stopped answering is
    /// not noted here: the session ends saying so ([`fault`](Self::fault)), once.
    fn follow_console(&mut self) -> Option<lamella_probe_console::ProgramEnd> {
        let reader = self.console.as_mut()?;
        let seen = match reader.poll(&mut WordReads(self.probe.get_mut())) {
            Ok(seen) => seen,
            Err(why) => {
                if self.probe.get_mut().stopped_answering().is_none() {
                    self.notes.push_str(&format!("Lost the program's console: {why}\n"));
                }
                self.console = None;
                return None;
            }
        };
        let mut end = None;
        for one in seen {
            match one {
                lamella_probe_console::Seen::Out(bytes) => self.append_console_bytes(&bytes),
                lamella_probe_console::Seen::Lost(count) => self.notes.push_str(&format!(
                    "{count} bytes of the board's console were overwritten on the board before they could \
                     be read.\n"
                )),
                lamella_probe_console::Seen::Restarted => self
                    .notes
                    .push_str("The board started again; following the program from its new start.\n"),
                lamella_probe_console::Seen::Ended(how) => end = Some(how),
            }
        }
        end
    }

    /// The stop a program's end is: `Done` with the code it returned, or, for a program the board
    /// stopped, a fault that names the code the board gives and says what it means, from the end-code
    /// table ([`lamella_exit_codes::meaning`]). A code the table does not define is named alone.
    fn ended_with(&mut self, end: lamella_probe_console::ProgramEnd) -> Stop {
        match end {
            lamella_probe_console::ProgramEnd::Returned(code) => {
                self.exit_code = code;
                Stop::Done
            }
            lamella_probe_console::ProgramEnd::Aborted(code) => {
                self.aborted_code = Some(code);
                Stop::Fault(match lamella_exit_codes::meaning(code) {
                    Some(meaning) => format!("The program ended with code {code}: {meaning}."),
                    None => format!("The program ended with code {code}."),
                })
            }
        }
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

    /// Services a halt while the target runs free: when the core stopped at a semihosting call,
    /// performs it and resumes, and goes on doing so while the core keeps stopping at one.
    ///
    /// `Some(true)` means the target is running again, `Some(false)` that it is halted for some
    /// other reason -- a breakpoint -- and `None` a probe error.
    ///
    /// **A BURST, BECAUSE A CONSOLE LINE IS ONE CALL PER BYTE.** Handing each call back to the
    /// adapter's poll would cost a poll interval per character. The target reaches its next call
    /// far sooner than a probe round trip, so the next halt is usually already there to answer.
    /// The burst is bounded so a program that prints without end still lets the session act.
    fn service_semihosting(&mut self) -> Option<bool> {
        for _ in 0..SEMIHOSTING_BURST {
            if !self.perform_semihosting_call()? {
                return Some(false);
            }
            self.probe.get_mut().resume().ok()?;
            if !self.probe.get_mut().is_halted().ok()? {
                return Some(true);
            }
        }
        Some(true)
    }

    /// Performs the semihosting call the core is halted at, if it is halted at one: a `BKPT 0xAB`,
    /// with the operation in `r0` and its argument in `r1`. What the call writes joins the
    /// session's output, and the PC moves past the two-byte `BKPT`, which is what executing it
    /// means. **The core is not resumed**, so a path that is stepping can go on stepping.
    ///
    /// `Some(true)` when a call was performed, `Some(false)` when the core is halted at anything
    /// else, `None` on a probe error.
    ///
    /// `SYS_WRITEC` writes the one byte `r1` points at, and is what the runtime's console issues;
    /// `SYS_WRITE0` writes the NUL-terminated string `r1` points at. Any other operation is stepped
    /// past with nothing written.
    fn perform_semihosting_call(&mut self) -> Option<bool> {
        let written = {
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
            let read_byte = |probe: &mut A, address: u32| -> Option<u8> {
                let word = probe.read_word(address & !3).ok()?;
                Some((word >> ((address & 3) * 8)) as u8)
            };
            let argument = probe.read_core_reg(1).ok()?;
            let written = match probe.read_core_reg(0).ok()? {
                SYS_WRITEC => vec![read_byte(probe, argument)?],
                SYS_WRITE0 => {
                    let mut collected = Vec::new();
                    let mut address = argument;
                    while collected.len() < 4096 {
                        let byte = read_byte(probe, address)?;
                        if byte == 0 {
                            break;
                        }
                        collected.push(byte);
                        address = address.wrapping_add(1);
                    }
                    collected
                }
                _ => Vec::new(),
            };
            probe.write_core_reg(15, pc.wrapping_add(2)).ok()?;
            written
        };
        self.append_console_bytes(&written);
        Some(true)
    }

    /// Adds bytes the target wrote to the session's output, holding back the start of a character
    /// that has not finished arriving.
    fn append_console_bytes(&mut self, bytes: &[u8]) {
        self.partial_character.extend_from_slice(bytes);
        let pending = core::mem::take(&mut self.partial_character);
        let mut rest: &[u8] = &pending;
        loop {
            match core::str::from_utf8(rest) {
                Ok(text) => {
                    self.output.push_str(text);
                    return;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    self.output
                        .push_str(core::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            self.output.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[length..];
                        }
                        None => {
                            self.partial_character = after.to_vec();
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Semihosting `SYS_WRITEC`: write the one byte `r1` points at.
const SYS_WRITEC: u32 = 0x03;
/// Semihosting `SYS_WRITE0`: write the NUL-terminated string `r1` points at.
const SYS_WRITE0: u32 = 0x04;
/// How many semihosting calls one poll answers before handing control back to the adapter.
const SEMIHOSTING_BURST: usize = 256;

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
            if self.probe.get_mut().set_breakpoints(&armed).is_err() {
                return self.fault("set the breakpoint that ends the step");
            }
            if self.probe.get_mut().resume().is_err() {
                return self.fault("run the call being stepped over");
            }
            let mut halted = false;
            for _ in 0..1_000_000u32 {
                match self.probe.get_mut().is_halted() {
                    Ok(true) => match self.perform_semihosting_call() {
                        Some(true) => {
                            if self.probe.get_mut().resume().is_err() {
                                return self.fault("let the program run on after it printed");
                            }
                        }
                        Some(false) => {
                            halted = true;
                            break;
                        }
                        None => return self.fault("read the instruction the program stopped at"),
                    },
                    Ok(false) => {}
                    Err(_) => return self.fault("tell whether the program had stopped"),
                }
            }
            let probe = self.probe.get_mut();
            let _ = probe.set_breakpoints(&self.breakpoints);
            if !halted {
                let _ = probe.halt();
                return self.fault_in_words(String::from(
                    "The call being stepped over did not return, so the program was stopped inside it and \n                     the session ends here.",
                ));
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
            match self.perform_semihosting_call() {
                Some(true) => {}
                Some(false) => {
                    if self.probe.get_mut().step().is_err() {
                        return self.fault("step through the call");
                    }
                }
                None => return self.fault("read the instruction the program stopped at"),
            }
            let pc = self.probe.get_mut().read_core_reg(15).unwrap_or(0) & !1;
            if pc == target {
                return Stop::Step;
            }
            if self.breakpoints.contains(&pc) {
                return Stop::Breakpoint;
            }
        }
        self.notes.push_str(
            "Step stopped inside a long-running call: every hardware breakpoint is in use, so the call \
             could not run at full speed. Remove a breakpoint, or set one past the call and Continue.\n",
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
            .map_err(|error| format!("could not reach the board's debug port: {error}"))?;
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

    fn attach(&mut self) -> Result<(), String> {
        self.launch()
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
            Err(_) => self.fault("let the program run"),
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
            .map_err(|error| format!("could not let the board run without the debugger: {error}"))?;
        match probe.is_halted() {
            Ok(false) => Ok(()),
            Ok(true) => {
                Err("the board stopped again as the debugger let it go".to_owned())
            }
            Err(error) => Err(format!(
                "could not read whether the core is halted: {error}"
            )),
        }
    }

    fn run_to_return(&mut self) -> Stop {
        let lr = match self.probe.get_mut().read_core_reg(14) {
            Ok(lr) => lr & !1,
            Err(_) => return self.fault("read where the method returns to"),
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
        if let Some(end) = self.ended.take() {
            return self.ended_with(end);
        }
        match self.probe.get_mut().is_halted() {
            Ok(false) if self.locked_up() => Stop::Fault(String::from(
                "The board's processor locked up: the program hit a fault it could not recover from, \
                 so it stopped running. Only a reset ends that, so the session ends here.",
            )),
            Ok(false) => match self.follow_console() {
                Some(end) => self.ended_with(end),
                None => self.unless_the_probe_stopped(Stop::Running),
            },
            Ok(true) => match self.service_semihosting() {
                Some(true) => Stop::Running,
                Some(false) => match self.follow_console() {
                    Some(end) => self.ended_with(end),
                    None => self.unless_the_probe_stopped(Stop::Breakpoint),
                },
                None => self.fault("read the instruction the program stopped at"),
            },
            Err(_) => self.fault("tell whether the program had stopped"),
        }
    }

    fn step(&mut self) -> Stop {
        match self.perform_semihosting_call() {
            Some(true) => return Stop::Step,
            Some(false) => {}
            None => return self.fault("read the instruction the program stopped at"),
        }
        match self.probe.get_mut().step() {
            Ok(()) => Stop::Step,
            Err(_) => self.fault("step the program"),
        }
    }

    fn depth(&self) -> usize {
        let frame_address = self
            .walk(2)
            .get(1)
            .map(|&(_, caller_sp, _)| caller_sp)
            .or_else(|| self.probe.borrow_mut().read_core_reg(13).ok());
        frame_address.map_or(0, |address| address.wrapping_neg() as usize)
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
        self.walk_frames().into_iter().map(|(frame, _, _)| frame).collect()
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

    fn frame_source_location(&self, index: usize, address: u64) -> Option<SourceLocation> {
        self.source_location(if index > 0 { address.saturating_sub(1) } else { address })
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
        let walked = self.walk_frames();
        let Some((entry, frame_pointer, registers)) = walked.get(frame) else {
            return Vec::new();
        };
        let (pc, frame_pointer) = (entry.address, *frame_pointer);

        let sections = self.locals.view();
        let Ok(locals) =
            lamella_dwarf::Locals::parse_within(&sections, &self.locals.code, &self.locals.options)
        else {
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
                let scalar = local
                    .type_offset
                    .and_then(|offset| lamella_dwarf::scalar_type(&sections, offset));
                let (value, kind) = self.render(
                    local,
                    lookup,
                    frame_pointer,
                    &program.frame_base,
                    frame,
                    registers,
                    scalar,
                );
                Variable { name, value, kind }
            })
            .collect()
    }

    fn read_memory(&self, address: u64, len: usize) -> Vec<u8> {
        let mut probe = self.probe.borrow_mut();
        let skip = (address & 3) as usize;
        let mut out = Vec::with_capacity(len + skip);
        let mut addr = (address as u32) & !3;
        while out.len() < len + skip {
            match probe.read_word(addr) {
                Ok(word) => out.extend_from_slice(&word.to_le_bytes()),
                Err(_) => break,
            }
            addr = addr.wrapping_add(4);
        }
        let mut out = out.split_off(skip.min(out.len()));
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
        if let Some(end) = self.follow_console() {
            self.ended = Some(end);
        }
        if self.output.is_empty() {
            None
        } else {
            Some(core::mem::take(&mut self.output))
        }
    }

    fn take_debug_output(&mut self) -> Option<String> {
        if self.notes.is_empty() {
            None
        } else {
            Some(core::mem::take(&mut self.notes))
        }
    }

    fn exit_code(&self) -> i32 {
        self.exit_code
    }

    fn fault_exit_code(&self) -> Option<i32> {
        self.aborted_code
    }
}

/// Clears the console ring a run before this one left at `ring`, on a part a write has just reset
/// and holds before the new program's first instruction: its magic word, which is all a reader goes
/// by.
///
/// **RAM SURVIVES THE RESET THAT ENDS A WRITE.** Until the new run's startup sets its ring up, the
/// old ring is still there -- with an end magic, where the old program ended -- and a look at it would
/// report the old run's output and the old run's end as the new one's. Startup clears the magic
/// itself, first, and writes it last, so clearing it early takes nothing from the program. A ring
/// not on a word boundary is left alone: its magic is not one word.
///
/// # Errors
/// The write the probe could not make.
pub fn clear_console_ring<A: TargetAccess>(
    target: &mut A,
    ring: crate::program::DataObject,
) -> Result<(), lamella_probe_core::ProbeError> {
    if ring.address % 4 != 0 {
        return Ok(());
    }
    target.write_word(ring.address, 0)
}

/// A part's memory read through its probe without halting it, in whole words, for the console
/// ring's reader: `lamella run` reads a ring the same way.
struct WordReads<'p, A>(&'p mut A);

impl<A: TargetAccess> lamella_probe_console::TargetMemory for WordReads<'_, A> {
    fn read(&mut self, address: u32, into: &mut [u8]) -> Result<(), String> {
        if into.is_empty() {
            return Ok(());
        }
        let past_the_end = || format!("{} bytes from {address:#010x} run past the end of memory", into.len());
        let first = address & !3;
        let skip = (address - first) as usize;
        let end = u64::try_from(into.len())
            .ok()
            .and_then(|length| u64::from(address).checked_add(length))
            .filter(|&end| end <= 1 << 32)
            .ok_or_else(past_the_end)?;
        let words = usize::try_from((end - u64::from(first)).div_ceil(4)).map_err(|_| past_the_end())?;
        let mut read = vec![0u32; words];
        self.0
            .read_words_into(first, &mut read)
            .map_err(|error| format!("reading the board's memory at {first:#010x}: {error}"))?;
        let bytes: Vec<u8> = read.iter().flat_map(|word| word.to_le_bytes()).collect();
        into.copy_from_slice(&bytes[skip..skip + into.len()]);
        Ok(())
    }
}

/// Four bytes as one word in hexadecimal, or `<unreadable>` where fewer came back.
fn hex_word(bytes: &[u8]) -> String {
    match bytes.get(..4) {
        Some(&[a, b, c, d]) => format!("0x{:08x}", u32::from_le_bytes([a, b, c, d])),
        _ => "<unreadable>".to_string(),
    }
}

/// A register's `value` as `scalar`, when it is a type of four bytes or fewer that this reads, and
/// in hexadecimal otherwise.
fn in_register(value: u32, scalar: Option<lamella_dwarf::ScalarType<'_>>) -> String {
    scalar
        .and_then(|scalar| {
            let width = usize::try_from(scalar.byte_size).ok().filter(|width| *width <= 4)?;
            show_scalar(value.to_le_bytes().get(..width)?, &scalar)
        })
        .unwrap_or_else(|| format!("0x{value:08x}"))
}

/// A `value` the debug information gives outright, as `scalar` reads in .NET's debugger, and in
/// hexadecimal where the type is not one this reads, as a value read from the target is. The number
/// is the object's contents, so a type of any width this reads takes its bytes from it.
fn constant(value: u64, scalar: Option<lamella_dwarf::ScalarType<'_>>) -> String {
    scalar
        .and_then(|scalar| show_scalar(&value.to_le_bytes(), &scalar))
        .unwrap_or_else(|| format!("0x{value:08x}"))
}

/// `bytes`, little-endian, as a value of `scalar` reads in .NET's debugger: a whole number in
/// decimal, a boolean as `true` or `false`, a character as its code and the character, and a
/// floating-point number in the shortest spelling that reads back as the same number. An
/// enumeration is its number, as its underlying type reads. `None` for an encoding or a size this
/// does not read, and for fewer bytes than the type has.
fn show_scalar(bytes: &[u8], scalar: &lamella_dwarf::ScalarType<'_>) -> Option<String> {
    use lamella_dwarf::types::{
        DW_ATE_BOOLEAN, DW_ATE_FLOAT, DW_ATE_SIGNED, DW_ATE_SIGNED_CHAR, DW_ATE_UNSIGNED,
        DW_ATE_UNSIGNED_CHAR, DW_ATE_UTF,
    };
    let width = usize::try_from(scalar.byte_size).ok().filter(|width| (1..=8).contains(width))?;
    let mut raw = [0u8; 8];
    raw[..width].copy_from_slice(bytes.get(..width)?);
    let unsigned = u64::from_le_bytes(raw);
    let shift = 64 - 8 * width as u32;
    let floating = |value: f64, text: String| {
        if value.is_nan() {
            "NaN".to_string()
        } else if value.is_infinite() {
            (if value > 0.0 { "Infinity" } else { "-Infinity" }).to_string()
        } else {
            text
        }
    };
    Some(match (scalar.encoding, width) {
        (DW_ATE_BOOLEAN, _) => (if unsigned == 0 { "false" } else { "true" }).to_string(),
        (DW_ATE_SIGNED | DW_ATE_SIGNED_CHAR, _) => (((unsigned << shift) as i64) >> shift).to_string(),
        (DW_ATE_UNSIGNED | DW_ATE_UNSIGNED_CHAR, _) => unsigned.to_string(),
        (DW_ATE_FLOAT, 4) => {
            let value = f32::from_bits(unsigned as u32);
            floating(f64::from(value), value.to_string())
        }
        (DW_ATE_FLOAT, 8) => {
            let value = f64::from_bits(unsigned);
            floating(value, value.to_string())
        }
        (DW_ATE_UTF, _) => match char::from_u32(unsigned as u32) {
            Some(character) if !character.is_control() => format!("{unsigned} '{character}'"),
            _ => unsigned.to_string(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod scalar_tests {
    use super::*;

    fn of(name: &'static str, encoding: u64, byte_size: u64) -> lamella_dwarf::ScalarType<'static> {
        lamella_dwarf::ScalarType { name: Some(name.as_bytes()), encoding, byte_size, enumeration: false }
    }

    /// Each scalar a C# program's locals have reads as .NET's debugger shows it.
    #[test]
    fn a_scalar_reads_as_dotnets_debugger_shows_it() {
        let int = of("int", 0x05, 4);
        assert_eq!(show_scalar(&6i32.to_le_bytes(), &int).as_deref(), Some("6"));
        assert_eq!(show_scalar(&(-13i32).to_le_bytes(), &int).as_deref(), Some("-13"));
        assert_eq!(show_scalar(&u32::MAX.to_le_bytes(), &of("uint", 0x07, 4)).as_deref(), Some("4294967295"));
        assert_eq!(show_scalar(&(-2i64).to_le_bytes(), &of("long", 0x05, 8)).as_deref(), Some("-2"));
        assert_eq!(show_scalar(&[0xff], &of("sbyte", 0x05, 1)).as_deref(), Some("-1"));
        assert_eq!(show_scalar(&[0xff], &of("byte", 0x08, 1)).as_deref(), Some("255"));
        assert_eq!(show_scalar(&[1], &of("bool", 0x02, 1)).as_deref(), Some("true"));
        assert_eq!(show_scalar(&[0], &of("bool", 0x02, 1)).as_deref(), Some("false"));
        assert_eq!(show_scalar(&65u16.to_le_bytes(), &of("char16_t", 0x10, 2)).as_deref(), Some("65 'A'"));
        assert_eq!(show_scalar(&1.5f32.to_le_bytes(), &of("float", 0x04, 4)).as_deref(), Some("1.5"));
        assert_eq!(show_scalar(&0.1f64.to_le_bytes(), &of("double", 0x04, 8)).as_deref(), Some("0.1"));
        assert_eq!(show_scalar(&f64::NAN.to_le_bytes(), &of("double", 0x04, 8)).as_deref(), Some("NaN"));
        assert_eq!(show_scalar(&f64::NEG_INFINITY.to_le_bytes(), &of("double", 0x04, 8)).as_deref(), Some("-Infinity"));
        let day = lamella_dwarf::ScalarType { enumeration: true, ..of("DayOfWeek", 0x05, 4) };
        assert_eq!(show_scalar(&5i32.to_le_bytes(), &day).as_deref(), Some("5"), "an enumeration as its number");
    }

    /// What this does not read is left to the hexadecimal word, never guessed at.
    #[test]
    fn a_type_this_does_not_read_is_left_to_the_word() {
        assert_eq!(show_scalar(&[1, 2, 3, 4], &of("address", 0x01, 4)), None, "an encoding it does not read");
        assert_eq!(show_scalar(&[1, 2], &of("int", 0x05, 4)), None, "fewer bytes than the type");
        assert_eq!(show_scalar(&[0; 16], &of("decimal", 0x05, 16)), None, "wider than eight bytes");
        assert_eq!(in_register(6, Some(of("int", 0x05, 4))), "6");
        assert_eq!(in_register(6, Some(of("long", 0x05, 8))), "0x00000006", "a value wider than a register");
        assert_eq!(in_register(6, None), "0x00000006");
    }

    /// A constant the debug information gives is its declared type's value with those bits, of
    /// any width this reads -- zero is each type's zero -- and the hexadecimal word where the type
    /// is not one this reads, as a value read from the target is.
    #[test]
    fn a_constant_reads_as_its_types_value_and_as_the_word_where_the_type_is_not_read() {
        assert_eq!(constant(0, Some(of("int", 0x05, 4))), "0");
        assert_eq!(constant(0, Some(of("long", 0x05, 8))), "0", "wider than a register");
        assert_eq!(constant(0, Some(of("byte", 0x08, 1))), "0");
        assert_eq!(constant(0, Some(of("bool", 0x02, 1))), "false");
        assert_eq!(constant(0, Some(of("char16_t", 0x10, 2))), "0", "a character's code, as zero in memory reads");
        assert_eq!(constant(0, Some(of("float", 0x04, 4))), "0");
        assert_eq!(constant(0, Some(of("double", 0x04, 8))), "0");
        assert_eq!(constant(31, Some(of("int", 0x05, 4))), "31", "the largest literal");
        assert_eq!(constant(1, Some(of("bool", 0x02, 1))), "true");
        assert_eq!(constant(0, None), "0x00000000", "an undescribed type, as its word in memory reads");
        assert_eq!(constant(0, Some(of("decimal", 0x05, 16))), "0x00000000", "wider than eight bytes");
    }
}
