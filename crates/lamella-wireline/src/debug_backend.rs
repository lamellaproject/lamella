//! [`WirelineBackend`]: the [`lamella_debug_backend::DebugBackend`] seam implemented over
//! the wireline debug channel -- VS Code (via lamella-dap) debugs code running ON A DEVICE
//! with zero adapter changes.

use crate::{SerialTransport, hello_blocking};
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, Stop, Variable,
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

/// A [`DebugBackend`] driving a wireline target's on-device interpreter session.
pub struct WirelineBackend {
    transport: SerialTransport,
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
        let mut transport = SerialTransport::open(port, baud)?;
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
        })
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
        if self.transport.send(debug::DBG_IMAGE, seq, &self.image).is_err() {
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
        let seq = self.next_seq();
        self.transport.send(debug::DBG_PAUSE, seq, &[]).is_ok()
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

    fn stack(&self) -> Vec<Frame> {
        self.frames
            .iter()
            .map(|&(method, offset)| Frame {
                address: pack(method, offset),
                name: format!("method {method}"),
                line: offset + 1,
            })
            .collect()
    }

    fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
        Vec::new()
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
