//! The v2 strata dialect and its emitters.

use crate::{brackets_open, err, format_int, pascal, strip_comment, Calibration, Channel, Fact, Field, Int, RawValue, Step, StepValue, ValueCursor};


/// One register of a block: a block-relative offset, an access width, and its fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRegister {
    /// The register name (the `[registers.*]` key).
    pub name: String,
    /// The byte offset within the block (instance-base-relative).
    pub offset: Int,
    /// The access width in bits: 8, 16, or 32 (widths are data, not prose).
    pub width: u32,
    /// The declared fields, in table order.
    pub fields: Vec<Field>,
}

/// A block-layout table: what one IP block looks like, independent of where it sits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockTable {
    /// The owning family id.
    pub family: String,
    /// The block id (`sercom`, `port`, `gclk`, `pm`).
    pub block: String,
    /// The block mode when modes reinterpret registers (`usart`); empty otherwise.
    pub mode: String,
    /// Registers, in table order.
    pub registers: Vec<BlockRegister>,
    /// Block-scoped constants (mode magic, function-letter values), in table order.
    pub constants: Vec<(String, Int)>,
    /// Driver-supplied parameters: name -> the table's prose contract.
    pub parameters: Vec<(String, String)>,
    /// Block-LOCAL sequences (every step touches a block register), in table order.
    pub sequences: Vec<(String, Vec<Step>)>,
    /// Facts-as-data (`[facts]`): chip/electrical facts conversions read, for channel-muxed
    /// blocks. BOARD facts do NOT ride here -- a board's reference rail is board truth and
    /// rides its adc binding.
    pub facts: Vec<(String, Fact)>,
    /// Channel-indexed input map (`[[channels]]`), in table order.
    pub channels: Vec<Channel>,
    /// Declarative calibration records (`[calibration.*]`): form + integer coefficients,
    /// never an expression language.
    pub calibrations: Vec<Calibration>,
}

impl BlockRegister {
    /// This register's field named `name`, when declared.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

impl BlockTable {
    /// The register named `name`, when declared.
    #[must_use]
    pub fn register(&self, name: &str) -> Option<&BlockRegister> {
        self.registers.iter().find(|r| r.name == name)
    }

    /// A `value` placed in register `register`'s field `field`, shifted and checked to fit.
    ///
    /// Composition goes through the TABLE's own field positions rather than through a shift
    /// written into the generator, so a field that moves in the manual moves the emitted word
    /// with it -- and a value too wide for its field is refused here rather than silently
    /// overwriting its neighbor, which is the failure mode of a hand-composed configuration word.
    pub fn place(&self, register: &str, field: &str, value: i64) -> Result<i64, String> {
        let Some(reg) = self.register(register) else {
            return Err(format!("block {}: no register '{register}'", self.block));
        };
        let Some(spec) = reg.field(field) else {
            return Err(format!("block {}: register {register} has no field '{field}'", self.block));
        };
        let limit = 1i64 << spec.width;
        if value < 0 || value >= limit {
            return Err(format!(
                "block {}: {value} does not fit {register}.{field}, which is {} bit(s) wide",
                self.block, spec.width
            ));
        }
        Ok(value << spec.lsb)
    }

    /// The named block constant's value, when declared.
    #[must_use]
    pub fn constant(&self, name: &str) -> Option<i64> {
        self.constants.iter().find(|(n, _)| n == name).map(|(_, v)| v.value)
    }
}

/// One instance row: a placed block copy with the family's per-instance record values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceRow {
    /// The instance id (`sercom3`, `porta`), family-scoped.
    pub name: String,
    /// The block this instance places.
    pub block: String,
    /// The record values, in the family's declared record order. `-1` = does not apply.
    pub values: Vec<i64>,
    /// The pin-name port group this instance IS, when the family states it (`"C"` on a row that
    /// backs pins named `PC<n>`); empty when it states nothing. See [`InstanceRow::port_char`].
    ///
    /// STATED RATHER THAN GUESSED FROM THE INSTANCE NAME. A control pin resolves to a port
    /// group's base address, and vendors do not agree on what that group is called: `PORTA`,
    /// `PORT0`, `GPIOA`, `PIOA`, `SIO` and `IO_MUX` all name one, and one of those is numbered
    /// where the rest are lettered. A derivation that pattern-matches the name has to grow by one
    /// arm per vendor and is wrong -- silently, at generation time -- for the vendor after that.
    /// One optional field per row costs a family that states nothing nothing.
    pub port: String,
}

impl InstanceRow {
    /// The port group this instance backs, lowercased to match [`split_pin`]; `None` when the
    /// row states none.
    #[must_use]
    pub fn port_char(&self) -> Option<char> {
        let mut chars = self.port.chars();
        let first = chars.next()?.to_ascii_lowercase();
        chars.next().is_none().then_some(first)
    }
}

/// The instance map: every placed block copy of a family.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstancesTable {
    /// The family id.
    pub family: String,
    /// The uniform record schema every row carries; `base` must be first.
    pub record: Vec<String>,
    /// The rows, in table order.
    pub rows: Vec<InstanceRow>,
}

impl InstancesTable {
    /// The row named `name`, when declared.
    #[must_use]
    pub fn row(&self, name: &str) -> Option<&InstanceRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    /// A row's record value by field name (`base`, `gclk_core_id`, ...), when both exist.
    #[must_use]
    pub fn value(&self, row: &str, field: &str) -> Option<i64> {
        let at = self.record.iter().position(|f| f == field)?;
        self.row(row).and_then(|r| r.values.get(at).copied())
    }
}

/// The `instance` value a pin row states when NOTHING reaches the cell.
///
/// Reserved: an instance map may not place an instance under this name, so the value can never be
/// read as a routing target that happens to be spelled unusually.
pub const NO_CONTROLLER: &str = "none";

/// One pin-function row: pin x function -> (instance, signal), or -> nothing.
///
/// A row takes ONE OF TWO SHAPES and every field of the shape it takes is required:
/// - ROUTED: `instance` names an instance the family places, `signal` names the cell's signal.
/// - UNROUTED: `instance = "none"` and there is no `signal`, because a cell that reaches no
///   controller has no signal to name.
///
/// THE SECOND SHAPE EXISTS BECAUSE AN ABSENCE AND AN OMISSION ARE INDISTINGUISHABLE IN A TABLE
/// THAT HAS ONLY ONE WAY TO SAY NEITHER. "No controller reaches this cell" is a fact somebody read
/// out of a pin table; a missing `instance` is a field somebody did not fill in. Rendered as the
/// same empty string they cannot be told apart, and they want opposite responses -- the first
/// wants to be believed, the second wants a datasheet opened. The costlier mistake is the silent
/// one: a cell believed to reach nothing is a cell nobody looks for a conflict on.
///
/// The unrouted shape is also the only one that cannot be checked by consequence. A routed row is
/// held to the binding that uses it and, past that, to silicon; a row saying nothing is here has
/// nothing downstream to disagree with it, so `source` is REQUIRED on it -- the citation is the
/// only check it will ever get. Same rule the `[sourcing]` tier holds a part to: a claim that
/// cannot be falsified by use has to carry its evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRow {
    /// The pin name (`PA22`).
    pub pin: String,
    /// The mux function letter (`C`).
    pub function: String,
    /// The instance the (pin, function) cell routes to, or [`NO_CONTROLLER`].
    pub instance: String,
    /// The signal at that cell (`pad0`); empty on an unrouted row.
    pub signal: String,
    /// Where the row was read. Required on every row, and load-bearing on an unrouted one.
    pub source: String,
}

impl PinRow {
    /// True when the row states that no controller reaches the cell.
    #[must_use]
    pub fn is_unrouted(&self) -> bool {
        self.instance == NO_CONTROLLER
    }
}

/// The pin-function map (partial, append-only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinsTable {
    /// The family id.
    pub family: String,
    /// The rows, in table order.
    pub rows: Vec<PinRow>,
}

/// One part row: an orderable chip with its package, memory, (partial) pin set, and -- when the
/// family states one -- the core's instruction-set profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartRow {
    /// The part id (`atsamd21g18a`).
    pub part: String,
    /// The package name (documentation).
    pub package: String,
    /// Flash size in bytes.
    pub flash: i64,
    /// RAM size in bytes.
    pub ram: i64,
    /// Present pins, as names or inclusive same-port ranges (`PA12-PA15`).
    pub pins: Vec<String>,
    /// The part's PROCESSOR SOCKETS, in boot order, each with the architecture(s) it can run.
    /// Empty on a single-core part, where the `isa*` fields describe the one core.
    ///
    /// A SOCKET RATHER THAN A CORE, and the distinction is forced by silicon rather than invented:
    /// some silicon lets each socket be an Arm core OR a RISC-V core, selected by a
    /// register, so "which core is in socket 1" is not a fixed property of the part. Listing the
    /// admissible architectures per socket states what the part offers; WHICH ONE AN IMAGE USES is
    /// a target's choice and is not a fact about the chip.
    ///
    /// THIS DOES NOT REPLACE `isa`. That field is a RISC-V PROFILE whose consequences (register
    /// count, multiply path) are derived and checked from its letters. This is a core-architecture
    /// NAME, a coarser thing, and the two answer different questions -- so a part may state both,
    /// and a mistake in one is not caught by the other.
    pub cores: Vec<(String, Vec<String>)>,
    /// Whether the sockets reach ONE address space. `None` on a single-core part; REQUIRED once
    /// `cores` is stated, because it is half of the combinability test and a default would decide
    /// it silently.
    ///
    /// SHARED MEMORY IS NOT SUFFICIENT FOR THREADING ACROSS CORES, and the part that proves it
    /// is shipping silicon: sockets running DIFFERENT architectures can share memory and still need
    /// two separate program images. Same architecture AND shared memory is the condition; this
    /// field is only the second half.
    pub cores_share_memory: Option<bool>,
    /// Pins the part carries that A PROGRAM MAY NOT USE, each with the peripheral that owns it.
    ///
    /// PRESENT AND UNAVAILABLE ARE DIFFERENT FACTS, and until this existed the model could only
    /// say the first. The present-list says a part HAS a pin; the pin map's unrouted shape says no
    /// controller reaches a (pin, FUNCTION) CELL. Neither says a pin is permanently owned by
    /// something inside the package, which is what a system-in-package states about the pins its
    /// integrated peripheral is wired through -- and those pins ARE present, so removing them from
    /// the present-list would be a second lie rather than a fix.
    ///
    /// THE GAP WAS SILENT AND THE HAZARD IS A BOARD FILE, NOT A DRIVER. A control line needs no
    /// pin-map row, so the unrouted shape is never consulted for one; a board could bind an LED to
    /// a reserved pin, pass the present-list, pass the control-pin check, and emit a real port base
    /// and a real mask. The write lands on a pin the program does not own and the board does
    /// nothing -- the same silence as a board with no LED at all.
    ///
    /// Each entry is `pin = "owner"`. The owner is REQUIRED: "unavailable" stated alone cannot be
    /// told apart from a field somebody filled in wrongly, and naming what holds the pin is what
    /// lets a reader decide whether another part of the family carries the same restriction.
    pub reserved: Vec<(String, String)>,
    /// The core's instruction-set profile (`rv32ec`), lowercase; empty when the family has not
    /// stated one. A part that merely names its architecture tells a code generator nothing it
    /// can act on, so the two consequences a backend must respect are stated BESIDE the name and
    /// CHECKED against it -- see `isa_registers` and `isa_muldiv`.
    pub isa: String,
    /// The integer register count the profile gives (`16` for an RV32E core, `32` for RV32I);
    /// 0 when unstated. Not decoration: a register allocator whose pool assumes 32 emits
    /// references to registers the silicon does not have.
    pub isa_registers: i64,
    /// How multiply and divide are reached: `hardware` (the M extension), `multiply-only` (the
    /// Zmmul extension, which is M's multiply half WITHOUT divide), or `soft` (runtime routines
    /// for both); empty when unstated. Derived-and-verified against `isa` rather than trusted.
    ///
    /// THE MIDDLE VALUE IS NOT A REFINEMENT, IT IS A CASE THAT REALLY OCCURS: a core can have a
    /// `mul` instruction and no `div`, so a two-value field would have to call such a part either
    /// hardware (and emit a division that traps) or soft (and give up a multiply it has).
    pub isa_muldiv: String,
    /// The widest hardware floating-point the core implements: `double` (the D extension),
    /// `single` (F alone), or `soft` (neither -- floating point is library routines); empty when
    /// unstated. Derived-and-verified against `isa` rather than trusted.
    ///
    /// THIS RECORDS WHAT THE SILICON HAS, NOT WHAT A BUILD EMITS. A part with an FPU may still be
    /// compiled soft-float deliberately -- for size, or to keep one image valid across a family
    /// whose members differ here -- so this field settles what is POSSIBLE and a profile knob
    /// settles what is CHOSEN. Reading it as permission to emit FP instructions would be a
    /// mistake in the other direction from ignoring it.
    pub isa_float: String,
}

impl PartRow {
    /// Whether `pin` (e.g. `PB10`) is inside this part's present-list.
    #[must_use]
    pub fn has_pin(&self, pin: &str) -> bool {
        let Some((port, index)) = split_pin(pin) else { return false };
        self.pins.iter().any(|entry| match entry.split_once('-') {
            None => entry == pin,
            Some((lo, hi)) => {
                matches!((split_pin(lo), split_pin(hi)),
                    (Some((lp, li)), Some((hp, hi_i)))
                        if lp == port && hp == port && li <= index && index <= hi_i)
            }
        })
    }

    /// What owns `pin` when the part reserves it, or `None` when a program may use it.
    ///
    /// Exact names only, deliberately -- no ranges. A reservation is read off a sentence naming
    /// specific pins, and a range would let one careless entry lock out a whole port.
    #[must_use]
    pub fn reserved_by(&self, pin: &str) -> Option<&str> {
        self.reserved.iter().find(|(p, _)| p == pin).map(|(_, owner)| owner.as_str())
    }
}

/// The parts table of a family.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartsTable {
    /// The family id.
    pub family: String,
    /// The rows, in table order.
    pub rows: Vec<PartRow>,
}

/// A pin reference inside a binding: the pin plus what the binding claims about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinRef {
    /// The pin name (`PA22`).
    pub pin: String,
    /// The claimed pad/signal index (`pad = 0` claims signal `pad0`); `-1` = no pad claim.
    pub pad: i64,
    /// A soft (GPIO-driven) line: the pin is NOT muxed to the block (a soft chip select).
    pub soft: bool,
}

/// One role binding: a board's (role -> instance + pins + routing) record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// The app-facing role id (`vcp`, `winc-spi`).
    pub role: String,
    /// The binding kind (`uart`, `spi`, `i2c`, `gpio-out`).
    pub kind: String,
    /// The bound instance id.
    pub instance: String,
    /// The mux function letter both/all muxed pins ride.
    pub function: String,
    /// The GCLK generator the instance's core clock rides under the default plan (-1 = n/a).
    pub gclk_gen: i64,
    /// The board's ADC reference voltage in microvolts (-1 = not stated). BOARD truth (the
    /// rail the converter measures against), legal only on an `adc` binding -- it is a
    /// property of the board's wiring, not of the chip.
    pub reference_uv: i64,
    /// The named signal pins (`tx`/`rx` for uart; `mosi`/`sck`/`miso`/`cs` for spi).
    pub pins: Vec<(String, PinRef)>,
}

/// A fixed on-board device or module control line: EITHER pin-wired (a named GPIO with
/// polarity) or bus-wired (an `i2c-device`-style row riding a binding role at an address).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlPin {
    /// The line name (`winc-reset_n`, `led0`, `lsm303agr`).
    pub name: String,
    /// The line kind when stated (`gpio-out`, `i2c-device`; empty for module pins).
    pub kind: String,
    /// The pin name (pin-wired rows; empty for bus devices).
    pub pin: String,
    /// `low` or `high` (the asserted level; pin-wired rows).
    pub active: String,
    /// The binding role a bus device rides (empty for pin-wired rows).
    pub role: String,
    /// The bus device's address (-1 for pin-wired rows).
    pub address: i64,
}

impl Default for ControlPin {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: String::new(),
            pin: String::new(),
            active: String::new(),
            role: String::new(),
            address: -1,
        }
    }
}

/// A module CSP: a host part plus fixed internal wiring, inherited by carrying boards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleTable {
    /// The module id (`samw25`).
    pub module: String,
    /// The host family id.
    pub family: String,
    /// The host part id.
    pub part: String,
    /// The module's fixed bindings.
    pub bindings: Vec<Binding>,
    /// The module's fixed control lines.
    pub module_pins: Vec<ControlPin>,
}

/// A board's carrier record: how the Lamella Link wire reaches it, PAIRED with the clock plan
/// the build carrying that wire runs under.
///
/// A board may declare several: a build carrying one wire runs at one operating point, so a
/// board whose wires run at different points states each wire's pair here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carrier {
    /// The carrier kind (`edbg-vcp`, `native-usb`, `probe`).
    pub kind: String,
    /// The bridge's USB VID, when the carrier has one.
    pub usb_vid: i64,
    /// The bridge's USB PID, when known.
    pub usb_pid: i64,
    /// The binding role that carries the wire (empty for native-usb).
    pub role: String,
    /// The wire baud for a VCP carrier (0 = n/a).
    pub baud: i64,
    /// The `[[plans]]` row this wire's build runs under. Empty means "the board's default plan",
    /// which is how a singular `[carrier]` section reads.
    pub plan: String,
    /// True for the wire a bare deploy reaches for. Exactly one carrier carries it -- but only
    /// when the board declares carriers at all (a bare chip declares none).
    pub default: bool,
}

/// A named clock plan: the chosen operating point divisors derive from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The plan name (`osc8m-8mhz`).
    pub name: String,
    /// Whether this is the board's default plan (exactly one must be).
    pub default: bool,
    /// The source the plan runs from (`osc8m`, `dfll48m-usb-recovery`).
    pub source: String,
    /// Generator rates, as (`gclk<N>_hz`, rate) pairs in table order.
    pub rates: Vec<(String, i64)>,
}

impl Plan {
    /// The rate of GCLK generator `generator` under this plan, when stated.
    #[must_use]
    pub fn gclk_hz(&self, generator: i64) -> Option<i64> {
        self.rate(&format!("gclk{generator}_hz"))
    }

    /// A named rate (`clk_peri_hz`, `xosc_hz`, ...) under this plan, when stated.
    #[must_use]
    pub fn rate(&self, key: &str) -> Option<i64> {
        self.rates.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

/// One memory region a board fits: a base, a size, and -- when the board brings it up itself --
/// the controller instance that must be configured before the region may be touched.
///
/// A region is NOT a number, and external memory makes that concrete three ways
/// at once: two regions rather than one, an address space of its own for each, and an ACCESSIBLE
/// size that differs from the fitted device's own (a 128-Mbit part with only its low 16 data
/// lines wired is 8 Mbytes, and the headline number is the wrong one to quote).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryRegion {
    /// The region id, board-scoped (`flash`, `qspi`, `sdram`).
    pub name: String,
    /// `flash` or `ram`. Load-bearing rather than descriptive: what may hold code, and what an
    /// allocator may place data in, are different questions with the same shape.
    pub kind: String,
    /// Where the region appears in the address space; -1 when the board does not state one (the
    /// chip's own fixed XIP window, which is chip truth and not the board's to repeat).
    pub base: i64,
    /// The ACCESSIBLE size in bytes -- what a program may actually use.
    pub size: i64,
    /// The fitted device's own size, when it differs from the accessible size (-1 = the same).
    /// Stated so the difference is a fact rather than a discrepancy someone re-derives.
    pub device_size: i64,
    /// The instance a program must bring up before touching the region (empty when the chip maps
    /// it with no help). An access before that is a bus fault, not a wrong value, so the
    /// precondition belongs in the facts rather than in a comment.
    pub controller: String,
    /// Whether the board runs without the region.
    pub optional: bool,
    /// The controller block's constant this region's `base` is a SECOND statement of (empty when
    /// there is none). A board states where its region appears because that is what every
    /// consumer reads; the block states where the controller puts it. Naming the constant is what
    /// turns the board's citation into a checked fact -- the two are held equal at generation
    /// time rather than trusted to have been copied correctly.
    pub window: String,
    /// The FITTED DEVICE's shape, as named integer facts: what the controller must be told about
    /// the part this board soldered on. Which keys are legal depends on the controller, so the
    /// reader takes any scalar and RESOLUTION enforces the set -- the knowledge of what an SDRAM
    /// is lives with the derivation, not with the parser.
    pub device: Vec<(String, Int)>,
    /// The device's read configurations, in table order (a flash region; empty otherwise).
    pub reads: Vec<MemoryRead>,
}

/// One read configuration of a fitted flash: the command, how many lines each phase uses, and
/// the dummy-cycle count MEASURED at a stated interface clock.
///
/// THE DUMMY COUNT IS THE ONE NUMBER IN THE STRATA THAT NOTHING CAN DERIVE. A dummy phase covers
/// the device's internal latency in TIME, so the number of CYCLES that covers it falls as the
/// interface clock falls -- a part whose datasheet states eight at its own rating reads correctly
/// with six at half that. Neither figure is wrong, and neither is a property of the part alone.
///
/// And a wrong count does not fail: it shifts every byte, so the flash returns plausible garbage.
/// That is why the count here is an ANCHOR rather than an answer -- it is emitted carrying the
/// name of the plan it was measured under, beside the range a driver walks when it asks the part
/// directly. A number that can only be checked against a payload the caller already knows is a
/// number a table must not pretend to know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryRead {
    /// The configuration id, region-scoped (`read`, `fast_read`, `quad_io`).
    pub name: String,
    /// The device command byte.
    pub instruction: i64,
    /// Lines the instruction phase uses. Stated rather than assumed single: a device that takes
    /// its commands on four lines exists, and a configuration named "quad" that still sends its
    /// command on one line is the normal case, not the exception.
    pub instruction_lines: i64,
    /// Lines the address phase uses (1, 2 or 4).
    pub address_lines: i64,
    /// Lines the data phase uses (1, 2 or 4).
    pub data_lines: i64,
    /// The dummy count the part answered to, at `clock_hz`. -1 = not measured.
    pub dummy: i64,
    /// The count the device's own datasheet states at its full rating (-1 = not stated). Kept
    /// beside the measured one because the DIFFERENCE is the fact, not either number.
    pub dummy_datasheet: i64,
    /// The interface clock `dummy` was measured at.
    pub clock_hz: i64,
}

/// One thing that can be READ from an attached board to confirm it is the board an image was
/// built for.
///
/// A chip identity register cannot answer this question. The parts that make a board differ from
/// its siblings are soldered OUTSIDE the die -- a bare board and a fully populated one report the
/// same chip id -- so a check built only on identity passes the case it exists to catch. A
/// discriminator therefore names WHAT CLAIM it reaches, not merely what it reads.
///
/// The rung vocabulary is [`SOURCING_VALIDATION`], reused verbatim rather than restated: a part
/// table's `[sourcing] validation` already grades a read as `identified` (one answered its
/// identity register) or `exercised` (one produced measurements a driver decoded), and that IS the
/// bare-versus-populated distinction. `reads` is the analogue of that table's `evidence`: a rung
/// may not be claimed without saying what read earns it.
///
/// The rung recorded here is a CEILING declared from documents -- the most a successful read of
/// this kind could establish -- and never a result. What an attached board actually answered is an
/// observation, and observations are not board truth.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discriminator {
    /// The discriminator id, board-scoped (`qspi-jedec`).
    pub name: String,
    /// The claim this read reaches: `part`, or `memory:<region>` naming a declared region.
    /// Resolved against the board, so a discriminator cannot confirm something that is not there.
    pub confirms: String,
    /// What is read, in the board's own terms, so the read can be implemented from this row.
    /// Required, because a rung without a named read is a rank claimed for nothing.
    pub reads: String,
    /// The most this read can establish: `identified` or `exercised`. `none` is refused -- a
    /// discriminator that can establish nothing discriminates nothing.
    pub validation: String,
    /// The answer that confirms the claim. An integer rather than free text because the reading is
    /// produced by a different program than the one that declares it, and a number has one
    /// spelling where a formatted string has as many as there are formatters.
    pub expect: i64,
    /// Where the expected answer comes from.
    pub source: String,
}

/// The connector standards a board may name, in the spelling each standard's own document uses.
///
/// A closed set, so a typo refuses rather than becoming a socket nothing can match. Growing it is
/// one line and a document; inventing a name here would let a board claim a socket that does not
/// exist.
///
/// `qwiic` and `stemma-qt` are two vendors' names for the same four-pin part, and both appear
/// because a board's silkscreen carries one of them and not the other. Whether the two are
/// interchangeable is a claim about the two standards rather than about any board, so it is not
/// stated here.
pub const CONNECTOR_STANDARDS: [&str; 4] = ["qwiic", "stemma-qt", "mikrobus", "arduino-uno-v3"];

/// The bus kinds a connector may bring out as a whole group, matching the binding kinds.
pub const CONNECTOR_BUS_SIGNALS: [&str; 3] = ["i2c", "spi", "uart"];

/// One bus a connector brings out whole, named by the binding role that serves it.
///
/// A bus is named ONCE, by its role, rather than pin by pin: the role already states the instance,
/// the pins and the mux function, and a socket that restated them would hold a second copy of a
/// fact that has a home. The `signal` is the standard's own name for the GROUP, and it is held
/// equal to the bound role's kind, so a socket cannot claim to carry I2C over a serial port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorBus {
    /// The standard's own group name (`i2c`, `spi`, `uart`).
    pub signal: String,
    /// The binding role that serves it.
    pub role: String,
}

/// One line a connector brings out as a single pin, named by the standard's own name for that
/// socket position.
///
/// A socket position is a NAME IN THE STANDARD'S VOCABULARY and the pin behind it is this board's
/// answer, which is the whole content of a connector: `a4` is a position every module built to
/// that standard knows, and which pin it reaches differs on every board that offers one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorPin {
    /// The standard's own name for the socket position (`a4`, `d13`, `int`, `cs`, `sda`).
    pub signal: String,
    /// The board pin behind it.
    pub pin: String,
}

/// A socket a removable module plugs into: which standard it follows, which buses it brings out
/// whole, and which single lines it brings out by name.
///
/// A CONNECTOR IS BOARD TRUTH AND WHAT IS PLUGGED INTO IT IS NOT. The socket is on the schematic,
/// it is identical on every unit, and it does not change; the module on the other end is different
/// on different desks and on different days, so it is supplied per invocation rather than stored
/// here.
///
/// TWO LISTS RATHER THAN ONE, BECAUSE THE TWO KINDS OF LINE ARE SHARED DIFFERENTLY. A board with
/// two sockets of one standard serves both from ONE set of bus roles and gives each its OWN chip
/// select, interrupt and reset. That asymmetry is what a bus role alone cannot describe: which
/// protocol an attached module speaks is a property of the module, and the side-band lines it uses
/// are per socket.
///
/// A LINE IS STATED ONCE. A pin a named bus already brings out must not appear again as a pin row,
/// because the two spellings would be two statements of one wire and nothing would hold them equal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connector {
    /// The connector id, board-scoped (`qt`, `mikrobus-1`).
    pub name: String,
    /// The standard it follows, from [`CONNECTOR_STANDARDS`].
    pub standard: String,
    /// Where the socket's wiring is stated.
    pub source: String,
    /// The buses it brings out whole, in table order.
    pub buses: Vec<ConnectorBus>,
    /// The single lines it brings out by name, in table order.
    pub pins: Vec<ConnectorPin>,
}

/// A board BSP: the bindings, carrier, plans, and identity of one product.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardTable {
    /// The board id (`samd21-xpro`).
    pub board: String,
    /// Who MADE the board (`raspberry-pi`), kebab-cased like every other id here. Board truth:
    /// the same chip ships on boards from a dozen vendors, and a product name is unambiguous only
    /// within one of them -- several vendors ship a board whose name is an ordinary English word.
    pub vendor: String,
    /// The family id, when the board names a bare chip (exclusive with `module`).
    pub family: String,
    /// The module id, when the board carries a module (exclusive with `family`).
    pub module: String,
    /// The exact part id (required with `family`; implied by `module`).
    pub part: String,
    /// The `lamella_wire::board_model` wire code.
    pub board_model: i64,
    /// The memory regions the board fits, in table order. Empty = no record: the part's own
    /// parts row is the whole memory truth.
    pub memory: Vec<MemoryRegion>,
    /// The DEFAULT carrier -- the wire a bare deploy reaches for. Kept as a single record because
    /// every emitter reads it, and the default pair is what they derive from; the full set lives
    /// in `carriers`. Empty `kind` means the board declares no wire at all (a bare chip).
    pub carrier: Carrier,
    /// Every declared carrier, each paired to a plan. A board that writes the singular
    /// `[carrier]` section gets exactly one row here, defaulted and paired to the default plan --
    /// which is what makes the two spellings mean the same thing during the migration.
    pub carriers: Vec<Carrier>,
    /// The board's own bindings (module bindings are inherited at resolution).
    pub bindings: Vec<Binding>,
    /// The named clock plans; exactly one `default = true`.
    pub plans: Vec<Plan>,
    /// On-board control lines (LEDs, buttons), source-cited.
    pub devices: Vec<ControlPin>,
    /// What can be read from an attached board to confirm it is the one an image assumed, in
    /// table order. Empty means the board declares none, and a reconciliation against it can
    /// confirm nothing -- which is a statement the verdict makes rather than one it hides.
    pub discriminators: Vec<Discriminator>,
    /// The sockets a removable module plugs into, in table order. Empty means the board offers
    /// none that this file states.
    pub connectors: Vec<Connector>,
}

impl BoardTable {
    /// The board's default plan (validated present by `parse`).
    #[must_use]
    pub fn default_plan(&self) -> Option<&Plan> {
        self.plans.iter().find(|p| p.default)
    }

    /// The size of the region a program's code is XIP'd from -- the board's own flash region that
    /// the chip maps with no bring-up. 0 when the board fits none, in which case the part's parts
    /// row is the memory truth. An external flash the board must configure ITSELF is deliberately
    /// not this: a linker script does not lay a program into a region that is not there at reset.
    #[must_use]
    pub fn xip_flash(&self) -> i64 {
        self.memory
            .iter()
            .find(|region| region.kind == "flash" && region.controller.is_empty())
            .map_or(0, |region| region.size)
    }
}

impl MemoryRegion {
    /// The fitted device's size -- its own when it differs from what the board can reach,
    /// otherwise the accessible size.
    #[must_use]
    pub fn fitted_size(&self) -> i64 {
        if self.device_size >= 0 { self.device_size } else { self.size }
    }

    /// A named device fact's value, when the region states one.
    #[must_use]
    pub fn fact(&self, key: &str) -> Option<i64> {
        self.device.iter().find(|(k, _)| k == key).map(|(_, v)| v.value)
    }
}

impl BoardTable {

    /// Every DISTINCT plan some carrier runs under, in table order -- the clock-tree twin of
    /// [`Self::carrier_points`]. A board's clock block is a property of an operating point a
    /// wire actually runs at, so a plan no carrier names emits nothing, and a board whose
    /// two wires name two plans states both. Unlike `carrier_points` this does NOT require a
    /// baud: a native-usb carrier has no wire rate at all, and its whole contribution to the
    /// emission is the operating point it names.
    #[must_use]
    pub fn carrier_plans(&self) -> Vec<&Plan> {
        let mut out: Vec<&Plan> = Vec::new();
        for carrier in &self.carriers {
            let plan = if carrier.plan.is_empty() {
                self.default_plan()
            } else {
                self.plans.iter().find(|p| p.name == carrier.plan)
            };
            if let Some(plan) = plan {
                if !out.iter().any(|seen| seen.name == plan.name) {
                    out.push(plan);
                }
            }
        }
        out
    }

    /// Every carrier whose wire rides `role`, each paired with the plan it runs under -- the
    /// plan it names, or the board's default when it names none (the singular-section reading).
    /// A divisor is a property of a (carrier, plan) PAIR, not of a board, so a board with two
    /// wires derives two -- and the `<rate>_<PLAN>` const suffix every arm already spells keeps
    /// them apart without a naming change. Order is table order, so a board's emissions follow
    /// the order its rows are written in.
    #[must_use]
    pub fn carrier_points(&self, role: &str) -> Vec<(&Carrier, &Plan)> {
        self.carriers
            .iter()
            .filter(|carrier| carrier.role == role && carrier.baud > 0)
            .filter_map(|carrier| {
                let plan = if carrier.plan.is_empty() {
                    self.default_plan()
                } else {
                    self.plans.iter().find(|p| p.name == carrier.plan)
                };
                plan.map(|plan| (carrier, plan))
            })
            .collect()
    }
}


/// One bus a part can be reached over. The register MAP is shared between a part's buses; the
/// ADDRESS TRANSFORM is not, so each bus carries its own as a NAMED DISPATCH rather than an
/// expression (a part that declared only a bus kind would produce an SPI path wrong by 0x80,
/// and wrong only on writes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceBus {
    /// The bus id (the `[buses.*]` key).
    pub name: String,
    /// The bus kind (`i2c`, `spi`).
    pub kind: String,
    /// The named transform applied to a register address on a READ.
    pub register_read_transform: String,
    /// The named transform applied to a register address on a WRITE.
    pub register_write_transform: String,
    /// The named read protocol the bus follows.
    pub read_protocol: String,
    /// The supported bus modes, in table order (SPI mode numbers; empty when not applicable).
    pub modes: Vec<Int>,
}

/// One address strap: which pin contributes which address bit, and the address each tie yields.
/// The part states the model; a CARRIER fixes the strap. A default here would be a guess that
/// compiles, so the strap is deliberately never defaulted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressStrap {
    /// The part's pin name (`SDO`).
    pub pin: String,
    /// The address bit this pin contributes.
    pub bit: i64,
    /// The address when the pin is tied low.
    pub low: Int,
    /// The address when the pin is tied high.
    pub high: Int,
}

/// A part's address model: a base plus the straps that move it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceAddress {
    /// The bus the address belongs to.
    pub bus: String,
    /// The base address with every strap tied low.
    pub base: Int,
    /// The straps, in table order.
    pub straps: Vec<AddressStrap>,
}

/// A part's identity register and the SET of values that register may answer. A one-value check
/// rejects a genuine engineering sample, and a rejected part reads as "no sensor there" -- so
/// the accepted values are a set, and every part must state one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// The register that answers the identity.
    pub reg: Int,
    /// The access width in bits.
    pub width: u32,
    /// Every accepted value, in table order.
    pub values: Vec<Int>,
    /// Why this part has NO identity register, when it has none. Empty for a part that does.
    ///
    /// NOT EVERY PART CAN BE IDENTIFIED, and a model that assumed otherwise could not describe
    /// parts that exist. A humidity sensor whose whole interface is a measurement request and a
    /// data fetch carries no readable id at all -- there is no register to read, so the question
    /// has no answer rather than an answer nobody has looked up.
    ///
    /// STATED RATHER THAN OMITTED, exactly as a reserved pin names its owner. An absence expressed
    /// as a missing section is indistinguishable from an unfinished table, and the two call for
    /// opposite responses. A part with no identity says so, and says why, so that a reader can
    /// tell a fact from a gap.
    ///
    /// A part with no identity can never reach the `identified` validation rung -- there is
    /// nothing to answer -- so anything wanting confidence in it must exercise it instead.
    pub absent: String,
}

/// One device register. `reg` is an OPERAND written on the wire, never an offset added to an
/// instance base -- the distinction is load-bearing, because a chip block's `offset` composes as
/// `base + *_OFF` and a device register composed that way would emit and be silently wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceRegister {
    /// The register name (the `[registers.*]` key).
    pub name: String,
    /// The register address written on the wire.
    pub reg: Int,
    /// The width in bits. NOT restricted to a machine access width the way a chip block's is:
    /// a device's multi-byte quantity is read as one burst from a starting register, so a
    /// 20-bit left-justified reading is a single 24-bit register here.
    pub width: u32,
    /// The declared access (`read-only`, `write-only`, `read-write`).
    pub access: String,
    /// The declared fields, in table order.
    pub fields: Vec<Field>,
}

/// A named encoding table (`[enums.*]`): member name -> code. A member REPLACES an inherited
/// enum whole rather than merging into it -- two family members can give one register's codes
/// different meanings, and a half-inherited encoding is wrong for one of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceEnum {
    /// The encoding name (the `[enums.*]` key).
    pub name: String,
    /// The members, in table order.
    pub members: Vec<(String, Int)>,
}

/// One trimming-parameter read: which register, what width, what signedness, and how the value
/// is packed when it does not occupy whole registers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalibrationRead {
    /// The parameter name (`dig_T1`).
    pub name: String,
    /// The starting register.
    pub reg: Int,
    /// The value width in bits (12 for a nibble-packed parameter).
    pub width: u32,
    /// Whether the value is signed. Not uniform within a record, and getting it backwards
    /// yields a driver that compiles, runs, and reports a plausible wrong answer.
    pub signed: bool,
    /// The named packing form, when the value does not occupy whole registers.
    pub packing: String,
}

/// A part's calibration record. Trimming parameters are READ from the part rather than being
/// constants, so what is described is the READ; `form` stays a NAMED DISPATCH into hand-written
/// per-language arithmetic (generating a struct reader is in scope, generating the compensation
/// math is not).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceCalibration {
    /// The record name (the `[calibration.*]` key).
    pub name: String,
    /// The conversion form -- selects which hand-written formula consumes the parameters.
    pub form: String,
    /// The multi-byte byte order (`little`).
    pub byte_order: String,
    /// The named scale of the compensated output.
    pub output_scale: String,
    /// Records whose result this one consumes, in table order.
    pub depends_on: Vec<String>,
    /// The described reads, in table order.
    pub reads: Vec<CalibrationRead>,
}

/// One declarative step of a part sequence. The vocabulary is the table's, and the description
/// is transport agnostic: one step list describes an I2C transaction, an SPI one, and a call
/// through a host import alike.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceStep {
    /// The step verb (`write-field`, `poll-field-until`, `burst-read`).
    pub step: String,
    /// The register the step touches (field steps).
    pub register: String,
    /// The field within that register (field steps).
    pub field: String,
    /// The literal the step writes or waits for.
    pub value: Option<Int>,
    /// Whether a poll is bounded, so an absent or wedged part surfaces as an error not a hang.
    pub bounded: bool,
    /// The register a burst starts at (burst steps).
    pub from: String,
    /// The table reference stating the burst length (burst steps) -- RESOLVED at emission.
    pub length_from: String,
}

/// The values `[sourcing] facts` may take, weakest last.
pub const SOURCING_FACTS: [&str; 2] = ["primary", "secondary"];

/// The values `[sourcing] validation` may take, weakest first.
pub const SOURCING_VALIDATION: [&str; 3] = ["none", "identified", "exercised"];

/// Where a part's facts came from, and what a physical part has been made to do.
///
/// TWO AXES RATHER THAN ONE LADDER, because they answer different questions and a part can be
/// strong on one and absent on the other. Two members of one family can differ on where their
/// facts came from while agreeing that neither has been made to answer, and no single rank could
/// order that pair without calling one of the two shortfalls the smaller.
///
/// Stated per MEMBER and never on the family base. A base's sentences have different provenance
/// depending on which member reads them -- the BMx280 base is the BME280's own datasheet and the
/// BMP280's compatibility section -- so any tier written there would be false for one member
/// whichever value it took.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSourcing {
    /// `primary`: the part's own datasheet. `secondary`: a primary vendor statement about this
    /// part carried by another document, which `derived_from` names.
    pub facts: String,
    /// The sibling part whose document carries the facts. Required by `secondary`, and resolved
    /// against the family so it cannot name a part that is not there.
    pub derived_from: String,
    /// `none`: no physical part of this type has been made to answer. `identified`: one answered
    /// its identity register, against a negative control. `exercised`: one produced measurements
    /// a driver decoded.
    pub validation: String,
    /// What was OBSERVED to earn a validation above `none`: what the part did, not the equipment
    /// that made it do so. Required above `none`, so a rank cannot be claimed without saying for
    /// what.
    pub evidence: String,
}

/// One part table: `kind = "device"` (an emitted part) or `"device-base"` (an authoring
/// convenience a member names with `base`, never emitted on its own).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceTable {
    /// The part family id.
    pub family: String,
    /// The part id (empty on a base).
    pub part: String,
    /// The base this member inherits from. AUTHORING ONLY: nothing downstream reads an
    /// inherited value, because a value that is only ever inherited is pinned in no emitted
    /// artifact and so cannot be checked for drift between languages.
    pub base: String,
    /// Whether this table is a base rather than a part.
    pub is_base: bool,
    /// The buses the part answers on, in table order.
    pub buses: Vec<DeviceBus>,
    /// The address model, when stated.
    pub address: Option<DeviceAddress>,
    /// The identity register and its accepted values, when stated.
    pub identity: Option<DeviceIdentity>,
    /// How well this part is sourced and how far it has been validated. Required of every
    /// emitted part; refused on a base.
    pub sourcing: Option<DeviceSourcing>,
    /// The registers, in table order.
    pub registers: Vec<DeviceRegister>,
    /// The named encodings, in table order.
    pub enums: Vec<DeviceEnum>,
    /// The measurement burst length in bytes (-1 = unstated).
    pub burst_length: i64,
    /// The calibration records, in table order.
    pub calibrations: Vec<DeviceCalibration>,
    /// The named sequences, in table order.
    pub sequences: Vec<(String, Vec<DeviceStep>)>,
}

impl DeviceTable {
    /// The bus named `name`, when declared.
    #[must_use]
    pub fn bus(&self, name: &str) -> Option<&DeviceBus> {
        self.buses.iter().find(|b| b.name == name)
    }

    /// The register named `name`, when declared.
    #[must_use]
    pub fn register(&self, name: &str) -> Option<&DeviceRegister> {
        self.registers.iter().find(|r| r.name == name)
    }

    /// The named encoding, when declared.
    #[must_use]
    pub fn enumeration(&self, name: &str) -> Option<&DeviceEnum> {
        self.enums.iter().find(|e| e.name == name)
    }
}

/// One parsed v2 strata file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Strata {
    /// A block layout.
    Block(BlockTable),
    /// The instance map.
    Instances(InstancesTable),
    /// The pin map.
    Pins(PinsTable),
    /// The parts table.
    Parts(PartsTable),
    /// A module CSP.
    Module(ModuleTable),
    /// A board BSP.
    Board(BoardTable),
    /// A part table from the `parts/` tree.
    Device(DeviceTable),
}


/// Splits `PA22` into (port letter lowercased, index), the bank-less spellings
/// `GP<n>` (RP-family) / `GPIO<n>` (ESP-family) into ('g', n), and the nRF spelling
/// `P<port>.<pin>` (`P0.08`) into (port digit, index) -- port groups resolve as
/// `port<char>` instance rows (`porta`, `port0`) in every derivation. None for anything else.
#[must_use]
pub fn split_pin(pin: &str) -> Option<(char, u32)> {
    if let Some(digits) = pin.strip_prefix("GPIO") {
        return digits.parse::<u32>().ok().map(|index| ('g', index));
    }
    if let Some(digits) = pin.strip_prefix("GP") {
        return digits.parse::<u32>().ok().map(|index| ('g', index));
    }
    let rest = pin.strip_prefix('P')?;
    if let Some((port_digits, index)) = rest.split_once('.') {
        let mut chars = port_digits.chars();
        let port = chars.next()?;
        if chars.next().is_some() || !port.is_ascii_digit() {
            return None;
        }
        return index.parse::<u32>().ok().map(|index| (port, index));
    }
    let mut chars = rest.chars();
    let port = chars.next()?.to_ascii_lowercase();
    if !port.is_ascii_alphabetic() {
        return None;
    }
    let index: String = chars.collect();
    let index = index.parse::<u32>().ok()?;
    Some((port, index))
}

fn upper_snake(text: &str) -> String {
    text.chars()
        .map(|c| if c == '-' || c == '.' { '_' } else { c.to_ascii_uppercase() })
        .collect()
}

fn snake(text: &str) -> String {
    text.chars()
        .map(|c| if c == '-' || c == '.' { '_' } else { c.to_ascii_lowercase() })
        .collect()
}


enum Item {
    /// `[name]`
    Section(String),
    /// `[[name]]`
    ArraySection(String),
    /// `key = value`
    KeyValue(String, RawValue),
}

/// Scans a v2 file into (line, item) pairs with the v0 value grammar and comment rules.
fn scan(text: &str) -> Result<Vec<(usize, Item)>, String> {
    let mut items = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((index, physical)) = lines.next() {
        let line_number = index + 1;
        let mut logical = strip_comment(physical).trim_end().to_string();
        while brackets_open(&logical) {
            let Some((_, next)) = lines.next() else {
                return Err(err(line_number, "unterminated multiline value"));
            };
            logical.push(' ');
            logical.push_str(strip_comment(next).trim());
        }
        let line = logical.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            items.push((line_number, Item::ArraySection(header.to_string())));
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            items.push((line_number, Item::Section(header.to_string())));
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            return Err(err(line_number, &format!("expected 'key = value', got '{line}'")));
        };
        let mut cursor = ValueCursor { text: rest.trim().as_bytes(), at: 0, line: line_number };
        items.push((line_number, Item::KeyValue(key.trim().to_string(), cursor.parse()?)));
    }
    Ok(items)
}

fn as_str(line: usize, key: &str, value: &RawValue) -> Result<String, String> {
    match value {
        RawValue::Str(s) => Ok(s.clone()),
        _ => Err(err(line, &format!("'{key}' must be a string"))),
    }
}

fn as_int(line: usize, key: &str, value: &RawValue) -> Result<Int, String> {
    match value {
        RawValue::Int(i) => Ok(*i),
        _ => Err(err(line, &format!("'{key}' must be an integer"))),
    }
}

fn as_pin_ref(line: usize, key: &str, value: &RawValue) -> Result<PinRef, String> {
    let RawValue::Inline(entries) = value else {
        return Err(err(line, &format!("'{key}' must be an inline table {{ pin = ..., pad = ... }}")));
    };
    let mut pin = PinRef { pin: String::new(), pad: -1, soft: false };
    for (k, v) in entries {
        match (k.as_str(), v) {
            ("pin", RawValue::Str(s)) => pin.pin = s.clone(),
            ("pad", RawValue::Int(i)) => pin.pad = i.value,
            ("soft", RawValue::Str(s)) => pin.soft = s == "true",
            ("soft", RawValue::Int(i)) => pin.soft = i.value != 0,
            (other, _) => return Err(err(line, &format!("unexpected pin key '{other}' in '{key}'"))),
        }
    }
    if pin.pin.is_empty() {
        return Err(err(line, &format!("'{key}' names no pin")));
    }
    Ok(pin)
}



/// Parses one v2 strata file, dispatching on `[table] kind`. LOUD with line numbers; every
/// file kind has a closed key set.
pub fn parse(text: &str) -> Result<Strata, String> {
    let text = rewrite_bools(text);
    let items = scan(&text)?;

    let mut at = 0usize;
    match items.first() {
        Some((_, Item::Section(name))) if name == "table" => at += 1,
        Some((line, _)) => return Err(err(*line, "a v2 strata file starts with [table]")),
        None => return Err("empty strata file".to_string()),
    }
    let mut header: Vec<(usize, String, RawValue)> = Vec::new();
    while at < items.len() {
        match &items[at] {
            (line, Item::KeyValue(k, v)) => header.push((*line, k.clone(), v.clone())),
            _ => break,
        }
        at += 1;
    }
    let kind = header
        .iter()
        .find(|(_, k, _)| k == "kind")
        .ok_or_else(|| "[table] must declare kind".to_string())
        .and_then(|(line, k, v)| as_str(*line, k, v))?;

    let rest = &items[at..];
    match kind.as_str() {
        "block" => build_block(&header, rest).map(Strata::Block),
        "instances" => build_instances(&header, rest).map(Strata::Instances),
        "pins" => build_pins(&header, rest).map(Strata::Pins),
        "parts" => build_parts(&header, rest).map(Strata::Parts),
        "module" => build_module(&header, rest).map(Strata::Module),
        "board" => build_board(&header, rest).map(Strata::Board),
        "device" => build_device(&header, rest, false).map(Strata::Device),
        "device-base" => build_device(&header, rest, true).map(Strata::Device),
        other => Err(format!("unknown strata kind '{other}'")),
    }
}

fn rewrite_bools(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let content_len = strip_comment(line).len();
        let (content, comment) = line.split_at(content_len);
        let rewritten = content.replace("= true", "= 1").replace("= false", "= 0");
        out.push_str(&rewritten);
        out.push_str(comment);
        out.push('\n');
    }
    out
}

fn header_str(header: &[(usize, String, RawValue)], key: &str) -> Result<String, String> {
    header
        .iter()
        .find(|(_, k, _)| k == key)
        .ok_or_else(|| format!("[table] must declare {key}"))
        .and_then(|(line, k, v)| as_str(*line, k, v))
}

fn header_str_opt(header: &[(usize, String, RawValue)], key: &str) -> Result<String, String> {
    match header.iter().find(|(_, k, _)| k == key) {
        Some((line, k, v)) => as_str(*line, k, v),
        None => Ok(String::new()),
    }
}

fn header_reject_unknown(
    header: &[(usize, String, RawValue)],
    allowed: &[&str],
) -> Result<(), String> {
    for (line, key, _) in header {
        if !allowed.contains(&key.as_str()) {
            return Err(err(*line, &format!("unexpected [table] key '{key}' for this kind -- the key set is closed")));
        }
    }
    Ok(())
}

fn build_block(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
) -> Result<BlockTable, String> {
    header_reject_unknown(header, &["kind", "family", "block", "mode", "sources", "notes"])?;
    let mut table = BlockTable {
        family: header_str(header, "family")?,
        block: header_str(header, "block")?,
        mode: header_str_opt(header, "mode")?,
        ..BlockTable::default()
    };

    enum At {
        None,
        Register(usize),
        Constants,
        Parameters,
        Sequence(usize),
        Facts,
        /// A documentation sub-table (`[facts.notes]`) -- parsed and ignored (the v0 rule).
        Ignored,
        Channel,
        Calibration(usize),
    }
    let mut at = At::None;
    for (line, item) in items {
        match item {
            Item::Section(name) => {
                at = match name.as_str() {
                    "constants" => At::Constants,
                    "parameters" => At::Parameters,
                    "facts" => At::Facts,
                    "facts.notes" => At::Ignored,
                    _ => {
                        if let Some(reg) = name.strip_prefix("registers.") {
                            table.registers.push(BlockRegister {
                                name: reg.to_string(),
                                offset: Int { value: 0, hex: false },
                                width: 0,
                                fields: Vec::new(),
                            });
                            At::Register(table.registers.len() - 1)
                        } else if let Some(record) = name.strip_prefix("calibration.") {
                            table.calibrations.push(Calibration {
                                name: record.to_string(),
                                form: String::new(),
                                coefficients: Vec::new(),
                            });
                            At::Calibration(table.calibrations.len() - 1)
                        } else {
                            return Err(err(*line, &format!("unexpected section '[{name}]' in a block file -- the key set is closed")));
                        }
                    }
                };
            }
            Item::ArraySection(name) => {
                if name == "channels" {
                    table.channels.push(Channel { index: 0, source: String::new() });
                    at = At::Channel;
                    continue;
                }
                let Some(seq) = name.strip_prefix("sequences.") else {
                    return Err(err(*line, &format!("unexpected [[{name}]] in a block file -- the key set is closed")));
                };
                if table.sequences.last().is_none_or(|(n, _)| n != seq) {
                    table.sequences.push((seq.to_string(), Vec::new()));
                }
                table.sequences.last_mut().expect("just ensured").1.push(Step {
                    op: String::new(),
                    reg: String::new(),
                    value: None,
                    mask: None,
                    want: None,
                    want_below: None,
                    note: None,
                });
                at = At::Sequence(table.sequences.len() - 1);
            }
            Item::KeyValue(key, value) => match &at {
                At::None => return Err(err(*line, "key outside any section")),
                At::Constants => table.constants.push((key.clone(), as_int(*line, key, value)?)),
                At::Parameters => table.parameters.push((key.clone(), as_str(*line, key, value)?)),
                At::Ignored => {}
                At::Facts => match value {
                    RawValue::Int(int) => table.facts.push((key.clone(), Fact::Int(*int))),
                    RawValue::Float(text) => table.facts.push((key.clone(), Fact::Float(text.clone()))),
                    _ => return Err(err(*line, "fact must be a number")),
                },
                At::Channel => {
                    let channel = table.channels.last_mut().expect("open channel");
                    match (key.as_str(), value) {
                        ("index", RawValue::Int(int)) => channel.index = int.value,
                        ("source", RawValue::Str(s)) => channel.source = s.clone(),
                        ("enable", RawValue::Str(_)) => {}
                        (other, _) => {
                            return Err(err(*line, &format!("unexpected channel key '{other}'")));
                        }
                    }
                }
                At::Calibration(index) => {
                    let record = &mut table.calibrations[*index];
                    match (key.as_str(), value) {
                        ("form", RawValue::Str(s)) => record.form = s.clone(),
                        ("notes", RawValue::Str(_)) => {}
                        (coefficient, RawValue::Int(int)) => {
                            record.coefficients.push((coefficient.to_string(), *int));
                        }
                        (other, _) => {
                            return Err(err(*line, &format!("calibration '{other}' must be an integer coefficient")));
                        }
                    }
                }
                At::Register(index) => {
                    let register = &mut table.registers[*index];
                    match (key.as_str(), value) {
                        ("offset", v) => register.offset = as_int(*line, key, v)?,
                        ("width", v) => {
                            let width = as_int(*line, key, v)?.value;
                            if ![8, 16, 32].contains(&width) {
                                return Err(err(*line, "width must be 8, 16, or 32"));
                            }
                            register.width = width as u32;
                        }
                        ("notes", _) => {}
                        ("fields", RawValue::Inline(entries)) => {
                            for (field_name, spec) in entries {
                                let RawValue::Array(parts) = spec else {
                                    return Err(err(*line, "field spec must be [lsb, width]"));
                                };
                                let ints: Vec<i64> = parts
                                    .iter()
                                    .filter_map(|p| match p {
                                        RawValue::Int(i) => Some(i.value),
                                        _ => None,
                                    })
                                    .collect();
                                if ints.len() != 2 {
                                    return Err(err(*line, "field spec must be [lsb, width]"));
                                }
                                register.fields.push(Field {
                                    name: field_name.clone(),
                                    lsb: ints[0] as u32,
                                    width: ints[1] as u32,
                                });
                            }
                        }
                        (other, _) => {
                            return Err(err(*line, &format!("unexpected register key '{other}'")));
                        }
                    }
                }
                At::Sequence(index) => {
                    let step = table.sequences[*index].1.last_mut().expect("step exists");
                    match (key.as_str(), value) {
                        ("op", RawValue::Str(s)) => step.op = s.clone(),
                        ("reg", RawValue::Str(s)) => step.reg = s.clone(),
                        ("value", RawValue::Int(i)) => step.value = Some(StepValue::Literal(*i)),
                        ("value", RawValue::Str(s)) => match s.strip_prefix('$') {
                            Some(p) => step.value = Some(StepValue::Parameter(p.to_string())),
                            None => return Err(err(*line, "string step value must be a $parameter")),
                        },
                        ("mask", RawValue::Int(i)) => step.mask = Some(*i),
                        ("want", RawValue::Int(i)) => step.want = Some(*i),
                        ("want_below", RawValue::Int(i)) => step.want_below = Some(*i),
                        ("note", RawValue::Str(s)) => step.note = Some(s.clone()),
                        (other, _) => return Err(err(*line, &format!("unexpected step key '{other}'"))),
                    }
                }
            },
        }
    }
    for register in &table.registers {
        if register.width == 0 {
            return Err(format!(
                "block {}/{}: register {} declares no width -- widths are data, not prose",
                table.block, table.mode, register.name
            ));
        }
    }
    Ok(table)
}

/// An enum member key may be written quoted (`"0" = 500`) when the code is numeric, since a bare
/// digit is not a key. The quotes are spelling, not name, and the scanner hands them through.
fn unquote(key: &str) -> &str {
    key.strip_prefix('"').and_then(|k| k.strip_suffix('"')).unwrap_or(key)
}

/// Refuses a non-integer number ANYWHERE in a device table, at any array/inline-table depth.
/// A part catalogue's premise is the no-float tier: one `0.5` in one row would force a float
/// into every emitted language, so a fact that is not an integer must be restated as one (the
/// standby codes are microseconds for exactly this reason) or expressed as a named dispatch.
fn reject_floats(line: usize, key: &str, value: &RawValue) -> Result<(), String> {
    match value {
        RawValue::Float(text) => Err(err(
            line,
            &format!("'{key}' is the non-integer {text} -- a part table carries integers and named dispatch only, never a float"),
        )),
        RawValue::Array(items) => items.iter().try_for_each(|item| reject_floats(line, key, item)),
        RawValue::Inline(entries) => {
            entries.iter().try_for_each(|(k, v)| reject_floats(line, &format!("{key}.{k}"), v))
        }
        _ => Ok(()),
    }
}

fn as_int_array(line: usize, key: &str, value: &RawValue) -> Result<Vec<Int>, String> {
    let RawValue::Array(items) = value else {
        return Err(err(line, &format!("'{key}' must be an array of integers")));
    };
    items
        .iter()
        .map(|item| match item {
            RawValue::Int(int) => Ok(*int),
            _ => Err(err(line, &format!("'{key}' must be an array of integers"))),
        })
        .collect()
}

fn as_str_array(line: usize, key: &str, value: &RawValue) -> Result<Vec<String>, String> {
    let RawValue::Array(items) = value else {
        return Err(err(line, &format!("'{key}' must be an array of strings")));
    };
    items
        .iter()
        .map(|item| match item {
            RawValue::Str(text) => Ok(text.clone()),
            _ => Err(err(line, &format!("'{key}' must be an array of strings"))),
        })
        .collect()
}

fn build_fields(line: usize, value: &RawValue) -> Result<Vec<Field>, String> {
    let RawValue::Inline(entries) = value else {
        return Err(err(line, "'fields' must be an inline table of { name = [lsb, width] }"));
    };
    let mut fields = Vec::new();
    for (name, spec) in entries {
        let ints = as_int_array(line, "field spec", spec)?;
        if ints.len() != 2 {
            return Err(err(line, "field spec must be [lsb, width]"));
        }
        fields.push(Field { name: name.clone(), lsb: ints[0].value as u32, width: ints[1].value as u32 });
    }
    Ok(fields)
}

/// Parses a `parts/` device table: `kind = "device"` (a part) or `"device-base"` (the authoring
/// convenience its members name with `base`). Every section has a closed key set, and two
/// refusals are load-bearing rather than defensive:
///
/// - a `[registers.*]` may not spell `offset`. A chip block's `offset` composes as
///   `base + *_OFF`; a device register is an OPERAND written on the wire. The same name would
///   invite one wrong line that compiles.
/// - no fact may be a non-integer number, at any depth (see [`reject_floats`]).
///
/// Prose keys (`notes` everywhere, `[measurement] resolution_bits`) are accepted and carried no
/// further: a facts table emits values, and a sentence is not one.
fn build_device(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
    is_base: bool,
) -> Result<DeviceTable, String> {
    header_reject_unknown(header, &["kind", "family", "part", "base", "sources", "notes"])?;
    let mut table = DeviceTable {
        family: header_str(header, "family")?,
        part: header_str_opt(header, "part")?,
        base: header_str_opt(header, "base")?,
        is_base,
        burst_length: -1,
        ..DeviceTable::default()
    };
    if is_base && !table.base.is_empty() {
        return Err(format!(
            "part base '{}' names a base of its own -- inheritance is one level, so that a member \
             states its deltas against exactly one table",
            table.family
        ));
    }
    if !is_base && table.part.is_empty() {
        return Err("a device table must declare its part id".to_string());
    }

    enum At {
        None,
        Bus(usize),
        Address,
        Strap(usize),
        Identity,
        Sourcing,
        Register(usize),
        Enum(usize),
        Measurement,
        Calibration(usize),
        CalibrationRead(usize),
        Sequence(usize),
    }
    let mut at = At::None;
    for (line, item) in items {
        match item {
            Item::Section(name) => {
                at = match name.as_str() {
                    "address" => {
                        table.address = Some(DeviceAddress::default());
                        At::Address
                    }
                    "identity" => {
                        table.identity.get_or_insert_with(DeviceIdentity::default);
                        At::Identity
                    }
                    "sourcing" => {
                        table.sourcing.get_or_insert_with(DeviceSourcing::default);
                        At::Sourcing
                    }
                    "measurement" => At::Measurement,
                    _ => {
                        if let Some(bus) = name.strip_prefix("buses.") {
                            table.buses.push(DeviceBus { name: bus.to_string(), ..DeviceBus::default() });
                            At::Bus(table.buses.len() - 1)
                        } else if let Some(reg) = name.strip_prefix("registers.") {
                            table.registers.push(DeviceRegister {
                                name: reg.to_string(),
                                ..DeviceRegister::default()
                            });
                            At::Register(table.registers.len() - 1)
                        } else if let Some(id) = name.strip_prefix("enums.") {
                            table.enums.push(DeviceEnum { name: id.to_string(), ..DeviceEnum::default() });
                            At::Enum(table.enums.len() - 1)
                        } else if let Some(record) = name.strip_prefix("calibration.") {
                            table.calibrations.push(DeviceCalibration {
                                name: record.to_string(),
                                ..DeviceCalibration::default()
                            });
                            At::Calibration(table.calibrations.len() - 1)
                        } else {
                            return Err(err(*line, &format!("unexpected section '[{name}]' in a part file -- the key set is closed")));
                        }
                    }
                };
            }
            Item::ArraySection(name) => {
                if name == "address.straps" {
                    let Some(address) = table.address.as_mut() else {
                        return Err(err(*line, "a strap must follow the [address] it moves"));
                    };
                    address.straps.push(AddressStrap::default());
                    at = At::Strap(address.straps.len() - 1);
                    continue;
                }
                if let Some(record) = name.strip_prefix("calibration.").and_then(|r| r.strip_suffix(".read")) {
                    let Some(index) = table.calibrations.iter().position(|c| c.name == record) else {
                        return Err(err(*line, &format!("a read must follow its [calibration.{record}] record")));
                    };
                    table.calibrations[index].reads.push(CalibrationRead::default());
                    at = At::CalibrationRead(index);
                    continue;
                }
                let Some(sequence) = name.strip_prefix("sequences.") else {
                    return Err(err(*line, &format!("unexpected [[{name}]] in a part file -- the key set is closed")));
                };
                if table.sequences.last().is_none_or(|(n, _)| n != sequence) {
                    table.sequences.push((sequence.to_string(), Vec::new()));
                }
                table.sequences.last_mut().expect("just ensured").1.push(DeviceStep::default());
                at = At::Sequence(table.sequences.len() - 1);
            }
            Item::KeyValue(key, value) => {
                reject_floats(*line, key, value)?;
                match &at {
                    At::None => return Err(err(*line, "key outside any section")),
                    At::Bus(index) => {
                        let bus = &mut table.buses[*index];
                        match (key.as_str(), value) {
                            ("kind", v) => bus.kind = as_str(*line, key, v)?,
                            ("register_read_transform", v) => bus.register_read_transform = as_str(*line, key, v)?,
                            ("register_write_transform", v) => bus.register_write_transform = as_str(*line, key, v)?,
                            ("read_protocol", v) => bus.read_protocol = as_str(*line, key, v)?,
                            ("modes", v) => bus.modes = as_int_array(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected bus key '{other}'"))),
                        }
                    }
                    At::Address => {
                        let address = table.address.as_mut().expect("open address");
                        match (key.as_str(), value) {
                            ("bus", v) => address.bus = as_str(*line, key, v)?,
                            ("base", v) => address.base = as_int(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected address key '{other}'"))),
                        }
                    }
                    At::Strap(index) => {
                        let strap = &mut table.address.as_mut().expect("open address").straps[*index];
                        match (key.as_str(), value) {
                            ("pin", v) => strap.pin = as_str(*line, key, v)?,
                            ("bit", v) => strap.bit = as_int(*line, key, v)?.value,
                            ("low", v) => strap.low = as_int(*line, key, v)?,
                            ("high", v) => strap.high = as_int(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected strap key '{other}'"))),
                        }
                    }
                    At::Identity => {
                        let identity = table.identity.as_mut().expect("open identity");
                        match (key.as_str(), value) {
                            ("reg", v) => identity.reg = as_int(*line, key, v)?,
                            ("width", v) => identity.width = as_int(*line, key, v)?.value as u32,
                            ("values", v) => identity.values = as_int_array(*line, key, v)?,
                            ("absent", v) => identity.absent = as_str(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected identity key '{other}'"))),
                        }
                    }
                    At::Sourcing => {
                        let sourcing = table.sourcing.as_mut().expect("open sourcing");
                        match (key.as_str(), value) {
                            ("facts", v) => sourcing.facts = as_str(*line, key, v)?,
                            ("derived_from", v) => sourcing.derived_from = as_str(*line, key, v)?,
                            ("validation", v) => sourcing.validation = as_str(*line, key, v)?,
                            ("evidence", v) => sourcing.evidence = as_str(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected sourcing key '{other}'"))),
                        }
                    }
                    At::Register(index) => {
                        let register = &mut table.registers[*index];
                        match (key.as_str(), value) {
                            ("offset", _) => {
                                return Err(err(*line, &format!(
                                    "register '{}' spells 'offset' -- a device register is an OPERAND written on the wire, so the key is 'reg'; 'offset' means base-relative and would compose as base + offset",
                                    register.name
                                )));
                            }
                            ("reg", v) => register.reg = as_int(*line, key, v)?,
                            ("width", v) => register.width = as_int(*line, key, v)?.value as u32,
                            ("access", v) => register.access = as_str(*line, key, v)?,
                            ("fields", v) => register.fields = build_fields(*line, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected register key '{other}'"))),
                        }
                    }
                    At::Enum(index) => {
                        let encoding = &mut table.enums[*index];
                        match (key.as_str(), value) {
                            ("notes", _) => {}
                            (member, RawValue::Int(int)) => {
                                encoding.members.push((unquote(member).to_string(), *int));
                            }
                            (other, _) => {
                                return Err(err(*line, &format!("enum member '{other}' must be an integer code")));
                            }
                        }
                    }
                    At::Measurement => match (key.as_str(), value) {
                        ("burst_length", v) => table.burst_length = as_int(*line, key, v)?.value,
                        ("resolution_bits", _) | ("notes", _) => {}
                        (other, _) => return Err(err(*line, &format!("unexpected measurement key '{other}'"))),
                    },
                    At::Calibration(index) => {
                        let record = &mut table.calibrations[*index];
                        match (key.as_str(), value) {
                            ("form", v) => record.form = as_str(*line, key, v)?,
                            ("byte_order", v) => record.byte_order = as_str(*line, key, v)?,
                            ("output_scale", v) => record.output_scale = as_str(*line, key, v)?,
                            ("depends_on", v) => record.depends_on = as_str_array(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected calibration key '{other}'"))),
                        }
                    }
                    At::CalibrationRead(index) => {
                        let read = table.calibrations[*index].reads.last_mut().expect("open read");
                        match (key.as_str(), value) {
                            ("name", v) => read.name = as_str(*line, key, v)?,
                            ("reg", v) => read.reg = as_int(*line, key, v)?,
                            ("width", v) => read.width = as_int(*line, key, v)?.value as u32,
                            ("signed", v) => read.signed = as_int(*line, key, v)?.value != 0,
                            ("packing", v) => read.packing = as_str(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected calibration-read key '{other}'"))),
                        }
                    }
                    At::Sequence(index) => {
                        let step = table.sequences[*index].1.last_mut().expect("open step");
                        match (key.as_str(), value) {
                            ("step", v) => step.step = as_str(*line, key, v)?,
                            ("register", v) => step.register = as_str(*line, key, v)?,
                            ("field", v) => step.field = as_str(*line, key, v)?,
                            ("value", v) => step.value = Some(as_int(*line, key, v)?),
                            ("bounded", v) => step.bounded = as_int(*line, key, v)?.value != 0,
                            ("from", v) => step.from = as_str(*line, key, v)?,
                            ("length_from", v) => step.length_from = as_str(*line, key, v)?,
                            ("notes", _) => {}
                            (other, _) => return Err(err(*line, &format!("unexpected step key '{other}'"))),
                        }
                    }
                }
            }
        }
    }

    for register in &table.registers {
        if register.width == 0 {
            return Err(format!(
                "part {}: register {} declares no width -- widths are data, not prose",
                if table.part.is_empty() { &table.family } else { &table.part },
                register.name
            ));
        }
    }
    if is_base && table.sourcing.is_some() {
        return Err(format!(
            "part base '{}' states [sourcing]. A base's facts are sourced differently for each member that inherits them, so the tier belongs to the member",
            table.family
        ));
    }
    Ok(table)
}

fn build_instances(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
) -> Result<InstancesTable, String> {
    header_reject_unknown(header, &["kind", "family", "record", "sources", "notes"])?;
    let mut table = InstancesTable { family: header_str(header, "family")?, ..Default::default() };
    for (line, key, value) in header {
        if key == "record" {
            let RawValue::Array(parts) = value else {
                return Err(err(*line, "record must be an array of field names"));
            };
            for part in parts {
                match part {
                    RawValue::Str(s) => table.record.push(s.clone()),
                    _ => return Err(err(*line, "record entries must be strings")),
                }
            }
        }
    }
    if table.record.first().map(String::as_str) != Some("base") {
        return Err("instances record must start with 'base'".to_string());
    }

    type PendingRow = (usize, String, String, String, Vec<(String, i64)>);
    let mut current: Option<PendingRow> = None;
    let finish = |current: &mut Option<PendingRow>,
                      table: &mut InstancesTable|
     -> Result<(), String> {
        if let Some((line, name, block, port, values)) = current.take() {
            if name.is_empty() || block.is_empty() {
                return Err(err(line, "an instance row needs name and block"));
            }
            if name == NO_CONTROLLER {
                return Err(err(
                    line,
                    &format!(
                        "'{NO_CONTROLLER}' is reserved: it is what a pin row states when no \
                         controller reaches the cell, so no instance may be placed under it"
                    ),
                ));
            }
            let mut ordered = Vec::new();
            for field in &table.record {
                let Some((_, v)) = values.iter().find(|(k, _)| k == field) else {
                    return Err(err(line, &format!("instance '{name}' misses record field '{field}'")));
                };
                ordered.push(*v);
            }
            if values.len() != table.record.len() {
                let extra: Vec<&String> = values
                    .iter()
                    .map(|(k, _)| k)
                    .filter(|k| !table.record.contains(k))
                    .collect();
                return Err(err(line, &format!("instance '{name}' carries off-record fields {extra:?}")));
            }
            table.rows.push(InstanceRow { name, block, values: ordered, port });
        }
        Ok(())
    };

    for (line, item) in items {
        match item {
            Item::ArraySection(name) if name == "instances" => {
                finish(&mut current, &mut table)?;
                current = Some((*line, String::new(), String::new(), String::new(), Vec::new()));
            }
            Item::Section(name) | Item::ArraySection(name) => {
                return Err(err(*line, &format!("unexpected section '{name}' in instances -- the key set is closed")));
            }
            Item::KeyValue(key, value) => {
                let Some((_, name, block, port, values)) = current.as_mut() else {
                    return Err(err(*line, "key outside [[instances]]"));
                };
                match (key.as_str(), value) {
                    ("name", RawValue::Str(s)) => *name = s.clone(),
                    ("block", RawValue::Str(s)) => *block = s.clone(),
                    ("port", RawValue::Str(s)) => *port = s.clone(),
                    (field, RawValue::Int(i)) => values.push((field.to_string(), i.value)),
                    (other, _) => return Err(err(*line, &format!("unexpected instance key '{other}'"))),
                }
            }
        }
    }
    finish(&mut current, &mut table)?;
    let mut claimed: Vec<(char, &str)> = Vec::new();
    for row in &table.rows {
        if row.port.is_empty() {
            continue;
        }
        let Some(port) = row.port_char() else {
            return Err(format!(
                "instances: '{}' declares port '{}' -- a port group is ONE character, the one a \
                 pin name carries ('C' for PC10, '0' for P0.13)",
                row.name, row.port
            ));
        };
        if let Some((_, first)) = claimed.iter().find(|(c, _)| *c == port) {
            return Err(format!(
                "instances: '{}' and '{}' both declare port '{port}' -- a pin's port group resolves \
                 to one instance, so two claims make which one it is a matter of table order",
                first, row.name
            ));
        }
        claimed.push((port, &row.name));
    }
    Ok(table)
}

fn build_pins(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
) -> Result<PinsTable, String> {
    header_reject_unknown(header, &["kind", "family", "sources", "notes"])?;
    let mut table = PinsTable { family: header_str(header, "family")?, ..Default::default() };
    let mut open = false;
    for (line, item) in items {
        match item {
            Item::ArraySection(name) if name == "pins" => {
                table.rows.push(PinRow {
                    pin: String::new(),
                    function: String::new(),
                    instance: String::new(),
                    signal: String::new(),
                    source: String::new(),
                });
                open = true;
            }
            Item::Section(name) | Item::ArraySection(name) => {
                return Err(err(*line, &format!("unexpected section '{name}' in pins -- the key set is closed")));
            }
            Item::KeyValue(key, value) => {
                if !open {
                    return Err(err(*line, "key outside [[pins]]"));
                }
                let row = table.rows.last_mut().expect("open row");
                match (key.as_str(), value) {
                    ("pin", RawValue::Str(s)) => row.pin = s.clone(),
                    ("function", RawValue::Str(s)) => row.function = s.clone(),
                    ("instance", RawValue::Str(s)) => row.instance = s.clone(),
                    ("signal", RawValue::Str(s)) => row.signal = s.clone(),
                    ("source", RawValue::Str(s)) => row.source = s.clone(),
                    (other, _) => return Err(err(*line, &format!("unexpected pin key '{other}'"))),
                }
            }
        }
    }
    for row in &table.rows {
        if split_pin(&row.pin).is_none() {
            return Err(format!("pins: '{}' is not a P<port><index> pin name", row.pin));
        }
        validate_pin_row(row)?;
    }
    Ok(table)
}

/// Hold a pin row to ONE OF THE TWO SHAPES, so that an absence and an omission stop looking alike.
///
/// Every field a shape uses is required, which is not tidiness: an empty string is legal in
/// neither field precisely so that it cannot stand in for a fact. An unrouted cell says what it is
/// out loud, carrying the citation that is the only check it can ever be given.
fn validate_pin_row(row: &PinRow) -> Result<(), String> {
    let pin = &row.pin;
    if row.function.is_empty() {
        return Err(format!(
            "pins: '{pin}' states no function -- a row is a (pin, function) CELL, and a row with no \
             function names no cell (an omitted function silently matches no binding forever)"
        ));
    }
    if row.instance.is_empty() {
        return Err(format!(
            "pins: '{pin}' function {} states no instance. If a controller reaches this cell, name \
             it; if NOTHING does, say so as a value -- instance = \"{NO_CONTROLLER}\" with the \
             `source` you read it from. An omitted field is not the same fact as an absent \
             controller, and only one of the two is checkable.",
            row.function
        ));
    }
    if row.source.is_empty() {
        return Err(format!(
            "pins: '{pin}' function {} states no source -- the pin map is evidence-only, and a row \
             with no citation cannot be told from one grown out of a binding",
            row.function
        ));
    }
    if row.is_unrouted() {
        if !row.signal.is_empty() {
            return Err(format!(
                "pins: '{pin}' function {} reaches no controller but names signal '{}' -- a cell \
                 with no controller has no signal, so the two together state a routing the row \
                 also denies",
                row.function, row.signal
            ));
        }
    } else if row.signal.is_empty() {
        return Err(format!(
            "pins: '{pin}' function {} routes to {} but names no signal -- which of the \
             instance's signals the cell carries is the half of the row a binding is checked \
             against",
            row.function, row.instance
        ));
    }
    Ok(())
}

/// Check a part's stated instruction-set profile AGAINST ITSELF.
///
/// The profile is stated as a name plus the two consequences a code generator must respect, and
/// the name is what decides both -- so this derives them from the name and refuses a row whose
/// three fields disagree. A name alone would not be checkable, and consequences alone would not
/// say which silicon they came from; stating both is what makes the row wrong out loud instead of
/// wrong in a register allocator.
///
/// The whole profile is optional -- families that have never stated one keep working -- but it is
/// all-or-nothing: a row that names an ISA must carry its consequences, because a half-stated
/// profile is the shape a reader would most confidently misread.
/// The core architectures this check knows. CLOSED on purpose, exactly as the RISC-V profile check
/// is: an unrecognized name is refused rather than carried, because a name nobody checked is a
/// name that can be wrong forever. Growing this list is the deliberate act of admitting a core.
const KNOWN_CORE_ARCHITECTURES: &[&str] = &[
    "cortex-m0",
    "cortex-m0plus",
    "cortex-m3",
    "cortex-m4",
    "cortex-m4f",
    "cortex-m7",
    "cortex-m7f",
    "cortex-m33",
    "hazard3",
];

/// A part's processor sockets: how many, what each can run, and whether they share memory.
///
/// WHAT THIS IS FOR, stated because the fields look redundant beside `isa` and are not. A target
/// is a selection an IMAGE makes over a part's cores, and one part admits several -- so the facts
/// layer has to say what there is to select FROM. Two sockets of the same architecture sharing
/// memory can be driven as one threaded system; two of different architectures need two images
/// whatever the memory looks like.
fn validate_part_cores(row: &PartRow) -> Result<(), String> {
    let part = &row.part;
    if row.cores.is_empty() {
        if row.cores_share_memory.is_some() {
            return Err(format!(
                "parts: {part} states cores_share_memory without stating `cores` -- there is nothing for the sockets to share"
            ));
        }
        return Ok(());
    }
    if row.cores.len() < 2 {
        return Err(format!(
            "parts: {part} states one core socket. `cores` describes a part with MORE THAN ONE, and the isa fields already describe a single core -- stating one here says the same thing in a second place"
        ));
    }
    if row.cores_share_memory.is_none() {
        return Err(format!(
            "parts: {part} states {} core sockets without stating cores_share_memory. That is half the test for whether they can be driven as ONE threaded system, and a default would decide it silently",
            row.cores.len()
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for (socket, archs) in &row.cores {
        if socket.is_empty() {
            return Err(format!("parts: {part} has a core socket with no name"));
        }
        if seen.contains(&socket.as_str()) {
            return Err(format!("parts: {part} names core socket '{socket}' twice"));
        }
        seen.push(socket);
        if archs.is_empty() {
            return Err(format!(
                "parts: {part} core socket '{socket}' lists no architecture -- a socket that runs nothing is not a socket"
            ));
        }
        for arch in archs {
            if arch != &arch.to_ascii_lowercase() {
                return Err(format!(
                    "parts: {part} core architecture '{arch}' must be lowercase"
                ));
            }
            if !KNOWN_CORE_ARCHITECTURES.contains(&arch.as_str()) {
                return Err(format!(
                    "parts: {part} core socket '{socket}' names architecture '{arch}', which this check does not know -- an unknown name is refused rather than carried, because nothing downstream would ever disagree with it"
                ));
            }
        }
        let mut unique = archs.clone();
        unique.sort();
        unique.dedup();
        if unique.len() != archs.len() {
            return Err(format!(
                "parts: {part} core socket '{socket}' lists an architecture twice"
            ));
        }
    }
    Ok(())
}

impl PartRow {
    /// Whether the part's sockets can be driven as ONE threaded system, and why not when they
    /// cannot. `None` on a single-core part, which is not asked the question.
    ///
    /// THE TEST IS TWO CONDITIONS, NOT ONE: the sockets must reach one
    /// address space AND run the same architecture. Shared memory alone is not enough -- a part in
    /// some silicon lets one socket run Arm while the other runs RISC-V over the same memory, with
    /// atomics interoperating across the two, and its own datasheet says that arrangement "requires
    /// two separate program images".
    #[must_use]
    pub fn cores_combinable(&self) -> Option<Result<(), String>> {
        if self.cores.is_empty() {
            return None;
        }
        if self.cores_share_memory != Some(true) {
            return Some(Err("the sockets do not reach one address space".to_string()));
        }
        let common = self.cores[0]
            .1
            .iter()
            .find(|arch| self.cores.iter().all(|(_, archs)| archs.contains(arch)));
        match common {
            Some(_) => Some(Ok(())),
            None => Some(Err(
                "no single architecture is admissible in every socket, so one image cannot run on all of them"
                    .to_string(),
            )),
        }
    }
}

fn validate_part_isa(row: &PartRow) -> Result<(), String> {
    let part = &row.part;
    let stated = !row.isa.is_empty()
        || row.isa_registers != 0
        || !row.isa_muldiv.is_empty()
        || !row.isa_float.is_empty();
    if !stated {
        return Ok(());
    }
    if row.isa.is_empty() {
        return Err(format!(
            "parts: {part} states an ISA consequence (isa_registers/isa_muldiv/isa_float) without \
             naming the profile in `isa` -- the name is what the others are checked against"
        ));
    }
    if row.isa != row.isa.to_ascii_lowercase() {
        return Err(format!("parts: {part} isa '{}' must be lowercase", row.isa));
    }
    let (single, multi) = match row.isa.split_once('_') {
        Some((head, tail)) => (head, tail.split('_').collect::<Vec<_>>()),
        None => (row.isa.as_str(), Vec::new()),
    };
    let Some(rest) = single.strip_prefix("rv32").or_else(|| single.strip_prefix("rv64")) else {
        return Err(format!(
            "parts: {part} isa '{}' is not a RISC-V profile name (rv32.../rv64...) -- this check \
             derives the register count and the multiply path from the base letter and the \
             extension letters, so an unrecognized shape is refused rather than half-checked",
            row.isa
        ));
    };
    for name in &multi {
        if !matches!(*name, "zmmul" | "zicsr" | "zifencei") {
            return Err(format!(
                "parts: {part} isa '{}' names multi-letter extension '{name}', which this check \
                 does not know -- an unknown extension is refused rather than ignored, because \
                 ignoring it would silently accept a profile nobody has checked",
                row.isa
            ));
        }
    }
    let mut letters = rest.chars();
    let base = letters.next().ok_or_else(|| {
        format!("parts: {part} isa '{}' names no base integer set (expected e or i)", row.isa)
    })?;
    let expected_registers = match base {
        'e' => 16,
        'i' => 32,
        other => {
            return Err(format!(
                "parts: {part} isa '{}' has base '{other}', which is neither 'e' (16 registers) \
                 nor 'i' (32)",
                row.isa
            ))
        }
    };
    if row.isa_registers != expected_registers {
        return Err(format!(
            "parts: {part} isa '{}' is a '{base}' base, so it has {expected_registers} integer \
             registers, but isa_registers = {} -- the letter and the count must agree",
            row.isa, row.isa_registers
        ));
    }
    let has_m = letters.clone().any(|c| c == 'm');
    let has_zmmul = multi.contains(&"zmmul");
    let expected_muldiv = if has_m {
        "hardware"
    } else if has_zmmul {
        "multiply-only"
    } else {
        "soft"
    };
    if row.isa_muldiv != expected_muldiv {
        let reason = if has_m {
            "carries an 'm' extension (multiply and divide)"
        } else if has_zmmul {
            "carries 'zmmul' (multiply WITHOUT divide) and no 'm'"
        } else {
            "carries neither 'm' nor 'zmmul'"
        };
        return Err(format!(
            "parts: {part} isa '{}' {reason}, so isa_muldiv must be '{expected_muldiv}', not '{}'",
            row.isa, row.isa_muldiv
        ));
    }

    let has_f = letters.clone().any(|c| c == 'f');
    let has_d = letters.clone().any(|c| c == 'd');
    if letters.clone().any(|c| c == 'q') {
        return Err(format!(
            "parts: {part} isa '{}' names quad-precision float, which this check has no value for \
             -- refused rather than silently recorded as double",
            row.isa
        ));
    }
    if has_d && !has_f {
        return Err(format!(
            "parts: {part} isa '{}' names 'd' without 'f', but the double-precision extension is \
             defined as an extension of the single-precision one -- the name is malformed",
            row.isa
        ));
    }
    let expected_float = if has_d {
        "double"
    } else if has_f {
        "single"
    } else {
        "soft"
    };
    if row.isa_float != expected_float {
        let reason = if has_d {
            "carries 'd' (double, which subsumes single)"
        } else if has_f {
            "carries 'f' (single precision)"
        } else {
            "carries no floating-point extension"
        };
        return Err(format!(
            "parts: {part} isa '{}' {reason}, so isa_float must be '{expected_float}', not '{}'",
            row.isa, row.isa_float
        ));
    }
    Ok(())
}

fn build_parts(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
) -> Result<PartsTable, String> {
    header_reject_unknown(header, &["kind", "family", "sources", "notes"])?;
    let mut table = PartsTable { family: header_str(header, "family")?, ..Default::default() };
    let mut open = false;
    for (line, item) in items {
        match item {
            Item::ArraySection(name) if name == "parts" => {
                table.rows.push(PartRow {
                    part: String::new(),
                    package: String::new(),
                    flash: 0,
                    ram: 0,
                    pins: Vec::new(),
                    cores: Vec::new(),
                    cores_share_memory: None,
                    reserved: Vec::new(),
                    isa: String::new(),
                    isa_registers: 0,
                    isa_muldiv: String::new(),
                    isa_float: String::new(),
                });
                open = true;
            }
            Item::Section(name) | Item::ArraySection(name) => {
                return Err(err(*line, &format!("unexpected section '{name}' in parts -- the key set is closed")));
            }
            Item::KeyValue(key, value) => {
                if !open {
                    return Err(err(*line, "key outside [[parts]]"));
                }
                let row = table.rows.last_mut().expect("open row");
                match (key.as_str(), value) {
                    ("part", RawValue::Str(s)) => row.part = s.clone(),
                    ("package", RawValue::Str(s)) => row.package = s.clone(),
                    ("flash", RawValue::Int(i)) => row.flash = i.value,
                    ("ram", RawValue::Int(i)) => row.ram = i.value,
                    ("isa", RawValue::Str(s)) => row.isa = s.clone(),
                    ("isa_registers", RawValue::Int(i)) => row.isa_registers = i.value,
                    ("isa_muldiv", RawValue::Str(s)) => row.isa_muldiv = s.clone(),
                    ("isa_float", RawValue::Str(s)) => row.isa_float = s.clone(),
                    ("isa_source", RawValue::Str(_)) => {}
                    ("source", RawValue::Str(_)) => {}
                    ("pins", RawValue::Array(parts)) => {
                        for part in parts {
                            match part {
                                RawValue::Str(s) => row.pins.push(s.clone()),
                                _ => return Err(err(*line, "pins entries must be strings")),
                            }
                        }
                    }
                    ("cores", RawValue::Inline(entries)) => {
                        for (socket, value) in entries {
                            let RawValue::Array(items) = value else {
                                return Err(err(
                                    *line,
                                    &format!("core socket '{socket}' must list its architectures as an array, even when there is only one"),
                                ));
                            };
                            let mut archs = Vec::new();
                            for item in items {
                                match item {
                                    RawValue::Str(s) => archs.push(s.clone()),
                                    _ => {
                                        return Err(err(
                                            *line,
                                            &format!("core socket '{socket}': an architecture must be a string"),
                                        ));
                                    }
                                }
                            }
                            row.cores.push((socket.clone(), archs));
                        }
                    }
                    ("cores_share_memory", RawValue::Int(i)) => {
                        row.cores_share_memory = Some(i.value != 0);
                    }
                    ("reserved", RawValue::Inline(entries)) => {
                        for (pin, owner) in entries {
                            match owner {
                                RawValue::Str(s) if !s.is_empty() => {
                                    row.reserved.push((pin.clone(), s.clone()));
                                }
                                RawValue::Str(_) => {
                                    return Err(err(
                                        *line,
                                        &format!("reserved pin '{pin}' states an empty owner -- name the peripheral that holds it, or drop the entry"),
                                    ));
                                }
                                _ => {
                                    return Err(err(
                                        *line,
                                        &format!("reserved pin '{pin}' must name its owner as a string"),
                                    ));
                                }
                            }
                        }
                    }
                    (other, _) => return Err(err(*line, &format!("unexpected part key '{other}'"))),
                }
            }
        }
    }
    for row in &table.rows {
        validate_part_isa(row)?;
        validate_part_cores(row)?;
        for (pin, owner) in &row.reserved {
            if !row.has_pin(pin) {
                return Err(format!(
                    "parts: {} reserves {pin} for {owner}, but {pin} is not in its pin list -- a reservation is a statement about a pin the part HAS, and one outside the list could never be checked against anything",
                    row.part
                ));
            }
        }
    }
    Ok(table)
}

fn build_binding(line: usize) -> Binding {
    let _ = line;
    Binding { gclk_gen: -1, reference_uv: -1, ..Default::default() }
}

fn binding_key(
    line: usize,
    binding: &mut Binding,
    key: &str,
    value: &RawValue,
) -> Result<(), String> {
    match (key, value) {
        ("role", RawValue::Str(s)) => binding.role = s.clone(),
        ("kind", RawValue::Str(s)) => binding.kind = s.clone(),
        ("instance", RawValue::Str(s)) => binding.instance = s.clone(),
        ("function", RawValue::Str(s)) => binding.function = s.clone(),
        ("gclk_gen", RawValue::Int(i)) => binding.gclk_gen = i.value,
        ("reference_uv", RawValue::Int(i)) => binding.reference_uv = i.value,
        ("source", RawValue::Str(_)) => {}
        (signal, v @ RawValue::Inline(_)) => {
            binding.pins.push((signal.to_string(), as_pin_ref(line, signal, v)?));
        }
        (other, _) => return Err(err(line, &format!("unexpected binding key '{other}'"))),
    }
    Ok(())
}

fn control_pin_key(
    line: usize,
    pin: &mut ControlPin,
    key: &str,
    value: &RawValue,
) -> Result<(), String> {
    match (key, value) {
        ("name", RawValue::Str(s)) => pin.name = s.clone(),
        ("pin", RawValue::Str(s)) => pin.pin = s.clone(),
        ("active", RawValue::Str(s)) => pin.active = s.clone(),
        ("kind", RawValue::Str(s)) => pin.kind = s.clone(),
        ("role", RawValue::Str(s)) => pin.role = s.clone(),
        ("address", RawValue::Int(i)) => pin.address = i.value,
        ("source", RawValue::Str(_)) => {}
        (other, _) => return Err(err(line, &format!("unexpected control-pin key '{other}'"))),
    }
    Ok(())
}

fn build_module(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
) -> Result<ModuleTable, String> {
    header_reject_unknown(header, &["kind", "module", "family", "part", "sources", "notes"])?;
    let mut table = ModuleTable {
        module: header_str(header, "module")?,
        family: header_str(header, "family")?,
        part: header_str(header, "part")?,
        ..Default::default()
    };
    enum At {
        None,
        Binding,
        ModulePin,
    }
    let mut at = At::None;
    for (line, item) in items {
        match item {
            Item::ArraySection(name) if name == "bindings" => {
                table.bindings.push(build_binding(*line));
                at = At::Binding;
            }
            Item::ArraySection(name) if name == "module_pins" => {
                table.module_pins.push(ControlPin::default());
                at = At::ModulePin;
            }
            Item::Section(name) | Item::ArraySection(name) => {
                return Err(err(*line, &format!("unexpected section '{name}' in a module file -- the key set is closed")));
            }
            Item::KeyValue(key, value) => match at {
                At::None => return Err(err(*line, "key outside any section")),
                At::Binding => {
                    binding_key(*line, table.bindings.last_mut().expect("open"), key, value)?;
                }
                At::ModulePin => {
                    control_pin_key(*line, table.module_pins.last_mut().expect("open"), key, value)?;
                }
            },
        }
    }
    Ok(table)
}

fn build_board(
    header: &[(usize, String, RawValue)],
    items: &[(usize, Item)],
) -> Result<BoardTable, String> {
    header_reject_unknown(
        header,
        &["kind", "board", "vendor", "family", "module", "part", "board_model", "sources", "notes"],
    )?;
    let mut table = BoardTable {
        board: header_str(header, "board")?,
        vendor: header_str(header, "vendor")?,
        family: header_str_opt(header, "family")?,
        module: header_str_opt(header, "module")?,
        part: header_str_opt(header, "part")?,
        ..Default::default()
    };
    for (line, key, value) in header {
        if key == "board_model" {
            table.board_model = as_int(*line, key, value)?.value;
        }
    }
    if table.family.is_empty() == table.module.is_empty() {
        return Err("a board names exactly one of family/module".to_string());
    }
    if pascal(&table.vendor).is_empty() || !pascal(&table.vendor).chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "board '{}' states vendor '{}', which does not make a namespace segment -- kebab-case ASCII, as `raspberry-pi` does",
            table.board, table.vendor
        ));
    }
    if !table.family.is_empty() && table.part.is_empty() {
        return Err("a family board must name its exact part".to_string());
    }

    enum At {
        None,
        Carrier,
        Memory,
        /// A named region's fitted-device record (`[memory.<region>.device]`).
        MemoryDevice(usize),
        /// One read configuration of a named region (`[[memory.<region>.reads]]`).
        MemoryRead(usize),
        Binding,
        Plan,
        Device,
        Discriminator,
        Connector,
        /// A named connector's bus list (`[[connectors.<name>.buses]]`).
        ConnectorBus(usize),
        /// A named connector's pin list (`[[connectors.<name>.pins]]`).
        ConnectorPin(usize),
    }
    let mut at = At::None;
    let mut memory_source_cited = false;
    for (line, item) in items {
        match item {
            Item::Section(name) if name == "carrier" => {
                table.carriers.push(Carrier { default: true, ..Carrier::default() });
                at = At::Carrier;
            }
            Item::ArraySection(name) if name == "carriers" => {
                table.carriers.push(Carrier::default());
                at = At::Carrier;
            }
            Item::Section(name) | Item::ArraySection(name) if name == "memory" => {
                table
                    .memory
                    .push(MemoryRegion { base: -1, device_size: -1, ..MemoryRegion::default() });
                at = At::Memory;
            }
            Item::ArraySection(name) if name == "bindings" => {
                table.bindings.push(build_binding(*line));
                at = At::Binding;
            }
            Item::ArraySection(name) if name == "plans" => {
                table.plans.push(Plan::default());
                at = At::Plan;
            }
            Item::ArraySection(name) if name == "devices" => {
                table.devices.push(ControlPin::default());
                at = At::Device;
            }
            Item::ArraySection(name) if name == "discriminators" => {
                table.discriminators.push(Discriminator { expect: -1, ..Discriminator::default() });
                at = At::Discriminator;
            }
            Item::ArraySection(name) if name == "connectors" => {
                table.connectors.push(Connector::default());
                at = At::Connector;
            }
            Item::ArraySection(name)
                if name.starts_with("connectors.") && name.matches('.').count() == 2 =>
            {
                let mut parts = name.splitn(3, '.');
                let (_, connector, leaf) =
                    (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let Some(index) = table.connectors.iter().position(|c| c.name == connector) else {
                    return Err(err(*line, &format!(
                        "'[[{name}]]' names connector '{connector}', which no [[connectors]] row above it declares"
                    )));
                };
                at = match leaf {
                    "buses" => {
                        table.connectors[index].buses.push(ConnectorBus::default());
                        At::ConnectorBus(index)
                    }
                    "pins" => {
                        table.connectors[index].pins.push(ConnectorPin::default());
                        At::ConnectorPin(index)
                    }
                    _ => {
                        return Err(err(*line, &format!(
                            "'{name}' is not a connector record -- a connector takes [[connectors.<name>.buses]] and [[connectors.<name>.pins]]"
                        )));
                    }
                };
            }
            Item::Section(name) | Item::ArraySection(name)
                if name.starts_with("memory.") && name.matches('.').count() == 2 =>
            {
                let mut parts = name.splitn(3, '.');
                let (_, region, leaf) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let Some(index) = table.memory.iter().position(|r| r.name == region) else {
                    return Err(err(*line, &format!(
                        "'[{name}]' names region '{region}', which no [[memory]] row above it declares"
                    )));
                };
                at = match (leaf, item) {
                    ("device", Item::Section(_)) => At::MemoryDevice(index),
                    ("reads", Item::ArraySection(_)) => {
                        table.memory[index].reads.push(MemoryRead {
                            instruction: -1,
                            instruction_lines: -1,
                            address_lines: -1,
                            data_lines: -1,
                            dummy: -1,
                            dummy_datasheet: -1,
                            clock_hz: -1,
                            ..MemoryRead::default()
                        });
                        At::MemoryRead(index)
                    }
                    _ => {
                        return Err(err(*line, &format!(
                            "'{name}' is not a memory record -- a region takes [memory.<region>.device] and [[memory.<region>.reads]]"
                        )));
                    }
                };
            }
            Item::Section(name) | Item::ArraySection(name) => {
                return Err(err(*line, &format!("unexpected section '{name}' in a board file -- the key set is closed")));
            }
            Item::KeyValue(key, value) => match at {
                At::None => return Err(err(*line, "key outside any section")),
                At::Carrier => {
                    let Some(carrier) = table.carriers.last_mut() else {
                        return Err(err(*line, "carrier key outside a carrier section"));
                    };
                    match (key.as_str(), value) {
                        ("kind", RawValue::Str(s)) => carrier.kind = s.clone(),
                        ("usb_vid", RawValue::Int(i)) => carrier.usb_vid = i.value,
                        ("usb_pid", RawValue::Int(i)) => carrier.usb_pid = i.value,
                        ("role", RawValue::Str(s)) => carrier.role = s.clone(),
                        ("baud", RawValue::Int(i)) => carrier.baud = i.value,
                        ("plan", RawValue::Str(s)) => carrier.plan = s.clone(),
                        ("default", RawValue::Int(i)) => carrier.default = i.value != 0,
                        (other, _) => return Err(err(*line, &format!("unexpected carrier key '{other}'"))),
                    }
                }
                At::Memory => match (key.as_str(), value) {
                    ("flash", RawValue::Int(i)) => {
                        let region = table.memory.last_mut().expect("open region");
                        region.name = "flash".to_string();
                        region.kind = "flash".to_string();
                        region.size = i.value;
                    }
                    ("name", RawValue::Str(s)) => table.memory.last_mut().expect("open region").name = s.clone(),
                    ("kind", RawValue::Str(s)) => table.memory.last_mut().expect("open region").kind = s.clone(),
                    ("base", RawValue::Int(i)) => table.memory.last_mut().expect("open region").base = i.value,
                    ("size", RawValue::Int(i)) => table.memory.last_mut().expect("open region").size = i.value,
                    ("device_size", RawValue::Int(i)) => {
                        table.memory.last_mut().expect("open region").device_size = i.value;
                    }
                    ("controller", RawValue::Str(s)) => {
                        table.memory.last_mut().expect("open region").controller = s.clone();
                    }
                    ("optional", RawValue::Int(i)) => {
                        table.memory.last_mut().expect("open region").optional = i.value != 0;
                    }
                    ("window", RawValue::Str(s)) => {
                        table.memory.last_mut().expect("open region").window = s.clone();
                    }
                    ("source", RawValue::Str(_)) => memory_source_cited = true,
                    (other, _) => {
                        return Err(err(
                            *line,
                            &format!("unexpected memory key '{other}' -- a region takes name/kind/base/size/device_size/controller/optional/window/source"),
                        ));
                    }
                },
                At::MemoryDevice(index) => match (key.as_str(), value) {
                    ("part" | "source" | "notes", RawValue::Str(_)) => {}
                    (fact, RawValue::Int(i)) => {
                        let region = &mut table.memory[index];
                        if region.device.iter().any(|(k, _)| k == fact) {
                            return Err(err(*line, &format!(
                                "region '{}' states device fact '{fact}' twice", region.name
                            )));
                        }
                        region.device.push((fact.to_string(), *i));
                    }
                    (fact, _) => {
                        return Err(err(*line, &format!(
                            "device fact '{fact}' must be an integer -- a shape the controller is told is a number, and a word for it is prose"
                        )));
                    }
                },
                At::MemoryRead(index) => {
                    let read = table.memory[index].reads.last_mut().expect("open read");
                    match (key.as_str(), value) {
                        ("name", RawValue::Str(s)) => read.name = s.clone(),
                        ("instruction", RawValue::Int(i)) => read.instruction = i.value,
                        ("instruction_lines", RawValue::Int(i)) => read.instruction_lines = i.value,
                        ("address_lines", RawValue::Int(i)) => read.address_lines = i.value,
                        ("data_lines", RawValue::Int(i)) => read.data_lines = i.value,
                        ("dummy", RawValue::Int(i)) => read.dummy = i.value,
                        ("dummy_datasheet", RawValue::Int(i)) => read.dummy_datasheet = i.value,
                        ("clock_hz", RawValue::Int(i)) => read.clock_hz = i.value,
                        ("source", RawValue::Str(_)) => {}
                        (other, _) => {
                            return Err(err(*line, &format!(
                                "unexpected read key '{other}' -- a read configuration takes name/instruction/instruction_lines/address_lines/data_lines/dummy/dummy_datasheet/clock_hz/source"
                            )));
                        }
                    }
                }
                At::Binding => {
                    binding_key(*line, table.bindings.last_mut().expect("open"), key, value)?;
                }
                At::Plan => {
                    let plan = table.plans.last_mut().expect("open");
                    let pll_chosen = |k: &str| {
                        k.starts_with("pll_")
                            && (k.ends_with("_fbdiv") || k.ends_with("_postdiv1") || k.ends_with("_postdiv2"))
                    };
                    match (key.as_str(), value) {
                        ("name", RawValue::Str(s)) => plan.name = s.clone(),
                        ("default", RawValue::Int(i)) => plan.default = i.value != 0,
                        ("source", RawValue::Str(s)) => plan.source = s.clone(),
                        (rate, RawValue::Int(i)) if rate.ends_with("_hz") || pll_chosen(rate) => {
                            plan.rates.push((rate.to_string(), i.value));
                        }
                        (other, _) => {
                            return Err(err(
                                *line,
                                &format!("unexpected plan key '{other}' (plans state *_hz rates + pll_*_fbdiv/postdiv1/postdiv2 chosen values only; divisor-like keys refuse: generation derives divisors)"),
                            ));
                        }
                    }
                }
                At::Device => {
                    control_pin_key(*line, table.devices.last_mut().expect("open"), key, value)?;
                }
                At::Discriminator => {
                    let row = table.discriminators.last_mut().expect("open");
                    match (key.as_str(), value) {
                        ("name", RawValue::Str(s)) => row.name = s.clone(),
                        ("confirms", RawValue::Str(s)) => row.confirms = s.clone(),
                        ("reads", RawValue::Str(s)) => row.reads = s.clone(),
                        ("validation", RawValue::Str(s)) => row.validation = s.clone(),
                        ("expect", RawValue::Int(i)) => row.expect = i.value,
                        ("source", RawValue::Str(s)) => row.source = s.clone(),
                        (other, _) => {
                            return Err(err(*line, &format!(
                                "unexpected discriminator key '{other}' (a discriminator states name, confirms, reads, validation, expect and source)"
                            )));
                        }
                    }
                }
                At::Connector => {
                    let row = table.connectors.last_mut().expect("open");
                    match (key.as_str(), value) {
                        ("name", RawValue::Str(s)) => row.name = s.clone(),
                        ("standard", RawValue::Str(s)) => row.standard = s.clone(),
                        ("source", RawValue::Str(s)) => row.source = s.clone(),
                        (other, _) => {
                            return Err(err(*line, &format!(
                                "unexpected connector key '{other}' (a connector states name, standard and source; its lines are [[connectors.<name>.buses]] and [[connectors.<name>.pins]] rows)"
                            )));
                        }
                    }
                }
                At::ConnectorBus(index) => {
                    let row = table.connectors[index].buses.last_mut().expect("open bus");
                    match (key.as_str(), value) {
                        ("signal", RawValue::Str(s)) => row.signal = s.clone(),
                        ("role", RawValue::Str(s)) => row.role = s.clone(),
                        (other, _) => {
                            return Err(err(*line, &format!(
                                "unexpected connector bus key '{other}' -- a bus a socket brings out whole states signal and role"
                            )));
                        }
                    }
                }
                At::ConnectorPin(index) => {
                    let row = table.connectors[index].pins.last_mut().expect("open pin");
                    match (key.as_str(), value) {
                        ("signal", RawValue::Str(s)) => row.signal = s.clone(),
                        ("pin", RawValue::Str(s)) => row.pin = s.clone(),
                        (other, _) => {
                            return Err(err(*line, &format!(
                                "unexpected connector pin key '{other}' -- a single line a socket brings out states signal and pin"
                            )));
                        }
                    }
                }
            },
        }
    }
    let defaults = table.plans.iter().filter(|p| p.default).count();
    if !table.plans.is_empty() && defaults != 1 {
        return Err(format!("board {}: exactly one default plan required, found {defaults}", table.board));
    }
    if table.plans.is_empty() && !table.bindings.is_empty() {
        return Err(format!(
            "board {}: bindings need a default clock plan (identity-first boards state neither)",
            table.board
        ));
    }
    if !table.memory.is_empty() && !memory_source_cited {
        return Err(format!("board {}: a memory region must be SOURCE-CITED", table.board));
    }
    for (at, region) in table.memory.iter().enumerate() {
        if region.name.is_empty() {
            return Err(format!("board {}: memory region {at} states no name", table.board));
        }
        if !["flash", "ram"].contains(&region.kind.as_str()) {
            return Err(format!(
                "board {}: memory region '{}' is kind '{}' -- a region holds code or it holds data, so the kinds are flash and ram",
                table.board, region.name, region.kind
            ));
        }
        if region.size <= 0 {
            return Err(format!("board {}: memory region '{}' states no size", table.board, region.name));
        }
        if region.device_size >= 0 && region.device_size < region.size {
            return Err(format!(
                "board {}: memory region '{}' reaches 0x{:X} bytes of a 0x{:X}-byte device -- the accessible size cannot exceed the part",
                table.board, region.name, region.size, region.device_size
            ));
        }
        if table.memory.iter().filter(|other| other.name == region.name).count() > 1 {
            return Err(format!(
                "board {}: two memory regions are named '{}'",
                table.board, region.name
            ));
        }
    }
    for (at, row) in table.discriminators.iter().enumerate() {
        if row.name.is_empty() {
            return Err(format!("board {}: discriminator {at} states no name", table.board));
        }
        if table.discriminators.iter().filter(|other| other.name == row.name).count() > 1 {
            return Err(format!(
                "board {}: two discriminators are named '{}' -- a reading names the one it came from",
                table.board, row.name
            ));
        }
        let confirms_ok = row.confirms == "part"
            || row
                .confirms
                .strip_prefix("memory:")
                .is_some_and(|region| table.memory.iter().any(|m| m.name == region));
        if !confirms_ok {
            return Err(format!(
                "board {}: discriminator '{}' confirms '{}', which is neither 'part' nor 'memory:<region>' naming a region this board declares",
                table.board, row.name, row.confirms
            ));
        }
        if row.reads.is_empty() {
            return Err(format!(
                "board {}: discriminator '{}' states no 'reads' -- a rung may not be claimed without saying what read earns it",
                table.board, row.name
            ));
        }
        if row.validation == "none" || !SOURCING_VALIDATION.contains(&row.validation.as_str()) {
            return Err(format!(
                "board {}: discriminator '{}' states validation '{}' -- a discriminator is 'identified' or 'exercised'",
                table.board, row.name, row.validation
            ));
        }
        if row.expect < 0 {
            return Err(format!(
                "board {}: discriminator '{}' states no 'expect' -- a read with no expected answer confirms nothing",
                table.board, row.name
            ));
        }
        if row.source.is_empty() {
            return Err(format!(
                "board {}: discriminator '{}' is not SOURCE-CITED",
                table.board, row.name
            ));
        }
        if row.validation == "identified" {
            if let Some(region) = row
                .confirms
                .strip_prefix("memory:")
                .and_then(|name| table.memory.iter().find(|m| m.name == name))
            {
                if let Some(identity) = region.fact("identity") {
                    if identity != row.expect {
                        return Err(format!(
                            "board {}: discriminator '{}' expects 0x{:X}, but region '{}' records the identity its fitted device answered as 0x{identity:X} -- one number, two statements, and they disagree",
                            table.board, row.name, row.expect, region.name
                        ));
                    }
                }
            }
        }
    }
    for (at, row) in table.connectors.iter().enumerate() {
        if row.name.is_empty() {
            return Err(format!("board {}: connector {at} states no name", table.board));
        }
        if table.connectors.iter().filter(|other| other.name == row.name).count() > 1 {
            return Err(format!(
                "board {}: two connectors are named '{}' -- a board with two sockets of one standard numbers them, because their side-band lines differ",
                table.board, row.name
            ));
        }
        if !CONNECTOR_STANDARDS.contains(&row.standard.as_str()) {
            return Err(format!(
                "board {}: connector '{}' follows standard '{}', which is not one of {} -- a socket names a standard some document defines",
                table.board,
                row.name,
                row.standard,
                CONNECTOR_STANDARDS.join("/")
            ));
        }
        if row.source.is_empty() {
            return Err(format!(
                "board {}: connector '{}' is not SOURCE-CITED",
                table.board, row.name
            ));
        }
        if row.buses.is_empty() && row.pins.is_empty() {
            return Err(format!(
                "board {}: connector '{}' brings out neither a bus nor a pin -- a socket nothing reaches through is a name, not a fact",
                table.board, row.name
            ));
        }
        for bus in &row.buses {
            if !CONNECTOR_BUS_SIGNALS.contains(&bus.signal.as_str()) {
                return Err(format!(
                    "board {}: connector '{}' brings out signal '{}', which is not one of {} -- a bus brought out whole is named for its kind",
                    table.board,
                    row.name,
                    bus.signal,
                    CONNECTOR_BUS_SIGNALS.join("/")
                ));
            }
            if bus.role.is_empty() {
                return Err(format!(
                    "board {}: connector '{}' brings out '{}' but names no role -- a bus is named once, by the binding that serves it",
                    table.board, row.name, bus.signal
                ));
            }
            if row.buses.iter().filter(|other| other.signal == bus.signal).count() > 1 {
                return Err(format!(
                    "board {}: connector '{}' brings out '{}' twice -- one socket carries one bus of a kind",
                    table.board, row.name, bus.signal
                ));
            }
        }
        for pin in &row.pins {
            if pin.signal.is_empty() || pin.pin.is_empty() {
                return Err(format!(
                    "board {}: connector '{}' has a line stating {} -- a single line states both its socket position and the pin behind it",
                    table.board,
                    row.name,
                    if pin.signal.is_empty() { "no signal" } else { "no pin" }
                ));
            }
            if row.pins.iter().filter(|other| other.signal == pin.signal).count() > 1 {
                return Err(format!(
                    "board {}: connector '{}' names position '{}' twice -- a socket position is one hole",
                    table.board, row.name, pin.signal
                ));
            }
            if row.pins.iter().filter(|other| other.pin == pin.pin).count() > 1 {
                return Err(format!(
                    "board {}: connector '{}' brings {} out at two positions -- one pin reaches one hole",
                    table.board, row.name, pin.pin
                ));
            }
        }
    }
    if let Some(default) = table.carriers.iter().find(|c| c.default) {
        table.carrier = default.clone();
    } else if let Some(only) = table.carriers.first() {
        table.carrier = only.clone();
    }
    Ok(table)
}


/// A family's loaded chip strata plus the modules that extend it.
#[derive(Clone, Debug, Default)]
pub struct FamilySet {
    /// The family id.
    pub family: String,
    /// The block tables, sorted by (block, mode).
    pub blocks: Vec<BlockTable>,
    /// The instance map.
    pub instances: InstancesTable,
    /// The pin map.
    pub pins: PinsTable,
    /// The parts table.
    pub parts: PartsTable,
    /// The modules whose host family this is, sorted by module id.
    pub modules: Vec<ModuleTable>,
}

impl FamilySet {
    /// The block table for (block, mode), when loaded.
    #[must_use]
    pub fn block(&self, block: &str, mode: &str) -> Option<&BlockTable> {
        self.blocks.iter().find(|b| b.block == block && b.mode == mode)
    }

    /// The pin-map row for (pin, function), when present.
    #[must_use]
    pub fn pin_row(&self, pin: &str, function: &str) -> Option<&PinRow> {
        self.pins.rows.iter().find(|r| r.pin == pin && r.function == function)
    }

    /// The module named `module`, when loaded.
    #[must_use]
    pub fn module(&self, module: &str) -> Option<&ModuleTable> {
        self.modules.iter().find(|m| m.module == module)
    }
}

fn read(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Loads `csp/<family>/` (blocks, instances, pins, parts) plus every `csp/*/module.toml`
/// whose host family matches, from the repo root.
pub fn load_family(repo_root: &std::path::Path, family: &str) -> Result<FamilySet, String> {
    let csp = repo_root.join("csp").join(family);
    let mut set = FamilySet { family: family.to_string(), ..Default::default() };

    let blocks_dir = csp.join("blocks");
    let mut block_paths: Vec<_> = match std::fs::read_dir(&blocks_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", blocks_dir.display())),
    };
    block_paths.sort();
    for path in block_paths {
        match parse(&read(&path)?)? {
            Strata::Block(block) if block.family == family => set.blocks.push(block),
            Strata::Block(block) => {
                return Err(format!("{}: block belongs to family '{}'", path.display(), block.family));
            }
            _ => return Err(format!("{}: expected kind = \"block\"", path.display())),
        }
    }

    match parse(&read(&csp.join("instances.toml"))?)? {
        Strata::Instances(t) => set.instances = t,
        _ => return Err("instances.toml: expected kind = \"instances\"".to_string()),
    }
    match parse(&read(&csp.join("pins.toml"))?)? {
        Strata::Pins(t) => set.pins = t,
        _ => return Err("pins.toml: expected kind = \"pins\"".to_string()),
    }
    match parse(&read(&csp.join("parts.toml"))?)? {
        Strata::Parts(t) => set.parts = t,
        _ => return Err("parts.toml: expected kind = \"parts\"".to_string()),
    }

    let csp_root = repo_root.join("csp");
    let mut module_dirs: Vec<_> = std::fs::read_dir(&csp_root)
        .map_err(|e| format!("{}: {e}", csp_root.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("module.toml"))
        .filter(|p| p.is_file())
        .collect();
    module_dirs.sort();
    for path in module_dirs {
        if let Strata::Module(module) = parse(&read(&path)?)? {
            if module.family == family {
                set.modules.push(module);
            }
        } else {
            return Err(format!("{}: expected kind = \"module\"", path.display()));
        }
    }

    validate_family(&set)?;
    Ok(set)
}

fn validate_family(set: &FamilySet) -> Result<(), String> {
    for row in &set.pins.rows {
        if !row.is_unrouted() && set.instances.row(&row.instance).is_none() {
            return Err(format!("pins: '{}' names unknown instance '{}'", row.pin, row.instance));
        }
        if !set.parts.rows.is_empty() && !set.parts.rows.iter().any(|p| p.has_pin(&row.pin)) {
            return Err(format!(
                "pins: '{}' is in no part's present-list (if the pin exists on silicon, grow the parts row from the datasheet)",
                row.pin
            ));
        }
    }
    for module in &set.modules {
        if !set.parts.rows.iter().any(|p| p.part == module.part) {
            return Err(format!(
                "module {}: host part '{}' is not in parts.toml",
                module.module, module.part
            ));
        }
        validate_bindings(set, &module.bindings, &module.part, &module.module)?;
    }
    Ok(())
}

fn validate_bindings(
    set: &FamilySet,
    bindings: &[Binding],
    part: &str,
    owner: &str,
) -> Result<(), String> {
    let part_row = set.parts.rows.iter().find(|p| p.part == part);
    for binding in bindings {
        let Some(instance) = set.instances.row(&binding.instance) else {
            return Err(format!(
                "{owner}: binding '{}' names unknown instance '{}'",
                binding.role, binding.instance
            ));
        };
        let _ = instance;
        if binding.reference_uv >= 0 && binding.kind != "adc" {
            return Err(format!(
                "{owner}: binding '{}' states reference_uv but is kind '{}' -- the reference rail is an adc-binding fact",
                binding.role, binding.kind
            ));
        }
        for (signal, pin) in &binding.pins {
            if let Some(row) = part_row {
                if !row.has_pin(&pin.pin) {
                    return Err(format!(
                        "{owner}: binding '{}' pin {} ({signal}) is not in part {}'s pin list (if the pin exists on silicon, grow the parts row from the datasheet)",
                        binding.role, pin.pin, part
                    ));
                }
                if let Some(holder) = row.reserved_by(&pin.pin) {
                    return Err(format!(
                        "{owner}: binding '{}' claims pin {} ({signal}), which part {} RESERVES for {} -- a binding cannot route a pin the part does not let a program own",
                        binding.role, pin.pin, part, holder
                    ));
                }
            }
            if pin.soft {
                continue;
            }
            let Some(cell) = set.pin_row(&pin.pin, &binding.function) else {
                return Err(format!(
                    "{owner}: binding '{}' claims {} function {} but pins.toml has no such row (grow the pin map from the datasheet, never from the binding)",
                    binding.role, pin.pin, binding.function
                ));
            };
            if cell.is_unrouted() {
                return Err(format!(
                    "{owner}: binding '{}' claims {} function {} reaches {}, but pins.toml states that NO controller reaches that cell ({})",
                    binding.role, pin.pin, binding.function, binding.instance, cell.source
                ));
            }
            if cell.instance != binding.instance {
                return Err(format!(
                    "{owner}: binding '{}' claims {}/{} routes to {}, but pins.toml says {}",
                    binding.role, pin.pin, binding.function, binding.instance, cell.instance
                ));
            }
            if pin.pad >= 0 && cell.signal != format!("pad{}", pin.pad) {
                return Err(format!(
                    "{owner}: binding '{}' claims {} is pad{}, but pins.toml says {}",
                    binding.role, pin.pin, pin.pad, cell.signal
                ));
            }
        }
    }
    Ok(())
}


/// A fully resolved board: the parsed board plus its inherited module bindings and host part.
#[derive(Clone, Debug)]
pub struct ResolvedBoard {
    /// The parsed board table.
    pub board: BoardTable,
    /// The effective part id (the board's own, or the module's host part).
    pub part: String,
    /// The effective bindings: the module's (if any) then the board's own.
    pub bindings: Vec<Binding>,
    /// The module's control pins (empty for a bare-chip board).
    pub module_pins: Vec<ControlPin>,
}

/// Resolves a board against its family set: inherits module bindings, then validates
/// every effective binding.
pub fn resolve_board(set: &FamilySet, board: BoardTable) -> Result<ResolvedBoard, String> {
    let (part, mut bindings, module_pins) = if board.module.is_empty() {
        (board.part.clone(), Vec::new(), Vec::new())
    } else {
        let Some(module) = set.module(&board.module) else {
            return Err(format!("board {}: unknown module '{}'", board.board, board.module));
        };
        (module.part.clone(), module.bindings.clone(), module.module_pins.clone())
    };
    bindings.extend(board.bindings.clone());
    let Some(part_row) = set.parts.rows.iter().find(|p| p.part == part) else {
        return Err(format!("board {}: part '{}' is not in parts.toml", board.board, part));
    };
    if part_row.flash != 0 {
        if let Some(region) =
            board.memory.iter().find(|r| r.kind == "flash" && r.controller.is_empty())
        {
            return Err(format!(
                "board {}: memory region '{}' is the part's program flash, but part '{}' already states one (0x{:X}) -- a second home for a chip fact refuses. An EXTERNAL region names the controller that brings it up.",
                board.board, region.name, part, part_row.flash
            ));
        }
    }
    for region in &board.memory {
        if !region.controller.is_empty() && set.instances.row(&region.controller).is_none() {
            return Err(format!(
                "board {}: memory region '{}' is brought up by '{}', which csp/{}/instances.toml does not place",
                board.board, region.name, region.controller, set.family
            ));
        }
    }
    validate_bindings(set, &bindings, &part, &board.board)?;
    for line in board.devices.iter().chain(module_pins.iter()) {
        if !line.pin.is_empty() && !part_row.has_pin(&line.pin) {
            return Err(format!(
                "board {}: device '{}' is wired to {}, which is not in part {}'s pin list (if the pin exists on silicon, grow the parts row from the datasheet)",
                board.board, line.name, line.pin, part
            ));
        }
        if let Some(owner) = part_row.reserved_by(&line.pin) {
            return Err(format!(
                "board {}: device '{}' is wired to {}, which part {} RESERVES for {} -- the pin exists but a program does not own it, so the write lands on a cell the board cannot drive and the device stays silent",
                board.board, line.name, line.pin, part, owner
            ));
        }
    }
    let mut pin_claims: Vec<(&str, String)> = Vec::new();
    for binding in &bindings {
        for (_, pin) in &binding.pins {
            pin_claims.push((pin.pin.as_str(), format!("binding '{}'", binding.role)));
        }
    }
    for line in module_pins.iter().chain(board.devices.iter()) {
        if !line.pin.is_empty() {
            pin_claims.push((line.pin.as_str(), format!("device line '{}'", line.name)));
        }
    }
    for (index, (pin, claimant)) in pin_claims.iter().enumerate() {
        if let Some((_, first)) = pin_claims[..index].iter().find(|(p, _)| p == pin) {
            return Err(format!(
                "board {}: pin {pin} is claimed by both {first} and {claimant} -- one pin, one claimant",
                board.board
            ));
        }
    }
    if !board.carriers.is_empty() {
        for carrier in &board.carriers {
            if !carrier.plan.is_empty() && !board.plans.iter().any(|p| p.name == carrier.plan) {
                return Err(format!(
                    "board {}: carrier '{}' names plan '{}' but no [[plans]] row declares it",
                    board.board, carrier.kind, carrier.plan
                ));
            }
        }
        let defaults = board.carriers.iter().filter(|c| c.default).count();
        if defaults != 1 {
            return Err(format!(
                "board {}: expected exactly one default carrier, found {defaults} -- a board that declares carriers names the one a bare deploy reaches for",
                board.board
            ));
        }
    }
    for carrier in &board.carriers {
        if !carrier.role.is_empty() && !bindings.iter().any(|b| b.role == carrier.role) {
            return Err(format!(
                "board {}: carrier names role '{}' but no binding declares it",
                board.board, carrier.role
            ));
        }
    }
    for device in &board.devices {
        let pin_wired = !device.pin.is_empty();
        let bus_wired = !device.role.is_empty() || device.address >= 0;
        if pin_wired == bus_wired {
            return Err(format!(
                "board {}: device '{}' must be EITHER pin-wired (pin =) or bus-wired (role = + address =)",
                board.board, device.name
            ));
        }
        if bus_wired {
            if device.role.is_empty() || device.address < 0 {
                return Err(format!(
                    "board {}: bus device '{}' needs both role and address",
                    board.board, device.name
                ));
            }
            if !bindings.iter().any(|b| b.role == device.role) {
                return Err(format!(
                    "board {}: device '{}' rides role '{}' but no binding declares it",
                    board.board, device.name, device.role
                ));
            }
        }
    }
    for connector in &board.connectors {
        let mut brought_out: Vec<(String, String)> = Vec::new();
        for bus in &connector.buses {
            let Some(binding) = bindings.iter().find(|b| b.role == bus.role) else {
                return Err(format!(
                    "board {}: connector '{}' brings out role '{}' but no binding declares it",
                    board.board, connector.name, bus.role
                ));
            };
            if binding.kind != bus.signal {
                return Err(format!(
                    "board {}: connector '{}' brings out '{}' over role '{}', which is a {} binding -- the socket's group and the bound kind are one fact",
                    board.board, connector.name, bus.signal, bus.role, binding.kind
                ));
            }
            for (signal, pin) in &binding.pins {
                brought_out.push((pin.pin.clone(), format!("role '{}' as {signal}", bus.role)));
            }
        }
        for line in &connector.pins {
            if !part_row.has_pin(&line.pin) {
                return Err(format!(
                    "board {}: connector '{}' brings {} out at position {}, which is not in part {}'s pin list (if the pin exists on silicon, grow the parts row from the datasheet)",
                    board.board, connector.name, line.pin, line.signal, part
                ));
            }
            if let Some(owner) = part_row.reserved_by(&line.pin) {
                return Err(format!(
                    "board {}: connector '{}' brings {} out at position {}, but part {} RESERVES that pin for {} -- a program does not own it, so nothing plugged into the socket can be reached through it",
                    board.board, connector.name, line.pin, line.signal, part, owner
                ));
            }
            if let Some((_, by)) = brought_out.iter().find(|(pin, _)| *pin == line.pin) {
                return Err(format!(
                    "board {}: connector '{}' brings {} out both as position {} and through {} -- one wire, one statement",
                    board.board, connector.name, line.pin, line.signal, by
                ));
            }
            brought_out.push((line.pin.clone(), format!("position {}", line.signal)));
        }
    }
    Ok(ResolvedBoard { board, part, bindings, module_pins })
}

/// True when a row list still holds something a doc comment could document.
fn documents_something(rest: &[Row]) -> bool {
    rest.iter().any(|row| !matches!(row, Row::Blank | Row::Comment(_)))
}

/// Renders a section comment with ONE MARKER PER LINE, at a given indent.
///
/// A doc marker, so a section's warning travels with the constants it qualifies rather than being
/// separable from them -- a caution such as "this dummy-cycle count must be asked of the part, and a
/// wrong one returns plausible garbage rather than failing" is worth nothing if it can be parted
/// from the numbers it guards.
///
/// A doc comment must document something, so a comment nothing follows keeps the ordinary marker
/// -- an empty trailing doc comment is not valid Rust.
fn push_section_comment(out: &mut String, indent: &str, marker: &str, comment: &str) {
    for line in comment.split('\n') {
        out.push_str(&format!("{indent}{marker} {line}\n"));
    }
}

fn emit_header(out: &mut String, class: &str, what: &str, sources: &[String], regen: &str) {
    out.push_str(&format!(
        "// GENERATED by lamella-bsp-gen from {list} -- DO NOT EDIT.\n// Regenerate: {regen}\n//\n// {what}\nnamespace Lamella.Generated\n{{\n    public sealed class {class}\n    {{\n        private {class}() {{ }}\n",
        list = sources.join(" + "),
    ));
}

fn push_const(out: &mut String, kind: &str, name: &str, value: &str) {
    out.push_str(&format!("        public const {kind} {name} = {value};\n"));
}

/// What `<ROLE>_DRIVER_FAMILY` means, rendered at a given indent.
///
/// A DOC COMMENT RATHER THAN A SECTION COMMENT, because this one has to reach a reader of the
/// published source: only `///` and `//!` survive stripping in a `.rs` or `.cs`, and a plain `//`
/// section header does not. This is a new public string, so the sentence that says how to read it
/// has to travel with it.
fn driver_family_note(indent: &str) -> String {
    let lines = [
        "driver family: which REGISTER MAP is behind a role, as `<chip family>-<block>`. The",
        "role's `KIND` says what the application asked for -- a uart, an spi -- and two",
        "peripherals of the same kind can share no register at all, so a consumer that selects a",
        "driver at run time needs both: KIND is what was asked for, this is what the silicon is.",
        "Neither alone is enough. One SERCOM block serves uart, spi and i2c, so the block does not",
        "name a driver; and a uart is a different register map on every family, so the kind does",
        "not either. Derived from the bound instance's block, so it cannot be transcribed wrongly.",
    ];
    let mut out = String::from("\n");
    for line in lines {
        out.push_str(&format!("{indent}/// {line}\n"));
    }
    out
}

fn finish_class(out: &mut String) -> Result<(), String> {
    out.push_str("    }\n}\n");
    let mut seen = std::collections::HashSet::new();
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix("public const ") {
            if let Some(name) = rest.split_whitespace().nth(1) {
                if !seen.insert(name.to_string()) {
                    return Err(format!("duplicate emitted constant '{name}'"));
                }
            }
        }
    }
    Ok(())
}

/// The generated layout class name for a block: `Samd21SercomUsartLayout`.
#[must_use]
pub fn layout_class(block: &BlockTable) -> String {
    format!("{}{}{}Layout", pascal(&block.family), pascal(&block.block), pascal(&block.mode))
}

/// Emits a block's C# layout class: `_OFF` offsets, `_WIDTH` widths, field masks + `_LSB`
/// shifts, and block constants -- all block-relative, no addresses.
pub fn emit_layout_csharp(block: &BlockTable, source: &str, regen: &str) -> Result<String, String> {
    let class = layout_class(block);
    let mut out = String::new();
    let what = format!(
        "The {} {}{} BLOCK layout: offsets are instance-base-relative (`base + *_OFF`);\n// instance bases live in {}Instances. Widths are access widths.",
        block.family,
        block.block,
        if block.mode.is_empty() { String::new() } else { format!(" ({} mode)", block.mode) },
        pascal(&block.family),
    );
    emit_header(&mut out, &class, &what, &[source.to_string()], regen);

    out.push_str("\n        // -- register offsets (block-relative) + access widths --\n");
    for register in &block.registers {
        push_const(&mut out, "uint", &format!("{}_OFF", register.name), &format_int(register.offset));
        push_const(&mut out, "int", &format!("{}_WIDTH", register.name), &register.width.to_string());
    }

    out.push_str("\n        // -- fields: <REG>_<FIELD> = the shifted mask; _LSB = the shift --\n");
    for register in &block.registers {
        for field in &register.fields {
            push_const(&mut out, "uint", &format!("{}_{}", register.name, field.name), &format!("0x{:X}", field.mask()));
            push_const(&mut out, "uint", &format!("{}_{}_LSB", register.name, field.name), &field.lsb.to_string());
        }
    }

    if !block.constants.is_empty() {
        out.push_str("\n        // -- block constants --\n");
        for (name, value) in &block.constants {
            let kind = if value.value < 0 { "int" } else { "uint" };
            push_const(&mut out, kind, name, &format_int(*value));
        }
    }

    if !block.facts.is_empty() {
        out.push_str("\n        // -- facts as data (chip/electrical facts conversions read) --\n");
        for (name, fact) in &block.facts {
            match fact {
                Fact::Int(value) => {
                    let kind = if value.value < 0 { "int" } else { "uint" };
                    push_const(&mut out, kind, &pascal(name), &format_int(*value));
                }
                Fact::Float(text) => {
                    push_const(&mut out, "double", &pascal(name), text);
                }
            }
        }
    }
    if !block.channels.is_empty() {
        out.push_str("\n        // -- channel map: Channel_<source> = the mux/AINSEL index; Channel<i>_Pin = the\n        // GPIO index a pin-fed channel taps (the inverse, so no driver carries a pin\n        // literal); ChannelCount = the package's mux width --\n");
        for channel in &block.channels {
            push_const(
                &mut out,
                "int",
                &format!("Channel_{}", pascal(&channel.source)),
                &channel.index.to_string(),
            );
        }
        for channel in &block.channels {
            if let Some(('g', pin_index)) = split_pin(&channel.source) {
                push_const(
                    &mut out,
                    "int",
                    &format!("Channel{}_Pin", channel.index),
                    &pin_index.to_string(),
                );
            }
        }
        push_const(&mut out, "int", "ChannelCount", &block.channels.len().to_string());
    }
    for record in &block.calibrations {
        out.push_str(&format!(
            "\n        // -- calibration '{}' (form: {}); integer coefficients, no hardcoding downstream --\n",
            record.name, record.form
        ));
        for (coefficient, value) in &record.coefficients {
            let kind = if value.value < 0 { "int" } else { "uint" };
            push_const(
                &mut out,
                kind,
                &format!("{}_{}", pascal(&record.name), pascal(coefficient)),
                &format_int(*value),
            );
        }
    }
    finish_class(&mut out)?;
    Ok(out)
}

/// The vendor segment a board contributes to a published identifier: the namespace its C# class
/// declares, and the segment its assembly name carries. `raspberry-pi` becomes `RaspberryPi`.
///
/// Public because it has two consumers that must not disagree -- the emitter writes it into
/// `BOARD_VENDOR`, and the gate holds each hand-written board class's `namespace` equal to it.
/// One function rather than one rule applied twice.
#[must_use]
pub fn vendor_segment(vendor: &str) -> String {
    pascal(vendor)
}

/// The generated instances class name for a family: `Samd21Instances`.
#[must_use]
pub fn instances_class(family: &str) -> String {
    format!("{}Instances", pascal(family))
}

/// Emits a family's C# instances class: per row and record field, `<NAME>_<FIELD>` (skipping
/// `-1` not-applicable values), plus a derived `_MASK` beside every `*_bit` field.
pub fn emit_instances_csharp(
    instances: &InstancesTable,
    source: &str,
    regen: &str,
) -> Result<String, String> {
    let class = instances_class(&instances.family);
    let mut out = String::new();
    let what = format!(
        "The {} INSTANCE map: where each block copy sits and its per-instance ids.\n// Layout offsets live in the *Layout classes; wiring lives in the per-board *Bindings.",
        instances.family
    );
    emit_header(&mut out, &class, &what, &[source.to_string()], regen);
    out.push('\n');
    for row in &instances.rows {
        let prefix = upper_snake(&row.name);
        for (field, value) in instances.record.iter().zip(&row.values) {
            if *value == -1 {
                continue;
            }
            let hex = field == "base";
            let spelled = if hex { format!("0x{value:X}") } else { value.to_string() };
            push_const(&mut out, "uint", &format!("{prefix}_{}", upper_snake(field)), &spelled);
            if let Some(stem) = field.strip_suffix("_bit") {
                push_const(
                    &mut out,
                    "uint",
                    &format!("{prefix}_{}_MASK", upper_snake(stem)),
                    &format!("0x{:X}", 1i64 << value),
                );
            }
        }
    }
    finish_class(&mut out)?;
    Ok(out)
}

/// The generated bindings class name for a board: `Samd21XproBindings`.
#[must_use]
pub fn bindings_class(board: &str) -> String {
    format!("{}Bindings", pascal(board))
}

/// One resolved uart-binding emission: the values the neutral binding contract names, derived
/// from (port block, instance row, pad rows, the plan, the carrier rate).
struct UartEmission {
    prefix: String,
    /// The raw role id (FACTS key + role-handle value in the Python emission).
    role: String,
    /// The bound instance id (a FACTS descriptive key).
    instance: String,
    sercom_base: i64,
    irq: i64,
    gclk_clkctrl_value: i64,
    apbc_mask: i64,
    pmux_reg: i64,
    pmux_pair: i64,
    pincfg_tx_reg: i64,
    pincfg_rx_reg: i64,
    txpo: i64,
    rxpo: i64,
    /// (const name suffix, divisor), one per carrier whose wire rides this binding, under THAT
    /// carrier's plan; empty when no carrier rides it.
    bauds: Vec<(String, i64)>,
}

fn resolve_uart(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<UartEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let irq = instances.value(name, "irq").unwrap_or(-1);
    let gclk_id = instances
        .value(name, "gclk_core_id")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance {name} has no gclk_core_id"))?;
    let apbc_bit = instances
        .value(name, "apbc_bit")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance {name} has no apbc_bit"))?;

    if binding.gclk_gen < 0 {
        return Err(format!(
            "{board}: uart binding '{}' declares no gclk_gen (which generator its core clock rides under the default plan)",
            binding.role
        ));
    }
    let gclk = set.block("gclk", "").ok_or_else(|| format!("{board}: no gclk block table"))?;
    let clkctrl = gclk.register("CLKCTRL").ok_or_else(|| format!("{board}: gclk has no CLKCTRL"))?;
    let shift = |field: &str| -> Result<u32, String> {
        clkctrl
            .fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| f.lsb)
            .ok_or_else(|| format!("{board}: CLKCTRL has no {field} field"))
    };
    let gclk_clkctrl_value =
        gclk_id << shift("ID")? | binding.gclk_gen << shift("GEN")? | 1i64 << shift("CLKEN")?;

    let tx = binding.pins.iter().find(|(s, _)| s == "tx").map(|(_, p)| p);
    let rx = binding.pins.iter().find(|(s, _)| s == "rx").map(|(_, p)| p);
    let (Some(tx), Some(rx)) = (tx, rx) else {
        return Err(format!("{board}: uart binding '{}' needs tx and rx pins", binding.role));
    };
    let (tx_port, tx_index) = split_pin(&tx.pin).ok_or_else(|| format!("{board}: bad pin {}", tx.pin))?;
    let (rx_port, rx_index) = split_pin(&rx.pin).ok_or_else(|| format!("{board}: bad pin {}", rx.pin))?;
    if tx_port != rx_port || tx_index / 2 != rx_index / 2 {
        return Err(format!(
            "{board}: uart binding '{}' pins {}/{} do not share a PMUX byte -- per-pin nibble emission is the named growth path; refuse until a real board needs it",
            binding.role, tx.pin, rx.pin
        ));
    }
    let group = format!("port{tx_port}");
    let group_base = instances
        .value(&group, "base")
        .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))?;
    let port = set.block("port", "").ok_or_else(|| format!("{board}: no port block table"))?;
    let pmux0 = port.register("PMUX0").ok_or_else(|| format!("{board}: port has no PMUX0"))?;
    let pincfg0 = port.register("PINCFG0").ok_or_else(|| format!("{board}: port has no PINCFG0"))?;
    let func = port
        .constant(&format!("FUNC_{}", binding.function.to_ascii_uppercase()))
        .ok_or_else(|| format!("{board}: port block has no FUNC_{} constant", binding.function))?;
    let pmux_reg = group_base + pmux0.offset.value + i64::from(tx_index / 2);
    let pmux_pair = (func << 4) | func;
    let pincfg_tx_reg = group_base + pincfg0.offset.value + i64::from(tx_index);
    let pincfg_rx_reg = group_base + pincfg0.offset.value + i64::from(rx_index);

    let txpo = match tx.pad {
        0 => 0,
        2 => 1,
        other => return Err(format!("{board}: TX on pad{other} has no USART TXPO encoding")),
    };
    let rxpo = rx.pad;
    if !(0..=3).contains(&rxpo) {
        return Err(format!("{board}: RX pad must be 0..3, got {rxpo}"));
    }

    let mut bauds = Vec::new();
    for (carrier, plan) in resolved.board.carrier_points(&binding.role) {
        let f = plan.gclk_hz(binding.gclk_gen).ok_or_else(|| {
            format!(
                "{board}: plan '{}' states no gclk{}_hz rate for binding '{}'",
                plan.name, binding.gclk_gen, binding.role
            )
        })?;
        let rate = carrier.baud;
        let divisor = 65536 - (65536 * 16 * rate) / f;
        bauds.push((format!("BAUD_{rate}_{}", upper_snake(&plan.name)), divisor));
    }

    Ok(UartEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        sercom_base: base,
        irq,
        gclk_clkctrl_value,
        apbc_mask: 1i64 << apbc_bit,
        pmux_reg,
        pmux_pair,
        pincfg_tx_reg,
        pincfg_rx_reg,
        txpo,
        rxpo,
        bauds,
    })
}

/// One resolved samd21 sercom-i2c binding emission (the SERCOM I2C-master shape): the sercom
/// base, the composed GCLK.CLKCTRL word, the APBC gate mask, the PMUX byte the SDA/SCL pair
/// shares plus each pin's PINCFG address, and the plan's core-clock RATE.
///
/// The rate, and NOT a divisor, is the emitted clock fact. An I2C bus speed is a runtime
/// `Configure` choice the caller makes (100 kHz, 400 kHz), so the BAUD register value is
/// derived on the device from this rate -- the same division of labor the pl022 spi arm uses
/// for SSPCLK. A divisor here would be a plan fact pretending to be a board one.
struct SercomI2cEmission {
    prefix: String,
    role: String,
    instance: String,
    sercom_base: i64,
    irq: i64,
    gclk_clkctrl_value: i64,
    apbc_mask: i64,
    pmux_reg: i64,
    pmux_pair: i64,
    pincfg_sda_reg: i64,
    pincfg_scl_reg: i64,
    core_clock_hz: i64,
}

/// Resolves a samd21 `kind = "i2c"` binding. Structurally the usart arm's twin -- same instance
/// records, same GCLK composition, same shared-PMUX-byte pin pair -- differing only in what the
/// SERCOM is being asked to be: no pad routing (I2C fixes SDA to pad0 and SCL to pad1, so there
/// is nothing for a board to choose) and a rate rather than a divisor.
fn resolve_i2c_samd21(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<SercomI2cEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let irq = instances.value(name, "irq").unwrap_or(-1);
    let gclk_id = instances
        .value(name, "gclk_core_id")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance {name} has no gclk_core_id"))?;
    let apbc_bit = instances
        .value(name, "apbc_bit")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance {name} has no apbc_bit"))?;

    if binding.gclk_gen < 0 {
        return Err(format!(
            "{board}: i2c binding '{}' declares no gclk_gen (which generator its core clock rides under the default plan)",
            binding.role
        ));
    }
    let gclk = set.block("gclk", "").ok_or_else(|| format!("{board}: no gclk block table"))?;
    let clkctrl = gclk.register("CLKCTRL").ok_or_else(|| format!("{board}: gclk has no CLKCTRL"))?;
    let shift = |field: &str| -> Result<u32, String> {
        clkctrl
            .fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| f.lsb)
            .ok_or_else(|| format!("{board}: CLKCTRL has no {field} field"))
    };
    let gclk_clkctrl_value =
        gclk_id << shift("ID")? | binding.gclk_gen << shift("GEN")? | 1i64 << shift("CLKEN")?;

    let sda = binding.pins.iter().find(|(s, _)| s == "sda").map(|(_, p)| p);
    let scl = binding.pins.iter().find(|(s, _)| s == "scl").map(|(_, p)| p);
    let (Some(sda), Some(scl)) = (sda, scl) else {
        return Err(format!("{board}: i2c binding '{}' needs sda and scl pins", binding.role));
    };
    let (sda_port, sda_index) = split_pin(&sda.pin).ok_or_else(|| format!("{board}: bad pin {}", sda.pin))?;
    let (scl_port, scl_index) = split_pin(&scl.pin).ok_or_else(|| format!("{board}: bad pin {}", scl.pin))?;
    if sda_port != scl_port || sda_index / 2 != scl_index / 2 {
        return Err(format!(
            "{board}: i2c binding '{}' pins {}/{} do not share a PMUX byte -- per-pin nibble emission is the named growth path; refuse until a real board needs it",
            binding.role, sda.pin, scl.pin
        ));
    }
    let group = format!("port{sda_port}");
    let group_base = instances
        .value(&group, "base")
        .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))?;
    let port = set.block("port", "").ok_or_else(|| format!("{board}: no port block table"))?;
    let pmux0 = port.register("PMUX0").ok_or_else(|| format!("{board}: port has no PMUX0"))?;
    let pincfg0 = port.register("PINCFG0").ok_or_else(|| format!("{board}: port has no PINCFG0"))?;
    let func = port
        .constant(&format!("FUNC_{}", binding.function.to_ascii_uppercase()))
        .ok_or_else(|| format!("{board}: port block has no FUNC_{} constant", binding.function))?;

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let core_clock_hz = plan.gclk_hz(binding.gclk_gen).ok_or_else(|| {
        format!(
            "{board}: default plan '{}' states no gclk{}_hz rate for binding '{}'",
            plan.name, binding.gclk_gen, binding.role
        )
    })?;

    Ok(SercomI2cEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        sercom_base: base,
        irq,
        gclk_clkctrl_value,
        apbc_mask: 1i64 << apbc_bit,
        pmux_reg: group_base + pmux0.offset.value + i64::from(sda_index / 2),
        pmux_pair: (func << 4) | func,
        pincfg_sda_reg: group_base + pincfg0.offset.value + i64::from(sda_index),
        pincfg_scl_reg: group_base + pincfg0.offset.value + i64::from(scl_index),
        core_clock_hz,
    })
}

/// One resolved rp-family uart-binding emission (the PL011 shape): the
/// uart base, the combined reset-release mask (the binding's instance + both IO banks), the
/// per-pin IO_BANK0 CTRL addresses from the rp pin
/// derivation (GPIO<n>_CTRL = io_bank0 base + GPIO0_CTRL_OFF + GPIO_CTRL_STRIDE * n, both
/// factors read from the io-bank0 block), the funcsel, the plan's UART clock rate, and the
/// carrier rate's PL011 divisor pair.
struct RpUartEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    reset_mask: i64,
    io_tx_ctrl: i64,
    io_rx_ctrl: i64,
    /// The per-pin PADS_BANK0 addresses as (tx, rx) -- the rp2350 delta (its pads reset
    /// ISOLATED, so each pin's pad register is part of the descriptor); None on the rp2040,
    /// whose pads are usable at reset and whose emission must not move (its gate anchors).
    pads: Option<(i64, i64)>,
    funcsel: i64,
    clk_peri_hz: i64,
    /// (`<rate>_<PLAN>` suffix, IBRD, FBRD), one per carrier whose wire rides this binding,
    /// under THAT carrier's plan; empty when no carrier rides it.
    bauds: Vec<(String, i64, i64)>,
}

/// The per-pin PADS_BANK0 register address (pads base + GPIO0 offset + GPIO_STRIDE * n) --
/// the rp-family pads derivation (stride 4 beside io's 8).
fn rp_pad_address(set: &FamilySet, board: &str, pin: &str) -> Result<i64, String> {
    let pads = set
        .block("pads-bank0", "")
        .ok_or_else(|| format!("{board}: no pads-bank0 block table"))?;
    let gpio0 = pads
        .register("GPIO0")
        .ok_or_else(|| format!("{board}: pads-bank0 has no GPIO0"))?;
    let stride = pads
        .constant("GPIO_STRIDE")
        .ok_or_else(|| format!("{board}: pads-bank0 has no GPIO_STRIDE constant"))?;
    let pads_base = set
        .instances
        .value("pads_bank0", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'pads_bank0'"))?;
    match split_pin(pin) {
        Some(('g', index)) => Ok(pads_base + gpio0.offset.value + stride * i64::from(index)),
        _ => Err(format!("{board}: '{pin}' is not a GP<n> pin (the rp-family spelling)")),
    }
}

fn resolve_uart_rp(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
    with_pads: bool,
) -> Result<RpUartEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let reset_bit = |row: &str| -> Result<i64, String> {
        instances
            .value(row, "reset_bit")
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{board}: instance '{row}' has no reset_bit"))
    };
    let reset_mask =
        (1i64 << reset_bit(name)?) | (1i64 << reset_bit("io_bank0")?) | (1i64 << reset_bit("pads_bank0")?);

    let io = set.block("io-bank0", "").ok_or_else(|| format!("{board}: no io-bank0 block table"))?;
    let ctrl0 = io
        .register("GPIO0_CTRL")
        .ok_or_else(|| format!("{board}: io-bank0 has no GPIO0_CTRL"))?;
    let stride = io
        .constant("GPIO_CTRL_STRIDE")
        .ok_or_else(|| format!("{board}: io-bank0 has no GPIO_CTRL_STRIDE constant"))?;
    let io_base = instances
        .value("io_bank0", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'io_bank0'"))?;

    let tx = binding.pins.iter().find(|(s, _)| s == "tx").map(|(_, p)| p);
    let rx = binding.pins.iter().find(|(s, _)| s == "rx").map(|(_, p)| p);
    let (Some(tx), Some(rx)) = (tx, rx) else {
        return Err(format!("{board}: uart binding '{}' needs tx and rx pins", binding.role));
    };
    let pin_index = |pin: &str| -> Result<i64, String> {
        match split_pin(pin) {
            Some(('g', index)) => Ok(i64::from(index)),
            _ => Err(format!("{board}: '{pin}' is not a GP<n> pin (the rp-family spelling)")),
        }
    };
    let io_tx_ctrl = io_base + ctrl0.offset.value + stride * pin_index(&tx.pin)?;
    let io_rx_ctrl = io_base + ctrl0.offset.value + stride * pin_index(&rx.pin)?;
    let pads = if with_pads {
        Some((rp_pad_address(set, board, &tx.pin)?, rp_pad_address(set, board, &rx.pin)?))
    } else {
        None
    };

    let funcsel = binding
        .function
        .strip_prefix('F')
        .and_then(|d| d.parse::<i64>().ok())
        .ok_or_else(|| {
            format!("{board}: uart binding '{}' function '{}' is not F<digit>", binding.role, binding.function)
        })?;
    if let Some(expected) = io.constant("FUNCSEL_UART") {
        if expected != funcsel {
            return Err(format!(
                "{board}: uart binding '{}' funcsel {funcsel} disagrees with io-bank0 FUNCSEL_UART = {expected}",
                binding.role
            ));
        }
    }

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let clk_peri_hz = plan.rate("clk_peri_hz").ok_or_else(|| {
        format!("{board}: default plan '{}' states no clk_peri_hz rate", plan.name)
    })?;
    if plan.source == "xosc" {
        let xosc = plan
            .rate("xosc_hz")
            .ok_or_else(|| format!("{board}: plan '{}' (source xosc) states no xosc_hz", plan.name))?;
        if xosc != clk_peri_hz {
            return Err(format!(
                "{board}: plan '{}' states clk_peri_hz {clk_peri_hz} != xosc_hz {xosc} under source xosc",
                plan.name
            ));
        }
    }

    let mut bauds = Vec::new();
    for (carrier, carrier_plan) in resolved.board.carrier_points(&binding.role) {
        let clk = carrier_plan.rate("clk_peri_hz").ok_or_else(|| {
            format!("{board}: plan '{}' states no clk_peri_hz rate", carrier_plan.name)
        })?;
        let rate = carrier.baud;
        let div64 = (4 * clk + rate / 2) / rate;
        bauds.push((
            format!("{rate}_{}", upper_snake(&carrier_plan.name)),
            div64 >> 6,
            div64 & 0x3F,
        ));
    }

    Ok(RpUartEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        reset_mask,
        io_tx_ctrl,
        io_rx_ctrl,
        pads,
        funcsel,
        clk_peri_hz,
        bauds,
    })
}

/// One resolved pl022-spi binding emission (the rp2350 shape): the SSP
/// base, the combined reset-release mask (spi instance + both IO banks), per-signal IO CTRL +
/// PADS addresses from the rp pin derivations, the funcsel (verified against the io-bank0
/// function table), and the plan's SSPCLK rate (clk_peri, crystal-exact under source xosc --
/// the same state-and-verify as the uart arm). NO divisor pair: the PL022 rate is a runtime
/// Configure choice (CPSDVSR/SCR from SspclkHz), like the samd21 spi BAUD.
struct SpiPl022Emission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    reset_mask: i64,
    /// Per signal, in (miso, cs, sck, mosi) order: (name, io_ctrl, pads).
    signals: Vec<(String, i64, i64)>,
    funcsel: i64,
    sspclk_hz: i64,
}

/// The rp-family reset-release mask for a bus binding: the instance's own reset bit plus
/// both IO banks.
fn rp_reset_mask(set: &FamilySet, board: &str, instance: &str) -> Result<i64, String> {
    let reset_bit = |row: &str| -> Result<i64, String> {
        set.instances
            .value(row, "reset_bit")
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{board}: instance '{row}' has no reset_bit"))
    };
    Ok((1i64 << reset_bit(instance)?) | (1i64 << reset_bit("io_bank0")?) | (1i64 << reset_bit("pads_bank0")?))
}

/// The rp-family per-pin IO_BANK0 CTRL address (io base + GPIO0_CTRL offset + stride * n).
fn rp_io_ctrl_address(set: &FamilySet, board: &str, pin: &str) -> Result<i64, String> {
    let io = set.block("io-bank0", "").ok_or_else(|| format!("{board}: no io-bank0 block table"))?;
    let ctrl0 = io
        .register("GPIO0_CTRL")
        .ok_or_else(|| format!("{board}: io-bank0 has no GPIO0_CTRL"))?;
    let stride = io
        .constant("GPIO_CTRL_STRIDE")
        .ok_or_else(|| format!("{board}: io-bank0 has no GPIO_CTRL_STRIDE constant"))?;
    let io_base = set
        .instances
        .value("io_bank0", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'io_bank0'"))?;
    match split_pin(pin) {
        Some(('g', index)) => Ok(io_base + ctrl0.offset.value + stride * i64::from(index)),
        _ => Err(format!("{board}: '{pin}' is not a GP<n> pin (the rp-family spelling)")),
    }
}

/// The binding's F<k> function digit, verified against the io-bank0 function-table constant
/// when the block states one (two statements of one fact refuse on disagreement).
fn rp_funcsel(
    set: &FamilySet,
    board: &str,
    binding: &Binding,
    io_constant: &str,
) -> Result<i64, String> {
    let funcsel = binding
        .function
        .strip_prefix('F')
        .and_then(|d| d.parse::<i64>().ok())
        .ok_or_else(|| {
            format!(
                "{board}: {} binding '{}' function '{}' is not F<digit>",
                binding.kind, binding.role, binding.function
            )
        })?;
    let io = set.block("io-bank0", "").ok_or_else(|| format!("{board}: no io-bank0 block table"))?;
    if let Some(expected) = io.constant(io_constant) {
        if expected != funcsel {
            return Err(format!(
                "{board}: {} binding '{}' funcsel {funcsel} disagrees with io-bank0 {io_constant} = {expected}",
                binding.kind, binding.role
            ));
        }
    }
    Ok(funcsel)
}

/// The plan's crystal-exact clk_peri rate under source "xosc" (state-and-verify: clk_peri
/// rides the crystal directly on that route, so the two stated rates must agree).
fn rp_clk_peri_hz(resolved: &ResolvedBoard) -> Result<i64, String> {
    let board = &resolved.board.board;
    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let clk_peri_hz = plan.rate("clk_peri_hz").ok_or_else(|| {
        format!("{board}: default plan '{}' states no clk_peri_hz rate", plan.name)
    })?;
    if plan.source == "xosc" {
        let xosc = plan
            .rate("xosc_hz")
            .ok_or_else(|| format!("{board}: plan '{}' (source xosc) states no xosc_hz", plan.name))?;
        if xosc != clk_peri_hz {
            return Err(format!(
                "{board}: plan '{}' states clk_peri_hz {clk_peri_hz} != xosc_hz {xosc} under source xosc",
                plan.name
            ));
        }
    }
    Ok(clk_peri_hz)
}

fn resolve_spi_pl022(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<SpiPl022Emission, String> {
    let board = &resolved.board.board;
    let base = set
        .instances
        .value(&binding.instance, "base")
        .ok_or_else(|| format!("{board}: no base for {}", binding.instance))?;
    let reset_mask = rp_reset_mask(set, board, &binding.instance)?;

    let mut signals = Vec::new();
    for (signal, cell_signal) in [("miso", "rx"), ("cs", "ss_n"), ("sck", "sclk"), ("mosi", "tx")] {
        let pin = binding
            .pins
            .iter()
            .find(|(s, _)| s == signal)
            .map(|(_, p)| p)
            .ok_or_else(|| format!("{board}: spi binding '{}' needs a {signal} pin", binding.role))?;
        if pin.soft {
            return Err(format!(
                "{board}: spi binding '{}' marks {signal} soft -- the pl022 arm emits the muxed wiring only (a managed CS is the driver's runtime choice, not a binding fact)",
                binding.role
            ));
        }
        let cell = set
            .pin_row(&pin.pin, &binding.function)
            .ok_or_else(|| {
                format!(
                    "{board}: spi binding '{}' claims {} function {} but pins.toml has no such row",
                    binding.role, pin.pin, binding.function
                )
            })?;
        if cell.signal != cell_signal {
            return Err(format!(
                "{board}: spi binding '{}' routes {signal} to {}, but its pin-map cell is {}/{} (the pl022 {signal} pin must be the block's {cell_signal})",
                binding.role, pin.pin, cell.instance, cell.signal
            ));
        }
        signals.push((
            signal.to_string(),
            rp_io_ctrl_address(set, board, &pin.pin)?,
            rp_pad_address(set, board, &pin.pin)?,
        ));
    }

    Ok(SpiPl022Emission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        reset_mask,
        signals,
        funcsel: rp_funcsel(set, board, binding, "FUNCSEL_SPI")?,
        sspclk_hz: rp_clk_peri_hz(resolved)?,
    })
}

/// One resolved dw-i2c binding emission (the rp2350's Synopsys DW_apb_i2c): the block
/// base, the combined reset-release mask, the SDA/SCL IO CTRL + PADS
/// addresses, the funcsel (verified), and the plan's ic_clk rate -- which is clk_sys on this
/// chip (the clocking delta from its uart/spi siblings; no per-block mux exists to re-source
/// it). The SCL count formulas stay DRIVER math per the official pico-sdk driver.
struct DwI2cEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    reset_mask: i64,
    io_sda_ctrl: i64,
    io_scl_ctrl: i64,
    pads_sda: i64,
    pads_scl: i64,
    funcsel: i64,
    ic_clk_hz: i64,
}

fn resolve_i2c_dw(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<DwI2cEmission, String> {
    let board = &resolved.board.board;
    let base = set
        .instances
        .value(&binding.instance, "base")
        .ok_or_else(|| format!("{board}: no base for {}", binding.instance))?;
    let signal = |signal_name: &str| -> Result<&PinRef, String> {
        binding
            .pins
            .iter()
            .find(|(s, _)| s == signal_name)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                format!("{board}: i2c binding '{}' needs a {signal_name} pin", binding.role)
            })
    };
    let sda = signal("sda")?;
    let scl = signal("scl")?;
    for (pin, cell_signal) in [(sda, "sda"), (scl, "scl")] {
        let cell = set.pin_row(&pin.pin, &binding.function).ok_or_else(|| {
            format!(
                "{board}: i2c binding '{}' claims {} function {} but pins.toml has no such row",
                binding.role, pin.pin, binding.function
            )
        })?;
        if cell.signal != cell_signal {
            return Err(format!(
                "{board}: i2c binding '{}' routes {} to pin-map signal {}, expected {cell_signal}",
                binding.role, pin.pin, cell.signal
            ));
        }
    }

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let ic_clk_hz = plan.rate("clk_sys_hz").ok_or_else(|| {
        format!(
            "{board}: default plan '{}' states no clk_sys_hz rate (the dw-i2c ic_clk is clk_sys)",
            plan.name
        )
    })?;

    Ok(DwI2cEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        reset_mask: rp_reset_mask(set, board, &binding.instance)?,
        io_sda_ctrl: rp_io_ctrl_address(set, board, &sda.pin)?,
        io_scl_ctrl: rp_io_ctrl_address(set, board, &scl.pin)?,
        pads_sda: rp_pad_address(set, board, &sda.pin)?,
        pads_scl: rp_pad_address(set, board, &scl.pin)?,
        funcsel: rp_funcsel(set, board, binding, "FUNCSEL_I2C")?,
        ic_clk_hz,
    })
}

/// One resolved rp-adc binding emission: the converter base, its reset-release mask (the ADC
/// releases alone -- its pins are analogue, no IO-bank route), and the BOARD's reference rail
/// in microvolts (board truth, so it rides the adc binding and emits per-board).
/// The channel map and calibration records stay CHIP truth in the adc block's layout
/// emission; the plan's clk_adc rate is verified against the block's required rate.
struct RpAdcEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    reset_mask: i64,
    reference_uv: i64,
}

fn resolve_adc_rp(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<RpAdcEmission, String> {
    let board = &resolved.board.board;
    let base = set
        .instances
        .value(&binding.instance, "base")
        .ok_or_else(|| format!("{board}: no base for {}", binding.instance))?;
    let reset_bit = set
        .instances
        .value(&binding.instance, "reset_bit")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance '{}' has no reset_bit", binding.instance))?;
    if binding.reference_uv <= 0 {
        return Err(format!(
            "{board}: adc binding '{}' states no reference_uv -- the reference rail is BOARD truth",
            binding.role
        ));
    }
    let adc_block = set
        .block(&binding.instance, "")
        .or_else(|| set.block("adc", ""))
        .ok_or_else(|| format!("{board}: no adc block table"))?;
    let required = adc_block
        .facts
        .iter()
        .find(|(n, _)| n == "clk_adc_hz")
        .and_then(|(_, f)| match f {
            Fact::Int(int) => Some(int.value),
            Fact::Float(_) => None,
        });
    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    if let Some(required) = required {
        let stated = plan.rate("clk_adc_hz").ok_or_else(|| {
            format!(
                "{board}: default plan '{}' states no clk_adc_hz rate (the adc block requires {required})",
                plan.name
            )
        })?;
        if stated != required {
            return Err(format!(
                "{board}: plan '{}' states clk_adc_hz {stated} but the adc block requires {required} (state-and-verify)",
                plan.name
            ));
        }
    }
    Ok(RpAdcEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        reset_mask: 1i64 << reset_bit,
        reference_uv: binding.reference_uv,
    })
}

/// The rp2350 clock-plan emission (the state-and-verify case): the plan
/// STATES the chosen PLL values (fbdiv + both post dividers per PLL) AND the resulting
/// rates; the generator VERIFIES hz == xosc * fbdiv / (postdiv1 * postdiv2) -- refusing
/// mismatch, never deriving backwards -- and composes each PLL's PRIM register word from the
/// pll block's field positions (a composed config word, generated-output-only).
struct RpClockEmission {
    /// The plan's name, upper-snake, suffixing every const this block emits: a
    /// board may declare more than one operating point, so a bare `CLK_SYS_HZ` could not say
    /// which of its wires it means. Suffixed UNIFORMLY, the default included -- the same
    /// symmetry the carrier-rate divisors already spell.
    plan: String,
    xosc_hz: i64,
    clk_sys_hz: i64,
    clk_usb_hz: i64,
    /// The plan's clk_adc rate when stated (-1 = absent; the adc arm verifies it).
    clk_adc_hz: i64,
    pll_sys_fbdiv: i64,
    pll_sys_prim: i64,
    pll_usb_fbdiv: i64,
    pll_usb_prim: i64,
}

/// Every rp clock-plan emission this board makes: one per DISTINCT plan a carrier names that
/// states a pll tree. Most boards yield zero or one -- one per wire that runs on a PLL, and
/// none for a wire that rides the bare crystal (which is not this shape).
fn resolve_clocks_rp(set: &FamilySet, resolved: &ResolvedBoard) -> Result<Vec<RpClockEmission>, String> {
    let mut out = Vec::new();
    for plan in resolved.board.carrier_plans() {
        if let Some(clock) = resolve_clock_rp(set, resolved, plan)? {
            out.push(clock);
        }
    }
    Ok(out)
}

fn resolve_clock_rp(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    plan: &Plan,
) -> Result<Option<RpClockEmission>, String> {
    let board = &resolved.board.board;
    if plan.rate("pll_sys_fbdiv").is_none() {
        return Ok(None);
    }
    let Some(pll) = set.block("pll", "") else {
        return Err(format!("{board}: the plan states pll_* values but the family has no pll block"));
    };
    let prim = pll.register("PRIM").ok_or_else(|| format!("{board}: pll block has no PRIM"))?;
    let field_lsb = |name: &str| -> Result<u32, String> {
        prim.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.lsb)
            .ok_or_else(|| format!("{board}: pll PRIM has no {name} field"))
    };
    let p1_lsb = field_lsb("POSTDIV1")?;
    let p2_lsb = field_lsb("POSTDIV2")?;

    let xosc_hz = plan
        .rate("xosc_hz")
        .ok_or_else(|| format!("{board}: plan '{}' states pll_* values but no xosc_hz", plan.name))?;
    let resolve_pll = |name: &str, out_hz_key: &str| -> Result<(i64, i64), String> {
        let chosen = |suffix: &str| -> Result<i64, String> {
            plan.rate(&format!("{name}_{suffix}")).ok_or_else(|| {
                format!("{board}: plan '{}' states no {name}_{suffix}", plan.name)
            })
        };
        let fbdiv = chosen("fbdiv")?;
        let postdiv1 = chosen("postdiv1")?;
        let postdiv2 = chosen("postdiv2")?;
        let stated_hz = plan
            .rate(out_hz_key)
            .ok_or_else(|| format!("{board}: plan '{}' states no {out_hz_key}", plan.name))?;
        let derived_hz = xosc_hz * fbdiv / (postdiv1 * postdiv2);
        if derived_hz != stated_hz {
            return Err(format!(
                "{board}: plan '{}' states {out_hz_key} {stated_hz} but xosc {xosc_hz} * fbdiv {fbdiv} / ({postdiv1} * {postdiv2}) = {derived_hz} (state-and-verify)",
            plan.name
            ));
        }
        Ok((fbdiv, (postdiv1 << p1_lsb) | (postdiv2 << p2_lsb)))
    };
    let (pll_sys_fbdiv, pll_sys_prim) = resolve_pll("pll_sys", "clk_sys_hz")?;
    let (pll_usb_fbdiv, pll_usb_prim) = resolve_pll("pll_usb", "clk_usb_hz")?;
    Ok(Some(RpClockEmission {
        plan: upper_snake(&plan.name),
        xosc_hz,
        clk_sys_hz: plan.rate("clk_sys_hz").expect("verified above"),
        clk_usb_hz: plan.rate("clk_usb_hz").expect("verified above"),
        clk_adc_hz: plan.rate("clk_adc_hz").unwrap_or(-1),
        pll_sys_fbdiv,
        pll_sys_prim,
        pll_usb_fbdiv,
        pll_usb_prim,
    }))
}

/// One resolved esp-family uart-binding emission (the C6 HP-UART shape):
/// the uart base, the instance's PCR slot registers (module clock/reset + function-clock
/// select -- resolved by slot name, `<INSTANCE>_CONF`/`<INSTANCE>_SCLK_CONF`), the per-pin
/// IO_MUX addresses from the esp pin derivation (GPIO<n> = io_mux base + GPIO0_OFF +
/// GPIO_STRIDE * n, both factors read from the block layout), the MCU_SEL function value, and
/// the plan's UART function-clock rate. No divisor pair is emitted: the carrier is not this
/// uart (on the C6 the wire is the chip's USB-Serial-JTAG), and a carrier-rate CLKDIV emission
/// is the named growth path when a board's wire actually rides an ESP uart.
struct EspUartEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    pcr_conf: i64,
    pcr_sclk_conf: i64,
    io_mux_tx: i64,
    io_mux_rx: i64,
    mcu_sel: i64,
    sclk_hz: i64,
}

fn resolve_uart_esp32c6(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<EspUartEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;

    let pcr = set.block("pcr", "").ok_or_else(|| format!("{board}: no pcr block table"))?;
    let pcr_base = instances
        .value("pcr", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'pcr'"))?;
    let slot_register = |suffix: &str| -> Result<i64, String> {
        let reg = format!("{}_{suffix}", upper_snake(name));
        pcr.register(&reg)
            .map(|r| pcr_base + r.offset.value)
            .ok_or_else(|| format!("{board}: pcr block has no {reg} slot register (append it, source-cited)"))
    };
    let pcr_conf = slot_register("CONF")?;
    let pcr_sclk_conf = slot_register("SCLK_CONF")?;

    let io = set.block("io-mux", "").ok_or_else(|| format!("{board}: no io-mux block table"))?;
    let gpio0 = io.register("GPIO0").ok_or_else(|| format!("{board}: io-mux has no GPIO0"))?;
    let stride = io
        .constant("GPIO_STRIDE")
        .ok_or_else(|| format!("{board}: io-mux has no GPIO_STRIDE constant"))?;
    let io_base = instances
        .value("io_mux", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'io_mux'"))?;
    let tx = binding.pins.iter().find(|(s, _)| s == "tx").map(|(_, p)| p);
    let rx = binding.pins.iter().find(|(s, _)| s == "rx").map(|(_, p)| p);
    let (Some(tx), Some(rx)) = (tx, rx) else {
        return Err(format!("{board}: uart binding '{}' needs tx and rx pins", binding.role));
    };
    let pin_index = |pin: &str| -> Result<i64, String> {
        match split_pin(pin) {
            Some(('g', index)) => Ok(i64::from(index)),
            _ => Err(format!("{board}: '{pin}' is not a GPIO<n> pin (the esp-family spelling)")),
        }
    };
    let io_mux_tx = io_base + gpio0.offset.value + stride * pin_index(&tx.pin)?;
    let io_mux_rx = io_base + gpio0.offset.value + stride * pin_index(&rx.pin)?;

    let mcu_sel = binding
        .function
        .strip_prefix('F')
        .and_then(|d| d.parse::<i64>().ok())
        .ok_or_else(|| {
            format!("{board}: uart binding '{}' function '{}' is not F<digit>", binding.role, binding.function)
        })?;

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let sclk_hz = plan.rate("uart_sclk_hz").ok_or_else(|| {
        format!("{board}: default plan '{}' states no uart_sclk_hz rate", plan.name)
    })?;
    if plan.source == "xtal" {
        let xtal = plan
            .rate("xtal_hz")
            .ok_or_else(|| format!("{board}: plan '{}' (source xtal) states no xtal_hz", plan.name))?;
        if xtal != sclk_hz {
            return Err(format!(
                "{board}: plan '{}' states uart_sclk_hz {sclk_hz} != xtal_hz {xtal} under source xtal",
                plan.name
            ));
        }
    }
    if !resolved.board.carrier_points(&binding.role).is_empty() {
        return Err(format!(
            "{board}: a carrier riding an esp32c6 uart needs the CLKDIV divisor emission -- not implemented (add it with its anchor first)"
        ));
    }

    Ok(EspUartEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        pcr_conf,
        pcr_sclk_conf,
        io_mux_tx,
        io_mux_rx,
        mcu_sel,
        sclk_hz,
    })
}

/// One resolved sam3x-family uart-binding emission (the SAM3X UART shape):
/// the uart base and peripheral id, the PMC clock gate the id resolves to (PCER0 register + the
/// id's bit), the PIO mux cell the pins resolve to (the port's PDR/ABSR registers + the combined
/// line mask + the function letter's ABSR value), the plan's MCK, and the carrier rate's BRGR
/// divisor.
///
/// The SAM3X differs from every arm above it in two ways worth naming: its peripheral id is ONE
/// number serving as both the PMC gate bit and the NVIC line (so there is no separate irq record
/// field), and its UART has one fixed signal per pin rather than SERCOM's selectable pads (so the
/// bindings carry no pad and the mux is a bare line mask).
struct Sam3xUartEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    pid: i64,
    pmc_pcer_reg: i64,
    pmc_pcer_mask: i64,
    pio_pdr_reg: i64,
    pio_absr_reg: i64,
    pio_mask: i64,
    pio_func: i64,
    mck_hz: i64,
    /// (`BRGR_CD_<rate>_<PLAN>` suffix, CD), one per carrier whose wire rides this binding,
    /// under THAT carrier's plan; empty when no carrier rides it.
    bauds: Vec<(String, i64)>,
}

fn resolve_uart_sam3x(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<Sam3xUartEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let pid = instances
        .value(name, "pid")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance '{name}' has no peripheral id (pid)"))?;

    let pmc = set.block("pmc", "").ok_or_else(|| format!("{board}: no pmc block table"))?;
    let pcer0 = pmc.register("PCER0").ok_or_else(|| format!("{board}: pmc has no PCER0"))?;
    let pmc_base = instances.value("pmc", "base").ok_or_else(|| format!("{board}: no instance row for 'pmc'"))?;
    if pid >= 32 {
        return Err(format!(
            "{board}: uart binding '{}' has pid {pid} >= 32, which gates through PCER1 -- transcribe PCER1 into the pmc block first",
            binding.role
        ));
    }
    let pmc_pcer_reg = pmc_base + pcer0.offset.value;
    let pmc_pcer_mask = 1i64 << pid;

    let tx = binding.pins.iter().find(|(s, _)| s == "tx").map(|(_, p)| p);
    let rx = binding.pins.iter().find(|(s, _)| s == "rx").map(|(_, p)| p);
    let (Some(tx), Some(rx)) = (tx, rx) else {
        return Err(format!("{board}: uart binding '{}' needs tx and rx pins", binding.role));
    };
    let line = |pin: &str| -> Result<(char, u32), String> {
        split_pin(pin).filter(|(port, _)| port.is_ascii_alphabetic()).ok_or_else(|| {
            format!("{board}: '{pin}' is not a P<port><line> pin (the sam3x spelling)")
        })
    };
    let (tx_port, tx_line) = line(&tx.pin)?;
    let (rx_port, rx_line) = line(&rx.pin)?;
    if tx_port != rx_port {
        return Err(format!(
            "{board}: uart binding '{}' straddles PIO ports ({} and {}) -- one binding, one controller (per-port emission is the growth path)",
            binding.role, tx.pin, rx.pin
        ));
    }
    let pio_group = format!("pio{tx_port}");
    let pio_base = instances
        .value(&pio_group, "base")
        .ok_or_else(|| format!("{board}: no instance row for '{pio_group}' (the port {tx} binds)", tx = tx.pin))?;
    let pio = set.block("pio", "").ok_or_else(|| format!("{board}: no pio block table"))?;
    let pdr = pio.register("PDR").ok_or_else(|| format!("{board}: pio has no PDR"))?;
    let absr = pio.register("ABSR").ok_or_else(|| format!("{board}: pio has no ABSR"))?;
    let pio_mask = (1i64 << tx_line) | (1i64 << rx_line);
    let pio_func = pio.constant(&format!("FUNC_{}", binding.function)).ok_or_else(|| {
        format!(
            "{board}: uart binding '{}' function '{}' has no FUNC_{} constant in the pio block",
            binding.role, binding.function, binding.function
        )
    })?;

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let mck_hz = plan
        .rate("mck_hz")
        .ok_or_else(|| format!("{board}: default plan '{}' states no mck_hz rate", plan.name))?;

    let mut bauds = Vec::new();
    for (carrier, carrier_plan) in resolved.board.carrier_points(&binding.role) {
        let mck = carrier_plan
            .rate("mck_hz")
            .ok_or_else(|| format!("{board}: plan '{}' states no mck_hz rate", carrier_plan.name))?;
        let rate = carrier.baud;
        let cd = (mck + (16 * rate) / 2) / (16 * rate);
        if cd == 0 || cd > 0xFFFF {
            return Err(format!(
                "{board}: carrier rate {rate} under plan '{}' needs BRGR CD {cd}, outside the 16-bit field (CD 0 disables the UART)",
                carrier_plan.name
            ));
        }
        bauds.push((format!("{rate}_{}", upper_snake(&carrier_plan.name)), cd));
    }

    Ok(Sam3xUartEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        pid,
        pmc_pcer_reg,
        pmc_pcer_mask,
        pio_pdr_reg: pio_base + pdr.offset.value,
        pio_absr_reg: pio_base + absr.offset.value,
        pio_mask,
        pio_func,
        mck_hz,
        bauds,
    })
}

/// The GPIO facts for ONE bound pin: which port gate to open, and the two read-modify-writes
/// that put the pin in alternate-function mode on the right function number.
///
/// PER PIN RATHER THAN PER BINDING, and that is the whole point of the shape. A uart's two pins
/// need not share anything: the F746G-DISCO transmits on port A and receives on port B, so its
/// two pins have different port gates, different MODER registers and different alternate-function
/// registers. An earlier shape folded both pins into one mask per register -- one write covering
/// both -- which is a real optimization and is only valid when the pins happen to coincide. This
/// states each pin on its own terms; where they do coincide, the two writes hit the same register
/// and the result is identical.
struct StUartPin {
    port_rcc_en_reg: i64,
    port_rcc_en_mask: i64,
    moder_reg: i64,
    moder_mask: i64,
    moder_value: i64,
    /// AFRL or AFRH, selected by the pin's index -- see [`st_uart_pin`].
    afr_reg: i64,
    afr_mask: i64,
    afr_value: i64,
}

/// One resolved st-usart binding emission (the modern ST USART IP, shared across the ST
/// families): the usart base, the RCC enable (register, mask) pair for the instance -- resolved
/// through the [base, rcc_en_off, rcc_en_bit] instance record, which is what makes the RCC-bank
/// split between ST lines DATA (one line's GPIOAEN is an AHB2 bit where another's IOPAEN is an
/// AHB bit) and lets ONE arm serve them all -- the per-pin GPIO facts, the APB rate feeding this
/// instance, and the carrier rate's BRR divisor (16x oversampling, ROUNDED division: e.g. 0x23 @
/// 4 MHz, 0x45 @ 8 MHz, 0x8B @ 16 MHz).
struct StUartEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    rcc_en_reg: i64,
    rcc_en_mask: i64,
    tx: StUartPin,
    rx: StUartPin,
    pclk_hz: i64,
    /// (`BRR_<rate>_<PLAN>` suffix, divisor), one per carrier whose wire rides this binding,
    /// under THAT carrier's plan; empty when no carrier rides it.
    bauds: Vec<(String, i64)>,
}

/// One muxed ST pin's facts, shared by every binding kind on these lines.
///
/// MODER holds 2 bits per pin at [2n]; the alternate-function nibble is 4 bits, but WHICH
/// register holds it is decided by the pin index and the nibble is counted from THAT register's
/// own base -- pins 0..7 in AFRL at [4n], pins 8..15 in AFRH at [4*(n-8)]. Getting that split
/// wrong is silent: the write lands in a real register, on a real pin, and muxes the wrong one.
///
/// `who` names the claimant for the error, because every failure here is a board fact being
/// wrong rather than a bug -- a pin index off the end of a port, a port the family does not
/// place, an AFR half the block table does not state.
fn st_mux_pin(
    set: &FamilySet,
    board: &str,
    rcc_base: i64,
    gpio: &BlockTable,
    af: i64,
    who: &str,
    pin: &PinRef,
) -> Result<StUartPin, String> {
    let moder = gpio.register("MODER").ok_or_else(|| format!("{board}: gpio has no MODER"))?;
    let mode_af = gpio
        .constant("MODER_MODE_AF")
        .ok_or_else(|| format!("{board}: gpio block has no MODER_MODE_AF constant"))?;
    let (port, index) = split_pin(&pin.pin).ok_or_else(|| format!("{board}: bad pin {}", pin.pin))?;
    if index > 15 {
        return Err(format!("{board}: {who} pin index {index} exceeds a port's 16 pins"));
    }
    let group = format!("gpio{port}");
    let group_base = set
        .instances
        .value(&group, "base")
        .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))?;
    let off = set
        .instances
        .value(&group, "rcc_en_off")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance '{group}' has no rcc_en_off"))?;
    let bit = set
        .instances
        .value(&group, "rcc_en_bit")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance '{group}' has no rcc_en_bit"))?;
    let (afr_name, nibble) = if index > 7 { ("AFRH", index - 8) } else { ("AFRL", index) };
    let afr = gpio
        .register(afr_name)
        .ok_or_else(|| format!("{board}: gpio has no {afr_name} (needed by {who} pin {})", pin.pin))?;
    Ok(StUartPin {
        port_rcc_en_reg: rcc_base + off,
        port_rcc_en_mask: 1i64 << bit,
        moder_reg: group_base + moder.offset.value,
        moder_mask: 0b11 << (2 * index),
        moder_value: mode_af << (2 * index),
        afr_reg: group_base + afr.offset.value,
        afr_mask: 0xF << (4 * nibble),
        afr_value: af << (4 * nibble),
    })
}

fn resolve_uart_stm32(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<StUartEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let rcc_base = instances
        .value("rcc", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'rcc'"))?;
    let rcc_enable = |row: &str| -> Result<(i64, i64), String> {
        let off = instances
            .value(row, "rcc_en_off")
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{board}: instance '{row}' has no rcc_en_off"))?;
        let bit = instances
            .value(row, "rcc_en_bit")
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{board}: instance '{row}' has no rcc_en_bit"))?;
        Ok((rcc_base + off, 1i64 << bit))
    };
    let (rcc_en_reg, rcc_en_mask) = rcc_enable(name)?;

    let tx = binding.pins.iter().find(|(s, _)| s == "tx").map(|(_, p)| p);
    let rx = binding.pins.iter().find(|(s, _)| s == "rx").map(|(_, p)| p);
    let (Some(tx), Some(rx)) = (tx, rx) else {
        return Err(format!("{board}: uart binding '{}' needs tx and rx pins", binding.role));
    };
    let gpio = set.block("gpio", "").ok_or_else(|| format!("{board}: no gpio block table"))?;
    let af = binding
        .function
        .strip_prefix("AF")
        .and_then(|d| d.parse::<i64>().ok())
        .ok_or_else(|| {
            format!("{board}: uart binding '{}' function '{}' is not AF<n>", binding.role, binding.function)
        })?;

    let tx_pin = st_mux_pin(set, board, rcc_base, gpio, af, &format!("uart binding '{}' tx", binding.role), tx)?;
    let rx_pin = st_mux_pin(set, board, rcc_base, gpio, af, &format!("uart binding '{}' rx", binding.role), rx)?;

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let pclk1_hz = plan.rate("pclk_hz").ok_or_else(|| {
        format!("{board}: default plan '{}' states no pclk_hz rate", plan.name)
    })?;
    let source_key = format!("{}_hz", plan.source);
    let source_hz = plan.rate(&source_key).ok_or_else(|| {
        format!("{board}: plan '{}' (source {}) states no {source_key}", plan.name, plan.source)
    })?;
    if source_hz != pclk1_hz {
        return Err(format!(
            "{board}: plan '{}' states pclk1_hz {pclk1_hz} != {source_key} {source_hz} -- a prescaled plan needs its own derivation (add it with its anchor)",
            plan.name
        ));
    }

    let mut bauds = Vec::new();
    for (carrier, carrier_plan) in resolved.board.carrier_points(&binding.role) {
        let pclk1 = carrier_plan.rate("pclk_hz").ok_or_else(|| {
            format!("{board}: plan '{}' states no pclk_hz rate", carrier_plan.name)
        })?;
        let rate = carrier.baud;
        let divisor = (pclk1 + rate / 2) / rate;
        bauds.push((format!("BRR_{rate}_{}", upper_snake(&carrier_plan.name)), divisor));
    }

    Ok(StUartEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        rcc_en_reg,
        rcc_en_mask,
        tx: tx_pin,
        rx: rx_pin,
        pclk_hz: pclk1_hz,
        bauds,
    })
}

/// A soft (GPIO-driven) chip select: a plain output the DRIVER toggles, not a muxed cell.
///
/// A soft select exists because a board wired the device's select somewhere the controller's own
/// NSS does not reach -- so it is board truth, and refusing to emit it would push the pin into a
/// driver as a hand-written constant, which is the one transcription generation exists to remove.
struct StSoftCs {
    port_rcc_en_reg: i64,
    port_rcc_en_mask: i64,
    moder_reg: i64,
    moder_mask: i64,
    moder_value: i64,
    /// BSRR: the set/reset register. Writing the mask ASSERTS nothing on its own -- the low half
    /// sets and the high half clears, which is why both words are emitted rather than one mask.
    bsrr_reg: i64,
    bsrr_set: i64,
    bsrr_clear: i64,
}

/// One resolved st-spi binding emission: the instance, its RCC enable, the three muxed data pins,
/// and the soft chip select when the board states one.
///
/// NO BAUD IS DERIVED HERE, DELIBERATELY, and that is the one design decision in this arm. A
/// USART binding derives its divisor because the wire rate is a carrier fact the board states. An
/// SPI master's rate is a property of the ATTACHED DEVICE -- the part's own maximum clock -- and a
/// binding names a bus, not a part. So this emits the APB rate feeding the instance and the block
/// table states the eight prescaler codes; the driver, which is the only layer that knows what is
/// on the other end, picks one. Deriving a rate here would put a device fact in a board file.
struct StSpiEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    rcc_en_reg: i64,
    rcc_en_mask: i64,
    sck: StUartPin,
    miso: StUartPin,
    mosi: StUartPin,
    cs: Option<StSoftCs>,
    pclk_hz: i64,
}

fn resolve_spi_stm32(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<StSpiEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let rcc_base = instances
        .value("rcc", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'rcc'"))?;
    let enable = |row: &str| -> Result<(i64, i64), String> {
        let off = instances
            .value(row, "rcc_en_off")
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{board}: instance '{row}' has no rcc_en_off"))?;
        let bit = instances
            .value(row, "rcc_en_bit")
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("{board}: instance '{row}' has no rcc_en_bit"))?;
        Ok((rcc_base + off, 1i64 << bit))
    };
    let (rcc_en_reg, rcc_en_mask) = enable(name)?;

    let gpio = set.block("gpio", "").ok_or_else(|| format!("{board}: no gpio block table"))?;
    let af = binding.function.strip_prefix("AF").and_then(|d| d.parse::<i64>().ok()).ok_or_else(
        || {
            format!(
                "{board}: spi binding '{}' function '{}' is not AF<n>",
                binding.role, binding.function
            )
        },
    )?;
    let pin_named = |signal: &str| -> Result<&PinRef, String> {
        binding
            .pins
            .iter()
            .find(|(s, _)| s == signal)
            .map(|(_, p)| p)
            .ok_or_else(|| format!("{board}: spi binding '{}' needs a {signal} pin", binding.role))
    };
    let mut muxed = Vec::new();
    for signal in ["sck", "miso", "mosi"] {
        let pin = pin_named(signal)?;
        if pin.soft {
            return Err(format!(
                "{board}: spi binding '{}' marks {signal} soft -- only the chip select may be a plain GPIO here; a soft clock or data line is a bit-banged master, not this binding",
                binding.role
            ));
        }
        let who = format!("spi binding '{}' {signal}", binding.role);
        muxed.push(st_mux_pin(set, board, rcc_base, gpio, af, &who, pin)?);
    }

    let cs = match binding.pins.iter().find(|(s, _)| s == "cs").map(|(_, p)| p) {
        None => None,
        Some(pin) if !pin.soft => {
            return Err(format!(
                "{board}: spi binding '{}' states a hardware chip select. This arm emits a SOFT select only: mark it `soft = true` and let the driver own the level, or leave it out and let the controller frame with its own NSS.",
                binding.role
            ));
        }
        Some(pin) => {
            let (port, index) = split_pin(&pin.pin)
                .ok_or_else(|| format!("{board}: bad chip-select pin {}", pin.pin))?;
            if index > 15 {
                return Err(format!("{board}: chip select {} exceeds a port's 16 pins", pin.pin));
            }
            let group = format!("gpio{port}");
            let group_base = instances
                .value(&group, "base")
                .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))?;
            let (port_rcc_en_reg, port_rcc_en_mask) = enable(&group)?;
            let moder = gpio.register("MODER").ok_or_else(|| format!("{board}: gpio has no MODER"))?;
            let bsrr = gpio.register("BSRR").ok_or_else(|| format!("{board}: gpio has no BSRR"))?;
            let mode_out = gpio
                .constant("MODER_MODE_OUTPUT")
                .ok_or_else(|| format!("{board}: gpio block has no MODER_MODE_OUTPUT constant"))?;
            Some(StSoftCs {
                port_rcc_en_reg,
                port_rcc_en_mask,
                moder_reg: group_base + moder.offset.value,
                moder_mask: 0b11 << (2 * index),
                moder_value: mode_out << (2 * index),
                bsrr_reg: group_base + bsrr.offset.value,
                bsrr_set: 1i64 << index,
                bsrr_clear: 1i64 << (index + 16),
            })
        }
    };

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let pclk_hz = plan
        .rate("pclk_hz")
        .ok_or_else(|| format!("{board}: default plan '{}' states no pclk_hz rate", plan.name))?;

    let mut muxed = muxed.into_iter();
    Ok(StSpiEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        rcc_en_reg,
        rcc_en_mask,
        sck: muxed.next().expect("three pins resolved"),
        miso: muxed.next().expect("three pins resolved"),
        mosi: muxed.next().expect("three pins resolved"),
        cs,
        pclk_hz,
    })
}

/// One resolved st-i2c binding emission (the newer ST I2C, the one with TIMINGR).
///
/// Two things here that a uart binding on the same part does not need:
///
/// * BOTH PINS ARE OPEN DRAIN, and that is not a preference. A push-pull output cannot be pulled
///   low by the device at the other end, so a target's acknowledge is fought instead of seen and
///   the controller reads a bus where nothing ever answers -- with the mux perfectly correct.
/// * The rate is not a divisor and is not derived from one. This block's timing is five separate
///   counts, and the manual TABULATES the compliant sets per kernel-clock rate rather than giving
///   a formula that works without knowing the bus's rise time -- which is a board's electrical
///   property, not a chip fact. So generation composes the word for each point the block states
///   AT THIS PLAN'S KERNEL RATE, the driver picks a speed, and no one writes a divisor down.
///
/// Whether an internal pull-up is wanted is deliberately NOT emitted: an open-drain bus needs a
/// pull somewhere, and whether the board already has one is board wiring nothing here states.
struct StI2cEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    rcc_en_reg: i64,
    rcc_en_mask: i64,
    scl: StUartPin,
    sda: StUartPin,
    /// (register address, bit mask) of each pin's output-type bit -- ONE bit per pin, so the
    /// shift is the pin index and not twice it. Setting the bit is open drain.
    otyper: (i64, i64, i64),
    kernel_hz: i64,
    /// (`TIMINGR_<rate>_<PLAN>` suffix, composed word) for every operating point the block
    /// tabulates at this kernel rate.
    timings: Vec<(String, i64)>,
}

fn resolve_i2c_stm32(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<StI2cEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let rcc_base = instances
        .value("rcc", "base")
        .ok_or_else(|| format!("{board}: no instance row for 'rcc'"))?;
    let off = instances
        .value(name, "rcc_en_off")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance '{name}' has no rcc_en_off"))?;
    let bit = instances
        .value(name, "rcc_en_bit")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance '{name}' has no rcc_en_bit"))?;

    let scl = binding.pins.iter().find(|(s, _)| s == "scl").map(|(_, p)| p);
    let sda = binding.pins.iter().find(|(s, _)| s == "sda").map(|(_, p)| p);
    let (Some(scl), Some(sda)) = (scl, sda) else {
        return Err(format!("{board}: i2c binding '{}' needs scl and sda pins", binding.role));
    };
    let gpio = set.block("gpio", "").ok_or_else(|| format!("{board}: no gpio block table"))?;
    let af = binding
        .function
        .strip_prefix("AF")
        .and_then(|d| d.parse::<i64>().ok())
        .ok_or_else(|| {
            format!("{board}: i2c binding '{}' function '{}' is not AF<n>", binding.role, binding.function)
        })?;
    let scl_pin = st_mux_pin(set, board, rcc_base, gpio, af, &format!("i2c binding '{}' scl", binding.role), scl)?;
    let sda_pin = st_mux_pin(set, board, rcc_base, gpio, af, &format!("i2c binding '{}' sda", binding.role), sda)?;

    let (scl_port, scl_index) = split_pin(&scl.pin).expect("validated by st_mux_pin");
    let (sda_port, sda_index) = split_pin(&sda.pin).expect("validated by st_mux_pin");
    if scl_port != sda_port {
        return Err(format!(
            "{board}: i2c binding '{}' has {} and {} in different ports -- the output-type bits would need one register address each, which is the named growth path",
            binding.role, scl.pin, sda.pin
        ));
    }
    let otyper = gpio.register("OTYPER").ok_or_else(|| format!("{board}: gpio has no OTYPER"))?;
    let group_base = instances
        .value(&format!("gpio{scl_port}"), "base")
        .expect("validated by st_mux_pin");

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let kernel_hz = plan.rate("pclk_hz").ok_or_else(|| {
        format!("{board}: default plan '{}' states no pclk_hz rate", plan.name)
    })?;
    let i2c = set.block("i2c", "").ok_or_else(|| format!("{board}: no i2c block table"))?;
    let clock_label = format!("{}MHZ", kernel_hz / 1_000_000);
    let mut timings = Vec::new();
    for rate in ["100K", "400K"] {
        let field = |name: &str| i2c.constant(&format!("TIMING_{rate}_{clock_label}_{name}"));
        let (Some(presc), Some(scldel), Some(sdadel), Some(sclh), Some(scll)) =
            (field("PRESC"), field("SCLDEL"), field("SDADEL"), field("SCLH"), field("SCLL"))
        else {
            continue;
        };
        let word = i2c.place("TIMINGR", "PRESC", presc)?
            | i2c.place("TIMINGR", "SCLDEL", scldel)?
            | i2c.place("TIMINGR", "SDADEL", sdadel)?
            | i2c.place("TIMINGR", "SCLH", sclh)?
            | i2c.place("TIMINGR", "SCLL", scll)?;
        timings.push((format!("TIMINGR_{rate}_{}", upper_snake(&plan.name)), word));
    }
    if timings.is_empty() {
        return Err(format!(
            "{board}: i2c binding '{}' runs from a {kernel_hz} Hz kernel clock, and the i2c block tabulates no compliant timing at that rate -- add the point with its source rather than interpolating one",
            binding.role
        ));
    }

    Ok(StI2cEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        base,
        rcc_en_reg: rcc_base + off,
        rcc_en_mask: 1i64 << bit,
        scl: scl_pin,
        sda: sda_pin,
        otyper: (
            group_base + otyper.offset.value,
            1i64 << scl_index,
            1i64 << sda_index,
        ),
        kernel_hz,
        timings,
    })
}

/// One resolved nrf-twi binding emission (the polled TWI): the TWI base, the PSEL register
/// VALUES ((port << 5) | pin; anchors 0x8/0x10 for P0.08/P0.16), and the per-pin PIN_CNF
/// register addresses (port base + PIN_CNF0 + stride * pin -- the nRF has no pinmux table; a
/// peripheral claims pins through PSEL and the pin's electrical config lives in PIN_CNF).
///
/// The composed PSEL word suits BOTH nRF generations, but for different reasons, so the
/// coincidence is worth stating rather than relying on. A two-port part splits the register
/// into a pin field, a port bit at 5 and a connect bit (0 = connected), which is what the
/// shift builds. A single-port part has none of those: its register is a plain pin number
/// whose disconnected value is all-ones. The shift agrees there only because that part's port
/// is always 0 -- and a pin naming any other port fails earlier, when its port group has no
/// instance row, rather than silently composing a word the register cannot mean.
/// No divisor derivation: the TWI FREQUENCY register is ENUMERATED (K100/K250/K400 layout
/// constants); nothing derives from a plan rate.
struct NrfTwiEmission {
    prefix: String,
    role: String,
    instance: String,
    twi_base: i64,
    psel_scl: i64,
    psel_sda: i64,
    pin_cnf_scl_reg: i64,
    pin_cnf_sda_reg: i64,
}

fn resolve_i2c_nrf(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<NrfTwiEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let twi_base =
        instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;

    let signal = |signal_name: &str| -> Result<&PinRef, String> {
        binding
            .pins
            .iter()
            .find(|(s, _)| s == signal_name)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                format!("{board}: i2c binding '{}' needs a {signal_name} pin", binding.role)
            })
    };
    let scl = signal("scl")?;
    let sda = signal("sda")?;

    let gpio = set.block("gpio", "").ok_or_else(|| format!("{board}: no gpio block table"))?;
    let cnf0 = gpio
        .register("PIN_CNF0")
        .ok_or_else(|| format!("{board}: gpio has no PIN_CNF0"))?;
    let stride = gpio
        .constant("PIN_CNF_STRIDE")
        .ok_or_else(|| format!("{board}: gpio block has no PIN_CNF_STRIDE constant"))?;

    let resolve_pin = |pin: &PinRef| -> Result<(i64, i64), String> {
        let (port, index) = split_pin(&pin.pin).ok_or_else(|| format!("{board}: bad pin {}", pin.pin))?;
        let port_digit = port
            .to_digit(10)
            .ok_or_else(|| format!("{board}: '{}' is not a P<port>.<pin> nRF pin", pin.pin))?;
        if index > 31 {
            return Err(format!("{board}: pin index {index} exceeds the PSEL PIN field (0..31)"));
        }
        let group = format!("port{port}");
        let group_base = instances
            .value(&group, "base")
            .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))?;
        let psel = (i64::from(port_digit) << 5) | i64::from(index);
        let pin_cnf = group_base + cnf0.offset.value + stride * i64::from(index);
        Ok((psel, pin_cnf))
    };
    let (psel_scl, pin_cnf_scl_reg) = resolve_pin(scl)?;
    let (psel_sda, pin_cnf_sda_reg) = resolve_pin(sda)?;

    Ok(NrfTwiEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        twi_base,
        psel_scl,
        psel_sda,
        pin_cnf_scl_reg,
        pin_cnf_sda_reg,
    })
}

/// One resolved sercom-spi binding emission (the samd21 shape): instance facts plus the
/// UNSHIFTED core-clock id -- a spi
/// binding may ride TWO plans (the W25's SERCOM2: gen 1 under usb-48mhz, gen 0 under
/// osc8m-8mhz, runtime-selected), so a composed CLKCTRL word would be the wrong stratum fact;
/// the consumer composes ID | GEN | CLKEN itself. Per-signal PMUX byte addresses + nibble
/// shifts instead of the uart's shared-pair byte: spi pins straddle PMUX pairs with a GPIO
/// neighbor that must stay unmuxed (the W25's MISO PA15 rides pair 7's odd nibble beside the
/// soft-CS PA14). DOPO/DIPO derive from the pad claims like TXPO/RXPO. NO BAUD emission until
/// the spi block states the formula (the wifi divisor is a runtime plan choice today).
struct SpiEmission {
    prefix: String,
    role: String,
    instance: String,
    sercom_base: i64,
    irq: i64,
    apbc_mask: i64,
    gclk_core_id: i64,
    /// Per muxed signal, in (mosi, sck, miso) order: (name, pmux_reg, pmux_shift, pincfg_reg).
    signals: Vec<(String, i64, i64, i64)>,
    /// The PMUX nibble VALUE the binding's function letter resolves to (the port block's
    /// `FUNC_<letter>`). Emitted because a driver cannot compose a PMUX byte without it, and the
    /// alternative is for each driver to carry a `PMUX_FUNC_C = 0x2` literal of its own --
    /// a hardware fact stranded in hand-written code.
    pmux_func: i64,
    dopo: i64,
    dipo: i64,
    cs_port_base: i64,
    cs_pin: i64,
    cs_mask: i64,
}

fn resolve_spi(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    binding: &Binding,
) -> Result<SpiEmission, String> {
    let board = &resolved.board.board;
    let instances = &set.instances;
    let name = &binding.instance;
    let base = instances.value(name, "base").ok_or_else(|| format!("{board}: no base for {name}"))?;
    let irq = instances.value(name, "irq").unwrap_or(-1);
    let gclk_core_id = instances
        .value(name, "gclk_core_id")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance {name} has no gclk_core_id"))?;
    let apbc_bit = instances
        .value(name, "apbc_bit")
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("{board}: instance {name} has no apbc_bit"))?;

    let signal = |signal_name: &str| -> Result<&PinRef, String> {
        binding
            .pins
            .iter()
            .find(|(s, _)| s == signal_name)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                format!("{board}: spi binding '{}' needs a {signal_name} pin", binding.role)
            })
    };
    let mosi = signal("mosi")?;
    let sck = signal("sck")?;
    let miso = signal("miso")?;
    let cs = signal("cs")?;
    if !cs.soft {
        return Err(format!(
            "{board}: spi binding '{}' has a MUXED chip select. This emitter states a chip select the driver drives as a plain output, so a peripheral-muxed one has no emission to name",
            binding.role
        ));
    }

    let dopo = match (mosi.pad, sck.pad) {
        (0, 1) => 0,
        (2, 3) => 1,
        (mosi_pad, sck_pad) => {
            return Err(format!(
                "{board}: spi binding '{}' routes mosi pad{mosi_pad} + sck pad{sck_pad} -- only pad0+pad1 (DOPO 0) and pad2+pad3 (DOPO 1) are implemented; add the encoding with its anchor first",
                binding.role
            ));
        }
    };
    let dipo = miso.pad;
    if !(0..=3).contains(&dipo) {
        return Err(format!("{board}: MISO pad must be 0..3, got {dipo}"));
    }

    let port = set.block("port", "").ok_or_else(|| format!("{board}: no port block table"))?;
    let pmux0 = port.register("PMUX0").ok_or_else(|| format!("{board}: port has no PMUX0"))?;
    let pincfg0 = port.register("PINCFG0").ok_or_else(|| format!("{board}: port has no PINCFG0"))?;
    let group_base = |pin: &str| -> Result<i64, String> {
        let (port_letter, _) = split_pin(pin).ok_or_else(|| format!("{board}: bad pin {pin}"))?;
        let group = format!("port{port_letter}");
        instances
            .value(&group, "base")
            .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))
    };

    let mut signals = Vec::new();
    for (label, pin) in [("mosi", mosi), ("sck", sck), ("miso", miso)] {
        let (_, index) = split_pin(&pin.pin).ok_or_else(|| format!("{board}: bad pin {}", pin.pin))?;
        let base = group_base(&pin.pin)?;
        signals.push((
            label.to_string(),
            base + pmux0.offset.value + i64::from(index / 2),
            i64::from(index % 2) * 4,
            base + pincfg0.offset.value + i64::from(index),
        ));
    }

    let pmux_func = port
        .constant(&format!("FUNC_{}", binding.function.to_ascii_uppercase()))
        .ok_or_else(|| format!("{board}: port block has no FUNC_{} constant", binding.function))?;

    let (_, cs_index) = split_pin(&cs.pin).ok_or_else(|| format!("{board}: bad pin {}", cs.pin))?;
    Ok(SpiEmission {
        prefix: upper_snake(&binding.role),
        role: binding.role.clone(),
        instance: binding.instance.clone(),
        sercom_base: base,
        irq,
        apbc_mask: 1i64 << apbc_bit,
        gclk_core_id,
        signals,
        pmux_func,
        dopo,
        dipo,
        cs_port_base: group_base(&cs.pin)?,
        cs_pin: i64::from(cs_index),
        cs_mask: 1i64 << cs_index,
    })
}

/// The per-family uart/spi resolutions of a board's bindings, plus the skip notes for kinds
/// without an emitter -- shared by the C# and Rust emitters so they can never disagree.
struct BoardEmissions {
    skipped: Vec<String>,
    /// Per emitted role, `(role, driver family)` -- see [`driver_family`].
    driver_families: Vec<(String, String)>,
    sercom_uarts: Vec<UartEmission>,
    rp_uarts: Vec<RpUartEmission>,
    esp_uarts: Vec<EspUartEmission>,
    sam3x_uarts: Vec<Sam3xUartEmission>,
    st_uarts: Vec<StUartEmission>,
    st_i2cs: Vec<StI2cEmission>,
    sercom_spis: Vec<SpiEmission>,
    sercom_i2cs: Vec<SercomI2cEmission>,
    pl022_spis: Vec<SpiPl022Emission>,
    st_spis: Vec<StSpiEmission>,
    nrf_twis: Vec<NrfTwiEmission>,
    dw_i2cs: Vec<DwI2cEmission>,
    rp_adcs: Vec<RpAdcEmission>,
    rp_clocks: Vec<RpClockEmission>,
}

/// The driver family a role's bound instance belongs to: `<family>-<block>`.
///
/// A ROLE DESCRIPTOR ALREADY SAYS WHAT SURFACE IT IS, AND NOT WHAT SILICON IT IS. `kind` is what
/// the application asked for -- a uart, an spi -- and two peripherals with the same `kind` can
/// share no register whatsoever. So a consumer that picks a driver at run time had only the board
/// identity to key on, which is the same board-to-driver table one level away, or the shape of the
/// fact set, which is inference.
///
/// This is neither: an instance names the BLOCK it is a copy of, and a block table is per family,
/// so the pair (family, block) is exactly "which register map". It is derived at generation time
/// from facts that are already gated, so it can never be transcribed wrongly and can never drift
/// from the descriptor beside it.
///
/// **`kind` AND THIS TOGETHER NAME A DRIVER; NEITHER ALONE DOES.** The SAMD21 is why: its six
/// SERCOM instances all name the block `sercom`, and a SERCOM is a uart, an spi or an i2c
/// depending on how it is configured -- three drivers over one register map. So `samd21-sercom`
/// does not say which driver, and `uart` does not say which silicon. The two fields divide the
/// question cleanly: **`kind` is what the application asked for, this is what the silicon is.**
///
/// Deliberately NOT composed into a single `<family>-<block>-<kind>` string. On families where the
/// block and the surface share a name that reads `rp2350-uart-uart`, and a spelling with a special
/// case in it is one that gets written wrongly.
///
/// Deliberately NOT an IP name like `pl011`, either. The block id is what the strata state; naming
/// the IP would be a new fact nobody has checked, and it belongs in a block table as its own
/// ratifiable column if it is ever wanted. Two families that turn out to share an IP get two keys
/// pointing at one driver, which is honest -- one key would assert a register-level identity that
/// has not been established.
fn driver_family(set: &FamilySet, binding: &Binding) -> Result<String, String> {
    let instance = set.instances.row(&binding.instance).ok_or_else(|| {
        format!(
            "binding '{}' names instance '{}', which csp/{}/instances.toml does not place",
            binding.role, binding.instance, set.family
        )
    })?;
    Ok(format!("{}-{}", set.family, instance.block))
}

fn resolve_board_emissions(set: &FamilySet, resolved: &ResolvedBoard) -> Result<BoardEmissions, String> {
    let mut emissions = BoardEmissions {
        skipped: Vec::new(),
        driver_families: Vec::new(),
        sercom_uarts: Vec::new(),
        rp_uarts: Vec::new(),
        esp_uarts: Vec::new(),
        sam3x_uarts: Vec::new(),
        st_uarts: Vec::new(),
        st_i2cs: Vec::new(),
        sercom_spis: Vec::new(),
        sercom_i2cs: Vec::new(),
        pl022_spis: Vec::new(),
        st_spis: Vec::new(),
        nrf_twis: Vec::new(),
        dw_i2cs: Vec::new(),
        rp_adcs: Vec::new(),
        rp_clocks: resolve_clocks_rp(set, resolved)?,
    };
    for binding in &resolved.bindings {
        let emitted_before = emissions.skipped.len();
        match binding.kind.as_str() {
            "uart" => match set.family.as_str() {
                "samd21" => emissions.sercom_uarts.push(resolve_uart(set, resolved, binding)?),
                "rp2040" => emissions.rp_uarts.push(resolve_uart_rp(set, resolved, binding, false)?),
                "rp2350" => emissions.rp_uarts.push(resolve_uart_rp(set, resolved, binding, true)?),
                "esp32c6" => emissions.esp_uarts.push(resolve_uart_esp32c6(set, resolved, binding)?),
                "sam3x" => emissions.sam3x_uarts.push(resolve_uart_sam3x(set, resolved, binding)?),
                "stm32l476" | "stm32f091" | "stm32f7" | "stm32f42x" | "stm32f769" | "stm32h7" => {
                    emissions.st_uarts.push(resolve_uart_stm32(set, resolved, binding)?);
                }
                other => {
                    return Err(format!(
                        "{}: no uart emission shape for family '{other}' -- add its derivation path first",
                        resolved.board.board
                    ));
                }
            },
            "spi" => match set.family.as_str() {
                "samd21" => emissions.sercom_spis.push(resolve_spi(set, resolved, binding)?),
                "rp2350" => emissions.pl022_spis.push(resolve_spi_pl022(set, resolved, binding)?),
                "stm32l476" | "stm32f091" | "stm32f7" | "stm32f42x" => {
                    emissions.st_spis.push(resolve_spi_stm32(set, resolved, binding)?);
                }
                other => {
                    return Err(format!(
                        "{}: no spi emission shape for family '{other}' -- add its derivation path first",
                        resolved.board.board
                    ));
                }
            },
            "i2c" => match set.family.as_str() {
                "samd21" => emissions.sercom_i2cs.push(resolve_i2c_samd21(set, resolved, binding)?),
                "nrf52833" | "nrf51" => {
                    emissions.nrf_twis.push(resolve_i2c_nrf(set, resolved, binding)?)
                }
                "rp2350" => emissions.dw_i2cs.push(resolve_i2c_dw(set, resolved, binding)?),
                "stm32l476" | "stm32f091" | "stm32f7" => {
                    emissions.st_i2cs.push(resolve_i2c_stm32(set, resolved, binding)?);
                }
                other => {
                    return Err(format!(
                        "{}: no i2c emission shape for family '{other}' -- add its derivation path first",
                        resolved.board.board
                    ));
                }
            },
            "adc" => match set.family.as_str() {
                "rp2350" => emissions.rp_adcs.push(resolve_adc_rp(set, resolved, binding)?),
                other => {
                    return Err(format!(
                        "{}: no adc emission shape for family '{other}' -- add its derivation path first",
                        resolved.board.board
                    ));
                }
            },
            other => emissions.skipped.push(format!("{} ({other})", binding.role)),
        }
        if emissions.skipped.len() == emitted_before {
            emissions.driver_families.push((binding.role.clone(), driver_family(set, binding)?));
        }
    }
    Ok(emissions)
}

/// Emits a board's C# bindings class: per uart role the resolved descriptor values, control
/// pins for module lines and devices, and the carrier identity. Binding kinds without an
/// emitter yet are SKIPPED with a named note in the header (loud, forward-compatible).
pub fn emit_board_csharp(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    sources: &[String],
    regen: &str,
) -> Result<String, String> {
    let class = bindings_class(&resolved.board.board);
    let BoardEmissions {
        skipped,
        driver_families,
        sercom_uarts: uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        st_i2cs,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
        st_spis,
        nrf_twis,
        dw_i2cs,
        rp_adcs,
        rp_clocks,
    } = resolve_board_emissions(set, resolved)?;

    let mut out = String::new();
    let mut what = format!(
        "The {} board BINDINGS (resolved against the {} chip strata): every value below is a\n// generation-time literal derived from the strata -- role descriptors, module control\n// lines, and the carrier identity. Board truth lives in board.toml, never here.",
        resolved.board.board, set.family
    );
    if !skipped.is_empty() {
        what.push_str(&format!(
            "\n// NOT YET EMITTED (no emitter for these binding kinds): {}.",
            skipped.join(", ")
        ));
    }
    emit_header(&mut out, &class, &what, sources, regen);

    out.push_str("\n        // -- identity --\n");
    push_const(&mut out, "int", "BOARD_MODEL", &resolved.board.board_model.to_string());
    push_const(&mut out, "string", "BOARD_VENDOR", &format!("\"{}\"", vendor_segment(&resolved.board.vendor)));
    if resolved.board.carrier.usb_vid > 0 {
        push_const(&mut out, "uint", "CARRIER_USB_VID", &format!("0x{:04X}", resolved.board.carrier.usb_vid));
    }
    if resolved.board.carrier.usb_pid > 0 {
        push_const(&mut out, "uint", "CARRIER_USB_PID", &format!("0x{:04X}", resolved.board.carrier.usb_pid));
    }

    if !driver_families.is_empty() {
        out.push_str(&driver_family_note("        "));
        for (role, family) in &driver_families {
            push_const(
                &mut out,
                "string",
                &format!("{}_DRIVER_FAMILY", upper_snake(role)),
                &format!("\"{family}\""),
            );
        }
    }

    for uart in &uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n        // -- {p}: a sercom-usart binding descriptor --\n"));
        push_const(&mut out, "uint", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", uart.sercom_base));
        if uart.irq >= 0 {
            push_const(&mut out, "uint", &format!("{p}_IRQ"), &uart.irq.to_string());
        }
        push_const(&mut out, "uint", &format!("{p}_GCLK_CLKCTRL_VALUE"), &format!("0x{:X}", uart.gclk_clkctrl_value));
        push_const(&mut out, "uint", &format!("{p}_APBC_MASK"), &format!("0x{:X}", uart.apbc_mask));
        push_const(&mut out, "uint", &format!("{p}_PMUX_REG"), &format!("0x{:X}", uart.pmux_reg));
        push_const(&mut out, "uint", &format!("{p}_PMUX_PAIR"), &format!("0x{:X}", uart.pmux_pair));
        push_const(&mut out, "uint", &format!("{p}_PINCFG_TX_REG"), &format!("0x{:X}", uart.pincfg_tx_reg));
        push_const(&mut out, "uint", &format!("{p}_PINCFG_RX_REG"), &format!("0x{:X}", uart.pincfg_rx_reg));
        push_const(&mut out, "uint", &format!("{p}_TXPO"), &uart.txpo.to_string());
        push_const(&mut out, "uint", &format!("{p}_RXPO"), &uart.rxpo.to_string());
        for (suffix, divisor) in &uart.bauds {
            push_const(&mut out, "uint", &format!("{p}_{suffix}"), &format!("0x{divisor:X}"));
        }
    }

    for uart in &rp_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n        // -- {p}: a pl011 uart binding descriptor --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_const(&mut out, "uint", &format!("{p}_RESET_MASK"), &format!("0x{:X}", uart.reset_mask));
        push_const(&mut out, "uint", &format!("{p}_IO_TX_CTRL"), &format!("0x{:X}", uart.io_tx_ctrl));
        push_const(&mut out, "uint", &format!("{p}_IO_RX_CTRL"), &format!("0x{:X}", uart.io_rx_ctrl));
        if let Some((pads_tx, pads_rx)) = &uart.pads {
            push_const(&mut out, "uint", &format!("{p}_PADS_TX"), &format!("0x{pads_tx:X}"));
            push_const(&mut out, "uint", &format!("{p}_PADS_RX"), &format!("0x{pads_rx:X}"));
        }
        push_const(&mut out, "uint", &format!("{p}_FUNCSEL"), &uart.funcsel.to_string());
        push_const(&mut out, "uint", &format!("{p}_CLK_PERI_HZ"), &uart.clk_peri_hz.to_string());
        for (suffix, ibrd, fbrd) in &uart.bauds {
            push_const(&mut out, "uint", &format!("{p}_IBRD_{suffix}"), &ibrd.to_string());
            push_const(&mut out, "uint", &format!("{p}_FBRD_{suffix}"), &fbrd.to_string());
        }
    }

    for uart in &esp_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n        // -- {p}: an esp32c6 hp-uart binding descriptor --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_const(&mut out, "uint", &format!("{p}_PCR_CONF"), &format!("0x{:X}", uart.pcr_conf));
        push_const(&mut out, "uint", &format!("{p}_PCR_SCLK_CONF"), &format!("0x{:X}", uart.pcr_sclk_conf));
        push_const(&mut out, "uint", &format!("{p}_IO_MUX_TX"), &format!("0x{:X}", uart.io_mux_tx));
        push_const(&mut out, "uint", &format!("{p}_IO_MUX_RX"), &format!("0x{:X}", uart.io_mux_rx));
        push_const(&mut out, "uint", &format!("{p}_MCU_SEL"), &uart.mcu_sel.to_string());
        push_const(&mut out, "uint", &format!("{p}_SCLK_HZ"), &uart.sclk_hz.to_string());
    }

    for uart in &sam3x_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n        // -- {p}: a sam3x uart binding descriptor --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_const(&mut out, "uint", &format!("{p}_PID"), &uart.pid.to_string());
        push_const(&mut out, "uint", &format!("{p}_PMC_PCER_REG"), &format!("0x{:X}", uart.pmc_pcer_reg));
        push_const(&mut out, "uint", &format!("{p}_PMC_PCER_MASK"), &format!("0x{:X}", uart.pmc_pcer_mask));
        push_const(&mut out, "uint", &format!("{p}_PIO_PDR_REG"), &format!("0x{:X}", uart.pio_pdr_reg));
        push_const(&mut out, "uint", &format!("{p}_PIO_ABSR_REG"), &format!("0x{:X}", uart.pio_absr_reg));
        push_const(&mut out, "uint", &format!("{p}_PIO_MASK"), &format!("0x{:X}", uart.pio_mask));
        push_const(&mut out, "uint", &format!("{p}_PIO_FUNC"), &uart.pio_func.to_string());
        push_const(&mut out, "uint", &format!("{p}_MCK_HZ"), &uart.mck_hz.to_string());
        for (suffix, cd) in &uart.bauds {
            push_const(&mut out, "uint", &format!("{p}_BRGR_CD_{suffix}"), &cd.to_string());
        }
    }

    for uart in &st_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n        // -- {p}: an st-usart binding descriptor --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_const(&mut out, "uint", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", uart.rcc_en_reg));
        push_const(&mut out, "uint", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", uart.rcc_en_mask));
        for (side, pin) in [("TX", &uart.tx), ("RX", &uart.rx)] {
            push_const(&mut out, "uint", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        push_const(&mut out, "uint", &format!("{p}_PCLK_HZ"), &uart.pclk_hz.to_string());
        for (suffix, divisor) in &uart.bauds {
            push_const(&mut out, "uint", &format!("{p}_{suffix}"), &format!("0x{divisor:X}"));
        }
    }

    for spi in &sercom_spis {
        let p = &spi.prefix;
        out.push_str(&format!("\n        // -- {p}: a sercom-spi binding descriptor (core-clock id UNSHIFTED: the\n        // consumer composes ID | GEN | CLKEN per its runtime-selected plan) --\n"));
        push_const(&mut out, "uint", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", spi.sercom_base));
        if spi.irq >= 0 {
            push_const(&mut out, "uint", &format!("{p}_IRQ"), &spi.irq.to_string());
        }
        push_const(&mut out, "uint", &format!("{p}_APBC_MASK"), &format!("0x{:X}", spi.apbc_mask));
        push_const(&mut out, "uint", &format!("{p}_GCLK_CORE_ID"), &spi.gclk_core_id.to_string());
        for (signal, pmux_reg, pmux_shift, pincfg_reg) in &spi.signals {
            let s = upper_snake(signal);
            push_const(&mut out, "uint", &format!("{p}_PMUX_{s}_REG"), &format!("0x{pmux_reg:X}"));
            push_const(&mut out, "uint", &format!("{p}_PMUX_{s}_SHIFT"), &pmux_shift.to_string());
            push_const(&mut out, "uint", &format!("{p}_PINCFG_{s}_REG"), &format!("0x{pincfg_reg:X}"));
        }
        push_const(&mut out, "uint", &format!("{p}_PMUX_FUNC"), &spi.pmux_func.to_string());
        push_const(&mut out, "uint", &format!("{p}_DOPO"), &spi.dopo.to_string());
        push_const(&mut out, "uint", &format!("{p}_DIPO"), &spi.dipo.to_string());
        push_const(&mut out, "uint", &format!("{p}_CS_PORT_BASE"), &format!("0x{:X}", spi.cs_port_base));
        push_const(&mut out, "uint", &format!("{p}_CS_PIN"), &spi.cs_pin.to_string());
        push_const(&mut out, "uint", &format!("{p}_CS_MASK"), &format!("0x{:X}", spi.cs_mask));
    }
    for i2c in &sercom_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("
        // -- {p}: a sercom-i2c binding descriptor (the CORE-CLOCK RATE, not a
        // divisor: an I2C bus speed is a runtime Configure choice) --
"));
        push_const(&mut out, "uint", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", i2c.sercom_base));
        if i2c.irq >= 0 {
            push_const(&mut out, "uint", &format!("{p}_IRQ"), &i2c.irq.to_string());
        }
        push_const(&mut out, "uint", &format!("{p}_GCLK_CLKCTRL_VALUE"), &format!("0x{:X}", i2c.gclk_clkctrl_value));
        push_const(&mut out, "uint", &format!("{p}_APBC_MASK"), &format!("0x{:X}", i2c.apbc_mask));
        push_const(&mut out, "uint", &format!("{p}_PMUX_REG"), &format!("0x{:X}", i2c.pmux_reg));
        push_const(&mut out, "uint", &format!("{p}_PMUX_PAIR"), &format!("0x{:X}", i2c.pmux_pair));
        push_const(&mut out, "uint", &format!("{p}_PINCFG_SDA_REG"), &format!("0x{:X}", i2c.pincfg_sda_reg));
        push_const(&mut out, "uint", &format!("{p}_PINCFG_SCL_REG"), &format!("0x{:X}", i2c.pincfg_scl_reg));
        push_const(&mut out, "uint", &format!("{p}_CORE_CLOCK_HZ"), &i2c.core_clock_hz.to_string());
    }

    for spi in &pl022_spis {
        let p = &spi.prefix;
        out.push_str(&format!("\n        // -- {p}: a pl022 spi binding descriptor (rate = a runtime Configure choice\n        // from SSPCLK, like the samd21 spi BAUD -- no divisor emits) --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", spi.base));
        push_const(&mut out, "uint", &format!("{p}_RESET_MASK"), &format!("0x{:X}", spi.reset_mask));
        for (signal, io_ctrl, pads) in &spi.signals {
            let s = upper_snake(signal);
            push_const(&mut out, "uint", &format!("{p}_IO_{s}_CTRL"), &format!("0x{io_ctrl:X}"));
            push_const(&mut out, "uint", &format!("{p}_PADS_{s}"), &format!("0x{pads:X}"));
        }
        push_const(&mut out, "uint", &format!("{p}_FUNCSEL"), &spi.funcsel.to_string());
        push_const(&mut out, "uint", &format!("{p}_SSPCLK_HZ"), &spi.sspclk_hz.to_string());
    }


    for spi in &st_spis {
        let p = &spi.prefix;
        out.push_str(&format!("
        // -- {p}: an st-spi binding descriptor. NO baud emits: an SPI master's rate is a property of the ATTACHED DEVICE, and a binding names a bus. The APB rate is here and the block table states the eight prescaler codes; the driver picks --
"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", spi.base));
        push_const(&mut out, "uint", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", spi.rcc_en_reg));
        push_const(&mut out, "uint", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", spi.rcc_en_mask));
        for (side, pin) in [("SCK", &spi.sck), ("MISO", &spi.miso), ("MOSI", &spi.mosi)] {
            push_const(&mut out, "uint", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        if let Some(cs) = &spi.cs {
            push_const(&mut out, "uint", &format!("{p}_CS_PORT_RCC_EN_REG"), &format!("0x{:X}", cs.port_rcc_en_reg));
            push_const(&mut out, "uint", &format!("{p}_CS_PORT_RCC_EN_MASK"), &format!("0x{:X}", cs.port_rcc_en_mask));
            push_const(&mut out, "uint", &format!("{p}_CS_MODER_REG"), &format!("0x{:X}", cs.moder_reg));
            push_const(&mut out, "uint", &format!("{p}_CS_MODER_MASK"), &format!("0x{:X}", cs.moder_mask));
            push_const(&mut out, "uint", &format!("{p}_CS_MODER_VALUE"), &format!("0x{:X}", cs.moder_value));
            push_const(&mut out, "uint", &format!("{p}_CS_BSRR_REG"), &format!("0x{:X}", cs.bsrr_reg));
            push_const(&mut out, "uint", &format!("{p}_CS_BSRR_SET"), &format!("0x{:X}", cs.bsrr_set));
            push_const(&mut out, "uint", &format!("{p}_CS_BSRR_CLEAR"), &format!("0x{:X}", cs.bsrr_clear));
        }
        push_const(&mut out, "uint", &format!("{p}_PCLK_HZ"), &spi.pclk_hz.to_string());
    }

    for i2c in &st_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("\n        // -- {p}: an st-i2c binding descriptor. BOTH PINS ARE OPEN DRAIN --\n        // a push-pull output cannot be pulled low by the device at the other end, so an\n        // acknowledge is fought instead of seen and nothing ever answers, with the mux\n        // perfectly correct. The timing words are the manual's own compliant points at\n        // this plan's kernel rate, composed here: five counts, not a divisor --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", i2c.base));
        push_const(&mut out, "uint", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", i2c.rcc_en_reg));
        push_const(&mut out, "uint", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", i2c.rcc_en_mask));
        for (side, pin) in [("SCL", &i2c.scl), ("SDA", &i2c.sda)] {
            push_const(&mut out, "uint", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_const(&mut out, "uint", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        push_const(&mut out, "uint", &format!("{p}_OTYPER_REG"), &format!("0x{:X}", i2c.otyper.0));
        push_const(&mut out, "uint", &format!("{p}_OTYPER_SCL_MASK"), &format!("0x{:X}", i2c.otyper.1));
        push_const(&mut out, "uint", &format!("{p}_OTYPER_SDA_MASK"), &format!("0x{:X}", i2c.otyper.2));
        push_const(&mut out, "uint", &format!("{p}_KERNEL_HZ"), &i2c.kernel_hz.to_string());
        for (suffix, word) in &i2c.timings {
            push_const(&mut out, "uint", &format!("{p}_{suffix}"), &format!("0x{word:X}"));
        }
    }
    for twi in &nrf_twis {
        let p = &twi.prefix;
        out.push_str(&format!("\n        // -- {p}: an nrf-twi binding descriptor --\n"));
        push_const(&mut out, "uint", &format!("{p}_TWI_BASE"), &format!("0x{:X}", twi.twi_base));
        push_const(&mut out, "uint", &format!("{p}_PSEL_SCL"), &format!("0x{:X}", twi.psel_scl));
        push_const(&mut out, "uint", &format!("{p}_PSEL_SDA"), &format!("0x{:X}", twi.psel_sda));
        push_const(&mut out, "uint", &format!("{p}_PIN_CNF_SCL_REG"), &format!("0x{:X}", twi.pin_cnf_scl_reg));
        push_const(&mut out, "uint", &format!("{p}_PIN_CNF_SDA_REG"), &format!("0x{:X}", twi.pin_cnf_sda_reg));
    }

    for i2c in &dw_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("\n        // -- {p}: a dw-i2c binding descriptor (ic_clk = clk_sys on this chip; the\n        // SCL count formulas stay driver math per the official pico-sdk driver) --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", i2c.base));
        push_const(&mut out, "uint", &format!("{p}_RESET_MASK"), &format!("0x{:X}", i2c.reset_mask));
        push_const(&mut out, "uint", &format!("{p}_IO_SDA_CTRL"), &format!("0x{:X}", i2c.io_sda_ctrl));
        push_const(&mut out, "uint", &format!("{p}_IO_SCL_CTRL"), &format!("0x{:X}", i2c.io_scl_ctrl));
        push_const(&mut out, "uint", &format!("{p}_PADS_SDA"), &format!("0x{:X}", i2c.pads_sda));
        push_const(&mut out, "uint", &format!("{p}_PADS_SCL"), &format!("0x{:X}", i2c.pads_scl));
        push_const(&mut out, "uint", &format!("{p}_FUNCSEL"), &i2c.funcsel.to_string());
        push_const(&mut out, "uint", &format!("{p}_IC_CLK_HZ"), &i2c.ic_clk_hz.to_string());
    }

    for adc in &rp_adcs {
        let p = &adc.prefix;
        out.push_str(&format!("\n        // -- {p}: an rp-adc binding descriptor (the reference rail is BOARD truth;\n        // the channel map + calibration records are chip truth in the adc layout) --\n"));
        push_const(&mut out, "uint", &format!("{p}_BASE"), &format!("0x{:X}", adc.base));
        push_const(&mut out, "uint", &format!("{p}_RESET_MASK"), &format!("0x{:X}", adc.reset_mask));
        push_const(&mut out, "uint", &format!("{p}_REFERENCE_UV"), &adc.reference_uv.to_string());
    }

    for clock in &rp_clocks {
        let s = &clock.plan;
        out.push_str("\n        // -- the clock plan (state-and-verify: the plan states the chosen PLL\n        // values AND the resulting rates; generation verified hz == xosc * fbdiv /\n        // (postdiv1 * postdiv2) and composed each PRIM word from the pll block fields).\n        // Every const carries the plan name as a suffix, the default plan included, so a\n        // name states which operating point it means -- a board may declare several --\n");
        push_const(&mut out, "uint", &format!("XOSC_HZ_{s}"), &clock.xosc_hz.to_string());
        push_const(&mut out, "uint", &format!("CLK_SYS_HZ_{s}"), &clock.clk_sys_hz.to_string());
        push_const(&mut out, "uint", &format!("CLK_USB_HZ_{s}"), &clock.clk_usb_hz.to_string());
        if clock.clk_adc_hz >= 0 {
            push_const(&mut out, "uint", &format!("CLK_ADC_HZ_{s}"), &clock.clk_adc_hz.to_string());
        }
        push_const(&mut out, "uint", &format!("PLL_SYS_FBDIV_{s}"), &clock.pll_sys_fbdiv.to_string());
        push_const(&mut out, "uint", &format!("PLL_SYS_PRIM_{s}"), &format!("0x{:X}", clock.pll_sys_prim));
        push_const(&mut out, "uint", &format!("PLL_USB_FBDIV_{s}"), &clock.pll_usb_fbdiv.to_string());
        push_const(&mut out, "uint", &format!("PLL_USB_PRIM_{s}"), &format!("0x{:X}", clock.pll_usb_prim));
    }

    let controls: Vec<(&str, &[ControlPin])> = vec![
        ("module control lines", &resolved.module_pins),
        ("on-board devices", &resolved.board.devices),
    ];
    for (label, pins) in controls {
        if pins.is_empty() {
            continue;
        }
        out.push_str(&format!("\n        // -- {label}: PORT group base + pin index + mask --\n"));
        for control in pins {
            if control.pin.is_empty() {
                let p = upper_snake(&control.name);
                push_const(&mut out, "uint", &format!("{p}_ADDRESS"), &format!("0x{:X}", control.address));
                continue;
            }
            let (group_base, index) = control_pin_group_base(set, &resolved.board.board, &control.pin)?;
            let p = upper_snake(&control.name);
            push_const(&mut out, "uint", &format!("{p}_PORT_BASE"), &format!("0x{group_base:X}"));
            push_const(&mut out, "uint", &format!("{p}_PIN"), &index.to_string());
            push_const(&mut out, "uint", &format!("{p}_MASK"), &format!("0x{:X}", 1u64 << index));
            push_const(
                &mut out,
                "uint",
                &format!("{p}_ACTIVE_LOW"),
                if control.active == "low" { "1" } else { "0" },
            );
        }
    }

    let rows_at = board_fact_rows(set, resolved)?;
    for (at, row) in rows_at.iter().enumerate() {
        let documents = documents_something(&rows_at[at + 1..]);
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "        ", if documents { "///" } else { "//" }, comment),
            Row::Uint(name, value) => push_const(&mut out, "uint", name, value),
            Row::Int(name, value) => push_const(&mut out, "int", name, value),
            Row::Str(name, value) => push_const(&mut out, "string", name, &format!("\"{value}\"")),
        }
    }

    finish_class(&mut out)?;
    Ok(out)
}

/// A control pin's GPIO-group row base + pin index. The rp-family's bank-less GP pins are driven
/// through the SIO block (the vendor's own instance name, base SIO 0xd0000000).
///
/// EVERY OTHER FAMILY RESOLVES A LETTERED OR NUMBERED PORT BY NAME, AND THE NAME IS THE VENDOR'S.
/// Microchip and Nordic call a GPIO group a PORT (`porta`, `port0`); ST calls it a GPIO (`gpioa`).
/// Both spellings mean the same thing, and an instance row keeps whichever name the part's own
/// manual uses rather than taking one vendor's word across all of them, so both are accepted. The
/// alternative is renaming a family's rows, which moves every emitted `PORTA_BASE` / `GPIOA_BASE`
/// constant and every driver that reads one -- a spelling preference paid for in published names.
///
/// NEITHER FIELD PREDICTS THE OTHER, so a row cannot be found from its block alone: Nordic's ports
/// place a block called `gpio` and are named `port0` and `port1`. A family that spells a group a
/// third way is a row here, and the error below names both spellings it looked for.
fn control_pin_group_base(set: &FamilySet, board: &str, pin: &str) -> Result<(i64, u32), String> {
    let Some((port, index)) = split_pin(pin) else {
        return Err(format!("{board}: bad control pin {pin}"));
    };
    if port == 'g' && set.family.starts_with("rp") {
        let base = set
            .instances
            .value("sio", "base")
            .ok_or_else(|| format!("{board}: no instance row for 'sio'"))?;
        return Ok((base, index));
    }
    if let Some(row) = set.instances.rows.iter().find(|row| row.port_char() == Some(port)) {
        if let Some(base) = set.instances.value(&row.name, "base") {
            return Ok((base, index));
        }
    }
    let spellings = [format!("port{port}"), format!("gpio{port}")];
    for group in &spellings {
        if let Some(base) = set.instances.value(group, "base") {
            return Ok((base, index));
        }
    }
    Err(format!(
        "{board}: no instance row for '{}' or '{}', and no instance declares `port` for the group \
         control pin {pin} needs. A family whose ports are named neither way states which instance \
         is which port as DATA rather than growing this list.",
        spellings[0], spellings[1]
    ))
}


/// Writes a generated Rust module's header as an INNER DOC comment (`//!`), not a plain `//`.
///
/// An inner doc comment binds the banner to the module: the DO-NOT-EDIT notice and the line saying
/// how to regenerate the file are the one thing a reader must see before touching it, and a marker
/// that can be separated from the constants is a warning that can go missing while they stay.
/// The memory-region facts a board emits, as one ordered row list the typed emitters share.
///
/// A board's `[memory]` record is emitted rather than merely validated against the linker
/// scripts. A region has more in it than a size, and every
/// part of it is something firmware would otherwise hand-type: where the region appears, how much
/// of it is reachable, and which controller must be running first.
///
/// An absent `_BASE` means the chip's own fixed window, which is chip truth and not the board's to
/// repeat. An absent `_CONTROLLER` means the chip maps the region with no help.
fn board_fact_rows(set: &FamilySet, resolved: &ResolvedBoard) -> Result<Vec<Row>, String> {
    let mut rows = memory_rows(set, resolved)?;
    rows.extend(discriminator_rows(&resolved.board));
    rows.extend(connector_rows(set, resolved)?);
    Ok(rows)
}

/// The sockets a removable module plugs into: which standard each follows, which buses it brings
/// out whole, and which single lines it brings out by name.
///
/// A BUS IS EMITTED AS ITS ROLE AND A LINE AS ITS PORT WIRING, because those are the two different
/// things a program needs. Reaching a module across a bus means opening the role the socket names;
/// driving a socket's chip select or reading its interrupt means writing one bit of one port, and
/// the base, index and mask for that are derived here rather than hand-composed by a driver.
///
/// A pin brought out here is EXPOSED, not owned. The same wire may carry a binding and reach a
/// socket, which is the ordinary case rather than a conflict.
fn connector_rows(set: &FamilySet, resolved: &ResolvedBoard) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    if resolved.board.connectors.is_empty() {
        return Ok(rows);
    }
    section(
        &mut rows,
        "-- connectors: the sockets a removable module plugs into. The socket is board truth --\n\
         it is on the schematic and identical on every unit -- and what is plugged into it is not,\n\
         so no row here names a module. A socket brings out whole BUSES, each named by the binding\n\
         role that serves it, and single LINES, each named by the standard's own name for that\n\
         position and carrying the port wiring a driver needs to drive it. Which of a socket's\n\
         protocols an attached module speaks is a property of the module, so a board that offers\n\
         several states all of them and chooses none --"
            .to_string(),
    );
    uint(&mut rows, "CONNECTOR_COUNT".to_string(), resolved.board.connectors.len().to_string());
    for connector in &resolved.board.connectors {
        let prefix = format!("CONNECTOR_{}", upper_snake(&connector.name));
        text(&mut rows, format!("{prefix}_STANDARD"), &connector.standard);
        for bus in &connector.buses {
            text(&mut rows, format!("{prefix}_{}_ROLE", upper_snake(&bus.signal)), &bus.role);
        }
        for line in &connector.pins {
            let (group_base, index) =
                control_pin_group_base(set, &resolved.board.board, &line.pin)?;
            let at = format!("{prefix}_{}", upper_snake(&line.signal));
            uint(&mut rows, format!("{at}_PORT_BASE"), format!("0x{group_base:X}"));
            uint(&mut rows, format!("{at}_PIN"), index.to_string());
            uint(&mut rows, format!("{at}_MASK"), format!("0x{:X}", 1u64 << index));
        }
    }
    Ok(rows)
}

/// What can be read from an attached board to confirm it is the one an image assumed.
///
/// Emitted as VALUES rather than left in the file, because the program that takes the readings is
/// not the program that holds this table: a discriminator names the claim it reaches, the rung a
/// successful read of its kind can establish, and the answer that confirms.
fn discriminator_rows(board: &BoardTable) -> Vec<Row> {
    let mut rows = Vec::new();
    if board.discriminators.is_empty() {
        return rows;
    }
    section(
        &mut rows,
        "-- discriminators: what an attached board can be asked to confirm it is the board an\n\
         image was built for. A chip identity register cannot answer this on its own, because the\n\
         parts that separate one board from its sibling are soldered outside the die and a bare\n\
         board answers the same identity as a populated one. So each row names the CLAIM it\n\
         reaches -- `part`, or `memory:<region>` -- alongside the rung a successful read of its\n\
         kind establishes: `identified` (it answered its identity register) or `exercised` (it\n\
         produced measurements a driver decoded). A region's ACCESSIBLE size is reachable only at\n\
         `exercised`: an identity read reports the fitted device, and a board may wire less of a\n\
         device than it holds --"
            .to_string(),
    );
    uint(&mut rows, "DISCRIMINATOR_COUNT".to_string(), board.discriminators.len().to_string());
    for row in &board.discriminators {
        let prefix = format!("DISCRIMINATOR_{}", upper_snake(&row.name));
        text(&mut rows, format!("{prefix}_CONFIRMS"), &row.confirms);
        text(&mut rows, format!("{prefix}_VALIDATION"), &row.validation);
        uint(&mut rows, format!("{prefix}_EXPECT"), format!("0x{:X}", row.expect));
        text(&mut rows, format!("{prefix}_READS"), &row.reads);
    }
    rows
}

fn memory_rows(set: &FamilySet, resolved: &ResolvedBoard) -> Result<Vec<Row>, String> {
    let board = &resolved.board;
    let mut rows = Vec::new();
    if board.memory.is_empty() {
        return Ok(rows);
    }
    section(
        &mut rows,
        "-- memory regions the board fits: SIZE is what a program may reach, which a device's own\nsize may exceed; a region with a CONTROLLER does not exist until that instance is brought\nup, and touching it first is a bus fault rather than a wrong value --".to_string(),
    );
    uint(&mut rows, "MEMORY_REGION_COUNT".to_string(), board.memory.len().to_string());
    for region in &board.memory {
        let prefix = format!("MEMORY_{}", upper_snake(&region.name));
        text(&mut rows, format!("{prefix}_KIND"), &region.kind);
        if region.base >= 0 {
            uint(&mut rows, format!("{prefix}_BASE"), format!("0x{:X}", region.base));
        }
        uint(&mut rows, format!("{prefix}_SIZE"), format!("0x{:X}", region.size));
        if region.device_size >= 0 && region.device_size != region.size {
            uint(&mut rows, format!("{prefix}_DEVICE_SIZE"), format!("0x{:X}", region.device_size));
        }
        if !region.controller.is_empty() {
            text(&mut rows, format!("{prefix}_CONTROLLER"), &region.controller);
        }
        uint(&mut rows, format!("{prefix}_OPTIONAL"), u32::from(region.optional).to_string());
    }
    for region in &board.memory {
        rows.extend(memory_device_rows(set, resolved, region)?);
    }
    Ok(rows)
}

/// The block a region's controller instance places, when the region names one AND the family has
/// a table for it. A region may legally name a controller the family places without a block table
/// -- that is the state this family's two memory controllers were in until a driver needed them
/// -- so an absent table is "nothing to derive", never an error.
fn region_block<'a>(set: &'a FamilySet, region: &MemoryRegion) -> Option<&'a BlockTable> {
    let row = set.instances.row(&region.controller)?;
    set.block(&row.block, "")
}

/// One region's FITTED-DEVICE emission: the configuration words its controller has to be told,
/// derived here rather than written down.
///
/// THIS IS WHERE THE THIRD STRATUM SHOWS ITSELF. A bring-up has three kinds of fact in it and
/// only two of them have a home in the file that states them:
///
/// * the ORDER is chip truth and rides the controller's block table;
/// * the DEVICE SHAPE is board truth and rides the region below;
/// * and SOME OF THE NUMBERS ARE NEITHER -- they are properties of a device AT A CLOCK, wrong at
///   every other operating point, so neither may be a bare constant. A baud
///   divisor is derived from a (carrier, plan) pair and never stated; a refresh count is derived
///   from a (device, plan) pair for exactly the same reason.
///
/// One of them cannot even be derived. A quad-SPI read's dummy-cycle count covers the device's
/// internal latency in TIME, so no formula over the chip's facts and the board's facts produces
/// it -- and a wrong count does not fail, it shifts every byte and returns plausible garbage. So
/// the emission carries the range a driver walks and the count that was MEASURED, named for the
/// plan it was measured under, and the driver asks the part.
fn memory_device_rows(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    region: &MemoryRegion,
) -> Result<Vec<Row>, String> {
    let board = &resolved.board.board;
    let mut rows = Vec::new();
    if region.device.is_empty() && region.reads.is_empty() && region.window.is_empty() {
        return Ok(rows);
    }
    let prefix = format!("MEMORY_{}", upper_snake(&region.name));
    let Some(block) = region_block(set, region) else {
        return Err(format!(
            "{board}: region '{}' states a fitted device, but its controller '{}' has no block table to configure -- the shape has nowhere to be written",
            region.name, region.controller
        ));
    };

    if !region.window.is_empty() {
        let Some(window) = block.constant(&region.window) else {
            return Err(format!(
                "{board}: region '{}' names window constant '{}', which block '{}' does not declare",
                region.name, region.window, block.block
            ));
        };
        if window != region.base {
            return Err(format!(
                "{board}: region '{}' states base 0x{:X} but block '{}' puts {} at 0x{:X} -- one number, two statements, and they disagree",
                region.name, region.base, block.block, region.window, window
            ));
        }
    }

    match block.block.as_str() {
        "quadspi" => quadspi_device_rows(resolved, region, block, &prefix, &mut rows)?,
        "fmc" => fmc_device_rows(resolved, region, block, &prefix, &mut rows)?,
        other => {
            return Err(format!(
                "{board}: region '{}' is brought up by a '{other}' controller, which has no device-shape derivation yet -- add its path before a board states one",
                region.name
            ));
        }
    }
    Ok(rows)
}

/// A region's required device facts, refused by NAME when one is missing. The set is per
/// controller because what an SDRAM is and what a NOR flash is are different questions, and
/// stating that in the reader would put the knowledge two files away from the arithmetic.
fn require_facts(
    board: &str,
    region: &MemoryRegion,
    required: &[&str],
) -> Result<(), String> {
    for key in required {
        if region.fact(key).is_none() {
            return Err(format!(
                "{board}: region '{}' states no '{key}' -- its controller cannot be configured without it",
                region.name
            ));
        }
    }
    for (key, _) in &region.device {
        if !required.contains(&key.as_str()) {
            return Err(format!(
                "{board}: region '{}' states device fact '{key}', which its controller does not take",
                region.name
            ));
        }
    }
    Ok(())
}

/// The AHB rate the default plan runs at -- what both memory controllers divide down from.
fn hclk_hz(resolved: &ResolvedBoard, plan: &Plan) -> Result<i64, String> {
    plan.rate("hclk_hz").ok_or_else(|| {
        format!(
            "{}: plan '{}' states no hclk_hz -- a memory controller's own clock is derived from the bus clock, so the point has to state it",
            resolved.board.board, plan.name
        )
    })
}

fn quadspi_device_rows(
    resolved: &ResolvedBoard,
    region: &MemoryRegion,
    block: &BlockTable,
    prefix: &str,
    rows: &mut Vec<Row>,
) -> Result<(), String> {
    let board = &resolved.board.board;
    require_facts(board, region, &["identity", "address_bits", "chip_select_high_cycles", "clock_idle"])?;
    let fact = |key: &str| region.fact(key).expect("required above");

    let fitted = region.fitted_size();
    if fitted <= 0 || fitted & (fitted - 1) != 0 {
        return Err(format!(
            "{board}: region '{}' is 0x{fitted:X} bytes, which is not a power of two -- the controller states a device's size as an exponent and cannot express this",
            region.name
        ));
    }
    let fsize = i64::from(fitted.trailing_zeros()) - 1;
    let dcr = block.place("DCR", "FSIZE", fsize)?
        | block.place("DCR", "CSHT", fact("chip_select_high_cycles") - 1)?
        | block.place("DCR", "CKMODE", fact("clock_idle"))?;

    section(rows, format!(
        "-- the flash fitted to '{}': what the controller must be told about the DEVICE, as\nopposed to how the controller itself is brought up (which is chip truth and lives in the\nblock layout). FSIZE is derived from the size above rather than stated beside it --\n2^(FSIZE+1) bytes -- so a size and an exponent cannot drift apart --",
        region.name
    ));
    uint(rows, format!("{prefix}_DEVICE_ID"), format!("0x{:X}", fact("identity")));
    uint(rows, format!("{prefix}_ADDRESS_BITS"), fact("address_bits").to_string());
    uint(rows, format!("{prefix}_FSIZE"), fsize.to_string());
    uint(rows, format!("{prefix}_DCR"), format!("0x{dcr:X}"));

    if region.reads.is_empty() {
        return Ok(());
    }
    section(rows, format!(
        "-- read configurations. Each word below is a COMPLETE command EXCEPT two fields, and\nwhich two is the point: the functional mode is the driver's choice (indirect or\nmemory-mapped, both in the block layout), and THE DUMMY COUNT IS THE PART'S ANSWER.\nA dummy phase covers the device's internal latency in TIME, so the cycles that cover it\nfall with the interface clock -- and a wrong count does not fail, it shifts every byte\nand returns plausible garbage. Walk the range and keep the first count that reads back a\npayload you already know; the anchors below say what this board answered, and at what --"
    ));
    uint(rows, format!("{prefix}_READ_CONFIGURATIONS"), region.reads.len().to_string());
    uint(rows, format!("{prefix}_DUMMY_MIN"), block.constant("CCR_DCYC_MIN").unwrap_or(0).to_string());
    uint(rows, format!("{prefix}_DUMMY_MAX"), block.constant("CCR_DCYC_MAX").unwrap_or(0).to_string());

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let hclk = hclk_hz(resolved, plan)?;
    let lines = |what: &str, count: i64| -> Result<i64, String> {
        match count {
            1 => block.constant("CCR_MODE_SINGLE"),
            2 => block.constant("CCR_MODE_DUAL"),
            4 => block.constant("CCR_MODE_QUAD"),
            _ => None,
        }
        .ok_or_else(|| format!("{board}: region '{}' states {what} on {count} lines, which the controller has no encoding for", region.name))
    };
    for read in &region.reads {
        if read.name.is_empty() {
            return Err(format!("{board}: region '{}' has a read configuration with no name", region.name));
        }
        for (what, value) in [
            ("instruction", read.instruction),
            ("instruction_lines", read.instruction_lines),
            ("address_lines", read.address_lines),
            ("data_lines", read.data_lines),
            ("clock_hz", read.clock_hz),
            ("dummy", read.dummy),
        ] {
            if value < 0 {
                return Err(format!(
                    "{board}: region '{}' read '{}' states no {what}",
                    region.name, read.name
                ));
            }
        }
        let read_prefix = format!("{prefix}_{}", upper_snake(&read.name));
        let address_size = (read.address_lines > 0).then_some(()).and(match region.fact("address_bits") {
            Some(24) => block.constant("CCR_ADSIZE_24"),
            Some(32) => block.constant("CCR_ADSIZE_32"),
            Some(16) => block.constant("CCR_ADSIZE_16"),
            Some(8) => block.constant("CCR_ADSIZE_8"),
            _ => None,
        }).ok_or_else(|| format!(
            "{board}: region '{}' states {} address bits, which the controller has no size encoding for",
            region.name, region.fact("address_bits").unwrap_or(-1)
        ))?;
        let phases = block.place("CCR", "INSTRUCTION", read.instruction)?
            | block.place("CCR", "IMODE", lines("its instruction", read.instruction_lines)?)?
            | block.place("CCR", "ADMODE", lines("an address", read.address_lines)?)?
            | block.place("CCR", "ADSIZE", address_size)?
            | block.place("CCR", "DMODE", lines("its data", read.data_lines)?)?;
        if read.clock_hz <= 0 || hclk % read.clock_hz != 0 {
            return Err(format!(
                "{board}: region '{}' read '{}' is qualified at {} Hz, which {hclk} Hz does not divide exactly -- the controller can only halve, third, quarter (and so on) the bus clock",
                region.name, read.name, read.clock_hz
            ));
        }
        uint(rows, format!("{read_prefix}_INSTRUCTION"), format!("0x{:X}", read.instruction));
        uint(rows, format!("{read_prefix}_CCR_PHASES"), format!("0x{phases:X}"));
        uint(rows, format!("{read_prefix}_PRESCALER_{}", upper_snake(&plan.name)), (hclk / read.clock_hz - 1).to_string());
        uint(rows, format!("{read_prefix}_CLOCK_HZ_{}", upper_snake(&plan.name)), read.clock_hz.to_string());
        uint(rows, format!("{read_prefix}_DUMMY_{}", upper_snake(&plan.name)), read.dummy.to_string());
        if read.dummy_datasheet >= 0 {
            uint(rows, format!("{read_prefix}_DUMMY_DATASHEET"), read.dummy_datasheet.to_string());
        }
    }
    Ok(())
}

fn fmc_device_rows(
    resolved: &ResolvedBoard,
    region: &MemoryRegion,
    block: &BlockTable,
    prefix: &str,
    rows: &mut Vec<Row>,
) -> Result<(), String> {
    let board = &resolved.board.board;
    require_facts(board, region, &[
        "bank", "column_bits", "row_bits", "banks", "data_bits", "device_data_bits", "cas_latency",
        "sdclk_hclk_periods", "read_burst", "mode_register", "refresh_period_ns", "refresh_rows",
        "refresh_burst", "settle_us",
        "tmrd_ns", "txsr_ns", "tras_ns", "trc_ns", "twr_ns", "trp_ns", "trcd_ns",
    ])?;
    let fact = |key: &str| region.fact(key).expect("required above");

    let cells = 1i64 << (fact("row_bits") + fact("column_bits"));
    let accessible = cells * fact("banks") * fact("data_bits") / 8;
    if accessible != region.size {
        return Err(format!(
            "{board}: region '{}' is {} rows x {} columns x {} banks x {} bits = 0x{accessible:X} bytes, but states size 0x{:X}",
            region.name, 1i64 << fact("row_bits"), 1i64 << fact("column_bits"), fact("banks"), fact("data_bits"), region.size
        ));
    }
    let fitted = cells * fact("banks") * fact("device_data_bits") / 8;
    if fitted != region.fitted_size() {
        return Err(format!(
            "{board}: region '{}' fits a {} bit device = 0x{fitted:X} bytes, but states device_size 0x{:X}",
            region.name, fact("device_data_bits"), region.fitted_size()
        ));
    }

    let encoded = |what: &str, name: String| -> Result<i64, String> {
        block.constant(&name).ok_or_else(|| format!(
            "{board}: region '{}' states {what}, which block '{}' has no '{name}' encoding for",
            region.name, block.block
        ))
    };
    let geometry_register = if fact("bank") == 2 { "SDCR2" } else { "SDCR1" };
    let geometry = block.place(geometry_register, "NC", encoded("its column count", format!("SDCR_NC_{}_COLUMN_BITS", fact("column_bits")))?)?
        | block.place(geometry_register, "NR", encoded("its row count", format!("SDCR_NR_{}_ROW_BITS", fact("row_bits")))?)?
        | block.place(geometry_register, "MWID", encoded("its bus width", format!("SDCR_MWID_{}", fact("data_bits")))?)?
        | block.place(geometry_register, "NB", encoded("its internal bank count", match fact("banks") { 4 => "SDCR_NB_FOUR".to_string(), _ => "SDCR_NB_TWO".to_string() })?)?
        | block.place(geometry_register, "CAS", encoded("its CAS latency", format!("SDCR_CAS_{}", fact("cas_latency")))?)?;
    let controller = block.place("SDCR1", "SDCLK", encoded("its clock period", format!("SDCR_SDCLK_HCLK_PERIODS_{}", fact("sdclk_hclk_periods")))?)?
        | block.place("SDCR1", "RBURST", fact("read_burst"))?;
    let sdcr1 = if fact("bank") == 2 { controller } else { geometry | controller };

    let target = match fact("bank") {
        1 => block.place("SDCMR", "CTB1", 1)?,
        2 => block.place("SDCMR", "CTB2", 1)?,
        other => return Err(format!("{board}: region '{}' names SDRAM bank {other}; the controller has two", region.name)),
    };
    let command = |mode: &str, extra: i64| -> Result<i64, String> {
        let value = block.constant(mode).ok_or_else(|| format!("{board}: block '{}' has no '{mode}'", block.block))?;
        Ok(block.place("SDCMR", "MODE", value)? | target | extra)
    };

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let hclk = hclk_hz(resolved, plan)?;
    let sdclk = hclk / fact("sdclk_hclk_periods");
    let margin = block.constant("SDRTR_COUNT_MARGIN").unwrap_or(0);
    let floor = block.constant("SDRTR_COUNT_MIN").unwrap_or(0);
    let period_cycles = fact("refresh_period_ns") * sdclk / (fact("refresh_rows") * 1_000_000_000);
    let count = period_cycles - margin;
    if count < floor {
        return Err(format!(
            "{board}: region '{}' refreshes every {period_cycles} cycles at {sdclk} Hz, which leaves {count} after the controller's {margin}-cycle margin -- below the {floor} it requires. The clock is too slow for this device's retention time",
            region.name
        ));
    }
    let cycles = |key: &str| -> Result<i64, String> {
        let ns = fact(key);
        if ns <= 0 {
            return Err(format!("{board}: region '{}' states {key} = {ns}; a timing is a positive minimum", region.name));
        }
        Ok((ns * sdclk + 999_999_999) / 1_000_000_000)
    };
    let timing_register = if fact("bank") == 2 { "SDTR2" } else { "SDTR1" };
    let timing = |register: &str, field: &str, key: &str| -> Result<i64, String> {
        block.place(register, field, cycles(key)? - 1)
    };
    let bank_timings = timing(timing_register, "TMRD", "tmrd_ns")?
        | timing(timing_register, "TXSR", "txsr_ns")?
        | timing(timing_register, "TRAS", "tras_ns")?
        | timing(timing_register, "TWR", "twr_ns")?
        | timing(timing_register, "TRCD", "trcd_ns")?;
    let controller_timings = timing("SDTR1", "TRC", "trc_ns")? | timing("SDTR1", "TRP", "trp_ns")?;
    let sdtr1 = if fact("bank") == 2 { controller_timings } else { bank_timings | controller_timings };

    let (twr, tras, trc, trcd, trp) =
        (cycles("twr_ns")?, cycles("tras_ns")?, cycles("trc_ns")?, cycles("trcd_ns")?, cycles("trp_ns")?);
    for (floor, how) in [(tras - trcd, "TRAS - TRCD"), (trc - trcd - trp, "TRC - TRCD - TRP")] {
        if twr < floor {
            return Err(format!(
                "{board}: region '{}' derives TWR {twr} cycles, below the {floor} the manual requires ({how}) at this plan's {sdclk} Hz memory clock -- raise twr_ns to at least {} ns",
                region.name,
                (floor - 1) * 1_000_000_000 / sdclk + 1
            ));
        }
    }

    let forbidden = cycles("twr_ns")? + cycles("trp_ns")? + cycles("trc_ns")? + cycles("trcd_ns")? + 4;
    if count == forbidden {
        return Err(format!(
            "{board}: region '{}' derives refresh count {count}, which is exactly TWR + TRP + TRC + TRCD + 4 at this plan's {sdclk} Hz memory clock -- the one value the manual forbids",
            region.name
        ));
    }

    section(rows, format!(
        "-- the SDRAM fitted to '{}': the shape its controller has to be told. The geometry and\nthe two sizes above are one fact stated twice and are checked against each other --\nrows x columns x banks x width IS the size, and a column count off by one halves the\nmemory, aliases every address above the first row, and passes a write-then-read --",
        region.name
    ));
    uint(rows, format!("{prefix}_BANK"), fact("bank").to_string());
    uint(rows, format!("{prefix}_SDCR1"), format!("0x{sdcr1:X}"));
    if fact("bank") == 2 {
        uint(rows, format!("{prefix}_SDCR2"), format!("0x{geometry:X}"));
    }
    uint(rows, format!("{prefix}_SDCMR_CLOCK_ENABLE"), format!("0x{:X}", command("SDCMR_MODE_CLOCK_ENABLE", 0)?));
    uint(rows, format!("{prefix}_SDCMR_PRECHARGE_ALL"), format!("0x{:X}", command("SDCMR_MODE_PRECHARGE_ALL", 0)?));
    uint(rows, format!("{prefix}_SDCMR_AUTO_REFRESH"), format!("0x{:X}", command("SDCMR_MODE_AUTO_REFRESH", block.place("SDCMR", "NRFS", fact("refresh_burst") - 1)?)?));
    uint(rows, format!("{prefix}_SDCMR_LOAD_MODE"), format!("0x{:X}", command("SDCMR_MODE_LOAD_MODE", block.place("SDCMR", "MRD", fact("mode_register"))?)?));
    uint(rows, format!("{prefix}_SETTLE_US"), fact("settle_us").to_string());
    section(rows, format!(
        "-- and the numbers that are NOT constants of anything. Each is derived from a formula\nthat is the CHIP's, a specification that is the DEVICE's, and a clock that is this PLAN's\n-- write any one of the three down and it is wrong at the other two's next value. They\ncarry the plan's name for that reason. The timings round UP, because a timing is a floor:\na cycle short is a violation and a cycle long is only throughput --"
    ));
    uint(rows, format!("{prefix}_SDCLK_HZ_{}", upper_snake(&plan.name)), sdclk.to_string());
    uint(rows, format!("{prefix}_REFRESH_COUNT_{}", upper_snake(&plan.name)), count.to_string());
    uint(rows, format!("{prefix}_SDRTR_{}", upper_snake(&plan.name)), format!("0x{:X}", block.place("SDRTR", "COUNT", count)?));
    uint(rows, format!("{prefix}_SDTR1_{}", upper_snake(&plan.name)), format!("0x{sdtr1:X}"));
    if fact("bank") == 2 {
        uint(rows, format!("{prefix}_SDTR2_{}", upper_snake(&plan.name)), format!("0x{bank_timings:X}"));
    }
    Ok(())
}

fn emit_rust_header(out: &mut String, what: &str, sources: &[String], regen: &str) {
    out.push_str(&format!(
        "//! GENERATED by lamella-bsp-gen from {list} -- DO NOT EDIT.\n//! Regenerate: {regen}\n//!\n//! {what}\n",
        list = sources.join(" + "),
        what = what.replace("\n//", "\n//!"),
    ));
}

fn push_rust_const(out: &mut String, kind: &str, name: &str, value: &str) {
    out.push_str(&format!("pub const {name}: {kind} = {value};\n"));
}

fn finish_rust(out: &str) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("pub const ") {
            if let Some(name) = rest.split(':').next() {
                if !seen.insert(name.trim().to_string()) {
                    return Err(format!("duplicate emitted constant '{}'", name.trim()));
                }
            }
        }
    }
    Ok(())
}

/// Emits a block layout as a Rust `pub const` module: the SAME names and value spellings as
/// [`emit_layout_csharp`] (offsets, access widths, shifted field masks + shifts, block constants,
/// the channel map and the calibration records), so a Rust driver composes its words from exactly
/// the constants a C# driver does. C# `uint` and `int` map to `u32` and `i32`.
///
/// ONE FILE PER BLOCK, because in Rust the file IS the namespace. A family's blocks reuse register
/// names freely -- two of them having a `CR` is ordinary -- so one module per family would collide
/// on names that are only unique within a block.
///
/// A FLOAT FACT IS NOT EMITTED, and the header names the ones withheld. A facts table carries
/// integers and named dispatch; offering a `f64` to a tier with no floating point would be
/// carrying that debt one language further rather than reporting it.
pub fn emit_layout_rust(block: &BlockTable, source: &str, regen: &str) -> Result<String, String> {
    let mut out = String::new();
    let withheld: Vec<&str> = block
        .facts
        .iter()
        .filter(|(_, fact)| matches!(fact, Fact::Float(_)))
        .map(|(name, _)| name.as_str())
        .collect();
    let what = format!(
        "The {} {}{} BLOCK layout as Rust consts, name/value-identical to {}.g.cs:\n// offsets are instance-base-relative (`base + *_OFF`) and the instance bases live in\n// {}_instances.rs. Widths are access widths.",
        block.family,
        block.block,
        if block.mode.is_empty() { String::new() } else { format!(" ({} mode)", block.mode) },
        layout_class(block),
        snake(&block.family),
    );
    let withheld_note = if withheld.is_empty() {
        String::new()
    } else {
        format!(
            "\n/// WITHHELD from this language: the float fact(s) {}. A facts table carries integers\n/// and named dispatch, and this tier has no floating point.",
            withheld.join(", ")
        )
    };
    emit_rust_header(&mut out, &what, &[source.to_string()], regen);

    let header = std::mem::take(&mut out);

    let has_integer_facts = block.facts.iter().any(|(_, fact)| matches!(fact, Fact::Int(_)));
    if !block.registers.is_empty() {
        out.push_str("\n/// -- register offsets (block-relative) + access widths --");
        if !has_integer_facts {
            out.push_str(&withheld_note);
        }
        out.push('\n');
        for register in &block.registers {
            push_rust_const(&mut out, "u32", &format!("{}_OFF", register.name), &format_int(register.offset));
            push_rust_const(&mut out, "i32", &format!("{}_WIDTH", register.name), &register.width.to_string());
        }
    }

    if block.registers.iter().any(|register| !register.fields.is_empty()) {
        out.push_str("\n/// -- fields: <REG>_<FIELD> = the shifted mask; _LSB = the shift --\n");
        for register in &block.registers {
            for field in &register.fields {
                push_rust_const(&mut out, "u32", &format!("{}_{}", register.name, field.name), &format!("0x{:X}", field.mask()));
                push_rust_const(&mut out, "u32", &format!("{}_{}_LSB", register.name, field.name), &field.lsb.to_string());
            }
        }
    }

    if !block.constants.is_empty() {
        out.push_str("\n/// -- block constants --\n");
        for (name, value) in &block.constants {
            let kind = if value.value < 0 { "i32" } else { "u32" };
            push_rust_const(&mut out, kind, name, &format_int(*value));
        }
    }

    let integers: Vec<(&String, &Int)> = block
        .facts
        .iter()
        .filter_map(|(name, fact)| match fact {
            Fact::Int(value) => Some((name, value)),
            Fact::Float(_) => None,
        })
        .collect();
    if !integers.is_empty() {
        out.push_str("\n/// -- facts as data (chip/electrical facts conversions read) --");
        out.push_str(&withheld_note);
        out.push('\n');
        for (name, value) in integers {
            let kind = if value.value < 0 { "i32" } else { "u32" };
            push_rust_const(&mut out, kind, &pascal(name), &format_int(*value));
        }
    }

    if !block.channels.is_empty() {
        out.push_str("\n/// -- channel map: Channel_<source> = the mux/AINSEL index; Channel<i>_Pin = the\n/// GPIO index a pin-fed channel taps; ChannelCount = the package's mux width --\n");
        for channel in &block.channels {
            push_rust_const(&mut out, "i32", &format!("Channel_{}", pascal(&channel.source)), &channel.index.to_string());
        }
        for channel in &block.channels {
            if let Some(('g', pin_index)) = split_pin(&channel.source) {
                push_rust_const(&mut out, "i32", &format!("Channel{}_Pin", channel.index), &pin_index.to_string());
            }
        }
        push_rust_const(&mut out, "i32", "ChannelCount", &block.channels.len().to_string());
    }

    for record in &block.calibrations {
        out.push_str(&format!(
            "\n/// -- calibration '{}' (form: {}); integer coefficients, no hardcoding downstream --\n",
            record.name, record.form
        ));
        for (coefficient, value) in &record.coefficients {
            let kind = if value.value < 0 { "i32" } else { "u32" };
            push_rust_const(&mut out, kind, &format!("{}_{}", pascal(&record.name), pascal(coefficient)), &format_int(*value));
        }
    }

    let mixed_case = out.lines().any(|line| {
        line.strip_prefix("pub const ")
            .and_then(|rest| rest.split(':').next())
            .is_some_and(|name| name.chars().any(|c| c.is_ascii_lowercase()))
    });
    let mut file = header;
    if mixed_case {
        file.push_str("\n#![allow(non_upper_case_globals)]\n");
    }
    file.push_str(&out);
    finish_rust(&file)?;
    Ok(file)
}

/// The generated Rust layout module's file stem for a block: `stm32f7_fmc_layout`.
#[must_use]
pub fn layout_module(block: &BlockTable) -> String {
    if block.mode.is_empty() {
        format!("{}_{}_layout", snake(&block.family), snake(&block.block))
    } else {
        format!("{}_{}_{}_layout", snake(&block.family), snake(&block.block), snake(&block.mode))
    }
}

/// Emits a family's instances as a Rust `pub const` module: the SAME names and value
/// spellings as [`emit_instances_csharp`] (every value a single resolved literal), so a Rust
/// firmware and a C# driver can never disagree on a placed-instance fact.
pub fn emit_instances_rust(
    instances: &InstancesTable,
    source: &str,
    regen: &str,
) -> Result<String, String> {
    let mut out = String::new();
    let what = format!(
        "The {} INSTANCE map as Rust consts, name/value-identical to {}.g.cs.\n// WHERE each block copy sits; WHAT is inside one is the per-block layout module\n// beside this file. A driver includes both and composes from neither's literals.",
        instances.family,
        instances_class(&instances.family),
    );
    emit_rust_header(&mut out, &what, &[source.to_string()], regen);
    out.push('\n');
    for row in &instances.rows {
        let prefix = upper_snake(&row.name);
        for (field, value) in instances.record.iter().zip(&row.values) {
            if *value == -1 {
                continue;
            }
            let hex = field == "base";
            let spelled = if hex { format!("0x{value:X}") } else { value.to_string() };
            push_rust_const(&mut out, "u32", &format!("{prefix}_{}", upper_snake(field)), &spelled);
            if let Some(stem) = field.strip_suffix("_bit") {
                push_rust_const(
                    &mut out,
                    "u32",
                    &format!("{prefix}_{}_MASK", upper_snake(stem)),
                    &format!("0x{:X}", 1i64 << value),
                );
            }
        }
    }
    finish_rust(&out)?;
    Ok(out)
}

/// Emits a board's bindings as a Rust `pub const` module: the SAME names and value spellings
/// as [`emit_board_csharp`] (BOARD_MODEL and the carrier USB identity ride the wire/USB-natural
/// `u16`; every fact const is `u32`). A firmware's per-board `board` module #[path]-includes
/// this file, which is what makes "board truth lives in one place" true for the firmware tier.
pub fn emit_board_rust(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    sources: &[String],
    regen: &str,
) -> Result<String, String> {
    let BoardEmissions {
        skipped,
        driver_families,
        sercom_uarts: uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        st_i2cs,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
        st_spis,
        nrf_twis,
        dw_i2cs,
        rp_adcs,
        rp_clocks,
    } = resolve_board_emissions(set, resolved)?;

    let mut out = String::new();
    let mut what = format!(
        "The {} board BINDINGS as Rust consts, name/value-identical to {}.g.cs:\n// every value below is a generation-time literal derived from the strata.\n// Board truth lives in board.toml, never here.",
        resolved.board.board,
        bindings_class(&resolved.board.board),
    );
    if !skipped.is_empty() {
        what.push_str(&format!(
            "\n// NOT YET EMITTED (no emitter for these binding kinds): {}.",
            skipped.join(", ")
        ));
    }
    emit_rust_header(&mut out, &what, sources, regen);

    out.push_str("\n// -- identity --\n");
    push_rust_const(&mut out, "u16", "BOARD_MODEL", &resolved.board.board_model.to_string());
    push_rust_const(&mut out, "&str", "BOARD_VENDOR", &format!("\"{}\"", vendor_segment(&resolved.board.vendor)));
    if resolved.board.carrier.usb_vid > 0 {
        push_rust_const(&mut out, "u16", "CARRIER_USB_VID", &format!("0x{:04X}", resolved.board.carrier.usb_vid));
    }
    if resolved.board.carrier.usb_pid > 0 {
        push_rust_const(&mut out, "u16", "CARRIER_USB_PID", &format!("0x{:04X}", resolved.board.carrier.usb_pid));
    }

    if !driver_families.is_empty() {
        out.push_str(&driver_family_note(""));
        for (role, family) in &driver_families {
            push_rust_const(
                &mut out,
                "&str",
                &format!("{}_DRIVER_FAMILY", upper_snake(role)),
                &format!("\"{family}\""),
            );
        }
    }

    for uart in &uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n// -- {p}: a sercom-usart binding descriptor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", uart.sercom_base));
        if uart.irq >= 0 {
            push_rust_const(&mut out, "u32", &format!("{p}_IRQ"), &uart.irq.to_string());
        }
        push_rust_const(&mut out, "u32", &format!("{p}_GCLK_CLKCTRL_VALUE"), &format!("0x{:X}", uart.gclk_clkctrl_value));
        push_rust_const(&mut out, "u32", &format!("{p}_APBC_MASK"), &format!("0x{:X}", uart.apbc_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_PMUX_REG"), &format!("0x{:X}", uart.pmux_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PMUX_PAIR"), &format!("0x{:X}", uart.pmux_pair));
        push_rust_const(&mut out, "u32", &format!("{p}_PINCFG_TX_REG"), &format!("0x{:X}", uart.pincfg_tx_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PINCFG_RX_REG"), &format!("0x{:X}", uart.pincfg_rx_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_TXPO"), &uart.txpo.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_RXPO"), &uart.rxpo.to_string());
        for (suffix, divisor) in &uart.bauds {
            push_rust_const(&mut out, "u32", &format!("{p}_{suffix}"), &format!("0x{divisor:X}"));
        }
    }

    for uart in &rp_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n// -- {p}: a pl011 uart binding descriptor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", uart.reset_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_IO_TX_CTRL"), &format!("0x{:X}", uart.io_tx_ctrl));
        push_rust_const(&mut out, "u32", &format!("{p}_IO_RX_CTRL"), &format!("0x{:X}", uart.io_rx_ctrl));
        if let Some((pads_tx, pads_rx)) = &uart.pads {
            push_rust_const(&mut out, "u32", &format!("{p}_PADS_TX"), &format!("0x{pads_tx:X}"));
            push_rust_const(&mut out, "u32", &format!("{p}_PADS_RX"), &format!("0x{pads_rx:X}"));
        }
        push_rust_const(&mut out, "u32", &format!("{p}_FUNCSEL"), &uart.funcsel.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_CLK_PERI_HZ"), &uart.clk_peri_hz.to_string());
        for (suffix, ibrd, fbrd) in &uart.bauds {
            push_rust_const(&mut out, "u32", &format!("{p}_IBRD_{suffix}"), &ibrd.to_string());
            push_rust_const(&mut out, "u32", &format!("{p}_FBRD_{suffix}"), &fbrd.to_string());
        }
    }

    for uart in &esp_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n// -- {p}: an esp32c6 hp-uart binding descriptor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_rust_const(&mut out, "u32", &format!("{p}_PCR_CONF"), &format!("0x{:X}", uart.pcr_conf));
        push_rust_const(&mut out, "u32", &format!("{p}_PCR_SCLK_CONF"), &format!("0x{:X}", uart.pcr_sclk_conf));
        push_rust_const(&mut out, "u32", &format!("{p}_IO_MUX_TX"), &format!("0x{:X}", uart.io_mux_tx));
        push_rust_const(&mut out, "u32", &format!("{p}_IO_MUX_RX"), &format!("0x{:X}", uart.io_mux_rx));
        push_rust_const(&mut out, "u32", &format!("{p}_MCU_SEL"), &uart.mcu_sel.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_SCLK_HZ"), &uart.sclk_hz.to_string());
    }

    for uart in &sam3x_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n// -- {p}: a sam3x uart binding descriptor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_rust_const(&mut out, "u32", &format!("{p}_PID"), &uart.pid.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_PMC_PCER_REG"), &format!("0x{:X}", uart.pmc_pcer_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PMC_PCER_MASK"), &format!("0x{:X}", uart.pmc_pcer_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_PIO_PDR_REG"), &format!("0x{:X}", uart.pio_pdr_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PIO_ABSR_REG"), &format!("0x{:X}", uart.pio_absr_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PIO_MASK"), &format!("0x{:X}", uart.pio_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_PIO_FUNC"), &uart.pio_func.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_MCK_HZ"), &uart.mck_hz.to_string());
        for (suffix, cd) in &uart.bauds {
            push_rust_const(&mut out, "u32", &format!("{p}_BRGR_CD_{suffix}"), &cd.to_string());
        }
    }

    for uart in &st_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n// -- {p}: an st-usart binding descriptor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", uart.rcc_en_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", uart.rcc_en_mask));
        for (side, pin) in [("TX", &uart.tx), ("RX", &uart.rx)] {
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        push_rust_const(&mut out, "u32", &format!("{p}_PCLK_HZ"), &uart.pclk_hz.to_string());
        for (suffix, divisor) in &uart.bauds {
            push_rust_const(&mut out, "u32", &format!("{p}_{suffix}"), &format!("0x{divisor:X}"));
        }
    }

    for spi in &sercom_spis {
        let p = &spi.prefix;
        out.push_str(&format!("\n// -- {p}: a sercom-spi binding descriptor (core-clock id UNSHIFTED: the\n// consumer composes ID | GEN | CLKEN per its runtime-selected plan) --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", spi.sercom_base));
        if spi.irq >= 0 {
            push_rust_const(&mut out, "u32", &format!("{p}_IRQ"), &spi.irq.to_string());
        }
        push_rust_const(&mut out, "u32", &format!("{p}_APBC_MASK"), &format!("0x{:X}", spi.apbc_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_GCLK_CORE_ID"), &spi.gclk_core_id.to_string());
        for (signal, pmux_reg, pmux_shift, pincfg_reg) in &spi.signals {
            let s = upper_snake(signal);
            push_rust_const(&mut out, "u32", &format!("{p}_PMUX_{s}_REG"), &format!("0x{pmux_reg:X}"));
            push_rust_const(&mut out, "u32", &format!("{p}_PMUX_{s}_SHIFT"), &pmux_shift.to_string());
            push_rust_const(&mut out, "u32", &format!("{p}_PINCFG_{s}_REG"), &format!("0x{pincfg_reg:X}"));
        }
        push_rust_const(&mut out, "u32", &format!("{p}_PMUX_FUNC"), &spi.pmux_func.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_DOPO"), &spi.dopo.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_DIPO"), &spi.dipo.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_CS_PORT_BASE"), &format!("0x{:X}", spi.cs_port_base));
        push_rust_const(&mut out, "u32", &format!("{p}_CS_PIN"), &spi.cs_pin.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_CS_MASK"), &format!("0x{:X}", spi.cs_mask));
    }
    for i2c in &sercom_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("
//  -- {p}: a sercom-i2c binding descriptor (the CORE-CLOCK RATE, not a
//  divisor: an I2C bus speed is a runtime Configure choice) --
"));
        push_rust_const(&mut out, "u32", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", i2c.sercom_base));
        if i2c.irq >= 0 {
            push_rust_const(&mut out, "u32", &format!("{p}_IRQ"), &i2c.irq.to_string());
        }
        push_rust_const(&mut out, "u32", &format!("{p}_GCLK_CLKCTRL_VALUE"), &format!("0x{:X}", i2c.gclk_clkctrl_value));
        push_rust_const(&mut out, "u32", &format!("{p}_APBC_MASK"), &format!("0x{:X}", i2c.apbc_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_PMUX_REG"), &format!("0x{:X}", i2c.pmux_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PMUX_PAIR"), &format!("0x{:X}", i2c.pmux_pair));
        push_rust_const(&mut out, "u32", &format!("{p}_PINCFG_SDA_REG"), &format!("0x{:X}", i2c.pincfg_sda_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PINCFG_SCL_REG"), &format!("0x{:X}", i2c.pincfg_scl_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_CORE_CLOCK_HZ"), &i2c.core_clock_hz.to_string());
    }

    for spi in &pl022_spis {
        let p = &spi.prefix;
        out.push_str(&format!("\n// -- {p}: a pl022 spi binding descriptor (rate = a runtime Configure choice\n// from SSPCLK, like the samd21 spi BAUD -- no divisor emits) --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", spi.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", spi.reset_mask));
        for (signal, io_ctrl, pads) in &spi.signals {
            let s = upper_snake(signal);
            push_rust_const(&mut out, "u32", &format!("{p}_IO_{s}_CTRL"), &format!("0x{io_ctrl:X}"));
            push_rust_const(&mut out, "u32", &format!("{p}_PADS_{s}"), &format!("0x{pads:X}"));
        }
        push_rust_const(&mut out, "u32", &format!("{p}_FUNCSEL"), &spi.funcsel.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_SSPCLK_HZ"), &spi.sspclk_hz.to_string());
    }


    for spi in &st_spis {
        let p = &spi.prefix;
        out.push_str(&format!("
// -- {p}: an st-spi binding descriptor. NO baud emits: an SPI master's rate is a property of the ATTACHED DEVICE, and a binding names a bus. The APB rate is here and the block table states the eight prescaler codes; the driver picks --
"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", spi.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", spi.rcc_en_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", spi.rcc_en_mask));
        for (side, pin) in [("SCK", &spi.sck), ("MISO", &spi.miso), ("MOSI", &spi.mosi)] {
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        if let Some(cs) = &spi.cs {
            push_rust_const(&mut out, "u32", &format!("{p}_CS_PORT_RCC_EN_REG"), &format!("0x{:X}", cs.port_rcc_en_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_PORT_RCC_EN_MASK"), &format!("0x{:X}", cs.port_rcc_en_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_MODER_REG"), &format!("0x{:X}", cs.moder_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_MODER_MASK"), &format!("0x{:X}", cs.moder_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_MODER_VALUE"), &format!("0x{:X}", cs.moder_value));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_BSRR_REG"), &format!("0x{:X}", cs.bsrr_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_BSRR_SET"), &format!("0x{:X}", cs.bsrr_set));
            push_rust_const(&mut out, "u32", &format!("{p}_CS_BSRR_CLEAR"), &format!("0x{:X}", cs.bsrr_clear));
        }
        push_rust_const(&mut out, "u32", &format!("{p}_PCLK_HZ"), &spi.pclk_hz.to_string());
    }

    for i2c in &st_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("\n// -- {p}: an st-i2c binding descriptor. BOTH PINS ARE OPEN DRAIN --\n// a push-pull output cannot be pulled low by the device at the other end, so an\n// acknowledge is fought instead of seen and nothing ever answers, with the mux\n// perfectly correct. The timing words are the manual's own compliant points at\n// this plan's kernel rate, composed here: five counts, not a divisor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", i2c.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", i2c.rcc_en_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", i2c.rcc_en_mask));
        for (side, pin) in [("SCL", &i2c.scl), ("SDA", &i2c.sda)] {
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_rust_const(&mut out, "u32", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        push_rust_const(&mut out, "u32", &format!("{p}_OTYPER_REG"), &format!("0x{:X}", i2c.otyper.0));
        push_rust_const(&mut out, "u32", &format!("{p}_OTYPER_SCL_MASK"), &format!("0x{:X}", i2c.otyper.1));
        push_rust_const(&mut out, "u32", &format!("{p}_OTYPER_SDA_MASK"), &format!("0x{:X}", i2c.otyper.2));
        push_rust_const(&mut out, "u32", &format!("{p}_KERNEL_HZ"), &i2c.kernel_hz.to_string());
        for (suffix, word) in &i2c.timings {
            push_rust_const(&mut out, "u32", &format!("{p}_{suffix}"), &format!("0x{word:X}"));
        }
    }
    for twi in &nrf_twis {
        let p = &twi.prefix;
        out.push_str(&format!("\n// -- {p}: an nrf-twi binding descriptor --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_TWI_BASE"), &format!("0x{:X}", twi.twi_base));
        push_rust_const(&mut out, "u32", &format!("{p}_PSEL_SCL"), &format!("0x{:X}", twi.psel_scl));
        push_rust_const(&mut out, "u32", &format!("{p}_PSEL_SDA"), &format!("0x{:X}", twi.psel_sda));
        push_rust_const(&mut out, "u32", &format!("{p}_PIN_CNF_SCL_REG"), &format!("0x{:X}", twi.pin_cnf_scl_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PIN_CNF_SDA_REG"), &format!("0x{:X}", twi.pin_cnf_sda_reg));
    }

    for i2c in &dw_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("\n// -- {p}: a dw-i2c binding descriptor (ic_clk = clk_sys on this chip; the\n// SCL count formulas stay driver math per the official pico-sdk driver) --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", i2c.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", i2c.reset_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_IO_SDA_CTRL"), &format!("0x{:X}", i2c.io_sda_ctrl));
        push_rust_const(&mut out, "u32", &format!("{p}_IO_SCL_CTRL"), &format!("0x{:X}", i2c.io_scl_ctrl));
        push_rust_const(&mut out, "u32", &format!("{p}_PADS_SDA"), &format!("0x{:X}", i2c.pads_sda));
        push_rust_const(&mut out, "u32", &format!("{p}_PADS_SCL"), &format!("0x{:X}", i2c.pads_scl));
        push_rust_const(&mut out, "u32", &format!("{p}_FUNCSEL"), &i2c.funcsel.to_string());
        push_rust_const(&mut out, "u32", &format!("{p}_IC_CLK_HZ"), &i2c.ic_clk_hz.to_string());
    }

    for adc in &rp_adcs {
        let p = &adc.prefix;
        out.push_str(&format!("\n// -- {p}: an rp-adc binding descriptor (the reference rail is BOARD truth;\n// the channel map + calibration records are chip truth in the adc layout) --\n"));
        push_rust_const(&mut out, "u32", &format!("{p}_BASE"), &format!("0x{:X}", adc.base));
        push_rust_const(&mut out, "u32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", adc.reset_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_REFERENCE_UV"), &adc.reference_uv.to_string());
    }

    for clock in &rp_clocks {
        let s = &clock.plan;
        out.push_str("\n// -- the clock plan (state-and-verify: the plan states the chosen PLL\n// values AND the resulting rates; generation verified hz == xosc * fbdiv /\n// (postdiv1 * postdiv2) and composed each PRIM word from the pll block fields).\n// Every const carries the plan name as a suffix, the default plan included, so a\n// name states which operating point it means -- a board may declare several --\n");
        push_rust_const(&mut out, "u32", &format!("XOSC_HZ_{s}"), &clock.xosc_hz.to_string());
        push_rust_const(&mut out, "u32", &format!("CLK_SYS_HZ_{s}"), &clock.clk_sys_hz.to_string());
        push_rust_const(&mut out, "u32", &format!("CLK_USB_HZ_{s}"), &clock.clk_usb_hz.to_string());
        if clock.clk_adc_hz >= 0 {
            push_rust_const(&mut out, "u32", &format!("CLK_ADC_HZ_{s}"), &clock.clk_adc_hz.to_string());
        }
        push_rust_const(&mut out, "u32", &format!("PLL_SYS_FBDIV_{s}"), &clock.pll_sys_fbdiv.to_string());
        push_rust_const(&mut out, "u32", &format!("PLL_SYS_PRIM_{s}"), &format!("0x{:X}", clock.pll_sys_prim));
        push_rust_const(&mut out, "u32", &format!("PLL_USB_FBDIV_{s}"), &clock.pll_usb_fbdiv.to_string());
        push_rust_const(&mut out, "u32", &format!("PLL_USB_PRIM_{s}"), &format!("0x{:X}", clock.pll_usb_prim));
    }

    let controls: Vec<(&str, &[ControlPin])> = vec![
        ("module control lines", &resolved.module_pins),
        ("on-board devices", &resolved.board.devices),
    ];
    for (label, pins) in controls {
        if pins.is_empty() {
            continue;
        }
        out.push_str(&format!("\n// -- {label}: PORT group base + pin index + mask --\n"));
        for control in pins {
            if control.pin.is_empty() {
                let p = upper_snake(&control.name);
                push_rust_const(&mut out, "u32", &format!("{p}_ADDRESS"), &format!("0x{:X}", control.address));
                continue;
            }
            let (group_base, index) = control_pin_group_base(set, &resolved.board.board, &control.pin)?;
            let p = upper_snake(&control.name);
            push_rust_const(&mut out, "u32", &format!("{p}_PORT_BASE"), &format!("0x{group_base:X}"));
            push_rust_const(&mut out, "u32", &format!("{p}_PIN"), &index.to_string());
            push_rust_const(&mut out, "u32", &format!("{p}_MASK"), &format!("0x{:X}", 1u64 << index));
            push_rust_const(
                &mut out,
                "u32",
                &format!("{p}_ACTIVE_LOW"),
                if control.active == "low" { "1" } else { "0" },
            );
        }
    }

    let rows_at = board_fact_rows(set, resolved)?;
    for (at, row) in rows_at.iter().enumerate() {
        let documents = documents_something(&rows_at[at + 1..]);
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "", if documents { "///" } else { "//" }, comment),
            Row::Uint(name, value) => push_rust_const(&mut out, "u32", name, value),
            Row::Int(name, value) => push_rust_const(&mut out, "i32", name, value),
            Row::Str(name, value) => push_rust_const(&mut out, "&str", name, &format!("\"{value}\"")),
        }
    }

    finish_rust(&out)?;
    Ok(out)
}


/// The families whose strata additionally emit the Swift projection. The emitters are
/// family-generic; each family joins this list deliberately as its Swift consumers arrive.
const SWIFT_FAMILIES: &[&str] =
    &["nrf51", "nrf52833", "rp2040", "rp2350", "samd21", "stm32f7", "stm32l476"];

fn emit_swift_header(out: &mut String, what: &str, sources: &[String], regen: &str) {
    out.push_str(&format!(
        "// GENERATED by lamella-bsp-gen from {list} -- DO NOT EDIT.\n// Regenerate: {regen}\n//\n// {what}\n",
        list = sources.join(" + "),
    ));
}

fn push_swift_const(out: &mut String, kind: &str, name: &str, value: &str) {
    out.push_str(&format!("    public static let {name}: {kind} = {value}\n"));
}

fn finish_swift(out: &str) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for line in out.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("public static let ") {
            if let Some(name) = rest.split(':').next() {
                if !seen.insert(name.trim().to_string()) {
                    return Err(format!("duplicate emitted constant '{}'", name.trim()));
                }
            }
        }
    }
    Ok(())
}

/// Emits a block layout as a Swift caseless-enum namespace: the SAME names and value
/// spellings as [`emit_layout_csharp`] (offsets, access widths, shifted field masks +
/// shifts, block constants, and the facts/channels/calibration records), so a Swift
/// layer-1 driver composes its words from exactly the constants the C# driver does.
/// C# `uint`/`int`/`double` map to Swift `UInt32`/`Int32`/`Double`.
pub fn emit_layout_swift(block: &BlockTable, source: &str, regen: &str) -> Result<String, String> {
    let mut out = String::new();
    let class = layout_class(block);
    let what = format!(
        "The {} {}{} BLOCK layout: offsets are instance-base-relative (`base + *_OFF`);\n// instance bases live in {}Instances. Widths are access widths.",
        block.family,
        block.block,
        if block.mode.is_empty() { String::new() } else { format!("-{}", block.mode) },
        pascal(&block.family),
    );
    emit_swift_header(&mut out, &what, &[source.to_string()], regen);
    out.push_str(&format!("\npublic enum {class} {{\n"));

    out.push_str("    // -- register offsets (block-relative) + access widths --\n");
    for register in &block.registers {
        push_swift_const(&mut out, "UInt32", &format!("{}_OFF", register.name), &format_int(register.offset));
        push_swift_const(&mut out, "Int32", &format!("{}_WIDTH", register.name), &register.width.to_string());
    }

    out.push_str("\n    // -- fields: <REG>_<FIELD> = the shifted mask; _LSB = the shift --\n");
    for register in &block.registers {
        for field in &register.fields {
            push_swift_const(&mut out, "UInt32", &format!("{}_{}", register.name, field.name), &format!("0x{:X}", field.mask()));
            push_swift_const(&mut out, "UInt32", &format!("{}_{}_LSB", register.name, field.name), &field.lsb.to_string());
        }
    }

    if !block.constants.is_empty() {
        out.push_str("\n    // -- block constants --\n");
        for (name, value) in &block.constants {
            let kind = if value.value < 0 { "Int32" } else { "UInt32" };
            push_swift_const(&mut out, kind, name, &format_int(*value));
        }
    }

    if !block.facts.is_empty() {
        out.push_str("\n    // -- facts as data (chip/electrical facts conversions read) --\n");
        for (name, fact) in &block.facts {
            match fact {
                Fact::Int(value) => {
                    let kind = if value.value < 0 { "Int32" } else { "UInt32" };
                    push_swift_const(&mut out, kind, &pascal(name), &format_int(*value));
                }
                Fact::Float(text) => {
                    push_swift_const(&mut out, "Double", &pascal(name), text);
                }
            }
        }
    }
    if !block.channels.is_empty() {
        out.push_str("\n    // -- channel map: Channel_<source> = the mux/AINSEL index; Channel<i>_Pin = the\n    // GPIO index a pin-fed channel taps; ChannelCount = the package's mux width --\n");
        for channel in &block.channels {
            push_swift_const(
                &mut out,
                "Int32",
                &format!("Channel_{}", pascal(&channel.source)),
                &channel.index.to_string(),
            );
        }
        for channel in &block.channels {
            if let Some(('g', pin_index)) = split_pin(&channel.source) {
                push_swift_const(
                    &mut out,
                    "Int32",
                    &format!("Channel{}_Pin", channel.index),
                    &pin_index.to_string(),
                );
            }
        }
        push_swift_const(&mut out, "Int32", "ChannelCount", &block.channels.len().to_string());
    }
    for record in &block.calibrations {
        out.push_str(&format!(
            "\n    // -- calibration '{}' (form: {}); integer coefficients, no hardcoding downstream --\n",
            record.name, record.form
        ));
        for (coefficient, value) in &record.coefficients {
            let kind = if value.value < 0 { "Int32" } else { "UInt32" };
            push_swift_const(
                &mut out,
                kind,
                &format!("{}_{}", pascal(&record.name), pascal(coefficient)),
                &format_int(*value),
            );
        }
    }

    out.push_str("}\n");
    finish_swift(&out)?;
    Ok(out)
}

/// Emits a family's instances as a Swift caseless-enum namespace of `static let` constants:
/// the SAME names and value spellings as [`emit_instances_csharp`], so Swift firmware and
/// every other language skin can never disagree on a placed-instance fact.
pub fn emit_instances_swift(
    instances: &InstancesTable,
    source: &str,
    regen: &str,
) -> Result<String, String> {
    let mut out = String::new();
    let class = instances_class(&instances.family);
    let what = format!(
        "The {} instance map as Swift constants, name/value-identical to {class}.g.cs:\n// where each block copy sits and its per-instance ids. Block-register offsets are not\n// emitted for Swift: a firmware project states its block constants and reads this file\n// for every placed-instance fact.",
        instances.family,
    );
    emit_swift_header(&mut out, &what, &[source.to_string()], regen);
    out.push_str(&format!("\npublic enum {class} {{\n"));
    for row in &instances.rows {
        let prefix = upper_snake(&row.name);
        for (field, value) in instances.record.iter().zip(&row.values) {
            if *value == -1 {
                continue;
            }
            let hex = field == "base";
            let spelled = if hex { format!("0x{value:X}") } else { value.to_string() };
            push_swift_const(&mut out, "UInt32", &format!("{prefix}_{}", upper_snake(field)), &spelled);
            if let Some(stem) = field.strip_suffix("_bit") {
                push_swift_const(
                    &mut out,
                    "UInt32",
                    &format!("{prefix}_{}_MASK", upper_snake(stem)),
                    &format!("0x{:X}", 1i64 << value),
                );
            }
        }
    }
    out.push_str("}\n");
    finish_swift(&out)?;
    Ok(out)
}

/// Emits a board's bindings as a Swift caseless-enum namespace: the SAME names and value
/// spellings as [`emit_board_csharp`] (BOARD_MODEL and the carrier USB identity ride the
/// wire/USB-natural `UInt16`; every fact constant is `UInt32`). A Swift firmware project
/// adds this one generated file and reads its board truth from it alone.
pub fn emit_board_swift(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    sources: &[String],
    regen: &str,
) -> Result<String, String> {
    let BoardEmissions {
        skipped,
        driver_families,
        sercom_uarts: uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        st_i2cs,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
        st_spis,
        nrf_twis,
        dw_i2cs,
        rp_adcs,
        rp_clocks,
    } = resolve_board_emissions(set, resolved)?;

    let mut out = String::new();
    let class = bindings_class(&resolved.board.board);
    let mut what = format!(
        "The {} board bindings as Swift constants, name/value-identical to {class}.g.cs:\n// every value below is a generation-time literal derived from the strata. Board truth\n// lives in board.toml, never here.",
        resolved.board.board,
    );
    if !skipped.is_empty() {
        what.push_str(&format!(
            "\n// NOT YET EMITTED (no Swift emitter for these binding kinds yet): {}.",
            skipped.join(", ")
        ));
    }
    emit_swift_header(&mut out, &what, sources, regen);
    out.push_str(&format!("\npublic enum {class} {{\n"));

    out.push_str("    // -- identity --\n");
    push_swift_const(&mut out, "UInt16", "BOARD_MODEL", &resolved.board.board_model.to_string());
    push_swift_const(&mut out, "String", "BOARD_VENDOR", &format!("\"{}\"", vendor_segment(&resolved.board.vendor)));
    if resolved.board.carrier.usb_vid > 0 {
        push_swift_const(&mut out, "UInt16", "CARRIER_USB_VID", &format!("0x{:04X}", resolved.board.carrier.usb_vid));
    }
    if resolved.board.carrier.usb_pid > 0 {
        push_swift_const(&mut out, "UInt16", "CARRIER_USB_PID", &format!("0x{:04X}", resolved.board.carrier.usb_pid));
    }

    if !driver_families.is_empty() {
        out.push_str(&driver_family_note("    "));
        for (role, family) in &driver_families {
            push_swift_const(
                &mut out,
                "String",
                &format!("{}_DRIVER_FAMILY", upper_snake(role)),
                &format!("\"{family}\""),
            );
        }
    }

    for uart in &uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n    // -- {p}: a sercom-usart binding descriptor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", uart.sercom_base));
        if uart.irq >= 0 {
            push_swift_const(&mut out, "UInt32", &format!("{p}_IRQ"), &uart.irq.to_string());
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_GCLK_CLKCTRL_VALUE"), &format!("0x{:X}", uart.gclk_clkctrl_value));
        push_swift_const(&mut out, "UInt32", &format!("{p}_APBC_MASK"), &format!("0x{:X}", uart.apbc_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_REG"), &format!("0x{:X}", uart.pmux_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_PAIR"), &format!("0x{:X}", uart.pmux_pair));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PINCFG_TX_REG"), &format!("0x{:X}", uart.pincfg_tx_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PINCFG_RX_REG"), &format!("0x{:X}", uart.pincfg_rx_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_TXPO"), &uart.txpo.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_RXPO"), &uart.rxpo.to_string());
        for (suffix, divisor) in &uart.bauds {
            push_swift_const(&mut out, "UInt32", &format!("{p}_{suffix}"), &format!("0x{divisor:X}"));
        }
    }

    for uart in &rp_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n    // -- {p}: a pl011 uart binding descriptor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", uart.reset_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_IO_TX_CTRL"), &format!("0x{:X}", uart.io_tx_ctrl));
        push_swift_const(&mut out, "UInt32", &format!("{p}_IO_RX_CTRL"), &format!("0x{:X}", uart.io_rx_ctrl));
        if let Some((pads_tx, pads_rx)) = &uart.pads {
            push_swift_const(&mut out, "UInt32", &format!("{p}_PADS_TX"), &format!("0x{pads_tx:X}"));
            push_swift_const(&mut out, "UInt32", &format!("{p}_PADS_RX"), &format!("0x{pads_rx:X}"));
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_FUNCSEL"), &uart.funcsel.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_CLK_PERI_HZ"), &uart.clk_peri_hz.to_string());
        for (suffix, ibrd, fbrd) in &uart.bauds {
            push_swift_const(&mut out, "UInt32", &format!("{p}_IBRD_{suffix}"), &ibrd.to_string());
            push_swift_const(&mut out, "UInt32", &format!("{p}_FBRD_{suffix}"), &fbrd.to_string());
        }
    }

    for uart in &esp_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n    // -- {p}: an esp32c6 hp-uart binding descriptor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PCR_CONF"), &format!("0x{:X}", uart.pcr_conf));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PCR_SCLK_CONF"), &format!("0x{:X}", uart.pcr_sclk_conf));
        push_swift_const(&mut out, "UInt32", &format!("{p}_IO_MUX_TX"), &format!("0x{:X}", uart.io_mux_tx));
        push_swift_const(&mut out, "UInt32", &format!("{p}_IO_MUX_RX"), &format!("0x{:X}", uart.io_mux_rx));
        push_swift_const(&mut out, "UInt32", &format!("{p}_MCU_SEL"), &uart.mcu_sel.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_SCLK_HZ"), &uart.sclk_hz.to_string());
    }

    for uart in &sam3x_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n    // -- {p}: a sam3x uart binding descriptor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PID"), &uart.pid.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMC_PCER_REG"), &format!("0x{:X}", uart.pmc_pcer_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMC_PCER_MASK"), &format!("0x{:X}", uart.pmc_pcer_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PIO_PDR_REG"), &format!("0x{:X}", uart.pio_pdr_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PIO_ABSR_REG"), &format!("0x{:X}", uart.pio_absr_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PIO_MASK"), &format!("0x{:X}", uart.pio_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PIO_FUNC"), &uart.pio_func.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_MCK_HZ"), &uart.mck_hz.to_string());
        for (suffix, cd) in &uart.bauds {
            push_swift_const(&mut out, "UInt32", &format!("{p}_BRGR_CD_{suffix}"), &cd.to_string());
        }
    }

    for uart in &st_uarts {
        let p = &uart.prefix;
        out.push_str(&format!("\n    // -- {p}: an st-usart binding descriptor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", uart.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", uart.rcc_en_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", uart.rcc_en_mask));
        for (side, pin) in [("TX", &uart.tx), ("RX", &uart.rx)] {
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_PCLK_HZ"), &uart.pclk_hz.to_string());
        for (suffix, divisor) in &uart.bauds {
            push_swift_const(&mut out, "UInt32", &format!("{p}_{suffix}"), &format!("0x{divisor:X}"));
        }
    }

    for spi in &sercom_spis {
        let p = &spi.prefix;
        out.push_str(&format!("\n    // -- {p}: a sercom-spi binding descriptor (core-clock id UNSHIFTED: the\n    // consumer composes ID | GEN | CLKEN per its runtime-selected plan) --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", spi.sercom_base));
        if spi.irq >= 0 {
            push_swift_const(&mut out, "UInt32", &format!("{p}_IRQ"), &spi.irq.to_string());
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_APBC_MASK"), &format!("0x{:X}", spi.apbc_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_GCLK_CORE_ID"), &spi.gclk_core_id.to_string());
        for (signal, pmux_reg, pmux_shift, pincfg_reg) in &spi.signals {
            let s = upper_snake(signal);
            push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_{s}_REG"), &format!("0x{pmux_reg:X}"));
            push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_{s}_SHIFT"), &pmux_shift.to_string());
            push_swift_const(&mut out, "UInt32", &format!("{p}_PINCFG_{s}_REG"), &format!("0x{pincfg_reg:X}"));
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_FUNC"), &spi.pmux_func.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_DOPO"), &spi.dopo.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_DIPO"), &spi.dipo.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_CS_PORT_BASE"), &format!("0x{:X}", spi.cs_port_base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_CS_PIN"), &spi.cs_pin.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_CS_MASK"), &format!("0x{:X}", spi.cs_mask));
    }
    for i2c in &sercom_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("
        // -- {p}: a sercom-i2c binding descriptor (the CORE-CLOCK RATE, not a
        // divisor: an I2C bus speed is a runtime Configure choice) --
"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_SERCOM_BASE"), &format!("0x{:X}", i2c.sercom_base));
        if i2c.irq >= 0 {
            push_swift_const(&mut out, "UInt32", &format!("{p}_IRQ"), &i2c.irq.to_string());
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_GCLK_CLKCTRL_VALUE"), &format!("0x{:X}", i2c.gclk_clkctrl_value));
        push_swift_const(&mut out, "UInt32", &format!("{p}_APBC_MASK"), &format!("0x{:X}", i2c.apbc_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_REG"), &format!("0x{:X}", i2c.pmux_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PMUX_PAIR"), &format!("0x{:X}", i2c.pmux_pair));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PINCFG_SDA_REG"), &format!("0x{:X}", i2c.pincfg_sda_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PINCFG_SCL_REG"), &format!("0x{:X}", i2c.pincfg_scl_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_CORE_CLOCK_HZ"), &i2c.core_clock_hz.to_string());
    }

    for spi in &pl022_spis {
        let p = &spi.prefix;
        out.push_str(&format!("\n    // -- {p}: a pl022 spi binding descriptor (rate = a runtime configuration\n    // choice from SSPCLK -- no divisor emits) --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", spi.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", spi.reset_mask));
        for (signal, io_ctrl, pads) in &spi.signals {
            let s = upper_snake(signal);
            push_swift_const(&mut out, "UInt32", &format!("{p}_IO_{s}_CTRL"), &format!("0x{io_ctrl:X}"));
            push_swift_const(&mut out, "UInt32", &format!("{p}_PADS_{s}"), &format!("0x{pads:X}"));
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_FUNCSEL"), &spi.funcsel.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_SSPCLK_HZ"), &spi.sspclk_hz.to_string());
    }


    for spi in &st_spis {
        let p = &spi.prefix;
        out.push_str(&format!("
    // -- {p}: an st-spi binding descriptor. NO baud emits: an SPI master's rate is a property of the ATTACHED DEVICE, and a binding names a bus. The APB rate is here and the block table states the eight prescaler codes; the driver picks --
"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", spi.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", spi.rcc_en_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", spi.rcc_en_mask));
        for (side, pin) in [("SCK", &spi.sck), ("MISO", &spi.miso), ("MOSI", &spi.mosi)] {
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        if let Some(cs) = &spi.cs {
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_PORT_RCC_EN_REG"), &format!("0x{:X}", cs.port_rcc_en_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_PORT_RCC_EN_MASK"), &format!("0x{:X}", cs.port_rcc_en_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_MODER_REG"), &format!("0x{:X}", cs.moder_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_MODER_MASK"), &format!("0x{:X}", cs.moder_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_MODER_VALUE"), &format!("0x{:X}", cs.moder_value));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_BSRR_REG"), &format!("0x{:X}", cs.bsrr_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_BSRR_SET"), &format!("0x{:X}", cs.bsrr_set));
            push_swift_const(&mut out, "UInt32", &format!("{p}_CS_BSRR_CLEAR"), &format!("0x{:X}", cs.bsrr_clear));
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_PCLK_HZ"), &spi.pclk_hz.to_string());
    }

    for i2c in &st_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("\n    // -- {p}: an st-i2c binding descriptor. BOTH PINS ARE OPEN DRAIN --\n    // a push-pull output cannot be pulled low by the device at the other end, so an\n    // acknowledge is fought instead of seen and nothing ever answers, with the mux\n    // perfectly correct. The timing words are the manual's own compliant points at\n    // this plan's kernel rate, composed here: five counts, not a divisor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", i2c.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RCC_EN_REG"), &format!("0x{:X}", i2c.rcc_en_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RCC_EN_MASK"), &format!("0x{:X}", i2c.rcc_en_mask));
        for (side, pin) in [("SCL", &i2c.scl), ("SDA", &i2c.sda)] {
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_PORT_RCC_EN_REG"), &format!("0x{:X}", pin.port_rcc_en_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_PORT_RCC_EN_MASK"), &format!("0x{:X}", pin.port_rcc_en_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_REG"), &format!("0x{:X}", pin.moder_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_MASK"), &format!("0x{:X}", pin.moder_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_MODER_VALUE"), &format!("0x{:X}", pin.moder_value));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_REG"), &format!("0x{:X}", pin.afr_reg));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_MASK"), &format!("0x{:X}", pin.afr_mask));
            push_swift_const(&mut out, "UInt32", &format!("{p}_{side}_AFR_VALUE"), &format!("0x{:X}", pin.afr_value));
        }
        push_swift_const(&mut out, "UInt32", &format!("{p}_OTYPER_REG"), &format!("0x{:X}", i2c.otyper.0));
        push_swift_const(&mut out, "UInt32", &format!("{p}_OTYPER_SCL_MASK"), &format!("0x{:X}", i2c.otyper.1));
        push_swift_const(&mut out, "UInt32", &format!("{p}_OTYPER_SDA_MASK"), &format!("0x{:X}", i2c.otyper.2));
        push_swift_const(&mut out, "UInt32", &format!("{p}_KERNEL_HZ"), &i2c.kernel_hz.to_string());
        for (suffix, word) in &i2c.timings {
            push_swift_const(&mut out, "UInt32", &format!("{p}_{suffix}"), &format!("0x{word:X}"));
        }
    }
    for twi in &nrf_twis {
        let p = &twi.prefix;
        out.push_str(&format!("\n    // -- {p}: an nrf-twi binding descriptor --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_TWI_BASE"), &format!("0x{:X}", twi.twi_base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PSEL_SCL"), &format!("0x{:X}", twi.psel_scl));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PSEL_SDA"), &format!("0x{:X}", twi.psel_sda));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PIN_CNF_SCL_REG"), &format!("0x{:X}", twi.pin_cnf_scl_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PIN_CNF_SDA_REG"), &format!("0x{:X}", twi.pin_cnf_sda_reg));
    }

    for i2c in &dw_i2cs {
        let p = &i2c.prefix;
        out.push_str(&format!("\n    // -- {p}: a dw-i2c binding descriptor (ic_clk = clk_sys on this chip; the\n    // SCL count formulas stay driver math per the official pico-sdk driver) --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", i2c.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", i2c.reset_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_IO_SDA_CTRL"), &format!("0x{:X}", i2c.io_sda_ctrl));
        push_swift_const(&mut out, "UInt32", &format!("{p}_IO_SCL_CTRL"), &format!("0x{:X}", i2c.io_scl_ctrl));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PADS_SDA"), &format!("0x{:X}", i2c.pads_sda));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PADS_SCL"), &format!("0x{:X}", i2c.pads_scl));
        push_swift_const(&mut out, "UInt32", &format!("{p}_FUNCSEL"), &i2c.funcsel.to_string());
        push_swift_const(&mut out, "UInt32", &format!("{p}_IC_CLK_HZ"), &i2c.ic_clk_hz.to_string());
    }

    for adc in &rp_adcs {
        let p = &adc.prefix;
        out.push_str(&format!("\n    // -- {p}: an rp-adc binding descriptor (the reference rail is board truth;\n    // the channel map + calibration records are chip truth in the adc layout) --\n"));
        push_swift_const(&mut out, "UInt32", &format!("{p}_BASE"), &format!("0x{:X}", adc.base));
        push_swift_const(&mut out, "UInt32", &format!("{p}_RESET_MASK"), &format!("0x{:X}", adc.reset_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_REFERENCE_UV"), &adc.reference_uv.to_string());
    }

    for clock in &rp_clocks {
        let s = &clock.plan;
        out.push_str("\n    // -- the clock plan (state-and-verify: the plan states the chosen PLL\n    // values AND the resulting rates; generation verified hz == xosc * fbdiv /\n    // (postdiv1 * postdiv2) and composed each PRIM word from the pll block fields).\n    // Every const carries the plan name as a suffix, the default plan included, so a\n    // name states which operating point it means -- a board may declare several --\n");
        push_swift_const(&mut out, "UInt32", &format!("XOSC_HZ_{s}"), &clock.xosc_hz.to_string());
        push_swift_const(&mut out, "UInt32", &format!("CLK_SYS_HZ_{s}"), &clock.clk_sys_hz.to_string());
        push_swift_const(&mut out, "UInt32", &format!("CLK_USB_HZ_{s}"), &clock.clk_usb_hz.to_string());
        if clock.clk_adc_hz >= 0 {
            push_swift_const(&mut out, "UInt32", &format!("CLK_ADC_HZ_{s}"), &clock.clk_adc_hz.to_string());
        }
        push_swift_const(&mut out, "UInt32", &format!("PLL_SYS_FBDIV_{s}"), &clock.pll_sys_fbdiv.to_string());
        push_swift_const(&mut out, "UInt32", &format!("PLL_SYS_PRIM_{s}"), &format!("0x{:X}", clock.pll_sys_prim));
        push_swift_const(&mut out, "UInt32", &format!("PLL_USB_FBDIV_{s}"), &clock.pll_usb_fbdiv.to_string());
        push_swift_const(&mut out, "UInt32", &format!("PLL_USB_PRIM_{s}"), &format!("0x{:X}", clock.pll_usb_prim));
    }

    let controls: Vec<(&str, &[ControlPin])> = vec![
        ("module control lines", &resolved.module_pins),
        ("on-board devices", &resolved.board.devices),
    ];
    for (label, pins) in controls {
        if pins.is_empty() {
            continue;
        }
        out.push_str(&format!("\n    // -- {label}: PORT group base + pin index + mask --\n"));
        for control in pins {
            if control.pin.is_empty() {
                let p = upper_snake(&control.name);
                push_swift_const(&mut out, "UInt32", &format!("{p}_ADDRESS"), &format!("0x{:X}", control.address));
                continue;
            }
            let (group_base, index) = control_pin_group_base(set, &resolved.board.board, &control.pin)?;
            let p = upper_snake(&control.name);
            push_swift_const(&mut out, "UInt32", &format!("{p}_PORT_BASE"), &format!("0x{group_base:X}"));
            push_swift_const(&mut out, "UInt32", &format!("{p}_PIN"), &index.to_string());
            push_swift_const(&mut out, "UInt32", &format!("{p}_MASK"), &format!("0x{:X}", 1u64 << index));
            push_swift_const(
                &mut out,
                "UInt32",
                &format!("{p}_ACTIVE_LOW"),
                if control.active == "low" { "1" } else { "0" },
            );
        }
    }

    let rows_at = board_fact_rows(set, resolved)?;
    for row in rows_at.iter() {
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "    ", "//", comment),
            Row::Uint(name, value) => push_swift_const(&mut out, "UInt32", name, value),
            Row::Int(name, value) => push_swift_const(&mut out, "Int32", name, value),
            Row::Str(name, value) => push_swift_const(&mut out, "StaticString", name, &format!("\"{value}\"")),
        }
    }

    out.push_str("}\n");
    finish_swift(&out)?;
    Ok(out)
}


/// Emits a board's bindings as the generated Python `board` module,
/// `bsp/<board>/python/board.py`:
/// role handles whose values are the role-id strings, per-role FACTS dicts carrying
/// the SAME resolved values as the C# bindings class under ONE mechanical renaming rule
/// (`VCP_SERCOM_BASE` -> `FACTS["vcp"]["sercom_base"]`; the anchors stay string-greppable
/// in every emission), plus CARRIER/PLANS/DEVICES. Every value is a single resolved literal
/// with its hex spelling preserved; no imports, no arithmetic, no name references -- nothing
/// for a consumer tier to mis-fold, nothing to import-fail on a sim.
pub fn emit_board_python(
    set: &FamilySet,
    resolved: &ResolvedBoard,
    sources: &[String],
    regen: &str,
) -> Result<String, String> {
    let BoardEmissions {
        skipped,
        driver_families,
        sercom_uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        st_i2cs,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
        st_spis,
        nrf_twis,
        dw_i2cs,
        rp_adcs,
        rp_clocks: _,
    } = resolve_board_emissions(set, resolved)?;

    let mut out = String::new();
    out.push_str(&format!(
        "# GENERATED by lamella-bsp-gen from {list} -- DO NOT EDIT.\n# Regenerate: {regen}\n",
        list = sources.join(" + "),
    ));
    if !skipped.is_empty() {
        out.push_str(&format!(
            "# NOT YET EMITTED (no emitter for these binding kinds): {}.\n",
            skipped.join(", ")
        ));
    }
    out.push('\n');
    out.push_str(&format!("BOARD = \"{}\"\n", resolved.board.board));
    out.push_str(&format!("BOARD_MODEL = {}\n", resolved.board.board_model));
    out.push_str(&format!("BOARD_VENDOR = \"{}\"\n", vendor_segment(&resolved.board.vendor)));

    let mut roles: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    for uart in &sercom_uarts {
        let mut rows = vec![
            ("kind".to_string(), "\"uart\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", uart.instance)),
            ("sercom_base".to_string(), format!("0x{:X}", uart.sercom_base)),
        ];
        if uart.irq >= 0 {
            rows.push(("irq".to_string(), uart.irq.to_string()));
        }
        rows.push(("gclk_clkctrl_value".to_string(), format!("0x{:X}", uart.gclk_clkctrl_value)));
        rows.push(("apbc_mask".to_string(), format!("0x{:X}", uart.apbc_mask)));
        rows.push(("pmux_reg".to_string(), format!("0x{:X}", uart.pmux_reg)));
        rows.push(("pmux_pair".to_string(), format!("0x{:X}", uart.pmux_pair)));
        rows.push(("pincfg_tx_reg".to_string(), format!("0x{:X}", uart.pincfg_tx_reg)));
        rows.push(("pincfg_rx_reg".to_string(), format!("0x{:X}", uart.pincfg_rx_reg)));
        rows.push(("txpo".to_string(), uart.txpo.to_string()));
        rows.push(("rxpo".to_string(), uart.rxpo.to_string()));
        for (suffix, divisor) in &uart.bauds {
            rows.push((suffix.to_ascii_lowercase(), format!("0x{divisor:X}")));
        }
        roles.push((&uart.role, rows));
    }
    for uart in &rp_uarts {
        let mut rows = vec![
            ("kind".to_string(), "\"uart\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", uart.instance)),
            ("base".to_string(), format!("0x{:X}", uart.base)),
            ("reset_mask".to_string(), format!("0x{:X}", uart.reset_mask)),
            ("io_tx_ctrl".to_string(), format!("0x{:X}", uart.io_tx_ctrl)),
            ("io_rx_ctrl".to_string(), format!("0x{:X}", uart.io_rx_ctrl)),
        ];
        if let Some((pads_tx, pads_rx)) = &uart.pads {
            rows.push(("pads_tx".to_string(), format!("0x{pads_tx:X}")));
            rows.push(("pads_rx".to_string(), format!("0x{pads_rx:X}")));
        }
        rows.push(("funcsel".to_string(), uart.funcsel.to_string()));
        rows.push(("clk_peri_hz".to_string(), uart.clk_peri_hz.to_string()));
        for (suffix, ibrd, fbrd) in &uart.bauds {
            rows.push((format!("ibrd_{}", suffix.to_ascii_lowercase()), ibrd.to_string()));
            rows.push((format!("fbrd_{}", suffix.to_ascii_lowercase()), fbrd.to_string()));
        }
        roles.push((&uart.role, rows));
    }
    for uart in &esp_uarts {
        roles.push((
            &uart.role,
            vec![
                ("kind".to_string(), "\"uart\"".to_string()),
                ("instance".to_string(), format!("\"{}\"", uart.instance)),
                ("base".to_string(), format!("0x{:X}", uart.base)),
                ("pcr_conf".to_string(), format!("0x{:X}", uart.pcr_conf)),
                ("pcr_sclk_conf".to_string(), format!("0x{:X}", uart.pcr_sclk_conf)),
                ("io_mux_tx".to_string(), format!("0x{:X}", uart.io_mux_tx)),
                ("io_mux_rx".to_string(), format!("0x{:X}", uart.io_mux_rx)),
                ("mcu_sel".to_string(), uart.mcu_sel.to_string()),
                ("sclk_hz".to_string(), uart.sclk_hz.to_string()),
            ],
        ));
    }
    for uart in &sam3x_uarts {
        let mut rows = vec![
            ("kind".to_string(), "\"uart\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", uart.instance)),
            ("base".to_string(), format!("0x{:X}", uart.base)),
            ("pid".to_string(), uart.pid.to_string()),
            ("pmc_pcer_reg".to_string(), format!("0x{:X}", uart.pmc_pcer_reg)),
            ("pmc_pcer_mask".to_string(), format!("0x{:X}", uart.pmc_pcer_mask)),
            ("pio_pdr_reg".to_string(), format!("0x{:X}", uart.pio_pdr_reg)),
            ("pio_absr_reg".to_string(), format!("0x{:X}", uart.pio_absr_reg)),
            ("pio_mask".to_string(), format!("0x{:X}", uart.pio_mask)),
            ("pio_func".to_string(), uart.pio_func.to_string()),
            ("mck_hz".to_string(), uart.mck_hz.to_string()),
        ];
        for (suffix, cd) in &uart.bauds {
            rows.push((format!("brgr_cd_{}", suffix.to_ascii_lowercase()), cd.to_string()));
        }
        roles.push((&uart.role, rows));
    }
    for i2c in &st_i2cs {
        let mut rows = vec![
            ("kind".to_string(), "\"i2c\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", i2c.instance)),
            ("base".to_string(), format!("0x{:X}", i2c.base)),
            ("rcc_en_reg".to_string(), format!("0x{:X}", i2c.rcc_en_reg)),
            ("rcc_en_mask".to_string(), format!("0x{:X}", i2c.rcc_en_mask)),
            ("kernel_hz".to_string(), i2c.kernel_hz.to_string()),
        ];
        for (side, pin) in [("scl", &i2c.scl), ("sda", &i2c.sda)] {
            rows.push((format!("{side}_port_rcc_en_reg"), format!("0x{:X}", pin.port_rcc_en_reg)));
            rows.push((format!("{side}_port_rcc_en_mask"), format!("0x{:X}", pin.port_rcc_en_mask)));
            rows.push((format!("{side}_moder_reg"), format!("0x{:X}", pin.moder_reg)));
            rows.push((format!("{side}_moder_mask"), format!("0x{:X}", pin.moder_mask)));
            rows.push((format!("{side}_moder_value"), format!("0x{:X}", pin.moder_value)));
            rows.push((format!("{side}_afr_reg"), format!("0x{:X}", pin.afr_reg)));
            rows.push((format!("{side}_afr_mask"), format!("0x{:X}", pin.afr_mask)));
            rows.push((format!("{side}_afr_value"), format!("0x{:X}", pin.afr_value)));
        }
        rows.push(("otyper_reg".to_string(), format!("0x{:X}", i2c.otyper.0)));
        rows.push(("otyper_scl_mask".to_string(), format!("0x{:X}", i2c.otyper.1)));
        rows.push(("otyper_sda_mask".to_string(), format!("0x{:X}", i2c.otyper.2)));
        for (suffix, word) in &i2c.timings {
            rows.push((suffix.to_ascii_lowercase(), format!("0x{word:X}")));
        }
        roles.push((&i2c.role, rows));
    }
    for uart in &st_uarts {
        let mut rows = vec![
            ("kind".to_string(), "\"uart\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", uart.instance)),
            ("base".to_string(), format!("0x{:X}", uart.base)),
            ("rcc_en_reg".to_string(), format!("0x{:X}", uart.rcc_en_reg)),
            ("rcc_en_mask".to_string(), format!("0x{:X}", uart.rcc_en_mask)),
            ("pclk_hz".to_string(), uart.pclk_hz.to_string()),
        ];
        for (side, pin) in [("tx", &uart.tx), ("rx", &uart.rx)] {
            rows.push((format!("{side}_port_rcc_en_reg"), format!("0x{:X}", pin.port_rcc_en_reg)));
            rows.push((format!("{side}_port_rcc_en_mask"), format!("0x{:X}", pin.port_rcc_en_mask)));
            rows.push((format!("{side}_moder_reg"), format!("0x{:X}", pin.moder_reg)));
            rows.push((format!("{side}_moder_mask"), format!("0x{:X}", pin.moder_mask)));
            rows.push((format!("{side}_moder_value"), format!("0x{:X}", pin.moder_value)));
            rows.push((format!("{side}_afr_reg"), format!("0x{:X}", pin.afr_reg)));
            rows.push((format!("{side}_afr_mask"), format!("0x{:X}", pin.afr_mask)));
            rows.push((format!("{side}_afr_value"), format!("0x{:X}", pin.afr_value)));
        }
        for (suffix, divisor) in &uart.bauds {
            rows.push((suffix.to_ascii_lowercase(), format!("0x{divisor:X}")));
        }
        roles.push((&uart.role, rows));
    }
    for spi in &sercom_spis {
        let mut rows = vec![
            ("kind".to_string(), "\"spi\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", spi.instance)),
            ("sercom_base".to_string(), format!("0x{:X}", spi.sercom_base)),
        ];
        if spi.irq >= 0 {
            rows.push(("irq".to_string(), spi.irq.to_string()));
        }
        rows.push(("apbc_mask".to_string(), format!("0x{:X}", spi.apbc_mask)));
        rows.push(("gclk_core_id".to_string(), spi.gclk_core_id.to_string()));
        for (signal, pmux_reg, pmux_shift, pincfg_reg) in &spi.signals {
            rows.push((format!("pmux_{signal}_reg"), format!("0x{pmux_reg:X}")));
            rows.push((format!("pmux_{signal}_shift"), pmux_shift.to_string()));
            rows.push((format!("pincfg_{signal}_reg"), format!("0x{pincfg_reg:X}")));
        }
        rows.push(("pmux_func".to_string(), spi.pmux_func.to_string()));
        rows.push(("dopo".to_string(), spi.dopo.to_string()));
        rows.push(("dipo".to_string(), spi.dipo.to_string()));
        rows.push(("cs_port_base".to_string(), format!("0x{:X}", spi.cs_port_base)));
        rows.push(("cs_pin".to_string(), spi.cs_pin.to_string()));
        rows.push(("cs_mask".to_string(), format!("0x{:X}", spi.cs_mask)));
        roles.push((&spi.role, rows));
    }
    for i2c in &sercom_i2cs {
        let mut rows = vec![
            ("kind".to_string(), "\"i2c\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", i2c.instance)),
            ("sercom_base".to_string(), format!("0x{:X}", i2c.sercom_base)),
        ];
        if i2c.irq >= 0 {
            rows.push(("irq".to_string(), i2c.irq.to_string()));
        }
        rows.push(("gclk_clkctrl_value".to_string(), format!("0x{:X}", i2c.gclk_clkctrl_value)));
        rows.push(("apbc_mask".to_string(), format!("0x{:X}", i2c.apbc_mask)));
        rows.push(("pmux_reg".to_string(), format!("0x{:X}", i2c.pmux_reg)));
        rows.push(("pmux_pair".to_string(), format!("0x{:X}", i2c.pmux_pair)));
        rows.push(("pincfg_sda_reg".to_string(), format!("0x{:X}", i2c.pincfg_sda_reg)));
        rows.push(("pincfg_scl_reg".to_string(), format!("0x{:X}", i2c.pincfg_scl_reg)));
        rows.push(("core_clock_hz".to_string(), i2c.core_clock_hz.to_string()));
        roles.push((&i2c.role, rows));
    }
    for spi in &pl022_spis {
        let mut rows = vec![
            ("kind".to_string(), "\"spi\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", spi.instance)),
            ("base".to_string(), format!("0x{:X}", spi.base)),
            ("reset_mask".to_string(), format!("0x{:X}", spi.reset_mask)),
        ];
        for (signal, io_ctrl, pads) in &spi.signals {
            rows.push((format!("io_{signal}_ctrl"), format!("0x{io_ctrl:X}")));
            rows.push((format!("pads_{signal}"), format!("0x{pads:X}")));
        }
        rows.push(("funcsel".to_string(), spi.funcsel.to_string()));
        rows.push(("sspclk_hz".to_string(), spi.sspclk_hz.to_string()));
        roles.push((&spi.role, rows));
    }
    for spi in &st_spis {
        let mut rows = vec![
            ("kind".to_string(), "\"spi\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", spi.instance)),
            ("base".to_string(), format!("0x{:X}", spi.base)),
            ("rcc_en_reg".to_string(), format!("0x{:X}", spi.rcc_en_reg)),
            ("rcc_en_mask".to_string(), format!("0x{:X}", spi.rcc_en_mask)),
        ];
        for (side, pin) in [("sck", &spi.sck), ("miso", &spi.miso), ("mosi", &spi.mosi)] {
            rows.push((format!("{side}_port_rcc_en_reg"), format!("0x{:X}", pin.port_rcc_en_reg)));
            rows.push((format!("{side}_port_rcc_en_mask"), format!("0x{:X}", pin.port_rcc_en_mask)));
            rows.push((format!("{side}_moder_reg"), format!("0x{:X}", pin.moder_reg)));
            rows.push((format!("{side}_moder_mask"), format!("0x{:X}", pin.moder_mask)));
            rows.push((format!("{side}_moder_value"), format!("0x{:X}", pin.moder_value)));
            rows.push((format!("{side}_afr_reg"), format!("0x{:X}", pin.afr_reg)));
            rows.push((format!("{side}_afr_mask"), format!("0x{:X}", pin.afr_mask)));
            rows.push((format!("{side}_afr_value"), format!("0x{:X}", pin.afr_value)));
        }
        if let Some(cs) = &spi.cs {
            rows.push(("cs_port_rcc_en_reg".to_string(), format!("0x{:X}", cs.port_rcc_en_reg)));
            rows.push(("cs_port_rcc_en_mask".to_string(), format!("0x{:X}", cs.port_rcc_en_mask)));
            rows.push(("cs_moder_reg".to_string(), format!("0x{:X}", cs.moder_reg)));
            rows.push(("cs_moder_mask".to_string(), format!("0x{:X}", cs.moder_mask)));
            rows.push(("cs_moder_value".to_string(), format!("0x{:X}", cs.moder_value)));
            rows.push(("cs_bsrr_reg".to_string(), format!("0x{:X}", cs.bsrr_reg)));
            rows.push(("cs_bsrr_set".to_string(), format!("0x{:X}", cs.bsrr_set)));
            rows.push(("cs_bsrr_clear".to_string(), format!("0x{:X}", cs.bsrr_clear)));
        }
        rows.push(("pclk_hz".to_string(), spi.pclk_hz.to_string()));
        roles.push((&spi.role, rows));
    }
    for twi in &nrf_twis {
        roles.push((
            &twi.role,
            vec![
                ("kind".to_string(), "\"i2c\"".to_string()),
                ("instance".to_string(), format!("\"{}\"", twi.instance)),
                ("twi_base".to_string(), format!("0x{:X}", twi.twi_base)),
                ("psel_scl".to_string(), format!("0x{:X}", twi.psel_scl)),
                ("psel_sda".to_string(), format!("0x{:X}", twi.psel_sda)),
                ("pin_cnf_scl_reg".to_string(), format!("0x{:X}", twi.pin_cnf_scl_reg)),
                ("pin_cnf_sda_reg".to_string(), format!("0x{:X}", twi.pin_cnf_sda_reg)),
            ],
        ));
    }
    for i2c in &dw_i2cs {
        roles.push((
            &i2c.role,
            vec![
                ("kind".to_string(), "\"i2c\"".to_string()),
                ("instance".to_string(), format!("\"{}\"", i2c.instance)),
                ("base".to_string(), format!("0x{:X}", i2c.base)),
                ("reset_mask".to_string(), format!("0x{:X}", i2c.reset_mask)),
                ("io_sda_ctrl".to_string(), format!("0x{:X}", i2c.io_sda_ctrl)),
                ("io_scl_ctrl".to_string(), format!("0x{:X}", i2c.io_scl_ctrl)),
                ("pads_sda".to_string(), format!("0x{:X}", i2c.pads_sda)),
                ("pads_scl".to_string(), format!("0x{:X}", i2c.pads_scl)),
                ("funcsel".to_string(), i2c.funcsel.to_string()),
                ("ic_clk_hz".to_string(), i2c.ic_clk_hz.to_string()),
            ],
        ));
    }
    for adc in &rp_adcs {
        roles.push((
            &adc.role,
            vec![
                ("kind".to_string(), "\"adc\"".to_string()),
                ("instance".to_string(), format!("\"{}\"", adc.instance)),
                ("base".to_string(), format!("0x{:X}", adc.base)),
                ("reset_mask".to_string(), format!("0x{:X}", adc.reset_mask)),
                ("reference_uv".to_string(), adc.reference_uv.to_string()),
            ],
        ));
    }

    for (role, rows) in &mut roles {
        let Some((_, family)) = driver_families.iter().find(|(r, _)| r == role) else {
            return Err(format!(
                "{}: role '{role}' has a descriptor but no driver family -- the two are recorded together, so one without the other is a generator defect",
                resolved.board.board
            ));
        };
        let at = rows.iter().position(|(key, _)| key == "kind").map_or(0, |at| at + 1);
        rows.insert(at, ("driver_family".to_string(), format!("\"{family}\"")));
    }

    out.push_str(
        "\n# Role handles: the ONLY peripheral names an app sees. The value of a\n# role handle is its role-id string; the runtime resolves role -> facts through FACTS\n# below, never through a surface-private enum.\n",
    );
    for (role, _) in &roles {
        out.push_str(&format!("{} = \"{role}\"\n", upper_snake(role)));
    }

    let carrier = &resolved.board.carrier;
    out.push_str("\nCARRIER = {\n");
    out.push_str(&format!("    \"kind\": \"{}\",\n", carrier.kind));
    if carrier.usb_vid > 0 {
        out.push_str(&format!("    \"usb_vid\": 0x{:04X},\n", carrier.usb_vid));
    }
    if carrier.usb_pid > 0 {
        out.push_str(&format!("    \"usb_pid\": 0x{:04X},\n", carrier.usb_pid));
    }
    if !carrier.role.is_empty() {
        out.push_str(&format!("    \"role\": \"{}\",\n", carrier.role));
    }
    out.push_str("}\n");

    out.push_str(
        "\n# Per-role descriptor dicts, grouped by the role each belongs to.\n# Emitted from this board's facts, like every other language's support for it:\n# an UPPER_SNAKE name in the shared facts is a lowercase key here.\n#\n# \"kind\" and \"driver_family\" are read TOGETHER and neither answers alone: kind is what the\n# application asked for -- a uart, an spi -- and driver_family is which REGISTER MAP is behind\n# it, as <chip family>-<block>. One SERCOM block serves uart, spi and i2c, so the block does not\n# name a driver; a uart is a different register map on every family, so the kind does not either.\nFACTS = {\n",
    );
    for (role, rows) in &roles {
        out.push_str(&format!("    \"{role}\": {{\n"));
        for (key, value) in rows {
            out.push_str(&format!("        \"{key}\": {value},\n"));
        }
        out.push_str("    },\n");
    }
    out.push_str("}\n");

    out.push_str(
        "\n# The chip's instance map: every block this family places, with its base address and\n# the block layout it follows. A role descriptor above states a PERIPHERAL; a bring-up also\n# touches blocks that belong to the chip rather than to any one role -- an oscillator, a clock\n# controller, a reset controller -- and those are one per chip, so they are stated once here.\n# Bases only: register offsets and bit encodings belong to the driver that knows the block.\nINSTANCES = {\n",
    );
    for row in &set.instances.rows {
        let mut entry = format!("    \"{}\": {{\"block\": \"{}\"", row.name, row.block);
        for (at, field) in set.instances.record.iter().enumerate() {
            let Some(value) = row.values.get(at) else { continue };
            if *value < 0 {
                continue;
            }
            entry.push_str(&format!(", \"{field}\": 0x{value:X}"));
        }
        out.push_str(&format!("{entry}}},\n"));
    }
    out.push_str("}\n");

    out.push_str("\nPLANS = {\n");
    for plan in &resolved.board.plans {
        let mut entry = format!(
            "    \"{}\": {{\"default\": {}, \"source\": \"{}\"",
            plan.name,
            if plan.default { "True" } else { "False" },
            plan.source
        );
        for (key, value) in &plan.rates {
            entry.push_str(&format!(", \"{key}\": {value}"));
        }
        entry.push_str("},\n");
        out.push_str(&entry);
    }
    out.push_str("}\n");

    out.push_str(
        "\n# On-board devices + module control lines: PORT group base + pin index + mask + polarity.\n# Emitted from this board's facts; each supported language states the same set in its\n# own idiom.\nDEVICES = {\n",
    );
    for control in resolved.module_pins.iter().chain(resolved.board.devices.iter()) {
        let kind = if control.kind.is_empty() {
            String::new()
        } else {
            format!("\"kind\": \"{}\", ", control.kind)
        };
        if control.pin.is_empty() {
            out.push_str(&format!(
                "    \"{}\": {{{kind}\"role\": \"{}\", \"address\": 0x{:X}}},\n",
                control.name, control.role, control.address
            ));
            continue;
        }
        let (group_base, index) = control_pin_group_base(set, &resolved.board.board, &control.pin)?;
        out.push_str(&format!(
            "    \"{}\": {{{kind}\"port_base\": 0x{group_base:X}, \"pin\": {index}, \"mask\": 0x{:X}, \"active_low\": {}}},\n",
            control.name,
            1u64 << index,
            if control.active == "low" { "True" } else { "False" }
        ));
    }
    out.push_str("}\n");

    if !resolved.board.memory.is_empty() {
        out.push_str(
            "\n# Memory regions the board fits. A region with a \"controller\" does not exist until\n# that instance is brought up; touching it first is a bus fault, not a wrong value.\nMEMORY = {\n",
        );
        for region in &resolved.board.memory {
            let mut entry = format!("    \"{}\": {{\"kind\": \"{}\"", region.name, region.kind);
            if region.base >= 0 {
                entry.push_str(&format!(", \"base\": 0x{:X}", region.base));
            }
            entry.push_str(&format!(", \"size\": 0x{:X}", region.size));
            if region.device_size >= 0 && region.device_size != region.size {
                entry.push_str(&format!(", \"device_size\": 0x{:X}", region.device_size));
            }
            if !region.controller.is_empty() {
                entry.push_str(&format!(", \"controller\": \"{}\"", region.controller));
            }
            entry.push_str(&format!(
                ", \"optional\": {}",
                if region.optional { "True" } else { "False" }
            ));
            let device: Vec<String> = memory_device_rows(set, resolved, region)?
                .iter()
                .filter_map(|row| {
                    let head = format!("MEMORY_{}_", upper_snake(&region.name));
                    match row {
                        Row::Uint(name, value) | Row::Int(name, value) => {
                            Some(format!("\"{}\": {value}", name.strip_prefix(&head)?.to_lowercase()))
                        }
                        Row::Str(name, value) => {
                            Some(format!("\"{}\": \"{value}\"", name.strip_prefix(&head)?.to_lowercase()))
                        }
                        _ => None,
                    }
                })
                .collect();
            if !device.is_empty() {
                entry.push_str(&format!(", \"device\": {{{}}}", device.join(", ")));
            }
            entry.push_str("},\n");
            out.push_str(&entry);
        }
        out.push_str("}\n");
    }

    if !resolved.board.discriminators.is_empty() {
        out.push_str(
            "\n# What an attached board can be asked, to confirm it is the board an image was built\n# for. A chip identity register cannot answer this alone: the parts that separate one board\n# from its sibling are soldered outside the die, so a bare board answers the same identity as\n# a populated one. Each row names the CLAIM it reaches -- \"part\", or \"memory:<region>\" -- and\n# the rung a successful read of its kind establishes. A region's ACCESSIBLE size is reachable\n# only at \"exercised\": an identity read reports the fitted device, and a board may wire less\n# of a device than it holds.\nDISCRIMINATORS = {\n",
        );
        for row in &resolved.board.discriminators {
            out.push_str(&format!(
                "    \"{}\": {{\"confirms\": \"{}\", \"validation\": \"{}\", \"expect\": 0x{:X}, \"reads\": \"{}\"}},\n",
                row.name, row.confirms, row.validation, row.expect, row.reads
            ));
        }
        out.push_str("}\n");
    }

    if !resolved.board.connectors.is_empty() {
        out.push_str(
            "\n# The sockets a removable module plugs into. The socket is board truth -- it is on the\n# schematic and identical on every unit -- and what is plugged into it is not, so no entry here\n# names a module. \"buses\" holds the roles a socket brings out whole; \"pins\" holds the single\n# lines, each under the standard's own name for that position. Which of a socket's protocols an\n# attached module speaks is a property of the module, so a board that offers several states all\n# of them and chooses none.\nCONNECTORS = {\n",
        );
        for connector in &resolved.board.connectors {
            let buses: Vec<String> = connector
                .buses
                .iter()
                .map(|bus| format!("\"{}\": \"{}\"", bus.signal, bus.role))
                .collect();
            let mut pins: Vec<String> = Vec::new();
            for line in &connector.pins {
                let (group_base, index) =
                    control_pin_group_base(set, &resolved.board.board, &line.pin)?;
                pins.push(format!(
                    "\"{}\": {{\"port_base\": 0x{group_base:X}, \"pin\": {index}, \"mask\": 0x{:X}}}",
                    line.signal,
                    1u64 << index
                ));
            }
            out.push_str(&format!(
                "    \"{}\": {{\"standard\": \"{}\", \"buses\": {{{}}}, \"pins\": {{{}}}}},\n",
                connector.name,
                connector.standard,
                buses.join(", "),
                pins.join(", ")
            ));
        }
        out.push_str("}\n");
    }
    Ok(out)
}


/// One part family's tables: the optional authoring base plus every part that is emitted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSet {
    /// The part family id (the `parts/<family>/` directory name).
    pub family: String,
    /// The family's authoring base, when it has one.
    pub base: Option<DeviceTable>,
    /// The emitted parts, sorted by part id.
    pub parts: Vec<DeviceTable>,
}

/// Loads `parts/<family>/*.toml` -- the authoring base (at most one) and every part.
pub fn load_device_family(repo_root: &std::path::Path, family: &str) -> Result<DeviceSet, String> {
    let dir = repo_root.join("parts").join(family);
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();

    let mut set = DeviceSet { family: family.to_string(), ..DeviceSet::default() };
    for path in &paths {
        let Strata::Device(table) = parse(&read(path)?)? else {
            return Err(format!("{}: expected kind = \"device\" or \"device-base\"", path.display()));
        };
        if table.family != family {
            return Err(format!("{}: part belongs to family '{}'", path.display(), table.family));
        }
        if table.is_base {
            if set.base.is_some() {
                return Err(format!(
                    "{}: family '{family}' declares a second base -- a member states its deltas against exactly one table",
                    path.display()
                ));
            }
            set.base = Some(table);
        } else {
            set.parts.push(table);
        }
    }
    if set.parts.is_empty() {
        return Err(format!("parts/{family}: no part declares kind = \"device\""));
    }
    set.parts.sort_by(|a, b| a.part.cmp(&b.part));
    Ok(set)
}

/// Merges a named-section list: the base's rows in base order, each REPLACED WHOLE by a
/// member's row of the same name, and the member's new rows appended in member order.
///
/// Whole replacement rather than a per-key merge is the load-bearing choice. Two members of one
/// family can give the same register's codes different meanings -- the BMx280 standby codes
/// '110'/'111' are 2 s / 4 s on one member and 10 ms / 20 ms on the other -- and a half-inherited
/// encoding is silently wrong for one of them.
fn merge_named<T: Clone>(base: &[T], member: &[T], name: impl Fn(&T) -> String) -> Vec<T> {
    let mut out: Vec<T> = base.to_vec();
    for row in member {
        match out.iter().position(|existing| name(existing) == name(row)) {
            Some(at) => out[at] = row.clone(),
            None => out.push(row.clone()),
        }
    }
    out
}

/// Resolves a part against its family base and returns a FULLY FLATTENED table.
///
/// Inheritance is an authoring convenience and stops here: nothing downstream ever reads an
/// inherited value, because a value that is only ever inherited is pinned in no emitted artifact
/// and so cannot be checked for drift between languages.
///
/// Every section merges by whole named row (see [`merge_named`]) -- except `[identity]`, which
/// merges per key, because it is the one section the schema deliberately SPLITS: the base states
/// where to look and each member states what to accept there.
pub fn resolve_device(set: &DeviceSet, part: &DeviceTable) -> Result<DeviceTable, String> {
    if let Some(sourcing) = part.sourcing.as_ref().filter(|s| s.facts == "secondary") {
        if !sourcing.derived_from.is_empty() {
            if sourcing.derived_from == part.part {
                return Err(format!(
                    "part '{}': [sourcing] derived_from names the part itself, which states nothing about where its facts came from",
                    part.part
                ));
            }
            if !set.parts.iter().any(|p| p.part == sourcing.derived_from) {
                return Err(format!(
                    "part '{}': [sourcing] derived_from names '{}', which is not a part of family '{}'",
                    part.part, sourcing.derived_from, set.family
                ));
            }
        }
    }
    let base = match part.base.as_str() {
        "" => None,
        named => {
            let base = set.base.as_ref().filter(|b| b.family == named).ok_or_else(|| {
                format!(
                    "part '{}' names base '{named}', which is not this family's `kind = \"device-base\"` table",
                    part.part
                )
            })?;
            Some(base)
        }
    };
    let Some(base) = base else { return Ok(part.clone()) };

    let mut identity = base.identity.clone().unwrap_or_default();
    if let Some(member) = &part.identity {
        if member.reg.value != 0 || member.width != 0 {
            identity.reg = member.reg;
            identity.width = member.width;
        }
        if !member.values.is_empty() {
            identity.values = member.values.clone();
        }
    }

    Ok(DeviceTable {
        family: part.family.clone(),
        part: part.part.clone(),
        base: part.base.clone(),
        is_base: false,
        buses: merge_named(&base.buses, &part.buses, |b| b.name.clone()),
        address: part.address.clone().or_else(|| base.address.clone()),
        identity: Some(identity),
        sourcing: part.sourcing.clone(),
        registers: merge_named(&base.registers, &part.registers, |r| r.name.clone()),
        enums: merge_named(&base.enums, &part.enums, |e| e.name.clone()),
        burst_length: if part.burst_length >= 0 { part.burst_length } else { base.burst_length },
        calibrations: merge_named(&base.calibrations, &part.calibrations, |c| c.name.clone()),
        sequences: merge_named(&base.sequences, &part.sequences, |(n, _)| n.clone()),
    })
}

/// Checks a FLATTENED part: what a driver would otherwise discover only when the part failed to
/// answer, and what a reference from one section to another would otherwise emit as a dangling
/// name that compiles.
fn validate_device(part: &DeviceTable) -> Result<(), String> {
    let who = format!("part '{}'", part.part);
    let sourcing = part.sourcing.as_ref().ok_or_else(|| {
        format!(
            "{who} states no [sourcing] -- every part declares where its facts came from and how far it has been validated, so a catalogue can be ranked rather than trusted whole"
        )
    })?;
    if !SOURCING_FACTS.contains(&sourcing.facts.as_str()) {
        return Err(format!(
            "{who}: [sourcing] facts is '{}', which is not one of {}",
            sourcing.facts,
            SOURCING_FACTS.join(", ")
        ));
    }
    if !SOURCING_VALIDATION.contains(&sourcing.validation.as_str()) {
        return Err(format!(
            "{who}: [sourcing] validation is '{}', which is not one of {}",
            sourcing.validation,
            SOURCING_VALIDATION.join(", ")
        ));
    }
    if sourcing.facts == "secondary" && sourcing.derived_from.is_empty() {
        return Err(format!(
            "{who}: [sourcing] facts is 'secondary' but names no derived_from -- a second-hand fact must name the part whose document carries it"
        ));
    }
    if sourcing.facts == "primary" && !sourcing.derived_from.is_empty() {
        return Err(format!(
            "{who}: [sourcing] facts is 'primary' and also names derived_from '{}' -- a part read from its own datasheet derives from nothing",
            sourcing.derived_from
        ));
    }
    if sourcing.validation != "none" && sourcing.evidence.is_empty() {
        return Err(format!(
            "{who}: [sourcing] validation is '{}' but no evidence is stated -- a validation rank names what the part was observed to do",
            sourcing.validation
        ));
    }
    if sourcing.validation == "none" && !sourcing.evidence.is_empty() {
        return Err(format!(
            "{who}: [sourcing] validation is 'none' but evidence is stated -- evidence that earns no rank reads as a validation the table is not claiming"
        ));
    }
    let identity = part
        .identity
        .as_ref()
        .ok_or_else(|| format!("{who} states no [identity] -- it is what tells two parts sharing a footprint apart"))?;
    if identity.absent.is_empty() {
        if identity.width == 0 {
            return Err(format!("{who}: the identity register declares no width"));
        }
        if identity.values.is_empty() {
            return Err(format!(
                "{who}: the identity accepts no value -- a part states the SET its identity register may answer, and a part rejected for answering an unlisted value reads as no part at all"
            ));
        }
    } else {
        if identity.reg.value != 0 || identity.width != 0 || !identity.values.is_empty() {
            return Err(format!(
                "{who}: [identity] states 'absent' AND a register, width or value -- a part either has an identity or it does not, and stating both leaves a consumer to pick"
            ));
        }
        if sourcing.validation == "identified" {
            return Err(format!(
                "{who}: [sourcing] validation is 'identified' while [identity] states the part has none -- there is no register to answer, so nothing could have earned that rung"
            ));
        }
    }
    if let Some(mapped) = part.registers.iter().find(|r| r.reg.value == identity.reg.value) {
        if mapped.width != identity.width {
            return Err(format!(
                "{who}: [identity] reads register 0x{:X} as {} bits and [registers.{}] declares it {} bits",
                identity.reg.value, identity.width, mapped.name, mapped.width
            ));
        }
    }
    if part.buses.is_empty() {
        return Err(format!("{who} declares no bus -- there is no way to reach it"));
    }
    if let Some(address) = &part.address {
        if part.bus(&address.bus).is_none() {
            return Err(format!("{who}: [address] rides bus '{}', which the part does not declare", address.bus));
        }
    }
    for record in &part.calibrations {
        if record.form.is_empty() {
            return Err(format!("{who}: calibration '{}' names no form", record.name));
        }
        for depends in &record.depends_on {
            if !part.calibrations.iter().any(|c| &c.name == depends) {
                return Err(format!(
                    "{who}: calibration '{}' depends on '{depends}', which the part does not describe",
                    record.name
                ));
            }
        }
        for read in &record.reads {
            if read.width == 0 {
                return Err(format!("{who}: calibration read '{}' declares no width", read.name));
            }
        }
    }
    for (sequence, steps) in &part.sequences {
        for (at, step) in steps.iter().enumerate() {
            if step.step.is_empty() {
                return Err(format!("{who}: sequence '{sequence}' step {at} names no step"));
            }
            let target = if step.register.is_empty() { &step.from } else { &step.register };
            if target.is_empty() {
                return Err(format!("{who}: sequence '{sequence}' step {at} names no register"));
            }
            let Some(register) = part.register(target) else {
                return Err(format!(
                    "{who}: sequence '{sequence}' step {at} names register '{target}', which the part does not declare"
                ));
            };
            if !step.field.is_empty() && !register.fields.iter().any(|f| f.name == step.field) {
                return Err(format!(
                    "{who}: sequence '{sequence}' step {at} names field '{}.{}', which the register does not declare",
                    target, step.field
                ));
            }
            if !step.length_from.is_empty() && device_length(part, &step.length_from).is_none() {
                return Err(format!(
                    "{who}: sequence '{sequence}' step {at} takes its length from '{}', which resolves to nothing",
                    step.length_from
                ));
            }
        }
    }
    Ok(())
}

/// Resolves a step's `length_from` reference against the flattened table. The reference exists
/// so a base can state WHERE a burst length comes from while each member states the value; it
/// is resolved here so no emitted artifact carries a name a reader has to chase.
fn device_length(part: &DeviceTable, reference: &str) -> Option<i64> {
    match reference {
        "measurement.burst_length" if part.burst_length >= 0 => Some(part.burst_length),
        _ => None,
    }
}

/// The generated class name for a part: `Bme280Part`. The `Part` suffix names the stratum the
/// way `Layout` / `Instances` / `Bindings` name theirs, and keeps the generated table from ever
/// colliding with a hand-written driver of the part's own name.
#[must_use]
pub fn part_class(part: &str) -> String {
    format!("{}Part", pascal(part))
}

/// One line of a part's emission. The four emitters render this ONE ordered list rather than
/// each composing its own, so a part's names and values are identical across languages by
/// construction and not by four code paths agreeing.
enum Row {
    /// A section comment (each language spells its own marker).
    Comment(String),
    /// A blank separator line.
    Blank,
    /// An unsigned integer constant: name, and the value EXACTLY as spelled.
    Uint(String, String),
    /// A signed integer constant.
    Int(String, String),
    /// A string constant -- a named dispatch, an access, or a provenance name.
    Str(String, String),
}

fn uint(rows: &mut Vec<Row>, name: String, value: String) {
    rows.push(Row::Uint(name, value));
}

fn text(rows: &mut Vec<Row>, name: String, value: &str) {
    rows.push(Row::Str(name, value.to_string()));
}

fn number(rows: &mut Vec<Row>, name: String, value: Int) {
    if value.value < 0 {
        rows.push(Row::Int(name, format_int(value)));
    } else {
        rows.push(Row::Uint(name, format_int(value)));
    }
}

fn section(rows: &mut Vec<Row>, comment: String) {
    rows.push(Row::Blank);
    rows.push(Row::Comment(comment));
}

/// Builds the flattened part's emission rows, in one order every language follows.
fn device_rows(part: &DeviceTable) -> Result<Vec<Row>, String> {
    validate_device(part)?;
    let mut rows = Vec::new();
    text(&mut rows, "PART".to_string(), &part.part);
    text(&mut rows, "FAMILY".to_string(), &part.family);

    let sourcing = part.sourcing.as_ref().expect("validated present");
    section(
        &mut rows,
        "sourcing: how far this part is established, on two INDEPENDENT axes -- a part can be \
         strong on one and absent on the other, which is why they are not one rank. FACTS is \
         `primary` (read from the part's own datasheet) or `secondary` (a primary vendor statement \
         about this part carried by another document, named by DERIVED_FROM). VALIDATION is `none` \
         (no physical part of this type has been made to answer), `identified` (one answered its \
         identity register, against a negative control that tells the part from the wire) or \
         `exercised` (one produced measurements a driver decoded). Anything above `none` states \
         what was observed in EVIDENCE."
            .to_string(),
    );
    text(&mut rows, "SOURCING_FACTS".to_string(), &sourcing.facts);
    if !sourcing.derived_from.is_empty() {
        text(&mut rows, "SOURCING_DERIVED_FROM".to_string(), &sourcing.derived_from);
    }
    text(&mut rows, "SOURCING_VALIDATION".to_string(), &sourcing.validation);
    if !sourcing.evidence.is_empty() {
        text(&mut rows, "SOURCING_EVIDENCE".to_string(), &sourcing.evidence);
    }

    let identity = part.identity.as_ref().expect("validated present");
    if !identity.absent.is_empty() {
        section(
            &mut rows,
            "identity: THIS PART HAS NONE, and that is a fact about the part rather than a gap in \
             this table. There is no register to read, so nothing can confirm which part answered \
             an address -- the `identified` validation rung is unreachable for it by construction, \
             and confidence has to be earned by exercising it instead."
                .to_string(),
        );
        text(&mut rows, "IDENTITY_ABSENT".to_string(), &identity.absent);
    } else {
        section(
            &mut rows,
            "identity: the accepted values are a SET. A driver that accepts only one of them rejects a genuine part, and a rejected part reads as no part at all -- so on a mismatch, name the id received AND this set.".to_string(),
        );
        number(&mut rows, "IDENTITY_REG".to_string(), identity.reg);
        uint(&mut rows, "IDENTITY_WIDTH".to_string(), identity.width.to_string());
        uint(&mut rows, "IDENTITY_VALUE_COUNT".to_string(), identity.values.len().to_string());
    }
    for (at, value) in identity.values.iter().enumerate() {
        number(&mut rows, format!("IDENTITY_VALUE_{at}"), *value);
    }

    if let Some(address) = &part.address {
        section(
            &mut rows,
            "address: a base plus which pin contributes which bit. The part states the range; a carrier fixes the straps, so no strap is defaulted here.".to_string(),
        );
        text(&mut rows, "ADDRESS_BUS".to_string(), &address.bus);
        number(&mut rows, "ADDRESS_BASE".to_string(), address.base);
        uint(&mut rows, "ADDRESS_STRAP_COUNT".to_string(), address.straps.len().to_string());
        for strap in &address.straps {
            let prefix = format!("ADDRESS_STRAP_{}", upper_snake(&strap.pin));
            uint(&mut rows, format!("{prefix}_BIT"), strap.bit.to_string());
            number(&mut rows, format!("{prefix}_LOW"), strap.low);
            number(&mut rows, format!("{prefix}_HIGH"), strap.high);
        }
    }

    section(
        &mut rows,
        "buses: the register MAP is shared between a part's buses; the ADDRESS TRANSFORM is not, so each states its own as a named dispatch. A path that took the wrong one is wrong by a fixed bit, and on one direction only.".to_string(),
    );
    uint(&mut rows, "BUS_COUNT".to_string(), part.buses.len().to_string());
    for bus in &part.buses {
        let prefix = format!("BUS_{}", upper_snake(&bus.name));
        text(&mut rows, format!("{prefix}_KIND"), &bus.kind);
        text(&mut rows, format!("{prefix}_REGISTER_READ_TRANSFORM"), &bus.register_read_transform);
        text(&mut rows, format!("{prefix}_REGISTER_WRITE_TRANSFORM"), &bus.register_write_transform);
        text(&mut rows, format!("{prefix}_READ_PROTOCOL"), &bus.read_protocol);
        if !bus.modes.is_empty() {
            uint(&mut rows, format!("{prefix}_MODE_COUNT"), bus.modes.len().to_string());
            for (at, mode) in bus.modes.iter().enumerate() {
                number(&mut rows, format!("{prefix}_MODE_{at}"), *mode);
            }
        }
    }

    if !part.registers.is_empty() {
    section(
        &mut rows,
        "registers: `_REG` is the address written on the wire -- an operand, NOT an offset to add to an instance base. Fields carry the shifted mask and its shift.".to_string(),
    );
    for register in &part.registers {
        let prefix = upper_snake(&register.name);
        number(&mut rows, format!("{prefix}_REG"), register.reg);
        uint(&mut rows, format!("{prefix}_WIDTH"), register.width.to_string());
        if !register.access.is_empty() {
            text(&mut rows, format!("{prefix}_ACCESS"), &register.access);
        }
        for field in &register.fields {
            let name = format!("{prefix}_{}", upper_snake(&field.name));
            uint(&mut rows, name.clone(), format!("0x{:X}", field.mask()));
            uint(&mut rows, format!("{name}_LSB"), field.lsb.to_string());
        }
    }
    }

    if !part.enums.is_empty() {
        section(&mut rows, "encodings: the codes a field takes, as this part reads them.".to_string());
        for encoding in &part.enums {
            let prefix = upper_snake(&encoding.name);
            for (member, code) in &encoding.members {
                number(&mut rows, format!("{prefix}_{}", upper_snake(member)), *code);
            }
        }
    }

    if part.burst_length >= 0 {
        section(
            &mut rows,
            "measurement: the part holds a whole data block steady for the duration of one burst, so a byte-at-a-time read can mix two conversions.".to_string(),
        );
        uint(&mut rows, "BURST_LENGTH".to_string(), part.burst_length.to_string());
    }

    for record in &part.calibrations {
        let prefix = upper_snake(&record.name);
        section(
            &mut rows,
            format!(
                "calibration '{}': these parameters are READ from the part and are per-device -- they are not constants and must not be baked into a driver. `_FORM` selects hand-written per-language arithmetic; signedness is not uniform, and reading it backwards yields a plausible wrong answer rather than an error.",
                record.name
            ),
        );
        text(&mut rows, format!("{prefix}_FORM"), &record.form);
        if !record.byte_order.is_empty() {
            text(&mut rows, format!("{prefix}_BYTE_ORDER"), &record.byte_order);
        }
        if !record.output_scale.is_empty() {
            text(&mut rows, format!("{prefix}_OUTPUT_SCALE"), &record.output_scale);
        }
        if !record.depends_on.is_empty() {
            uint(&mut rows, format!("{prefix}_DEPENDS_ON_COUNT"), record.depends_on.len().to_string());
            for (at, depends) in record.depends_on.iter().enumerate() {
                text(&mut rows, format!("{prefix}_DEPENDS_ON_{at}"), depends);
            }
        }
        uint(&mut rows, format!("{prefix}_READ_COUNT"), record.reads.len().to_string());
        for read in &record.reads {
            let name = format!("{prefix}_{}", upper_snake(&read.name));
            number(&mut rows, format!("{name}_REG"), read.reg);
            uint(&mut rows, format!("{name}_WIDTH"), read.width.to_string());
            uint(&mut rows, format!("{name}_SIGNED"), u32::from(read.signed).to_string());
            if !read.packing.is_empty() {
                text(&mut rows, format!("{name}_PACKING"), &read.packing);
            }
        }
    }

    for (name, steps) in &part.sequences {
        let prefix = upper_snake(name);
        section(
            &mut rows,
            format!(
                "sequence '{name}': declarative steps, resolved here so a step carries its own register and mask. The description is transport agnostic -- one step list describes an I2C transaction, an SPI one, and a call through a host import alike."
            ),
        );
        uint(&mut rows, format!("{prefix}_STEP_COUNT"), steps.len().to_string());
        for (at, step) in steps.iter().enumerate() {
            let name = format!("{prefix}_STEP_{at}");
            text(&mut rows, format!("{name}_OP"), &step.step);
            let target = if step.register.is_empty() { &step.from } else { &step.register };
            let register = part.register(target).expect("validated present");
            if step.field.is_empty() {
                text(&mut rows, format!("{name}_TARGET"), target);
            } else {
                text(&mut rows, format!("{name}_TARGET"), &format!("{target}.{}", step.field));
            }
            number(&mut rows, format!("{name}_REG"), register.reg);
            if let Some(field) = register.fields.iter().find(|f| f.name == step.field) {
                uint(&mut rows, format!("{name}_MASK"), format!("0x{:X}", field.mask()));
                uint(&mut rows, format!("{name}_LSB"), field.lsb.to_string());
            }
            if let Some(value) = step.value {
                number(&mut rows, format!("{name}_VALUE"), value);
            }
            if !step.length_from.is_empty() {
                let length = device_length(part, &step.length_from).expect("validated resolvable");
                uint(&mut rows, format!("{name}_LENGTH"), length.to_string());
            }
            uint(&mut rows, format!("{name}_BOUNDED"), u32::from(step.bounded).to_string());
        }
    }

    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let name = match row {
            Row::Uint(name, _) | Row::Int(name, _) | Row::Str(name, _) => name,
            _ => continue,
        };
        if !seen.insert(name.clone()) {
            return Err(format!("part '{}': two facts emit the constant '{name}'", part.part));
        }
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!(
                "part '{}': the fact '{name}' does not spell an identifier -- a part's key becomes a constant name in every emitted language",
                part.part
            ));
        }
    }
    for row in &rows {
        if let Row::Str(name, value) = row {
            if value.contains('"') || value.contains('\\') {
                return Err(format!(
                    "part '{}': the value of '{name}' carries a quote or a backslash, which no emitted string literal can hold",
                    part.part
                ));
            }
        }
    }
    Ok(rows)
}

/// The generated class name for a family's INVARIANT facts: `Bmx280Common`.
#[must_use]
pub fn common_class(family: &str) -> String {
    format!("{}Common", pascal(family))
}

/// The rows every member of a family emits IDENTICALLY -- same name, same kind, same value.
///
/// Why this exists, and why it is an intersection rather than the authoring base emitted. A part
/// FAMILY has a hand-written driver base in every language (the parts share a register map, so
/// their drivers share code), and a per-part-only emission gives that base nothing to reference:
/// it would have to name one member's class arbitrarily, or reach every fact through a dozen
/// virtual properties.
///
/// Emitting the authoring base would be the wrong fix, because a member may OVERRIDE a shared
/// register and a base that carried the pre-override value would be silently wrong for exactly
/// that member. An intersection cannot be: the day a member overrides a fact, that fact LEAVES
/// this class, and the shared driver base that used it stops compiling -- which is the failure
/// that is wanted, at the moment it becomes true.
///
/// The section comments of the first member are carried only where a fact under them survived.
fn common_rows(parts: &[DeviceTable]) -> Result<Vec<Row>, String> {
    let per_part: Vec<Vec<Row>> = parts.iter().map(device_rows).collect::<Result<_, _>>()?;
    let Some((first, rest)) = per_part.split_first() else { return Ok(Vec::new()) };

    let key = |row: &Row| {
        let ranked = |name: &String| !name.starts_with("SOURCING_");
        match row {
            Row::Uint(name, value) if ranked(name) => Some((name.clone(), format!("u{value}"))),
            Row::Int(name, value) if ranked(name) => Some((name.clone(), format!("i{value}"))),
            Row::Str(name, value) if ranked(name) => Some((name.clone(), format!("s{value}"))),
            _ => None,
        }
    };
    let shared: std::collections::HashSet<(String, String)> = rest
        .iter()
        .map(|rows| rows.iter().filter_map(key).collect::<std::collections::HashSet<_>>())
        .fold(first.iter().filter_map(key).collect(), |acc, next| {
            acc.intersection(&next).cloned().collect()
        });

    let mut out: Vec<Row> = Vec::new();
    let mut pending: Vec<Row> = Vec::new();
    for row in first {
        match row {
            Row::Blank | Row::Comment(_) => pending.push(match row {
                Row::Comment(text) => Row::Comment(text.clone()),
                _ => Row::Blank,
            }),
            _ => {
                if key(row).is_some_and(|k| shared.contains(&k)) {
                    out.append(&mut pending);
                    out.push(match row {
                        Row::Uint(n, v) => Row::Uint(n.clone(), v.clone()),
                        Row::Int(n, v) => Row::Int(n.clone(), v.clone()),
                        Row::Str(n, v) => Row::Str(n.clone(), v.clone()),
                        _ => unreachable!("matched a constant row"),
                    });
                } else {
                    pending.clear();
                }
            }
        }
    }
    Ok(out)
}

/// What a family's invariant emission says it is.
fn common_what(family: &str, parts: &[DeviceTable]) -> String {
    let members: Vec<&str> = parts.iter().map(|p| p.part.as_str()).collect();
    format!(
        "The facts every {family} member emits IDENTICALLY -- the family's INVARIANT subset,\n\
         // computed across {}. A part family's driver base is shared code in every language, and\n\
         // this is what it may rely on. A fact is here only while EVERY member agrees on it, so the\n\
         // day one member states its own value the fact LEAVES this class and the shared code that\n\
         // used it stops compiling. Nothing here is inherited: each member's own table spells all\n\
         // of it too, flattened.",
        members.join(", "),
    )
}

/// What every language's header says a part emission is. Written with the C#/Rust/Swift comment
/// marker; the Python emitter rewrites the continuations to its own.
fn device_what(part: &DeviceTable) -> String {
    format!(
        "The {} part table, FLATTENED: every value it inherited is spelled out here, because a\n\
         // value that is only ever inherited is pinned in no emitted artifact and so cannot be\n\
         // checked for drift between languages. A part is something we talk TO rather than a chip\n\
         // we run on -- a board states which buses exist, this table states the part's identity,\n\
         // and a probe joins the two at run time.",
        part.part,
    )
}

/// Renders a row list as a C# facts class.
fn render_csharp(
    class: &str,
    what: &str,
    rows: &[Row],
    sources: &[String],
    regen: &str,
) -> Result<String, String> {
    let mut out = String::new();
    emit_header(&mut out, class, what, sources, regen);
    let rows_at = rows;
    for (at, row) in rows_at.iter().enumerate() {
        let documents = documents_something(&rows_at[at + 1..]);
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "        ", if documents { "///" } else { "//" }, comment),
            Row::Uint(name, value) => push_const(&mut out, "uint", name, value),
            Row::Int(name, value) => push_const(&mut out, "int", name, value),
            Row::Str(name, value) => push_const(&mut out, "string", name, &format!("\"{value}\"")),
        }
    }
    finish_class(&mut out)?;
    Ok(out)
}

/// Renders a row list as a Rust `pub const` module, name/value-identical to the C#.
fn render_rust(what: &str, rows: &[Row], sources: &[String], regen: &str) -> Result<String, String> {
    let mut out = String::new();
    emit_rust_header(&mut out, what, sources, regen);
    let rows_at = rows;
    for (at, row) in rows_at.iter().enumerate() {
        let documents = documents_something(&rows_at[at + 1..]);
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "", if documents { "///" } else { "//" }, comment),
            Row::Uint(name, value) => push_rust_const(&mut out, "u32", name, value),
            Row::Int(name, value) => push_rust_const(&mut out, "i32", name, value),
            Row::Str(name, value) => push_rust_const(&mut out, "&str", name, &format!("\"{value}\"")),
        }
    }
    finish_rust(&out)?;
    Ok(out)
}

/// Renders a row list as a Swift caseless-enum namespace, name/value-identical to the C#. A
/// named dispatch rides `StaticString`, which a Swift image with no heap can hold.
fn render_swift(
    class: &str,
    what: &str,
    rows: &[Row],
    sources: &[String],
    regen: &str,
) -> Result<String, String> {
    let mut out = String::new();
    emit_swift_header(&mut out, what, sources, regen);
    out.push_str(&format!("\npublic enum {class} {{\n"));
    let rows_at = rows;
    for row in rows_at.iter() {
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "    ", "//", comment),
            Row::Uint(name, value) => push_swift_const(&mut out, "UInt32", name, value),
            Row::Int(name, value) => push_swift_const(&mut out, "Int32", name, value),
            Row::Str(name, value) => push_swift_const(&mut out, "StaticString", name, &format!("\"{value}\"")),
        }
    }
    out.push_str("}\n");
    finish_swift(&out)?;
    Ok(out)
}

/// Renders a row list as a Python module of the SAME names and value spellings. A part has no
/// roles, so unlike a board's `board.py` this is flat -- one truth, one shape.
fn render_python(what: &str, rows: &[Row], sources: &[String], regen: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# GENERATED by lamella-bsp-gen from {list} -- DO NOT EDIT.\n# Regenerate: {regen}\n#\n# {what}\n",
        list = sources.join(" + "),
        what = what.replace("\n//", "\n#"),
    ));
    let rows_at = rows;
    for row in rows_at.iter() {
        match row {
            Row::Blank => out.push('\n'),
            Row::Comment(comment) => push_section_comment(&mut out, "", "#", comment),
            Row::Uint(name, value) | Row::Int(name, value) => {
                out.push_str(&format!("{name} = {value}\n"));
            }
            Row::Str(name, value) => out.push_str(&format!("{name} = \"{value}\"\n")),
        }
    }
    out
}

/// Emits the flattened part table as a C# facts class.
pub fn emit_part_csharp(part: &DeviceTable, sources: &[String], regen: &str) -> Result<String, String> {
    render_csharp(&part_class(&part.part), &device_what(part), &device_rows(part)?, sources, regen)
}

/// Emits the flattened part table as a Rust `pub const` module.
pub fn emit_part_rust(part: &DeviceTable, sources: &[String], regen: &str) -> Result<String, String> {
    render_rust(&device_what(part), &device_rows(part)?, sources, regen)
}

/// Emits the flattened part table as a Swift caseless-enum namespace.
pub fn emit_part_swift(part: &DeviceTable, sources: &[String], regen: &str) -> Result<String, String> {
    render_swift(&part_class(&part.part), &device_what(part), &device_rows(part)?, sources, regen)
}

/// Emits the flattened part table as a Python module.
pub fn emit_part_python(part: &DeviceTable, sources: &[String], regen: &str) -> Result<String, String> {
    Ok(render_python(&device_what(part), &device_rows(part)?, sources, regen))
}

/// Generates every artifact of a part family: per part, the flattened table in all four
/// languages boards emit today. Every part emits every language deliberately -- gate B's whole
/// value is that a concrete value cannot drift between languages, which only holds for a
/// language the value is actually spelled in.
pub fn generate_parts(repo_root: &std::path::Path, family: &str) -> Result<Vec<Generated>, String> {
    let regen = format!("cargo run -p lamella-bsp-gen -- gen-parts . {family}");
    let set = load_device_family(repo_root, family)?;
    let mut out = Vec::new();
    let mut resolved_parts = Vec::new();
    for part in &set.parts {
        let resolved = resolve_device(&set, part)?;
        let mut sources = vec![format!("parts/{family}/{}.toml", part.part)];
        if !part.base.is_empty() {
            sources.push(format!("parts/{family}/{}.toml", part.base));
        }
        let id = &resolved.part;
        out.push(Generated {
            path: format!("parts/{family}/csharp/{}.g.cs", part_class(id)),
            contents: emit_part_csharp(&resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("parts/{family}/rust/{}_part.rs", snake(id)),
            contents: emit_part_rust(&resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("parts/{family}/swift/{}.swift", part_class(id)),
            contents: emit_part_swift(&resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("parts/{family}/python/{}.py", snake(id)),
            contents: emit_part_python(&resolved, &sources, &regen)?,
        });
        resolved_parts.push(resolved);
    }

    if resolved_parts.len() >= 2 {
        let rows = common_rows(&resolved_parts)?;
        let what = common_what(family, &resolved_parts);
        let mut sources: Vec<String> =
            resolved_parts.iter().map(|p| format!("parts/{family}/{}.toml", p.part)).collect();
        if let Some(base) = &set.base {
            sources.push(format!("parts/{family}/{}.toml", base.family));
        }
        let class = common_class(family);
        out.push(Generated {
            path: format!("parts/{family}/csharp/{class}.g.cs"),
            contents: render_csharp(&class, &what, &rows, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("parts/{family}/rust/{}_common.rs", snake(family)),
            contents: render_rust(&what, &rows, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("parts/{family}/swift/{class}.swift"),
            contents: render_swift(&class, &what, &rows, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("parts/{family}/python/{}_common.py", snake(family)),
            contents: render_python(&what, &rows, &sources, &regen),
        });
    }
    Ok(out)
}


/// One generated file: its repo-relative path and contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generated {
    /// Repo-relative output path, forward slashes.
    pub path: String,
    /// The emitted file contents.
    pub contents: String,
}

/// Generates every artifact of a family: the C# layout classes, the instances class (C# and
/// its 1:1 Rust twin), and per board under `bsp/*/board.toml` whose family (or module's
/// family) matches, a bindings class (C#) plus its 1:1 Rust twin -- one strata load, every
/// projection, so gate A freshness and the gate B anchors cover all of them together.
pub fn generate_family(repo_root: &std::path::Path, family: &str) -> Result<Vec<Generated>, String> {
    let regen = format!("cargo run -p lamella-bsp-gen -- gen-family . {family}");
    let set = load_family(repo_root, family)?;
    let mut out = Vec::new();

    let swift = SWIFT_FAMILIES.contains(&family);
    for block in &set.blocks {
        let mode = if block.mode.is_empty() { String::new() } else { format!("-{}", block.mode) };
        let source = format!("csp/{family}/blocks/{}{mode}.toml", block.block);
        out.push(Generated {
            path: format!("csp/{family}/csharp/{}.g.cs", layout_class(block)),
            contents: emit_layout_csharp(block, &source, &regen)?,
        });
        out.push(Generated {
            path: format!("csp/{family}/rust/{}.rs", layout_module(block)),
            contents: emit_layout_rust(block, &source, &regen)?,
        });
        if swift {
            out.push(Generated {
                path: format!("csp/{family}/swift/{}.swift", layout_class(block)),
                contents: emit_layout_swift(block, &source, &regen)?,
            });
        }
    }
    out.push(Generated {
        path: format!("csp/{family}/csharp/{}.g.cs", instances_class(family)),
        contents: emit_instances_csharp(&set.instances, &format!("csp/{family}/instances.toml"), &regen)?,
    });
    out.push(Generated {
        path: format!("csp/{family}/rust/{}_instances.rs", snake(family)),
        contents: emit_instances_rust(&set.instances, &format!("csp/{family}/instances.toml"), &regen)?,
    });
    if swift {
        out.push(Generated {
            path: format!("csp/{family}/swift/{}.swift", instances_class(family)),
            contents: emit_instances_swift(&set.instances, &format!("csp/{family}/instances.toml"), &regen)?,
        });
    }

    let bsp_root = repo_root.join("bsp");
    let mut board_paths: Vec<_> = std::fs::read_dir(&bsp_root)
        .map_err(|e| format!("{}: {e}", bsp_root.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("board.toml"))
        .filter(|p| p.is_file())
        .collect();
    board_paths.sort();
    for path in board_paths {
        let Strata::Board(board) = parse(&read(&path)?)? else {
            return Err(format!("{}: expected kind = \"board\"", path.display()));
        };
        let board_family = if board.module.is_empty() {
            board.family.clone()
        } else {
            set.module(&board.module).map(|m| m.family.clone()).unwrap_or_default()
        };
        if board_family != family {
            continue;
        }
        let id = board.board.clone();
        let dir = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| format!("{}: has no board directory", path.display()))?;
        if dir != id && !dir.ends_with(&format!("-{id}")) {
            return Err(format!(
                "bsp/{dir}/board.toml states board = \"{id}\" -- a board directory is its id, or its id behind a vendor prefix"
            ));
        }
        let resolved = resolve_board(&set, board)?;
        let mut sources = vec![format!("bsp/{dir}/board.toml"), format!("csp/{family}/ strata")];
        if !resolved.board.module.is_empty() {
            sources.insert(1, format!("csp/{}/module.toml", resolved.board.module));
        }
        out.push(Generated {
            path: format!("bsp/{dir}/csharp/{}.g.cs", bindings_class(&id)),
            contents: emit_board_csharp(&set, &resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("bsp/{dir}/rust/{}_bindings.rs", snake(&id)),
            contents: emit_board_rust(&set, &resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("bsp/{dir}/python/board.py"),
            contents: emit_board_python(&set, &resolved, &sources, &regen)?,
        });
        if swift {
            out.push(Generated {
                path: format!("bsp/{dir}/swift/{}.swift", bindings_class(&id)),
                contents: emit_board_swift(&set, &resolved, &sources, &regen)?,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = r#"
[table]
kind = "block"
family = "fam"
block = "blk"
mode = "m"
[registers.CTRL]
offset = 0x0
width = 32
fields = { EN = [1, 1], SEL = [4, 2] }
[constants]
MAGIC = 7
[[sequences.go]]
op = "write"
reg = "CTRL"
value = 0x2
"#;

    #[test]
    fn parses_a_block_and_emits_offsets_widths_masks() {
        let Strata::Block(block) = parse(BLOCK).expect("parses") else { panic!("kind") };
        assert_eq!(block.family, "fam");
        assert_eq!(block.mode, "m");
        assert_eq!(block.register("CTRL").unwrap().width, 32);
        let emitted = emit_layout_csharp(&block, "src.toml", "regen").expect("emits");
        assert!(emitted.contains("class FamBlkMLayout"));
        assert!(emitted.contains("public const uint CTRL_OFF = 0x0;"));
        assert!(emitted.contains("public const int CTRL_WIDTH = 32;"));
        assert!(emitted.contains("public const uint CTRL_SEL = 0x30;"));
        assert!(emitted.contains("public const uint CTRL_SEL_LSB = 4;"));
        assert!(emitted.contains("public const uint MAGIC = 7;"));
    }

    #[test]
    fn a_block_register_without_width_refuses() {
        let bad = BLOCK.replace("width = 32\n", "");
        assert!(parse(&bad).unwrap_err().contains("no width"));
    }

    #[test]
    fn a_board_with_a_divisor_key_refuses_v4() {
        let bad = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[[plans]]
name = "n"
default = true
source = "s"
baud_divisor = 0x1234
"#;
        let error = parse(bad).unwrap_err();
        assert!(error.contains("generation derives divisors"), "{error}");
    }

    /// The two carrier spellings mean the same thing: a singular `[carrier]` reads
    /// as a one-row list that is already the default, so the migration sweep can rewrite 15 boards
    /// without moving a single emitted value.
    #[test]
    fn a_singular_carrier_section_reads_as_a_defaulted_one_row_list() {
        let source = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[carrier]
kind = "edbg-vcp"
baud = 115200
[[plans]]
name = "n"
default = true
source = "s"
"#;
        let Strata::Board(board) = parse(source).expect("parses") else { panic!("expected a board") };
        assert_eq!(board.carriers.len(), 1, "the singular section is one row");
        assert!(board.carriers[0].default, "and that row is the default");
        assert_eq!(board.carrier.kind, "edbg-vcp", "the default row is what emitters read");
        assert_eq!(board.carrier.baud, 115200);
    }

    /// A paired list names each wire's operating point, and the DEFAULT row is the one
    /// emitters derive from -- not merely the first one written.
    #[test]
    fn a_carrier_list_pairs_each_wire_to_a_plan_and_names_one_default() {
        let source = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[[carriers]]
kind = "uart"
baud = 115200
plan = "slow"
[[carriers]]
kind = "native-usb"
plan = "fast"
default = true
[[plans]]
name = "slow"
default = true
source = "s"
[[plans]]
name = "fast"
source = "s"
"#;
        let Strata::Board(board) = parse(source).expect("parses") else { panic!("expected a board") };
        assert_eq!(board.carriers.len(), 2);
        assert_eq!(board.carrier.kind, "native-usb", "the DEFAULT row wins, not the first");
        assert_eq!(board.carrier.plan, "fast");
    }

    /// A clock block belongs to an operating point some WIRE runs at, so the plans a board
    /// emits one for are the plans its carriers name -- in table order, deduplicated, and
    /// WITHOUT the baud filter the divisor path applies (a native-usb wire has no rate at all,
    /// and naming an operating point is its entire contribution). A plan nothing names is
    /// declared, not consumed, and emits nothing.
    #[test]
    fn the_plans_a_board_emits_a_clock_block_for_are_the_ones_its_carriers_name() {
        let source = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[[carriers]]
kind = "uart"
role = "uart0"
baud = 115200
plan = "slow"
default = true
[[carriers]]
kind = "native-usb"
plan = "fast"
[[carriers]]
kind = "uart"
role = "uart1"
baud = 9600
plan = "fast"
[[plans]]
name = "slow"
default = true
source = "s"
[[plans]]
name = "fast"
source = "s"
[[plans]]
name = "unused"
source = "s"
"#;
        let Strata::Board(board) = parse(source).expect("parses") else { panic!("expected a board") };
        let names: Vec<&str> = board.carrier_plans().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["slow", "fast"], "table order, deduplicated, declared-but-unnamed dropped");
        let rated_only: Vec<&str> =
            board.carrier_points("uart0").iter().map(|(_, p)| p.name.as_str()).collect();
        assert_eq!(rated_only, ["slow"], "carrier_points still pairs only rated wires to a role");
    }

    /// A divisor belongs to a (carrier, plan) PAIR, so a board whose secondary wire
    /// rides the same binding at a different operating point derives TWO -- each under its own
    /// plan, kept apart by the `<rate>_<PLAN>` suffix every arm already spells. A carrier that
    /// names no plan reads as the board default (the singular-section reading), and one
    /// that rides no binding role contributes nothing.
    #[test]
    fn a_binding_derives_one_divisor_per_carrier_that_rides_it() {
        let source = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[[carriers]]
kind = "native-usb"
plan = "fast"
default = true
[[carriers]]
kind = "uart"
role = "uart0"
baud = 115200
plan = "slow"
[[carriers]]
kind = "probe"
role = "uart0"
baud = 9600
[[plans]]
name = "slow"
source = "s"
[[plans]]
name = "fast"
default = true
source = "s"
"#;
        let Strata::Board(board) = parse(source).expect("parses") else { panic!("expected a board") };
        let points = board.carrier_points("uart0");
        assert_eq!(points.len(), 2, "both wires riding uart0 derive, the native-usb one does not");
        assert_eq!((points[0].0.baud, points[0].1.name.as_str()), (115200, "slow"));
        assert_eq!(
            (points[1].0.baud, points[1].1.name.as_str()),
            (9600, "fast"),
            "a carrier naming no plan reads as the board default"
        );
        assert!(board.carrier_points("spi0").is_empty(), "no carrier rides that role");
    }

    /// "Exactly one default" binds only WHEN a board declares carriers. A bare chip with no
    /// wire at all declares none, and the validator must not demand a default of it.
    #[test]
    fn a_board_declaring_no_carrier_is_not_asked_for_a_default() {
        let source = r#"
[table]
kind = "board"
board = "bare"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[[plans]]
name = "n"
default = true
source = "s"
"#;
        let Strata::Board(board) = parse(source).expect("a carrier-less board parses") else {
            panic!("expected a board")
        };
        assert!(board.carriers.is_empty());
        assert!(board.carrier.kind.is_empty(), "no wire declared, no default invented");
    }

    #[test]
    fn a_board_memory_section_has_a_closed_source_cited_key_set() {
        let good = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1
[memory]
flash = 0x200000
source = "board datasheet"
[[plans]]
name = "n"
default = true
source = "s"
"#;
        let Strata::Board(board) = parse(good).expect("parses") else { panic!("kind") };
        assert_eq!(board.memory.len(), 1);
        assert_eq!(board.memory[0].name, "flash");
        assert_eq!(board.memory[0].kind, "flash");
        assert_eq!(board.memory[0].size, 0x200000);
        assert_eq!(board.memory[0].base, -1, "the chip's own XIP window is not the board's to state");
        assert_eq!(board.xip_flash(), 0x200000);
        let bad_key = good.replace("flash = 0x200000", "ram = 0x1000");
        assert!(parse(&bad_key).unwrap_err().contains("unexpected memory key 'ram'"));
        let uncited = good.replace("source = \"board datasheet\"\n", "");
        assert!(parse(&uncited).unwrap_err().contains("SOURCE-CITED"));
    }

    #[test]
    fn a_board_may_fit_several_memory_regions() {
        let source = r#"
[table]
kind = "board"
board = "b"
vendor = "vend"
family = "fam"
part = "p"
board_model = 1

[[memory]]
name = "qspi"
kind = "flash"
base = 0x90000000
size = 0x1000000
controller = "quadspi"
optional = true
source = "the board user manual"

[[memory]]
name = "sdram"
kind = "ram"
base = 0xC0000000
size = 0x800000
device_size = 0x1000000
controller = "fmc"
optional = true
source = "the board user manual"

[[plans]]
name = "n"
default = true
source = "s"
"#;
        let Strata::Board(board) = parse(source).expect("parses") else { panic!("kind") };
        assert_eq!(board.memory.len(), 2);
        assert_eq!(board.memory[0].base, 0x9000_0000);
        assert_eq!(board.memory[0].controller, "quadspi");
        assert!(board.memory[0].optional);
        assert_eq!(board.xip_flash(), 0, "an externally-controlled flash is not the XIP window");
        assert_eq!(board.memory[1].size, 0x80_0000);
        assert_eq!(board.memory[1].device_size, 0x100_0000);
        assert_eq!(board.memory[1].fitted_size(), 0x100_0000);
        assert_eq!(board.memory[0].fitted_size(), 0x100_0000, "unstated device_size means the same");

        let widened = source.replace("size = 0x800000\ndevice_size = 0x1000000", "size = 0x1000000\ndevice_size = 0x800000");
        assert!(parse(&widened).unwrap_err().contains("cannot exceed the part"));
        let unkinded = source.replace("kind = \"ram\"", "kind = \"eeprom\"");
        assert!(parse(&unkinded).unwrap_err().contains("holds code or it holds data"));
        let unnamed = source.replace("name = \"sdram\"\n", "");
        assert!(parse(&unnamed).unwrap_err().contains("states no name"));
        let collided = source.replace("name = \"sdram\"", "name = \"qspi\"");
        assert!(parse(&collided).unwrap_err().contains("two memory regions are named"));
    }

    #[test]
    fn instance_rows_enforce_the_record_v6() {
        let bad = r#"
[table]
kind = "instances"
family = "fam"
record = ["base", "irq"]
[[instances]]
name = "x0"
block = "blk"
base = 0x1000
"#;
        let error = parse(bad).unwrap_err();
        assert!(error.contains("misses record field 'irq'"), "{error}");
    }

    #[test]
    fn part_pin_ranges_expand() {
        let row = PartRow {
            part: "p".into(),
            package: String::new(),
            flash: 0,
            ram: 0,
            pins: vec!["PA12-PA15".into(), "PB09".into()],
            ..Default::default()
        };
        assert!(row.has_pin("PA13"));
        assert!(row.has_pin("PB09"));
        assert!(!row.has_pin("PA16"));
        assert!(!row.has_pin("PB10"));
    }

    /// A pin-map row naming a pin no part of the family carries. THE DEFECT: the present-list and
    /// the pin map are two statements about the same silicon, and until this check existed only
    /// pins a BINDING reached were held equal -- so a bus with thirty-eight muxed cells and no
    /// binding could name a pin the part does not have, and every gate would stay green.
    #[test]
    fn a_pin_map_row_outside_every_part_present_list_is_refused() {
        let mut set = FamilySet {
            family: "fam".into(),
            instances: InstancesTable {
                family: "fam".into(),
                record: vec!["base".into()],
                rows: vec![InstanceRow {
                    name: "fmc".into(),
                    block: "fmc".into(),
                    values: vec![0xA000_0000], port: String::new() }],
                ..Default::default()
            },
            parts: PartsTable {
                family: "fam".into(),
                rows: vec![PartRow {
                    part: "p".into(),
                    package: "LQFP144".into(),
                    flash: 0,
                    ram: 0,
                    pins: vec!["PF0-PF5".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let row = |pin: &str| PinRow {
            pin: pin.into(),
            function: "AF12".into(),
            instance: "fmc".into(),
            signal: "a0".into(),
            source: "a datasheet".into(),
        };

        set.pins = PinsTable { family: "fam".into(), rows: vec![row("PF3")], ..Default::default() };
        assert!(validate_family(&set).is_ok(), "a pin inside the present-list is fine");

        set.pins = PinsTable { family: "fam".into(), rows: vec![row("PF9")], ..Default::default() };
        let error = validate_family(&set).unwrap_err();
        assert!(error.contains("'PF9' is in no part's present-list"), "{error}");

        set.parts.rows.clear();
        assert!(validate_family(&set).is_ok(), "a family with no parts row states no present-list");
    }

    /// A board's WIRED CONTROL LINE naming a pin its part does not carry -- the third list stating
    /// pins about a part, and the last one nothing checked.
    ///
    /// THE DEFECT, AND ITS SHAPE IS THE SAME ONE TWICE ALREADY: a binding's pins were held to the
    /// present-list from the start, a pin-map row's joined them later, and an LED or a button row
    /// was neither. It is not a harmless omission, because a control line EMITS: an LED row naming
    /// a pin the part does not have produces a real port base and a real mask, so the firmware
    /// writes a live register belonging to some other cell and the board simply does nothing.
    ///
    /// Measured when this check landed: eight control lines across three rp2040 boards named pins
    /// that family's parts row did not carry, and every gate was green.
    #[test]
    fn a_board_control_line_outside_its_parts_present_list_is_refused() {
        let set = FamilySet {
            family: "fam".into(),
            parts: PartsTable {
                family: "fam".into(),
                rows: vec![PartRow {
                    part: "p".into(),
                    package: "LQFP144".into(),
                    pins: vec!["PB0".into(), "PB7".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let board = |pin: &str| BoardTable {
            board: "b".into(),
            family: "fam".into(),
            part: "p".into(),
            devices: vec![ControlPin {
                name: "led0".into(),
                kind: "gpio-out".into(),
                pin: pin.into(),
                active: "high".into(),
                address: -1,
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(resolve_board(&set, board("PB7")).is_ok(), "a pin inside the present-list is fine");

        let error = resolve_board(&set, board("PA3")).unwrap_err();
        assert!(error.contains("device 'led0' is wired to PA3"), "{error}");
        assert!(error.contains("not in part p's pin list"), "{error}");

        let error = resolve_board(&set, board("PB3")).unwrap_err();
        assert!(error.contains("device 'led0' is wired to PB3"), "{error}");
    }

    /// A board wiring a control line to a pin the PART RESERVES for something inside the package.
    ///
    /// THE DEFECT IS THAT EVERY EXISTING CHECK PASSES IT. A reserved pin IS in the present-list
    /// -- the part carries it -- so the check above says yes. A control line needs no pin-map row,
    /// so the unrouted shape is never consulted. Generation then composes a real port base and a
    /// real mask, the firmware writes them, and the silicon ignores the write because the pin
    /// belongs to an integrated peripheral. The board does nothing, which looks exactly like a
    /// board with no LED.
    ///
    #[test]
    fn a_reserved_pin_is_refused_to_a_board_and_to_a_binding() {
        let part = |reserved: Vec<(String, String)>| PartRow {
            part: "p".into(),
            package: "QFN48".into(),
            pins: vec!["PA10".into(), "PB7".into()],
            reserved,
            ..Default::default()
        };
        let set = |row: PartRow| FamilySet {
            family: "fam".into(),
            parts: PartsTable { family: "fam".into(), rows: vec![row], ..Default::default() },
            ..Default::default()
        };
        let board = |pin: &str| BoardTable {
            board: "b".into(),
            family: "fam".into(),
            part: "p".into(),
            devices: vec![ControlPin {
                name: "led0".into(),
                kind: "gpio-out".into(),
                pin: pin.into(),
                active: "high".into(),
                address: -1,
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(
            resolve_board(&set(part(Vec::new())), board("PA10")).is_ok(),
            "an unreserved pin in the present-list is fine -- without this the test below proves nothing"
        );

        let reserved = vec![("PA10".to_string(), "the integrated radio's RFCTRL".to_string())];
        let error = resolve_board(&set(part(reserved.clone())), board("PA10")).unwrap_err();
        assert!(error.contains("device 'led0' is wired to PA10"), "{error}");
        assert!(error.contains("RESERVES for the integrated radio's RFCTRL"), "{error}");

        let mut with_binding = set(part(reserved));
        with_binding.instances = InstancesTable {
            family: "fam".into(),
            record: vec!["base".into()],
            rows: vec![InstanceRow {
                name: "sercom4".into(),
                block: "sercom".into(),
                values: vec![0x4200_0000],
                port: String::new(),
            }],
            ..Default::default()
        };
        let binding = Binding {
            role: "radio".into(),
            kind: "spi".into(),
            instance: "sercom4".into(),
            function: "F".into(),
            pins: vec![("mosi".to_string(), PinRef { pin: "PA10".into(), ..Default::default() })],
            gclk_gen: -1,
            reference_uv: -1,
        };
        let error = validate_bindings(&with_binding, &[binding], "p", "b").unwrap_err();
        assert!(error.contains("claims pin PA10"), "{error}");
        assert!(error.contains("RESERVES"), "{error}");
    }

    /// A part's core sockets, and the combinability test that decides them.
    ///
    /// THE RULE: a target is a selection an IMAGE makes over a part's cores, and
    /// one part admits several -- each core can be an individual target, and two cores "of the same
    /// architecture sharing the same memory space" can also be driven as ONE threaded system.
    ///
    /// SHARED MEMORY IS NOT SUFFICIENT, and the counter-example is real silicon rather than a
    /// hypothetical: one part here puts either an Arm or a RISC-V core in each of two sockets, over
    /// one address space, with atomics interoperating across the pair -- and its own manual says
    /// that arrangement "requires two separate program images". So the test is BOTH conditions, and
    /// this asserts each one can fail on its own.
    #[test]
    fn core_sockets_are_combinable_only_on_one_architecture_and_one_address_space() {
        let part = |cores: Vec<(&str, Vec<&str>)>, shared: Option<bool>| PartRow {
            part: "p".into(),
            cores: cores
                .into_iter()
                .map(|(s, a)| (s.to_string(), a.into_iter().map(String::from).collect()))
                .collect(),
            cores_share_memory: shared,
            ..Default::default()
        };

        assert!(part(vec![], None).cores_combinable().is_none());

        let symmetric = part(
            vec![("core0", vec!["cortex-m0plus"]), ("core1", vec!["cortex-m0plus"])],
            Some(true),
        );
        assert_eq!(symmetric.cores_combinable(), Some(Ok(())));

        let selectable = part(
            vec![
                ("core0", vec!["cortex-m33", "hazard3"]),
                ("core1", vec!["cortex-m33", "hazard3"]),
            ],
            Some(true),
        );
        assert_eq!(selectable.cores_combinable(), Some(Ok(())));

        let mixed = part(
            vec![("core0", vec!["cortex-m33"]), ("core1", vec!["hazard3"])],
            Some(true),
        );
        let Some(Err(why)) = mixed.cores_combinable() else { panic!("mixed pair reported combinable") };
        assert!(why.contains("no single architecture"), "{why}");

        let split = part(
            vec![("core0", vec!["cortex-m7f"]), ("core1", vec!["cortex-m7f"])],
            Some(false),
        );
        let Some(Err(why)) = split.cores_combinable() else { panic!("split memory reported combinable") };
        assert!(why.contains("one address space"), "{why}");
    }

    /// The core-set table refuses what it cannot check, in the same spirit as the ISA profile.
    #[test]
    fn a_core_set_states_its_sockets_completely_or_is_refused() {
        let base = "\
[table]
kind = \"parts\"
family = \"fam\"

[[parts]]
part = \"p\"
package = \"QFN-56\"
flash = 0
ram = 0x1000
pins = [\"GP0\"]
";
        let with = |line: &str| format!("{base}{line}\nsource = \"a datasheet\"\n");

        assert!(
            parse(&with(
                "cores = { core0 = [\"cortex-m0plus\"], core1 = [\"cortex-m0plus\"] }\ncores_share_memory = true"
            ))
            .is_ok(),
            "two sockets with a shared-memory statement is the whole valid shape"
        );

        let error = parse(&with(
            "cores = { core0 = [\"cortex-m0plus\"], core1 = [\"cortex-m0plus\"] }",
        ))
        .unwrap_err();
        assert!(error.contains("without stating cores_share_memory"), "{error}");

        let error = parse(&with("cores = { core0 = [\"cortex-m0plus\"] }\ncores_share_memory = true"))
            .unwrap_err();
        assert!(error.contains("states one core socket"), "{error}");

        let error = parse(&with(
            "cores = { core0 = [\"cortex-m0plus\"], core1 = [\"m0+\"] }\ncores_share_memory = true",
        ))
        .unwrap_err();
        assert!(error.contains("architecture 'm0+'"), "{error}");

        let error = parse(&with("cores = { core0 = [], core1 = [\"cortex-m0plus\"] }\ncores_share_memory = true"))
            .unwrap_err();
        assert!(error.contains("lists no architecture"), "{error}");

        let error = parse(&with("cores_share_memory = true")).unwrap_err();
        assert!(error.contains("without stating `cores`"), "{error}");
    }

    /// A reservation naming a pin the part does not have: two statements that cannot both be true.
    #[test]
    fn a_reservation_outside_the_present_list_is_refused() {
        let text = "\
[table]
kind = \"parts\"
family = \"fam\"

[[parts]]
part = \"p\"
package = \"QFN48\"
flash = 0x40000
ram = 0x8000
pins = [\"PA10\"]
reserved = { PB16 = \"the radio\" }
source = \"a datasheet\"
";
        let error = parse(text).unwrap_err();
        assert!(error.contains("reserves PB16"), "{error}");
        assert!(error.contains("not in its pin list"), "{error}");

        let empty = text.replace("PB16 = \"the radio\"", "PA10 = \"\"");
        let error = parse(&empty).unwrap_err();
        assert!(error.contains("empty owner"), "{error}");

        let good = text.replace("PB16 = \"the radio\"", "PA10 = \"the radio\"");
        assert!(parse(&good).is_ok(), "a reservation on a present pin, with an owner, is fine");
    }

    #[test]
    fn rust_instances_mirror_the_csharp_names_and_spellings() {
        let table = InstancesTable {
            family: "fam".into(),
            record: vec!["base".into(), "gclk_core_id".into(), "apbc_bit".into(), "irq".into()],
            rows: vec![
                InstanceRow {
                    name: "sercom3".into(),
                    block: "sercom".into(),
                    values: vec![0x42001400, 0x17, 5, 12], port: String::new() },
                InstanceRow { name: "gclk".into(), block: "gclk".into(), values: vec![0x40000C00, -1, -1, -1], port: String::new() },
            ],
        };
        let rust = emit_instances_rust(&table, "src.toml", "regen").expect("emits");
        let csharp = emit_instances_csharp(&table, "src.toml", "regen").expect("emits");
        for (rust_line, csharp_line) in [
            ("pub const SERCOM3_BASE: u32 = 0x42001400;", "public const uint SERCOM3_BASE = 0x42001400;"),
            ("pub const SERCOM3_GCLK_CORE_ID: u32 = 23;", "public const uint SERCOM3_GCLK_CORE_ID = 23;"),
            ("pub const SERCOM3_APBC_BIT: u32 = 5;", "public const uint SERCOM3_APBC_BIT = 5;"),
            ("pub const SERCOM3_APBC_MASK: u32 = 0x20;", "public const uint SERCOM3_APBC_MASK = 0x20;"),
            ("pub const SERCOM3_IRQ: u32 = 12;", "public const uint SERCOM3_IRQ = 12;"),
            ("pub const GCLK_BASE: u32 = 0x40000C00;", "public const uint GCLK_BASE = 0x40000C00;"),
        ] {
            assert!(rust.contains(rust_line), "missing: {rust_line}\n{rust}");
            assert!(csharp.contains(csharp_line), "missing: {csharp_line}\n{csharp}");
        }
        assert!(!rust.contains("GCLK_GCLK_CORE_ID"), "-1 fields must emit nothing");
        assert!(rust.starts_with("//! GENERATED by lamella-bsp-gen from src.toml -- DO NOT EDIT."));
    }

    #[test]
    fn swift_instances_mirror_the_csharp_names_and_spellings() {
        let table = InstancesTable {
            family: "fam".into(),
            record: vec!["base".into(), "gclk_core_id".into(), "apbc_bit".into(), "irq".into()],
            rows: vec![
                InstanceRow {
                    name: "sercom3".into(),
                    block: "sercom".into(),
                    values: vec![0x42001400, 0x17, 5, 12], port: String::new() },
                InstanceRow { name: "gclk".into(), block: "gclk".into(), values: vec![0x40000C00, -1, -1, -1], port: String::new() },
            ],
        };
        let swift = emit_instances_swift(&table, "src.toml", "regen").expect("emits");
        for line in [
            "public enum FamInstances {",
            "    public static let SERCOM3_BASE: UInt32 = 0x42001400",
            "    public static let SERCOM3_GCLK_CORE_ID: UInt32 = 23",
            "    public static let SERCOM3_APBC_BIT: UInt32 = 5",
            "    public static let SERCOM3_APBC_MASK: UInt32 = 0x20",
            "    public static let SERCOM3_IRQ: UInt32 = 12",
            "    public static let GCLK_BASE: UInt32 = 0x40000C00",
        ] {
            assert!(swift.contains(line), "missing: {line}\n{swift}");
        }
        assert!(!swift.contains("GCLK_GCLK_CORE_ID"), "-1 fields must emit nothing");
        assert!(swift.starts_with("// GENERATED by lamella-bsp-gen from src.toml -- DO NOT EDIT."));
        assert!(swift.trim_end().ends_with('}'), "the namespace closes");
    }

    #[test]
    fn snake_maps_board_ids_to_module_file_stems() {
        assert_eq!(snake("samd21-xpro"), "samd21_xpro");
        assert_eq!(snake("arduino-zero"), "arduino_zero");
        assert_eq!(snake("samd21"), "samd21");
    }


    const DEVICE_BASE: &str = r#"
[table]
kind = "device-base"
family = "fam"
sources = ["a vendor datasheet"]
notes = ["shared by both members"]

[buses.i2c]
kind = "i2c"
register_read_transform = "identity"
register_write_transform = "identity"
read_protocol = "write-register-then-repeated-start-read"

[buses.spi]
kind = "spi"
register_read_transform = "set-bit7"
register_write_transform = "clear-bit7"
read_protocol = "single-chip-select-address-then-clock-data"
modes = [0, 3]

[address]
bus = "i2c"
base = 0x76

[[address.straps]]
pin = "SDO"
bit = 0
low = 0x76
high = 0x77

[identity]
reg = 0xD0
width = 8

[registers.ctrl_meas]
reg = 0xF4
width = 8
access = "read-write"
fields = { mode = [0, 2], osrs_p = [2, 3] }
notes = "prose"

[registers.status]
reg = 0xF3
width = 8
access = "read-only"
fields = { measuring = [3, 1] }

[registers.press]
reg = 0xF7
width = 24
access = "read-only"
fields = { press_msb = [16, 8], press_lsb = [8, 8], press_xlsb = [4, 4] }

[enums.mode]
sleep = 0
forced = 1
normal = 3
notes = "value 2 also selects forced"

[calibration.temperature]
form = "fam-t-int32"
byte_order = "little"
output_scale = "centi-degrees-celsius"

[[calibration.temperature.read]]
name = "dig_T1"
reg = 0x88
width = 16
signed = false

[[calibration.temperature.read]]
name = "dig_T2"
reg = 0x8A
width = 16
signed = true

[[sequences.forced_measurement]]
step = "write-field"
register = "ctrl_meas"
field = "mode"
value = 1

[[sequences.forced_measurement]]
step = "poll-field-until"
register = "status"
field = "measuring"
value = 0
bounded = true

[[sequences.forced_measurement]]
step = "burst-read"
from = "press"
length_from = "measurement.burst_length"
"#;

    const DEVICE_MEMBER: &str = r#"
[table]
kind = "device"
family = "fam"
part = "one"
base = "fam"

[sourcing]
facts = "primary"
validation = "none"

[identity]
reg = 0xD0
width = 8
values = [0x56, 0x57, 0x58]

[measurement]
burst_length = 6
resolution_bits = "16..20, depending only on the oversampling setting"

[enums.standby_us]
"0" = 500
"6" = 2000000
"7" = 4000000
"#;

    fn device(text: &str) -> DeviceTable {
        match parse(text).expect("the part table parses") {
            Strata::Device(table) => table,
            other => panic!("expected a device table, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_device_base() {
        let table = device(DEVICE_BASE);
        assert!(table.is_base);
        assert_eq!(table.family, "fam");
        assert!(table.part.is_empty());

        assert_eq!(table.buses.len(), 2);
        let spi = table.bus("spi").expect("the spi bus");
        assert_eq!(spi.register_read_transform, "set-bit7");
        assert_eq!(spi.register_write_transform, "clear-bit7");
        assert_eq!(spi.modes, vec![Int { value: 0, hex: false }, Int { value: 3, hex: false }]);
        assert_eq!(table.bus("i2c").expect("the i2c bus").register_read_transform, "identity");

        let address = table.address.as_ref().expect("an address model");
        assert_eq!(address.base.value, 0x76);
        assert_eq!(address.straps.len(), 1);
        assert_eq!(address.straps[0].pin, "SDO");
        assert_eq!(address.straps[0].high.value, 0x77);

        let identity = table.identity.as_ref().expect("an identity");
        assert_eq!(identity.reg.value, 0xD0);
        assert!(identity.values.is_empty(), "a base states where to look, not what to accept");

        let ctrl = table.register("ctrl_meas").expect("ctrl_meas");
        assert_eq!(ctrl.reg.value, 0xF4);
        assert_eq!(ctrl.access, "read-write");
        assert_eq!(ctrl.fields.len(), 2);
        assert_eq!(ctrl.fields[1].name, "osrs_p");
        assert_eq!(ctrl.fields[1].mask(), 0x1C);

        let mode = table.enumeration("mode").expect("the mode encoding");
        assert_eq!(mode.members.len(), 3, "`notes` is prose, never a member");
        assert_eq!(mode.members[2], ("normal".to_string(), Int { value: 3, hex: false }));

        let record = &table.calibrations[0];
        assert_eq!(record.form, "fam-t-int32");
        assert_eq!(record.reads.len(), 2);
        assert_eq!(record.reads[0].name, "dig_T1");
        assert!(!record.reads[0].signed, "dig_T1 is unsigned and its sibling is not");
        assert!(record.reads[1].signed);

        let (name, steps) = &table.sequences[0];
        assert_eq!(name, "forced_measurement");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].value, Some(Int { value: 1, hex: false }));
        assert!(steps[1].bounded, "an unbounded poll on an absent part is a hang");
        assert_eq!(steps[2].length_from, "measurement.burst_length");
    }

    #[test]
    fn parses_a_device_member_with_a_value_set_and_numeric_enum_keys() {
        let table = device(DEVICE_MEMBER);
        assert!(!table.is_base);
        assert_eq!(table.part, "one");
        assert_eq!(table.base, "fam");
        assert_eq!(table.burst_length, 6);

        let identity = table.identity.as_ref().expect("an identity");
        assert_eq!(
            identity.values,
            vec![
                Int { value: 0x56, hex: true },
                Int { value: 0x57, hex: true },
                Int { value: 0x58, hex: true },
            ]
        );

        let standby = table.enumeration("standby_us").expect("the standby encoding");
        assert_eq!(standby.members[0], ("0".to_string(), Int { value: 500, hex: false }));
        assert_eq!(standby.members[2], ("7".to_string(), Int { value: 4_000_000, hex: false }));
    }

    #[test]
    fn a_device_register_may_not_spell_offset() {
        let text = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n\
                    [registers.id]\noffset = 0xD0\nwidth = 8\n";
        let error = parse(text).expect_err("offset is refused");
        assert!(error.contains("'offset'"), "the message names the key: {error}");
        assert!(error.contains("'reg'"), "the message names the repair: {error}");
    }

    #[test]
    fn a_device_table_refuses_a_float_at_any_depth() {
        let bare = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n\
                    [enums.standby]\n\"6\" = 0.5\n";
        let error = parse(bare).expect_err("a bare float is refused");
        assert!(error.contains("never a float"), "{error}");
        let nested = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n\
                      [buses.spi]\nkind = \"spi\"\nmodes = [0, 3.5]\n";
        assert!(parse(nested).expect_err("a float inside an array is refused").contains("never a float"));
    }

    #[test]
    fn a_device_file_refuses_what_is_outside_its_closed_key_set() {
        let section = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n[regions]\nR = 0x1000\n";
        assert!(parse(section).is_err(), "a chip block's section is not a part's");
        let key = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n\
                   [identity]\nreg = 0xD0\nwidth = 8\nvalue = 0x60\n";
        assert!(parse(key).is_err(), "the singular 'value' is not the identity key set");
        let widthless = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n\
                         [registers.id]\nreg = 0xD0\n";
        assert!(parse(widthless).expect_err("a register needs a width").contains("widths are data"));
        let unnamed = "[table]\nkind = \"device\"\nfamily = \"f\"\n";
        assert!(parse(unnamed).expect_err("a part needs an id").contains("part id"));
        let chained = "[table]\nkind = \"device-base\"\nfamily = \"f\"\nbase = \"g\"\n";
        assert!(parse(chained).expect_err("a base has no base").contains("one level"));
        let orphan = "[table]\nkind = \"device\"\nfamily = \"f\"\npart = \"p\"\n\
                      [[calibration.temperature.read]]\nname = \"dig_T1\"\nreg = 0x88\nwidth = 16\n";
        assert!(parse(orphan).expect_err("a read needs its record").contains("[calibration.temperature]"));
    }


    /// The least a part may say about its own sourcing, so a fixture testing something else can
    /// satisfy the requirement without being about it.
    const SOURCING: &str = "[sourcing]\nfacts = \"primary\"\nvalidation = \"none\"\n";

    fn flattened(member: &str) -> DeviceTable {
        let set = DeviceSet {
            family: "fam".into(),
            base: Some(device(DEVICE_BASE)),
            parts: vec![device(member)],
        };
        resolve_device(&set, &set.parts[0]).expect("the member resolves against its base")
    }

    fn emissions(part: &DeviceTable) -> [String; 4] {
        let sources = vec!["member.toml".to_string(), "base.toml".to_string()];
        [
            emit_part_csharp(part, &sources, "regen").expect("C# emits"),
            emit_part_rust(part, &sources, "regen").expect("rust emits"),
            emit_part_swift(part, &sources, "regen").expect("swift emits"),
            emit_part_python(part, &sources, "regen").expect("python emits"),
        ]
    }

    #[test]
    fn a_member_emits_its_base_flattened() {
        let part = flattened(DEVICE_MEMBER);
        let [csharp, ..] = emissions(&part);
        for line in [
            "public const uint CTRL_MEAS_REG = 0xF4;",
            "public const uint CTRL_MEAS_OSRS_P = 0x1C;",
            "public const string BUS_SPI_REGISTER_READ_TRANSFORM = \"set-bit7\";",
            "public const uint ADDRESS_STRAP_SDO_HIGH = 0x77;",
            "public const string TEMPERATURE_FORM = \"fam-t-int32\";",
            "public const uint TEMPERATURE_DIG_T2_SIGNED = 1;",
            "public const uint IDENTITY_VALUE_COUNT = 3;",
            "public const uint IDENTITY_VALUE_2 = 0x58;",
            "public const uint BURST_LENGTH = 6;",
            "public const uint STANDBY_US_6 = 2000000;",
            "public const uint IDENTITY_REG = 0xD0;",
        ] {
            assert!(csharp.contains(line), "missing: {line}\n{csharp}");
        }
        assert!(!csharp.contains("Bmx280"), "the base is resolved away, not referenced");
    }

    #[test]
    fn an_inherited_encoding_is_replaced_whole_and_never_merged() {
        let member = format!(
            "[table]\nkind = \"device\"\nfamily = \"fam\"\npart = \"one\"\nbase = \"fam\"\n{SOURCING}\
             [identity]\nreg = 0xD0\nwidth = 8\nvalues = [0x60]\n\
             [measurement]\nburst_length = 6\n\
             [enums.mode]\nsleep = 0\nforced = 2\n"
        );
        let part = flattened(&member);
        let [csharp, ..] = emissions(&part);
        assert!(csharp.contains("public const uint MODE_FORCED = 2;"), "the member's code wins");
        assert!(
            !csharp.contains("MODE_NORMAL"),
            "a code the member did not restate must NOT survive from the base:\n{csharp}"
        );
    }

    #[test]
    fn the_four_languages_spell_the_same_names_and_values() {
        let part = flattened(DEVICE_MEMBER);
        let [csharp, rust, swift, python] = emissions(&part);
        let pairs = |text: &str, strip: &str, name_of: fn(&str) -> String| -> Vec<(String, String)> {
            text.lines()
                .filter_map(|line| line.trim().strip_prefix(strip).map(str::to_string))
                .filter_map(|line| {
                    let (declared, value) = line.split_once(" = ")?;
                    Some((name_of(declared), value.trim_end_matches(';').trim().to_string()))
                })
                .collect()
        };
        let after_type = |declared: &str| declared.split_whitespace().last().unwrap_or_default().to_string();
        let before_type = |declared: &str| declared.split(':').next().unwrap_or_default().trim().to_string();
        let cs = pairs(&csharp, "public const ", after_type);
        let rs = pairs(&rust, "pub const ", before_type);
        let sw = pairs(&swift, "public static let ", before_type);
        let py: Vec<(String, String)> = python
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .filter_map(|line| line.split_once(" = "))
            .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            .collect();

        assert!(cs.len() > 60, "the sample part emits a real table, got {}", cs.len());
        assert_eq!(cs.len(), rs.len(), "C# and rust emit a different number of facts");
        assert_eq!(cs.len(), sw.len(), "C# and swift emit a different number of facts");
        assert_eq!(cs.len(), py.len(), "C# and python emit a different number of facts");
        for (at, (name, value)) in cs.iter().enumerate() {
            assert_eq!((name, value), (&rs[at].0, &rs[at].1), "rust differs at row {at}");
            assert_eq!((name, value), (&sw[at].0, &sw[at].1), "swift differs at row {at}");
            assert_eq!((name, value), (&py[at].0, &py[at].1), "python differs at row {at}");
        }
    }

    #[test]
    fn a_sequence_step_carries_its_own_resolved_register_mask_and_length() {
        let part = flattened(DEVICE_MEMBER);
        let [csharp, ..] = emissions(&part);
        for line in [
            "public const string FORCED_MEASUREMENT_STEP_1_OP = \"poll-field-until\";",
            "public const string FORCED_MEASUREMENT_STEP_1_TARGET = \"status.measuring\";",
            "public const uint FORCED_MEASUREMENT_STEP_1_REG = 0xF3;",
            "public const uint FORCED_MEASUREMENT_STEP_1_MASK = 0x8;",
            "public const uint FORCED_MEASUREMENT_STEP_1_LSB = 3;",
            "public const uint FORCED_MEASUREMENT_STEP_1_BOUNDED = 1;",
            "public const uint FORCED_MEASUREMENT_STEP_2_LENGTH = 6;",
        ] {
            assert!(csharp.contains(line), "missing: {line}\n{csharp}");
        }
        assert!(!csharp.contains("LENGTH_FROM"), "a reference the generator can resolve is resolved");
    }

    #[test]
    fn a_flattened_part_is_refused_when_a_reference_goes_nowhere() {
        let with = |body: &str| -> String {
            format!(
                "[table]\nkind = \"device\"\nfamily = \"fam\"\npart = \"one\"\nbase = \"fam\"\n{SOURCING}\
                 [identity]\nreg = 0xD0\nwidth = 8\nvalues = [0x60]\n[measurement]\nburst_length = 6\n{body}"
            )
        };
        let set = |text: &str| DeviceSet {
            family: "fam".into(),
            base: Some(device(DEVICE_BASE)),
            parts: vec![device(text)],
        };
        let emit = |text: &str| -> Result<String, String> {
            let s = set(text);
            let part = resolve_device(&s, &s.parts[0])?;
            emit_part_csharp(&part, &["m.toml".to_string()], "regen")
        };

        let dangling = with("[[sequences.reset]]\nstep = \"write-field\"\nregister = \"nowhere\"\nvalue = 1\n");
        assert!(emit(&dangling).expect_err("a dangling register is refused").contains("nowhere"));

        let bad_field = with("[[sequences.reset]]\nstep = \"write-field\"\nregister = \"status\"\nfield = \"nope\"\nvalue = 1\n");
        assert!(emit(&bad_field).expect_err("a dangling field is refused").contains("status.nope"));

        let bad_depends = with("[calibration.humidity]\nform = \"h\"\ndepends_on = [\"nowhere\"]\n");
        assert!(emit(&bad_depends).expect_err("a dangling dependency is refused").contains("nowhere"));

        let no_values =
            format!("[table]\nkind = \"device\"\nfamily = \"fam\"\npart = \"one\"\nbase = \"fam\"\n{SOURCING}");
        assert!(emit(&no_values).expect_err("an empty identity is refused").contains("accepts no value"));

        let mismatch = format!(
            "[table]\nkind = \"device\"\nfamily = \"fam\"\npart = \"one\"\nbase = \"fam\"\n{SOURCING}\
             [identity]\nreg = 0xF4\nwidth = 16\nvalues = [0x60]\n"
        );
        assert!(emit(&mismatch).expect_err("a width disagreement is refused").contains("[registers.ctrl_meas]"));
    }


    /// A member with an arbitrary `[sourcing]` body, everything else minimal and valid.
    fn sourced(body: &str) -> String {
        format!(
            "[table]\nkind = \"device\"\nfamily = \"fam\"\npart = \"one\"\nbase = \"fam\"\n\
             [sourcing]\n{body}\
             [identity]\nreg = 0xD0\nwidth = 8\nvalues = [0x60]\n[measurement]\nburst_length = 6\n"
        )
    }

    /// Resolves and emits one member against the shared base, so a refusal from either stage
    /// surfaces the same way.
    fn emit_one(text: &str) -> Result<String, String> {
        let set = DeviceSet {
            family: "fam".into(),
            base: Some(device(DEVICE_BASE)),
            parts: vec![device(text)],
        };
        let part = resolve_device(&set, &set.parts[0])?;
        emit_part_csharp(&part, &["m.toml".to_string()], "regen")
    }

    #[test]
    fn a_part_must_declare_how_well_it_is_known() {
        let silent = "[table]\nkind = \"device\"\nfamily = \"fam\"\npart = \"one\"\nbase = \"fam\"\n\
                      [identity]\nreg = 0xD0\nwidth = 8\nvalues = [0x60]\n[measurement]\nburst_length = 6\n";
        let error = emit_one(silent).expect_err("a part that states no sourcing is refused");
        assert!(error.contains("states no [sourcing]"), "{error}");

        assert!(emit_one(&sourced("facts = \"primary\"\nvalidation = \"none\"\n")).is_ok());
    }

    #[test]
    fn a_tier_outside_the_closed_set_is_refused() {
        let facts = emit_one(&sourced("facts = \"datasheet\"\nvalidation = \"none\"\n"))
            .expect_err("an unknown facts tier is refused");
        assert!(facts.contains("primary, secondary"), "{facts}");

        let validation = emit_one(&sourced("facts = \"primary\"\nvalidation = \"tested\"\n"))
            .expect_err("an unknown validation tier is refused");
        assert!(validation.contains("none, identified, exercised"), "{validation}");
    }

    #[test]
    fn second_hand_facts_must_name_what_they_are_second_hand_to() {
        let unnamed = emit_one(&sourced("facts = \"secondary\"\nvalidation = \"none\"\n"))
            .expect_err("secondary sourcing with no derived_from is refused");
        assert!(unnamed.contains("names no derived_from"), "{unnamed}");

        let both = emit_one(&sourced(
            "facts = \"primary\"\nderived_from = \"two\"\nvalidation = \"none\"\n",
        ))
        .expect_err("primary sourcing that also derives is refused");
        assert!(both.contains("derives from nothing"), "{both}");
    }

    #[test]
    fn a_derived_from_must_name_a_sibling_the_family_actually_has() {
        let absent = emit_one(&sourced(
            "facts = \"secondary\"\nderived_from = \"ghost\"\nvalidation = \"none\"\n",
        ))
        .expect_err("a derived_from naming no sibling is refused");
        assert!(absent.contains("not a part of family 'fam'"), "{absent}");

        let itself = emit_one(&sourced(
            "facts = \"secondary\"\nderived_from = \"one\"\nvalidation = \"none\"\n",
        ))
        .expect_err("a self-referential derived_from is refused");
        assert!(itself.contains("names the part itself"), "{itself}");
    }

    #[test]
    fn a_validation_rank_must_say_what_earned_it() {
        let bare = emit_one(&sourced("facts = \"primary\"\nvalidation = \"identified\"\n"))
            .expect_err("a rank above none with no evidence is refused");
        assert!(bare.contains("no evidence is stated"), "{bare}");

        let unclaimed = emit_one(&sourced(
            "facts = \"primary\"\nvalidation = \"none\"\nevidence = \"it answered once\"\n",
        ))
        .expect_err("evidence under a rank of none is refused");
        assert!(unclaimed.contains("earns no rank"), "{unclaimed}");

        let earned = emit_one(&sourced(
            "facts = \"primary\"\nvalidation = \"identified\"\n\
             evidence = \"the identity register answered the same value in every read, and a control that never selected the part read a different one\"\n",
        ))
        .expect("a rank with its evidence emits");
        assert!(earned.contains("public const string SOURCING_VALIDATION = \"identified\";"), "{earned}");
        assert!(earned.contains("SOURCING_EVIDENCE"), "{earned}");
    }

    #[test]
    fn a_family_base_may_not_rank_itself() {
        let ranked = format!("{DEVICE_BASE}\n[sourcing]\nfacts = \"primary\"\nvalidation = \"none\"\n");
        let error = parse(&ranked).expect_err("a base that ranks itself is refused");
        assert!(error.contains("sourced differently for each member"), "{error}");
    }

    #[test]
    fn the_sourcing_tier_never_joins_the_family_invariant() {
        let one = device(&sourced("facts = \"primary\"\nvalidation = \"none\"\n"));
        let two = device(
            &sourced("facts = \"secondary\"\nderived_from = \"one\"\nvalidation = \"none\"\n")
                .replace("part = \"one\"", "part = \"two\""),
        );
        let set = DeviceSet { family: "fam".into(), base: Some(device(DEVICE_BASE)), parts: vec![one, two] };
        let resolved: Vec<DeviceTable> =
            set.parts.iter().map(|p| resolve_device(&set, p).expect("resolves")).collect();

        for part in &resolved {
            let rows = device_rows(part).expect("rows");
            assert!(
                rows.iter().any(|r| matches!(r, Row::Str(n, v) if n == "SOURCING_VALIDATION" && v == "none")),
                "both members state validation = none"
            );
        }

        let common = common_rows(&resolved).expect("the family intersects");
        assert!(
            !common.iter().any(|r| matches!(r, Row::Str(n, _) if n.starts_with("SOURCING_"))),
            "no sourcing row may be a family invariant"
        );
        assert!(
            common.iter().any(|r| matches!(r, Row::Uint(n, _) if n == "CTRL_MEAS_REG")),
            "an ordinary shared fact still intersects"
        );
    }

    #[test]
    fn every_language_spells_the_tier() {
        let second = device(
            &sourced("facts = \"secondary\"\nderived_from = \"one\"\nvalidation = \"none\"\n")
                .replace("part = \"one\"", "part = \"two\""),
        );
        let set = DeviceSet {
            family: "fam".into(),
            base: Some(device(DEVICE_BASE)),
            parts: vec![device(&sourced("facts = \"primary\"\nvalidation = \"none\"\n")), second],
        };
        let part = resolve_device(&set, &set.parts[1]).expect("the second member resolves");
        let [csharp, rust, swift, python] = emissions(&part);
        for (rendered, spelling) in [
            (&csharp, "public const string SOURCING_FACTS = \"secondary\";"),
            (&rust, "pub const SOURCING_FACTS: &str = \"secondary\";"),
            (&swift, "public static let SOURCING_FACTS: StaticString = \"secondary\""),
            (&python, "SOURCING_FACTS = \"secondary\""),
        ] {
            assert!(rendered.contains(spelling), "missing: {spelling}\n{rendered}");
        }
        for rendered in [&csharp, &rust, &swift, &python] {
            assert!(rendered.contains("SOURCING_DERIVED_FROM"), "{rendered}");
            assert!(rendered.contains("SOURCING_VALIDATION"), "{rendered}");
        }
    }
}
