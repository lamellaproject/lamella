//! `.debug_frame`: the call-frame information a stack walk unwinds through.

use alloc::vec::Vec;

use crate::cursor::{Cursor, Format};
use crate::{DwarfError, Options, Sections};

/// Call-frame instructions (DWARF 5 section 6.4.2.1 to 6.4.2.4, table 7.29).
const DW_CFA_ADVANCE_LOC: u8 = 0x40;
const DW_CFA_OFFSET: u8 = 0x80;
const DW_CFA_RESTORE: u8 = 0xc0;

const DW_CFA_NOP: u8 = 0x00;
const DW_CFA_SET_LOC: u8 = 0x01;
const DW_CFA_ADVANCE_LOC1: u8 = 0x02;
const DW_CFA_ADVANCE_LOC2: u8 = 0x03;
const DW_CFA_ADVANCE_LOC4: u8 = 0x04;
const DW_CFA_OFFSET_EXTENDED: u8 = 0x05;
const DW_CFA_RESTORE_EXTENDED: u8 = 0x06;
const DW_CFA_UNDEFINED: u8 = 0x07;
const DW_CFA_SAME_VALUE: u8 = 0x08;
const DW_CFA_REGISTER: u8 = 0x09;
const DW_CFA_REMEMBER_STATE: u8 = 0x0a;
const DW_CFA_RESTORE_STATE: u8 = 0x0b;
const DW_CFA_DEF_CFA: u8 = 0x0c;
const DW_CFA_DEF_CFA_REGISTER: u8 = 0x0d;
const DW_CFA_DEF_CFA_OFFSET: u8 = 0x0e;
const DW_CFA_DEF_CFA_EXPRESSION: u8 = 0x0f;
const DW_CFA_EXPRESSION: u8 = 0x10;
const DW_CFA_OFFSET_EXTENDED_SF: u8 = 0x11;
const DW_CFA_DEF_CFA_SF: u8 = 0x12;
const DW_CFA_DEF_CFA_OFFSET_SF: u8 = 0x13;
const DW_CFA_VAL_OFFSET: u8 = 0x14;
const DW_CFA_VAL_OFFSET_SF: u8 = 0x15;
const DW_CFA_VAL_EXPRESSION: u8 = 0x16;

/// How the canonical frame address is computed for a row.
///
/// The CFA is the value the stack pointer had in the CALLER immediately before the call, and every
/// other rule in a row is expressed relative to it. That indirection is the whole reason unwinding
/// works without a frame pointer: the offsets a prologue creates all move together, and one rule
/// per range describes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfaRule<'a> {
    /// No rule was established, which for a well-formed table means the address is not in a frame
    /// this producer described.
    Unknown,
    /// The value of `register`, plus `offset`.
    RegisterOffset {
        /// The DWARF register number to read from the halted frame.
        register: u64,
        /// A signed byte offset to add to it.
        offset: i64,
    },
    /// A DWARF expression whose value IS the CFA. Carried as bytes: evaluating an expression needs
    /// a stack machine with access to the target, which is a tier this crate does not read.
    Expression(&'a [u8]),
}

/// Where a register's value from the caller's frame is found, given the row's CFA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterRule<'a> {
    /// The producer said nothing, so the caller's value is wherever the calling convention leaves
    /// it. DWARF 5 section 6.4.1 leaves this to the ABI, and for a callee-saved register it means
    /// the callee did not touch it.
    Undefined,
    /// The callee preserved it: the value in the halted frame is the caller's.
    SameValue,
    /// At `CFA + offset` in memory.
    Offset(i64),
    /// The VALUE is `CFA + offset`, not the memory there. Used for a register holding an address
    /// into the frame rather than a saved datum.
    ValOffset(i64),
    /// In another register of the halted frame.
    Register(u64),
    /// At the address a DWARF expression computes. Carried as bytes, for the reason
    /// [`CfaRule::Expression`] gives.
    Expression(&'a [u8]),
    /// The VALUE a DWARF expression computes. Carried as bytes.
    ValExpression(&'a [u8]),
}

/// One row of the unwind table: how to recover the caller's frame from the halted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindRow<'a> {
    /// The lowest address this row describes.
    pub start: u64,
    /// One past the highest address this row describes.
    pub end: u64,
    /// The address range of the whole frame description this row came from, `[start, end)`.
    ///
    /// A row covers the span over which the rules do not change; this covers the function. A walk
    /// needs the second to tell a caller from a leftover: where the table gives no rule for the
    /// return address, the convention is that it is still in the return address register -- true of
    /// a leaf, and false of a function that has called out since without saving it. Those two are
    /// indistinguishable from the rules alone, and the tell is that the second one's "caller" lands
    /// back inside the function it was read from.
    pub function: (u64, u64),
    /// How to compute the canonical frame address.
    pub cfa: CfaRule<'a>,
    /// The register number the producer designated to hold the return address. Its rule is in
    /// [`Self::registers`] and [`Self::return_address`] is the shortcut to it.
    pub return_address_register: u64,
    /// Every register the table established a rule for, ascending by register number.
    pub registers: Vec<(u64, RegisterRule<'a>)>,
    /// The opcode that ended the row early, or `None` where the instruction stream ran to its end.
    ///
    /// # A PARTIAL ROW AND A COMPLETE ONE MUST NOT LOOK ALIKE
    ///
    /// A call-frame instruction this reader does not know cannot be stepped over -- its operands
    /// have no length -- so the row stops there carrying the rules established up to that point.
    /// That is the right answer for a tool REPORTING what a section contains, and the wrong one for
    /// a stack walk, which would apply a prefix of a function's rules as if they were all of them
    /// and get a plausible wrong frame.
    ///
    /// Both consumers are served by the row saying which it is. **A walk must refuse a row where
    /// this is `Some`**; a reporter prints it and says how far it got.
    pub truncated_at: Option<u8>,
}

impl<'a> UnwindRow<'a> {
    /// The rule for `register`, or [`RegisterRule::Undefined`] where the table established none.
    #[must_use]
    pub fn register(&self, register: u64) -> RegisterRule<'a> {
        self.registers
            .binary_search_by_key(&register, |&(number, _)| number)
            .map_or(RegisterRule::Undefined, |i| self.registers[i].1.clone())
    }

    /// The rule for the return address, which is the one a stack walk needs first.
    #[must_use]
    pub fn return_address(&self) -> RegisterRule<'a> {
        self.register(self.return_address_register)
    }

    /// Whether this row describes a frame that can exist, given which register holds the stack
    /// pointer.
    ///
    /// # A ROW CAN BE INTERNALLY IMPOSSIBLE, AND A SHIPPING COMPILER EMITS ONE
    ///
    /// The CFA is the stack pointer as the CALLER had it, so anything the callee SAVED IN MEMORY
    /// lies between its own stack pointer and there: `CFA + offset >= SP`, which under
    /// `CFA = SP + n` is `n + offset >= 0`. A rule putting a saved register below the stack pointer
    /// names dead stack, and a walk reading a return address from it gets whatever the last
    /// interrupt or the last deeper call left behind.
    ///
    /// The shape that produces one is a prologue that moves the stack TWICE -- on a target without a
    /// multi-register push, `push {r4, r5, lr}` followed by `mov lr, r10; push {lr}` -- where the
    /// second push gets its `DW_CFA_offset` and no `DW_CFA_def_cfa_offset` to go with it. The frame
    /// is sixteen bytes and the table says twelve, so every rule in that row is read four bytes
    /// high. Every reader decodes it identically, because the file is wrong rather than the readers,
    /// and this is the check that says so.
    ///
    /// It takes the stack pointer's register number because only a CFA relative to it can be checked
    /// this way -- one computed from a frame pointer says nothing about where the stack is, and is
    /// reported possible rather than guessed at. Rules whose location is a DWARF expression are not
    /// checked either: this crate does not evaluate them, and [`RegisterRule::Expression`] is the
    /// signal to a walk that it must stop.
    ///
    /// **A truncated stack is visible and a caller read from dead stack is not**, which is why this
    /// answers the safe way round.
    ///
    /// # THIS IS NOT SCAFFOLDING WAITING ON A COMPILER FIX
    ///
    /// The producer defect above is being repaired upstream, and that does not retire this check.
    /// A repair has to land in the compiler, ship in a compiler RELEASE, and then be bundled by a
    /// language toolchain that somebody has installed -- three independent release cycles, and the
    /// rows an image carries are the rows the toolchain that BUILT it emitted.
    ///
    /// And a debugger reads images it did not build. Anything compiled before the fix keeps its
    /// contradictory table for as long as that binary exists, so this stays load-bearing after the
    /// defect is fixed everywhere. **Do not remove it on the strength of a changelog.**
    #[must_use]
    pub fn describes_a_possible_frame(&self, stack_pointer: u64) -> bool {
        let CfaRule::RegisterOffset { register, offset } = self.cfa else {
            return true;
        };
        if register != stack_pointer {
            return true;
        }
        self.registers.iter().all(|(_, rule)| match rule {
            RegisterRule::Offset(saved) => offset + saved >= 0,
            _ => true,
        })
    }
}

/// A parsed common information entry: the header its FDEs share, and the instructions that build
/// the row they all start from.
#[derive(Debug, Clone)]
struct Cie<'a> {
    offset: usize,
    version: u8,
    address_size: u8,
    code_alignment: u64,
    data_alignment: i64,
    return_address_register: u64,
    initial_instructions: &'a [u8],
}

/// One frame description entry: an address range and the instructions that refine its rows.
#[derive(Debug, Clone)]
struct Fde<'a> {
    cie: usize,
    start: u64,
    end: u64,
    instructions: &'a [u8],
}

/// What one common information entry declares, for a tool that reports what a section contains.
///
/// Every field here changes how the FDEs sharing this entry decode, and a producer's choices are
/// not guessable from the instruction stream -- which is why a comparison against another reader
/// starts by agreeing on these rather than on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CieSummary {
    /// The entry's own offset in `.debug_frame`, which is what its FDEs point at.
    pub offset: usize,
    /// The entry's version, which is its own and not the compilation unit's.
    pub version: u8,
    /// The size in bytes of an address in this entry's FDEs.
    pub address_size: u8,
    /// Every `advance_loc` delta is multiplied by this.
    pub code_alignment: u64,
    /// Every factored offset is multiplied by this, and it is negative on a stack that grows down.
    pub data_alignment: i64,
    /// The register the producer designated to hold the return address.
    pub return_address_register: u64,
}

/// The call-frame information in a `.debug_frame` section, ready to answer a stack walk.
///
/// Parsed separately from [`crate::DebugInfo`] rather than alongside it, because the two serve
/// different questions and most consumers ask only one: a session that maps addresses to source
/// lines never unwinds, and a backtrace never needs the line program. Both take the same
/// [`Sections`].
#[derive(Debug, Clone)]
pub struct FrameTable<'a> {
    cies: Vec<Cie<'a>>,
    fdes: Vec<Fde<'a>>,
}

impl<'a> FrameTable<'a> {
    /// Parses `.debug_frame`, reporting addresses exactly as the section stores them.
    ///
    /// # Errors
    /// See [`DwarfError`]. An absent or empty section parses to a table that answers nothing.
    pub fn parse(sections: &Sections<'a>) -> Result<Self, DwarfError> {
        Self::parse_with(sections, &Options::default())
    }

    /// Parses `.debug_frame` against what the caller knows about the target.
    ///
    /// An FDE's `initial_location` is relocated against a function symbol exactly as a line table's
    /// `DW_LNE_set_address` is, so on ARM it can arrive with the Thumb bit set for the same reason
    /// and with the same consequence: every lookup lands one address late. [`Options`] carries the
    /// caller's answer.
    ///
    /// # Errors
    /// See [`DwarfError`].
    pub fn parse_with(sections: &Sections<'a>, options: &Options) -> Result<Self, DwarfError> {
        let mut cies: Vec<Cie<'a>> = Vec::new();
        let mut fdes: Vec<Fde<'a>> = Vec::new();
        let mut cursor = Cursor::new(sections.debug_frame);

        while !cursor.is_empty() {
            if cursor.remaining() < 4 {
                break;
            }
            let offset = cursor.offset();
            let (length, format) = cursor.initial_length()?;
            if length == 0 {
                break;
            }
            let length = usize::try_from(length).map_err(|_| DwarfError::Truncated)?;
            let mut entry = cursor.split(length)?;

            let id = entry.offset_of(format)?;
            let is_cie = match format {
                Format::Dwarf32 => id == u64::from(u32::MAX),
                Format::Dwarf64 => id == u64::MAX,
            };

            if is_cie {
                cies.push(parse_cie(&mut entry, offset)?);
                continue;
            }

            let cie_offset = usize::try_from(id).map_err(|_| DwarfError::Truncated)?;
            let Some(index) = cies.iter().position(|cie| cie.offset == cie_offset) else {
                return Err(DwarfError::UnknownAbbreviation(id));
            };
            let address_size = cies[index].address_size;
            let mut start = entry.address(address_size)?;
            let range = entry.address(address_size)?;
            if options.arm_thumb_addresses {
                start &= !1;
            }
            fdes.push(Fde {
                cie: index,
                start,
                end: start.saturating_add(range),
                instructions: entry.take(entry.remaining())?,
            });
        }

        fdes.sort_by_key(|fde| fde.start);
        Ok(FrameTable { cies, fdes })
    }

    /// What each common information entry declares, in section order.
    #[must_use]
    pub fn cies(&self) -> Vec<CieSummary> {
        self.cies
            .iter()
            .map(|cie| CieSummary {
                offset: cie.offset,
                version: cie.version,
                address_size: cie.address_size,
                code_alignment: cie.code_alignment,
                data_alignment: cie.data_alignment,
                return_address_register: cie.return_address_register,
            })
            .collect()
    }

    /// How many frame descriptions the section carried.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fdes.len()
    }

    /// Whether the section described no frames at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fdes.is_empty()
    }

    /// The address ranges the table describes, ascending. A stack walk that reaches an address in
    /// none of them has left the code this producer described.
    #[must_use]
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.fdes.iter().map(|fde| (fde.start, fde.end)).collect()
    }

    /// Every row of every frame description, ascending by address.
    ///
    /// The whole table rather than one query, which is what a comparison against another reader
    /// needs: a row is where the rules CHANGE, so sampling addresses can agree everywhere it looks
    /// and still miss a boundary that moved.
    ///
    /// # Errors
    /// See [`DwarfError`].
    pub fn rows(&self) -> Result<Vec<UnwindRow<'a>>, DwarfError> {
        let mut out = Vec::new();
        for fde in &self.fdes {
            let mut address = fde.start;
            while address < fde.end {
                let Some(row) = self.unwind(address)? else {
                    break;
                };
                let next = row.end;
                out.push(row);
                if next <= address {
                    break;
                }
                address = next;
            }
        }
        Ok(out)
    }

    /// The unwind rules in force at `address`.
    ///
    /// The row is built by running the CIE's initial instructions and then the FDE's, stopping at
    /// the first instruction that advances past `address` -- which is what makes this a row of a
    /// virtual table rather than a lookup: the table is a program, and every row is the state the
    /// program is in at that point.
    ///
    /// # Errors
    /// See [`DwarfError`]. An address in no FDE is `Ok(None)` rather than an error: most of a linked
    /// image is code some object was built without frame information for.
    pub fn unwind(&self, address: u64) -> Result<Option<UnwindRow<'a>>, DwarfError> {
        let index = self.fdes.partition_point(|fde| fde.start <= address);
        let Some(fde) = self.fdes[..index]
            .iter()
            .rev()
            .find(|fde| address < fde.end)
        else {
            return Ok(None);
        };
        let cie = &self.cies[fde.cie];

        let mut state = State::new();
        run(&mut Cursor::new(cie.initial_instructions), cie, &mut state, u64::MAX)?;
        state.initial = state.registers.clone();
        state.location = fde.start;
        state.start = fde.start;
        run(&mut Cursor::new(fde.instructions), cie, &mut state, address)?;

        let mut registers = state.registers;
        registers.sort_by_key(|&(number, _)| number);
        Ok(Some(UnwindRow {
            start: state.start,
            end: state.end.unwrap_or(fde.end),
            function: (fde.start, fde.end),
            cfa: state.cfa,
            return_address_register: cie.return_address_register,
            registers,
            truncated_at: state.truncated_at,
        }))
    }
}

/// Parses a CIE's header, whose shape changes with its version in two places a reader cannot guess.
fn parse_cie<'a>(entry: &mut Cursor<'a>, offset: usize) -> Result<Cie<'a>, DwarfError> {
    let version = entry.u8()?;
    if version != 1 && version != 3 && version != 4 {
        return Err(DwarfError::UnsupportedUnitVersion(u16::from(version)));
    }
    let augmentation = entry.cstr()?;
    if !augmentation.is_empty() {
        return Err(DwarfError::UnknownForm(u64::from(augmentation[0])));
    }
    let (address_size, segment_selector_size) = if version >= 4 {
        (entry.u8()?, entry.u8()?)
    } else {
        (4, 0)
    };
    if !matches!(address_size, 1 | 2 | 4 | 8) {
        return Err(DwarfError::UnsupportedAddressSize(address_size));
    }
    if segment_selector_size != 0 {
        return Err(DwarfError::SegmentedAddresses);
    }
    let code_alignment = entry.uleb128()?;
    if code_alignment == 0 {
        return Err(DwarfError::ZeroLineRange);
    }
    let data_alignment = entry.sleb128()?;
    let return_address_register = if version == 1 {
        u64::from(entry.u8()?)
    } else {
        entry.uleb128()?
    };
    Ok(Cie {
        offset,
        version,
        address_size,
        code_alignment,
        data_alignment,
        return_address_register,
        initial_instructions: entry.take(entry.remaining())?,
    })
}

/// The state machine's registers: the rules established so far, plus what to restore them to.
struct State<'a> {
    cfa: CfaRule<'a>,
    registers: Vec<(u64, RegisterRule<'a>)>,
    initial: Vec<(u64, RegisterRule<'a>)>,
    remembered: Vec<(CfaRule<'a>, Vec<(u64, RegisterRule<'a>)>)>,
    /// The address the instruction stream has advanced to.
    location: u64,
    /// The lowest address the row being built describes.
    start: u64,
    /// Where the row being built stops, set when the stream advanced past the address asked about.
    end: Option<u64>,
    /// The opcode that ended the stream early, if one did.
    truncated_at: Option<u8>,
}

impl<'a> State<'a> {
    fn new() -> State<'a> {
        State {
            cfa: CfaRule::Unknown,
            registers: Vec::new(),
            initial: Vec::new(),
            remembered: Vec::new(),
            location: 0,
            start: 0,
            end: None,
            truncated_at: None,
        }
    }

    fn set(&mut self, register: u64, rule: RegisterRule<'a>) {
        match self
            .registers
            .iter()
            .position(|&(number, _)| number == register)
        {
            Some(i) => self.registers[i].1 = rule,
            None => self.registers.push((register, rule)),
        }
    }

    fn restore(&mut self, register: u64) {
        let rule = self
            .initial
            .iter()
            .find(|&&(number, _)| number == register)
            .map_or(RegisterRule::Undefined, |(_, rule)| rule.clone());
        self.set(register, rule);
    }
}

/// Runs a call-frame instruction stream until it advances past `address`.
///
/// `u64::MAX` runs the whole stream, which is what the CIE's initial instructions want: they
/// describe the row at a function's entry and contain no advance at all.
fn run<'a>(
    cursor: &mut Cursor<'a>,
    cie: &Cie<'a>,
    state: &mut State<'a>,
    address: u64,
) -> Result<(), DwarfError> {
    while !cursor.is_empty() {
        let opcode = cursor.u8()?;
        let high = opcode & 0xc0;
        let low = u64::from(opcode & 0x3f);
        match high {
            DW_CFA_ADVANCE_LOC => {
                if advance(state, scaled(low, cie), address) {
                    return Ok(());
                }
                continue;
            }
            DW_CFA_OFFSET => {
                let factored = cursor.uleb128()?;
                let offset = factored_offset(factored as i64, cie.data_alignment);
                state.set(low, RegisterRule::Offset(offset));
                continue;
            }
            DW_CFA_RESTORE => {
                state.restore(low);
                continue;
            }
            _ => {}
        }
        match opcode {
            DW_CFA_NOP => {}
            DW_CFA_SET_LOC => {
                let to = cursor.address(cie.address_size)?;
                if to > address {
                    state.end = Some(to);
                    return Ok(());
                }
                state.location = to;
                state.start = to;
            }
            DW_CFA_ADVANCE_LOC1 => {
                let delta = scaled(u64::from(cursor.u8()?), cie);
                if advance(state, delta, address) {
                    return Ok(());
                }
            }
            DW_CFA_ADVANCE_LOC2 => {
                let delta = scaled(u64::from(cursor.u16()?), cie);
                if advance(state, delta, address) {
                    return Ok(());
                }
            }
            DW_CFA_ADVANCE_LOC4 => {
                let delta = scaled(u64::from(cursor.u32()?), cie);
                if advance(state, delta, address) {
                    return Ok(());
                }
            }
            DW_CFA_OFFSET_EXTENDED => {
                let register = cursor.uleb128()?;
                let factored = cursor.uleb128()?;
                let offset = factored_offset(factored as i64, cie.data_alignment);
                state.set(register, RegisterRule::Offset(offset));
            }
            DW_CFA_OFFSET_EXTENDED_SF => {
                let register = cursor.uleb128()?;
                let factored = cursor.sleb128()?;
                state.set(
                    register,
                    RegisterRule::Offset(factored_offset(factored, cie.data_alignment)),
                );
            }
            DW_CFA_RESTORE_EXTENDED => {
                let register = cursor.uleb128()?;
                state.restore(register);
            }
            DW_CFA_UNDEFINED => {
                let register = cursor.uleb128()?;
                state.set(register, RegisterRule::Undefined);
            }
            DW_CFA_SAME_VALUE => {
                let register = cursor.uleb128()?;
                state.set(register, RegisterRule::SameValue);
            }
            DW_CFA_REGISTER => {
                let register = cursor.uleb128()?;
                let holder = cursor.uleb128()?;
                state.set(register, RegisterRule::Register(holder));
            }
            DW_CFA_REMEMBER_STATE => {
                state.remembered.push((state.cfa.clone(), state.registers.clone()));
            }
            DW_CFA_RESTORE_STATE => {
                if let Some((cfa, registers)) = state.remembered.pop() {
                    state.cfa = cfa;
                    state.registers = registers;
                }
            }
            DW_CFA_DEF_CFA => {
                let register = cursor.uleb128()?;
                let offset = cursor.uleb128()?;
                state.cfa = CfaRule::RegisterOffset {
                    register,
                    offset: offset as i64,
                };
            }
            DW_CFA_DEF_CFA_SF => {
                let register = cursor.uleb128()?;
                let factored = cursor.sleb128()?;
                state.cfa = CfaRule::RegisterOffset {
                    register,
                    offset: factored_offset(factored, cie.data_alignment),
                };
            }
            DW_CFA_DEF_CFA_REGISTER => {
                let register = cursor.uleb128()?;
                if let CfaRule::RegisterOffset { offset, .. } = state.cfa {
                    state.cfa = CfaRule::RegisterOffset { register, offset };
                }
            }
            DW_CFA_DEF_CFA_OFFSET => {
                let offset = cursor.uleb128()?;
                if let CfaRule::RegisterOffset { register, .. } = state.cfa {
                    state.cfa = CfaRule::RegisterOffset {
                        register,
                        offset: offset as i64,
                    };
                }
            }
            DW_CFA_DEF_CFA_OFFSET_SF => {
                let factored = cursor.sleb128()?;
                if let CfaRule::RegisterOffset { register, .. } = state.cfa {
                    state.cfa = CfaRule::RegisterOffset {
                        register,
                        offset: factored_offset(factored, cie.data_alignment),
                    };
                }
            }
            DW_CFA_VAL_OFFSET => {
                let register = cursor.uleb128()?;
                let factored = cursor.uleb128()?;
                state.set(
                    register,
                    RegisterRule::ValOffset(factored_offset(factored as i64, cie.data_alignment)),
                );
            }
            DW_CFA_VAL_OFFSET_SF => {
                let register = cursor.uleb128()?;
                let factored = cursor.sleb128()?;
                state.set(
                    register,
                    RegisterRule::ValOffset(factored_offset(factored, cie.data_alignment)),
                );
            }
            DW_CFA_DEF_CFA_EXPRESSION => {
                let block = block(cursor)?;
                state.cfa = CfaRule::Expression(block);
            }
            DW_CFA_EXPRESSION => {
                let register = cursor.uleb128()?;
                let block = block(cursor)?;
                state.set(register, RegisterRule::Expression(block));
            }
            DW_CFA_VAL_EXPRESSION => {
                let register = cursor.uleb128()?;
                let block = block(cursor)?;
                state.set(register, RegisterRule::ValExpression(block));
            }
            other => {
                state.truncated_at = Some(other);
                return Ok(());
            }
        }
    }
    Ok(())
}

/// One advance delta in bytes: the operand times the CIE's code alignment.
///
/// Four opcodes carry a delta and a fifth packs one into its own low bits, so this is one rule with
/// five call sites -- which is why it is a function rather than a multiply repeated five times. It
/// SATURATES: a code alignment is a ULEB128 and a damaged one can be any value up to `u64::MAX`, and
/// a reader that faults on a corrupt header is worse than one that answers a bounded nonsense.
fn scaled(delta: u64, cie: &Cie<'_>) -> u64 {
    delta.saturating_mul(cie.code_alignment)
}

/// Advances the program counter, and reports whether the row being built is now complete.
fn advance(state: &mut State<'_>, delta: u64, address: u64) -> bool {
    let to = state.location.saturating_add(delta);
    if to > address {
        state.end = Some(to);
        return true;
    }
    state.location = to;
    state.start = to;
    false
}

/// A factored offset in bytes: the format stores offsets divided by the CIE's data alignment, which
/// on a target whose stack slots are all one size makes almost every offset a single-byte operand.
fn factored_offset(factored: i64, data_alignment: i64) -> i64 {
    factored.saturating_mul(data_alignment)
}

/// Reads a length-prefixed expression block, returning its bytes without evaluating them.
fn block<'a>(cursor: &mut Cursor<'a>) -> Result<&'a [u8], DwarfError> {
    let length = cursor.uleb128()?;
    let length = usize::try_from(length).map_err(|_| DwarfError::Truncated)?;
    cursor.take(length)
}
