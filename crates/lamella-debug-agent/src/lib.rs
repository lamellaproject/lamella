//! The on-target Lamella Link debug agent: the half that answers debug frames from INSIDE a
//! running image, over the [`lamella_wire::Transport`] carrier seam.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use lamella_wire::{Transport, TransportError};


/// The message types of the LIVE debug agent: read and write the target's memory **while a deployed
/// program is still running**, without stopping it.
///
/// This is the on-target half of a host evaluating against a live program: the host runs the
/// interpreter and redirects its loads and stores over the wire to here. It is deliberately the
/// smallest thing that can answer that question -- an address and a length -- because that primitive
/// is the same on every tier. An interpreted program's state lives on a heap the host cannot name
/// and a compiled program's lives at addresses a symbol map does name, and neither changes what this
/// op does.
///
/// # Why this is a distinct block from the DEBUG ops rather than more of them
///
/// The debug block is a HALTED channel. Its own contract says so: a frame's variables are read while
/// halted, because between stops the values are in motion. Every op there presumes a program stopped
/// at a known point, and several of them have no meaning otherwise. These two ops presume the
/// opposite. Mixing them would give one range two contracts, with nothing in a message type to say
/// which one a target is honoring.
///
/// # What a running target's answer does NOT promise
///
/// **A multi-word read is not atomic with respect to the program.** The agent is serviced between
/// the program's instructions, so a structure the program updates in more than one store can be read
/// half-updated. A host that renders such a value as though it were consistent is worse than one
/// that refuses: showing a torn value is a wrong answer presented as a right one. Nothing on the
/// target can fix this -- the target does not know which words belong together -- so the host must
/// either read something it knows is single-word, or read twice and compare, or say plainly that the
/// value was in motion.
///
/// **A write may not be what the program reads next.** On a compiled tier the program may hold the
/// location in a register across the write, so the store lands in memory and the program keeps using
/// the stale copy. That is a property of the program's code, not of this op.
///
/// Both are reasons for a host to be careful, not reasons for a target to refuse: the alternative to
/// an inexact live read is halting a controller, which for a machine that is actually running
/// something is the more expensive of the two.
pub mod live {
    pub use lamella_wire::msg::{LIVE_DATA, LIVE_READ, LIVE_WRITE, LIVE_WROTE};
    pub use lamella_wire::msg::live_status as status;

    /// The most bytes one [`LIVE_READ`] may ask for.
    ///
    /// The bound is not about buffer space; it is about the program. Servicing a read is time the
    /// deployed program is not running, and that time is proportional to the length, so bounding the
    /// length is the only way the target bounds the stall it imposes on a program it is supposed to
    /// be leaving alone. A host inspecting a variable needs a handful of bytes; one that wants a
    /// region asks repeatedly and lets the program run in between.
    pub const MAX_READ: usize = 256;

    /// Whether `msg_type` is one of this block's REQUESTS (the two a target serves).
    #[must_use]
    pub fn is_request(msg_type: u8) -> bool {
        msg_type == LIVE_READ || msg_type == LIVE_WRITE
    }
}

/// The span of the target's address space the LIVE debug agent ([`live`]) will read and write:
/// base, and length in bytes. `(0, 0)` -- the default -- means no window, and every live request
/// is refused ([`live::status::NO_WINDOW`]).
///
/// # Why the agent needs a declared window rather than the whole address space
///
/// An unmapped address is not a quiet zero on this architecture; dereferencing one is a bus fault,
/// and a bus fault inside the service callback takes down the FIRMWARE -- so a host's typo would
/// stop the very program the op exists to leave running. Worse, it would stop it in the way that
/// looks exactly like the answer we are trying to measure. A window converts that into a two-byte
/// refusal.
///
/// # Why the HOST installs it, and why this crate does not even store it
///
/// Which spans of an address space are readable is a PER-CHIP fact, and this crate is shared by
/// every target. The firmware knows its own part; it also knows the linker script it was built
/// with, which is where the number actually comes from.
///
/// WARNING: where the two numbers LIVE is not the same question on both tiers, which is why the
/// storage belongs to the host and not to this crate. An interpreted firmware has a `.bss` and can
/// keep them in a pair of atomics. An ahead-of-time compiled image cannot: its linker has no
/// writable-data segment model, so a static in the archive such an image links resolves into
/// `.text`, where reading the cell reads a code word and writing it corrupts code. A compiled
/// image therefore keeps its window in the caller-provided RAM region that holds the rest of its
/// state. Either way the agent owns no mutable storage, which is why [`serve_live_frame`] takes
/// the window as an argument rather than reading it from anywhere.
///
/// What this crate does own is the RULE: [`checked_window`] refuses a span that wraps the end of
/// the address space, so a bad window can never widen into an unbounded one, and it refuses it at
/// the setter rather than at request time.
///
/// # What this function answers
///
/// A window that does not wrap the end of the address space, or `None`. `len` 0 is a valid answer
/// and means the agent is off: every request is refused with [`live::status::NO_WINDOW`]. What is
/// refused here is a span whose end does not exist.
#[must_use]
pub fn checked_window(base: u32, len: u32) -> Option<(u32, u32)> {
    base.checked_add(len).map(|_| (base, len))
}

/// Whether the span `[addr, addr + len)` lies entirely inside `window`, and the window exists at
/// all. `Err(status)` names which of those failed, so a host is told the difference between a
/// firmware without an agent and an address it should not have asked for.
pub fn live_span_ok(window: (u32, u32), addr: u32, len: usize) -> Result<(), u8> {
    let (base, window) = window;
    if window == 0 {
        return Err(live::status::NO_WINDOW);
    }
    let Ok(len) = u32::try_from(len) else {
        return Err(live::status::BAD_REQUEST);
    };
    let Some(end) = addr.checked_add(len) else {
        return Err(live::status::OUT_OF_WINDOW);
    };
    if addr < base || end > base.saturating_add(window) {
        return Err(live::status::OUT_OF_WINDOW);
    }
    Ok(())
}

/// The byte-level access the LIVE agent makes into the target's address space.
///
/// A seam rather than a direct call for the same reason the interpreter's MMIO is one: the real
/// implementation dereferences an address a host chose, which is only meaningful on the device, and
/// a HOST cannot even express a target address -- a 64-bit test machine has no buffer whose address
/// fits the `u32` the wire carries. Without this the byte loop, the reply shape, and the
/// whole-or-nothing write rule would be provable only on silicon, which means provable only when
/// someone remembers to run a board.
pub trait LiveMemory {
    /// The byte at `address`, fetched where it is asked for.
    fn read8(&self, address: u32) -> u8;
    /// Stores `value` at `address`.
    fn write8(&mut self, address: u32, value: u8);
}

/// The real one: a volatile byte access at a raw address, through the crate that owns that unsafe
/// (this one forbids it). VOLATILE is load-bearing -- the app is mutating this memory concurrently,
/// so each byte must be fetched where it is asked for rather than folded or hoisted.
#[cfg(feature = "device")]
pub struct TargetMemory;

#[cfg(feature = "device")]
impl LiveMemory for TargetMemory {
    fn read8(&self, address: u32) -> u8 {
        lamella_mmio::read8(address)
    }

    fn write8(&mut self, address: u32, value: u8) {
        lamella_mmio::write8(address, value);
    }
}

/// Serve one LIVE debug-agent request ([`live::LIVE_READ`] / [`live::LIVE_WRITE`]) -- read or write
/// the target's memory and answer, WITHOUT stopping anything.
///
/// This is the whole of the on-target agent. It is called from two places on purpose: from the
/// deployed app's service callback ([`run_deployed_with`], the point of the op) and from the serve
/// loop ([`serve_deploy_frame`], where no app is running). **Answering identically in both is what
/// makes the op usable as its own control**: a host that keeps reading a location and sees the
/// answers stop CHANGING, while the answers keep ARRIVING, has learned that the app stopped -- not
/// that the link or the agent did. Serving it in only the running case would leave those two
/// indistinguishable, which is the confound that makes a "it kept running" claim unfalsifiable.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier. A refused REQUEST is not an error: it is an
/// ordinary reply carrying a [`live::status`] byte.
pub fn serve_live_frame(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
    window: (u32, u32),
    memory: &mut dyn LiveMemory,
) -> Result<(), TransportError> {
    match frame.msg_type {
        live::LIVE_READ => {
            let payload = &frame.payload;
            let request = if payload.len() >= 6 {
                let addr = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let len = u16::from_le_bytes([payload[4], payload[5]]);
                if len == 0 || usize::from(len) > live::MAX_READ {
                    Err(live::status::BAD_REQUEST)
                } else {
                    live_span_ok(window, addr, usize::from(len)).map(|()| (addr, len))
                }
            } else {
                Err(live::status::BAD_REQUEST)
            };
            match request {
                Ok((addr, len)) => {
                    let mut reply = Vec::with_capacity(usize::from(len) + 1);
                    reply.push(live::status::OK);
                    for address in addr..addr + u32::from(len) {
                        reply.push(memory.read8(address));
                    }
                    transport.send(live::LIVE_DATA, frame.seq, &reply)?;
                }
                Err(status) => transport.send(live::LIVE_DATA, frame.seq, &[status])?,
            }
        }
        live::LIVE_WRITE => {
            let payload = &frame.payload;
            let request = if payload.len() >= 5 {
                let addr = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                live_span_ok(window, addr, payload.len() - 4).map(|()| addr)
            } else {
                Err(live::status::BAD_REQUEST)
            };
            let (status, written) = match request {
                Ok(addr) => {
                    for (address, byte) in (addr..).zip(payload[4..].iter()) {
                        memory.write8(address, *byte);
                    }
                    (live::status::OK, payload.len() - 4)
                }
                Err(status) => (status, 0),
            };
            let count = u16::try_from(written).unwrap_or(u16::MAX).to_le_bytes();
            transport.send(live::LIVE_WROTE, frame.seq, &[status, count[0], count[1]])?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod live_agent {
    use super::{LiveMemory, checked_window, live, live_span_ok, serve_live_frame};
        use alloc::vec;
        use alloc::vec::Vec;
        use lamella_wire::{Frame, MemTransport, Transport};

        /// A 1 KiB window at a plausible SRAM base.
        const WINDOW: (u32, u32) = (0x2000_0000, 1024);

        /// A fake address space: `bytes` live at `base`, and anything outside PANICS.
        ///
        /// Panicking rather than returning zero is the point. On the device an address outside the
        /// declared window is a bus fault, so a test whose fake quietly answered 0 would pass while
        /// the shipped code faulted -- the bounds tests below would be checking nothing, and their
        /// green would be the most misleading kind.
        struct FakeMemory {
            base: u32,
            bytes: Vec<u8>,
        }

        impl FakeMemory {
            fn new(base: u32, bytes: Vec<u8>) -> Self {
                Self { base, bytes }
            }

            fn offset(&self, address: u32) -> usize {
                let offset = address
                    .checked_sub(self.base)
                    .map(|offset| offset as usize)
                    .filter(|offset| *offset < self.bytes.len());
                offset.unwrap_or_else(|| {
                    panic!("the agent touched {address:#010x}, outside the fake address space")
                })
            }
        }

        impl LiveMemory for FakeMemory {
            fn read8(&self, address: u32) -> u8 {
                self.bytes[self.offset(address)]
            }

            fn write8(&mut self, address: u32, value: u8) {
                let offset = self.offset(address);
                self.bytes[offset] = value;
            }
        }

        #[test]
        fn a_firmware_with_no_window_refuses_rather_than_dereferencing() {
            assert_eq!(live_span_ok((0, 0), 0x2000_0000, 4), Err(live::status::NO_WINDOW));
            assert_eq!(live_span_ok((0x2000_0000, 0), 0x2000_0000, 4), Err(live::status::NO_WINDOW));
        }

        #[test]
        fn a_span_must_lie_wholly_inside_the_window() {
            assert_eq!(live_span_ok(WINDOW, 0x2000_0000, 1024), Ok(()));
            assert_eq!(live_span_ok(WINDOW, 0x2000_03fc, 4), Ok(()));
            assert_eq!(live_span_ok(WINDOW, 0x2000_03fd, 4), Err(live::status::OUT_OF_WINDOW));
            assert_eq!(live_span_ok(WINDOW, 0x1fff_fffc, 8), Err(live::status::OUT_OF_WINDOW));
            assert_eq!(live_span_ok(WINDOW, 0x4000_0000, 4), Err(live::status::OUT_OF_WINDOW));
        }

        #[test]
        fn a_span_that_wraps_the_address_space_is_refused() {
            assert_eq!(
                live_span_ok((0, u32::MAX), 0xffff_fffe, 8),
                Err(live::status::OUT_OF_WINDOW)
            );
            assert_eq!(checked_window(0xffff_ff00, 0x200), None);
            assert_eq!(checked_window(0x2000_0000, 256 * 1024), Some((0x2000_0000, 256 * 1024)));
        }


        /// Serve one live request against `window` and return the reply frame. The fake address
        /// space spans the window exactly, so any access the agent makes outside it panics.
        fn serve(msg_type: u8, payload: &[u8], window: (u32, u32)) -> Frame {
            serve_against(msg_type, payload, window, &mut FakeMemory::new(window.0, vec![0; 1024]))
        }

        /// [`serve`] with the address space supplied, for a test that inspects it afterwards.
        fn serve_against(
            msg_type: u8,
            payload: &[u8],
            window: (u32, u32),
            memory: &mut FakeMemory,
        ) -> Frame {
            let mut transport = MemTransport::new();
            let frame = Frame { msg_type, seq: 77, payload: payload.to_vec() };
            serve_live_frame(&mut transport, &frame, window, memory).expect("the carrier held");
            let sent = transport.take_sent();
            transport.feed(&sent);
            transport.poll().expect("the carrier held").expect("the agent answered")
        }

        #[test]
        fn a_read_returns_the_bytes_that_are_there() {
            let window = (WINDOW.0, 8);
            let mut memory = FakeMemory::new(window.0, vec![0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]);

            let mut request = (window.0 + 2).to_le_bytes().to_vec();
            request.extend_from_slice(&4u16.to_le_bytes());
            let reply = serve_against(live::LIVE_READ, &request, window, &mut memory);
            assert_eq!(reply.msg_type, live::LIVE_DATA);
            assert_eq!(reply.seq, 77, "the reply answers the request's sequence");
            assert_eq!(reply.payload, [live::status::OK, 0xbe, 0xef, 1, 2]);

            let mut whole = window.0.to_le_bytes().to_vec();
            whole.extend_from_slice(&8u16.to_le_bytes());
            let reply = serve_against(live::LIVE_READ, &whole, window, &mut memory);
            assert_eq!(reply.payload, [live::status::OK, 0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]);
        }

        #[test]
        fn a_refused_read_carries_a_status_and_no_bytes() {
            let mut request = 0x2000_0000u32.to_le_bytes().to_vec();
            request.extend_from_slice(&4u16.to_le_bytes());
            let reply = serve(live::LIVE_READ, &request, (0, 0));
            assert_eq!(reply.msg_type, live::LIVE_DATA);
            assert_eq!(reply.payload, [live::status::NO_WINDOW], "a refusal carries no data");
        }

        #[test]
        fn a_read_longer_than_the_bound_is_refused_before_the_window_is_consulted() {
            let mut request = WINDOW.0.to_le_bytes().to_vec();
            let over = u16::try_from(live::MAX_READ + 1).expect("the bound fits a u16");
            request.extend_from_slice(&over.to_le_bytes());
            let reply = serve(live::LIVE_READ, &request, WINDOW);
            assert_eq!(reply.payload, [live::status::BAD_REQUEST]);

            let mut zero = WINDOW.0.to_le_bytes().to_vec();
            zero.extend_from_slice(&0u16.to_le_bytes());
            assert_eq!(serve(live::LIVE_READ, &zero, WINDOW).payload, [live::status::BAD_REQUEST]);
        }

        #[test]
        fn a_write_lands_whole_or_not_at_all() {
            let base = WINDOW.0;
            let mut memory = FakeMemory::new(base, vec![0; 8]);

            let mut over = base.to_le_bytes().to_vec();
            over.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
            let reply = serve_against(live::LIVE_WRITE, &over, (base, 4), &mut memory);
            assert_eq!(reply.msg_type, live::LIVE_WROTE);
            assert_eq!(reply.payload, [live::status::OUT_OF_WINDOW, 0, 0]);
            assert_eq!(memory.bytes, [0; 8], "a refused write touched nothing");

            let mut ok = (base + 2).to_le_bytes().to_vec();
            ok.extend_from_slice(&[9, 8, 7, 6]);
            let reply = serve_against(live::LIVE_WRITE, &ok, (base, 8), &mut memory);
            assert_eq!(reply.payload, [live::status::OK, 4, 0]);
            assert_eq!(memory.bytes, [0, 0, 9, 8, 7, 6, 0, 0]);
        }

        #[test]
        fn a_truncated_request_is_refused_rather_than_read_short() {
            assert_eq!(
                serve(live::LIVE_READ, &[0, 0, 0, 0x20, 4], WINDOW).payload,
                [live::status::BAD_REQUEST]
            );
            assert_eq!(
                serve(live::LIVE_WRITE, &WINDOW.0.to_le_bytes(), WINDOW).payload,
                [live::status::BAD_REQUEST, 0, 0]
            );
        }

        #[test]
        fn only_the_two_requests_are_this_ranges_business() {
            assert!(live::is_request(live::LIVE_READ));
            assert!(live::is_request(live::LIVE_WRITE));
            assert!(!live::is_request(live::LIVE_DATA));
            assert!(!live::is_request(live::LIVE_WROTE));
            assert!(!live::is_request(lamella_wire::msg::HELLO));
            assert!(!live::is_request(lamella_wire::msg::DBG_PAUSE));
        }
    }
