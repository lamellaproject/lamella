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

impl BlockTable {
    /// The register named `name`, when declared.
    #[must_use]
    pub fn register(&self, name: &str) -> Option<&BlockRegister> {
        self.registers.iter().find(|r| r.name == name)
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

/// One pin-function row: pin x function -> (instance, signal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRow {
    /// The pin name (`PA22`).
    pub pin: String,
    /// The mux function letter (`C`).
    pub function: String,
    /// The instance the (pin, function) cell routes to.
    pub instance: String,
    /// The signal at that cell (`pad0`).
    pub signal: String,
}

/// The pin-function map (partial, append-only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinsTable {
    /// The family id.
    pub family: String,
    /// The rows, in table order.
    pub rows: Vec<PinRow>,
}

/// One part row: an orderable chip with its package, memory, and (partial) pin set.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The module id (`atsamw25`).
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

/// A board BSP: the bindings, carrier, plans, and identity of one product.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardTable {
    /// The board id (`samd21-xpro`).
    pub board: String,
    /// The family id, when the board names a bare chip (exclusive with `module`).
    pub family: String,
    /// The module id, when the board carries a module (exclusive with `family`).
    pub module: String,
    /// The exact part id (required with `family`; implied by `module`).
    pub part: String,
    /// The `lamella_wire::board_model` wire code.
    pub board_model: i64,
    /// The board's soldered external-XIP flash in bytes (the closed `[memory]`
    /// section, source-cited). 0 = no record: the part's own parts row is the memory truth.
    pub memory_flash: i64,
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
}

impl BoardTable {
    /// The board's default plan (validated present by `parse`).
    #[must_use]
    pub fn default_plan(&self) -> Option<&Plan> {
        self.plans.iter().find(|p| p.default)
    }

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

    type PendingRow = (usize, String, String, Vec<(String, i64)>);
    let mut current: Option<PendingRow> = None;
    let finish = |current: &mut Option<PendingRow>,
                      table: &mut InstancesTable|
     -> Result<(), String> {
        if let Some((line, name, block, values)) = current.take() {
            if name.is_empty() || block.is_empty() {
                return Err(err(line, "an instance row needs name and block"));
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
            table.rows.push(InstanceRow { name, block, values: ordered });
        }
        Ok(())
    };

    for (line, item) in items {
        match item {
            Item::ArraySection(name) if name == "instances" => {
                finish(&mut current, &mut table)?;
                current = Some((*line, String::new(), String::new(), Vec::new()));
            }
            Item::Section(name) | Item::ArraySection(name) => {
                return Err(err(*line, &format!("unexpected section '{name}' in instances -- the key set is closed")));
            }
            Item::KeyValue(key, value) => {
                let Some((_, name, block, values)) = current.as_mut() else {
                    return Err(err(*line, "key outside [[instances]]"));
                };
                match (key.as_str(), value) {
                    ("name", RawValue::Str(s)) => *name = s.clone(),
                    ("block", RawValue::Str(s)) => *block = s.clone(),
                    (field, RawValue::Int(i)) => values.push((field.to_string(), i.value)),
                    (other, _) => return Err(err(*line, &format!("unexpected instance key '{other}'"))),
                }
            }
        }
    }
    finish(&mut current, &mut table)?;
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
                    ("source", RawValue::Str(_)) => {}
                    (other, _) => return Err(err(*line, &format!("unexpected pin key '{other}'"))),
                }
            }
        }
    }
    for row in &table.rows {
        if split_pin(&row.pin).is_none() {
            return Err(format!("pins: '{}' is not a P<port><index> pin name", row.pin));
        }
    }
    Ok(table)
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
                    ("source", RawValue::Str(_)) => {}
                    ("pins", RawValue::Array(parts)) => {
                        for part in parts {
                            match part {
                                RawValue::Str(s) => row.pins.push(s.clone()),
                                _ => return Err(err(*line, "pins entries must be strings")),
                            }
                        }
                    }
                    (other, _) => return Err(err(*line, &format!("unexpected part key '{other}'"))),
                }
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
        &["kind", "board", "family", "module", "part", "board_model", "sources", "notes"],
    )?;
    let mut table = BoardTable {
        board: header_str(header, "board")?,
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
    if !table.family.is_empty() && table.part.is_empty() {
        return Err("a family board must name its exact part".to_string());
    }

    enum At {
        None,
        Carrier,
        Memory,
        Binding,
        Plan,
        Device,
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
            Item::Section(name) if name == "memory" => at = At::Memory,
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
                    ("flash", RawValue::Int(i)) => table.memory_flash = i.value,
                    ("source", RawValue::Str(_)) => memory_source_cited = true,
                    (other, _) => {
                        return Err(err(
                            *line,
                            &format!("unexpected [memory] key '{other}' (the closed [memory] record takes flash + source only)"),
                        ));
                    }
                },
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
    if table.memory_flash > 0 && !memory_source_cited {
        return Err(format!(
            "board {}: [memory] flash must be SOURCE-CITED",
            table.board
        ));
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
        if set.instances.row(&row.instance).is_none() {
            return Err(format!("pins: '{}' names unknown instance '{}'", row.pin, row.instance));
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
    if board.memory_flash > 0 && part_row.flash != 0 {
        return Err(format!(
            "board {}: [memory] flash stated but part '{}' has internal flash (part flash = 0x{:X}) -- a second home for a chip fact refuses; the [memory] record is for external-XIP parts",
            board.board, part, part_row.flash
        ));
    }
    validate_bindings(set, &bindings, &part, &board.board)?;
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
    Ok(ResolvedBoard { board, part, bindings, module_pins })
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

/// One resolved sam3x-family uart-binding emission (the SAM3X UART shape; the first sam3x board):
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

/// One resolved st-usart binding emission (the modern ST USART IP, shared across the ST
/// families): the usart base, the RCC enable (register, mask) pairs for the instance and its
/// port -- resolved through the [base, rcc_en_off, rcc_en_bit] instance record, which is what
/// makes the F0/L4 RCC-bank split DATA (L4: GPIOAEN = AHB2ENR bit 0; F0: IOPAEN = AHBENR
/// bit 17) and lets ONE arm serve both families -- the GPIO MODER/AFRL registers with the
/// per-pin-pair masks and set values derived from the bound pins + AF number, the plan's PCLK1
/// rate, and the carrier rate's BRR divisor (16x oversampling, ROUNDED division: e.g. 0x23 @
/// 4 MHz, 0x45 @ 8 MHz, 139 @ 16 MHz).
struct StUartEmission {
    prefix: String,
    role: String,
    instance: String,
    base: i64,
    rcc_en_reg: i64,
    rcc_en_mask: i64,
    port_rcc_en_reg: i64,
    port_rcc_en_mask: i64,
    moder_reg: i64,
    moder_mask: i64,
    moder_value: i64,
    afrl_reg: i64,
    afrl_mask: i64,
    afrl_value: i64,
    pclk1_hz: i64,
    /// (`BRR_<rate>_<PLAN>` suffix, divisor), one per carrier whose wire rides this binding,
    /// under THAT carrier's plan; empty when no carrier rides it.
    bauds: Vec<(String, i64)>,
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
    let (tx_port, tx_index) = split_pin(&tx.pin).ok_or_else(|| format!("{board}: bad pin {}", tx.pin))?;
    let (rx_port, rx_index) = split_pin(&rx.pin).ok_or_else(|| format!("{board}: bad pin {}", rx.pin))?;
    if tx_port != rx_port {
        return Err(format!(
            "{board}: uart binding '{}' pins {}/{} sit on different ports -- per-port emission is the growth path; add it with the board that needs it",
            binding.role, tx.pin, rx.pin
        ));
    }
    for (label, index) in [("tx", tx_index), ("rx", rx_index)] {
        if index > 7 {
            return Err(format!(
                "{board}: uart binding '{}' {label} pin index {index} needs AFRH emission -- not implemented",
                binding.role
            ));
        }
    }
    let group = format!("gpio{tx_port}");
    let group_base = instances
        .value(&group, "base")
        .ok_or_else(|| format!("{board}: no instance row for port group '{group}'"))?;
    let (port_rcc_en_reg, port_rcc_en_mask) = rcc_enable(&group)?;

    let gpio = set.block("gpio", "").ok_or_else(|| format!("{board}: no gpio block table"))?;
    let moder = gpio.register("MODER").ok_or_else(|| format!("{board}: gpio has no MODER"))?;
    let afrl = gpio.register("AFRL").ok_or_else(|| format!("{board}: gpio has no AFRL"))?;
    let mode_af = gpio
        .constant("MODER_MODE_AF")
        .ok_or_else(|| format!("{board}: gpio block has no MODER_MODE_AF constant"))?;
    let af = binding
        .function
        .strip_prefix("AF")
        .and_then(|d| d.parse::<i64>().ok())
        .ok_or_else(|| {
            format!("{board}: uart binding '{}' function '{}' is not AF<n>", binding.role, binding.function)
        })?;
    let moder_mask = (0b11 << (2 * tx_index)) | (0b11 << (2 * rx_index));
    let moder_value = (mode_af << (2 * tx_index)) | (mode_af << (2 * rx_index));
    let afrl_mask = (0xF << (4 * tx_index)) | (0xF << (4 * rx_index));
    let afrl_value = (af << (4 * tx_index)) | (af << (4 * rx_index));

    let plan = resolved.board.default_plan().expect("validated: exactly one default plan");
    let pclk1_hz = plan.rate("pclk1_hz").ok_or_else(|| {
        format!("{board}: default plan '{}' states no pclk1_hz rate", plan.name)
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
        let pclk1 = carrier_plan.rate("pclk1_hz").ok_or_else(|| {
            format!("{board}: plan '{}' states no pclk1_hz rate", carrier_plan.name)
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
        port_rcc_en_reg,
        port_rcc_en_mask,
        moder_reg: group_base + moder.offset.value,
        moder_mask,
        moder_value,
        afrl_reg: group_base + afrl.offset.value,
        afrl_mask,
        afrl_value,
        pclk1_hz,
        bauds,
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
            "{board}: spi binding '{}' has a MUXED chip select -- only the soft-CS shape is ruled; add the hard-CS emission with its anchor first",
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
    sercom_uarts: Vec<UartEmission>,
    rp_uarts: Vec<RpUartEmission>,
    esp_uarts: Vec<EspUartEmission>,
    sam3x_uarts: Vec<Sam3xUartEmission>,
    st_uarts: Vec<StUartEmission>,
    sercom_spis: Vec<SpiEmission>,
    sercom_i2cs: Vec<SercomI2cEmission>,
    pl022_spis: Vec<SpiPl022Emission>,
    nrf_twis: Vec<NrfTwiEmission>,
    dw_i2cs: Vec<DwI2cEmission>,
    rp_adcs: Vec<RpAdcEmission>,
    rp_clocks: Vec<RpClockEmission>,
}

fn resolve_board_emissions(set: &FamilySet, resolved: &ResolvedBoard) -> Result<BoardEmissions, String> {
    let mut emissions = BoardEmissions {
        skipped: Vec::new(),
        sercom_uarts: Vec::new(),
        rp_uarts: Vec::new(),
        esp_uarts: Vec::new(),
        sam3x_uarts: Vec::new(),
        st_uarts: Vec::new(),
        sercom_spis: Vec::new(),
        sercom_i2cs: Vec::new(),
        pl022_spis: Vec::new(),
        nrf_twis: Vec::new(),
        dw_i2cs: Vec::new(),
        rp_adcs: Vec::new(),
        rp_clocks: resolve_clocks_rp(set, resolved)?,
    };
    for binding in &resolved.bindings {
        match binding.kind.as_str() {
            "uart" => match set.family.as_str() {
                "samd21" => emissions.sercom_uarts.push(resolve_uart(set, resolved, binding)?),
                "rp2040" => emissions.rp_uarts.push(resolve_uart_rp(set, resolved, binding, false)?),
                "rp2350" => emissions.rp_uarts.push(resolve_uart_rp(set, resolved, binding, true)?),
                "esp32c6" => emissions.esp_uarts.push(resolve_uart_esp32c6(set, resolved, binding)?),
                "sam3x" => emissions.sam3x_uarts.push(resolve_uart_sam3x(set, resolved, binding)?),
                "stm32l476" | "stm32f091" => {
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
        sercom_uarts: uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
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
    if resolved.board.carrier.usb_vid > 0 {
        push_const(&mut out, "uint", "CARRIER_USB_VID", &format!("0x{:04X}", resolved.board.carrier.usb_vid));
    }
    if resolved.board.carrier.usb_pid > 0 {
        push_const(&mut out, "uint", "CARRIER_USB_PID", &format!("0x{:04X}", resolved.board.carrier.usb_pid));
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
        push_const(&mut out, "uint", &format!("{p}_PORT_RCC_EN_REG"), &format!("0x{:X}", uart.port_rcc_en_reg));
        push_const(&mut out, "uint", &format!("{p}_PORT_RCC_EN_MASK"), &format!("0x{:X}", uart.port_rcc_en_mask));
        push_const(&mut out, "uint", &format!("{p}_MODER_REG"), &format!("0x{:X}", uart.moder_reg));
        push_const(&mut out, "uint", &format!("{p}_MODER_MASK"), &format!("0x{:X}", uart.moder_mask));
        push_const(&mut out, "uint", &format!("{p}_MODER_VALUE"), &format!("0x{:X}", uart.moder_value));
        push_const(&mut out, "uint", &format!("{p}_AFRL_REG"), &format!("0x{:X}", uart.afrl_reg));
        push_const(&mut out, "uint", &format!("{p}_AFRL_MASK"), &format!("0x{:X}", uart.afrl_mask));
        push_const(&mut out, "uint", &format!("{p}_AFRL_VALUE"), &format!("0x{:X}", uart.afrl_value));
        push_const(&mut out, "uint", &format!("{p}_PCLK1_HZ"), &uart.pclk1_hz.to_string());
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

    finish_class(&mut out)?;
    Ok(out)
}

/// A control pin's GPIO-group row base + pin index. Lettered/numbered ports resolve as
/// `port<x>` instance rows (`porta`, `port0`); the rp-family's bank-less GP pins are driven
/// through the SIO block (the vendor's own instance name, base SIO 0xd0000000).
fn control_pin_group_base(set: &FamilySet, board: &str, pin: &str) -> Result<(i64, u32), String> {
    let Some((port, index)) = split_pin(pin) else {
        return Err(format!("{board}: bad control pin {pin}"));
    };
    let group = if port == 'g' && set.family.starts_with("rp") {
        "sio".to_string()
    } else {
        format!("port{port}")
    };
    let group_base = set
        .instances
        .value(&group, "base")
        .ok_or_else(|| format!("{board}: no instance row for '{group}'"))?;
    Ok((group_base, index))
}


fn emit_rust_header(out: &mut String, what: &str, sources: &[String], regen: &str) {
    out.push_str(&format!(
        "// GENERATED by lamella-bsp-gen from {list} -- DO NOT EDIT.\n// Regenerate: {regen}\n//\n// {what}\n",
        list = sources.join(" + "),
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
        "The {} INSTANCE map as Rust consts, name/value-identical to {}.g.cs.\n// Block-register offsets are NOT emitted for Rust: firmware hand-codes its\n// own block constants and includes this file for every placed-instance fact.",
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
        sercom_uarts: uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
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
    if resolved.board.carrier.usb_vid > 0 {
        push_rust_const(&mut out, "u16", "CARRIER_USB_VID", &format!("0x{:04X}", resolved.board.carrier.usb_vid));
    }
    if resolved.board.carrier.usb_pid > 0 {
        push_rust_const(&mut out, "u16", "CARRIER_USB_PID", &format!("0x{:04X}", resolved.board.carrier.usb_pid));
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
        push_rust_const(&mut out, "u32", &format!("{p}_PORT_RCC_EN_REG"), &format!("0x{:X}", uart.port_rcc_en_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_PORT_RCC_EN_MASK"), &format!("0x{:X}", uart.port_rcc_en_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_MODER_REG"), &format!("0x{:X}", uart.moder_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_MODER_MASK"), &format!("0x{:X}", uart.moder_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_MODER_VALUE"), &format!("0x{:X}", uart.moder_value));
        push_rust_const(&mut out, "u32", &format!("{p}_AFRL_REG"), &format!("0x{:X}", uart.afrl_reg));
        push_rust_const(&mut out, "u32", &format!("{p}_AFRL_MASK"), &format!("0x{:X}", uart.afrl_mask));
        push_rust_const(&mut out, "u32", &format!("{p}_AFRL_VALUE"), &format!("0x{:X}", uart.afrl_value));
        push_rust_const(&mut out, "u32", &format!("{p}_PCLK1_HZ"), &uart.pclk1_hz.to_string());
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

    finish_rust(&out)?;
    Ok(out)
}


/// The families whose strata additionally emit the Swift projection. The emitters are
/// family-generic; each family joins this list deliberately as its Swift consumers arrive.
const SWIFT_FAMILIES: &[&str] = &["nrf51", "nrf52833", "rp2040", "rp2350", "samd21", "stm32l476"];

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
        sercom_uarts: uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
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
    if resolved.board.carrier.usb_vid > 0 {
        push_swift_const(&mut out, "UInt16", "CARRIER_USB_VID", &format!("0x{:04X}", resolved.board.carrier.usb_vid));
    }
    if resolved.board.carrier.usb_pid > 0 {
        push_swift_const(&mut out, "UInt16", "CARRIER_USB_PID", &format!("0x{:04X}", resolved.board.carrier.usb_pid));
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
        push_swift_const(&mut out, "UInt32", &format!("{p}_PORT_RCC_EN_REG"), &format!("0x{:X}", uart.port_rcc_en_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PORT_RCC_EN_MASK"), &format!("0x{:X}", uart.port_rcc_en_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_MODER_REG"), &format!("0x{:X}", uart.moder_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_MODER_MASK"), &format!("0x{:X}", uart.moder_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_MODER_VALUE"), &format!("0x{:X}", uart.moder_value));
        push_swift_const(&mut out, "UInt32", &format!("{p}_AFRL_REG"), &format!("0x{:X}", uart.afrl_reg));
        push_swift_const(&mut out, "UInt32", &format!("{p}_AFRL_MASK"), &format!("0x{:X}", uart.afrl_mask));
        push_swift_const(&mut out, "UInt32", &format!("{p}_AFRL_VALUE"), &format!("0x{:X}", uart.afrl_value));
        push_swift_const(&mut out, "UInt32", &format!("{p}_PCLK1_HZ"), &uart.pclk1_hz.to_string());
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
        sercom_uarts,
        rp_uarts,
        esp_uarts,
        sam3x_uarts,
        st_uarts,
        sercom_spis,
        sercom_i2cs,
        pl022_spis,
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
    for uart in &st_uarts {
        let mut rows = vec![
            ("kind".to_string(), "\"uart\"".to_string()),
            ("instance".to_string(), format!("\"{}\"", uart.instance)),
            ("base".to_string(), format!("0x{:X}", uart.base)),
            ("rcc_en_reg".to_string(), format!("0x{:X}", uart.rcc_en_reg)),
            ("rcc_en_mask".to_string(), format!("0x{:X}", uart.rcc_en_mask)),
            ("port_rcc_en_reg".to_string(), format!("0x{:X}", uart.port_rcc_en_reg)),
            ("port_rcc_en_mask".to_string(), format!("0x{:X}", uart.port_rcc_en_mask)),
            ("moder_reg".to_string(), format!("0x{:X}", uart.moder_reg)),
            ("moder_mask".to_string(), format!("0x{:X}", uart.moder_mask)),
            ("moder_value".to_string(), format!("0x{:X}", uart.moder_value)),
            ("afrl_reg".to_string(), format!("0x{:X}", uart.afrl_reg)),
            ("afrl_mask".to_string(), format!("0x{:X}", uart.afrl_mask)),
            ("afrl_value".to_string(), format!("0x{:X}", uart.afrl_value)),
            ("pclk1_hz".to_string(), uart.pclk1_hz.to_string()),
        ];
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

    out.push_str(&format!(
        "\n# Per-role descriptor dicts: the same values the C# {} class carries as consts,\n# under one mechanical renaming rule -- an UPPER_SNAKE const there is a lowercase key here,\n# grouped by the role it belongs to.\nFACTS = {{\n",
        bindings_class(&resolved.board.board)
    ));
    for (role, rows) in &roles {
        out.push_str(&format!("    \"{role}\": {{\n"));
        for (key, value) in rows {
            out.push_str(&format!("        \"{key}\": {value},\n"));
        }
        out.push_str("    },\n");
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
        "\n# On-board devices + module control lines (the C# <NAME>_PORT_BASE/_PIN/_MASK/\n# _ACTIVE_LOW consts, dict-shaped): PORT group base + pin index + mask + polarity.\nDEVICES = {\n",
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
        let resolved = resolve_board(&set, board)?;
        let mut sources = vec![format!("bsp/{id}/board.toml"), format!("csp/{family}/ strata")];
        if !resolved.board.module.is_empty() {
            sources.insert(1, format!("csp/{}/module.toml", resolved.board.module));
        }
        out.push(Generated {
            path: format!("bsp/{id}/csharp/{}.g.cs", bindings_class(&id)),
            contents: emit_board_csharp(&set, &resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("bsp/{id}/rust/{}_bindings.rs", snake(&id)),
            contents: emit_board_rust(&set, &resolved, &sources, &regen)?,
        });
        out.push(Generated {
            path: format!("bsp/{id}/python/board.py"),
            contents: emit_board_python(&set, &resolved, &sources, &regen)?,
        });
        if swift {
            out.push(Generated {
                path: format!("bsp/{id}/swift/{}.swift", bindings_class(&id)),
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
        assert_eq!(board.memory_flash, 0x200000);
        let bad_key = good.replace("flash = 0x200000", "ram = 0x1000");
        let error = parse(&bad_key).unwrap_err();
        assert!(error.contains("[memory]"), "{error}");
        let uncited = good.replace("source = \"board datasheet\"\n", "");
        let error = parse(&uncited).unwrap_err();
        assert!(error.contains("SOURCE-CITED"), "{error}");
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
        };
        assert!(row.has_pin("PA13"));
        assert!(row.has_pin("PB09"));
        assert!(!row.has_pin("PA16"));
        assert!(!row.has_pin("PB10"));
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
                    values: vec![0x42001400, 0x17, 5, 12],
                },
                InstanceRow { name: "gclk".into(), block: "gclk".into(), values: vec![0x40000C00, -1, -1, -1] },
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
        assert!(rust.starts_with("// GENERATED by lamella-bsp-gen from src.toml -- DO NOT EDIT."));
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
                    values: vec![0x42001400, 0x17, 5, 12],
                },
                InstanceRow { name: "gclk".into(), block: "gclk".into(), values: vec![0x40000C00, -1, -1, -1] },
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
}
