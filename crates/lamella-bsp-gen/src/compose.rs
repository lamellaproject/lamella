//! Composes an ADAPTER extension onto a board: each module fitted in one of the adapter's sockets
//! is resolved through the adapter's wiring to the board's header, and so to the board's pads, with
//! what serves each line and which of the board's own bindings it takes a pad from.
//!
//! THREE FILES, TWO JOINS, AND NONE OF THEM NAMES ANOTHER. A module names positions of its socket's
//! standard (`cs`); the adapter wires each socket position to a position of the header it plugs
//! into (`gp13`); the board's row for that header names the pad (`GP13`). The adapter's revision
//! is a fact about one physical unit, so it arrives with the build rather than from any file here.
//!
//! A LINE IS SHARED WHERE THE BOARD ALREADY BINDS WHAT IT NEEDS, AND DISPLACES WHAT IS THERE WHERE
//! IT DOES NOT. A module's I2C on the pads the board binds as its I2C bus rides that binding, as a
//! second device on the bus would. A module's chip select on a pad the board gives to a PWM output
//! takes the pad for the build, and the report names the PWM output it took the pad from -- a board
//! default is a default, and fitting a module is how a user overrides one.
//!
//! WHAT IS REFUSED, AND WHY EACH REFUSAL NAMES BOTH SIDES. A pad two claimants want for different
//! things cannot serve both, and a refusal that named only the second would leave the first to be
//! found by hand: a line of the build's link carrier and a module's line on one pad; two modules'
//! lines on one pad for different functions; a module's line on a pad the board's own device holds.
//! A bus a module needs that no binding and no peripheral cell on those pads can serve is refused
//! with the pads' own facts, so a wiring the chip's hardware cannot drive says so rather than
//! reading as a board that forgot a binding.

use crate::strata::{
    rp_adc_channel_for_pad, Carrier, Connector, ExtensionSocket, ExtensionTable, FamilySet, ResolvedBoard,
    SocketWiring,
};

/// A module plugged into one of an adapter's sockets.
#[derive(Clone, Copy, Debug)]
pub struct Fitted<'a> {
    /// The adapter's socket it is plugged into (`socket-2`).
    pub socket: &'a str,
    /// The module: an extension of the socket's standard.
    pub module: &'a ExtensionTable,
}

/// What a build knows before any module is placed.
#[derive(Clone, Copy, Debug)]
pub struct Build<'a> {
    /// The board's chip family.
    pub set: &'a FamilySet,
    /// The board, resolved against its family.
    pub board: &'a ResolvedBoard,
    /// The carrier this build's link rides. A pad its binding routes belongs to the link.
    pub carrier: &'a Carrier,
    /// The adapter fitted to the board's header.
    pub adapter: &'a ExtensionTable,
    /// The adapter unit's revision, as the unit's profile names it; `None` when it names none.
    pub revision: Option<&'a str>,
}

/// What serves one of a module's lines on the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Serves {
    /// A binding the board states, shared with the module: its role, and its signal on the pad.
    Bound {
        /// The binding's role (`i2c0`).
        role: String,
        /// Its signal on the pad (`sda`).
        signal: String,
    },
    /// A peripheral the board binds nothing to on this pad, taken for the module from the chip's
    /// pin map.
    Added {
        /// The peripheral instance (`uart1`).
        instance: String,
        /// The mux function (`F2`).
        function: String,
        /// The cell's signal (`tx`).
        signal: String,
    },
    /// A plain GPIO.
    Gpio,
    /// A channel of the board's converter.
    Analog {
        /// The adc binding's role.
        role: String,
        /// The channel.
        channel: i64,
    },
}

/// One line of a fitted module, resolved to a pad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedLine {
    /// The adapter socket (`socket-2`).
    pub socket: String,
    /// The module's extension id.
    pub module: String,
    /// What the module uses the line for, in its own terms (`bus 'serial' tx`, `device 'led'`).
    pub user: String,
    /// The socket position (`tx`).
    pub position: String,
    /// The adapter's host position the socket position is wired to (`gp8`).
    pub host: String,
    /// The board pad (`GP8`).
    pub pad: String,
    /// What serves the line.
    pub serves: Serves,
}

impl ComposedLine {
    /// The line as a claimant names it: `socket-2's tx (module 'x', bus 'serial' tx)`.
    #[must_use]
    pub fn claimant(&self) -> String {
        format!("{}'s {} (module '{}', {})", self.socket, self.position, self.module, self.user)
    }
}

/// A board binding that loses a pad to a fitted module for this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Displaced {
    /// The binding's role (`pwm4`).
    pub role: String,
    /// Its signal on the pad (`a`).
    pub signal: String,
    /// The pad (`GP8`).
    pub pad: String,
    /// The line that takes it, as [`ComposedLine::claimant`] names it.
    pub by: String,
}

/// A build's modules, resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Composition {
    /// Every line a fitted module uses, in fitting order.
    pub lines: Vec<ComposedLine>,
    /// The board bindings that lose a pad, in the order the lines take them.
    pub displaced: Vec<Displaced>,
}

impl Composition {
    /// The line a module in `socket` uses at `position`.
    #[must_use]
    pub fn line(&self, socket: &str, position: &str) -> Option<&ComposedLine> {
        self.lines.iter().find(|line| line.socket == socket && line.position == position)
    }

    /// The build report: a line for each peripheral a module adds, then one for each board binding
    /// a module displaces, naming both sides.
    #[must_use]
    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();
        for line in &self.lines {
            if let Serves::Added { instance, function, signal } = &line.serves {
                out.push(format!("{} takes {} as {instance} {signal} ({function})", line.claimant(), line.pad));
            }
        }
        for displaced in &self.displaced {
            out.push(format!(
                "{} displaces binding '{}' ({} on {})",
                displaced.by, displaced.role, displaced.signal, displaced.pad
            ));
        }
        out
    }
}

/// What a module needs of one socket position.
enum Need {
    /// A line of a bus: its kind and the bus's own name for the line (`mosi`).
    Bus { kind: String, signal: String, bus: String },
    /// An SPI bus's chip select, which a GPIO serves wherever the bus's own select is elsewhere.
    ChipSelect,
    Gpio,
    Pwm,
    Analog,
}

struct Use {
    position: String,
    user: String,
    need: Need,
}

/// Every socket position a module uses, and what it needs there.
fn uses(module: &ExtensionTable) -> Vec<Use> {
    let mut out = Vec::new();
    for bus in &module.buses {
        for (line, position) in &bus.lines {
            let need = if bus.kind == "spi" && line == "ss" {
                Need::ChipSelect
            } else {
                Need::Bus { kind: bus.kind.clone(), signal: line.clone(), bus: bus.role.clone() }
            };
            out.push(Use { position: position.clone(), user: format!("bus '{}' {line}", bus.role), need });
        }
    }
    for device in &module.devices {
        if device.signal.is_empty() {
            continue;
        }
        let need = match device.kind.as_str() {
            "pwm-out" => Need::Pwm,
            "analog-in" => Need::Analog,
            _ => Need::Gpio,
        };
        out.push(Use { position: device.signal.clone(), user: format!("device '{}'", device.name), need });
    }
    out
}

/// The pin-map cell a bus line needs on the rp families: the instance prefix of its peripheral and
/// the cell's signal. The PL022's transmit is the master's MOSI and its receive the MISO.
fn rp_cell(kind: &str, signal: &str) -> Option<(&'static str, &'static str)> {
    match (kind, signal) {
        ("uart", "tx") => Some(("uart", "tx")),
        ("uart", "rx") => Some(("uart", "rx")),
        ("i2c", "sda") => Some(("i2c", "sda")),
        ("i2c", "scl") => Some(("i2c", "scl")),
        ("spi", "mosi") => Some(("spi", "tx")),
        ("spi", "miso") => Some(("spi", "rx")),
        ("spi", "sck") => Some(("spi", "sclk")),
        _ => None,
    }
}

/// True when `instance` is an instance of the `prefix` peripheral (`uart1` of `uart`).
fn instance_of(instance: &str, prefix: &str) -> bool {
    instance.strip_prefix(prefix).is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
}

/// The board's socket the adapter plugs into: its one connector of the adapter's standard.
fn header<'a>(build: &Build<'a>) -> Result<&'a Connector, String> {
    let board = &build.board.board;
    let found: Vec<&Connector> =
        board.connectors.iter().filter(|c| c.standard == build.adapter.standard).collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "board {} offers no {} socket, so {} does not fit it",
            board.board, build.adapter.standard, build.adapter.extension
        )),
        many => Err(format!(
            "board {} offers {} {} sockets ({}), and which one {} is fitted to is not named",
            board.board,
            many.len(),
            build.adapter.standard,
            many.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "),
            build.adapter.extension
        )),
    }
}

/// What serves `need` on `pad`. `bus_pads` holds every line of the need's bus with its pad, so a
/// binding that serves the whole bus is preferred to one that serves this line alone.
fn serve(build: &Build<'_>, need: &Need, pad: &str, line: &str, bus_pads: &[(String, String)]) -> Result<Serves, String> {
    let board = &build.board;
    let revision = build.revision.map(|r| format!(" on revision {r}")).unwrap_or_default();
    match need {
        Need::Gpio => Ok(Serves::Gpio),
        Need::ChipSelect => Ok(board
            .bindings
            .iter()
            .find(|b| b.kind == "spi" && b.pins.iter().any(|(signal, pin)| signal == "cs" && pin.pin == pad))
            .map_or(Serves::Gpio, |b| Serves::Bound { role: b.role.clone(), signal: "cs".to_string() })),
        Need::Bus { kind, signal, bus } => {
            let states = |b: &&crate::strata::Binding, signal: &str, pad: &str| {
                b.kind == *kind && b.pins.iter().any(|(s, pin)| s == signal && pin.pin == pad)
            };
            let whole = board
                .bindings
                .iter()
                .find(|b| !bus_pads.is_empty() && bus_pads.iter().all(|(s, p)| states(b, s, p)));
            if let Some(binding) = whole.or_else(|| board.bindings.iter().find(|b| states(b, signal, pad))) {
                return Ok(Serves::Bound { role: binding.role.clone(), signal: signal.clone() });
            }
            let family = &build.set.family;
            if family != "rp2040" && family != "rp2350" {
                return Err(format!(
                    "{line}: the board binds no {kind} {signal} on {pad}, and a peripheral cell is resolved on the rp2040 and rp2350 families only, not {family}"
                ));
            }
            let Some((prefix, cell_signal)) = rp_cell(kind, signal) else {
                return Err(format!("{line}: a {kind} bus has no line '{signal}'"));
            };
            let cells: Vec<_> =
                build.set.pins.rows.iter().filter(|row| row.pin == pad && instance_of(&row.instance, prefix)).collect();
            if let Some(cell) = cells.iter().find(|row| row.signal == cell_signal) {
                return Ok(Serves::Added {
                    instance: cell.instance.clone(),
                    function: cell.function.clone(),
                    signal: cell.signal.clone(),
                });
            }
            if let Some(cell) = cells.first() {
                let hook = if kind == "spi" {
                    format!(
                        ". A software or PIO SPI can drive these pads: a binding of kind spi that states {pad} as {signal} would serve bus '{bus}', and none does"
                    )
                } else {
                    String::new()
                };
                return Err(format!(
                    "{line}: hardware {kind} cannot serve {signal} on {pad}{revision}, whose {kind} cell is {} {} ({}) -- the peripheral's other direction{hook}",
                    cell.instance, cell.signal, cell.function
                ));
            }
            Err(format!(
                "{line}: nothing serves {signal} on {pad} -- the board binds no {kind} there and the {family} pin map states no {kind} cell on it"
            ))
        }
        Need::Pwm => {
            if let Some((binding, signal)) = board.bindings.iter().filter(|b| b.kind == "pwm").find_map(|b| {
                b.pins.iter().find(|(_, pin)| pin.pin == pad).map(|(signal, _)| (b, signal))
            }) {
                return Ok(Serves::Bound { role: binding.role.clone(), signal: signal.clone() });
            }
            match build.set.pins.rows.iter().find(|row| row.pin == pad && row.instance == "pwm") {
                Some(cell) => Ok(Serves::Added {
                    instance: cell.instance.clone(),
                    function: cell.function.clone(),
                    signal: cell.signal.clone(),
                }),
                None => Err(format!("{line}: no PWM output reaches {pad} -- the board binds none there and the pin map states no pwm cell on it")),
            }
        }
        Need::Analog => {
            let Some(adc) = board.bindings.iter().find(|b| b.kind == "adc") else {
                return Err(format!("{line}: board {} binds no converter, so nothing digitizes {pad}", board.board.board));
            };
            if build.set.family != "rp2350" {
                return Err(format!(
                    "{line}: a converter channel is resolved on the rp2350 family only, not {}",
                    build.set.family
                ));
            }
            match rp_adc_channel_for_pad(build.set, board, pad)? {
                Some((channel, None)) => Ok(Serves::Analog { role: adc.role.clone(), channel }),
                Some((channel, Some(owner))) => Err(format!(
                    "{line}: {pad} reaches converter channel {channel}, which the board reserves for {owner}"
                )),
                None => Err(format!("{line}: no channel of the board's converter reads {pad}")),
            }
        }
    }
}

/// Composes the fitted modules onto the build's board through its adapter.
pub fn compose(build: &Build<'_>, fitted: &[Fitted<'_>]) -> Result<Composition, String> {
    let adapter = &build.adapter.extension;
    if let Some(revision) = build.revision {
        if !build.adapter.revisions.iter().any(|r| r.name == revision) {
            let known: Vec<&str> = build.adapter.revisions.iter().map(|r| r.name.as_str()).collect();
            return Err(format!(
                "{adapter} has no revision '{revision}' -- it declares {}",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            ));
        }
    }
    let header = header(build)?;
    let board = &build.board.board.board;

    let mut out = Composition::default();
    for fit in fitted {
        let socket: &ExtensionSocket = build.adapter.socket(fit.socket).ok_or_else(|| {
            format!(
                "{adapter} has no socket '{}' -- it carries {}",
                fit.socket,
                build.adapter.sockets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            )
        })?;
        let module = &fit.module.extension;
        if fit.module.standard != socket.standard {
            return Err(format!(
                "module '{module}' plugs into a {} socket, and {adapter}'s {} is a {} socket",
                fit.module.standard, socket.name, socket.standard
            ));
        }
        if let Some(other) = out.lines.iter().find(|line| line.socket == socket.name && line.module != *module) {
            return Err(format!(
                "{adapter}'s {} holds one module, and both '{}' and '{module}' are fitted to it",
                socket.name, other.module
            ));
        }
        let mut placed: Vec<(Use, String, String, String)> = Vec::new();
        for usage in uses(fit.module) {
            let line = format!("{}'s {} (module '{module}', {})", socket.name, usage.position, usage.user);
            let host = match socket.wiring(&usage.position, build.revision) {
                SocketWiring::Wired(host) => host.to_string(),
                SocketWiring::Absent => {
                    return Err(format!("{line}: {adapter}'s {} does not wire position {}", socket.name, usage.position));
                }
                SocketWiring::ByRevision(rows) => {
                    return Err(format!(
                        "{line}: {adapter} wires {} by revision ({}), and the unit's revision is not named -- its profile names the revision it is",
                        usage.position,
                        rows.iter().map(|(revision, host)| format!("{revision}: {host}")).collect::<Vec<_>>().join(", ")
                    ));
                }
            };
            let pad = header
                .pins
                .iter()
                .find(|row| row.signal == host)
                .map(|row| row.pin.clone())
                .ok_or_else(|| {
                    format!("{line}: board {board}'s {} socket does not bring out {host}", header.name)
                })?;
            placed.push((usage, line, host, pad));
        }
        for (usage, line, host, pad) in &placed {
            let bus_pads: Vec<(String, String)> = match &usage.need {
                Need::Bus { bus, .. } => placed
                    .iter()
                    .filter_map(|(other, _, _, pad)| match &other.need {
                        Need::Bus { bus: b, signal, .. } if b == bus => Some((signal.clone(), pad.clone())),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let serves = serve(build, &usage.need, pad, line, &bus_pads)?;
            out.lines.push(ComposedLine {
                socket: socket.name.clone(),
                module: module.clone(),
                user: usage.user.clone(),
                position: usage.position.clone(),
                host: host.clone(),
                pad: pad.clone(),
                serves,
            });
        }
    }
    check_buses(fitted, &out)?;
    check_claims(build, &out)?;
    out.displaced = displaced(build, &out);
    Ok(out)
}

/// Every line of one module bus is served by one peripheral: a bus split across two would be two
/// half-buses, and neither would carry a transfer.
fn check_buses(fitted: &[Fitted<'_>], out: &Composition) -> Result<(), String> {
    for fit in fitted {
        for bus in &fit.module.buses {
            let served: Vec<&ComposedLine> = out
                .lines
                .iter()
                .filter(|line| {
                    line.socket == fit.socket
                        && line.user.starts_with(&format!("bus '{}' ", bus.role))
                        && !matches!(line.serves, Serves::Gpio)
                })
                .collect();
            let peripheral = |line: &ComposedLine| match &line.serves {
                Serves::Bound { role, .. } => format!("binding '{role}'"),
                Serves::Added { instance, .. } => format!("{instance}, added"),
                _ => String::new(),
            };
            if let Some(first) = served.first() {
                if let Some(other) = served.iter().find(|line| peripheral(line) != peripheral(first)) {
                    return Err(format!(
                        "module '{}': bus '{}' in {} would span two peripherals -- {} on {} is {}, and {} on {} is {}",
                        fit.module.extension,
                        bus.role,
                        fit.socket,
                        first.position,
                        first.pad,
                        peripheral(first),
                        other.position,
                        other.pad,
                        peripheral(other)
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Two claimants on one pad, refused with both named: the link's carrier and a module; two modules
/// wanting different things of one pad; a module and a line the board solders to the pad.
fn check_claims(build: &Build<'_>, out: &Composition) -> Result<(), String> {
    let board = &build.board;
    if !build.carrier.role.is_empty() {
        if let Some(binding) = board.bindings.iter().find(|b| b.role == build.carrier.role) {
            for (signal, pin) in &binding.pins {
                if let Some(line) = out.lines.iter().find(|line| line.pad == pin.pin) {
                    return Err(format!(
                        "{} is claimed twice: the build's {} carrier rides binding '{}' with {signal} on it, and {} needs it too",
                        pin.pin,
                        build.carrier.kind,
                        binding.role,
                        line.claimant()
                    ));
                }
            }
        }
    }
    for (index, line) in out.lines.iter().enumerate() {
        for other in &out.lines[..index] {
            if other.pad != line.pad || other.socket == line.socket {
                continue;
            }
            let shared = other.serves == line.serves && matches!(line.serves, Serves::Bound { .. } | Serves::Added { .. });
            if !shared {
                return Err(format!(
                    "{} is claimed twice: {} needs it as {}, and {} as {}",
                    line.pad,
                    other.claimant(),
                    describe(&other.serves),
                    line.claimant(),
                    describe(&line.serves)
                ));
            }
        }
        let devices = board.board.devices.iter().chain(board.module_pins.iter());
        if let Some(device) = devices.into_iter().find(|device| device.pin == line.pad) {
            return Err(format!(
                "{} is claimed twice: board {} wires its '{}' to it, and {} needs it too",
                line.pad,
                board.board.board,
                device.name,
                line.claimant()
            ));
        }
    }
    Ok(())
}

fn describe(serves: &Serves) -> String {
    match serves {
        Serves::Bound { role, signal } => format!("binding '{role}' {signal}"),
        Serves::Added { instance, signal, .. } => format!("{instance} {signal}"),
        Serves::Gpio => "a GPIO".to_string(),
        Serves::Analog { channel, .. } => format!("converter channel {channel}"),
    }
}

/// The board bindings each line takes a pad from: every binding that routes the pad, but the one
/// that serves the line.
fn displaced(build: &Build<'_>, out: &Composition) -> Vec<Displaced> {
    let mut found: Vec<Displaced> = Vec::new();
    for line in &out.lines {
        let serving = match &line.serves {
            Serves::Bound { role, .. } | Serves::Analog { role, .. } => Some(role.as_str()),
            _ => None,
        };
        for binding in &build.board.bindings {
            if Some(binding.role.as_str()) == serving {
                continue;
            }
            for (signal, pin) in &binding.pins {
                if pin.pin != line.pad {
                    continue;
                }
                if found.iter().any(|d| d.role == binding.role && d.signal == *signal && d.pad == line.pad) {
                    continue;
                }
                found.push(Displaced {
                    role: binding.role.clone(),
                    signal: signal.clone(),
                    pad: line.pad.clone(),
                    by: line.claimant(),
                });
            }
        }
    }
    found
}
