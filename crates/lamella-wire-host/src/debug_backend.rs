//! [`WireHostBackend`]: the [`lamella_debug_backend::DebugBackend`] seam implemented over
//! the Lamella Link debug channel -- VS Code (via lamella-dap) debugs code running ON A DEVICE
//! with zero adapter changes.

use crate::{
    SerialTransport, TransferAck, abort_blocking, baked_image_checksum, deploy_image_blocking,
    deployed_status_blocking, hello_blocking, start_execution,
};
#[cfg(feature = "usb")]
use crate::{UsbTransport, parse_usb_target};
use lamella_debug_backend::{
    ChildReference, DebugBackend, Disassembled, Frame, Register, Scope, SourceLocation, Stop,
    Variable,
};
use lamella_runner::debug::{self, reason};
use lamella_runner::exec;
use lamella_wire::{Capabilities, Frame as WireFrame, TargetIdentity, Transport, TransportError};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// Packs a wire `(method_id, offset)` location into the seam's opaque address.
fn pack(method: u32, offset: u32) -> u64 {
    (u64::from(method) << 32) | u64::from(offset)
}

/// The inverse of [`pack`].
fn unpack(address: u64) -> (u32, u32) {
    ((address >> 32) as u32, address as u32)
}

/// The connect line the debug console shows for a target's self-reported identity: the product
/// name, the chip identity when the firmware fills it, and which firmware build is answering.
/// `None` when there is nothing to identify at all, so a target with nothing to declare stays
/// silent rather than printing a line of unknowns.
fn identity_line(identity: &TargetIdentity) -> Option<String> {
    let board = crate::board_name(identity.product_model);
    let chip = chip_identity(identity);
    let firmware = identity.firmware_version != [0, 0];
    if board.is_none() && chip.is_none() && !firmware {
        return None;
    }
    let mut line = String::from("Lamella Link: ");
    line.push_str(board.unwrap_or("unrecognized product"));
    if let Some(chip) = chip {
        line.push_str(&chip);
    }
    if firmware {
        line.push_str(&format!(
            ", firmware {}.{}",
            identity.firmware_version[0], identity.firmware_version[1]
        ));
    }
    line.push('\n');
    Some(line)
}

/// The chip half of the connect line, read according to the scheme the identity declares.
///
/// A debug-port code names a PORT CLASS and is shared across unrelated parts, so it is printed
/// beside the vendor register that separates them rather than alone: a reader who takes a part
/// name from the port code by itself takes the wrong one, confidently.
fn chip_identity(identity: &TargetIdentity) -> Option<String> {
    let word = |at: usize| {
        identity
            .chip_id
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
    };
    match identity.chip_id_kind {
        lamella_wire::chip_id_kind::DEBUG_PORT_AND_DEVICE_ID => {
            let port = word(0)?;
            let mut out = format!(", chip IDCODE {port:#010x}");
            if let Some(devid) = word(4).filter(|d| *d != 0) {
                out.push_str(&format!(" (devid {devid:#010x})"));
            }
            Some(out)
        }
        lamella_wire::chip_id_kind::RISCV_MVENDOR_MARCH_MIMP => Some(format!(
            ", chip vendor {:#010x} arch {:#010x} impl {:#010x}",
            word(0)?,
            word(4)?,
            word(8)?
        )),
        _ => None,
    }
}

/// The carrier a [`WireHostBackend`] drives: a serial port (USB-CDC / UART / a debug-probe VCP), or --
/// with the `usb` feature -- a board's native driverless-WinUSB device. A LOCAL enum (not `Box<dyn>`) so it
/// implements the foreign `Transport` trait, and the blocking deploy/hello drivers take it directly.
pub enum WireTransport {
    /// A serial-port carrier (USB-CDC / UART / a debug-probe VCP).
    Serial(SerialTransport),
    /// A board's native driverless-USB carrier (WinUSB / libusb), behind the `usb` feature.
    #[cfg(feature = "usb")]
    Usb(UsbTransport),
    /// An in-memory stand-in for a board, so the backend's own behaviour is testable with no
    /// hardware. `#[cfg(test)]`, so it exists in no build anyone ships and widens no public enum.
    /// Shared, because a test has to read what the backend sent AFTER the backend is gone.
    #[cfg(test)]
    Mem(std::sync::Arc<std::sync::Mutex<lamella_wire::MemTransport>>),
}

impl Transport for WireTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        match self {
            WireTransport::Serial(t) => t.send(msg_type, seq, payload),
            #[cfg(feature = "usb")]
            WireTransport::Usb(t) => t.send(msg_type, seq, payload),
            #[cfg(test)]
            WireTransport::Mem(t) => t.lock().expect("the test transport").send(msg_type, seq, payload),
        }
    }
    fn poll(&mut self) -> Result<Option<WireFrame>, TransportError> {
        match self {
            WireTransport::Serial(t) => t.poll(),
            #[cfg(feature = "usb")]
            WireTransport::Usb(t) => t.poll(),
            #[cfg(test)]
            WireTransport::Mem(t) => t.lock().expect("the test transport").poll(),
        }
    }
}

/// A pre-built source map for the DEPLOYED image (`lamella_srcmap`'s JSON, keyed by the SAME method_id the wire
/// reports), loaded alongside the `.lmli` so a Lamella Link session is SOURCE-LEVEL. `points` are `(il_offset, line,
/// column)` ascending by offset -- the wire reports the CIL offset directly, so no index conversion is needed.
struct MethodSrc {
    document: String,
    /// The method's qualified display name (`Type.Method`), for stack frames. Empty if the map predates names.
    name: String,
    points: Vec<(u32, u32, u32)>,
    /// `(slot, name)` for the method's source locals, ascending by slot.
    ///
    /// **THE WIRE IS POSITIONAL AND THIS IS THE ONLY THING THAT NAMES A SLOT.** `DBG_VARS` carries
    /// values in slot order and no names, deliberately -- the host already has them, and a target
    /// carrying a second copy would be carrying the PDB. So a map without this lane can still show
    /// a frame's values, as `local0`, `local1`; it is this that makes them `total` and `count`.
    locals: Vec<(u32, String)>,
}

/// The deployed image's source map -- `method_id -> (document, qualified name, sequence points)`, parsed from
/// `lamella_srcmap`'s JSON. Makes a Lamella Link session SOURCE-LEVEL: line lookups, source breakpoints, frame names.
pub struct SrcMap {
    methods: std::collections::HashMap<u32, MethodSrc>,
}

impl SrcMap {
    /// Parse `lamella_srcmap`'s JSON: `{ methods: { "<id>": { document, points: [{o,l,c}] } } }`.
    pub fn parse(json: &[u8]) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct RawPoint { o: u32, l: u32, #[serde(default)] c: u32 }
        #[derive(serde::Deserialize)]
        struct RawLocal { index: u32, name: String }
        #[derive(serde::Deserialize)]
        struct RawMethod {
            document: String,
            #[serde(default)] name: String,
            points: Vec<RawPoint>,
            #[serde(default)] locals: Vec<RawLocal>,
        }
        #[derive(serde::Deserialize)]
        struct Raw { methods: std::collections::HashMap<String, RawMethod> }
        let raw: Raw = serde_json::from_slice(json).ok()?;
        let mut methods = std::collections::HashMap::new();
        for (id, m) in raw.methods {
            let Ok(id) = id.parse::<u32>() else { continue };
            let mut points: Vec<(u32, u32, u32)> = m.points.into_iter().map(|p| (p.o, p.l, p.c)).collect();
            points.sort_by_key(|point| point.0);
            let mut locals: Vec<(u32, String)> =
                m.locals.into_iter().map(|local| (local.index, local.name)).collect();
            locals.sort_by_key(|local| local.0);
            methods.insert(id, MethodSrc { document: m.document, name: m.name, points, locals });
        }
        (!methods.is_empty()).then_some(Self { methods })
    }

    /// The `(file, line, column)` of the last sequence point at or before `offset` in `method`.
    fn location(&self, method: u32, offset: u32) -> Option<(&str, u32, u32)> {
        let method = self.methods.get(&method)?;
        let point = method
            .points
            .iter()
            .rev()
            .find(|(o, _, _)| *o <= offset)
            .or_else(|| method.points.first())?;
        Some((&method.document, point.1, point.2))
    }

    /// The qualified display name (`Type.Method`) recorded for `method`, if the map carries one (non-empty).
    fn name_of(&self, method: u32) -> Option<&str> {
        self.methods
            .get(&method)
            .map(|source| source.name.as_str())
            .filter(|name| !name.is_empty())
    }

    /// Resolve a source `(document, line)` to a `(method_id, il_offset)` breakpoint -- the nearest sequence point at
    /// or after `line` (matched by full path, else file basename).
    fn resolve(&self, document: &str, line: u32) -> Option<(u32, u32)> {
        let basename: fn(&str) -> &str = |path| path.rsplit(['/', '\\']).next().unwrap_or(path);
        let target = basename(document);
        let mut best: Option<(u32, u32, u32)> = None;
        for (&method, source) in &self.methods {
            if source.document != document && basename(&source.document) != target {
                continue;
            }
            for &(offset, l, _) in &source.points {
                if l >= line && best.map_or(true, |(_, _, distance)| l - line < distance) {
                    best = Some((method, offset, l - line));
                }
            }
        }
        best.map(|(method, offset, _)| (method, offset))
    }

    /// Is `offset` exactly a sequence point in `method` (a source-statement boundary)?
    fn is_sequence_point(&self, method: u32, offset: u32) -> bool {
        self.methods
            .get(&method)
            .map_or(false, |source| source.points.iter().any(|&(o, _, _)| o == offset))
    }

    /// The source name of local `slot` in `method`, if the map names it.
    ///
    /// **MATCHED BY THE RECORDED SLOT, NOT BY POSITION IN THE LIST.** A Portable PDB names only the
    /// locals it has names for, so the list is a SUBSET of the frame's slots: a method whose slot 0
    /// is a compiler temp and whose slot 1 is `total` records one entry, for slot 1. Indexing this
    /// list by the wire's slot number would call slot 0 `total` -- the pane would read plausibly and
    /// name the wrong value, which is the one outcome worse than showing no name at all.
    fn local_name(&self, method: u32, slot: u32) -> Option<&str> {
        self.methods
            .get(&method)?
            .locals
            .iter()
            .find(|(index, _)| *index == slot)
            .map(|(_, name)| name.as_str())
            .filter(|name| !name.is_empty())
    }

    /// Every sequence-point offset in `method` (the temp-breakpoint set for a source step-over into a call).
    fn points_of(&self, method: u32) -> Vec<u32> {
        self.methods
            .get(&method)
            .map_or(Vec::new(), |source| source.points.iter().map(|&(o, _, _)| o).collect())
    }
}

/// A [`DebugBackend`] driving a Lamella Link target's on-device interpreter session.
pub struct WireHostBackend {
    /// The carrier, behind a [`RefCell`] for the reason the sibling probe backends give: the seam's
    /// INSPECTION methods take `&self` while every wire operation needs `&mut`, because asking a
    /// target for a value is a round trip and not a field access.
    ///
    /// [`DebugBackend::variables`] is the method that needs it. The alternative -- caching every
    /// frame's values at each stop, as this backend does for the call stack -- costs one round trip
    /// PER FRAME at every stop, for panes the caller may never open. A client asks for the variables
    /// of the frame a person selected and nothing else, and reading on demand keeps that.
    transport: RefCell<WireTransport>,
    /// The deployed image's source map, if present -- makes the session source-level; `None` => IL-level.
    srcmap: Option<SrcMap>,
    /// The user's current breakpoint addresses (from set_breakpoints) -- kept armed alongside the temp breakpoints
    /// run_to_return() uses, so a source step-over never drops a user breakpoint.
    user_bps: Vec<u64>,
    image: Vec<u8>,
    timeout: Duration,
    seq: Cell<u16>,
    /// The product the target named in its HELLO, for a sentence that has to name the board.
    /// `None` when it named none.
    board: Option<&'static str>,
    /// What the TARGET said it can do, from its HELLO.
    ///
    /// Kept because a capability is the difference between a target that cannot answer a question
    /// and one that answered it with nothing, and only the first of those is worth telling the user
    /// about. [`DebugBackend::variables`] is the reader.
    target_caps: Capabilities,
    /// A debug session is live on the target (between the start and Done/Trap/detach).
    session_live: bool,
    /// A resume is in flight: [`DebugBackend::poll`] watches for its stop event.
    running: bool,
    /// The call stack cached at the last stop, innermost first.
    frames: Vec<(u32, u32)>,
    exit_code: i32,
    pending_output: RefCell<Option<String>>,
    /// The DEBUGGER's channel: what a program writes for a tool rather than for its user.
    ///
    /// Kept apart from the program's own output all the way across, because a client shows the two
    /// in separate panes and only the TARGET knows which is which. The output event names its
    /// stream, and this is the end that keeps them apart afterwards.
    pending_debug_output: RefCell<Option<String>>,
    /// The target's most recent run of standard-error output, HELD while nothing else has arrived.
    ///
    /// A target reports why a program could not go on -- an image it could not boot, an exception
    /// nothing caught, a firmware panic -- on its error stream, immediately before the stop that ends
    /// the program. So this is what that stop's fault quotes, and it reaches the console there, once.
    /// If anything else arrives first, or the program stops some other way, it was not that report
    /// after all, and it goes to the console as the output it arrived as ([`Self::release_error`]).
    last_error: RefCell<Option<String>>,
    /// When the free-running target last sent anything, for [`Self::poll`]'s question below.
    heard_at: Instant,
    /// The `EXEC_STATUS` [`Self::poll`] asked while the target was silent, and when: its sequence
    /// number, so only the answer to that question is read as one.
    status_asked: Option<(u16, Instant)>,
    /// What each [`ChildReference`] handed out since the last stop names: reference
    /// `first_reference + 1 + i` is entry `i`. Emptied at every stop, because what a reference names
    /// is a place in the paused program, and a program that has run has moved everything that place
    /// held.
    selectors: RefCell<Vec<Selector>>,
    /// The references handed out before the last stop, which no longer name anything.
    ///
    /// A reference is never reused within a session, so one kept from an earlier stop is REFUSED
    /// rather than answered with whatever this stop happened to file under the same number -- a pane
    /// showing another value's members under this one's name is the worst answer it could give.
    first_reference: u32,
}

/// One value in a paused program, named the way `DBG_EXPAND` names it: a root slot of a frame, then
/// the child index taken at each level below it.
///
/// Stateless on the target -- it re-walks the path from the frame's slot on every request -- so a
/// reference costs the target nothing to hold, and the host holds the path.
#[derive(Clone)]
struct Selector {
    /// The frame, counted as [`WireHostBackend::stack`] lists them (0 is innermost).
    frame: u16,
    /// Whether the root slot is an argument rather than a local.
    argument: bool,
    /// The root slot.
    slot: u16,
    /// The child index taken at each level below the root.
    path: Vec<u16>,
}

/// How long a running target may say nothing before the host asks whether it is still running.
///
/// A running target drops the question, so asking costs one short frame and disturbs nothing; only a
/// board back in its serve loop answers it, and that board is no longer running the program. The
/// interval is a bound on how long a session can go on believing a program runs that has gone.
const QUIET_BEFORE_ASKING: Duration = Duration::from_secs(2);

impl WireHostBackend {
    /// Open `port`, HELLO the target, and require the debug capabilities. `image` is the
    /// baked program this backend launches (and relaunches on a restart).
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the port cannot be opened; [`TransportError::Closed`]
    /// if the handshake times out or the target cannot debug.
    pub fn open(
        port: &str,
        baud: u32,
        image: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::from_transport(WireTransport::Serial(SerialTransport::open(port, baud)?), image, timeout)
    }

    /// Open a NATIVE-USB (driverless WinUSB) Lamella Link target by `vid`/`pid` + an optional serial
    /// substring (the picker key: an RP2350, for instance, reports its 16-hex chip id).
    ///
    /// # Errors
    /// As [`Self::open`], with a carrier error if no matching USB device is present.
    #[cfg(feature = "usb")]
    pub fn open_usb(
        vid: u16,
        pid: u16,
        serial: Option<&str>,
        image: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::from_transport(WireTransport::Usb(UsbTransport::open_matching(vid, pid, serial)?), image, timeout)
    }

    /// Open by a TARGET STRING: `usb` / `usb:<serial>` / `usb:<vid>:<pid>[:<serial>]` selects the native-USB
    /// carrier (when the `usb` feature is on); anything else is a serial port name.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub fn open_target(
        target: &str,
        baud: u32,
        image: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        #[cfg(feature = "usb")]
        if target == "usb" || target.starts_with("usb:") {
            let (vid, pid, serial) = parse_usb_target(target);
            return Self::open_usb(vid, pid, serial.as_deref(), image, timeout);
        }
        Self::open(target, baud, image, timeout)
    }

    /// HELLO `transport`, require the debug caps, and build the backend around it.
    fn from_transport(
        mut transport: WireTransport,
        image: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        let caps = Capabilities(
            Capabilities::DEBUG_BASIC
                | Capabilities::BREAKPOINTS
                | Capabilities::STEPPING
                | Capabilities::LOCALS
                | Capabilities::BAKED_IMAGE
                | Capabilities::PROFILE_CHIPID
                | Capabilities::DEPLOY_PREFIX_CRC
                | Capabilities::STRING_VALUES,
        );
        let session = hello_blocking(&mut transport, 0, caps, timeout)?;
        if !(session.caps.has(Capabilities::DEBUG_BASIC)
            && session.caps.has(Capabilities::BREAKPOINTS)
            && session.caps.has(Capabilities::STEPPING))
        {
            return Err(TransportError::Closed);
        }
        let pending_output = identity_line(&session.identity);
        Ok(Self {
            transport: RefCell::new(transport),
            image,
            timeout,
            seq: Cell::new(0),
            target_caps: session.caps,
            board: crate::board_name(session.identity.product_model),
            session_live: false,
            running: false,
            frames: Vec::new(),
            exit_code: 0,
            pending_output: RefCell::new(pending_output),
            pending_debug_output: RefCell::new(None),
            last_error: RefCell::new(None),
            heard_at: Instant::now(),
            status_asked: None,
            selectors: RefCell::new(Vec::new()),
            first_reference: 0,
            srcmap: None,
            user_bps: Vec::new(),
        })
    }

    /// Attach a pre-built source map (`lamella_srcmap` JSON, e.g. `<image>.srcmap.json`) so the session is
    /// source-level. `None` or unparseable JSON leaves it IL-level.
    #[must_use]
    pub fn with_srcmap(mut self, json: Option<Vec<u8>>) -> Self {
        if let Some(bytes) = json {
            self.srcmap = SrcMap::parse(&bytes);
        }
        self
    }

    fn next_seq(&self) -> u16 {
        let next = self.seq.get().wrapping_add(1);
        self.seq.set(next);
        next
    }

    /// End the session on the target if one is live: `DBG_DETACH`, wait for the ack, forget it.
    ///
    /// **ONE IMPLEMENTATION, CALLED FROM BOTH ENDS.** `launch` clears a session before starting
    /// another, and [`Drop`] closes the one this host opened.
    ///
    /// Best effort by construction: a target that has already gone away cannot be told anything,
    /// and a failed send is not worth reporting to a caller that is on its way out.
    fn detach_if_live(&mut self) {
        if !self.session_live {
            return;
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_DETACH, seq, &[]).is_ok() {
            self.await_type(debug::DBG_ACK);
        }
        self.session_live = false;
    }

    /// Blocks until a frame of `msg_type` arrives (dropping others -- the protocol runs
    /// one command in flight), or the timeout passes.
    fn await_type(&self, msg_type: u8) -> Option<WireFrame> {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            let polled = self.transport.borrow_mut().poll();
            match polled {
                Ok(Some(frame)) if frame.msg_type == msg_type => return Some(frame),
                Ok(Some(frame)) => self.absorb(&frame),
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => return None,
            }
        }
        None
    }

    /// Folds a frame that is not the one being waited for into backend state.
    ///
    /// Output is the whole of it, and it has to happen at EVERY place a frame is taken off the
    /// wire rather than only where output is expected. Output arrives UNSOLICITED, during a
    /// resume that has not answered yet, so a loop that drops what it is not waiting for drops
    /// the program's output -- and that failure is silent, because a program that printed nothing
    /// and a host that discarded what it printed look identical.
    fn absorb(&self, frame: &WireFrame) {
        use lamella_wire::msg::output;
        if frame.msg_type != debug::EVT_OUTPUT || frame.payload.len() < 2 {
            return;
        }
        let text = String::from_utf8_lossy(&frame.payload[2..]);
        if text.is_empty() {
            return;
        }
        if frame.payload[0] == output::STDERR {
            match &mut *self.last_error.borrow_mut() {
                Some(held) => held.push_str(&text),
                empty => *empty = Some(text.into_owned()),
            }
            return;
        }
        self.release_error();
        let mut sink = if frame.payload[0] == output::DEBUG {
            self.pending_debug_output.borrow_mut()
        } else {
            self.pending_output.borrow_mut()
        };
        match &mut *sink {
            Some(held) => held.push_str(&text),
            None => *sink = Some(text.into_owned()),
        }
    }

    /// Moves a held error-stream report into the program's console output, where it goes when no
    /// trap turned out to quote it.
    fn release_error(&self) {
        let Some(report) = self.last_error.borrow_mut().take() else {
            return;
        };
        match &mut *self.pending_output.borrow_mut() {
            Some(held) => held.push_str(&report),
            empty => *empty = Some(report),
        }
    }

    /// Folds an `EVT_STOPPED` into backend state and the seam's [`Stop`].
    fn on_stopped(&mut self, frame: &WireFrame) -> Stop {
        let issued = u32::try_from(self.selectors.get_mut().len()).unwrap_or(u32::MAX);
        self.first_reference = self.first_reference.saturating_add(issued);
        self.selectors.get_mut().clear();
        let why = frame.payload.first().copied().unwrap_or(reason::TRAP);
        match why {
            reason::DONE | reason::TRAP => {
                self.session_live = false;
                self.running = false;
                self.frames.clear();
                if let Some(exit) = frame.payload.get(9..13) {
                    self.exit_code = i32::from_le_bytes(exit.try_into().unwrap_or_default());
                }
                if why == reason::DONE {
                    Stop::Done
                } else {
                    let reported = self.last_error.borrow_mut().take();
                    match reported.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
                        Some(text) => Stop::Fault(format!("the target reported: {text}")),
                        None => Stop::Fault("unhandled trap on the target".to_string()),
                    }
                }
            }
            _ => {
                self.running = false;
                self.refresh_stack();
                if why == reason::BREAKPOINT { Stop::Breakpoint } else { Stop::Step }
            }
        }
    }

    /// Re-reads the call stack from the target (cached for `stack`/`depth`, which the
    /// seam wants synchronously and immutably).
    fn refresh_stack(&mut self) {
        self.frames.clear();
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_STACK, seq, &[]).is_err() {
            return;
        }
        let Some(frame) = self.await_type(debug::DBG_FRAMES) else {
            return;
        };
        let count = frame
            .payload
            .get(0..2)
            .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize);
        for index in 0..count {
            let base = 2 + index * 8;
            let Some(bytes) = frame.payload.get(base..base + 8) else {
                break;
            };
            self.frames.push((
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            ));
        }
    }

    /// Send the breakpoint set (each address = packed method_id|offset) to the target. DBG_BREAK REPLACES all
    /// breakpoints, so callers pass the FULL set they want armed.
    /// Programs the device's breakpoint set, reporting a wire failure rather than swallowing it:
    /// both halves are silent otherwise, and they fail differently. A `send` error is a dead
    /// transport; a missing `DBG_ACK` is a device that took the frame and did not answer, which is
    /// the case that leaves the host believing a breakpoint is armed.
    fn send_breakpoints(&mut self, addresses: &[u64]) -> Result<(), String> {
        let mut payload = Vec::with_capacity(2 + addresses.len() * 8);
        payload.extend_from_slice(&(addresses.len() as u16).to_le_bytes());
        for &address in addresses {
            let (method, offset) = unpack(address);
            payload.extend_from_slice(&method.to_le_bytes());
            payload.extend_from_slice(&offset.to_le_bytes());
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_BREAK, seq, &payload).is_err() {
            return Err("the wire dropped while sending breakpoints".to_string());
        }
        match self.await_type(debug::DBG_ACK) {
            Some(_) => Ok(()),
            None => Err("the target did not acknowledge the breakpoints".to_string()),
        }
    }

    /// Records that the target was just set running: nothing heard from it yet, and nothing asked.
    fn now_running(&mut self) {
        self.running = true;
        self.heard_at = Instant::now();
        self.status_asked = None;
    }

    /// Asks the board what it is executing, when a running target has been silent a while and the
    /// last such question has gone unanswered as long.
    ///
    /// **A board running the program never answers**: a debug session's run loop drops every frame
    /// but the few a run can act on. A board that has left the program -- a firmware fault that reset
    /// it into its serve loop -- answers IDLE. So the question separates a program that is quiet from
    /// one that is gone, without disturbing the first.
    fn ask_whether_running_if_quiet(&mut self) {
        let now = Instant::now();
        let asked_recently = self
            .status_asked
            .is_some_and(|(_, at)| now.duration_since(at) < QUIET_BEFORE_ASKING);
        if now.duration_since(self.heard_at) < QUIET_BEFORE_ASKING || asked_recently {
            return;
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(exec::EXEC_STATUS, seq, &[]).is_ok() {
            self.status_asked = Some((seq, now));
        }
    }

    /// Whether `frame` is the board answering this host's own `EXEC_STATUS` with IDLE.
    fn is_the_idle_answer(&self, frame: &WireFrame) -> bool {
        frame.msg_type == exec::EXEC_ACK
            && self.status_asked.is_some_and(|(seq, _)| seq == frame.seq)
            && frame.payload.first() == Some(&exec::ack::IDLE)
    }

    /// Ends a session whose program is gone without a stop: the board said nothing is executing.
    fn ended_unreported(&mut self) -> Stop {
        self.session_live = false;
        self.running = false;
        self.frames.clear();
        self.status_asked = None;
        Stop::Fault(
            "the program is no longer running on the target, and the target reported no stop: the \
             board says nothing is executing. A board that resets -- a firmware fault, or running out \
             of memory -- ends a program this way, with its output stopping where it ended."
                .to_string(),
        )
    }

    /// Whether the board already holds exactly the image this session launches, VERIFIED -- the
    /// question `wire-flash` asks before it deploys.
    ///
    /// Only a baked image carries a checksum to compare, so anything else is answered `false` without
    /// asking. A board that cannot say, or says something other than a verified match, is answered
    /// `false` too, and the image is sent: every wrong answer here costs a transfer, never a session
    /// debugging different code from the source map it was given.
    /// Brings the board back to its serve loop before a launch asks it anything.
    ///
    /// A board can be running a program no session here started: a debugger's disconnect RESUMES
    /// the program, as VS Code's detach expects, and the next launch is usually a new adapter with
    /// no session of its own to close. A running board answers none of a deploy's questions, so a
    /// launch that went straight to them waited out its timeout and then blamed the image.
    ///
    /// The target answers an `ABORT` on every path -- an idle board acknowledges from its serve
    /// loop -- so it is sent every time rather than guessed at, for one round trip.
    fn take_back_the_board(&mut self) -> Result<(), String> {
        let seq = self.next_seq();
        abort_blocking(&mut *self.transport.borrow_mut(), seq, self.timeout)
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "the target did not acknowledge an ABORT ({error:?}): it is neither serving nor \
                     running a program this host can stop"
                )
            })
    }

    fn board_holds_this_image(&self) -> bool {
        let Some(want) = baked_image_checksum(&self.image) else {
            return false;
        };
        let seq = self.next_seq();
        let held = deployed_status_blocking(&mut *self.transport.borrow_mut(), seq, self.timeout);
        matches!(held, Ok(Some(checksum)) if checksum == want)
    }

    /// Starts the deployed image HALTED at its entry, reads the entry stop, and arms the breakpoints the
    /// client set before there was a session -- the whole of a launch after the image is in place.
    fn start_deployed(&mut self) -> Result<(), String> {
        let seq = self.next_seq();
        start_execution(
            &mut *self.transport.borrow_mut(),
            seq,
            exec::exec_source::DEPLOYED,
            exec::exec_flags::START_HALTED,
            self.timeout,
        )
        .map_err(|failure| failure.to_string())?;
        let Some(stop) = self.await_type(debug::EVT_STOPPED) else {
            return Err(
                "the target started the program and reported no stop at its entry point".to_string(),
            );
        };
        self.session_live = true;
        if let Stop::Fault(reason) = self.on_stopped(&stop) {
            return Err(format!("the program did not reach its entry point: {reason}"));
        }
        if self.user_bps.is_empty() {
            return Ok(());
        }
        let pending = self.user_bps.clone();
        self.send_breakpoints(&pending)
    }

    /// One frame's arguments or locals as rows, each with a reference to its members when
    /// `with_children` asks for them -- the body of both [`DebugBackend::variables`] and
    /// [`DebugBackend::variables_with_children`], so the two can never disagree about a row.
    fn frame_rows(&self, frame: usize, scope: Scope, with_children: bool) -> Vec<(Variable, ChildReference)> {
        if matches!(scope, Scope::Stack) {
            return Vec::new();
        }
        if !self.session_live {
            return Vec::new();
        }
        if self.running {
            return refusal("the program is running -- pause it to read its variables");
        }
        if !self.target_caps.has(Capabilities::LOCALS) {
            return refusal(
                "this target's firmware does not serve variables (no LOCALS capability in its HELLO)",
            );
        }
        let Some(&(method, _)) = self.frames.get(frame) else {
            return refusal("that frame is not in the target's current call stack");
        };
        let Ok(index) = u16::try_from(frame) else {
            return refusal("that frame is deeper than the wire can address");
        };
        let Some((locals, arguments)) = self.locals(index) else {
            return refusal("the target did not answer the request for this frame's variables");
        };

        let arguments_wanted = matches!(scope, Scope::Arguments);
        let values = if arguments_wanted { arguments } else { locals };
        values
            .iter()
            .enumerate()
            .map(|(slot, value)| {
                let name = if arguments_wanted {
                    format!("arg{slot}")
                } else {
                    self.srcmap
                        .as_ref()
                        .and_then(|srcmap| srcmap.local_name(method, slot as u32))
                        .map_or_else(|| format!("local{slot}"), str::to_owned)
                };
                let children = match u16::try_from(slot) {
                    Ok(slot) if with_children => self.reference_for(
                        value,
                        Selector { frame: index, argument: arguments_wanted, slot, path: Vec::new() },
                    ),
                    _ => ChildReference::NONE,
                };
                let (value, kind) = render(value);
                (Variable { name, value, kind }, children)
            })
            .collect()
    }

    /// A reference to `value`'s members, filed under `selector`, or [`ChildReference::NONE`] for a
    /// value that has none to list: a number, a null, a pointer, a struct with no fields.
    fn reference_for(&self, value: &WireValue, selector: Selector) -> ChildReference {
        let has_members = match value {
            WireValue::Object { .. } => true,
            WireValue::Struct { field_count, .. } => *field_count > 0,
            _ => false,
        };
        if !has_members {
            return ChildReference::NONE;
        }
        let mut selectors = self.selectors.borrow_mut();
        selectors.push(selector);
        let issued = u32::try_from(selectors.len()).unwrap_or(u32::MAX);
        ChildReference(self.first_reference.saturating_add(issued))
    }

    /// Requests one frame's variables (`DBG_LOCALS`, `frame_index` in the [`WireHostBackend::stack`]
    /// order, 0 = innermost) and decodes the positional `DBG_VARS` reply into `(locals, args)`.
    /// `None` on a wire failure/timeout; a target without `Capabilities::LOCALS` never gets asked
    /// (the caller gates on the HELLO). Slot NAMES are the caller's to layer on (the srcmap's
    /// `local_variables` slot -> name lane); the wire is positional by design.
    pub fn locals(&self, frame_index: u16) -> Option<(Vec<WireValue>, Vec<WireValue>)> {
        let seq = self.next_seq();
        self.transport.borrow_mut().send(debug::DBG_LOCALS, seq, &frame_index.to_le_bytes()).ok()?;
        let frame = self.await_type(debug::DBG_VARS)?;
        decode_vars(&frame.payload)
    }

    /// Expands one value's children (`DBG_EXPAND` with the STATELESS selector: the frame, the
    /// root local/argument slot, and a path of child indices re-walked on-device from that
    /// root) and decodes the `DBG_CHILDREN` reply into `(name, value)` pairs. The names are the
    /// target's runtime type metadata (`fieldN`, `[i]`, a box's `value`). An unresolvable
    /// selector (e.g. the target resumed since the slot was read) decodes as the empty list.
    ///
    /// The request carries a RANGE and the reply says how many children the value has, so a large
    /// aggregate is expandable at all: a hundred-thousand-element array is about 1.3 MB whole, which
    /// no frame can carry and which is therefore lost entirely rather than truncated. The range
    /// costs the selector nothing, because it was already stateless -- the target re-walks it from
    /// the frame root every time, so a window is a slice of a fresh answer rather than a cursor
    /// anything has to remember.
    pub fn expand(
        &self,
        frame_index: u16,
        root_is_argument: bool,
        root_slot: u16,
        path: &[u16],
    ) -> Option<Vec<(String, WireValue)>> {
        self.expand_range(frame_index, root_is_argument, root_slot, path, 0, Self::EXPAND_PAGE)
    }

    /// One page of children, starting at `first_child`. [`Self::expand`] is this over the first
    /// page; the count the reply carries is how a caller knows whether to ask for another.
    pub fn expand_range(
        &self,
        frame_index: u16,
        root_is_argument: bool,
        root_slot: u16,
        path: &[u16],
        first_child: u16,
        max_children: u16,
    ) -> Option<Vec<(String, WireValue)>> {
        let payload = self.request_children(
            frame_index,
            root_is_argument,
            root_slot,
            path,
            first_child,
            max_children,
        )?;
        decode_children(&payload)
    }

    /// One page of a value's children AND how many it has in all, read off one `DBG_CHILDREN` reply.
    fn expand_page(
        &self,
        selector: &Selector,
        first_child: u16,
        max_children: u16,
    ) -> Option<(Vec<(String, WireValue)>, u16)> {
        let payload = self.request_children(
            selector.frame,
            selector.argument,
            selector.slot,
            &selector.path,
            first_child,
            max_children,
        )?;
        Some((decode_children(&payload)?, children_total(&payload)?))
    }

    /// Sends one `DBG_EXPAND` and returns the payload of the `DBG_CHILDREN` that answers it.
    fn request_children(
        &self,
        frame_index: u16,
        root_is_argument: bool,
        root_slot: u16,
        path: &[u16],
        first_child: u16,
        max_children: u16,
    ) -> Option<Vec<u8>> {
        let mut payload = Vec::with_capacity(10 + path.len() * 2);
        payload.extend_from_slice(&frame_index.to_le_bytes());
        payload.push(u8::from(root_is_argument));
        payload.extend_from_slice(&root_slot.to_le_bytes());
        payload.push(path.len().min(255) as u8);
        for step in path.iter().take(255) {
            payload.extend_from_slice(&step.to_le_bytes());
        }
        payload.extend_from_slice(&first_child.to_le_bytes());
        payload.extend_from_slice(&max_children.to_le_bytes());
        let seq = self.next_seq();
        self.transport.borrow_mut().send(debug::DBG_EXPAND, seq, &payload).ok()?;
        let frame = self.await_type(debug::DBG_CHILDREN)?;
        Some(frame.payload.to_vec())
    }

    /// How many children one expansion asks for by default.
    ///
    /// A variables pane shows a screenful and a person scrolls, so a page is what a first request
    /// wants; asking for everything is what makes an ordinary act -- expanding an array -- fail.
    const EXPAND_PAGE: u16 = 256;
}

/// One decoded `<val>` from a `DBG_VARS`/`DBG_CHILDREN` payload (the wire encoding is
/// specified at [`lamella_runner::debug::val`]). Positional and shallow: an [`WireValue::Object`]
/// or a non-empty [`WireValue::Struct`] drills down via [`WireHostBackend::expand`]; the
/// `type_token` resolves to a display name through the host's metadata (0 = no recoverable
/// type identity on the target).
#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    /// The null reference.
    Null,
    /// A 32-bit integer (also `bool`/`char`/small ints, widened on the target's stack).
    Int32(i32),
    /// A 64-bit integer.
    Int64(i64),
    /// A native-sized integer.
    NativeInt(i64),
    /// A `System.Double`.
    Float(f64),
    /// A `System.Single`.
    Single(f32),
    /// An object reference: the target heap handle (display/correlation only -- stale after
    /// a resume) and the asm-folded type handle.
    Object {
        /// The target heap slot (an id, never a pointer).
        handle: u32,
        /// The asm-folded `TypeDef` handle, 0 when the target has no type identity for it.
        type_token: u64,
    },
    /// An inline value-type instance: its field count (drill down for the fields).
    Struct {
        /// How many fields the instance carries.
        field_count: u16,
        /// Always 0 today: an inline struct carries no runtime type id on the target.
        type_token: u64,
    },
    /// A managed pointer, as the wire's fixed-width location descriptor.
    ByRef {
        /// The location kind (see the wire spec's kind table).
        kind: u8,
        /// The first descriptor word (its meaning depends on `kind`).
        a: u32,
        /// The second descriptor word.
        b: u32,
        /// The third descriptor word.
        c: u32,
    },
    /// A string's text, from a target whose session negotiated
    /// [`lamella_wire::Capabilities::STRING_VALUES`].
    String {
        /// The whole string's length in UTF-16 code units -- what C#'s `Length` reports.
        units: u32,
        /// Its first characters, as many as the target sent.
        text: String,
    },
    /// A typed reference: the referent's type token plus the location descriptor.
    TypedRef {
        /// The asm-folded type handle of the referent.
        type_token: u64,
        /// The location kind.
        kind: u8,
        /// The first descriptor word.
        a: u32,
        /// The second descriptor word.
        b: u32,
        /// The third descriptor word.
        c: u32,
    },
}

/// A variables row that reports why there is no value, for [`DebugBackend::variables`].
///
/// **THE NAME IS IN ANGLE BRACKETS ON PURPOSE.** It shares a pane with the program's own variables
/// and is matched against by `evaluate` when a person hovers a name, so it has to be a string no C#
/// identifier can be -- otherwise a hover over a variable could resolve to a diagnostic and display
/// it as that variable's value.
fn unavailable(reason: &str) -> Variable {
    Variable {
        name: "<unavailable>".to_string(),
        value: reason.to_string(),
        kind: "unsupported".to_string(),
    }
}

/// An [`unavailable`] row as the whole answer, with nothing to expand under it.
fn refusal(reason: &str) -> Vec<(Variable, ChildReference)> {
    vec![(unavailable(reason), ChildReference::NONE)]
}

/// Renders one decoded wire value as `(value, type name)` for a variables pane.
///
/// The type names are the CIL stack kinds the wire tags carry, which is what the target knows: a
/// value crosses as INT32 whether its source declared `int`, `bool`, `char` or `short`, because the
/// interpreter widened it on its own stack. Reporting the declared type would mean reading it from
/// metadata this host is not given, and guessing it from the tag would name `bool` an `int`.
///
/// An [`WireValue::Object`] or a non-empty [`WireValue::Struct`] renders as an identity a reader
/// can correlate across stops, never as a value: what it holds is its CHILDREN, which the row's
/// [`ChildReference`] lists through [`DebugBackend::children`].
fn render(value: &WireValue) -> (String, String) {
    match *value {
        WireValue::Null => ("null".to_string(), "object".to_string()),
        WireValue::Int32(value) => (value.to_string(), "int".to_string()),
        WireValue::Int64(value) => (value.to_string(), "long".to_string()),
        WireValue::NativeInt(value) => (value.to_string(), "nint".to_string()),
        WireValue::Float(value) => (value.to_string(), "double".to_string()),
        WireValue::Single(value) => (value.to_string(), "float".to_string()),
        WireValue::Object { handle, .. } => {
            (format!("object #{handle}"), "object".to_string())
        }
        WireValue::Struct { field_count, .. } => (
            format!("{field_count} field{}", if field_count == 1 { "" } else { "s" }),
            "struct".to_string(),
        ),
        WireValue::ByRef { .. } => {
            ("<managed pointer>".to_string(), "byref".to_string())
        }
        WireValue::TypedRef { .. } => {
            ("<typed reference>".to_string(), "typedref".to_string())
        }
        WireValue::String { units, ref text } => (quoted(text, units), "string".to_string()),
    }
}

/// A string as a C# literal spells it -- so a quote, a backslash or a line break inside it cannot
/// be mistaken for where it ends -- and, when the target sent only its first characters, how long
/// it really is, so a cut string never reads as the whole of it.
fn quoted(text: &str, units: u32) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other if other.is_control() => out.push_str(&format!("\\u{:04x}", other as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    let sent = text.encode_utf16().count();
    if (sent as u64) < u64::from(units) {
        out.push_str(&format!("... (Length {units})"));
    }
    out
}

/// Decodes one `<val>` at `*at`, advancing past it. `None` on a truncated/unknown payload.
fn decode_value(payload: &[u8], at: &mut usize) -> Option<WireValue> {
    use lamella_runner::debug::val;
    let tag = *payload.get(*at)?;
    *at += 1;
    let mut take = |n: usize| -> Option<&[u8]> {
        let bytes = payload.get(*at..*at + n)?;
        *at += n;
        Some(bytes)
    };
    Some(match tag {
        val::NULL => WireValue::Null,
        val::INT32 => WireValue::Int32(i32::from_le_bytes(take(4)?.try_into().ok()?)),
        val::INT64 => WireValue::Int64(i64::from_le_bytes(take(8)?.try_into().ok()?)),
        val::NATIVE_INT => WireValue::NativeInt(i64::from_le_bytes(take(8)?.try_into().ok()?)),
        val::FLOAT => WireValue::Float(f64::from_le_bytes(take(8)?.try_into().ok()?)),
        val::SINGLE => WireValue::Single(f32::from_le_bytes(take(4)?.try_into().ok()?)),
        val::OBJECT => {
            let handle = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let type_token = u64::from_le_bytes(take(8)?.try_into().ok()?);
            WireValue::Object { handle, type_token }
        }
        val::STRUCT => {
            let field_count = u16::from_le_bytes(take(2)?.try_into().ok()?);
            let type_token = u64::from_le_bytes(take(8)?.try_into().ok()?);
            WireValue::Struct { field_count, type_token }
        }
        val::BYREF => {
            let kind = *take(1)?.first()?;
            let a = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let b = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let c = u32::from_le_bytes(take(4)?.try_into().ok()?);
            WireValue::ByRef { kind, a, b, c }
        }
        val::TYPED_REF => {
            let type_token = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let kind = *take(1)?.first()?;
            let a = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let b = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let c = u32::from_le_bytes(take(4)?.try_into().ok()?);
            WireValue::TypedRef { type_token, kind, a, b, c }
        }
        val::STRING => {
            let units = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
            let text = String::from_utf8_lossy(take(len)?).into_owned();
            WireValue::String { units, text }
        }
        _ => return None,
    })
}

/// Decodes a `DBG_VARS` payload into `(locals, args)`. `None` on a malformed payload.
#[must_use]
pub fn decode_vars(payload: &[u8]) -> Option<(Vec<WireValue>, Vec<WireValue>)> {
    let mut at = 0;
    let count = |at: &mut usize| -> Option<usize> {
        let bytes = payload.get(*at..*at + 2)?;
        *at += 2;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]) as usize)
    };
    let locals_n = count(&mut at)?;
    let mut locals = Vec::with_capacity(locals_n);
    for _ in 0..locals_n {
        locals.push(decode_value(payload, &mut at)?);
    }
    let args_n = count(&mut at)?;
    let mut args = Vec::with_capacity(args_n);
    for _ in 0..args_n {
        args.push(decode_value(payload, &mut at)?);
    }
    Some((locals, args))
}

/// Decodes a `DBG_CHILDREN` payload into `(name, value)` pairs. `None` on a malformed payload.
///
/// The payload opens with the value's TOTAL child count and then the count in this page, so a caller
/// can tell a value with four children from a page of four out of forty thousand. This returns the
/// page; [`children_total`] reads the total from the same bytes.
#[must_use]
pub fn decode_children(payload: &[u8]) -> Option<Vec<(String, WireValue)>> {
    let bytes = payload.get(2..4)?;
    let count = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    let mut at = 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let len = *payload.get(at)? as usize;
        at += 1;
        let name = String::from_utf8(payload.get(at..at + len)?.to_vec()).ok()?;
        at += len;
        out.push((name, decode_value(payload, &mut at)?));
    }
    Some(out)
}

/// How many children the expanded value has in total, from the same `DBG_CHILDREN` payload.
///
/// It is what tells a caller that a page is a page. Without it, a host asking for the first
/// 256 elements of an array gets 256 back and has no way to distinguish that from an array of
/// exactly 256 -- so it either stops early or asks forever.
#[must_use]
pub fn children_total(payload: &[u8]) -> Option<u16> {
    let bytes = payload.get(0..2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Closing the host ends a session nothing handed back.
///
/// When the debug adapter ends a session -- a `disconnect`, or a client that goes away -- it releases
/// the target first ([`DebugBackend::release`]): the breakpoints come off, the program runs on, and no
/// session is left for this to end. A host dropped without a release is where this matters, so that
/// its session does not stay live on the device until some later host launches into it.
///
/// This covers a host whose session ends with the host, which is every host that runs one session
/// per process. A host holding one backend across several sessions wants an explicit detach on the
/// [`DebugBackend`] seam itself, which this crate does not own.
impl Drop for WireHostBackend {
    fn drop(&mut self) {
        self.detach_if_live();
    }
}

impl DebugBackend for WireHostBackend {
    fn launch(&mut self) -> Result<(), String> {
        self.detach_if_live();
        self.running = false;
        self.exit_code = 0;
        self.last_error.borrow_mut().take();
        self.take_back_the_board()?;
        if self.board_holds_this_image() {
            return self.start_deployed();
        }
        let seq = self.next_seq();
        let deployed = deploy_image_blocking(
            &mut *self.transport.borrow_mut(),
            seq,
            &self.image,
            8 * 1024,
            self.timeout,
            self.target_caps,
        );
        match deployed {
            Ok(TransferAck::Accepted) => {}
            Ok(TransferAck::Rejected { chunk }) => {
                return Err(format!("the target refused chunk {chunk} of the image"));
            }
            Ok(TransferAck::Mismatched { chunk, .. }) => {
                return Err(format!(
                    "the target's flash does not hold the image that was sent, from chunk {chunk} on"
                ));
            }
            Ok(TransferAck::TooLarge { image, window }) => {
                return Err(crate::image_too_large(self.board, image, window));
            }
            Ok(TransferAck::OutOfRange { chunk }) => {
                return Err(format!(
                    "the target refused chunk {chunk} of the image as reaching past its deploy window"
                ));
            }
            Err(TransportError::Closed) => {
                return Err("the target did not acknowledge the image as it was sent".to_string());
            }
            Err(error) => {
                return Err(format!("the carrier failed while the image was sent: {error:?}"));
            }
        }
        self.start_deployed()
    }

    fn resume(&mut self) -> Stop {
        if !self.session_live {
            return Stop::Done;
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_RESUME, seq, &[]).is_err() {
            return Stop::Fault("the wire dropped".to_string());
        }
        self.now_running();
        Stop::Running
    }

    fn poll(&mut self) -> Stop {
        if !self.running {
            return if self.session_live { Stop::Step } else { Stop::Done };
        }
        let polled = self.transport.borrow_mut().poll();
        match polled {
            Ok(Some(frame)) if frame.msg_type == debug::EVT_STOPPED => self.on_stopped(&frame),
            Ok(Some(frame)) if self.is_the_idle_answer(&frame) => self.ended_unreported(),
            Ok(Some(frame)) => {
                self.heard_at = Instant::now();
                self.absorb(&frame);
                Stop::Running
            }
            Ok(None) => {
                self.ask_whether_running_if_quiet();
                Stop::Running
            }
            Err(_) => Stop::Fault("the wire dropped".to_string()),
        }
    }

    fn pause(&mut self) -> bool {
        if !self.session_live || !self.running {
            return true;
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_PAUSE, seq, &[]).is_err() {
            return false;
        }
        match self.await_type(debug::EVT_STOPPED) {
            Some(frame) => {
                self.on_stopped(&frame);
            }
            None => {
                self.running = false;
            }
        }
        true
    }

    /// Takes every breakpoint off and lets a stopped program run on, so it goes on as it would with
    /// no debugger attached. It sends no detach: on this wire a detach ends the execution, and the
    /// target goes back to waiting for its next command.
    fn release(&mut self) -> Result<(), String> {
        if !self.session_live {
            return Ok(());
        }
        self.set_breakpoints(&[])
            .map_err(|reason| format!("could not remove the breakpoints: {reason}"))?;
        if !self.running {
            let seq = self.next_seq();
            self.transport
                .borrow_mut()
                .send(debug::DBG_RESUME, seq, &[])
                .map_err(|_| "could not resume the program: the wire dropped".to_string())?;
        }
        self.session_live = false;
        self.running = false;
        self.frames.clear();
        Ok(())
    }

    fn step(&mut self) -> Stop {
        if !self.session_live {
            return Stop::Done;
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_STEP, seq, &[debug::step_mode::IN]).is_err() {
            return Stop::Fault("the wire dropped".to_string());
        }
        match self.await_type(debug::EVT_STOPPED) {
            Some(frame) => self.on_stopped(&frame),
            None => Stop::Fault("the step timed out".to_string()),
        }
    }

    fn exit_code(&self) -> i32 {
        self.exit_code
    }

    fn depth(&self) -> usize {
        self.frames.len().max(1)
    }

    fn set_breakpoints(&mut self, addresses: &[u64]) -> Result<(), String> {
        self.user_bps = addresses.to_vec();
        if !self.session_live {
            return Ok(());
        }
        if self.running {
            self.pause();
            self.send_breakpoints(addresses)?;
            let seq = self.next_seq();
            if self.transport.borrow_mut().send(debug::DBG_RESUME, seq, &[]).is_ok() {
                self.now_running();
            }
            Ok(())
        } else {
            self.send_breakpoints(addresses)
        }
    }

    fn run_to_return(&mut self) -> Stop {
        if !self.session_live {
            return Stop::Done;
        }
        let Some(&(caller, _)) = self.frames.get(1) else {
            return self.step();
        };
        let temps: Vec<u64> = match self.srcmap.as_ref() {
            Some(srcmap) => srcmap.points_of(caller).into_iter().map(|offset| pack(caller, offset)).collect(),
            None => Vec::new(),
        };
        if temps.is_empty() {
            return self.step();
        }
        let mut armed = self.user_bps.clone();
        armed.extend_from_slice(&temps);
        if let Err(reason) = self.send_breakpoints(&armed) {
            return Stop::Fault(reason);
        }
        let seq = self.next_seq();
        if self.transport.borrow_mut().send(debug::DBG_RESUME, seq, &[]).is_err() {
            return Stop::Fault("the wire dropped".to_string());
        }
        self.running = true;
        let stop = match self.await_type(debug::EVT_STOPPED) {
            Some(frame) => self.on_stopped(&frame),
            None => Stop::Fault("run-to-return timed out".to_string()),
        };
        let restore = self.user_bps.clone();
        if let Err(reason) = self.send_breakpoints(&restore) {
            return Stop::Fault(reason);
        }
        match stop {
            Stop::Breakpoint => {
                let at = self.frames.first().map(|&(method, offset)| pack(method, offset));
                if at.map_or(false, |address| self.user_bps.contains(&address)) {
                    Stop::Breakpoint
                } else {
                    Stop::Step
                }
            }
            other => other,
        }
    }

    fn step_out(&mut self) -> Option<Stop> {
        if self.frames.len() < 2 {
            return None;
        }
        Some(self.run_to_return())
    }

    fn stack(&self) -> Vec<Frame> {
        self.frames
            .iter()
            .map(|&(method, offset)| Frame {
                address: pack(method, offset),
                name: self
                    .srcmap
                    .as_ref()
                    .and_then(|srcmap| srcmap.name_of(method))
                    .map_or_else(|| format!("method {method}"), String::from),
                line: offset + 1,
            })
            .collect()
    }

    /// One frame's arguments or locals, read from the paused target over `DBG_LOCALS`.
    ///
    /// One round trip per call, and DAP makes the call for the frame a person clicked -- so a stop
    /// costs nothing until a pane is opened. The wire answers POSITIONALLY; the slot names come from
    /// the source map, which is why a target carries none.
    ///
    /// # A wrong value here is worse than no value
    ///
    /// Someone opens this pane precisely to find out whether a value is what they think it is, so
    /// every way of not knowing answers with a row that SAYS SO rather than with an empty pane or a
    /// plausible number. The rows are named in angle brackets, which no C# identifier can be, so
    /// `evaluate` -- which resolves a hover or a watch by matching a name against this list -- can
    /// never match one and report a diagnostic as the value of somebody's variable.
    ///
    fn variables(&self, frame: usize, scope: Scope) -> Vec<Variable> {
        self.frame_rows(frame, scope, false)
            .into_iter()
            .map(|(variable, _)| variable)
            .collect()
    }

    /// [`DebugBackend::variables`], with a reference on each row whose value has members -- an
    /// object, or a struct with fields -- that [`DebugBackend::children`] lists.
    fn variables_with_children(&self, frame: usize, scope: Scope) -> Vec<(Variable, ChildReference)> {
        self.frame_rows(frame, scope, true)
    }

    /// The members of a value, one `DBG_EXPAND` each time a client opens one: an object's fields as
    /// the target names them (`field0`, `field1`), an array's elements (`[0]`, `[1]`), a box's
    /// `value`. Each member that has members of its own carries a reference to them, so a client can
    /// keep opening, and the target re-walks the whole path from the frame's slot every time.
    ///
    /// A page is 256 members; a value with more says how many it left out, in a row of its own.
    fn children(&self, reference: ChildReference) -> Vec<(Variable, ChildReference)> {
        let selector = reference
            .0
            .checked_sub(self.first_reference.saturating_add(1))
            .and_then(|index| self.selectors.borrow().get(index as usize).cloned());
        let Some(selector) = selector else {
            return refusal("that value is from an earlier stop -- the program has run since");
        };
        if self.running {
            return refusal("the program is running -- pause it to read its variables");
        }
        let Some((members, total)) = self.expand_page(&selector, 0, Self::EXPAND_PAGE) else {
            return refusal("the target did not answer the request for this value's members");
        };
        if members.is_empty() {
            return vec![(
                Variable {
                    name: "<no members>".to_string(),
                    value: "the target lists nothing inside this value".to_string(),
                    kind: String::new(),
                },
                ChildReference::NONE,
            )];
        }
        let shown = members.len();
        let mut rows: Vec<(Variable, ChildReference)> = members
            .into_iter()
            .enumerate()
            .map(|(index, (name, value))| {
                let children = u16::try_from(index).map_or(ChildReference::NONE, |step| {
                    let mut path = selector.path.clone();
                    path.push(step);
                    self.reference_for(&value, Selector { path, ..selector.clone() })
                });
                let (text, kind) = render(&value);
                (Variable { name, value: text, kind }, children)
            })
            .collect();
        let total = usize::from(total);
        if total > shown {
            rows.push((
                Variable {
                    name: "<more>".to_string(),
                    value: format!("{} more not listed", total - shown),
                    kind: String::new(),
                },
                ChildReference::NONE,
            ));
        }
        rows
    }

    fn has_source(&self) -> bool {
        self.srcmap.is_some()
    }

    /// Is the current (innermost) stop exactly at a source-statement boundary? `source_step()` single-steps until
    /// this is true, so without it a source step-over would never terminate.
    fn at_source_boundary(&self) -> bool {
        let Some(&(method, offset)) = self.frames.first() else {
            return false;
        };
        self.srcmap
            .as_ref()
            .map_or(false, |srcmap| srcmap.is_sequence_point(method, offset))
    }

    /// Resolve a frame's opaque address `(method_id, il_offset)` to a source line via the deployed image's map.
    fn source_location(&self, address: u64) -> Option<SourceLocation> {
        let (method, offset) = unpack(address);
        let (file, line, column) = self.srcmap.as_ref()?.location(method, offset)?;
        Some(SourceLocation {
            file: file.to_string(),
            line,
            column,
            end_line: line,
            end_column: column,
        })
    }

    /// Map a source `(document, line)` breakpoint to the `(method_id, il_offset)` address DBG_BREAK wants.
    fn resolve_source_breakpoint(&self, document: &str, line: u32) -> Option<u64> {
        let (method, offset) = self.srcmap.as_ref()?.resolve(document, line)?;
        Some(pack(method, offset))
    }

    fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
        Vec::new()
    }

    fn read_registers(&self) -> Vec<Register> {
        Vec::new()
    }

    fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
        Vec::new()
    }

    fn take_output(&mut self) -> Option<String> {
        if !self.running {
            self.release_error();
        }
        self.pending_output.borrow_mut().take()
    }

    fn take_debug_output(&mut self) -> Option<String> {
        self.pending_debug_output.borrow_mut().take()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Capabilities, Cell, RefCell, SrcMap, WireHostBackend, WireTransport, debug, exec, pack,
        reason,
    };
    use lamella_debug_backend::{DebugBackend, Stop};
    use lamella_wire::{MemTransport, Transport};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A backend wired to an in-memory board, with a live session and one `DBG_ACK` already
    /// waiting -- so a detach completes instead of sitting out its timeout.
    fn backend_with_a_live_session() -> (WireHostBackend, Arc<Mutex<MemTransport>>) {
        live_session(1)
    }

    /// Queues one `DBG_ACK` per acknowledgement the test expects the board to give. Zero is the
    /// silent board: every wait runs out its timeout, which is the case a swallowed result cannot
    /// be told apart from a working one.
    fn live_session(acks: usize) -> (WireHostBackend, Arc<Mutex<MemTransport>>) {
        let mut host = MemTransport::new();
        for index in 0..acks {
            let mut board = MemTransport::new();
            board
                .send(debug::DBG_ACK, index as u16 + 1, &[])
                .expect("queue the board's ack");
            let acked = board.take_sent();
            host.feed(&acked);
        }

        let shared = Arc::new(Mutex::new(host));
        let backend = WireHostBackend {
            transport: RefCell::new(WireTransport::Mem(Arc::clone(&shared))),
            srcmap: None,
            user_bps: Vec::new(),
            image: Vec::new(),
            timeout: Duration::from_millis(50),
            seq: Cell::new(0),
            target_caps: Capabilities(u64::MAX),
            board: None,
            session_live: true,
            running: false,
            frames: Vec::new(),
            exit_code: 0,
            pending_output: RefCell::new(None),
            pending_debug_output: RefCell::new(None),
            last_error: RefCell::new(None),
            heard_at: Instant::now(),
            status_asked: None,
            selectors: RefCell::new(Vec::new()),
            first_reference: 0,
        };
        (backend, shared)
    }

    /// The frames the host sent, as `(type, payload)`, decoded through the wire's own framing rather
    /// than by looking for a byte -- a raw scan would pass on a payload that happens to contain one.
    fn sent_frames(shared: &Arc<Mutex<MemTransport>>) -> Vec<(u8, Vec<u8>)> {
        let bytes = shared.lock().expect("the test transport").take_sent();
        let mut reader = MemTransport::new();
        reader.feed(&bytes);
        let mut frames = Vec::new();
        while let Ok(Some(frame)) = reader.poll() {
            frames.push((frame.msg_type, frame.payload.to_vec()));
        }
        frames
    }

    /// The message types of [`sent_frames`].
    fn sent_types(shared: &Arc<Mutex<MemTransport>>) -> Vec<u8> {
        sent_frames(shared).into_iter().map(|(msg_type, _)| msg_type).collect()
    }

    /// The sequence number a fresh backend's launch sends its `ABORT` with: the first it takes.
    const ABORT_SEQ: u16 = 1;

    /// An idle board's answer to that `ABORT`, which is an acknowledgement from its serve loop.
    const ANSWERED_IDLE: (u8, u16, &[u8]) = (exec::EXEC_ACK, ABORT_SEQ, &[exec::ack::IDLE]);

    /// The sequence number a fresh backend's launch starts its execution with: the `ABORT` takes the
    /// first and the deploy the second, whether or not it sends anything, and a board's answer to the
    /// start has to carry the start's own number to be read as its answer.
    const START_SEQ: u16 = 3;

    /// A host with no session open, launching an empty image -- which deploys nothing, so the launch
    /// goes straight to its start -- into an idle board whose answers are already queued, in order.
    fn launching_into(
        answers: &[(u8, u16, &[u8])],
        timeout: Duration,
    ) -> (WireHostBackend, Arc<Mutex<MemTransport>>) {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.timeout = timeout;
        queue_board_answers(&shared, &[ANSWERED_IDLE]);
        queue_board_answers(&shared, answers);
        (backend, shared)
    }

    /// Queues `answers` -- `(type, seq, payload)`, in order -- as frames the board has already sent.
    fn queue_board_answers(shared: &Arc<Mutex<MemTransport>>, answers: &[(u8, u16, &[u8])]) {
        let mut board = MemTransport::new();
        for &(msg_type, seq, payload) in answers {
            board.send(msg_type, seq, payload).expect("queue the board's answer");
        }
        let queued = board.take_sent();
        shared.lock().expect("the test transport").feed(&queued);
    }

    #[test]
    fn a_target_that_never_acknowledges_the_breakpoints_is_reported() {
        let (mut backend, _shared) = live_session(0);
        let Err(reason) = backend.set_breakpoints(&[pack(1, 0)]) else {
            panic!("an unacknowledged breakpoint set must not be reported as armed");
        };
        assert!(
            reason.contains("acknowledge"),
            "and the reason must distinguish a silent device from a dead wire: {reason}"
        );
    }

    #[test]
    fn an_acknowledged_breakpoint_set_reports_success() {
        let (mut backend, _shared) = live_session(1);
        assert!(
            backend.set_breakpoints(&[pack(1, 0)]).is_ok(),
            "an acknowledged set is armed, and saying otherwise would grey working breakpoints"
        );
    }

    #[test]
    fn breakpoints_set_before_the_session_exists_are_not_reported_as_a_failure() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        assert!(backend.set_breakpoints(&[pack(1, 0)]).is_ok());
        assert_eq!(
            sent_types(&shared).iter().filter(|&&t| t == debug::DBG_BREAK).count(),
            0,
            "and nothing is sent into a session that does not exist -- launch arms it instead"
        );
    }

    #[test]
    fn dropping_the_host_detaches_the_session_it_opened() {
        let (backend, shared) = backend_with_a_live_session();

        drop(backend);

        assert_eq!(sent_types(&shared), vec![debug::DBG_DETACH], "the target is told, once");
    }

    #[test]
    fn dropping_a_host_with_no_live_session_sends_nothing() {
        let (mut backend, shared) = backend_with_a_live_session();
        backend.session_live = false;

        drop(backend);

        assert!(sent_types(&shared).is_empty(), "silence, because there is nothing to close");
    }

    #[test]
    fn a_start_the_target_refuses_fails_the_launch_at_once_with_its_reason() {
        let refused: &[u8] = &[exec::ack::NOTHING_TO_RUN];
        let (mut backend, _shared) =
            launching_into(&[(exec::EXEC_ACK, START_SEQ, refused)], Duration::from_secs(2));
        let began = Instant::now();
        let Err(reason) = backend.launch() else {
            panic!("a refused start must not be reported as a launched session");
        };
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "the refusal is the answer, so nothing waits for a stop after it: {:?}",
            began.elapsed()
        );
        assert!(reason.contains("NOTHING_TO_RUN"), "and the user is told what the target said: {reason}");
    }

    #[test]
    fn a_target_that_refuses_the_start_command_fails_the_launch_with_its_reason() {
        let held = lamella_wire::error::session_held(2);
        let (mut backend, _shared) =
            launching_into(&[(lamella_wire::msg::ERROR, START_SEQ, &held)], Duration::from_secs(2));
        let began = Instant::now();
        let Err(reason) = backend.launch() else {
            panic!("a target that refused the command must not be reported as launched");
        };
        assert!(began.elapsed() < Duration::from_secs(1), "{:?}", began.elapsed());
        assert!(reason.contains("refused"), "the reason says the target refused: {reason}");
    }

    #[test]
    fn a_start_nothing_acknowledges_fails_the_launch_saying_so() {
        let (mut backend, _shared) = launching_into(&[], Duration::from_millis(50));
        let Err(reason) = backend.launch() else {
            panic!("a silent target must not be reported as launched");
        };
        assert!(reason.contains("acknowledge"), "silence is named as silence: {reason}");
    }

    #[test]
    fn an_acknowledged_start_launches_into_the_entry_stop() {
        let started: &[u8] = &[exec::ack::STARTED];
        let entry: &[u8] = &[reason::ENTRY, 0, 0, 0, 0, 0, 0, 0, 0];
        let no_frames: &[u8] = &[0, 0];
        let (mut backend, shared) = launching_into(
            &[
                (exec::EXEC_ACK, START_SEQ, started),
                (debug::EVT_STOPPED, START_SEQ, entry),
                (debug::DBG_FRAMES, START_SEQ + 1, no_frames),
            ],
            Duration::from_secs(2),
        );
        assert_eq!(backend.launch(), Ok(()));
        assert_eq!(sent_types(&shared), vec![debug::ABORT, exec::EXEC, debug::DBG_STACK]);
    }

    #[test]
    fn a_board_that_answers_no_abort_fails_the_launch_before_anything_is_sent() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.timeout = Duration::from_millis(50);
        let Err(reason) = backend.launch() else {
            panic!("a board that answers nothing must not be reported as launched");
        };
        assert!(reason.contains("ABORT"), "the question that went unanswered is named: {reason}");
        assert_eq!(sent_types(&shared), vec![debug::ABORT], "and nothing is sent after it");
    }

    /// A baked-image header recording `checksum`, which is all a deploy-skip reads.
    ///
    /// Checked against the reader itself, so a header layout that moves fails HERE, by name, rather
    /// than as a launch that deploys when a test says it should not.
    fn image_with_checksum(checksum: u64) -> Vec<u8> {
        let mut image = Vec::new();
        image.extend_from_slice(b"LML1");
        image.extend_from_slice(&1u32.to_le_bytes());
        image.resize(8 + 32 * 8, 0);
        image[8 + 20 * 8..8 + 21 * 8].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            super::baked_image_checksum(&image),
            Some(checksum),
            "the fixture must be a header the checksum reader accepts -- the layout has moved"
        );
        image
    }

    /// The board's answer to `DEPLOY_STATUS`: a verified image with `checksum`.
    fn holds_verified(checksum: u64) -> Vec<u8> {
        let mut payload = vec![lamella_runner::deploy::deploy_state::VERIFIED, 0];
        payload.extend_from_slice(&checksum.to_le_bytes());
        payload
    }

    #[test]
    fn a_board_that_already_holds_this_image_is_not_sent_it_again() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.timeout = Duration::from_secs(2);
        backend.image = image_with_checksum(0x1122_3344_5566_7788);
        let held = holds_verified(0x1122_3344_5566_7788);
        let started: &[u8] = &[exec::ack::STARTED];
        let entry: &[u8] = &[reason::ENTRY, 0, 0, 0, 0, 0, 0, 0, 0];
        let no_frames: &[u8] = &[0, 0];
        queue_board_answers(
            &shared,
            &[
                ANSWERED_IDLE,
                (lamella_runner::deploy::DEPLOY_STATUS_RESULT, 2, &held),
                (exec::EXEC_ACK, 3, started),
                (debug::EVT_STOPPED, 3, entry),
                (debug::DBG_FRAMES, 4, no_frames),
            ],
        );

        assert_eq!(backend.launch(), Ok(()));

        assert_eq!(
            sent_types(&shared),
            vec![debug::ABORT, lamella_runner::deploy::DEPLOY_STATUS, exec::EXEC, debug::DBG_STACK],
            "asked what the board holds, then started it -- and sent no image"
        );
    }

    /// **A launch takes back a board that is still running the program a disconnect left it.**
    ///
    /// A debugger's disconnect RESUMES the program, as VS Code's detach expects, and the next launch
    /// is usually a new adapter with no session of its own to close. A board running a program drops
    /// a deploy's questions, so this board's answers only line up behind an `ABORT`: the stop at the
    /// abort's own number, with the program's last output still ahead of it, and then the serve
    /// loop's answers to everything after.
    #[test]
    fn a_launch_takes_back_a_board_still_running_the_program_a_disconnect_left() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.timeout = Duration::from_millis(500);
        backend.image = image_with_checksum(0x1122_3344_5566_7788);
        let last_output: &[u8] = &[debug::output::STDOUT, 0, b'x'];
        let aborted: &[u8] = &[reason::ABORTED, 0, 0, 0, 0, 0, 0, 0, 0];
        let held = holds_verified(0x1122_3344_5566_7788);
        let started: &[u8] = &[exec::ack::STARTED];
        let entry: &[u8] = &[reason::ENTRY, 0, 0, 0, 0, 0, 0, 0, 0];
        let no_frames: &[u8] = &[0, 0];
        queue_board_answers(
            &shared,
            &[
                (debug::EVT_OUTPUT, 0, last_output),
                (debug::EVT_STOPPED, 1, aborted),
                (lamella_runner::deploy::DEPLOY_STATUS_RESULT, 2, &held),
                (exec::EXEC_ACK, 3, started),
                (debug::EVT_STOPPED, 3, entry),
                (debug::DBG_FRAMES, 4, no_frames),
            ],
        );

        assert_eq!(backend.launch(), Ok(()));

        assert_eq!(
            sent_types(&shared),
            vec![debug::ABORT, lamella_runner::deploy::DEPLOY_STATUS, exec::EXEC, debug::DBG_STACK],
            "stopped the program first, then asked what the board holds and started it"
        );
    }

    /// **F5 with an image larger than the board's window is refused before a byte is sent**, in the
    /// one sentence every host uses: both sizes and the board. Not as a chunk the target "refused".
    #[test]
    fn a_launch_refuses_an_image_larger_than_the_window_naming_both_sizes_and_the_board() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.board = Some("BBC micro:bit v2");
        backend.image = image_with_checksum(0x1122_3344_5566_7788);
        let mut nothing_held = vec![lamella_runner::deploy::deploy_state::NONE, 0];
        nothing_held.extend_from_slice(&0u64.to_le_bytes());
        nothing_held.extend_from_slice(&200u32.to_le_bytes());
        let write_failed: &[u8] = &[lamella_wire::msg::xfer::WRITE_FAILED, 0, 0, 0, 0];
        queue_board_answers(
            &shared,
            &[
                ANSWERED_IDLE,
                (lamella_runner::deploy::DEPLOY_STATUS_RESULT, 2, &nothing_held),
                (lamella_runner::deploy::DEPLOY_STATUS_RESULT, 3, &nothing_held),
                (lamella_runner::deploy::XFER_RESULT, 3, write_failed),
            ],
        );

        let Err(reason) = backend.launch() else {
            panic!("an image larger than the window must not be reported as launched");
        };

        assert_eq!(reason, crate::image_too_large(Some("BBC micro:bit v2"), 264, 200));
        assert!(
            !sent_types(&shared).contains(&lamella_runner::deploy::DEPLOY_IMAGE),
            "and not one chunk crossed: {:?}",
            sent_types(&shared)
        );
    }

    #[test]
    fn a_board_holding_a_different_image_is_sent_this_one() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.image = image_with_checksum(0x1122_3344_5566_7788);
        let other = holds_verified(0x0bad_0bad_0bad_0bad);
        queue_board_answers(
            &shared,
            &[ANSWERED_IDLE, (lamella_runner::deploy::DEPLOY_STATUS_RESULT, 2, &other)],
        );

        let Err(reason) = backend.launch() else {
            panic!("no chunk was acknowledged, so the deploy cannot have succeeded");
        };

        let types = sent_types(&shared);
        assert_eq!(types.get(..2), Some(&[debug::ABORT, lamella_runner::deploy::DEPLOY_STATUS][..]));
        assert!(
            types.contains(&lamella_runner::deploy::DEPLOY_IMAGE),
            "a board holding something else is sent the image: {types:?}"
        );
        assert!(reason.contains("acknowledge"), "{reason}");
    }

    #[test]
    fn a_board_that_cannot_say_what_it_holds_is_sent_the_image() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;
        backend.image = image_with_checksum(0x1122_3344_5566_7788);
        queue_board_answers(&shared, &[ANSWERED_IDLE]);

        assert!(backend.launch().is_err());

        assert!(sent_types(&shared).contains(&lamella_runner::deploy::DEPLOY_IMAGE));
    }

    /// A backend whose target was set running long enough ago that `poll` will ask about it.
    fn running_and_silent_since(seconds: u64) -> (WireHostBackend, Arc<Mutex<MemTransport>>) {
        let (mut backend, shared) = live_session(0);
        backend.running = true;
        backend.heard_at = Instant::now()
            .checked_sub(Duration::from_secs(seconds))
            .expect("a clock that has run this long");
        (backend, shared)
    }

    #[test]
    fn a_program_the_board_no_longer_executes_ends_the_session_as_a_fault() {
        let (mut backend, shared) = running_and_silent_since(3);

        assert!(matches!(backend.poll(), Stop::Running));
        assert_eq!(sent_types(&shared), vec![exec::EXEC_STATUS], "a silent target is asked");
        let (asked, _) = backend.status_asked.expect("the question is remembered");
        let idle: &[u8] = &[exec::ack::IDLE];
        queue_board_answers(&shared, &[(exec::EXEC_ACK, asked, idle)]);

        let Stop::Fault(reason) = backend.poll() else {
            panic!("a board executing nothing is not running the program");
        };
        assert!(reason.contains("nothing is executing"), "{reason}");
        assert!(!backend.session_live, "and there is no session left to detach");
    }

    #[test]
    fn a_quiet_program_is_asked_about_once_per_interval_and_left_running() {
        let (mut backend, shared) = live_session(0);
        backend.running = true;
        assert!(matches!(backend.poll(), Stop::Running));
        assert!(sent_types(&shared).is_empty(), "a target heard from just now is not asked");

        let (mut backend, shared) = running_and_silent_since(3);
        assert!(matches!(backend.poll(), Stop::Running));
        assert!(matches!(backend.poll(), Stop::Running));
        assert_eq!(sent_types(&shared), vec![exec::EXEC_STATUS], "asked once, not once per poll");

        let (asked, _) = backend.status_asked.expect("the question is remembered");
        let idle: &[u8] = &[exec::ack::IDLE];
        let running: &[u8] = &[exec::ack::RUNNING];
        queue_board_answers(
            &shared,
            &[(exec::EXEC_ACK, asked.wrapping_add(7), idle), (exec::EXEC_ACK, asked, running)],
        );
        assert!(matches!(backend.poll(), Stop::Running));
        assert!(matches!(backend.poll(), Stop::Running));
        assert!(backend.session_live);
    }

    /// One `EVT_OUTPUT` payload on `stream`.
    fn output_on(stream: u8, text: &str) -> Vec<u8> {
        let mut payload = vec![stream, 0];
        payload.extend_from_slice(text.as_bytes());
        payload
    }

    /// A trap stop: reason, a location, the exit code, flags.
    const TRAPPED: &[u8] = &[reason::TRAP, 0, 0, 0, 0, 0, 0, 0, 0, 70, 0, 0, 0, 0];

    #[test]
    fn a_trap_quotes_the_reason_the_target_sent_before_it() {
        use lamella_wire::msg::output;
        let (mut backend, shared) = live_session(0);
        backend.running = true;
        let reason_text = output_on(output::STDERR, "image does not boot: NotAnImage");
        queue_board_answers(&shared, &[(debug::EVT_OUTPUT, 0, &reason_text), (debug::EVT_STOPPED, 0, TRAPPED)]);

        assert!(matches!(backend.poll(), Stop::Running));
        let Stop::Fault(reason) = backend.poll() else {
            panic!("a trap ends the run as a fault");
        };
        assert_eq!(reason, "the target reported: image does not boot: NotAnImage");
        assert_eq!(
            backend.take_output(),
            None,
            "the report reaches the console once, in the fault, and not again as program output"
        );
    }

    #[test]
    fn error_output_that_is_not_the_last_word_is_not_quoted_by_the_trap() {
        use lamella_wire::msg::output;
        let (mut backend, shared) = live_session(0);
        backend.running = true;
        let earlier = output_on(output::STDERR, "a warning the program printed");
        let later = output_on(output::STDOUT, "more output\n");
        queue_board_answers(
            &shared,
            &[
                (debug::EVT_OUTPUT, 0, &earlier),
                (debug::EVT_OUTPUT, 0, &later),
                (debug::EVT_STOPPED, 0, TRAPPED),
            ],
        );

        assert!(matches!(backend.poll(), Stop::Running));
        assert!(matches!(backend.poll(), Stop::Running));
        let Stop::Fault(reason) = backend.poll() else {
            panic!("a trap ends the run as a fault");
        };
        assert_eq!(reason, "unhandled trap on the target");
        assert_eq!(
            backend.take_output().as_deref(),
            Some("a warning the program printedmore output\n"),
            "the unquoted error text is printed, in the order it arrived"
        );
    }

    #[test]
    fn error_output_before_a_normal_end_reaches_the_console() {
        use lamella_wire::msg::output;
        const FINISHED: &[u8] = &[reason::DONE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let (mut backend, shared) = live_session(0);
        backend.running = true;
        let report = output_on(output::STDERR, "written to the error stream");
        queue_board_answers(&shared, &[(debug::EVT_OUTPUT, 0, &report), (debug::EVT_STOPPED, 0, FINISHED)]);

        assert!(matches!(backend.poll(), Stop::Running));
        assert!(matches!(backend.poll(), Stop::Done));
        assert_eq!(backend.take_output().as_deref(), Some("written to the error stream"));
    }

    #[test]
    fn releasing_a_stopped_session_clears_the_breakpoints_and_resumes_the_program() {
        let (mut backend, shared) = live_session(1);
        backend.user_bps = vec![pack(1, 0)];

        assert_eq!(backend.release(), Ok(()));

        let sent = sent_frames(&shared);
        let types: Vec<u8> = sent.iter().map(|(msg_type, _)| *msg_type).collect();
        assert_eq!(types, vec![debug::DBG_BREAK, debug::DBG_RESUME]);
        assert_eq!(sent[0].1, vec![0, 0], "the breakpoint set it sends is empty");
        drop(backend);
        assert!(sent_types(&shared).is_empty(), "a released session is not detached afterwards");
    }

    #[test]
    fn releasing_a_running_session_clears_the_breakpoints_and_leaves_it_running() {
        let (mut backend, shared) = live_session(0);
        backend.user_bps = vec![pack(1, 0)];
        backend.running = true;
        let paused: &[u8] = &[reason::PAUSED, 0, 0, 0, 0, 0, 0, 0, 0];
        let no_frames: &[u8] = &[0, 0];
        let acknowledged: &[u8] = &[];
        queue_board_answers(
            &shared,
            &[
                (debug::EVT_STOPPED, 1, paused),
                (debug::DBG_FRAMES, 2, no_frames),
                (debug::DBG_ACK, 3, acknowledged),
            ],
        );

        assert_eq!(backend.release(), Ok(()));

        assert_eq!(
            sent_types(&shared),
            vec![debug::DBG_PAUSE, debug::DBG_STACK, debug::DBG_BREAK, debug::DBG_RESUME],
            "paused for the change, and resumed exactly once"
        );
        drop(backend);
        assert!(sent_types(&shared).is_empty(), "and it is not detached");
    }

    #[test]
    fn a_release_the_target_does_not_acknowledge_is_reported() {
        let (mut backend, shared) = live_session(0);
        backend.user_bps = vec![pack(1, 0)];

        let Err(reason) = backend.release() else {
            panic!("breakpoints nobody acknowledged removing may still be armed");
        };
        assert!(reason.contains("acknowledge"), "{reason}");
        assert_eq!(sent_types(&shared), vec![debug::DBG_BREAK], "nothing resumes past a failed removal");
        drop(backend);
        assert_eq!(sent_types(&shared), vec![debug::DBG_DETACH]);
    }

    #[test]
    fn releasing_a_host_with_no_live_session_sends_nothing() {
        let (mut backend, shared) = live_session(0);
        backend.session_live = false;

        assert_eq!(backend.release(), Ok(()));

        assert!(sent_types(&shared).is_empty());
    }

    #[test]
    fn srcmap_carries_qualified_method_names() {
        let json = br#"{ "methods": {
            "1154": { "document": "blink-rp2350.cs", "name": "BlinkRp2350.Main",
                      "points": [{ "o": 0, "l": 113, "c": 5 }] },
            "1147": { "document": "blink-rp2350.cs", "name": "Rp2350GpioDriver.SetPinMode",
                      "points": [{ "o": 0, "l": 65, "c": 5 }] }
        }, "entryPoint": 1154, "error": null }"#;
        let map = SrcMap::parse(json).expect("parse");
        assert_eq!(map.name_of(1154), Some("BlinkRp2350.Main"));
        assert_eq!(map.name_of(1147), Some("Rp2350GpioDriver.SetPinMode"));
        assert_eq!(map.name_of(9999), None);
    }

    #[test]
    fn nameless_srcmap_still_parses() {
        let json = br#"{ "methods": {
            "42": { "document": "a.cs", "points": [{ "o": 0, "l": 1, "c": 1 }] }
        }, "entryPoint": null, "error": null }"#;
        let map = SrcMap::parse(json).expect("parse");
        assert_eq!(map.name_of(42), None);
        assert!(map.location(42, 0).is_some());
    }

    #[test]
    fn identity_line_names_board_and_chip() {
        use lamella_wire::{TargetIdentity, chip_id_kind};

        let arm = |model: u16, port: u32| {
            TargetIdentity { product_model: model, ..TargetIdentity::default() }
                .with_chip_id(chip_id_kind::DEBUG_PORT_AND_DEVICE_ID, &port.to_le_bytes())
        };

        let line = super::identity_line(&arm(6, 0x0bc11477)).unwrap();
        assert!(line.contains("ATSAMW25 Xplained Pro") && line.contains("0x0bc11477"), "{line}");

        let bare = TargetIdentity { product_model: 4, ..TargetIdentity::default() };
        assert_eq!(super::identity_line(&bare).unwrap(), "Lamella Link: SAM E54 Xplained Pro\n");

        assert!(super::identity_line(&TargetIdentity::default()).is_none());

        let line = super::identity_line(&arm(0xffff, 0x2ba01477)).unwrap();
        assert!(line.contains("unrecognized product") && line.contains("0x2ba01477"), "{line}");

        let dated = TargetIdentity { firmware_version: [9734, 0], ..TargetIdentity::default() };
        assert_eq!(super::identity_line(&dated).unwrap(), "Lamella Link: unrecognized product, firmware 9734.0\n");

        let mut riscv_id = Vec::new();
        for word in [0x0000_0489u32, 0x8000_0001, 0x0000_0007] {
            riscv_id.extend_from_slice(&word.to_le_bytes());
        }
        let riscv = TargetIdentity::default()
            .with_chip_id(chip_id_kind::RISCV_MVENDOR_MARCH_MIMP, &riscv_id);
        let line = super::identity_line(&riscv).unwrap();
        assert!(line.contains("vendor 0x00000489") && line.contains("impl 0x00000007"), "{line}");
    }

    #[test]
    fn wire_value_payloads_decode() {
        use super::{WireValue, decode_children, decode_vars};

        let mut vars = Vec::new();
        vars.extend_from_slice(&4u16.to_le_bytes());
        vars.push(0x01);
        vars.extend_from_slice(&18i32.to_le_bytes());
        vars.push(0x06);
        vars.extend_from_slice(&5u32.to_le_bytes());
        vars.extend_from_slice(&0x0002_0000_0001u64.to_le_bytes());
        vars.push(0x07);
        vars.extend_from_slice(&2u16.to_le_bytes());
        vars.extend_from_slice(&0u64.to_le_bytes());
        vars.push(0x00);
        vars.extend_from_slice(&1u16.to_le_bytes());
        vars.push(0x08);
        vars.push(0);
        vars.extend_from_slice(&0u32.to_le_bytes());
        vars.extend_from_slice(&3u32.to_le_bytes());
        vars.extend_from_slice(&0u32.to_le_bytes());
        let (locals, args) = decode_vars(&vars).expect("the payload decodes");
        assert_eq!(
            locals,
            vec![
                WireValue::Int32(18),
                WireValue::Object { handle: 5, type_token: 0x0002_0000_0001 },
                WireValue::Struct { field_count: 2, type_token: 0 },
                WireValue::Null,
            ]
        );
        assert_eq!(args, vec![WireValue::ByRef { kind: 0, a: 0, b: 3, c: 0 }]);

        let mut kids = Vec::new();
        kids.extend_from_slice(&40_000u16.to_le_bytes());
        kids.extend_from_slice(&2u16.to_le_bytes());
        kids.push(6);
        kids.extend_from_slice(b"field0");
        kids.push(0x02);
        kids.extend_from_slice(&(-9i64).to_le_bytes());
        kids.push(3);
        kids.extend_from_slice(b"[1]");
        kids.push(0x04);
        kids.extend_from_slice(&1.5f64.to_le_bytes());
        let children = decode_children(&kids).expect("the payload decodes");
        assert_eq!(children.len(), 2, "this page");
        assert_eq!(super::children_total(&kids), Some(40_000), "and what it is a page of");
        assert_eq!(children[0], ("field0".to_string(), WireValue::Int64(-9)));
        assert_eq!(children[1], ("[1]".to_string(), WireValue::Float(1.5)));

        for cut in 0..vars.len() - 1 {
            let _ = decode_vars(&vars[..cut]);
        }
        for cut in 0..kids.len() - 1 {
            let _ = decode_children(&kids[..cut]);
        }
    }
}

#[cfg(test)]
mod variables_tests {
    use super::{
        Capabilities, Cell, ChildReference, DebugBackend, RefCell, Scope, SrcMap, Stop,
        WireHostBackend, WireTransport, WireValue, debug, reason, render,
    };
    use lamella_wire::{MemTransport, Transport};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Encodes one `<val>` as the wire spells it, so a test builds the bytes a board would send
    /// rather than reaching past the decoder it is exercising.
    fn encode(value: &WireValue, out: &mut Vec<u8>) {
        use debug::val;
        match *value {
            WireValue::Null => out.push(val::NULL),
            WireValue::Int32(value) => {
                out.push(val::INT32);
                out.extend_from_slice(&value.to_le_bytes());
            }
            WireValue::Object { handle, type_token } => {
                out.push(val::OBJECT);
                out.extend_from_slice(&handle.to_le_bytes());
                out.extend_from_slice(&type_token.to_le_bytes());
            }
            WireValue::Struct { field_count, type_token } => {
                out.push(val::STRUCT);
                out.extend_from_slice(&field_count.to_le_bytes());
                out.extend_from_slice(&type_token.to_le_bytes());
            }
            WireValue::String { units, ref text } => {
                out.push(val::STRING);
                out.extend_from_slice(&units.to_le_bytes());
                out.extend_from_slice(&(text.len() as u16).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            ref other => unreachable!("no test builds a {other:?} yet"),
        }
    }

    /// A `DBG_VARS` payload: `locals(u16 LE)` then their values, `args(u16 LE)` then theirs.
    fn vars_payload(locals: &[WireValue], arguments: &[WireValue]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(locals.len() as u16).to_le_bytes());
        for value in locals {
            encode(value, &mut payload);
        }
        payload.extend_from_slice(&(arguments.len() as u16).to_le_bytes());
        for value in arguments {
            encode(value, &mut payload);
        }
        payload
    }

    /// A source map naming `locals` (as `(slot, name)`) in method 7, with one sequence point so the
    /// map parses at all.
    fn srcmap_naming(locals: &[(u32, &str)]) -> SrcMap {
        let named: Vec<String> = locals
            .iter()
            .map(|(slot, name)| format!("{{\"index\":{slot},\"name\":\"{name}\"}}"))
            .collect();
        let json = format!(
            "{{\"methods\":{{\"7\":{{\"document\":\"Program.cs\",\"name\":\"P.Main\",\
             \"points\":[{{\"o\":0,\"l\":3,\"c\":1}}],\"locals\":[{}]}}}}}}",
            named.join(",")
        );
        SrcMap::parse(json.as_bytes()).expect("the source map parses")
    }

    /// Every debug capability, for a test that is not about capabilities.
    const ALL_CAPS: u64 = u64::MAX;

    /// A halted backend whose stack is one frame in method 7, with `reply` already queued as the
    /// board's answer to the `DBG_LOCALS` it is about to be asked.
    fn halted_in_method_7(
        reply: Option<&[u8]>,
        srcmap: Option<SrcMap>,
        caps: u64,
    ) -> WireHostBackend {
        halted_with(reply, srcmap, caps, vec![(7, 0)]).0
    }

    /// As [`halted_in_method_7`], with the stack given and the shared transport handed back so a
    /// test can read what the host actually sent.
    fn halted_with(
        reply: Option<&[u8]>,
        srcmap: Option<SrcMap>,
        caps: u64,
        frames: Vec<(u32, u32)>,
    ) -> (WireHostBackend, Arc<Mutex<MemTransport>>) {
        let mut host = MemTransport::new();
        if let Some(payload) = reply {
            let mut board = MemTransport::new();
            board.send(debug::DBG_VARS, 1, payload).expect("queue the board's answer");
            let queued = board.take_sent();
            host.feed(&queued);
        }
        let shared = Arc::new(Mutex::new(host));
        let backend = WireHostBackend {
            transport: RefCell::new(WireTransport::Mem(Arc::clone(&shared))),
            srcmap,
            user_bps: Vec::new(),
            image: Vec::new(),
            timeout: Duration::from_millis(50),
            seq: Cell::new(0),
            target_caps: Capabilities(caps),
            board: None,
            session_live: true,
            running: false,
            frames,
            exit_code: 0,
            pending_output: RefCell::new(None),
            pending_debug_output: RefCell::new(None),
            last_error: RefCell::new(None),
            heard_at: Instant::now(),
            status_asked: None,
            selectors: RefCell::new(Vec::new()),
            first_reference: 0,
        };
        (backend, shared)
    }

    /// The rows as `(name, value, kind)`, which is the whole of what a pane shows.
    fn rows(backend: &WireHostBackend, scope: Scope) -> Vec<(String, String, String)> {
        backend
            .variables(0, scope)
            .into_iter()
            .map(|row| (row.name, row.value, row.kind))
            .collect()
    }

    /// A halted frame's locals reach the pane, named from the source map.
    #[test]
    fn a_halted_frames_locals_arrive_named() {
        let payload = vars_payload(&[WireValue::Int32(41), WireValue::Int32(-7)], &[]);
        let backend = halted_in_method_7(
            Some(&payload),
            Some(srcmap_naming(&[(0, "total"), (1, "delta")])),
            ALL_CAPS,
        );
        assert_eq!(
            rows(&backend, Scope::Locals),
            vec![
                ("total".to_string(), "41".to_string(), "int".to_string()),
                ("delta".to_string(), "-7".to_string(), "int".to_string()),
            ],
        );
    }

    /// A name is matched by its recorded slot, not by its position in the list.
    ///
    /// A Portable PDB names only the locals it has names for, so the list is a SUBSET of the
    /// frame's slots: here slot 0 is a compiler temp the map does not name and slot 1 is `total`.
    /// Indexing the list by the wire's slot number would label slot 0 `total` -- a pane that reads
    /// perfectly and names the wrong value, which is worse than showing no name.
    #[test]
    fn an_unnamed_slot_does_not_borrow_the_next_names_label() {
        let payload = vars_payload(&[WireValue::Int32(999), WireValue::Int32(41)], &[]);
        let backend =
            halted_in_method_7(Some(&payload), Some(srcmap_naming(&[(1, "total")])), ALL_CAPS);
        let named: Vec<(String, String)> = rows(&backend, Scope::Locals)
            .into_iter()
            .map(|(name, value, _)| (name, value))
            .collect();
        assert_eq!(
            named,
            vec![
                ("local0".to_string(), "999".to_string()),
                ("total".to_string(), "41".to_string()),
            ],
            "the unnamed slot keeps its slot spelling and `total` stays on the value it names",
        );
    }

    /// A map with no `locals` lane at all still shows the values, by slot.
    ///
    /// Which is what keeps a source map written before this lane existed from turning a missing
    /// NAME into a missing SESSION.
    #[test]
    fn a_map_without_names_still_shows_the_values() {
        let payload = vars_payload(&[WireValue::Int32(5)], &[]);
        let backend = halted_in_method_7(Some(&payload), Some(srcmap_naming(&[])), ALL_CAPS);
        assert_eq!(rows(&backend, Scope::Locals)[0].0, "local0");
    }

    /// Arguments come from the same reply's second half, and are spelled `argN`.
    ///
    /// A parameter's name is in the assembly's `Param` table rather than in the Portable PDB, so
    /// the source map cannot name one and this does not pretend otherwise.
    #[test]
    fn arguments_come_from_the_replys_second_half() {
        let payload =
            vars_payload(&[WireValue::Int32(1)], &[WireValue::Int32(2), WireValue::Null]);
        let backend =
            halted_in_method_7(Some(&payload), Some(srcmap_naming(&[(0, "total")])), ALL_CAPS);
        assert_eq!(
            rows(&backend, Scope::Arguments),
            vec![
                ("arg0".to_string(), "2".to_string(), "int".to_string()),
                ("arg1".to_string(), "null".to_string(), "object".to_string()),
            ],
            "the arguments, not the locals -- one reply carries both and the scope picks",
        );
    }

    /// The evaluation stack is empty rather than refused: a compiled target has none.
    #[test]
    fn the_evaluation_stack_scope_is_empty_and_costs_no_round_trip() {
        let backend = halted_in_method_7(None, None, ALL_CAPS);
        assert!(backend.variables(0, Scope::Stack).is_empty());
    }

    /// A silent target says so instead of showing an empty pane.
    ///
    /// A variables pane draws the distinction the `read_memory` contract draws: "this read failed"
    /// is not "there is nothing here". Someone opens this pane to find out whether a value is what
    /// they think it is, so a wire that dropped has to reach them.
    #[test]
    fn a_target_that_does_not_answer_is_reported_rather_than_shown_as_empty() {
        let backend = halted_in_method_7(None, None, ALL_CAPS);
        let rows = rows(&backend, Scope::Locals);
        assert_eq!(rows.len(), 1, "one diagnostic row, not an empty pane");
        assert_eq!(rows[0].0, "<unavailable>");
        assert!(
            rows[0].1.contains("did not answer"),
            "and it says what went wrong: {}",
            rows[0].1,
        );
    }

    /// A frame index the target's current stack does not hold is refused here.
    ///
    /// It has to be, because `DBG_VARS` answers `0, 0` to an unknown frame index -- byte-identical
    /// to a frame that genuinely holds nothing. Only the host's own stack can tell them apart.
    #[test]
    fn a_frame_outside_the_current_stack_is_refused_not_asked_about() {
        let payload = vars_payload(&[], &[]);
        let backend = halted_in_method_7(Some(&payload), None, ALL_CAPS);
        let rows = backend.variables(4, Scope::Locals);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "<unavailable>");
        assert!(
            rows[0].value.contains("call stack"),
            "and it names the reason: {}",
            rows[0].value,
        );
    }

    /// A firmware that never advertised LOCALS is reported as such, and is never asked.
    ///
    /// "This target cannot answer" and "this frame has nothing" are different facts, and only the
    /// capability distinguishes them before a round trip is spent.
    #[test]
    fn a_target_without_the_locals_capability_says_so() {
        let payload = vars_payload(&[WireValue::Int32(1)], &[]);
        let backend = halted_in_method_7(Some(&payload), None, !Capabilities::LOCALS);
        let rows = rows(&backend, Scope::Locals);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].1.contains("LOCALS"),
            "and it names the missing capability: {}",
            rows[0].1,
        );
    }

    /// A running target is not asked: between stops the values are in motion.
    #[test]
    fn a_running_target_is_told_to_pause_rather_than_read_mid_flight() {
        let mut backend = halted_in_method_7(None, None, ALL_CAPS);
        backend.running = true;
        let rows = rows(&backend, Scope::Locals);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.contains("running"), "{}", rows[0].1);
    }

    /// Between sessions the pane is EMPTY rather than diagnostic: with no paused program there is
    /// no subject, and a refusal row in every not-yet-started session would be noise.
    #[test]
    fn no_session_shows_nothing_rather_than_a_refusal() {
        let mut backend = halted_in_method_7(None, None, ALL_CAPS);
        backend.session_live = false;
        assert!(backend.variables(0, Scope::Locals).is_empty());
    }

    /// One `DBG_LOCALS` goes out per call, carrying the frame index the caller asked about -- so a
    /// stop costs nothing until a pane is opened, which is the whole reason the carrier is behind a
    /// `RefCell` rather than the values being cached for every frame at every stop.
    #[test]
    fn one_request_per_call_carries_the_frame_index() {
        let payload = vars_payload(&[WireValue::Int32(1)], &[]);
        let (backend, shared) =
            halted_with(Some(&payload), None, ALL_CAPS, vec![(7, 0), (9, 4)]);
        let _ = backend.variables(1, Scope::Locals);
        let bytes = shared.lock().expect("the test transport").take_sent();
        let mut reader = MemTransport::new();
        reader.feed(&bytes);
        let mut asked = Vec::new();
        while let Ok(Some(frame)) = reader.poll() {
            asked.push((frame.msg_type, frame.payload.to_vec()));
        }
        assert_eq!(asked.len(), 1, "one round trip, not one per frame");
        assert_eq!(asked[0].0, debug::DBG_LOCALS);
        assert_eq!(asked[0].1, 1u16.to_le_bytes(), "the frame the caller asked about");
    }

    /// The renderings, including the two that must not look like numbers.
    ///
    /// An object's handle is a heap slot and is labelled as one, because a bare hex number reads as
    /// a pointer and invites someone to follow it in a memory view. A managed pointer is not
    /// dereferenced at all: showing its descriptor words would be showing the machinery and calling
    /// it somebody's variable.
    #[test]
    fn a_value_that_is_not_a_number_never_renders_as_one() {
        assert_eq!(render(&WireValue::Int32(7)), ("7".to_string(), "int".to_string()));
        assert_eq!(render(&WireValue::Null), ("null".to_string(), "object".to_string()));
        assert_eq!(
            render(&WireValue::Object { handle: 3, type_token: 0 }),
            ("object #3".to_string(), "object".to_string()),
        );
        assert_eq!(
            render(&WireValue::Struct { field_count: 1, type_token: 0 }).0,
            "1 field",
            "singular, because a pane reads as prose",
        );
        assert_eq!(render(&WireValue::Struct { field_count: 3, type_token: 0 }).0, "3 fields");
        assert_eq!(
            render(&WireValue::ByRef { kind: 1, a: 0xdead_beef, b: 0, c: 0 }),
            ("<managed pointer>".to_string(), "byref".to_string()),
            "the descriptor words are machinery and never reach the pane",
        );
    }

    /// A diagnostic row can never be mistaken for a program variable, which is what keeps a hover
    /// honest: `evaluate` resolves a hovered name by matching it against these rows, so a row named
    /// like an identifier could be returned as the value of somebody's variable.
    #[test]
    fn a_diagnostic_row_is_named_so_no_identifier_can_match_it() {
        let backend = halted_in_method_7(None, None, ALL_CAPS);
        for row in backend.variables(0, Scope::Locals) {
            assert!(
                row.name.starts_with('<') && row.name.ends_with('>'),
                "{} would be a legal C# identifier",
                row.name,
            );
        }
    }

    /// A `DBG_CHILDREN` payload: the value's total, then this page's `(name, value)` members.
    fn children_payload(total: u16, members: &[(&str, WireValue)]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&total.to_le_bytes());
        payload.extend_from_slice(&(members.len() as u16).to_le_bytes());
        for (name, value) in members {
            payload.push(name.len() as u8);
            payload.extend_from_slice(name.as_bytes());
            encode(value, &mut payload);
        }
        payload
    }

    /// Queues `(type, payload)` frames as the board's next answers.
    fn queue(shared: &Arc<Mutex<MemTransport>>, answers: &[(u8, &[u8])]) {
        let mut board = MemTransport::new();
        for &(msg_type, payload) in answers {
            board.send(msg_type, 0, payload).expect("queue the board's answer");
        }
        let queued = board.take_sent();
        shared.lock().expect("the test transport").feed(&queued);
    }

    /// The payloads of the `DBG_EXPAND` requests the host has sent since the last call.
    fn expand_requests(shared: &Arc<Mutex<MemTransport>>) -> Vec<Vec<u8>> {
        let bytes = shared.lock().expect("the test transport").take_sent();
        let mut reader = MemTransport::new();
        reader.feed(&bytes);
        let mut requests = Vec::new();
        while let Ok(Some(frame)) = reader.poll() {
            if frame.msg_type == debug::DBG_EXPAND {
                requests.push(frame.payload.to_vec());
            }
        }
        requests
    }

    /// The object local every expansion test starts from: frame 0 of method 7, local slot 0.
    fn halted_on_an_object() -> (WireHostBackend, Arc<Mutex<MemTransport>>, ChildReference) {
        let payload = vars_payload(&[WireValue::Object { handle: 3, type_token: 0 }], &[]);
        let (backend, shared) = halted_with(Some(&payload), None, ALL_CAPS, vec![(7, 0)]);
        let (_, reference) = backend.variables_with_children(0, Scope::Locals).remove(0);
        let _ = expand_requests(&shared);
        (backend, shared, reference)
    }

    #[test]
    fn an_object_or_a_struct_with_fields_can_be_opened_and_a_number_cannot() {
        let payload = vars_payload(
            &[
                WireValue::Int32(5),
                WireValue::Object { handle: 3, type_token: 0 },
                WireValue::Struct { field_count: 2, type_token: 0 },
                WireValue::Struct { field_count: 0, type_token: 0 },
                WireValue::Null,
            ],
            &[],
        );
        let (backend, _shared) = halted_with(Some(&payload), None, ALL_CAPS, vec![(7, 0)]);
        let references: Vec<ChildReference> = backend
            .variables_with_children(0, Scope::Locals)
            .into_iter()
            .map(|(_, reference)| reference)
            .collect();
        assert_eq!(references[0], ChildReference::NONE, "a number has nothing inside it");
        assert_ne!(references[1], ChildReference::NONE, "an object does");
        assert_ne!(references[2], ChildReference::NONE, "a struct with fields does");
        assert_eq!(references[3], ChildReference::NONE, "a struct with no fields has nothing to show");
        assert_eq!(references[4], ChildReference::NONE, "neither does a null");
        assert_ne!(references[1], references[2], "and each names its own value");
    }

    #[test]
    fn plain_variables_hands_out_no_references() {
        let payload = vars_payload(&[WireValue::Object { handle: 3, type_token: 0 }], &[]);
        let (backend, _shared) = halted_with(Some(&payload), None, ALL_CAPS, vec![(7, 0)]);
        assert_eq!(backend.variables(0, Scope::Locals).len(), 1);
        assert!(backend.selectors.borrow().is_empty());
    }

    #[test]
    fn a_values_members_are_listed_as_the_target_names_them_and_open_in_turn() {
        let (backend, shared, counter) = halted_on_an_object();
        queue(
            &shared,
            &[(
                debug::DBG_CHILDREN,
                &children_payload(
                    2,
                    &[
                        ("field0", WireValue::Object { handle: 9, type_token: 0 }),
                        ("field1", WireValue::Int32(33)),
                    ],
                ),
            )],
        );

        let members = backend.children(counter);

        let shown: Vec<(&str, &str)> =
            members.iter().map(|(row, _)| (row.name.as_str(), row.value.as_str())).collect();
        assert_eq!(shown, vec![("field0", "object #9"), ("field1", "33")]);
        assert_ne!(members[0].1, ChildReference::NONE, "a member that is an object opens too");
        assert_eq!(members[1].1, ChildReference::NONE);
        assert_eq!(expand_requests(&shared), vec![vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 1]]);

        queue(&shared, &[(debug::DBG_CHILDREN, &children_payload(1, &[("[0]", WireValue::Int32(4))]))]);
        let nested = backend.children(members[0].1);
        assert_eq!(nested[0].0.value, "4");
        assert_eq!(expand_requests(&shared), vec![vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1]]);
    }

    #[test]
    fn a_reference_kept_from_an_earlier_stop_is_refused_rather_than_answered_with_another_value() {
        let (mut backend, shared, old) = halted_on_an_object();
        backend.running = true;
        let stop: &[u8] = &[reason::BREAKPOINT, 7, 0, 0, 0, 0, 0, 0, 0];
        let one_frame: &[u8] = &[1, 0, 7, 0, 0, 0, 0, 0, 0, 0];
        let payload = vars_payload(&[WireValue::Object { handle: 5, type_token: 0 }], &[]);
        queue(
            &shared,
            &[(debug::EVT_STOPPED, stop), (debug::DBG_FRAMES, one_frame), (debug::DBG_VARS, &payload)],
        );
        assert!(matches!(backend.poll(), Stop::Breakpoint));
        let (_, fresh) = backend.variables_with_children(0, Scope::Locals).remove(0);
        let _ = expand_requests(&shared);

        assert_ne!(old, fresh, "a reference is never reused within a session");
        let rows = backend.children(old);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "<unavailable>");
        assert!(rows[0].0.value.contains("earlier stop"), "{}", rows[0].0.value);
        assert!(expand_requests(&shared).is_empty(), "and nothing is asked of the target");
    }

    #[test]
    fn a_value_with_more_members_than_one_page_says_how_many_it_left_out() {
        let (backend, shared, reference) = halted_on_an_object();
        queue(
            &shared,
            &[(
                debug::DBG_CHILDREN,
                &children_payload(300, &[("[0]", WireValue::Int32(1)), ("[1]", WireValue::Int32(2))]),
            )],
        );
        let rows = backend.children(reference);
        let last = &rows.last().expect("rows").0;
        assert_eq!((last.name.as_str(), last.value.as_str()), ("<more>", "298 more not listed"));
    }

    #[test]
    fn a_string_shows_its_text_and_is_not_offered_as_something_to_open() {
        let payload = vars_payload(&[WireValue::String { units: 3, text: "sum".to_string() }], &[]);
        let (backend, _shared) = halted_with(Some(&payload), None, ALL_CAPS, vec![(7, 0)]);
        let (row, children) = backend.variables_with_children(0, Scope::Locals).remove(0);
        assert_eq!((row.value.as_str(), row.kind.as_str()), ("\"sum\"", "string"));
        assert_eq!(children, ChildReference::NONE, "its text is the whole of what it shows");
    }

    #[test]
    fn a_string_the_target_cut_short_says_how_long_it_really_is() {
        let cut = WireValue::String { units: 1000, text: "abc".to_string() };
        assert_eq!(render(&cut).0, "\"abc\"... (Length 1000)");
        let whole = WireValue::String { units: 3, text: "abc".to_string() };
        assert_eq!(render(&whole).0, "\"abc\"", "and a whole one says nothing more");
    }

    #[test]
    fn a_strings_own_quotes_and_line_breaks_cannot_read_as_its_end() {
        let tricky = WireValue::String { units: 7, text: "a\"b\nc\\d".to_string() };
        assert_eq!(render(&tricky).0, "\"a\\\"b\\nc\\\\d\"");
    }

    #[test]
    fn a_value_the_target_lists_nothing_inside_says_so_instead_of_opening_onto_nothing() {
        let (backend, shared, reference) = halted_on_an_object();
        queue(&shared, &[(debug::DBG_CHILDREN, &children_payload(0, &[]))]);
        let rows = backend.children(reference);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "<no members>");
        assert_eq!(rows[0].1, ChildReference::NONE);
    }
}
