//! The v0 peripheral-table harvester: parses a per-(chip, peripheral) register-fact table
//! (the family's single source of truth) and emits generated per-language bindings under
//! `bsp/<chip>/<lang>/`, so no driver hand-transcribes a register address again.

/// The v2 strata dialect (family CSPs, module CSPs, board BSPs) and its emitters.
/// The v0 per-(chip, peripheral) dialect below coexists with it until every
/// family migrates.
pub mod fit;
pub mod strata;

/// An integer fact plus how the table spelled it (hex spelling is preserved in the emission,
/// so a generated file reads like the table that produced it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Int {
    /// The value (wide enough for u32 facts and signed calibration records alike).
    pub value: i64,
    /// Whether the table wrote it in hex.
    pub hex: bool,
}

/// One field of a register: `NAME = [lsb, width]` in the table's inline field maps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The field name (unique within its register).
    pub name: String,
    /// The least significant bit position.
    pub lsb: u32,
    /// The width in bits.
    pub width: u32,
}

impl Field {
    /// The shifted mask covering this field -- the form register math consumes.
    pub fn mask(&self) -> u64 {
        (((1u64 << self.width) - 1) << self.lsb) & 0xffff_ffff
    }
}

/// One register: a named offset within a region, plus its fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register {
    /// The register name (the table's key under `[registers.*]`).
    pub name: String,
    /// The region whose base the offset resolves against.
    pub region: String,
    /// The byte offset within the region.
    pub offset: Int,
    /// The declared fields, in table order.
    pub fields: Vec<Field>,
}

/// One step of a named sequence: the table's op/reg/value/mask/note records, kept as data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The operation: `write`, `poll`, `read`, `delay` (the vocabulary is the table's, not ours).
    ///
    /// `delay` is the one that touches no register, and it earns its place rather than bending
    /// the rule: a dynamic memory's initialization has a settling period between starting its
    /// clock and commanding it, measured from the step BEFORE it, so it can neither be folded
    /// into a neighbor nor left to a comment. Its duration is a `$parameter`, because how long
    /// is a property of the fitted device and not of the block.
    pub op: String,
    /// The register the step touches.
    pub reg: String,
    /// A literal value, or a `$parameter` reference left symbolic.
    pub value: Option<StepValue>,
    /// The poll/read mask, when the step carries one.
    pub mask: Option<Int>,
    /// The masked value a `poll` waits for (absent = wait for any set bit; `0` = wait for clear).
    pub want: Option<Int>,
    /// A threshold a `poll` waits to drop below (the masked value < this -- e.g. a FIFO level).
    pub want_below: Option<Int>,
    /// The table's own annotation.
    pub note: Option<String>,
}

/// A step value: resolved integer or a named `$parameter`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepValue {
    /// A literal from the table.
    Literal(Int),
    /// A `$name` parameter the driver supplies at run time.
    Parameter(String),
}

/// A `[facts]` value: an integer, or a non-integer number carried as its source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    /// An integer fact.
    Int(Int),
    /// A non-integer fact (source spelling preserved -- documentation only, e.g. ENOB).
    Float(String),
}

/// One entry of a channel-indexed table's `[[channels]]` map: which input the mux index taps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// The AINSEL/mux index.
    pub index: i64,
    /// The signal source (a pad name like `GPIO26`, or `temperature_sensor`).
    pub source: String,
}

/// A declarative `[calibration.*]` record: named integer coefficients a conversion derives from
/// (the family's "no expression language" rule -- form + coefficients, never code). This is the
/// exact data a demo would otherwise hand-inline (the drift the single-source design targets).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calibration {
    /// The record name (the `[calibration.<name>]` key).
    pub name: String,
    /// The conversion form (e.g. `vbe-linear`) -- selects which integer formula consumes the
    /// coefficients.
    pub form: String,
    /// The integer coefficients, in table order (channel + the form's terms).
    pub coefficients: Vec<(String, Int)>,
}

/// The parsed table: every fact category, in table order.
#[derive(Clone, Debug, Default)]
pub struct Table {
    /// `[table] chip`.
    pub chip: String,
    /// `[table] peripheral`.
    pub peripheral: String,
    /// `[table] instance` (empty when the table declares none).
    pub instance: String,
    /// Region bases, in table order.
    pub regions: Vec<(String, Int)>,
    /// Registers, in table order.
    pub registers: Vec<Register>,
    /// Named constants, in table order.
    pub constants: Vec<(String, Int)>,
    /// Facts-as-data (`[facts]`): board/electrical facts a conversion reads, in table order.
    pub facts: Vec<(String, Fact)>,
    /// Channel-indexed input map (`[[channels]]`), in table order.
    pub channels: Vec<Channel>,
    /// Declarative calibration records (`[calibration.*]`), in table order.
    pub calibrations: Vec<Calibration>,
    /// Driver-supplied parameters: name -> the table's prose contract.
    pub parameters: Vec<(String, String)>,
    /// Named sequences (init, transfer_byte, ...) as ordered step lists.
    pub sequences: Vec<(String, Vec<Step>)>,
}

impl Table {
    /// The region base for `name`, when declared.
    pub fn region(&self, name: &str) -> Option<i64> {
        self.regions.iter().find(|(n, _)| n == name).map(|(_, v)| v.value)
    }

    /// The RESOLVED absolute address of register `name` (region base + offset).
    pub fn register_address(&self, name: &str) -> Option<i64> {
        let register = self.registers.iter().find(|r| r.name == name)?;
        Some(self.region(&register.region)? + register.offset.value)
    }

    /// The shifted mask of `register.field`, when both exist.
    pub fn field_mask(&self, register: &str, field: &str) -> Option<u64> {
        let register = self.registers.iter().find(|r| r.name == register)?;
        register.fields.iter().find(|f| f.name == field).map(Field::mask)
    }

    /// The named constant's value, when declared.
    pub fn constant(&self, name: &str) -> Option<i64> {
        self.constants.iter().find(|(n, _)| n == name).map(|(_, v)| v.value)
    }

    /// An integer fact's value, when declared as an integer.
    pub fn fact_int(&self, name: &str) -> Option<i64> {
        self.facts.iter().find(|(n, _)| n == name).and_then(|(_, f)| match f {
            Fact::Int(int) => Some(int.value),
            Fact::Float(_) => None,
        })
    }

    /// A calibration coefficient's value (`record.coefficient`), when declared.
    pub fn calibration_coefficient(&self, record: &str, coefficient: &str) -> Option<i64> {
        self.calibrations
            .iter()
            .find(|c| c.name == record)?
            .coefficients
            .iter()
            .find(|(n, _)| n == coefficient)
            .map(|(_, v)| v.value)
    }
}


pub(crate) fn err(line: usize, message: &str) -> String {
    format!("line {line}: {message}")
}

/// Splits a physical line into content and comment, honoring `#` inside quoted strings.
pub(crate) fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (at, ch) in line.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..at],
            _ => {}
        }
    }
    line
}

/// Whether the accumulated logical line still has unbalanced `[`/`{` brackets outside strings
/// (a multiline array/inline-table value continues on the next physical line).
pub(crate) fn brackets_open(text: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    for ch in text.chars() {
        match ch {
            '"' => in_string = !in_string,
            '[' | '{' if !in_string => depth += 1,
            ']' | '}' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// One parsed `key = value` right-hand side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RawValue {
    Str(String),
    Int(Int),
    /// A non-integer number, carried as its source text (exact spelling into the emission --
    /// documentation facts like ENOB, never register math).
    Float(String),
    /// An array of raw values (strings or ints; nested arrays appear inside inline tables only).
    Array(Vec<RawValue>),
    /// An inline table, keys in source order.
    Inline(Vec<(String, RawValue)>),
}

/// A cursor over one logical value string.
pub(crate) struct ValueCursor<'a> {
    pub(crate) text: &'a [u8],
    pub(crate) at: usize,
    pub(crate) line: usize,
}

impl<'a> ValueCursor<'a> {
    fn skip_space(&mut self) {
        while self.at < self.text.len()
            && (self.text[self.at] as char).is_whitespace()
        {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    pub(crate) fn parse(&mut self) -> Result<RawValue, String> {
        self.skip_space();
        match self.peek() {
            Some(b'"') => self.parse_string(),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_inline(),
            Some(_) => self.parse_int(),
            None => Err(err(self.line, "expected a value")),
        }
    }

    fn parse_string(&mut self) -> Result<RawValue, String> {
        self.at += 1;
        let start = self.at;
        while let Some(byte) = self.peek() {
            if byte == b'"' {
                let text = core::str::from_utf8(&self.text[start..self.at])
                    .map_err(|_| err(self.line, "non-utf8 string"))?;
                self.at += 1;
                return Ok(RawValue::Str(text.to_string()));
            }
            self.at += 1;
        }
        Err(err(self.line, "unterminated string"))
    }

    fn parse_int(&mut self) -> Result<RawValue, String> {
        let start = self.at;
        while let Some(byte) = self.peek() {
            if (byte as char).is_ascii_alphanumeric()
                || byte == b'_'
                || byte == b'-'
                || byte == b'x'
                || byte == b'.'
            {
                self.at += 1;
            } else {
                break;
            }
        }
        let text = core::str::from_utf8(&self.text[start..self.at])
            .map_err(|_| err(self.line, "non-utf8 number"))?
            .replace('_', "");
        if text.contains('.') && !text.starts_with("0x") {
            text.parse::<f64>().map_err(|_| err(self.line, &format!("bad number '{text}'")))?;
            return Ok(RawValue::Float(text));
        }
        let (value, hex) = if let Some(hex_digits) =
            text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
        {
            (i64::from_str_radix(hex_digits, 16), true)
        } else {
            (text.parse::<i64>(), false)
        };
        let value = value.map_err(|_| err(self.line, &format!("bad integer '{text}'")))?;
        Ok(RawValue::Int(Int { value, hex }))
    }

    fn parse_array(&mut self) -> Result<RawValue, String> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                Some(b']') => {
                    self.at += 1;
                    return Ok(RawValue::Array(items));
                }
                Some(b',') => {
                    self.at += 1;
                }
                Some(_) => items.push(self.parse()?),
                None => return Err(err(self.line, "unterminated array")),
            }
        }
    }

    fn parse_inline(&mut self) -> Result<RawValue, String> {
        self.at += 1;
        let mut entries = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                Some(b'}') => {
                    self.at += 1;
                    return Ok(RawValue::Inline(entries));
                }
                Some(b',') => {
                    self.at += 1;
                }
                Some(_) => {
                    let start = self.at;
                    while let Some(byte) = self.peek() {
                        if byte == b'=' || (byte as char).is_whitespace() {
                            break;
                        }
                        self.at += 1;
                    }
                    let key = core::str::from_utf8(&self.text[start..self.at])
                        .map_err(|_| err(self.line, "non-utf8 key"))?
                        .to_string();
                    self.skip_space();
                    if self.peek() != Some(b'=') {
                        return Err(err(self.line, "inline table entry needs '='"));
                    }
                    self.at += 1;
                    entries.push((key, self.parse()?));
                }
                None => return Err(err(self.line, "unterminated inline table")),
            }
        }
    }
}

/// Which section of the table the reader is inside.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Section {
    None,
    Table,
    Regions,
    Register(String),
    Constants,
    Facts,
    /// A documentation sub-table (`[facts.notes]`) -- parsed and ignored.
    Ignored,
    /// The most recent `[[channels]]` entry.
    Channel,
    Calibration(String),
    Parameters,
    Sequence(String),
}

/// Parses one peripheral table from its text. Errors carry a line number and are LOUD --
/// an unrecognized construct is a reader gap to fix, never something to skip silently.
pub fn parse(text: &str) -> Result<Table, String> {
    let mut table = Table::default();
    let mut section = Section::None;
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
            if header == "channels" {
                table.channels.push(Channel { index: 0, source: String::new() });
                section = Section::Channel;
                continue;
            }
            let Some(name) = header.strip_prefix("sequences.") else {
                return Err(err(line_number, &format!("unknown array-of-tables '{header}'")));
            };
            if table.sequences.last().is_none_or(|(n, _)| n != name) {
                table.sequences.push((name.to_string(), Vec::new()));
            }
            table
                .sequences
                .last_mut()
                .expect("just ensured")
                .1
                .push(Step {
                    op: String::new(),
                    reg: String::new(),
                    value: None,
                    mask: None,
                    want: None,
                    want_below: None,
                    note: None,
                });
            section = Section::Sequence(name.to_string());
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = match header {
                "table" => Section::Table,
                "regions" => Section::Regions,
                "constants" => Section::Constants,
                "facts" => Section::Facts,
                "facts.notes" => Section::Ignored,
                "parameters" => Section::Parameters,
                _ => {
                    if let Some(name) = header.strip_prefix("registers.") {
                        table.registers.push(Register {
                            name: name.to_string(),
                            region: String::new(),
                            offset: Int { value: 0, hex: false },
                            fields: Vec::new(),
                        });
                        Section::Register(name.to_string())
                    } else if let Some(name) = header.strip_prefix("calibration.") {
                        table.calibrations.push(Calibration {
                            name: name.to_string(),
                            form: String::new(),
                            coefficients: Vec::new(),
                        });
                        Section::Calibration(name.to_string())
                    } else {
                        return Err(err(line_number, &format!("unknown section '{header}'")));
                    }
                }
            };
            continue;
        }

        let Some((key, rest)) = line.split_once('=') else {
            return Err(err(line_number, &format!("expected 'key = value', got '{line}'")));
        };
        let key = key.trim();
        let mut cursor = ValueCursor { text: rest.trim().as_bytes(), at: 0, line: line_number };
        let value = cursor.parse()?;

        match &section {
            Section::None => return Err(err(line_number, "key outside any section")),
            Section::Table => match (key, &value) {
                ("chip", RawValue::Str(s)) => table.chip = s.clone(),
                ("peripheral", RawValue::Str(s)) => table.peripheral = s.clone(),
                ("instance", RawValue::Str(s)) => table.instance = s.clone(),
                ("sources" | "notes", RawValue::Array(_)) => {}
                _ => return Err(err(line_number, &format!("unexpected [table] key '{key}'"))),
            },
            Section::Regions => match value {
                RawValue::Int(int) => table.regions.push((key.to_string(), int)),
                _ => return Err(err(line_number, "region base must be an integer")),
            },
            Section::Constants => match value {
                RawValue::Int(int) => table.constants.push((key.to_string(), int)),
                _ => return Err(err(line_number, "constant must be an integer")),
            },
            Section::Facts => match value {
                RawValue::Int(int) => table.facts.push((key.to_string(), Fact::Int(int))),
                RawValue::Float(text) => table.facts.push((key.to_string(), Fact::Float(text))),
                _ => return Err(err(line_number, "fact must be a number")),
            },
            Section::Ignored => {}
            Section::Channel => {
                let channel = table.channels.last_mut().expect("section channel exists");
                match (key, value) {
                    ("index", RawValue::Int(int)) => channel.index = int.value,
                    ("source", RawValue::Str(s)) => channel.source = s,
                    ("enable", RawValue::Str(_)) => {}
                    (other, _) => {
                        return Err(err(line_number, &format!("unexpected channel key '{other}'")));
                    }
                }
            }
            Section::Calibration(name) => {
                let record = table
                    .calibrations
                    .iter_mut()
                    .find(|c| &c.name == name)
                    .expect("section calibration exists");
                match (key, value) {
                    ("form", RawValue::Str(s)) => record.form = s,
                    ("notes", RawValue::Str(_)) => {}
                    (coefficient, RawValue::Int(int)) => {
                        record.coefficients.push((coefficient.to_string(), int));
                    }
                    (other, _) => {
                        return Err(err(line_number, &format!("calibration '{other}' must be an integer coefficient")));
                    }
                }
            }
            Section::Parameters => match value {
                RawValue::Str(text) => table.parameters.push((key.to_string(), text)),
                _ => return Err(err(line_number, "parameter contract must be a string")),
            },
            Section::Register(name) => {
                let register = table
                    .registers
                    .iter_mut()
                    .find(|r| &r.name == name)
                    .expect("section register exists");
                match (key, value) {
                    ("region", RawValue::Str(s)) => register.region = s,
                    ("offset", RawValue::Int(int)) => register.offset = int,
                    ("notes", RawValue::Str(_)) => {}
                    ("fields", RawValue::Inline(entries)) => {
                        for (field_name, spec) in entries {
                            let RawValue::Array(parts) = spec else {
                                return Err(err(line_number, "field spec must be [lsb, width]"));
                            };
                            let ints: Vec<i64> = parts
                                .iter()
                                .filter_map(|p| match p {
                                    RawValue::Int(i) => Some(i.value),
                                    _ => None,
                                })
                                .collect();
                            if ints.len() != 2 {
                                return Err(err(line_number, "field spec must be [lsb, width]"));
                            }
                            register.fields.push(Field {
                                name: field_name,
                                lsb: ints[0] as u32,
                                width: ints[1] as u32,
                            });
                        }
                    }
                    (other, _) => {
                        return Err(err(line_number, &format!("unexpected register key '{other}'")));
                    }
                }
            }
            Section::Sequence(name) => {
                let step = table
                    .sequences
                    .iter_mut()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, steps)| steps.last_mut())
                    .expect("section step exists");
                match (key, value) {
                    ("op", RawValue::Str(s)) => step.op = s,
                    ("reg", RawValue::Str(s)) => step.reg = s,
                    ("value", RawValue::Int(int)) => step.value = Some(StepValue::Literal(int)),
                    ("value", RawValue::Str(s)) => match s.strip_prefix('$') {
                        Some(param) => step.value = Some(StepValue::Parameter(param.to_string())),
                        None => {
                            return Err(err(line_number, "string step value must be a $parameter"));
                        }
                    },
                    ("mask", RawValue::Int(int)) => step.mask = Some(int),
                    ("want", RawValue::Int(int)) => step.want = Some(int),
                    ("want_below", RawValue::Int(int)) => step.want_below = Some(int),
                    ("note", RawValue::Str(s)) => step.note = Some(s),
                    (other, _) => {
                        return Err(err(line_number, &format!("unexpected step key '{other}'")));
                    }
                }
            }
        }
    }

    if table.chip.is_empty() || table.peripheral.is_empty() {
        return Err("[table] must declare chip and peripheral".to_string());
    }
    Ok(table)
}


/// `rp2350` -> `Rp2350`, `spi` -> `Spi` (identifier casing for the generated class name).
pub(crate) fn pascal(text: &str) -> String {
    let mut out = String::new();
    let mut upper_next = true;
    for ch in text.chars() {
        if ch == '-' || ch == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn format_int(int: Int) -> String {
    if int.hex {
        format!("0x{:X}", int.value)
    } else {
        format!("{}", int.value)
    }
}

/// The generated facts class name for a table: chip + peripheral + `Facts`, Pascal-cased
/// (`Rp2350SpiFacts`). The `Facts` suffix keeps the generated class from ever colliding with a
/// driver of the same chip+peripheral name (`Rp2040Uart`, `Esp32C6Uart`, ... have no `Driver`
/// suffix), so a BSP can `using Lamella.Generated;` without ambiguity.
pub fn class_name(table: &Table) -> String {
    format!("{}{}Facts", pascal(&table.chip), pascal(&table.peripheral))
}

/// The repo-relative output path for a table's C# emission: `bsp/<chip>/csharp/<Class>.g.cs`.
pub fn csharp_path(table: &Table) -> String {
    format!("bsp/{}/csharp/{}.g.cs", table.chip, class_name(table))
}

/// Emits the C# flat-facts binding: region bases, RESOLVED register addresses, field
/// masks/shifts, and named constants -- every value a single literal, in table order.
/// `source` names the table file in the generated header (repo-relative).
pub fn emit_csharp(table: &Table, source: &str) -> Result<String, String> {
    let class = class_name(table);
    let mut out = String::new();
    out.push_str(&format!(
        "// GENERATED by lamella-bsp-gen from {source} -- DO NOT EDIT.\n\
         // Regenerate: cargo run -p lamella-bsp-gen -- gen {source} bsp\n\
         //\n\
         // The flat register facts of the {chip} {peripheral}{instance}: region bases, RESOLVED\n\
         // register addresses (region + offset computed at generation time -- every value below is\n\
         // a single literal, never spelled in terms of another const), field masks (shifted, ready\n\
         // for register math) with their _LSB shifts, and the table's named constants. Sequences\n\
         // stay in the table; drivers hand-execute them with their own bounded polls.\n\
         namespace Lamella.Generated\n\
         {{\n\
         \x20   public sealed class {class}\n\
         \x20   {{\n\
         \x20       private {class}() {{ }}\n",
        source = source,
        chip = table.chip,
        peripheral = table.peripheral,
        instance = if table.instance.is_empty() {
            String::new()
        } else {
            format!(" (instance {})", table.instance)
        },
        class = class,
    ));

    out.push_str("\n        // -- region bases --\n");
    for (name, base) in &table.regions {
        out.push_str(&format!("        public const uint {name} = {};\n", format_int(*base)));
    }

    out.push_str("\n        // -- registers (resolved absolute addresses) --\n");
    for register in &table.registers {
        let Some(address) = table.register_address(&register.name) else {
            return Err(format!(
                "register {} names unknown region {}",
                register.name, register.region
            ));
        };
        out.push_str(&format!(
            "        public const uint {} = 0x{:08X};\n",
            register.name, address
        ));
    }

    out.push_str("\n        // -- fields: <REG>_<FIELD> = the shifted mask; _LSB = the shift --\n");
    for register in &table.registers {
        for field in &register.fields {
            out.push_str(&format!(
                "        public const uint {reg}_{field} = 0x{mask:X};\n\
                 \x20       public const uint {reg}_{field}_LSB = {lsb};\n",
                reg = register.name,
                field = field.name,
                mask = field.mask(),
                lsb = field.lsb,
            ));
        }
    }

    if !table.constants.is_empty() {
        out.push_str("\n        // -- named constants --\n");
        for (name, value) in &table.constants {
            let kind = if value.value < 0 { "int" } else { "uint" };
            out.push_str(&format!(
                "        public const {kind} {name} = {};\n",
                format_int(*value)
            ));
        }
    }

    if !table.facts.is_empty() {
        out.push_str("\n        // -- facts as data (board/electrical facts conversions read) --\n");
        for (name, fact) in &table.facts {
            match fact {
                Fact::Int(value) => {
                    let kind = if value.value < 0 { "int" } else { "uint" };
                    out.push_str(&format!(
                        "        public const {kind} {} = {};\n",
                        pascal(name),
                        format_int(*value)
                    ));
                }
                Fact::Float(text) => {
                    out.push_str(&format!("        public const double {} = {text};\n", pascal(name)));
                }
            }
        }
    }

    if !table.channels.is_empty() {
        out.push_str("\n        // -- channel map: Channel_<source> = the mux/AINSEL index --\n");
        for channel in &table.channels {
            out.push_str(&format!(
                "        public const int Channel_{} = {};\n",
                pascal(&channel.source),
                channel.index
            ));
        }
    }

    for record in &table.calibrations {
        out.push_str(&format!(
            "\n        // -- calibration '{}' (form: {}); integer coefficients, no hardcoding downstream --\n",
            record.name, record.form
        ));
        for (coefficient, value) in &record.coefficients {
            let kind = if value.value < 0 { "int" } else { "uint" };
            out.push_str(&format!(
                "        public const {kind} {}_{} = {};\n",
                pascal(&record.name),
                pascal(coefficient),
                format_int(*value)
            ));
        }
    }

    out.push_str("    }\n}\n");

    let mut seen = std::collections::HashSet::new();
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix("public const ") {
            if let Some(name) = rest.split_whitespace().nth(1) {
                if !seen.insert(name.to_string()) {
                    return Err(format!(
                        "duplicate emitted constant '{name}': a [constants] entry collides with a \
                         generated field/register name -- drop the redundant one from {}",
                        class_name(table)
                    ));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# a comment
[table]
chip = "rp2350"
peripheral = "spi"
instance = "spi0"
notes = [
    "multiline",
    "array",
]

[regions]
SPI0 = 0x40080000
RESETS = 0x40020000

[registers.SSPCR0]
region = "SPI0"
offset = 0x0
fields = { DSS = [0, 4], SCR = [8, 8] }
notes = "prose"

[registers.RESETS_DONE]
region = "RESETS"
offset = 0x8
fields = { SPI0 = [18, 1] }

[constants]
SSPCLK_HZ = 12000000

[parameters]
tx_byte = "the byte"

[[sequences.init]]
op = "write"
reg = "SSPCR0"
value = 0x7
note = "configure"

[[sequences.init]]
op = "poll"
reg = "RESETS_DONE"
mask = 0x40240

[[sequences.transfer_byte]]
op = "write"
reg = "SSPCR0"
value = "$tx_byte"
"#;

    #[test]
    fn parses_the_table_dialect() {
        let table = parse(SAMPLE).expect("parses");
        assert_eq!(table.chip, "rp2350");
        assert_eq!(table.peripheral, "spi");
        assert_eq!(table.instance, "spi0");
        assert_eq!(table.region("SPI0"), Some(0x4008_0000));
        assert_eq!(table.register_address("SSPCR0"), Some(0x4008_0000));
        assert_eq!(table.register_address("RESETS_DONE"), Some(0x4002_0008));
        assert_eq!(table.field_mask("SSPCR0", "DSS"), Some(0xF));
        assert_eq!(table.field_mask("SSPCR0", "SCR"), Some(0xFF00));
        assert_eq!(table.field_mask("RESETS_DONE", "SPI0"), Some(0x4_0000));
        assert_eq!(table.constant("SSPCLK_HZ"), Some(12_000_000));
        assert_eq!(table.parameters, vec![("tx_byte".to_string(), "the byte".to_string())]);
        let (name, steps) = &table.sequences[0];
        assert_eq!(name, "init");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].op, "write");
        assert_eq!(steps[0].value, Some(StepValue::Literal(Int { value: 7, hex: true })));
        assert_eq!(steps[1].mask, Some(Int { value: 0x40240, hex: true }));
        let (name, steps) = &table.sequences[1];
        assert_eq!(name, "transfer_byte");
        assert_eq!(steps[0].value, Some(StepValue::Parameter("tx_byte".to_string())));
    }

    #[test]
    fn emits_resolved_literals_and_shifted_masks() {
        let table = parse(SAMPLE).expect("parses");
        assert_eq!(class_name(&table), "Rp2350SpiFacts");
        assert_eq!(csharp_path(&table), "bsp/rp2350/csharp/Rp2350SpiFacts.g.cs");
        let emitted = emit_csharp(&table, "spi-rp2350.toml").expect("emits");
        assert!(emitted.contains("public const uint SSPCR0 = 0x40080000;"));
        assert!(emitted.contains("public const uint RESETS_DONE = 0x40020008;"));
        assert!(emitted.contains("public const uint SSPCR0_SCR = 0xFF00;"));
        assert!(emitted.contains("public const uint SSPCR0_SCR_LSB = 8;"));
        assert!(emitted.contains("public const uint SSPCLK_HZ = 12000000;"));
        assert!(emitted.contains("DO NOT EDIT"));
        assert!(emitted.contains("public const uint SPI0 = 0x40080000;"));
    }

    const SAMPLE_ADC: &str = r#"
[table]
chip = "rp2350"
peripheral = "adc"
instance = "adc"

[regions]
ADC = 0x400a0000

[registers.CS]
region = "ADC"
offset = 0x0
fields = { EN = [0, 1], AINSEL = [12, 4] }

[facts]
resolution_bits = 12
enob_typical = 9.5
reference_uv = 3300000

[facts.notes]
reference_uv = "a board fact, ignored by the emitter"

[[channels]]
index = 0
source = "GPIO26"

[[channels]]
index = 4
source = "temperature_sensor"
enable = "CS.TS_EN"

[calibration.temperature_sensor]
channel = 4
form = "vbe-linear"
t0_millicelsius = 27000
slope_microvolts_per_celsius = -1721
notes = "typical values"
"#;

    #[test]
    fn parses_facts_channels_and_calibration() {
        let table = parse(SAMPLE_ADC).expect("parses");
        assert_eq!(table.fact_int("resolution_bits"), Some(12));
        assert_eq!(table.fact_int("reference_uv"), Some(3_300_000));
        assert_eq!(table.fact_int("enob_typical"), None);
        assert!(matches!(
            table.facts.iter().find(|(n, _)| n == "enob_typical").map(|(_, f)| f),
            Some(Fact::Float(_))
        ));
        assert_eq!(table.channels.len(), 2);
        assert_eq!(table.channels[1].index, 4);
        assert_eq!(table.channels[1].source, "temperature_sensor");
        assert_eq!(table.calibration_coefficient("temperature_sensor", "channel"), Some(4));
        assert_eq!(table.calibration_coefficient("temperature_sensor", "t0_millicelsius"), Some(27000));
        assert_eq!(
            table.calibration_coefficient("temperature_sensor", "slope_microvolts_per_celsius"),
            Some(-1721)
        );
        assert_eq!(table.calibrations[0].form, "vbe-linear");
    }

    #[test]
    fn emits_adc_facts_channels_and_calibration() {
        let table = parse(SAMPLE_ADC).expect("parses");
        let out = emit_csharp(&table, "adc-rp2350.toml").expect("emits");
        assert!(out.contains("public const uint ResolutionBits = 12;"));
        assert!(out.contains("public const double EnobTypical = 9.5;"));
        assert!(out.contains("public const uint ReferenceUv = 3300000;"));
        assert!(out.contains("public const int Channel_TemperatureSensor = 4;"));
        assert!(out.contains("public const uint TemperatureSensor_T0Millicelsius = 27000;"));
        assert!(out.contains("public const int TemperatureSensor_SlopeMicrovoltsPerCelsius = -1721;"));
    }

    #[test]
    fn loud_on_a_constant_colliding_with_a_field_name() {
        let table = "[table]\nchip = \"c\"\nperipheral = \"p\"\n\
                     [regions]\nR = 0x1000\n\
                     [registers.SHORTS]\nregion = \"R\"\noffset = 0x0\nfields = { BB = [0, 1] }\n\
                     [constants]\nSHORTS_BB = 0x1\n";
        let parsed = parse(table).expect("parses");
        let emitted = emit_csharp(&parsed, "t.toml");
        assert!(emitted.is_err(), "expected a duplicate-constant error, got {emitted:?}");
        assert!(emitted.unwrap_err().contains("duplicate emitted constant 'SHORTS_BB'"));
    }

    #[test]
    fn loud_on_unknown_constructs() {
        assert!(parse("[mystery]\nkey = 1\n").is_err());
        assert!(parse("[table]\nchip = \"c\"\nperipheral = \"p\"\n[regions]\nX = \"nope\"\n").is_err());
        let bad = "[table]\nchip = \"c\"\nperipheral = \"p\"\n[[sequences.s]]\nop = \"write\"\nreg = \"R\"\nvalue = \"raw\"\n";
        assert!(parse(bad).is_err());
    }
}
