//! [`WirelineBackend`]: the [`lamella_debug_backend::DebugBackend`] seam implemented over
//! the wireline debug channel -- VS Code (via lamella-dap) debugs code running ON A DEVICE
//! with zero adapter changes.

use crate::{SerialTransport, deploy_chunked_blocking, hello_blocking};
#[cfg(feature = "usb")]
use crate::{UsbTransport, parse_usb_target};
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, SourceLocation, Stop, Variable,
};
use lamella_runner::RunResult;
use lamella_runner::debug::{self, reason};
use lamella_wire::{Capabilities, Frame as WireFrame, Transport, TransportError};
use std::time::{Duration, Instant};

/// Packs a wire `(method_id, offset)` location into the seam's opaque address.
fn pack(method: u32, offset: u32) -> u64 {
    (u64::from(method) << 32) | u64::from(offset)
}

/// The inverse of [`pack`].
fn unpack(address: u64) -> (u32, u32) {
    ((address >> 32) as u32, address as u32)
}

/// The carrier a [`WirelineBackend`] drives: a serial port (USB-CDC / UART / a debug-probe VCP), or --
/// with the `usb` feature -- a board's native driverless-WinUSB device. A LOCAL enum (not `Box<dyn>`) so it
/// implements the foreign `Transport` trait, and the blocking deploy/hello drivers take it directly.
pub enum WireTransport {
    Serial(SerialTransport),
    #[cfg(feature = "usb")]
    Usb(UsbTransport),
}

impl Transport for WireTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        match self {
            WireTransport::Serial(t) => t.send(msg_type, seq, payload),
            #[cfg(feature = "usb")]
            WireTransport::Usb(t) => t.send(msg_type, seq, payload),
        }
    }
    fn poll(&mut self) -> Result<Option<WireFrame>, TransportError> {
        match self {
            WireTransport::Serial(t) => t.poll(),
            #[cfg(feature = "usb")]
            WireTransport::Usb(t) => t.poll(),
        }
    }
}

/// A pre-built source map for the DEPLOYED image (`lamella_srcmap`'s JSON, keyed by the SAME method_id the wire
/// reports), loaded alongside the `.lmli` so a wireline session is SOURCE-LEVEL. `points` are `(il_offset, line,
/// column)` ascending by offset -- the wire reports the CIL offset directly, so no index conversion is needed.
struct MethodSrc {
    document: String,
    /// The method's qualified display name (`Type.Method`), for stack frames. Empty if the map predates names.
    name: String,
    points: Vec<(u32, u32, u32)>,
}

pub struct SrcMap {
    methods: std::collections::HashMap<u32, MethodSrc>,
}

impl SrcMap {
    /// Parse `lamella_srcmap`'s JSON: `{ methods: { "<id>": { document, points: [{o,l,c}] } } }`.
    pub fn parse(json: &[u8]) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct RawPoint { o: u32, l: u32, #[serde(default)] c: u32 }
        #[derive(serde::Deserialize)]
        struct RawMethod { document: String, #[serde(default)] name: String, points: Vec<RawPoint> }
        #[derive(serde::Deserialize)]
        struct Raw { methods: std::collections::HashMap<String, RawMethod> }
        let raw: Raw = serde_json::from_slice(json).ok()?;
        let mut methods = std::collections::HashMap::new();
        for (id, m) in raw.methods {
            let Ok(id) = id.parse::<u32>() else { continue };
            let mut points: Vec<(u32, u32, u32)> = m.points.into_iter().map(|p| (p.o, p.l, p.c)).collect();
            points.sort_by_key(|point| point.0);
            methods.insert(id, MethodSrc { document: m.document, name: m.name, points });
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

    /// Every sequence-point offset in `method` (the temp-breakpoint set for a source step-over into a call).
    fn points_of(&self, method: u32) -> Vec<u32> {
        self.methods
            .get(&method)
            .map_or(Vec::new(), |source| source.points.iter().map(|&(o, _, _)| o).collect())
    }
}

/// A [`DebugBackend`] driving a wireline target's on-device interpreter session.
pub struct WirelineBackend {
    transport: WireTransport,
    /// The deployed image's source map, if present -- makes the session source-level; `None` => IL-level.
    srcmap: Option<SrcMap>,
    /// The user's current breakpoint addresses (from set_breakpoints) -- kept armed alongside the temp breakpoints
    /// run_to_return() uses, so a source step-over never drops a user breakpoint.
    user_bps: Vec<u64>,
    image: Vec<u8>,
    timeout: Duration,
    seq: u16,
    /// A debug session is live on the target (between `DBG_IMAGE` and Done/Trap/detach).
    session_live: bool,
    /// A resume is in flight: [`DebugBackend::poll`] watches for its stop event.
    running: bool,
    /// The call stack cached at the last stop, innermost first.
    frames: Vec<(u32, u32)>,
    exit_code: i32,
    pending_output: Option<String>,
}

impl WirelineBackend {
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

    /// Open a NATIVE-USB (driverless WinUSB) wireline target by `vid`/`pid` + an optional serial
    /// substring (the picker key: an RP2350 reports its 16-hex chip id, the F427 `F427-0001`).
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
                | Capabilities::BAKED_IMAGE,
        );
        let session = hello_blocking(&mut transport, 0, caps, timeout)?;
        if !(session.caps.has(Capabilities::DEBUG_BASIC)
            && session.caps.has(Capabilities::BREAKPOINTS)
            && session.caps.has(Capabilities::STEPPING))
        {
            return Err(TransportError::Closed);
        }
        Ok(Self {
            transport,
            image,
            timeout,
            seq: 0,
            session_live: false,
            running: false,
            frames: Vec::new(),
            exit_code: 0,
            pending_output: None,
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

    fn next_seq(&mut self) -> u16 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Blocks until a frame of `msg_type` arrives (dropping others -- the protocol runs
    /// one command in flight), or the timeout passes.
    fn await_type(&mut self, msg_type: u8) -> Option<WireFrame> {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            match self.transport.poll() {
                Ok(Some(frame)) if frame.msg_type == msg_type => return Some(frame),
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => return None,
            }
        }
        None
    }

    /// Folds an `EVT_STOPPED` into backend state and the seam's [`Stop`].
    fn on_stopped(&mut self, frame: &WireFrame) -> Stop {
        let why = frame.payload.first().copied().unwrap_or(reason::TRAP);
        match why {
            reason::DONE | reason::TRAP => {
                self.session_live = false;
                self.running = false;
                self.frames.clear();
                let tail = frame.payload.get(9..).unwrap_or(&[]);
                let text = if let Some(result) = RunResult::decode(tail) {
                    self.exit_code = result.exit;
                    if !result.stdout.is_empty() {
                        self.pending_output = Some(result.stdout.clone());
                    }
                    result.stdout
                } else {
                    String::new()
                };
                if why == reason::DONE {
                    Stop::Done
                } else {
                    Stop::Fault(if text.is_empty() {
                        "unhandled trap on the target".to_string()
                    } else {
                        text
                    })
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
        if self.transport.send(debug::DBG_STACK, seq, &[]).is_err() {
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
    fn send_breakpoints(&mut self, addresses: &[u64]) {
        let mut payload = Vec::with_capacity(2 + addresses.len() * 8);
        payload.extend_from_slice(&(addresses.len() as u16).to_le_bytes());
        for &address in addresses {
            let (method, offset) = unpack(address);
            payload.extend_from_slice(&method.to_le_bytes());
            payload.extend_from_slice(&offset.to_le_bytes());
        }
        let seq = self.next_seq();
        if self.transport.send(debug::DBG_BREAK, seq, &payload).is_ok() {
            self.await_type(debug::DBG_ACK);
        }
    }
}

impl DebugBackend for WirelineBackend {
    fn launch(&mut self) -> bool {
        if self.session_live {
            let seq = self.next_seq();
            if self.transport.send(debug::DBG_DETACH, seq, &[]).is_ok() {
                self.await_type(debug::DBG_ACK);
            }
            self.session_live = false;
        }
        self.running = false;
        self.exit_code = 0;
        let seq = self.next_seq();
        if !matches!(
            deploy_chunked_blocking(&mut self.transport, seq, &self.image, 8 * 1024, self.timeout),
            Ok(true)
        ) {
            return false;
        }
        let seq = self.next_seq();
        if self.transport.send(debug::DBG_ATTACH, seq, &[]).is_err() {
            return false;
        }
        let Some(stop) = self.await_type(debug::EVT_STOPPED) else {
            return false;
        };
        self.session_live = true;
        !matches!(self.on_stopped(&stop), Stop::Fault(_))
    }

    fn resume(&mut self) -> Stop {
        if !self.session_live {
            return Stop::Done;
        }
        let seq = self.next_seq();
        if self.transport.send(debug::DBG_RESUME, seq, &[]).is_err() {
            return Stop::Fault("the wire dropped".to_string());
        }
        self.running = true;
        Stop::Running
    }

    fn poll(&mut self) -> Stop {
        if !self.running {
            return if self.session_live { Stop::Step } else { Stop::Done };
        }
        match self.transport.poll() {
            Ok(Some(frame)) if frame.msg_type == debug::EVT_STOPPED => self.on_stopped(&frame),
            Ok(_) => Stop::Running,
            Err(_) => Stop::Fault("the wire dropped".to_string()),
        }
    }

    fn pause(&mut self) -> bool {
        if !self.session_live || !self.running {
            return true;
        }
        let seq = self.next_seq();
        if self.transport.send(debug::DBG_PAUSE, seq, &[]).is_err() {
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

    fn step(&mut self) -> Stop {
        if !self.session_live {
            return Stop::Done;
        }
        let seq = self.next_seq();
        if self.transport.send(debug::DBG_STEP, seq, &[]).is_err() {
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

    fn set_breakpoints(&mut self, addresses: &[u64]) {
        self.user_bps = addresses.to_vec();
        if self.session_live && self.running {
            self.pause();
            self.send_breakpoints(addresses);
            let seq = self.next_seq();
            if self.transport.send(debug::DBG_RESUME, seq, &[]).is_ok() {
                self.running = true;
            }
        } else {
            self.send_breakpoints(addresses);
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
        self.send_breakpoints(&armed);
        let seq = self.next_seq();
        if self.transport.send(debug::DBG_RESUME, seq, &[]).is_err() {
            return Stop::Fault("the wire dropped".to_string());
        }
        self.running = true;
        let stop = match self.await_type(debug::EVT_STOPPED) {
            Some(frame) => self.on_stopped(&frame),
            None => Stop::Fault("run-to-return timed out".to_string()),
        };
        let restore = self.user_bps.clone();
        self.send_breakpoints(&restore);
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

    fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
        Vec::new()
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
        self.pending_output.take()
    }
}

#[cfg(test)]
mod tests {
    use super::SrcMap;

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
}
