//! The Lamella Link debug + REPL **runner core**: the piece that runs a host-compiled program on the
//! interpreter and answers over the wire. ONE implementation serves three hosts:
//! - the **host reference runner** (in-process, for the `lamella-repl` CLI loopback + tests),
//! - the **browser runner** (compiled into `lamella-wasm` for the Studio REPL),
//! - the **on-device firmware** (flashed onto a microcontroller behind the wire).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod carriers;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use lamella_cil_runtime::memory::SafeMemory;
use lamella_cil_runtime::{Value, Vm, run};
use lamella_load::load_with_corlib;
use lamella_metadata::Assembly;
use lamella_wire::{Transport, TransportError};

#[cfg(feature = "repl-session")]
use lamella_cil_runtime::{MethodId, Module, ObjectRef, intrinsics::object_to_string};
#[cfg(feature = "repl-session")]
use lamella_load::{
    DeltaContext, load_bootstrap, load_bootstrap_lazy_corlib, load_delta, load_delta_with_corlib,
};

/// The message types of the REPL SESSION channel, where a device holds a LIVE session -- a
/// persistent interpreter, its heap, and a growing session object -- and accepts a compiled
/// submission DELTA that loads into it and runs only the new code.
///
/// A session ACCUMULATES, which is what makes it a different channel from loading a standalone
/// artifact: prior submissions run exactly once and never re-execute, and a delta binds by name to
/// variables and types that already exist.
pub mod repl {
    pub use lamella_wire::msg::{
        REPL_CLOSE, REPL_CLOSED, REPL_DELTA, REPL_DELTA_RESULT, REPL_OPEN, REPL_OPENED, REPL_PING,
        REPL_RESET, REPL_RESETTING,
    };
}

/// The message types of the DEBUG channel: control what is running, and inspect it.
///
/// A code location crosses the wire as a method id and an offset in the TARGET's own code-unit
/// domain -- on an interpreted tier that is the byte offset a source map's sequence points already
/// use, so a host's source mapping needs no conversion. All source knowledge stays host-side; the
/// target only ever sees ids and offsets.
pub mod debug {
    pub use lamella_wire::msg::{
        ABORT, DBG_ACK, DBG_BREAK, DBG_CHILDREN, DBG_DETACH, DBG_EVAL, DBG_EVAL_RESULT, DBG_EXPAND,
        DBG_FRAMES, DBG_LOCALS, DBG_PAUSE, DBG_RESUME, DBG_STACK, DBG_STEP, DBG_VARS, EVT_OUTPUT,
        EVT_STOPPED, output, step_mode, val,
    };
    pub use lamella_wire::msg::stop_reason as reason;

    /// Splits streamed output into pieces that each fit one [`EVT_OUTPUT`] payload, **never
    /// splitting a UTF-8 character**, and reports the flags each piece carries.
    ///
    /// The payload is declared UTF-8, so a frame ending mid-code-point is not decodable by anything
    /// downstream -- and the boundary the size cap lands on is a byte, not a character. Every
    /// firmware that streams output has to walk that boundary back, so it is written once here
    /// rather than once per target.
    ///
    /// `max_chunk` is what one payload can hold after its two-byte stream header. **It is raised to
    /// 4 if it is smaller**, because a payload that cannot hold one code point cannot express this
    /// protocol at all -- and the alternative to clamping is an iterator that either loses the rest
    /// of the text or never terminates. A caller with a real frame budget is nowhere near this.
    #[must_use]
    pub fn output_chunks(text: &str, max_chunk: usize) -> OutputChunks<'_> {
        OutputChunks { rest: text, max_chunk: max_chunk.max(MAX_UTF8_CHARACTER) }
    }

    /// The widest a single UTF-8 code point can be, and therefore the smallest payload that can
    /// carry one.
    const MAX_UTF8_CHARACTER: usize = 4;

    /// The iterator [`output_chunks`] returns: `(chunk, flags)` per frame.
    #[derive(Debug)]
    pub struct OutputChunks<'a> {
        rest: &'a str,
        max_chunk: usize,
    }

    impl<'a> Iterator for OutputChunks<'a> {
        /// The piece to put in one payload, and the `flags` byte to put ahead of it.
        type Item = (&'a str, u8);

        fn next(&mut self) -> Option<Self::Item> {
            if self.rest.is_empty() {
                return None;
            }
            let mut take = self.rest.len().min(self.max_chunk);
            while take > 0 && !self.rest.is_char_boundary(take) {
                take -= 1;
            }
            if take == 0 {
                self.rest = "";
                return None;
            }
            let (chunk, rest) = self.rest.split_at(take);
            self.rest = rest;
            let flags = if chunk.ends_with('\n') { output::ENDS_ON_LINE_BOUNDARY } else { 0 };
            Some((chunk, flags))
        }
    }
}

/// The message types of an artifact LOAD: place an artifact in RAM, without starting it and without
/// persisting it.
pub mod load {
    pub use lamella_wire::msg::{LOAD_BUNDLE, LOAD_CLEAR, LOAD_IMAGE, LOAD_JS, LOAD_PE, XFER_RESULT};

    use alloc::vec::Vec;
    use lamella_wire::msg::xfer;

    /// An artifact being assembled in RAM by a chunked `LOAD_x`, and the completed one an `EXEC`
    /// runs.
    ///
    /// # Why this is here rather than in each firmware
    ///
    /// Every resident runtime needs the same transfer: one artifact at a time, chunks carrying
    /// `offset` and `total`, a capacity the board sets, and a completed buffer at the end. **Only
    /// the KIND CHECK differs between them** -- a Python bundle, a PE, a JS artifact -- and that
    /// arrives here as a closure rather than as a reason to write the loop again. A rule with
    /// several implementations gains a new case in none of them, and this one was about to have
    /// five.
    ///
    /// # What it deliberately does not decide
    ///
    /// **The capacity is the caller's**, passed per chunk rather than stored, because it is a fact
    /// about a board's RAM and not about the protocol. **And nothing here starts anything**: a LOAD
    /// places an artifact and an `EXEC` runs it, which is the whole reason the protocol has two ops.
    #[derive(Debug, Default)]
    pub struct LoadBuffer {
        bytes: Vec<u8>,
        total: usize,
        filled: usize,
    }

    impl LoadBuffer {
        /// An empty buffer, holding nothing and having reserved nothing.
        #[must_use]
        pub const fn new() -> Self {
            Self { bytes: Vec::new(), total: 0, filled: 0 }
        }

        /// Discards whatever is held, complete or not -- the `LOAD_CLEAR` op's whole effect.
        ///
        /// WARNING: this is NOT expressible as a zero-length transfer -- one would satisfy
        /// `offset + len == total` immediately and COMPLETE as a loaded empty artifact, leaving an
        /// `EXEC` with something to refuse rather than nothing to run.
        pub fn clear(&mut self) {
            self.bytes = Vec::new();
            self.total = 0;
            self.filled = 0;
        }

        /// Whether every byte the first chunk promised has arrived.
        #[must_use]
        pub fn complete(&self) -> bool {
            self.total > 0 && self.filled == self.total
        }

        /// The assembled artifact, or `None` while it is still incomplete.
        ///
        /// **Incomplete is `None` rather than a short slice**, because the bytes past `filled` are
        /// the zeros the buffer was sized with: handing them over would have a reader refuse for the
        /// wrong reason, or -- worse -- not refuse.
        #[must_use]
        pub fn bytes(&self) -> Option<&[u8]> {
            self.complete().then_some(self.bytes.as_slice())
        }

        /// Takes one chunk -- payload `offset: u32, total: u32, bytes[tail]` -- answering with the
        /// [`xfer`] status its `XFER_RESULT` carries.
        ///
        /// `capacity` is the most this board will hold. `accepts` is called ONCE, on the first
        /// chunk's bytes, to ask whether this is an artifact the caller can run at all.
        ///
        /// **A chunk at offset 0 discards an incomplete load**, per the protocol: within the load
        /// domain there is one transfer at a time, and any `LOAD_x` at offset 0 replaces what is in
        /// flight -- so a host that dropped a transfer recovers by starting another and never has to
        /// ask whether it must.
        ///
        /// **The kind check runs BEFORE the allocation**, and it has to. Nothing above it looks at what the
        /// payload IS, so without it an artifact the caller can never run reserves the whole budget
        /// and is acknowledged as a good transfer.
        pub fn take_chunk(
            &mut self,
            payload: &[u8],
            capacity: usize,
            accepts: impl FnOnce(&[u8]) -> bool,
        ) -> u8 {
            if payload.len() < 8 {
                return xfer::RANGE_REJECTED;
            }
            let offset =
                u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
            let total = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
            let chunk = &payload[8..];
            if offset == 0 {
                self.clear();
                if total > capacity || chunk.len() > total || total == 0 {
                    return xfer::RANGE_REJECTED;
                }
                if !accepts(chunk) {
                    return xfer::RANGE_REJECTED;
                }
                if self.bytes.try_reserve_exact(total).is_err() {
                    return xfer::WRITE_FAILED;
                }
                self.bytes.resize(total, 0);
                self.bytes[..chunk.len()].copy_from_slice(chunk);
                self.total = total;
                self.filled = chunk.len();
                return xfer::MATCHED;
            }
            if self.total == 0 {
                return xfer::RANGE_REJECTED;
            }
            if offset != self.filled || total != self.total || offset + chunk.len() > self.total {
                return xfer::RANGE_REJECTED;
            }
            self.bytes[offset..offset + chunk.len()].copy_from_slice(chunk);
            self.filled += chunk.len();
            xfer::MATCHED
        }
    }
}

/// The message types of an artifact DEPLOY: write an artifact to the target's persistent region so
/// it boots on reset. The mirror of a LOAD, differing only in where the bytes land.
pub mod deploy {
    pub use lamella_wire::msg::{
        DEPLOY_BUNDLE, DEPLOY_CLEAR, DEPLOY_IMAGE, DEPLOY_JS, DEPLOY_PE, DEPLOY_STATUS,
        DEPLOY_STATUS_RESULT, XFER_RESULT, deploy_state, xfer,
    };
}

/// The message types that START something, and ask what is running.
pub mod exec {
    pub use lamella_wire::msg::{EXEC, EXEC_ACK, EXEC_STATUS, exec_flags, exec_source, tier};
    pub use lamella_wire::msg::exec_ack as ack;
}

/// The message types of PROFILE INTROSPECTION: the board tells a host what it is, in full.
///
/// The handshake already carries the compact identity at no extra round trip; this pair pulls the
/// full manifest only when a host's cache misses that identity's hash.
pub mod profile {
    pub use lamella_wire::msg::{PROFILE_GET, PROFILE_MANIFEST};
}

/// The message types of on-device TELEMETRY: a host subscribes to device signals -- command
/// outputs, sensor traces, energy -- and the target streams samples asynchronously.
///
/// RESERVED: the payload shapes belong with the implementation. The type bytes and the capability
/// bit are allocated so that nothing else takes them.
pub mod telemetry {
    pub use lamella_wire::msg::{SCOPE_SAMPLE, SCOPE_SUBSCRIBE, SCOPE_UNSUBSCRIBE};
}

/// The message types of the DEVICE and FIRMWARE block: update the thing that serves this protocol.
///
/// RESERVED: the type bytes are allocated so that nothing else takes them, and a target that does
/// not implement one refuses it by name.
pub mod device {
    pub use lamella_wire::msg::{
        ENTER_HW_BOOTLOADER, ENTER_SW_BOOTLOADER, FW_ACTIVATE, FW_ACTIVATE_RESULT, FW_COMMIT,
        FW_COMMIT_RESULT, FW_RESULT, FW_STATUS, FW_STATUS_RESULT, FW_WRITE, fw_activate_status,
        fw_intent, fw_slot, fw_state,
    };
}

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

/// That nothing this crate SHIPS uses a 64-bit atomic, because the devices it is written for do not
/// have one.
///
/// # Why the width is pinned
///
/// `AtomicU64` is not available here. ARMv7-M has no doubleword exclusive, so rustc gives
/// `thumbv7em-none-eabi` a `max-atomic-width` of 32 and `core::sync::atomic::AtomicU64` DOES
/// NOT EXIST on it. The type resolves on a host and on wasm and nowhere else -- so it compiles for
/// every target that is not a microcontroller, and fails for **every device the identity was added
/// for** -- a failure a host-only build cannot surface at all.
///
/// # Why a source read rather than a build
///
/// The honest gate is LINKING A FIRMWARE for a target whose `max-atomic-width` is 32, and this test
/// is not that. A `#[cfg(target_os = "none")]` binary is compiled by no host test run, so a type
/// that resolves everywhere except a microcontroller passes every check a workspace test performs.
/// This reads the source instead, which is a weaker instrument aimed at the same defect: it cannot
/// prove a firmware links, only that this crate names no doubleword atomic for one to trip over.
///
/// What this closes is narrower and worth having anyway: the specific hazard is a KNOWN, NAMED
/// property of a target this crate is compiled for, the check costs nothing, and it runs in the
/// DEFAULT gate -- which is the one place the original defect had no chance of being seen.
/// **It is a tripwire for one cause, not a substitute for building the firmware.** A different
/// target-width mistake will walk straight past it.
#[cfg(test)]
mod device_portability {
    /// This file's own text. Read rather than reasoned about, for the same reason the message-type
    /// table is: the mistake is one line that looks correct everywhere it is reviewed.
    const SOURCE: &str = include_str!("lib.rs");

    /// The types that do not exist on a 32-bit-atomic target.
    const ABSENT_ON_THUMBV7EM: [&str; 3] = ["AtomicU64", "AtomicI64", "AtomicF64"];

    /// The source split into (SHIPPED, TEST-ONLY) lines.
    ///
    /// Test-only code is genuinely exempt -- it is compiled for the host and never linked into a
    /// firmware -- and the exemption is load-bearing rather than a convenience: the interpreter, the
    /// network stack, and the WiFi driver all legitimately use a 64-bit atomic in a `#[cfg(test)]`
    /// clock stand-in, so a check without it would report four defects that are not defects, and be
    /// switched off.
    fn partition() -> (alloc::vec::Vec<&'static str>, alloc::vec::Vec<&'static str>) {
        let mut shipped = alloc::vec::Vec::new();
        let mut test_only = alloc::vec::Vec::new();
        let mut in_test = false;
        for line in SOURCE.lines() {
            if line.starts_with("#[cfg(") && line.contains("test") {
                in_test = true;
            }
            if in_test {
                test_only.push(line);
                if line == "}" {
                    in_test = false;
                }
            } else {
                shipped.push(line);
            }
        }
        (shipped, test_only)
    }

    #[test]
    fn no_64_bit_atomic_reaches_a_device_build() {
        let (shipped, test_only) = partition();

        assert!(
            shipped.len() > test_only.len(),
            "the partition put {} lines in shipped and {} in test -- it is broken, not the source",
            shipped.len(),
            test_only.len()
        );
        assert!(
            shipped.iter().any(|line| line.contains("RESIDENT_CORLIB_HASH_LO")),
            "the shipped side does not contain the statics that replaced the AtomicU64 -- the \
             partition is not reading shipped code"
        );
        assert!(
            test_only.iter().any(|line| line.contains("fn no_two_message_types_claim_the_same_byte")),
            "the test side does not contain a known test -- the partition is not finding cfg(test)"
        );

        for line in shipped {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for absent in ABSENT_ON_THUMBV7EM {
                assert!(
                    !code.contains(absent),
                    "shipped code uses {absent}, which does not exist on thumbv7em \
                     (max-atomic-width 32): every device firmware linking this crate will fail to \
                     build, and no host gate will notice. Use two 32-bit halves.\n    {line}"
                );
            }
        }
    }
}

/// That the resident library DECLARES every capability symbol it can be built with.
///
/// # Why this is a test and not a convention
///
/// The surface bitmap a target advertises is folded from `[assembly: Lamella.Runtime.SurfaceSymbol]`
/// declarations in the library's own source, one per `#if`. A symbol added to a profile and NOT
/// declared there compiles, links, ships, and reports a board as carrying LESS than it does -- so a
/// host refuses a program that would have run, and the reason is a missing line in a file nobody was
/// looking at.
///
/// **The failure is in the recoverable direction, which is exactly why it needs a test.** A false
/// refusal is survivable and therefore quiet: nobody reports it as a defect, they report that their
/// program does not fit.


/// Re-exported for a deploy host: read a baked image's content checksum from its header, to compare
/// against what a target already holds (content-addressed deploy-skip) without a direct dependency on
/// the interpreter crate.
pub use lamella_cil_runtime::baked_image_checksum;

/// The persistent-image flash region a deploy-capable target owns. The generic runner
/// drives the deploy protocol against this seam; the erase/write primitives are
/// device-specific (each MCU's flash controller differs), so the firmware implements them.
#[cfg(feature = "baked-image")]
pub trait FlashSink {
    /// The image region as a `'static` slice -- where a deployed image is written, and
    /// where the boot path reads `Module::from_baked` from. May be longer than any stored
    /// image; `from_baked` reads the true length from the image header.
    fn image_slice(&self) -> &'static [u8];
    /// Erase enough of the region to invalidate any stored image (its fingerprint), so a
    /// subsequent boot finds none.
    fn erase(&mut self);
    /// Erase, then program `image` into the region. Returns whether a readback verified.
    fn program(&mut self, image: &[u8]) -> bool;
    /// Program one CHUNK of a larger image: write `chunk` at `offset` within the region, having
    /// erased enough of the region for `total` bytes on the first chunk (`offset == 0`). Returns
    /// whether the chunk read back. The default is unsupported (a target that never receives an
    /// image larger than one wire frame need not implement it); a roomy target overrides it to
    /// stream a corlib-baked image to flash a frame at a time.
    fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
        let _ = (offset, chunk, total);
        false
    }

    /// The corlib this firmware holds RESIDENT in flash, if any -- what a deployed bare PE
    /// resolves its corlib references against ([`load_deployed`]).
    ///
    /// It belongs on this seam, beside the region it resolves against, rather than in each
    /// caller's arguments: booting the deployed artifact and debugging it must accept the SAME
    /// artifacts, and a target where one path resolves a program PE and another does not is one
    /// that runs a program it then refuses to debug. One answer, on the seam both already take.
    ///
    /// Defaulted to `None`, so a firmware carrying no corlib is unchanged and a bare PE is refused
    /// by name on every path at once.
    fn resident_corlib(&self) -> Option<&'static [u8]> {
        None
    }
}

/// A WINC1500 WiFi module's OWN firmware store, reachable from the firmware over the module's
/// SPI: the target side of the `WINC_FW_*` streaming update (the module image is far larger
/// than a small part's RAM, so the host chunks it and the firmware programs each chunk as it
/// arrives). The generic runner drives the protocol against this seam; a WiFi board implements
/// it over its hardware SPI, and a board without a module passes none (the runner then answers
/// every `WINC_FW_*` request `ok = 0`).
#[cfg(feature = "baked-image")]
pub trait WincFlasher {
    /// Brings the module into its flash download mode from scratch (a fresh power-up sequence,
    /// so it works whether or not the module's firmware was booted) and erases the module-flash
    /// span `[offset, offset + total)`. Returns whether the module answered sanely and the
    /// erase completed.
    fn begin(&mut self, offset: usize, total: usize) -> bool;
    /// Programs `data` at the absolute module-flash `offset` (within the span `begin` erased)
    /// and verifies it by read-back. Returns whether the readback matched.
    fn program(&mut self, offset: usize, data: &[u8]) -> bool;
    /// Finishes the update: a final module-flash read proves the wire stayed sane, then the
    /// module is parked (powered down) so the next board reset boots it fresh into the new
    /// firmware.
    fn finish(&mut self) -> bool;
}

/// How a [`run_deployed`] app run ended.
#[cfg(feature = "baked-image")]
pub enum Deployed {
    /// The app ran to completion or trapped -- nothing is listening now; serve.
    Completed(RunResult),
    /// A host sent `HELLO` mid-run: the app was aborted and the host acknowledged; the
    /// firmware should enter its serve loop where the host's next command lands.
    Interrupted,
}

/// What [`serve_one_deploy`] did with a pending frame -- so a firmware serve loop knows when the host
/// asked to BOOT the deployed image, which is a reset the firmware must perform itself (the runner is
/// target-agnostic and cannot reset the MCU).
#[cfg(feature = "baked-image")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Served {
    /// No frame was pending.
    Nothing,
    /// A frame was handled; keep serving.
    Handled,
    /// The host sent [`deploy::DEPLOY_RUN`]: the firmware should reset into its boot-run path.
    RunRequested,
    /// The host sent [`repl::REPL_RESET`]: the firmware should reset back into SERVE mode. The
    /// acknowledgement has already been sent; the caller resets once its transport has drained.
    ///
    /// This is the only path that reclaims an exhausted interpreter arena -- see [`repl::REPL_RESET`]
    /// for why dropping the session does not.
    ResetRequested,
}

/// The result of running a program on the target: its process exit code and captured console output.
///
/// **The output half does not cross the wire in this shape.** A run's output is streamed as it
/// happens ([`debug::EVT_OUTPUT`]) and the run's END carries only the exit code
/// ([`debug::EVT_STOPPED`]); a driver assembles the two with a [`RunCollector`]. This type is what a
/// caller gets back once they have been assembled, and what an in-process run produces directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// The program's exit code (its `Main` return, 0 if none, 70 on an unhandled trap).
    pub exit: i32,
    /// The program's captured console (`Console.Out`) output, followed by anything written to the
    /// error stream -- a trap report, a load failure -- in the order the two arrived.
    ///
    /// The two streams are DISTINCT on the wire and merged here, which is the opposite of the usual
    /// direction and is deliberate: a terminal shows one column, every existing caller of this field
    /// is a terminal, and a caller that wants them apart reads them apart from the collector that
    /// filled this. What the merge must never do is happen on the WIRE, where a host that wanted to
    /// colour a trap report differently would have no way to recover the split.
    pub stdout: String,
}

/// The stop-event tail a terminal [`debug::EVT_STOPPED`] carries: `exit(i32 LE)`, `flags(u8)`.
///
/// Only [`debug::reason::DONE`] and [`debug::reason::TRAP`] carry it -- a breakpoint or a pause has
/// no exit value to state, and inventing one would make "stopped at line 12" and "returned 0" the
/// same bytes.
#[cfg(feature = "baked-image")]
const STOP_FLAGS_RESERVED: u8 = 0;

/// The exit code and flags from a terminal [`debug::EVT_STOPPED`] payload, or `None` when the stop
/// carried no result tail (every reason but `DONE` and `TRAP`) or the payload is short.
#[must_use]
pub fn stop_exit(payload: &[u8]) -> Option<(i32, u8)> {
    match payload.first().copied()? {
        debug::reason::DONE | debug::reason::TRAP => {}
        _ => return None,
    }
    let tail = payload.get(9..14)?;
    Some((i32::from_le_bytes(tail[0..4].try_into().ok()?), tail[4]))
}

/// Run a program assembly against `corlib_bytes`, capturing its console output + exit code. This is the
/// runner's actual work -- the host reference runner, the browser runner, and the device firmware all call
/// it. A bad assembly / load failure is reported as exit -1 with the reason in `stdout`; an unhandled
/// trap is exit 70 (matching the interpreter's abort convention).
#[must_use]
pub fn run_program(corlib_bytes: &[u8], program_bytes: &[u8]) -> RunResult {
    let corlib_bytes = lamella_load::resident_bytes(corlib_bytes);
    let program_bytes = lamella_load::resident_bytes(program_bytes);
    let corlib = match Assembly::read(corlib_bytes) {
        Ok(assembly) => assembly,
        Err(error) => return failure(&format!("corlib does not parse: {error:?}")),
    };
    let program = match Assembly::read(program_bytes) {
        Ok(assembly) => assembly,
        Err(error) => return failure(&format!("program does not parse: {error:?}")),
    };
    let loaded = match load_with_corlib(&corlib, &program) {
        Ok(loaded) => loaded,
        Err(error) => return failure(&format!("load failed: {error}")),
    };
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    #[cfg(target_os = "none")]
    vm.set_mmio_subword(
        lamella_mmio::write8,
        lamella_mmio::read8,
        lamella_mmio::write16,
        lamella_mmio::read16,
    );
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    let outcome = run(&loaded.module, &mut vm, loaded.entry, Vec::new());
    let exit = match outcome {
        Ok(Some(Value::Int32(code))) => code,
        Ok(_) => 0,
        Err(_) => 70,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

fn failure(reason: &str) -> RunResult {
    RunResult { exit: -1, stdout: reason.to_string() }
}

/// Answers a `HELLO` with this baked target's honest advertisement (or a `NAK` on a
/// version mismatch). A malformed `HELLO` is dropped; the host times out and retries.
#[cfg(feature = "baked-image")]
/// The capability set every baked-image serve advertises. `PROFILE_CHIPID` is unconditional
/// -- this build's identity always carries the structured board/chip fields, even when a
/// firmware left them `0` = unknown.
fn serve_caps() -> lamella_wire::Capabilities {
    use core::sync::atomic::Ordering;
    serve_caps_with(
        RESIDENT_CORLIB_PRESENT.load(Ordering::Relaxed),
        MONOTONIC_CLOCK_LIVE.load(Ordering::Relaxed),
    )
}

/// [`serve_caps`] with the resident-corlib and clock answers supplied rather than read from the
/// statics, so the advertisement rule is checkable without a test having to reach into process-wide
/// state -- which would make every other test in the process depend on whether that one had run yet.
#[cfg(feature = "baked-image")]
fn serve_caps_with(resident_corlib: bool, monotonic_clock: bool) -> lamella_wire::Capabilities {
    use lamella_wire::Capabilities;
    Capabilities(
        Capabilities::BAKED_IMAGE
            | Capabilities::DEBUG_BASIC
            | Capabilities::BREAKPOINTS
            | Capabilities::STEPPING
            | Capabilities::LOCALS
            | Capabilities::PROFILE_CHIPID
            | if resident_corlib { Capabilities::RESIDENT_CORLIB } else { 0 }
            | if monotonic_clock { Capabilities::MONOTONIC_CLOCK } else { 0 },
    )
}

/// Whether this firmware installed a monotonic clock it CHECKED to be moving -- the source of
/// [`lamella_wire::Capabilities::MONOTONIC_CLOCK`], recorded by [`note_monotonic_clock`].
///
/// A static for the same reason the resident-corlib answer is one: it is a property of the running
/// FIRMWARE rather than of a request, settled once at boot and true for the board's whole life.
/// It starts FALSE, so a firmware that installs no clock, or installs one without checking it,
/// advertises nothing -- the bit has to be earned by a positive observation.
#[cfg(feature = "baked-image")]
static MONOTONIC_CLOCK_LIVE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Record that this firmware's monotonic clock was OBSERVED ADVANCING at boot, so a `HELLO_ACK`
/// advertises [`lamella_wire::Capabilities::MONOTONIC_CLOCK`] and a host knows a timing number from
/// this board means something.
///
/// The board's clock module calls this from `install`, which is the only place that has both the
/// counter and the reason to look at it. Pass `false` to state the opposite explicitly -- a clock
/// that was installed and found DEAD -- which keeps the bit clear without relying on nobody having
/// set it.
#[cfg(feature = "baked-image")]
pub fn note_monotonic_clock(live: bool) {
    MONOTONIC_CLOCK_LIVE.store(live, core::sync::atomic::Ordering::Relaxed);
}

/// Whether this firmware holds a resident corlib, and that corlib's content hash.
///
/// Statics for the same reason the board/chip words below are: it is a property of the running
/// FIRMWARE, not of a request. **The hash is computed ONCE** -- a corlib is a couple of hundred
/// kilobytes of flash and a `HELLO` is answered on the reclaim path, where a host is waiting.
#[cfg(feature = "baked-image")]
static RESIDENT_CORLIB_PRESENT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "baked-image")]
static RESIDENT_CORLIB_HASH_LO: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static RESIDENT_CORLIB_HASH_HI: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// The resident corlib's two declared versions and its surface set, decoded ONCE beside the hash.
#[cfg(feature = "baked-image")]
static RESIDENT_LIB_VERSION_LO: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static RESIDENT_LIB_VERSION_HI: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static RESIDENT_LIB_FILE_VERSION_LO: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static RESIDENT_LIB_FILE_VERSION_HI: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static RESIDENT_SURFACE_LO: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static RESIDENT_SURFACE_HI: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Records the resident corlib a serve path was handed, so the advertisement can describe it.
///
/// Called at the entry of every serve path that answers a `HELLO`, which is what makes the identity
/// correct on the FIRST frame of a session -- a `HELLO` is often the first frame, and an identity that
/// gained the corlib only afterwards would advertise two different surfaces for one firmware.
///
/// The metadata is read HERE rather than where the identity is built, because this is the one place
/// that holds the bytes and it runs once: a `HELLO` is answered on the reclaim path with a host
/// waiting, and parsing a few hundred kilobytes of PE per handshake would be paid on every frame
/// that matters.
#[cfg(feature = "baked-image")]
fn note_resident_corlib(corlib: Option<&'static [u8]>) {
    use core::sync::atomic::Ordering;
    let Some(bytes) = corlib else { return };
    if RESIDENT_CORLIB_PRESENT.load(Ordering::Relaxed) {
        return;
    }
    let hash = fnv1a(FNV_OFFSET, bytes);
    RESIDENT_CORLIB_HASH_LO.store(hash as u32, Ordering::Relaxed);
    RESIDENT_CORLIB_HASH_HI.store((hash >> 32) as u32, Ordering::Relaxed);
    let declared = ResidentLibrary::of(bytes);
    RESIDENT_LIB_VERSION_LO.store(pack(declared.lib_version[0], declared.lib_version[1]), Ordering::Relaxed);
    RESIDENT_LIB_VERSION_HI.store(pack(declared.lib_version[2], declared.lib_version[3]), Ordering::Relaxed);
    RESIDENT_LIB_FILE_VERSION_LO
        .store(pack(declared.lib_file_version[0], declared.lib_file_version[1]), Ordering::Relaxed);
    RESIDENT_LIB_FILE_VERSION_HI
        .store(pack(declared.lib_file_version[2], declared.lib_file_version[3]), Ordering::Relaxed);
    RESIDENT_SURFACE_LO.store(declared.surface as u32, Ordering::Relaxed);
    RESIDENT_SURFACE_HI.store((declared.surface >> 32) as u32, Ordering::Relaxed);
    RESIDENT_CORLIB_PRESENT.store(true, Ordering::Relaxed);
}

/// Two `u16`s in one `u32`, low half first -- see the note on the statics for why the pairing is
/// deliberate rather than a saving.
#[cfg(feature = "baked-image")]
const fn pack(low: u16, high: u16) -> u32 {
    (low as u32) | ((high as u32) << 16)
}

/// The halves of a [`pack`]ed word, in the order they went in.
#[cfg(feature = "baked-image")]
const fn unpack(word: u32) -> (u16, u16) {
    (word as u16, (word >> 16) as u16)
}

/// What a resident managed library DECLARES about itself: which contract it was built against, which
/// build of it this is, and how much of that contract it actually carries.
///
/// Read out of the resident image rather than from a firmware constant. A constant would be a second
/// spelling of a value that moves whenever the library does, and the spelling that goes stale is the
/// one nobody is looking at.
#[cfg(feature = "baked-image")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ResidentLibrary {
    /// The Assembly row's version (II.22.2): the BCL GENERATION the library was built against.
    lib_version: [u16; 4],
    /// The `[assembly: AssemblyFileVersion]` blob's version: WHICH BUILD of that generation.
    lib_file_version: [u16; 4],
    /// The `LAMELLA_SURFACE_*` symbols the library declares, as [`lamella_wire::surface`] bits.
    surface: u64,
}

#[cfg(feature = "baked-image")]
impl ResidentLibrary {
    /// Everything a library states about itself, or all zeros when the image does not parse -- which
    /// is the same answer as a library declaring nothing, and deliberately so: a firmware cannot
    /// repair an unreadable image, and the host-side rule that refuses an all-zero version from a
    /// target claiming a resident library ([`lamella_wire::Surface::unreadable_version`]) is what
    /// separates the two cases.
    fn of(bytes: &[u8]) -> Self {
        let Ok(assembly) = Assembly::read(bytes) else {
            return Self::default();
        };
        let (major, minor, build, revision) = assembly.assembly_version();
        let lib_file_version = assembly
            .assembly_attribute_strings("System.Reflection", "AssemblyFileVersionAttribute")
            .first()
            .and_then(|declared| parse_version(declared))
            .unwrap_or([0; 4]);
        let mut surface = 0;
        for symbol in
            assembly.assembly_attribute_strings("Lamella.Runtime", "SurfaceSymbolAttribute")
        {
            surface |= lamella_wire::surface::bit_of(symbol).unwrap_or(0);
        }
        Self { lib_version: [major, minor, build, revision], lib_file_version, surface }
    }
}

/// A four-part `"a.b.c.d"` version string as its parts, or `None` when it is not one.
///
/// Strict about the shape -- exactly four parts, each a `u16` -- because the value goes onto the
/// wire as a version a host will ORDER two boards by. A lenient parse that read `"4.5"` as
/// `4.5.0.0` would turn a malformed declaration into a plausible one, which is the shape this whole
/// field exists to remove.
#[cfg(feature = "baked-image")]
fn parse_version(text: &str) -> Option<[u16; 4]> {
    let mut parts = text.split('.');
    let mut version = [0u16; 4];
    for slot in &mut version {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

/// The resident surface's content hash: the intrinsic registry's fingerprint, continued over the
/// resident corlib's digest when there is one.
///
/// One continued fold rather than two schemes stitched together -- the registry's fingerprint is
/// FNV-1a, so this is the same walk carried on. A target with no resident corlib reports the
/// fingerprint unchanged, which is what a firmware holding no corlib has always reported.
#[cfg(feature = "baked-image")]
fn resident_surface_hash() -> u64 {
    use core::sync::atomic::Ordering;
    resident_surface_hash_of(RESIDENT_CORLIB_PRESENT.load(Ordering::Relaxed).then(|| {
        u64::from(RESIDENT_CORLIB_HASH_LO.load(Ordering::Relaxed))
            | (u64::from(RESIDENT_CORLIB_HASH_HI.load(Ordering::Relaxed)) << 32)
    }))
}

/// The hash RULE, with the resident corlib's digest supplied rather than read from the statics.
///
/// Split out so a test exercises the shipped arithmetic instead of a copy of it: a test that
/// reimplemented this rule would agree with a broken version of it, which is the same trap as proving a
/// codec against a transcription of itself. Reaching the statics from a test is the alternative, and it
/// would make every other test in the process depend on whether that one had run.
#[cfg(feature = "baked-image")]
fn resident_surface_hash_of(resident_corlib: Option<u64>) -> u64 {
    let registry = lamella_cil_runtime::intrinsic_registry::registry_fingerprint();
    match resident_corlib {
        Some(corlib) => fnv1a(registry, &corlib.to_le_bytes()),
        None => registry,
    }
}

/// FNV-1a's offset basis and prime, matching the intrinsic registry's fingerprint.
#[cfg(feature = "baked-image")]
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
#[cfg(feature = "baked-image")]
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// FNV-1a over `bytes`, continuing from `seed`.
#[cfg(feature = "baked-image")]
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The board/chip identity words the firmware installed at boot ([`set_board_identity`]):
/// `product_model` in the low half of the first word; the IDCODE and device-id whole. Statics
/// (not parameters) because the identity is a property of the running FIRMWARE, set once at
/// boot and read wherever a `HELLO_ACK`/manifest is built -- the same shape as the other
/// boot-installed seams.
#[cfg(feature = "baked-image")]
static BOARD_MODEL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static CHIP_IDCODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static CHIP_DEVID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Installs the board/chip identity this firmware advertises in its `HELLO_ACK` profile
/// (docs in [`lamella_wire::ProfileIdentity`]): the board-model code from
/// [`lamella_wire::product_model`] (0 for a custom board), the chip's SW-DP IDCODE, and the
/// vendor device-id register value the firmware read from its own silicon (each 0 =
/// unknown). Call once at boot, before serving.
#[cfg(feature = "baked-image")]
pub fn set_board_identity(product_model: u16, chip_idcode: u32, chip_devid: u32) {
    use core::sync::atomic::Ordering;
    BOARD_MODEL.store(u32::from(product_model), Ordering::Relaxed);
    CHIP_IDCODE.store(chip_idcode, Ordering::Relaxed);
    CHIP_DEVID.store(chip_devid, Ordering::Relaxed);
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
/// # Why the firmware installs it rather than this crate knowing it
///
/// Which spans of an address space are readable is a PER-CHIP fact, and this crate is shared by
/// every target. The firmware knows its own part; it also knows the linker script it was built
/// with, which is where the number actually comes from. Same shape as the other boot-installed
/// seams ([`set_board_identity`]).
#[cfg(feature = "baked-image")]
static LIVE_WINDOW_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static LIVE_WINDOW_LEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Declares the address span the LIVE debug agent may read and write (see [`LIVE_WINDOW_BASE`]),
/// and by doing so turns the agent on: until this is called, every [`live`] request is refused.
/// Call once at boot, before serving.
///
/// A `len` of 0 leaves the agent off. A span that wraps the end of the address space is rejected
/// here rather than at request time, so a bad window can never widen into an unbounded one.
#[cfg(feature = "baked-image")]
pub fn set_live_window(base: u32, len: u32) {
    use core::sync::atomic::Ordering;
    let Some((base, len)) = checked_window(base, len) else { return };
    LIVE_WINDOW_BASE.store(base, Ordering::Relaxed);
    LIVE_WINDOW_LEN.store(len, Ordering::Relaxed);
}

/// A window that does not wrap the end of the address space, or `None`.
///
/// Split from [`set_live_window`] so the rule is checkable without a test writing the statics --
/// process-wide state would make every other test in the process depend on whether that one had
/// run yet, the same reason [`serve_caps_with`] exists.
#[cfg(feature = "baked-image")]
fn checked_window(base: u32, len: u32) -> Option<(u32, u32)> {
    base.checked_add(len).map(|_| (base, len))
}

/// The live window the firmware declared, as `(base, len)`.
#[cfg(feature = "baked-image")]
fn live_window() -> (u32, u32) {
    use core::sync::atomic::Ordering;
    (LIVE_WINDOW_BASE.load(Ordering::Relaxed), LIVE_WINDOW_LEN.load(Ordering::Relaxed))
}

/// Whether the span `[addr, addr + len)` lies entirely inside `window`, and the window exists at
/// all. `Err(status)` names which of those failed, so a host is told the difference between a
/// firmware without an agent and an address it should not have asked for.
#[cfg(feature = "baked-image")]
fn live_span_ok(window: (u32, u32), addr: u32, len: usize) -> Result<(), u8> {
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
#[cfg(feature = "baked-image")]
trait LiveMemory {
    fn read8(&self, address: u32) -> u8;
    fn write8(&mut self, address: u32, value: u8);
}

/// The real one: a volatile byte access at a raw address, through the crate that owns that unsafe
/// (this one forbids it). VOLATILE is load-bearing -- the app is mutating this memory concurrently,
/// so each byte must be fetched where it is asked for rather than folded or hoisted.
#[cfg(feature = "baked-image")]
struct TargetMemory;

#[cfg(feature = "baked-image")]
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
#[cfg(feature = "baked-image")]
fn serve_live_frame(
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

/// [`serve_caps`] plus the deploy tier's extras: a target with a persistent image region
/// also debugs it in place ([`debug::DBG_ATTACH`]), and -- when the firmware declared a live
/// window ([`set_live_window`]) -- answers the LIVE agent's memory ops while a deployed app runs.
///
/// The live bit is conditional on the window because the bit is a promise a host acts on: a
/// firmware that carries the code but declares no window would refuse every request it advertised.
#[cfg(feature = "baked-image")]
fn deploy_caps() -> lamella_wire::Capabilities {
    deploy_caps_with(serve_caps(), live_window().1)
}

/// The [`deploy_caps`] RULE with both inputs supplied rather than read from statics, so the
/// advertisement is checkable without a test reaching into process-wide state.
#[cfg(feature = "baked-image")]
fn deploy_caps_with(
    base: lamella_wire::Capabilities,
    live_window_len: u32,
) -> lamella_wire::Capabilities {
    let live = if live_window_len == 0 { 0 } else { lamella_wire::Capabilities::LIVE_MEMORY };
    lamella_wire::Capabilities(base.0 | lamella_wire::Capabilities::DEBUG_BOOT_DEPLOYED | live)
}

/// The rustc target triple this crate was compiled for, from the build script -- the only statement
/// of the target ABI available to a source file. See `build.rs` for why no `cfg` can replace it.
#[cfg(feature = "baked-image")]
const TARGET_TRIPLE: &str = env!("LAMELLA_TARGET_TRIPLE");

/// This build's firmware version, as `[days-since-2000, build-of-day]`.
#[cfg(feature = "baked-image")]
const FIRMWARE_VERSION: [u16; 2] = [0, 0];

/// What this target IS: the product, its target ABI, which firmware build is answering, the chip's
/// own identity, and one record per resident runtime.
///
/// **The profile display NAME is deliberately not here** -- it moved to the profile manifest, where
/// it has no length limit to be truncated by.
#[cfg(feature = "baked-image")]
fn profile_identity() -> lamella_wire::TargetIdentity {
    use core::sync::atomic::Ordering;
    lamella_wire::TargetIdentity {
        product_model: BOARD_MODEL.load(Ordering::Relaxed) as u16,
        arch: lamella_wire::arch::from_target_triple(TARGET_TRIPLE),
        firmware_version: FIRMWARE_VERSION,
        ..lamella_wire::TargetIdentity::default()
    }
    .with_chip_id(
        lamella_wire::chip_id_kind::DEBUG_PORT_AND_DEVICE_ID,
        &chip_identity_bytes(
            CHIP_IDCODE.load(Ordering::Relaxed),
            CHIP_DEVID.load(Ordering::Relaxed),
        ),
    )
    .with_surface(cil_surface())
}

/// The eight bytes of a [`lamella_wire::chip_id_kind::DEBUG_PORT_AND_DEVICE_ID`] identity: the debug
/// port's identification code, then the vendor's device-id register, both little-endian.
#[cfg(feature = "baked-image")]
fn chip_identity_bytes(idcode: u32, devid: u32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&idcode.to_le_bytes());
    bytes[4..8].copy_from_slice(&devid.to_le_bytes());
    bytes
}

/// The one surface record this firmware answers for: the CIL runtime it carries, and the managed
/// library resident beside it when there is one.
///
/// ONE record because this crate is one runtime. A board carrying a second (a Python firmware on the
/// same silicon) answers for its own, and a board carrying both would append a record rather than
/// widening this one -- which is the whole reason the surface is a LIST and the versions live in it
/// rather than beside it.
#[cfg(feature = "baked-image")]
fn cil_surface() -> lamella_wire::Surface {
    use core::sync::atomic::Ordering;
    use lamella_cil_runtime::intrinsic_registry;
    let resident = RESIDENT_CORLIB_PRESENT.load(Ordering::Relaxed);
    let quad = |low: &core::sync::atomic::AtomicU32, high: &core::sync::atomic::AtomicU32| {
        let (a, b) = unpack(low.load(Ordering::Relaxed));
        let (c, d) = unpack(high.load(Ordering::Relaxed));
        [a, b, c, d]
    };
    lamella_wire::Surface {
        tier: lamella_wire::msg::tier::CIL,
        abi: intrinsic_registry::INTRINSIC_ABI,
        hash: resident_surface_hash(),
        lib_version: if resident {
            quad(&RESIDENT_LIB_VERSION_LO, &RESIDENT_LIB_VERSION_HI)
        } else {
            [0; 4]
        },
        lib_file_version: if resident {
            quad(&RESIDENT_LIB_FILE_VERSION_LO, &RESIDENT_LIB_FILE_VERSION_HI)
        } else {
            [0; 4]
        },
        caps: 0,
    }
}

/// The `LAMELLA_SURFACE_*` bitmap the resident library declares, or `0` when there is none.
#[cfg(feature = "baked-image")]
fn resident_surface_symbols() -> u64 {
    use core::sync::atomic::Ordering;
    if !RESIDENT_CORLIB_PRESENT.load(Ordering::Relaxed) {
        return 0;
    }
    u64::from(RESIDENT_SURFACE_LO.load(Ordering::Relaxed))
        | (u64::from(RESIDENT_SURFACE_HI.load(Ordering::Relaxed)) << 32)
}

#[cfg(feature = "baked-image")]
fn hello_reply(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
) -> Result<(), TransportError> {
    hello_reply_caps(transport, frame, serve_caps())
}

#[cfg(feature = "baked-image")]
fn hello_reply_caps(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
    caps: lamella_wire::Capabilities,
) -> Result<(), TransportError> {
    use lamella_wire::{Hello, PROTOCOL_VERSION, ProtocolRange, msg, target_respond};
    let range = ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION };
    match Hello::decode(&frame.payload) {
        Some(hello) => match target_respond(
            &hello,
            range,
            caps,
            profile_identity(),
            transport.max_inbound_payload(),
        ) {
            Ok(ack) => transport.send(msg::HELLO_ACK, frame.seq, &ack.encode()),
            Err(nak) => transport.send(msg::HELLO_NAK, frame.seq, &nak.encode()),
        },
        None => Ok(()),
    }
}

/// How a [`run_debug_session`] resume leg ended.
#[cfg(feature = "baked-image")]
enum RunStop {
    Breakpoint,
    Paused,
    Done(RunResult),
    Trap(RunResult),
    /// A mid-run [`debug::DBG_DETACH`] (already acked): the session is over.
    Detached,
    /// A step landed: the depth predicate was satisfied, or a single step completed.
    Stepped,
    /// A mid-run [`debug::ABORT`] stopped the execution. Carries the aborting request's `seq`, so
    /// the stop event that answers it is attributable to the request that caused it -- an abort is
    /// the one stop a host ASKED for, and reporting it unsolicited would make it indistinguishable
    /// from a program that happened to finish at that moment.
    Aborted(u16),
}

/// The debugged program's current `(method_id, offset)` -- the innermost frame. On the
/// baked (`in_place`) tier the offset is the CIL BYTE offset.
#[cfg(feature = "baked-image")]
fn debug_location(session: &lamella_cil_runtime::Session) -> (u32, u32) {
    let depth = session.depth();
    if depth == 0 {
        return (0, 0);
    }
    session
        .frame(depth - 1)
        .map_or((0, 0), |frame| (frame.method, frame.ip))
}

/// Announce that execution stopped: `reason(u8)`, `method_id(u32 LE)`, `offset(u32 LE)`, and for a
/// `DONE` or a `TRAP` the result tail `exit(i32 LE)`, `flags(u8)`.
///
/// **THERE IS NO OUTPUT TAIL.** A program's output is [`debug::EVT_OUTPUT`] and only that, on
/// every path. Carrying a tail here would put the one message no execution can avoid at the mercy of
/// a frame boundary: a chatty program's output clips the frame that says how the run ended, and it
/// clips it silently.
#[cfg(feature = "baked-image")]
fn send_stopped(
    transport: &mut impl Transport,
    seq: u16,
    why: u8,
    location: (u32, u32),
    exit: Option<i32>,
) -> Result<(), TransportError> {
    let mut payload = Vec::with_capacity(14);
    payload.push(why);
    payload.extend_from_slice(&location.0.to_le_bytes());
    payload.extend_from_slice(&location.1.to_le_bytes());
    if let Some(exit) = exit {
        payload.extend_from_slice(&exit.to_le_bytes());
        payload.push(STOP_FLAGS_RESERVED);
    }
    transport.send(debug::EVT_STOPPED, seq, &payload)
}

#[cfg(feature = "baked-image")]
fn run_result_of(vm: &Vm, value: &Option<lamella_cil_runtime::Value>) -> RunResult {
    let exit = match value {
        Some(Value::Int32(code)) => *code,
        _ => 0,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

/// Applies a [`debug::DBG_BREAK`] payload to `session`: replace the whole breakpoint set with
/// the `(method_id, offset)` pairs it carries (`count(u16 LE)` then `count` x two u32 LE).
///
/// The `offset` is stored VERBATIM as the breakpoint key, and that is deliberate, not a missing
/// conversion: a `baked-image` device runs the interpreter `code-in-place`, under which the
/// executing `frame.ip` is a CIL BYTE offset (interp.rs advances it via `decode_at` -> `next_ip`,
/// a variable-width byte step), and `is_at_breakpoint` matches `breakpoints.get(&(method,
/// frame.ip))` -- so the key domain IS byte offsets. The Lamella Link host sends the srcmap's raw
/// il_offset (a byte offset) precisely because of this (see lamella-wire-host debug_backend: "the
/// wire reports the CIL offset directly, so no index conversion is needed"). Converting the
/// offset to an instruction INDEX here would store a key the byte-domain ip never equals -- the
/// breakpoint would silently miss. Index conversion is the HOST interpreter's job (that Session
/// is `not(code-in-place)`, so its ip IS an index; lamella-dap's interp_backend converts at
/// source-resolution time). If a `baked-image` build ever pairs with a `not(code-in-place)`
/// interpreter, THAT variant -- and only it -- would need the byte->index conversion here.
///
/// Shared by the halted command loop and the mid-run poll, so setting a breakpoint WHILE the
/// program runs behaves identically to setting one while it is stopped.
#[cfg(feature = "baked-image")]
fn apply_breakpoints(session: &mut lamella_cil_runtime::Session, payload: &[u8]) {
    session.clear_breakpoints();
    let count = payload
        .get(0..2)
        .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize);
    for pair in 0..count {
        let base = 2 + pair * 8;
        let Some(bytes) = payload.get(base..base + 8) else {
            break;
        };
        let method = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let offset = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        session.add_breakpoint(method, offset);
    }
}

/// Appends the [`debug::val`] location descriptor of a managed pointer: `kind(u8)` + three
/// `u32 LE` words (unused trailing words are 0). Purely descriptive -- the host renders it;
/// on-device drill-down goes through the [`debug::DBG_EXPAND`] selector instead.
#[cfg(feature = "baked-image")]
fn encode_location(location: &lamella_cil_runtime::Location, out: &mut Vec<u8>) {
    use lamella_cil_runtime::Location;
    let (kind, a, b, c) = match location {
        Location::Local { frame, slot } => (0u8, *frame as u32, *slot as u32, 0),
        Location::Arg { frame, slot } => (1, *frame as u32, *slot as u32, 0),
        Location::Stack { frame, buffer, offset } => (2, *frame as u32, *buffer as u32, *offset),
        Location::Field { object, slot } => (3, object.index(), *slot, 0),
        Location::Element { array, index, byte_offset } => (4, array.index(), *index as u32, *byte_offset),
        Location::StringChar { string, byte_offset } => (5, string.index(), *byte_offset, 0),
        Location::LocalBytes { frame, slot, byte_offset } => (6, *frame as u32, *slot as u32, *byte_offset),
        Location::ArgBytes { frame, slot, byte_offset } => (7, *frame as u32, *slot as u32, *byte_offset),
        Location::Static { slot } => (8, *slot as u32, 0, 0),
        Location::Boxed { object } => (9, object.index(), 0, 0),
        Location::Nested { base, slot } => (10, *slot, location_kind(base), 0),
    };
    out.push(kind);
    out.extend_from_slice(&a.to_le_bytes());
    out.extend_from_slice(&b.to_le_bytes());
    out.extend_from_slice(&c.to_le_bytes());
}

/// The [`encode_location`] kind byte of a location, for flattening a nested pointer's base.
#[cfg(feature = "baked-image")]
fn location_kind(location: &lamella_cil_runtime::Location) -> u32 {
    use lamella_cil_runtime::Location;
    match location {
        Location::Local { .. } => 0,
        Location::Arg { .. } => 1,
        Location::Stack { .. } => 2,
        Location::Field { .. } => 3,
        Location::Element { .. } => 4,
        Location::StringChar { .. } => 5,
        Location::LocalBytes { .. } => 6,
        Location::ArgBytes { .. } => 7,
        Location::Static { .. } => 8,
        Location::Boxed { .. } => 9,
        Location::Nested { .. } => 10,
    }
}

/// Appends one [`debug::val`]-encoded value to a [`debug::DBG_VARS`]/[`debug::DBG_CHILDREN`]
/// payload. Positional and shallow by design: aggregates go
/// out as a handle/count + type token and the host drills down lazily via
/// [`debug::DBG_EXPAND`]; source-local NAMES never cross the wire (the host maps slots
/// through the srcmap). A value whose feature this tier lacks cannot occur; the defensive
/// tail arm keeps the match total under feature unification and reports such a value null.
#[cfg(feature = "baked-image")]
fn encode_value(
    vm: &Vm,
    module: &lamella_cil_runtime::Module,
    value: &lamella_cil_runtime::Value,
    out: &mut Vec<u8>,
) {
    use lamella_cil_runtime::{Object, Value};
    match value {
        Value::Null => out.push(debug::val::NULL),
        Value::Int32(v) => {
            out.push(debug::val::INT32);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int64(v) => {
            out.push(debug::val::INT64);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::NativeInt(v) => {
            out.push(debug::val::NATIVE_INT);
            out.extend_from_slice(&v.to_le_bytes());
        }
        #[cfg(feature = "float")]
        Value::Float(v) => {
            out.push(debug::val::FLOAT);
            out.extend_from_slice(&v.to_le_bytes());
        }
        #[cfg(feature = "float")]
        Value::Single(v) => {
            out.push(debug::val::SINGLE);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Object(reference) => {
            out.push(debug::val::OBJECT);
            out.extend_from_slice(&reference.index().to_le_bytes());
            let token = match vm.heap().get(*reference) {
                Some(Object::Instance { type_id, .. }) => module.type_handle_of(*type_id).unwrap_or(0),
                _ => 0,
            };
            out.extend_from_slice(&token.to_le_bytes());
        }
        Value::Struct(fields) => {
            out.push(debug::val::STRUCT);
            out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        Value::ByRef(location) => {
            out.push(debug::val::BYREF);
            encode_location(location, out);
        }
        #[cfg(feature = "typed-references")]
        Value::TypedRef { location, type_token } => {
            out.push(debug::val::TYPED_REF);
            out.extend_from_slice(&type_token.to_le_bytes());
            encode_location(location, out);
        }
        #[allow(unreachable_patterns)]
        _ => out.push(debug::val::NULL),
    }
}

/// Builds the [`debug::DBG_VARS`] payload for the [`debug::DBG_FRAMES`]-ordered
/// `wire_index` (0 = innermost): the frame's locals then arguments, each a
/// [`debug::val`]-encoded value. An out-of-range frame answers `0, 0`.
#[cfg(feature = "baked-image")]
fn locals_reply(
    session: &lamella_cil_runtime::Session,
    vm: &Vm,
    module: &lamella_cil_runtime::Module,
    request: &[u8],
) -> Vec<u8> {
    let wire_index = request
        .get(0..2)
        .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize);
    let mut payload = Vec::new();
    let session_index = session.depth().checked_sub(1 + wire_index);
    match session_index.and_then(|index| session.frame(index)) {
        Some(view) => {
            payload.extend_from_slice(&(view.locals.len() as u16).to_le_bytes());
            for value in view.locals {
                encode_value(vm, module, value, &mut payload);
            }
            payload.extend_from_slice(&(view.args.len() as u16).to_le_bytes());
            for value in view.args {
                encode_value(vm, module, value, &mut payload);
            }
        }
        None => payload.extend_from_slice(&[0, 0, 0, 0]),
    }
    payload
}

/// Builds the [`debug::DBG_CHILDREN`] payload for a [`debug::DBG_EXPAND`] selector:
/// re-walks the stateless slot/field path from the frame root, then expands the value it
/// lands on. Any unresolvable step (a bad frame, slot, or child index -- e.g. the host
/// raced a resume) answers the EMPTY expansion rather than an error: the pane shows a
/// leaf, and the next stop re-requests fresh.
#[cfg(feature = "baked-image")]
fn expand_reply(
    session: &lamella_cil_runtime::Session,
    vm: &Vm,
    module: &lamella_cil_runtime::Module,
    request: &[u8],
) -> Vec<u8> {
    let empty = alloc::vec![0u8, 0u8, 0u8, 0u8];
    let (Some(frame_bytes), Some(&root_kind), Some(slot_bytes), Some(&path_len)) = (
        request.get(0..2),
        request.get(2),
        request.get(3..5),
        request.get(5),
    ) else {
        return empty;
    };
    let wire_index = u16::from_le_bytes([frame_bytes[0], frame_bytes[1]]) as usize;
    let root_slot = u16::from_le_bytes([slot_bytes[0], slot_bytes[1]]) as usize;
    let Some(view) = session
        .depth()
        .checked_sub(1 + wire_index)
        .and_then(|index| session.frame(index))
    else {
        return empty;
    };
    let roots = if root_kind == 1 { view.args } else { view.locals };
    let Some(mut value) = roots.get(root_slot).cloned() else {
        return empty;
    };
    for step in 0..path_len as usize {
        let base = 6 + step * 2;
        let Some(bytes) = request.get(base..base + 2) else {
            return empty;
        };
        let child = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        let Some(next) = session.expand(vm, &value).into_iter().nth(child) else {
            return empty;
        };
        value = next.value;
    }
    let first = request
        .get(6 + path_len as usize * 2..8 + path_len as usize * 2)
        .map_or(0usize, |at| u16::from_le_bytes([at[0], at[1]]) as usize);
    let max = request
        .get(8 + path_len as usize * 2..10 + path_len as usize * 2)
        .map_or(usize::MAX, |at| match u16::from_le_bytes([at[0], at[1]]) {
            0 => usize::MAX,
            limit => limit as usize,
        });
    let children = session.expand(vm, &value);
    let total = children.len();
    let mut payload = Vec::new();
    payload.extend_from_slice(&(total as u16).to_le_bytes());
    let window: Vec<_> = children.into_iter().skip(first).take(max).collect();
    payload.extend_from_slice(&(window.len() as u16).to_le_bytes());
    for child in window {
        let name = child.name.as_bytes();
        let len = name.len().min(255);
        payload.push(len as u8);
        payload.extend_from_slice(&name[..len]);
        encode_value(vm, module, &child.value, &mut payload);
    }
    payload
}

/// How many interpreter steps a serve run driver takes between servicing the polled Lamella Link
/// carrier (and, on the debug path, checking for a mid-run command). Op-count, not wall-clock -- the
/// portable runner has no time source -- so it is a knob balancing keepalive against poll overhead:
/// smaller keeps the carrier alive through slower I/O-bound loops (a tight `Mmio`-poll loop), and it
/// matches the interpreter's own scheduler quantum, so the `run_serviced` (RUN_IMAGE) path services
/// at the same cadence. A board-supplied wall-clock budget is the robust future refinement.
#[cfg(feature = "baked-image")]
const RUN_SERVICE_STEPS: u32 = 256;

/// How much of each of a run's output streams has already gone out as a [`debug::EVT_OUTPUT`] frame.
///
/// One cursor per stream, held across a whole run so a program that prints in a loop streams once
/// per service point rather than re-sending its history.
#[cfg(feature = "baked-image")]
#[derive(Clone, Copy, Debug, Default)]
struct OutputCursors {
    /// Units of `vm.output()` already sent on [`debug::output::STDOUT`].
    stdout: usize,
    /// Units of `vm.debug_output()` already sent on [`debug::output::DEBUG`].
    debug: usize,
}

#[cfg(feature = "baked-image")]
impl OutputCursors {
    /// The cursors positioned at everything the machine has ALREADY produced, so a resume streams
    /// what happens next rather than replaying what the host has seen.
    fn at_end_of(vm: &Vm) -> Self {
        Self { stdout: vm.output().len(), debug: vm.debug_output().len() }
    }
}

/// Sends whatever the program has produced on each stream since the cursors, as
/// [`debug::EVT_OUTPUT`] frames, and advances the cursors past what went out.
///
/// The delta is taken from the VM's own buffers rather than a tap, because a buffer IS the record
/// and a cursor over it cannot lose a write or double-send one -- whereas a tap is a `fn` pointer
/// that cannot capture this transport, and a side buffer for it would be a second copy of the same
/// bytes with its own overflow question.
///
/// Nothing is sent when there is nothing new, so an idle or silent program adds no wire traffic.
#[cfg(feature = "baked-image")]
fn stream_output(
    transport: &mut impl Transport,
    vm: &Vm,
    sent: &mut OutputCursors,
) -> Result<(), TransportError> {
    stream_units(transport, debug::output::STDOUT, vm.output(), &mut sent.stdout)?;
    stream_units(transport, debug::output::DEBUG, vm.debug_output(), &mut sent.debug)
}

/// One stream's unsent tail as an [`debug::EVT_OUTPUT`] frame: `stream(u8)`, `flags(u8)`, bytes.
///
/// A TRAILING HIGH SURROGATE IS HELD BACK, so a frame never carries half of a pair: the host can
/// then decode each frame on its own. It costs one comparison here and saves every host a
/// cross-frame joining rule.
#[cfg(feature = "baked-image")]
fn stream_units(
    transport: &mut impl Transport,
    stream: u8,
    units: &[u16],
    sent: &mut usize,
) -> Result<(), TransportError> {
    let mut end = units.len();
    if end <= *sent {
        return Ok(());
    }
    if matches!(units[end - 1], 0xD800..=0xDBFF) {
        end -= 1;
        if end <= *sent {
            return Ok(());
        }
    }
    let text = String::from_utf16_lossy(&units[*sent..end]);
    *sent = end;
    send_output(transport, stream, &text)
}

/// One [`debug::EVT_OUTPUT`] frame of `text` on `stream`.
///
/// The line-boundary flag is computed HERE because only the target knows where its lines end, and a
/// host interleaving two streams into one terminal has to know whether it is mid-line -- a question
/// one stream never raised and two do.
fn send_output(
    transport: &mut impl Transport,
    stream: u8,
    text: &str,
) -> Result<(), TransportError> {
    let mut payload = Vec::with_capacity(2 + text.len());
    payload.push(stream);
    payload.push(if text.ends_with('\n') { debug::output::ENDS_ON_LINE_BOUNDARY } else { 0 });
    payload.extend_from_slice(text.as_bytes());
    transport.send(debug::EVT_OUTPUT, 0, &payload)
}

/// Report a RUNTIME FAULT -- a trap, a request the tier cannot serve -- on the ERROR stream, and
/// build the in-process result that carries it back to the firmware.
///
/// # Why the error stream and not the program's own output
///
/// The text is the RUNNER's, not the program's. Splicing it into `Console.Out` leaves a host with
/// no way to tell a diagnostic ABOUT a program from something the program printed, so a trap
/// report renders as though the program had written it. .NET puts an unhandled-exception trace
/// on standard error for
/// exactly this reason, and the wire now has a stream for it.
#[cfg(feature = "baked-image")]
fn fault(
    transport: &mut impl Transport,
    vm: &Vm,
    text: &str,
) -> Result<RunResult, TransportError> {
    send_output(transport, debug::output::STDERR, text)?;
    let mut stdout = String::from_utf16_lossy(vm.output());
    stdout.push_str(text);
    Ok(RunResult { exit: 70, stdout })
}

/// Report a fault that ENDS an execution: the reason on the error stream, then a terminal
/// [`debug::EVT_STOPPED`] carrying the trap exit code.
///
/// Two frames rather than one, because the stop event carries no output tail. Without the first
/// frame a host learns that something trapped and never learns what -- which is the shape of the
/// message that sends someone to look at a cable.
#[cfg(feature = "baked-image")]
fn fault_stop(
    transport: &mut impl Transport,
    seq: u16,
    text: &str,
) -> Result<(), TransportError> {
    send_output(transport, debug::output::STDERR, text)?;
    send_stopped(transport, seq, debug::reason::TRAP, (0, 0), Some(TRAP_EXIT))
}

/// The exit code a trapped run reports, matching the interpreter's abort convention.
#[cfg(feature = "baked-image")]
const TRAP_EXIT: i32 = 70;

/// Step until the call stack is no deeper than `floor`, or -- with `floor` `None` -- exactly one
/// instruction.
///
/// This is the whole of step-OVER and step-OUT, and it is deliberately one predicate rather than
/// two: over is *do not go deeper than where I am*, out is *do not stop until I am shallower*, and
/// both are a bound on depth checked at a boundary the single-step loop already crosses.
///
/// **A breakpoint inside the skipped call still stops it.** A step-over that ran straight past a
/// breakpoint the user set in the callee would be a debugger that ignores breakpoints when it feels
/// like it, which is worse than not having step-over.
///
/// **The carrier is serviced on the same cadence a resume uses**, so a step-over of a call that
/// never returns is abortable rather than a wedged board. That is not a nicety here: taking the
/// depth predicate off the host is what makes stepping usable on a serial carrier, and it also
/// makes the target the only thing that can end a runaway callee.
#[cfg(feature = "baked-image")]
fn step_to_depth(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    vm: &mut Vm,
    session: &mut lamella_cil_runtime::Session,
    floor: Option<usize>,
    sent: &mut OutputCursors,
    caps: lamella_wire::Capabilities,
) -> Result<RunStop, TransportError> {
    use lamella_cil_runtime::Status;
    let mut until_service = RUN_SERVICE_STEPS;
    loop {
        match session.step(module, vm) {
            Ok(Status::Done(value)) => {
                stream_output(transport, vm, sent)?;
                return Ok(RunStop::Done(run_result_of(vm, &value)));
            }
            Err(trap) => {
                stream_output(transport, vm, sent)?;
                return Ok(RunStop::Trap(fault(transport, vm, &format!("TRAP: {trap:?}"))?));
            }
            Ok(Status::Running | Status::Paused) => {}
        }
        let landed = floor.is_none_or(|floor| session.depth() <= floor && session.depth() > 0);
        if session.is_at_breakpoint() {
            stream_output(transport, vm, sent)?;
            return Ok(RunStop::Breakpoint);
        }
        if landed {
            stream_output(transport, vm, sent)?;
            return Ok(RunStop::Stepped);
        }
        until_service -= 1;
        if until_service == 0 {
            until_service = RUN_SERVICE_STEPS;
            stream_output(transport, vm, sent)?;
            if let Some(stop) = service_wire(transport, session, caps)? {
                return Ok(stop);
            }
        }
    }
}

/// Report one stop to the host, and say whether the debug session CONTINUES.
///
/// One place rather than one per command, because every stop answers the same three questions --
/// which reason, whether a result tail belongs on it, and whether anything is left to resume -- and
/// three copies of that would gain their next case in one of them. `at_reported_breakpoint` is the
/// parked state the next resume steps off first, so it is settled here beside the reason that sets
/// it.
#[cfg(feature = "baked-image")]
fn report_stop(
    transport: &mut impl Transport,
    seq: u16,
    stop: RunStop,
    session: &lamella_cil_runtime::Session,
    at_reported_breakpoint: &mut bool,
) -> Result<bool, TransportError> {
    use debug::reason;
    *at_reported_breakpoint = matches!(stop, RunStop::Breakpoint);
    match stop {
        RunStop::Stepped => {
            send_stopped(transport, seq, reason::STEP, debug_location(session), None)?;
            Ok(true)
        }
        RunStop::Breakpoint => {
            send_stopped(transport, seq, reason::BREAKPOINT, debug_location(session), None)?;
            Ok(true)
        }
        RunStop::Paused => {
            send_stopped(transport, seq, reason::PAUSED, debug_location(session), None)?;
            Ok(true)
        }
        RunStop::Done(result) => {
            send_stopped(transport, seq, reason::DONE, (0, 0), Some(result.exit))?;
            Ok(false)
        }
        RunStop::Trap(result) => {
            send_stopped(transport, seq, reason::TRAP, (0, 0), Some(result.exit))?;
            Ok(false)
        }
        RunStop::Aborted(abort_seq) => {
            send_stopped(transport, abort_seq, reason::ABORTED, debug_location(session), None)?;
            Ok(false)
        }
        RunStop::Detached => Ok(false),
    }
}

/// Run until a breakpoint, completion, a trap, or a [`debug::DBG_PAUSE`]: bounded bursts
/// of steps with a wire poll between bursts, so a running target stays pause-able.
/// A mid-run `HELLO` is answered with `caps` and the program KEEPS RUNNING; a mid-run
/// [`debug::ABORT`] ends it, and a mid-run detach is acked and ends the session. Without those, a
/// resume over a non-terminating program would leave the Lamella Link permanently deaf on every
/// carrier.
///
/// Console output is streamed as it appears ([`debug::EVT_OUTPUT`]) rather than only in the
/// terminal frame, so a long-running program is visible while it runs.
#[cfg(feature = "baked-image")]
fn run_until_stop(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    vm: &mut Vm,
    session: &mut lamella_cil_runtime::Session,
    caps: lamella_wire::Capabilities,
) -> Result<RunStop, TransportError> {
    use lamella_cil_runtime::{PendingOp, Status};
    let mut sent = OutputCursors::at_end_of(vm);
    loop {
        for _ in 0..RUN_SERVICE_STEPS {
            match session.step(module, vm) {
                Ok(Status::Done(value)) => {
                    stream_output(transport, vm, &mut sent)?;
                    return Ok(RunStop::Done(run_result_of(vm, &value)));
                }
                Err(trap) => {
                    stream_output(transport, vm, &mut sent)?;
                    return Ok(RunStop::Trap(fault(
                        transport,
                        vm,
                        &format!("TRAP: {trap:?}"),
                    )?));
                }
                Ok(Status::Running | Status::Paused) => {
                    if session.is_at_breakpoint() {
                        stream_output(transport, vm, &mut sent)?;
                        return Ok(RunStop::Breakpoint);
                    }
                }
            }
            match lamella_cil_runtime::take_pending_op(vm) {
                PendingOp::None | PendingOp::Yield => {}
                PendingOp::SleepUntil(deadline) => {
                    stream_output(transport, vm, &mut sent)?;
                    if let Some(stop) = wait_until(transport, vm, session, deadline, caps)? {
                        return Ok(stop);
                    }
                }
                PendingOp::NeedsScheduler(what) => {
                    stream_output(transport, vm, &mut sent)?;
                    return Ok(RunStop::Trap(fault(
                        transport,
                        vm,
                        &format!(
                            "{what} needs the scheduler, which a debug session does not run: \
                             attach steps ONE thread. Deploy it and EXEC it to use threads."
                        ),
                    )?));
                }
            }
        }
        stream_output(transport, vm, &mut sent)?;
        if let Some(stop) = service_wire(transport, session, caps)? {
            return Ok(stop);
        }
    }
}

/// Hold the session until `deadline` (monotonic ms), keeping the carrier serviced.
///
/// A sleeping program executes no instructions, so the burst loop's poll never comes round: without
/// this the target would stop answering for the length of the sleep, and a host could neither pause
/// it nor take it back. `None` means the deadline arrived; a `Some` is the host ending the run.
///
/// It spins on the clock rather than asking the board to sleep, which keeps the poll continuous --
/// there is nothing else for this tier to run while one thread waits.
#[cfg(feature = "baked-image")]
fn wait_until(
    transport: &mut impl Transport,
    vm: &mut Vm,
    session: &mut lamella_cil_runtime::Session,
    deadline: u64,
    caps: lamella_wire::Capabilities,
) -> Result<Option<RunStop>, TransportError> {
    while vm.now_millis().is_some_and(|now| now < deadline) {
        if let Some(stop) = service_wire(transport, session, caps)? {
            return Ok(Some(stop));
        }
    }
    Ok(None)
}

/// One pass of the mid-run wire contract, shared by the burst loop and the sleep wait so the two
/// cannot drift: a `DBG_PAUSE` stops, a `HELLO` reclaims, a `DBG_DETACH` acks and ends, breakpoints
/// may be edited without pausing first, and anything else is dropped. `None` = keep running.
#[cfg(feature = "baked-image")]
fn service_wire(
    transport: &mut impl Transport,
    session: &mut lamella_cil_runtime::Session,
    caps: lamella_wire::Capabilities,
) -> Result<Option<RunStop>, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(None);
    };
    match frame.msg_type {
        debug::DBG_PAUSE => return Ok(Some(RunStop::Paused)),
        debug::ABORT => return Ok(Some(RunStop::Aborted(frame.seq))),
        lamella_wire::msg::HELLO => {
            hello_reply_caps(transport, &frame, caps)?;
        }
        debug::DBG_DETACH => {
            transport.send(debug::DBG_ACK, frame.seq, &[])?;
            return Ok(Some(RunStop::Detached));
        }
        debug::DBG_BREAK => {
            apply_breakpoints(session, &frame.payload);
            transport.send(debug::DBG_ACK, frame.seq, &[])?;
        }
        _ => {}
    }
    Ok(None)
}

/// Debug a baked image over the wire: boot it HALTED at the entry, report
/// [`debug::EVT_STOPPED`] (reason `Entry`), then serve debug commands until the program
/// completes, traps, or the host detaches. See the [`debug`] message set; a device
/// firmware reaches this through [`serve_one_baked`]'s `DBG_IMAGE` arm.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_debug_session(
    transport: &mut impl Transport,
    image: Vec<u8>,
    image_seq: u16,
) -> Result<(), TransportError> {
    let image: &'static [u8] = Box::leak(image.into_boxed_slice());
    run_debug_session_static(transport, image, None, image_seq, serve_caps(), &mut |_| {})
}

/// [`run_debug_session`] over an ALREADY-RESIDENT image -- the [`debug::DBG_ATTACH`] arm:
/// a deploy-capable target debugs the flash region it already holds, so nothing but
/// commands crosses the wire. `caps` is what a mid-session `HELLO` (a NEW host adopting a
/// stale session) is answered with -- the serve tier's own set. `configure` is the
/// firmware's [`Vm`] hook, the SAME one every other run path gets -- an attach-run must see
/// the board's backends (network, output policy), or "attach and resume to completion"
/// silently runs a lesser machine than a deployed boot does.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_debug_session_static(
    transport: &mut impl Transport,
    image: &'static [u8],
    corlib: Option<&'static [u8]>,
    image_seq: u16,
    caps: lamella_wire::Capabilities,
    configure: &mut dyn FnMut(&mut Vm),
) -> Result<(), TransportError> {
    use debug::reason;
    use lamella_cil_runtime::Session;

    let (module, entry) = match load_deployed(image, corlib) {
        Ok(booted) => booted,
        Err(why) => return fault_stop(transport, image_seq, &why),
    };
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    #[cfg(target_os = "none")]
    vm.set_mmio_subword(
        lamella_mmio::write8,
        lamella_mmio::read8,
        lamella_mmio::write16,
        lamella_mmio::read16,
    );
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    configure(&mut vm);
    let entry = match lamella_cil_runtime::boot_baked(&module, &mut vm, entry) {
        Ok(entry) => entry,
        Err(trap) => {
            return fault_stop(transport, image_seq, &format!("static constructor: {trap:?}"));
        }
    };
    let mut session = match Session::new(&module, entry, Vec::new()) {
        Ok(session) => session,
        Err(trap) => return fault_stop(transport, image_seq, &format!("session: {trap:?}")),
    };
    let mut at_reported_breakpoint = false;
    let mut sent = OutputCursors::at_end_of(&vm);
    send_stopped(transport, image_seq, reason::ENTRY, debug_location(&session), None)?;

    loop {
        let Some(frame) = transport.poll()? else {
            continue;
        };
        match frame.msg_type {
            debug::DBG_STEP => {
                let mode = frame.payload.first().copied().unwrap_or(debug::step_mode::IN);
                let floor = match mode {
                    debug::step_mode::OVER => Some(session.depth()),
                    debug::step_mode::OUT => Some(session.depth().saturating_sub(1)),
                    _ => None,
                };
                let stop =
                    step_to_depth(transport, &module, &mut vm, &mut session, floor, &mut sent, caps)?;
                if !report_stop(transport, frame.seq, stop, &session, &mut at_reported_breakpoint)? {
                    return Ok(());
                }
            }
            debug::DBG_RESUME => {
                if at_reported_breakpoint {
                    let stepped =
                        step_to_depth(transport, &module, &mut vm, &mut session, None, &mut sent, caps)?;
                    if !matches!(stepped, RunStop::Stepped | RunStop::Breakpoint) {
                        report_stop(transport, frame.seq, stepped, &session, &mut at_reported_breakpoint)?;
                        return Ok(());
                    }
                    at_reported_breakpoint = false;
                }
                let stop = run_until_stop(transport, &module, &mut vm, &mut session, caps)?;
                if !report_stop(transport, frame.seq, stop, &session, &mut at_reported_breakpoint)? {
                    return Ok(());
                }
            }
            debug::DBG_BREAK => {
                apply_breakpoints(&mut session, &frame.payload);
                transport.send(debug::DBG_ACK, frame.seq, &[])?;
            }
            debug::DBG_STACK => {
                let depth = session.depth();
                let mut payload = Vec::with_capacity(2 + depth * 8);
                payload.extend_from_slice(&(depth as u16).to_le_bytes());
                for index in (0..depth).rev() {
                    let (method, offset) =
                        session.frame(index).map_or((0, 0), |frame| (frame.method, frame.ip));
                    payload.extend_from_slice(&method.to_le_bytes());
                    payload.extend_from_slice(&offset.to_le_bytes());
                }
                transport.send(debug::DBG_FRAMES, frame.seq, &payload)?;
            }
            debug::DBG_LOCALS => {
                let payload = locals_reply(&session, &vm, &module, &frame.payload);
                transport.send(debug::DBG_VARS, frame.seq, &payload)?;
            }
            debug::DBG_EXPAND => {
                let payload = expand_reply(&session, &vm, &module, &frame.payload);
                transport.send(debug::DBG_CHILDREN, frame.seq, &payload)?;
            }
            debug::DBG_PAUSE => {
                send_stopped(transport, frame.seq, reason::PAUSED, debug_location(&session), None)?;
            }
            debug::ABORT => {
                stream_output(transport, &vm, &mut sent)?;
                return send_stopped(
                    transport,
                    frame.seq,
                    reason::ABORTED,
                    debug_location(&session),
                    None,
                );
            }
            exec::EXEC_STATUS => {
                send_stopped(transport, frame.seq, reason::PAUSED, debug_location(&session), None)?;
            }
            load::LOAD_PE | load::LOAD_IMAGE | load::LOAD_BUNDLE | load::LOAD_CLEAR => {
                let payload = lamella_wire::error::unknown_message_type(frame.msg_type);
                transport.send(lamella_wire::msg::ERROR, frame.seq, &payload)?;
            }
            debug::DBG_DETACH => {
                transport.send(debug::DBG_ACK, frame.seq, &[])?;
                return Ok(());
            }
            lamella_wire::msg::HELLO => {
                hello_reply_caps(transport, &frame, caps)?;
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Boot a DEPLOYED ARTIFACT, which may be either a baked image or a bare program PE, deciding by
/// the artifact's own first four bytes: `LMLI` is a baked image, `MZ` is a PE.
///
/// # Why a PE is worth booting at all
///
/// A baked image is self-contained -- it carries the corlib it needs -- which is what makes it
/// deployable to a board with nothing resident. That is also its cost: the image is dominated by
/// the corlib rather than by the program, so a small program bakes to a large image, and a device
/// that already holds a resident corlib is storing a second copy of it per deploy.
///
/// A device that HAS one can take the program alone and resolve its corlib references against the
/// resident bytes, so the deployed artifact is the PE and nothing else.
///
/// # The magic is the discriminator, so nothing on the wire changes
///
/// The deploy protocol writes bytes into the image region and the boot path reads them back; it has
/// never inspected them. Both artifacts are self-identifying, so a host may deploy either and an
/// older host keeps working unchanged. A PE arriving at a device with NO resident corlib is the one
/// case that must fail loudly rather than silently: it names the situation instead of trapping later
/// on the first unresolved corlib call.
///
/// # How the PE's corlib references are resolved
///
/// By default, EAGERLY: the whole resident corlib is loaded into RAM and the program binds against
/// it. That costs about a megabyte and is the right answer on a host or a roomy part.
///
/// Under `corlib-lazy` it is the constrained tier's resolution instead -- the corlib stays in flash
/// and only the members the program reaches are materialized, which is what a 256 KB part can
/// afford. The two produce the same output and the same exit value; they differ in RAM and in when
/// an unresolvable member is reported (the lazy path names it here, rather than trapping at run).
///
/// # Errors
/// A message naming what was wrong: an unrecognized artifact, a PE with no resident corlib to
/// resolve against, a malformed assembly, an image that records no entry point, or -- on the
/// constrained tier -- a corlib member the resident corlib does not carry.
#[cfg(feature = "baked-image")]
pub fn load_deployed(
    artifact: &'static [u8],
    corlib: Option<&'static [u8]>,
) -> Result<(lamella_cil_runtime::Module, lamella_cil_runtime::MethodId), String> {
    match artifact.get(..2) {
        Some(b"LM") => match lamella_cil_runtime::Module::from_baked(artifact) {
            Ok((module, Some(entry))) => Ok((module, entry)),
            Ok((_, None)) => Err(String::from("image records no entry point")),
            Err(error) => Err(format!("image does not boot: {error:?}")),
        },
        Some(b"MZ") => {
            let Some(corlib) = corlib else {
                return Err(String::from(
                    "deployed a bare PE but this firmware has no resident corlib to resolve it against",
                ));
            };
            let corlib = Assembly::read(corlib)
                .map_err(|error| format!("resident corlib does not parse: {error:?}"))?;
            let program = Assembly::read(artifact)
                .map_err(|error| format!("deployed PE does not parse: {error:?}"))?;
            #[cfg(not(feature = "corlib-lazy"))]
            let loaded = load_with_corlib(&corlib, &program)
                .map_err(|error| format!("deployed PE does not load: {error:?}"))?;
            #[cfg(feature = "corlib-lazy")]
            let loaded = lamella_load::load_program_lazy_corlib(&corlib, &program)
                .map_err(|error| format!("deployed PE does not load: {error}"))?;
            Ok((loaded.module, loaded.entry))
        }
        _ => Err(String::from(
            "deployed artifact is neither a baked image (LMLI) nor a program PE (MZ)",
        )),
    }
}

/// Boot a baked image ([`lamella_cil_runtime::Module::from_baked`]) and run its entry point,
/// capturing console output + exit code -- [`run_program`]'s twin for the PE-less path.
///
/// **The image bytes are LEAKED to `'static`** -- the loader borrows an image in place rather than
/// copying it, and the type it produces is not generic over a lifetime, so the bytes must outlive
/// every reference the run can create. One leak per evaluation.
///
/// # On a device this retains one image per request, and only one allocator hides that
///
/// A bump arena's reset between evaluations does reclaim this leak: the reset moves one pointer and
/// takes every allocation with it, dropped or not. **A reclaiming heap does not.** Its per-request
/// rewind is a no-op by construction -- it frees through `Drop`, and a leaked allocation never
/// drops. So the board whose collector actually reclaims is the board that runs out first, which is
/// the reverse of what the two allocators' names suggest.
///
/// **And the bump arena is not vindicated by hiding it**: it rewinds N images' worth of leak per
/// request rather than not having them.
///
/// **A device that serves repeatedly should bound this** by supplying an [`ImageResidence`] to
/// [`serve_one_baked_with_residence`], which reuses one buffer instead of retaining every image.
/// This entry point keeps leaking because a host runner evaluating once has nothing to gain from a
/// ceiling it would have to pick.
#[cfg(feature = "baked-image")]
#[must_use]
pub fn run_image(image: Vec<u8>) -> RunResult {
    run_image_with(image, &mut |_vm| {})
}

/// [`run_image`] with a firmware hook that configures the fresh [`Vm`] before anything
/// runs (static constructors included): a device installs its board seams here -- the
/// clock, the networking backend -- exactly as a host embedder does after `Vm::default()`.
/// The runner cannot know a board's backends (they live in the firmware crate), so the
/// firmware hands them in per evaluation.
#[cfg(feature = "baked-image")]
#[must_use]
pub fn run_image_with(image: Vec<u8>, configure: &mut dyn FnMut(&mut Vm)) -> RunResult {
    run_image_serviced(image, None, configure, &mut |_vm| {})
}

/// [`run_image_with`] plus a `service` callback the interpreter fires at every scheduler quantum
/// (~256 instructions), single- OR multi-threaded. A device serve passes a USB-poll closure so a
/// running image's tight loop cannot starve the POLLED Lamella Link carrier -- without it a
/// single-threaded program runs to completion without the serve ever pumping USB and the host drops
/// the Link mid-run. The host [`run_image`] path passes a no-op.
#[cfg(feature = "baked-image")]
#[must_use]
pub fn run_image_serviced(
    image: Vec<u8>,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    service: &mut dyn FnMut(&Vm),
) -> RunResult {
    run_image_static(Box::leak(image.into_boxed_slice()), corlib, configure, service)
}

/// [`run_image_serviced`] over bytes the caller has already placed somewhere that outlives the run.
///
/// **This is the entry point that does not leak.** A device serving repeatedly reaches it through
/// [`serve_one_baked_with_residence`], having put the image in a buffer it reuses; the retention is
/// then one image rather than one per request. See [`run_image`] for why the arena rewind that
/// appears to cover the difference does not.
#[cfg(feature = "baked-image")]
#[must_use]
pub fn run_image_static(
    image: &'static [u8],
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    service: &mut dyn FnMut(&Vm),
) -> RunResult {
    let (result, fault) = run_image_reporting(image, corlib, configure, &mut |vm| {
        service(vm);
        true
    });
    match fault {
        Some(text) => RunResult { exit: result.exit, stdout: result.stdout + &text },
        None => result,
    }
}

/// [`run_image_static`] with the FAULT REASON kept apart from the program's own output.
///
/// The reason a run failed is the RUNNER's text, not the program's, and it is the half a host needs
/// on its error stream. Returning it separately is what lets the serve put each on the stream it
/// belongs to -- and it is also the fix for a plain run reporting exit 70 and no reason at all,
/// which was every trap this path ever hit.
#[cfg(feature = "baked-image")]
fn run_image_reporting(
    image: &'static [u8],
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    service: &mut dyn FnMut(&Vm) -> bool,
) -> (RunResult, Option<String>) {
    let (module, entry) = match load_deployed(image, corlib) {
        Ok(booted) => booted,
        Err(why) => return (RunResult { exit: -1, stdout: String::new() }, Some(why)),
    };
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    #[cfg(target_os = "none")]
    vm.set_mmio_subword(
        lamella_mmio::write8,
        lamella_mmio::read8,
        lamella_mmio::write16,
        lamella_mmio::read16,
    );
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    configure(&mut vm);
    let entry = match lamella_cil_runtime::boot_baked(&module, &mut vm, entry) {
        Ok(entry) => entry,
        Err(trap) => {
            let stdout = String::from_utf16_lossy(vm.output());
            return (
                RunResult { exit: TRAP_EXIT, stdout },
                Some(format!("BOOT TRAP (static constructor): {trap:?}")),
            );
        }
    };
    let outcome =
        lamella_cil_runtime::run_interruptible(&module, &mut vm, entry, Vec::new(), service);
    let (exit, fault) = match outcome {
        Ok(lamella_cil_runtime::Ran::Finished(Some(Value::Int32(code)))) => (code, None),
        Ok(lamella_cil_runtime::Ran::Finished(_)) => (0, None),
        Ok(lamella_cil_runtime::Ran::Interrupted) => (INTERRUPTED_EXIT, None),
        Err(trap) => (TRAP_EXIT, Some(format!("TRAP: {trap:?}"))),
    };
    (RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }, fault)
}

/// The exit code a run that was INTERRUPTED reports: the program produced no value, and the caller
/// that asked for the stop is the one that knows why.
#[cfg(feature = "baked-image")]
const INTERRUPTED_EXIT: i32 = -2;

/// Serve one pending request on a PE-less BAKED-IMAGE target: a `HELLO` gets a `HELLO_ACK`
/// advertising `Capabilities::BAKED_IMAGE` (or a `HELLO_NAK` on version mismatch), a chunked
/// [`load::LOAD_IMAGE`] places an image in RAM, and an [`exec::EXEC`] runs it.
/// Returns whether a frame was handled. A device firmware's main loop is this call in a
/// loop, plus its own storage reset between evaluations (nothing survives a request by
/// design -- the host-stateless REPL re-sends whole sessions).
///
/// `load` is the firmware's transfer arena: it has to outlive one frame, because a transfer is
/// chunked, and this crate has nowhere to keep it that a device would tolerate.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_baked(
    transport: &mut impl Transport,
    load: &mut ArtifactLoad,
) -> Result<bool, TransportError> {
    serve_one_baked_with(transport, &mut |_vm| {}, load)
}

/// [`serve_one_baked`] with the firmware's [`Vm`]-configure hook (see [`run_image_with`]): every
/// evaluation this serve runs gets the board's seams installed.
///
/// A board with no deploy region still needs it, for the same reason a deploy-capable one does: the
/// CLOCK arrives through this hook, and an evaluation that runs without one computes a
/// `Thread.Sleep` deadline that has already passed and does not wait.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_baked_with(
    transport: &mut impl Transport,
    configure: &mut dyn FnMut(&mut Vm),
    load: &mut ArtifactLoad,
) -> Result<bool, TransportError> {
    serve_one_baked_with_residence(transport, configure, &mut LeakEachImage, load)
}

/// Where a served image's bytes live for as long as the loader can reach them.
///
/// # Why this is the caller's choice and not this crate's
///
/// The loader borrows an image in place and the type it produces is not generic over a lifetime, so
/// the bytes have to be `'static`. This crate can only get that by leaking, and it cannot take a
/// leak back: it forbids unsafe code, and recovering a leaked allocation is the one thing that
/// needs it. **So an unbounded number of served images is not a policy this crate chose -- it is
/// the only policy it can implement.**
///
/// The caller can do better, because the caller knows two things this crate does not: how much
/// memory the board has, and that **nothing survives a request by design** -- the module and the
/// machine are dropped before the next frame is read. That second fact is what makes reuse sound,
/// and it is knowable exactly where the serve loop is written and nowhere else.
///
/// A host implementation should just leak ([`LeakEachImage`]); a host evaluating once gains nothing
/// from a ceiling it would have to invent. A device implementation should hand back slices of one
/// buffer it owns for the lifetime of the program.
#[cfg(feature = "baked-image")]
pub trait ImageResidence {
    /// Place one image's bytes where the loader can borrow them for as long as it needs.
    ///
    /// `None` refuses the image -- the bytes do not fit whatever the implementation set aside. A
    /// refusal is reported to the host as a failed run rather than dropped, because an image that
    /// is too large for the board is a fact the person who sent it needs.
    fn admit(&mut self, image: Vec<u8>) -> Option<&'static [u8]>;
}

/// A chunked artifact transfer into RAM, and the artifact it produces.
///
/// A LOAD does not start anything -- [`exec::EXEC`] does -- so the two halves of "run this" are
/// separable: a host can place an artifact and decide later whether to run it, run it halted, or
/// discard it. The state lives in the FIRMWARE and is threaded into the serve, because it has to
/// survive between frames and this crate has nowhere to keep it that a device would tolerate.
///
/// # The artifact KIND rides on every chunk, not only the last
///
/// The type byte of each `LOAD_x` says what is being transferred, so an interrupted transfer cannot
/// be misread as a partial artifact of another kind, and a target reassembling bytes never holds
/// them in a kind-unknown state. A chunk whose kind differs from the transfer in progress is
/// treated as the start of a new one, which is the same rule as `offset == 0`.
#[derive(Default)]
pub struct ArtifactLoad {
    /// The `LOAD_x` type byte of the transfer in progress, or of the artifact held.
    kind: u8,
    /// The `total` the first chunk declared.
    total: usize,
    /// The bytes accumulated so far.
    bytes: Vec<u8>,
    /// Whether `bytes` reached `total` -- an artifact is HELD, not merely in flight.
    complete: bool,
    /// The completed artifact once an [`ImageResidence`] has placed it where a loader can borrow it
    /// for as long as it needs.
    #[cfg(feature = "baked-image")]
    ready: Option<&'static [u8]>,
}

impl ArtifactLoad {
    /// An empty arena: nothing in flight and nothing held.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one chunk -- `offset(u32 LE)`, `total(u32 LE)`, then bytes -- and report the
    /// [`lamella_wire::msg::xfer`] status a [`load::XFER_RESULT`] carries.
    ///
    /// A chunk at offset 0 DISCARDS whatever incomplete transfer this arena held, including one
    /// begun by a different `LOAD_x`: within the load domain there is one transfer at a time. A
    /// chunk anywhere else must continue exactly where the last one ended, because a gap would be an
    /// artifact with a hole in it that still satisfies the completion rule.
    fn chunk(&mut self, kind: u8, payload: &[u8]) -> u8 {
        use lamella_wire::msg::{CHUNK_HEADER_LEN, xfer};
        let Some(header) = payload.get(..CHUNK_HEADER_LEN) else {
            return xfer::RANGE_REJECTED;
        };
        let offset = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let total = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let data = &payload[CHUNK_HEADER_LEN..];
        if offset == 0 {
            self.clear();
            self.kind = kind;
            self.total = total;
        } else if kind != self.kind || offset != self.bytes.len() || total != self.total {
            return xfer::RANGE_REJECTED;
        }
        if offset + data.len() > total {
            return xfer::RANGE_REJECTED;
        }
        self.bytes.extend_from_slice(data);
        self.complete = self.bytes.len() == total;
        xfer::MATCHED
    }

    /// Discard everything: the transfer in progress, and any completed artifact.
    ///
    /// The completed one goes too, because [`load::LOAD_CLEAR`] means *reclaim the arena* and a host
    /// that has cleared and then sends [`exec::EXEC`] must be told there is nothing to run rather
    /// than get the artifact before last.
    fn clear(&mut self) {
        self.kind = 0;
        self.total = 0;
        self.bytes.clear();
        self.complete = false;
        #[cfg(feature = "baked-image")]
        {
            self.ready = None;
        }
    }

    /// The complete artifact's bytes and kind, borrowed for as long as this arena is untouched.
    /// `None` while a transfer is still in flight.
    ///
    fn held(&self) -> Option<(u8, &[u8])> {
        if !self.complete {
            return None;
        }
        #[cfg(feature = "baked-image")]
        if let Some(placed) = self.ready {
            return Some((self.kind, placed));
        }
        Some((self.kind, self.bytes.as_slice()))
    }

    /// Hand a COMPLETED transfer to the residence, so a loader can borrow it for as long as it
    /// needs. `false` refuses -- the bytes do not fit what this board set aside.
    ///
    /// Separate from [`ArtifactLoad::chunk`] because the placement is what needs `'static` and the
    /// accumulation is not: the reference runner runs a program out of a borrowed slice and has no
    /// residence at all.
    #[cfg(feature = "baked-image")]
    fn place(&mut self, residence: &mut dyn ImageResidence) -> bool {
        if !self.complete || self.ready.is_some() {
            return true;
        }
        let bytes = core::mem::take(&mut self.bytes);
        match residence.admit(bytes) {
            Some(placed) => {
                self.ready = Some(placed);
                true
            }
            None => {
                self.clear();
                false
            }
        }
    }

    /// The placed artifact and its kind, or `None` when nothing is loaded.
    #[cfg(feature = "baked-image")]
    fn ready(&self) -> Option<(u8, &'static [u8])> {
        self.ready.map(|bytes| (self.kind, bytes))
    }

    /// The CRC-32 of the RAM as assembled, which is what a [`load::XFER_RESULT`] on this side
    /// reports -- a deploy reports over the flash as read back instead.
    fn crc32(&self) -> u32 {
        #[cfg(feature = "baked-image")]
        if let Some(placed) = self.ready {
            return crc32(placed);
        }
        crc32(&self.bytes)
    }
}

/// CRC-32 (IEEE, reflected, `0xEDB88320`) over `bytes` -- what a transfer reply reports so a host
/// can check what the target assembled against what it sent.
///
/// Computed rather than tabled: a 1 KiB table is real flash on the parts this serves, and a
/// transfer is already bounded by how fast the bytes arrive.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// One [`load::XFER_RESULT`]: `status(u8)`, `crc32(u32 LE)`.
fn send_xfer_result(
    transport: &mut impl Transport,
    seq: u16,
    status: u8,
    crc: u32,
) -> Result<(), TransportError> {
    let mut payload = [0u8; 5];
    payload[0] = status;
    payload[1..].copy_from_slice(&crc.to_le_bytes());
    transport.send(load::XFER_RESULT, seq, &payload)
}

/// The [`ImageResidence`] that leaks each image, retaining every one it is given.
///
/// Correct for a host, and for a device that serves a bounded number of times. **On a device that
/// serves repeatedly it is a leak per request**, and on a reclaiming heap nothing takes it back --
/// see [`run_image`] for why the arena rewind that appears to cover this does not.
#[cfg(feature = "baked-image")]
pub struct LeakEachImage;

#[cfg(feature = "baked-image")]
impl ImageResidence for LeakEachImage {
    fn admit(&mut self, image: Vec<u8>) -> Option<&'static [u8]> {
        Some(Box::leak(image.into_boxed_slice()))
    }
}

/// [`serve_one_baked_with`] with the image-retention policy named rather than assumed.
///
/// This is the entry point a device that serves repeatedly should use: supply an
/// [`ImageResidence`] that reuses one buffer and the board retains one image instead of all of
/// them. Everything else about the serve is identical.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_baked_with_residence(
    transport: &mut impl Transport,
    configure: &mut dyn FnMut(&mut Vm),
    residence: &mut dyn ImageResidence,
    load: &mut ArtifactLoad,
) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    serve_frame_baked(transport, frame, None, configure, residence, load)?;
    Ok(true)
}

/// Start what a [`load::LOAD_PE`]/[`load::LOAD_IMAGE`]/[`load::LOAD_BUNDLE`] placed in the arena:
/// acknowledge, then run it -- halted under a debugger, or straight through to its
/// [`debug::EVT_STOPPED`].
///
/// # Why the acknowledgement goes first
///
/// ANSWER, THEN ACT. A run can take a very long time or end in a reset, and a target that started
/// first would leave the host unable to tell "it began and is working" from "the frame never
/// arrived" -- which are the same silence.
#[cfg(feature = "baked-image")]
fn exec_loaded(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    load: &ArtifactLoad,
    caps: lamella_wire::Capabilities,
) -> Result<(), TransportError> {
    let source = frame.payload.first().copied().unwrap_or(exec::exec_source::LOADED);
    let flags = frame.payload.get(1).copied().unwrap_or(0);
    if source != exec::exec_source::LOADED {
        return transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::NO_SUCH_SOURCE]);
    }
    let Some((_kind, image)) = load.ready() else {
        return transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::NOTHING_TO_RUN]);
    };
    let halted = flags & exec::exec_flags::START_HALTED != 0;
    if halted && !caps.has(lamella_wire::Capabilities::DEBUG_BASIC) {
        return transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::HALTED_UNSUPPORTED]);
    }
    transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::STARTED])?;
    if halted {
        return run_debug_session_static(transport, image, corlib, frame.seq, caps, configure);
    }
    match run_image_streaming(transport, image, corlib, configure)? {
        PlainRun::Ended(result, reason) => {
            send_stopped(transport, frame.seq, reason, (0, 0), Some(result.exit))
        }
        PlainRun::Aborted => Ok(()),
    }
}

/// How a plain (not halted) run ended.
///
#[cfg(feature = "baked-image")]
enum PlainRun {
    /// The program ended. Its result, and the stop reason that describes how.
    Ended(RunResult, u8),
    /// An [`debug::ABORT`] ended it, and the stop event has already gone out at the ABORT's own
    /// sequence -- so the caller must not send a second one at the request's.
    Aborted,
}

/// Run an artifact to completion on the PLAIN path, streaming its output as it appears and
/// answering a mid-run [`debug::ABORT`].
///
/// The terminal event carries no output, so streaming is the only way a program's output reaches a
/// host -- on this path as much as on the debug one.
#[cfg(feature = "baked-image")]
fn run_image_streaming(
    transport: &mut impl Transport,
    image: &'static [u8],
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
) -> Result<PlainRun, TransportError> {
    let mut sent = OutputCursors::default();
    let mut carrier: Result<(), TransportError> = Ok(());
    let mut aborted = None;
    let (result, fault) = run_image_reporting(image, corlib, configure, &mut |vm| {
        if let Err(error) = stream_output(transport, vm, &mut sent) {
            carrier = Err(error);
            return false;
        }
        match transport.poll() {
            Ok(Some(frame)) if frame.msg_type == debug::ABORT => {
                aborted = Some(frame.seq);
                false
            }
            Ok(_) => true,
            Err(error) => {
                carrier = Err(error);
                false
            }
        }
    });
    carrier?;
    stream_final(transport, &result, &mut sent)?;
    if let Some(text) = &fault {
        send_output(transport, debug::output::STDERR, text)?;
    }
    if let Some(seq) = aborted {
        send_stopped(transport, seq, debug::reason::ABORTED, (0, 0), None)?;
        return Ok(PlainRun::Aborted);
    }
    let reason = if fault.is_some() { debug::reason::TRAP } else { debug::reason::DONE };
    let result = match fault {
        Some(text) => RunResult { exit: result.exit, stdout: result.stdout + &text },
        None => result,
    };
    Ok(PlainRun::Ended(result, reason))
}

/// Flush whatever a finished run produced after its last service point.
///
/// The machine is gone by the time a plain run returns -- `run_image_static` owns and drops it -- so
/// what is left to send is the difference between the merged result text and what already went out.
#[cfg(feature = "baked-image")]
fn stream_final(
    transport: &mut impl Transport,
    result: &RunResult,
    sent: &mut OutputCursors,
) -> Result<(), TransportError> {
    let units: Vec<u16> = result.stdout.encode_utf16().collect();
    stream_units(transport, debug::output::STDOUT, &units, &mut sent.stdout)
}

/// The profile manifest as a chunk answering `PROFILE_GET`'s offset: `offset(u32 LE)`,
/// `total(u32 LE)`, then bytes.
///
/// The manifest is rebuilt per request rather than cached, because it is asked for rarely -- a host
/// asks only when its cache misses the hash the handshake already carried -- and a cache of it would
/// be a second copy of the identity with its own staleness question.
#[cfg(feature = "baked-image")]
fn send_manifest(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
) -> Result<(), TransportError> {
    use lamella_wire::msg::{CHUNK_HEADER_LEN, MAX_CHUNK_DATA};
    let manifest = lamella_wire::ProfileManifest {
        identity: profile_identity(),
        surface: resident_surface_symbols(),
        name: lamella_cil_runtime::intrinsic_registry::profile_name().into(),
        intrinsic_ids: lamella_cil_runtime::intrinsic_registry::registry_ids().collect(),
    }
    .encode();
    let offset = frame
        .payload
        .get(..4)
        .map_or(0usize, |at| u32::from_le_bytes([at[0], at[1], at[2], at[3]]) as usize);
    let end = manifest.len().min(offset.saturating_add(MAX_CHUNK_DATA));
    let body = manifest.get(offset.min(manifest.len())..end).unwrap_or(&[]);
    let mut payload = Vec::with_capacity(CHUNK_HEADER_LEN + body.len());
    payload.extend_from_slice(&(offset as u32).to_le_bytes());
    payload.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    payload.extend_from_slice(body);
    transport.send(profile::PROFILE_MANIFEST, frame.seq, &payload)
}

/// Handle one already-polled frame on a baked-image target: `HELLO` -> `HELLO_ACK`, a chunked
/// `LOAD_x` -> the transfer arena + `XFER_RESULT`, `EXEC` -> run it (halted, with a debug
/// capability). Anything else is REFUSED by name. Split out so a deploy-capable serve loop can
/// dispatch the deploy messages itself and delegate the rest here.
#[cfg(feature = "baked-image")]
fn serve_frame_baked(
    transport: &mut impl Transport,
    frame: lamella_wire::Frame,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    residence: &mut dyn ImageResidence,
    load: &mut ArtifactLoad,
) -> Result<(), TransportError> {
    use lamella_wire::msg;
    note_resident_corlib(corlib);
    match frame.msg_type {
        msg::HELLO => hello_reply(transport, &frame)?,
        load::LOAD_PE | load::LOAD_IMAGE | load::LOAD_BUNDLE => {
            let mut status = load.chunk(frame.msg_type, &frame.payload);
            if status == lamella_wire::msg::xfer::MATCHED && !load.place(residence) {
                status = lamella_wire::msg::xfer::WRITE_FAILED;
            }
            send_xfer_result(transport, frame.seq, status, load.crc32())?;
        }
        load::LOAD_CLEAR => {
            load.clear();
            send_xfer_result(transport, frame.seq, lamella_wire::msg::xfer::MATCHED, 0)?;
        }
        exec::EXEC => {
            exec_loaded(transport, &frame, corlib, configure, load, serve_caps())?;
        }
        exec::EXEC_STATUS => {
            transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::IDLE])?;
        }
        debug::ABORT => {
            transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::IDLE])?;
        }
        profile::PROFILE_GET => {
            send_manifest(transport, &frame)?;
        }
        other => transport.send(
            lamella_wire::msg::ERROR,
            frame.seq,
            &lamella_wire::error::unknown_message_type(other),
        )?,
    }
    Ok(())
}

/// Serve one pending request on a DEPLOY-capable baked-image target: `DEPLOY_IMAGE`
/// programs the image into `flash` and keeps it (boots on reset); `DEPLOY_CLEAR` erases
/// it; every other frame is delegated to [`serve_frame_baked`]. A device firmware's serve
/// loop is this call in a loop (plus its own arena reset between requests).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_deploy(
    transport: &mut impl Transport,
    flash: &mut impl FlashSink,
    load: &mut ArtifactLoad,
) -> Result<Served, TransportError> {
    serve_one_deploy_with(transport, flash, &mut |_vm| {}, None, load)
}

/// [`serve_one_deploy`] with the firmware's [`Vm`]-configure hook (see
/// [`run_image_with`]) -- every evaluation the serve runs gets the board seams installed --
/// and, on a WiFi board, its [`WincFlasher`] so a host can stream a module-firmware update
/// over the wire (`None` answers every `WINC_FW_*` request `ok = 0`).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_deploy_with(
    transport: &mut impl Transport,
    flash: &mut impl FlashSink,
    configure: &mut dyn FnMut(&mut Vm),
    winc: Option<&mut dyn WincFlasher>,
    load: &mut ArtifactLoad,
) -> Result<Served, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(Served::Nothing);
    };
    serve_deploy_frame(transport, frame, flash, configure, winc, load)
}

/// Handle one ALREADY-POLLED frame on a DEPLOY-capable baked-image target: the deploy ops
/// (`DEPLOY_IMAGE`/`CLEAR`/`CHUNK`, the `WINC_FW_*` module-update stream, `DEPLOY_STATUS`/`RUN`,
/// `DBG_ATTACH`, and `HELLO` with the deploy caps), with every other frame delegated to
/// [`serve_frame_baked`]. Split out (mirroring that split) so a combined deploy+session serve
/// loop -- [`serve_one_deploy_repl_with`] -- can poll the wire ONCE and route the frame here or
/// to the session handler, without double-reading the carrier.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
fn serve_deploy_frame(
    transport: &mut impl Transport,
    frame: lamella_wire::Frame,
    flash: &mut impl FlashSink,
    configure: &mut dyn FnMut(&mut Vm),
    winc: Option<&mut dyn WincFlasher>,
    load: &mut ArtifactLoad,
) -> Result<Served, TransportError> {
    use lamella_wire::msg::{CHUNK_HEADER_LEN, xfer};
    note_resident_corlib(flash.resident_corlib());
    match frame.msg_type {
        deploy::DEPLOY_PE | deploy::DEPLOY_IMAGE | deploy::DEPLOY_BUNDLE => {
            let payload = &frame.payload;
            let status = match payload.get(..CHUNK_HEADER_LEN) {
                Some(header) => {
                    let offset =
                        u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
                    let total =
                        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                    if flash.program_chunk(offset, &payload[CHUNK_HEADER_LEN..], total) {
                        xfer::MATCHED
                    } else {
                        xfer::WRITE_FAILED
                    }
                }
                None => xfer::RANGE_REJECTED,
            };
            let crc = crc32(flash.image_slice());
            send_xfer_result(transport, frame.seq, status, crc)?;
        }
        deploy::DEPLOY_CLEAR => {
            flash.erase();
            send_xfer_result(transport, frame.seq, xfer::MATCHED, 0)?;
        }
        deploy::DEPLOY_STATUS => {
            let (state, checksum) =
                match lamella_cil_runtime::verified_image_checksum(flash.image_slice()) {
                    Some(sum) => (deploy::deploy_state::VERIFIED, sum),
                    None => (deploy::deploy_state::NONE, 0u64),
                };
            let tier = if state == deploy::deploy_state::VERIFIED { exec::tier::CIL } else { 0 };
            let mut payload = [0u8; 10];
            payload[0] = state;
            payload[1] = tier;
            payload[2..].copy_from_slice(&checksum.to_le_bytes());
            transport.send(deploy::DEPLOY_STATUS_RESULT, frame.seq, &payload)?;
        }
        lamella_wire::msg::EXTENDED => {
            serve_extended_frame(transport, &frame, winc)?;
        }
        exec::EXEC if exec_source_of(&frame) == exec::exec_source::DEPLOYED => {
            let halted = frame.payload.get(1).copied().unwrap_or(0) & exec::exec_flags::START_HALTED != 0;
            if !halted && lamella_cil_runtime::verified_image_checksum(flash.image_slice()).is_none()
            {
                transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::NOTHING_TO_RUN])?;
                return Ok(Served::Handled);
            }
            transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::STARTED])?;
            if halted {
                run_debug_session_static(
                    transport,
                    flash.image_slice(),
                    flash.resident_corlib(),
                    frame.seq,
                    deploy_caps(),
                    configure,
                )?;
                return Ok(Served::Handled);
            }
            return Ok(Served::RunRequested);
        }
        exec::EXEC => {
            exec_loaded(transport, &frame, flash.resident_corlib(), configure, load, deploy_caps())?;
        }
        lamella_wire::msg::HELLO => hello_reply_caps(transport, &frame, deploy_caps())?,
        live::LIVE_READ | live::LIVE_WRITE => {
            serve_live_frame(transport, &frame, live_window(), &mut TargetMemory)?;
        }
        _ => serve_frame_baked(
            transport,
            frame,
            flash.resident_corlib(),
            configure,
            &mut LeakEachImage,
            load,
        )?,
    }
    Ok(Served::Handled)
}

/// The `source` byte of an [`exec::EXEC`], defaulting to the loaded arena.
///
/// A default rather than a refusal, because `0` IS [`exec::exec_source::LOADED`] and an empty
/// payload therefore says exactly what a zero byte would have.
#[cfg(feature = "baked-image")]
fn exec_source_of(frame: &lamella_wire::Frame) -> u8 {
    frame.payload.first().copied().unwrap_or(exec::exec_source::LOADED)
}

/// Handle one `EXTENDED` frame: `ns(u16 LE)`, `op(u16 LE)`, then the op's own payload.
///
/// A namespace is advertised in the profile manifest, so a host learns which extensions a board
/// understands rather than probing for them; an op number is meaningful only inside its namespace.
#[cfg(feature = "baked-image")]
fn serve_extended_frame(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
    mut winc: Option<&mut dyn WincFlasher>,
) -> Result<(), TransportError> {
    use lamella_wire::msg::ext;
    let Some(header) = frame.payload.get(..4) else {
        let payload = lamella_wire::error::unknown_message_type(lamella_wire::msg::EXTENDED);
        return transport.send(lamella_wire::msg::ERROR, frame.seq, &payload);
    };
    let namespace = u16::from_le_bytes([header[0], header[1]]);
    let op = u16::from_le_bytes([header[2], header[3]]);
    let body = &frame.payload[4..];
    if namespace != ext::NS_MODULE_FIRMWARE || winc.is_none() {
        let payload = lamella_wire::error::unknown_message_type(lamella_wire::msg::EXTENDED);
        return transport.send(lamella_wire::msg::ERROR, frame.seq, &payload);
    }
    let ok = match op {
        ext::MODULE_FW_START => match (body.get(..8), winc.as_deref_mut()) {
            (Some(at), Some(flasher)) => flasher.begin(
                u32::from_le_bytes([at[0], at[1], at[2], at[3]]) as usize,
                u32::from_le_bytes([at[4], at[5], at[6], at[7]]) as usize,
            ),
            _ => false,
        },
        ext::MODULE_FW_CHUNK => match (body.get(..4), winc.as_deref_mut()) {
            (Some(at), Some(flasher)) => flasher.program(
                u32::from_le_bytes([at[0], at[1], at[2], at[3]]) as usize,
                &body[4..],
            ),
            _ => false,
        },
        ext::MODULE_FW_END => winc.as_deref_mut().is_some_and(|flasher| flasher.finish()),
        _ => {
            let payload = lamella_wire::error::unknown_message_type(lamella_wire::msg::EXTENDED);
            return transport.send(lamella_wire::msg::ERROR, frame.seq, &payload);
        }
    };
    let mut reply = Vec::with_capacity(5);
    reply.extend_from_slice(&ext::NS_MODULE_FIRMWARE.to_le_bytes());
    reply.extend_from_slice(&ext::MODULE_FW_RESULT.to_le_bytes());
    reply.push(u8::from(ok));
    transport.send(lamella_wire::msg::EXTENDED, frame.seq, &reply)
}

/// The capability set a serve that carries BOTH the deploy tier AND a live REPL session
/// advertises: the deploy set (incl. `DEBUG_ATTACH`) plus `REPL_RUN`, in one `HELLO_ACK`.
#[cfg(all(feature = "baked-image", feature = "repl-session"))]
fn deploy_repl_caps() -> lamella_wire::Capabilities {
    lamella_wire::Capabilities(deploy_caps().0 | lamella_wire::Capabilities::REPL_RUN)
}

/// Serve one pending frame on a target that is BOTH deploy-capable and session-capable, holding
/// the live REPL `session` across calls. One poll, then route: a session-channel op (`REPL_OPEN`/
/// `REPL_DELTA`/`REPL_CLOSE`/`REPL_PING`) is dispatched against `session` (a persistent interpreter
/// that survives the request), a `HELLO` advertises the deploy tier and `REPL_RUN` in a single ack,
/// and every other frame (`DEPLOY_*`, `WINC_FW_*`, `DBG_ATTACH`, `RUN_IMAGE`, ...) takes the transient
/// deploy path. So one serve loop carries both tiers without double-reading the wire; a board that
/// never receives a `REPL_OPEN` keeps `session` `None` and behaves exactly as [`serve_one_deploy_with`].
/// `configure` installs the board's [`Vm`] seams -- on the session at `REPL_OPEN`, and on each
/// transient evaluation -- the same hook [`run_image_with`] takes.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(all(feature = "baked-image", feature = "repl-session"))]
pub fn serve_one_deploy_repl_with(
    transport: &mut impl Transport,
    flash: &mut impl FlashSink,
    session: &mut Option<ReplSessionState>,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    winc: Option<&mut dyn WincFlasher>,
    load: &mut ArtifactLoad,
) -> Result<Served, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(Served::Nothing);
    };
    note_resident_corlib(corlib);
    match frame.msg_type {
        repl::REPL_OPEN | repl::REPL_DELTA | repl::REPL_CLOSE | repl::REPL_PING => {
            serve_repl_frame(transport, frame, session, corlib, configure, load)?;
            Ok(Served::Handled)
        }
        repl::REPL_RESET => {
            serve_repl_frame(transport, frame, session, corlib, configure, load)?;
            Ok(Served::ResetRequested)
        }
        lamella_wire::msg::HELLO => {
            hello_reply_caps(transport, &frame, deploy_repl_caps())?;
            Ok(Served::Handled)
        }
        _ => serve_deploy_frame(transport, frame, flash, configure, winc, load),
    }
}

/// Run a DEPLOYED baked image (booted from flash) at full speed, staying interruptible: it
/// drives the interpreter session in bounded bursts and polls the wire between them, so a
/// host that sends `HELLO` always takes the board back within a burst -- guaranteed,
/// because an interpreted app cannot execute without the firmware stepping it (there is no
/// "app masks interrupts and spins" failure mode a native deployment has). A trap or a
/// completed run returns [`Deployed::Completed`]; a `HELLO` returns [`Deployed::Interrupted`]
/// (acknowledged), and the firmware then serves the host's next command.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_deployed(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    entry: lamella_cil_runtime::MethodId,
) -> Result<Deployed, TransportError> {
    run_deployed_with(transport, module, entry, &mut |_vm| {})
}

/// [`run_deployed`] with the firmware's [`Vm`]-configure hook (see [`run_image_with`]):
/// the deployed app's [`Vm`] gets the board seams before its static constructors run.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_deployed_with(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    entry: lamella_cil_runtime::MethodId,
    configure: &mut dyn FnMut(&mut Vm),
) -> Result<Deployed, TransportError> {
    use lamella_wire::msg;
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    #[cfg(target_os = "none")]
    vm.set_mmio_subword(
        lamella_mmio::write8,
        lamella_mmio::read8,
        lamella_mmio::write16,
        lamella_mmio::read16,
    );
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    configure(&mut vm);
    let entry = match lamella_cil_runtime::boot_baked(module, &mut vm, entry) {
        Ok(entry) => entry,
        Err(trap) => {
            let mut sent = OutputCursors::default();
            stream_output(transport, &vm, &mut sent)?;
            let result =
                fault(transport, &vm, &format!("BOOT TRAP (static constructor): {trap:?}"))?;
            return Ok(completed(transport, result, debug::reason::TRAP));
        }
    };
    let mut carrier: Result<(), TransportError> = Ok(());
    let mut sent = OutputCursors::default();
    let mut aborted = None;
    let outcome = lamella_cil_runtime::run_interruptible(module, &mut vm, entry, Vec::new(), &mut |vm| {
        if let Err(error) = stream_output(transport, vm, &mut sent) {
            carrier = Err(error);
            return false;
        }
        match transport.poll() {
            Ok(Some(frame)) if frame.msg_type == msg::HELLO => {
                carrier = hello_reply_caps(transport, &frame, deploy_caps());
                false
            }
            Ok(Some(frame)) if frame.msg_type == debug::ABORT => {
                aborted = Some(frame.seq);
                false
            }
            Ok(Some(frame)) if frame.msg_type == exec::EXEC_STATUS => {
                carrier = transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::RUNNING]);
                carrier.is_ok()
            }
            Ok(Some(frame)) if live::is_request(frame.msg_type) => {
                match serve_live_frame(transport, &frame, live_window(), &mut TargetMemory) {
                    Ok(()) => true,
                    Err(error) => {
                        carrier = Err(error);
                        false
                    }
                }
            }
            Ok(_) => true,
            Err(error) => {
                carrier = Err(error);
                false
            }
        }
    });
    carrier?;
    stream_output(transport, &vm, &mut sent)?;
    if let Some(seq) = aborted {
        send_stopped(transport, seq, debug::reason::ABORTED, (0, 0), None)?;
        return Ok(Deployed::Interrupted);
    }
    match outcome {
        Ok(lamella_cil_runtime::Ran::Finished(value)) => {
            Ok(completed(transport, run_result_of(&vm, &value), debug::reason::DONE))
        }
        Ok(lamella_cil_runtime::Ran::Interrupted) => Ok(Deployed::Interrupted),
        Err(trap) => {
            let result = fault(transport, &vm, &format!("TRAP: {trap:?}"))?;
            Ok(completed(transport, result, debug::reason::TRAP))
        }
    }
}

/// Announce a finished deployed run on the wire, and hand the same result back to the firmware.
///
/// # Why the boot path reports at all, and why from HERE
///
/// A deployed run answers no request -- the board RESET into it, so there is no seq to reply to and
/// nothing to return a value to. That is exactly why the result has to be SENT: a host that issued
/// `DEPLOY_RUN` and is listening has no other way to learn the app's exit value or its output, and
/// **discarding it is the difference between a deploy you can verify and one you can only assume.**
/// Sent unsolicited at seq 0; a host that is not listening simply reads a frame it did not ask for,
/// which the framing already tolerates.
///
/// **IT LIVES IN THE RUNNER RATHER THAN IN EACH FIRMWARE, AND THAT IS WHAT MAKES IT RELIABLE.**
/// Written per firmware, the ways to get it wrong all look like working code: printing the app's
/// output to a raw UART as human text is not a wire frame and reaches no host driver, and dropping
/// the result on the floor leaves `DEPLOY_RUN` delivering no `RUN_RESULT` at all -- which a host
/// meets as a 120-second timeout rather than as an error. **A firmware cannot forget a step it does
/// not perform.**
///
/// A carrier fault here is deliberately DROPPED rather than propagated. The run's outcome is the
/// return value, and it already happened; failing to announce it must not turn a completed run into
/// an error, nor lose the exit code the caller is about to act on.
#[cfg(feature = "baked-image")]
fn completed(transport: &mut impl Transport, result: RunResult, reason: u8) -> Deployed {
    let _ = send_stopped(transport, 0, reason, (0, 0), Some(result.exit));
    Deployed::Completed(result)
}


/// The qualified name of the bootstrap's parameterless `<repl>.__Repl` constructor: it anchors the
/// type to instantiate (its declaring type) and runs once to initialize the live instance. The same
/// name the host emit (lamella_assemble's bootstrap) and driver (lamella_repl) use, so a device load
/// binds it identically.
#[cfg(feature = "repl-session")]
const REPL_CTOR_NAME: &str = "<repl>.__Repl..ctor";

/// A live incremental-REPL session held on the device across wire frames: ONE interpreter, ONE heap,
/// and ONE `<repl>.__Repl` instance that GROWS as deltas load, so declared state -- including
/// REFERENCE-typed state (a string / array / object, whose handle stays valid on the one unrebuilt
/// heap) -- survives submission to submission. A device firmware holds an `Option<ReplSessionState>`
/// and threads it into [`serve_one_repl`]; REPL_OPEN fills it, REPL_DELTA runs a submission against
/// it, REPL_CLOSE drops it.
#[cfg(feature = "repl-session")]
pub struct ReplSessionState {
    vm: Vm,
    module: Module,
    context: DeltaContext,
    instance: ObjectRef,
    root_slot: usize,
    heartbeat_ms: u32,
    delta: ArtifactLoad,
    corlib: Option<&'static [u8]>,
}

/// One submission's result, rendered for the wire: how many session variables the delta added (the
/// live instance grew by that many), the submission's displayed value (`""` for a void statement),
/// and the console output THIS submission produced.
#[cfg(feature = "repl-session")]
struct SubmitOutcome {
    new_fields: u16,
    display: String,
    output: String,
}

/// Why a submission did not produce a result: the delta failed to load / grow (`NotLoaded`, wire
/// status 2), the submission trapped while running (`Trapped`, wire status 3), or the session had
/// no room left to attempt it at all (`OutOfMemory`, wire status 4). Distinct so the host can tell
/// a bad delta from a runtime fault from an exhausted session without parsing the reason text.
#[cfg(feature = "repl-session")]
enum SubmitError {
    NotLoaded(String),
    Trapped(String),
    OutOfMemory,
}

#[cfg(feature = "repl-session")]
impl SubmitError {
    fn status(&self) -> u8 {
        match self {
            SubmitError::NotLoaded(_) => 2,
            SubmitError::Trapped(_) => 3,
            SubmitError::OutOfMemory => 4,
        }
    }
    fn reason(&self) -> &str {
        match self {
            SubmitError::NotLoaded(reason) | SubmitError::Trapped(reason) => reason,
            SubmitError::OutOfMemory => OUT_OF_MEMORY_REASON,
        }
    }
}

/// What a host is told when a session runs out of room -- and what it can actually DO about it.
///
/// Reopening the session does NOT reclaim it, which is why this text names a reset instead:
/// dropping a session returns its blocks to per-class free lists, but the bump frontier never
/// retreats and blocks are never split or coalesced across classes, so a reopened session has no
/// frontier and is refused its first submission. Only a target reset reclaims the arena.
#[cfg(feature = "repl-session")]
pub const OUT_OF_MEMORY_REASON: &str = "session out of memory -- the target must be reset to reclaim it";

/// The room a submission is refused for want of, beyond its own delta PE. Sized from the observed
/// per-submission cost -- roughly 2.8 KiB for a trivial submission and 3.7 KiB for one that throws
/// and catches -- with slack, so an ordinary submission clears it and the one that would have
/// aborted mid-load is turned away first.
#[cfg(feature = "repl-session")]
const SUBMISSION_HEADROOM: usize = 8192;

/// Whether the session can still afford a submission carrying a `delta_len`-byte PE.
///
/// A live session never reclaims, so one that runs long enough exhausts its arena. The failure that
/// matters is HOW: Rust's allocation path is infallible, so exhaustion ABORTS inside the loader -- on a
/// device a panic, a reset, and a session that vanishes with no diagnostic. Asking the embedder's
/// probe BEFORE the first allocation converts that into a refusal the session survives.
///
/// Deliberately asks the allocator rather than probing it with a trial allocation: the device heap
/// is a segregated free-list that never SPLITS blocks, so a trial allocation freed back to its size
/// class would be popped again by the next probe and keep succeeding while the classes a real
/// submission needs are exhausted -- a probe that stops predicting exactly when it matters.
///
/// A guard band, not a proof: a submission with an unusually large working set can still exhaust
/// the arena after passing, and the probe under-reports (free-list bytes are already reusable but
/// uncounted), so it errs toward refusing early. Both are the right direction.
#[cfg(feature = "repl-session")]
fn has_submission_headroom(vm: &Vm, delta_len: usize) -> bool {
    vm.heap_headroom()
        .is_none_or(|free| free >= delta_len.saturating_add(SUBMISSION_HEADROOM))
}

/// Whether a session can be OPENED over a `bootstrap_len`-byte PE. Same probe and same band as
/// [`has_submission_headroom`]; named apart because the two guard different moments and the
/// question of whether they should differ has now been settled by measurement.
///
/// They should NOT. Guarding the open on already-reclaimed bytes instead was tried, on the
/// reasoning that an open follows a session DROP and so re-runs the very allocation pattern whose
/// space it is asking about. On a SAME54 it did let the open succeed on a fully-carved arena -- and
/// bought nothing: every subsequent submission was still refused, because the frontier is gone and
/// the free lists are not fungible across size classes. An open that succeeds into a session which
/// can never accept a submission is worse than a clean refusal, since the host has to discover the
/// uselessness by trying.
///
/// **A device that has exhausted its arena cannot be recovered by reopening; it needs a reset.**
/// That is a property of a non-coalescing allocator, not of this guard, and the honest thing is to
/// refuse here and say so rather than to appear to recover. A heap tier that splits or coalesces
/// would change the answer -- it is a documented future rung in `lamella-heap`.
#[cfg(feature = "repl-session")]
fn has_open_headroom(vm: &Vm, bootstrap_len: usize) -> bool {
    has_submission_headroom(vm, bootstrap_len)
}

#[cfg(feature = "repl-session")]
impl ReplSessionState {
    /// Opens a live session over the empty `<repl>.__Repl` in `bootstrap` (the PE the host emits
    /// once): loads it, allocates the single instance, roots it for the collector, runs its `.ctor`,
    /// and installs the board's Vm seams via `configure` -- the SAME machine a RUN_IMAGE gets, so a
    /// submission that drives a peripheral reaches real registers on device.
    ///
    /// # Errors
    /// Returns `Err` if the bootstrap does not parse, declares no `<repl>.__Repl..ctor`, or the
    /// constructor traps.
    fn open(
        bootstrap: &[u8],
        heartbeat_ms: u32,
        corlib: Option<&'static [u8]>,
        configure: &mut dyn FnMut(&mut Vm),
    ) -> Result<ReplSessionState, String> {
        let assembly = Assembly::read(bootstrap)
            .map_err(|error| format!("bootstrap does not parse: {error:?}"))?;

        let mut vm = Vm::default();
        #[cfg(target_os = "none")]
        vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
        #[cfg(target_os = "none")]
        vm.set_mmio_subword(
            lamella_mmio::write8,
            lamella_mmio::read8,
            lamella_mmio::write16,
            lamella_mmio::read16,
        );
        vm.set_memory_backend(Box::new(SafeMemory::new()));
        configure(&mut vm);

        if !has_open_headroom(&vm, bootstrap.len()) {
            return Err(String::from("not enough heap to open a session"));
        }

        let (mut module, name_index, type_index, first_delta_asm) = if corlib.is_some() {
            let (module, name_index, type_index) = load_bootstrap_lazy_corlib(&assembly);
            (module, name_index, type_index, 2)
        } else {
            let (module, name_index, type_index) = load_bootstrap(&assembly);
            (module, name_index, type_index, 1)
        };

        let ctor = find_method(&module, REPL_CTOR_NAME)
            .ok_or_else(|| format!("bootstrap defines no {REPL_CTOR_NAME}"))?;
        let type_id = module
            .method_type(ctor)
            .ok_or_else(|| format!("{REPL_CTOR_NAME} has no declaring type"))?;
        let fields = module
            .type_field_defaults(type_id)
            .ok_or_else(|| String::from("__Repl has no recorded field layout"))?;

        let root_slot = module.reserve_static_slot(Value::Null);
        let storage = module.static_field_defaults().to_vec();

        vm.init_statics(&storage);
        let instance = vm.heap_mut().alloc_instance(type_id, fields);
        vm.set_static_field(root_slot, Value::Object(instance));

        run(&module, &mut vm, ctor, alloc::vec![Value::Object(instance)])
            .map_err(|trap| format!("trap running {REPL_CTOR_NAME}: {trap:?}"))?;
        let instance = current_instance(&vm, root_slot)?;

        Ok(ReplSessionState {
            vm,
            module,
            context: DeltaContext::new_at(type_id, name_index, type_index, first_delta_asm),
            instance,
            root_slot,
            heartbeat_ms,
            delta: ArtifactLoad::new(),
            corlib,
        })
    }

    /// Loads one submission `delta` into the live session, grows the single `__Repl` instance for
    /// any new session variable, runs its `Submit$N` against that instance, and returns the render.
    /// Reuses [`lamella_load::load_delta`] unchanged: the bytes arriving over the wire is the only
    /// difference from the host driver.
    fn submit(&mut self, delta: &[u8]) -> Result<SubmitOutcome, SubmitError> {
        if !has_submission_headroom(&self.vm, delta.len()) {
            return Err(SubmitError::OutOfMemory);
        }
        let assembly = Assembly::read(delta)
            .map_err(|error| SubmitError::NotLoaded(format!("delta does not parse: {error:?}")))?;
        let info = if let Some(corlib_bytes) = self.corlib {
            let corlib = Assembly::read(corlib_bytes).map_err(|error| {
                SubmitError::NotLoaded(format!("corlib does not parse: {error:?}"))
            })?;
            load_delta_with_corlib(&mut self.module, &mut self.context, &assembly, &corlib)
        } else {
            load_delta(&mut self.module, &mut self.context, &assembly)
        }
        .map_err(|error| SubmitError::NotLoaded(format!("delta did not load: {error}")))?;

        self.vm.grow_statics(self.module.static_field_defaults());

        let new_fields = info.new_field_defaults.len() as u16;
        if !info.new_field_defaults.is_empty() {
            self.vm
                .heap_mut()
                .grow_instance(self.instance, &info.new_field_defaults)
                .ok_or_else(|| {
                    SubmitError::NotLoaded(alloc::format!(
                        "live __Repl instance could not be grown: root slot {} holds {:?}, which is {:.80?}",
                        self.root_slot,
                        self.vm.static_field(self.root_slot),
                        self.vm.heap().get(self.instance),
                    ))
                })?;
        }

        let output_before = self.vm.output().len();
        let result = run(
            &self.module,
            &mut self.vm,
            info.submit,
            alloc::vec![Value::Object(self.instance)],
        )
        .map_err(|trap| SubmitError::Trapped(format!("submission trapped: {trap:?}")))?;
        self.instance = current_instance(&self.vm, self.root_slot)
            .map_err(SubmitError::Trapped)?;
        let output = String::from_utf16_lossy(&self.vm.output()[output_before..]);

        Ok(SubmitOutcome {
            new_fields,
            display: self.display(result),
            output,
        })
    }

    /// Renders a submission's return value for display exactly as `Object.ToString` would -- void
    /// (`None`) as `""`, a boxed value / string by its representation -- reusing the runtime's own
    /// `object_to_string`, so a device display matches the host's byte for byte.
    fn display(&mut self, result: Option<Value>) -> String {
        let Some(value) = result else {
            return String::new();
        };
        let rendered = object_to_string(&mut self.vm, &self.module, &[value]);
        if let Ok(instance) = current_instance(&self.vm, self.root_slot) {
            self.instance = instance;
        }
        match rendered {
            Ok(Some(Value::Object(reference))) => self
                .vm
                .heap()
                .as_string(reference)
                .map(|chars| String::from_utf16_lossy(&chars))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The comms-deadman interval this session was opened with (0 = disabled), for a fail-safe
    /// supervisor to arm on. Read-only: nothing in this crate acts on it.
    #[must_use]
    pub fn heartbeat_ms(&self) -> u32 {
        self.heartbeat_ms
    }
}

/// Finds a method by its loader-qualified name (`namespace.type.method`) by scanning the module's
/// bound methods. Used to anchor the bootstrap's `<repl>.__Repl..ctor`; a session runs later
/// submissions by the `MethodId` [`lamella_load::load_delta`] returns, never by name.
#[cfg(feature = "repl-session")]
fn find_method(module: &Module, name: &str) -> Option<MethodId> {
    let mut id: MethodId = 0;
    while module.method_kind(id).is_some() {
        if module.method_name(id) == Some(name) {
            return Some(id);
        }
        id += 1;
    }
    None
}

/// The persistent instance's current handle, read back from its static root `slot` (a collection may
/// have relocated it since it was stored). Errors if the slot no longer holds an object reference --
/// which would mean the GC root was lost, a bug.
#[cfg(feature = "repl-session")]
fn current_instance(vm: &Vm, slot: usize) -> Result<ObjectRef, String> {
    match vm.static_field(slot) {
        Some(Value::Object(reference)) => Ok(reference),
        other => Err(format!("__Repl instance root was lost (slot held {other:?})")),
    }
}

/// The capability set a session-capable serve advertises: [`lamella_wire::Capabilities::REPL_RUN`]
/// (it holds a live session and runs host-compiled deltas), plus the baked-image tier's set when
/// this build also carries it (a REPL device is a resident-corlib device).
#[cfg(feature = "repl-session")]
fn repl_caps() -> lamella_wire::Capabilities {
    #[cfg(feature = "baked-image")]
    {
        lamella_wire::Capabilities(serve_caps().0 | lamella_wire::Capabilities::REPL_RUN)
    }
    #[cfg(not(feature = "baked-image"))]
    {
        lamella_wire::Capabilities(lamella_wire::Capabilities::REPL_RUN)
    }
}

/// Answers a `HELLO` on the session channel, advertising [`repl_caps`] (and the resident-profile
/// identity when this build carries the baked tier). A malformed `HELLO` is dropped; the host
/// retries.
#[cfg(feature = "repl-session")]
fn hello_reply_repl(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
) -> Result<(), TransportError> {
    use lamella_wire::{Hello, PROTOCOL_VERSION, ProtocolRange, msg, target_respond};
    let range = ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION };
    #[cfg(feature = "baked-image")]
    let identity = profile_identity();
    #[cfg(not(feature = "baked-image"))]
    let identity = lamella_wire::TargetIdentity::default();
    match Hello::decode(&frame.payload) {
        Some(hello) => match target_respond(
            &hello,
            range,
            repl_caps(),
            identity,
            transport.max_inbound_payload(),
        ) {
            Ok(ack) => transport.send(msg::HELLO_ACK, frame.seq, &ack.encode()),
            Err(nak) => transport.send(msg::HELLO_NAK, frame.seq, &nak.encode()),
        },
        None => Ok(()),
    }
}

/// Decodes a [`repl::REPL_OPEN`] payload into its `heartbeat_ms`, skipping the RESERVED config blob,
/// which carries nothing today. A short or garbled header yields 0, which is what "no heartbeat"
/// means anyway.
#[cfg(feature = "repl-session")]
fn decode_repl_open(payload: &[u8]) -> u32 {
    payload
        .get(0..4)
        .map_or(0, |head| u32::from_le_bytes([head[0], head[1], head[2], head[3]]))
}

/// The [`repl::REPL_OPENED`] payload for a session that opened: `status 0` then the session id and
/// its resource caps, which are RESERVED and report 0 (= unspecified). The fields are carried from
/// the first frame so the reply shape does not break when real numbers land.
#[cfg(feature = "repl-session")]
fn repl_opened_ok() -> Vec<u8> {
    let mut payload = Vec::with_capacity(13);
    payload.push(0);
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload
}

/// The [`repl::REPL_OPENED`] payload for a session that did NOT open: `status 1` then the reason.
#[cfg(feature = "repl-session")]
fn repl_opened_err(reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + reason.len());
    payload.push(1);
    payload.extend_from_slice(reason.as_bytes());
    payload
}

/// The [`repl::REPL_DELTA_RESULT`] payload for a submission that ran: `status 0`, the new-field
/// count, the display string, then the console output tail.
#[cfg(feature = "repl-session")]
fn repl_delta_result_ok(outcome: &SubmitOutcome) -> Vec<u8> {
    let display = outcome.display.as_bytes();
    let display_len = display.len().min(u16::MAX as usize);
    let mut payload = Vec::with_capacity(5 + display_len + outcome.output.len());
    payload.push(0);
    payload.extend_from_slice(&outcome.new_fields.to_le_bytes());
    payload.extend_from_slice(&(display_len as u16).to_le_bytes());
    payload.extend_from_slice(&display[..display_len]);
    payload.extend_from_slice(outcome.output.as_bytes());
    payload
}

/// The [`repl::REPL_DELTA_RESULT`] payload for a submission that could not run: the `status` (1 no
/// session, 2 delta did not load, 3 trapped, 4 the session is out of memory -- still open, and
/// reclaimable by reopening it), no new fields, no display, and the reason in the output tail.
#[cfg(feature = "repl-session")]
fn repl_delta_result_err(status: u8, reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + reason.len());
    payload.push(status);
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(reason.as_bytes());
    payload
}

/// Dispatches one already-polled session-channel frame against the held `session`. Split from
/// [`serve_one_repl`] so a firmware that multiplexes channels itself can route a frame here after
/// its own poll.
#[cfg(feature = "repl-session")]
fn serve_repl_frame(
    transport: &mut impl Transport,
    frame: lamella_wire::Frame,
    session: &mut Option<ReplSessionState>,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    load: &mut ArtifactLoad,
) -> Result<(), TransportError> {
    match frame.msg_type {
        repl::REPL_OPEN => {
            let heartbeat_ms = decode_repl_open(&frame.payload);
            let Some((_kind, bootstrap)) = load.held() else {
                let payload = repl_opened_err(
                    "no artifact is loaded: send the session bootstrap with LOAD_PE first",
                );
                transport.send(repl::REPL_OPENED, frame.seq, &payload)?;
                return Ok(());
            };
            *session = None;
            match ReplSessionState::open(bootstrap, heartbeat_ms, corlib, configure) {
                Ok(state) => {
                    *session = Some(state);
                    transport.send(repl::REPL_OPENED, frame.seq, &repl_opened_ok())?;
                }
                Err(reason) => {
                    *session = None;
                    transport.send(repl::REPL_OPENED, frame.seq, &repl_opened_err(&reason))?;
                }
            }
        }
        repl::REPL_DELTA => {
            let Some(state) = session.as_mut() else {
                let payload = repl_delta_result_err(1, "no open REPL session");
                transport.send(repl::REPL_DELTA_RESULT, frame.seq, &payload)?;
                return Ok(());
            };
            let status = state.delta.chunk(frame.msg_type, &frame.payload);
            if status != lamella_wire::msg::xfer::MATCHED {
                let payload = repl_delta_result_err(2, "the delta transfer was refused");
                transport.send(repl::REPL_DELTA_RESULT, frame.seq, &payload)?;
                return Ok(());
            }
            let Some((_kind, delta)) = state.delta.held() else {
                send_xfer_result(transport, frame.seq, status, state.delta.crc32())?;
                return Ok(());
            };
            let delta = delta.to_vec();
            state.delta.clear();
            let payload = match state.submit(&delta) {
                Ok(outcome) => repl_delta_result_ok(&outcome),
                Err(error) => repl_delta_result_err(error.status(), error.reason()),
            };
            transport.send(repl::REPL_DELTA_RESULT, frame.seq, &payload)?;
        }
        repl::REPL_CLOSE => {
            *session = None;
            transport.send(repl::REPL_CLOSED, frame.seq, &[1])?;
        }
        repl::REPL_PING => {
        }
        repl::REPL_RESET => {
            *session = None;
            transport.send(repl::REPL_RESETTING, frame.seq, &[1])?;
        }
        lamella_wire::msg::HELLO => hello_reply_repl(transport, &frame)?,
        #[cfg(not(feature = "baked-image"))]
        load::LOAD_PE => {
            let status = load.chunk(frame.msg_type, &frame.payload);
            send_xfer_result(transport, frame.seq, status, load.crc32())?;
        }
        #[cfg(not(feature = "baked-image"))]
        load::LOAD_CLEAR => {
            load.clear();
            send_xfer_result(transport, frame.seq, lamella_wire::msg::xfer::MATCHED, 0)?;
        }
        #[cfg(feature = "baked-image")]
        _ => serve_frame_baked(transport, frame, corlib, configure, &mut LeakEachImage, load)?,
        #[cfg(not(feature = "baked-image"))]
        other => transport.send(
            lamella_wire::msg::ERROR,
            frame.seq,
            &lamella_wire::error::unknown_message_type(other),
        )?,
    }
    Ok(())
}

/// Serve one pending frame on a SESSION-CAPABLE target, holding the live REPL `session` across
/// calls: `REPL_OPEN` (re)opens it, `REPL_DELTA` runs a submission against it, `REPL_CLOSE` tears it
/// down, a `HELLO` advertises `REPL_RUN`, a `REPL_PING` refreshes contact. Returns whether a frame
/// was handled. A device firmware's session loop is this call in a loop over an
/// `Option<ReplSessionState>` it owns; a board that never receives a `REPL_OPEN` keeps it `None` and
/// behaves exactly as the stateless serve loop. `configure` installs the board's Vm seams on the
/// session at open (the same hook [`run_image_with`] takes).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "repl-session")]
pub fn serve_one_repl(
    transport: &mut impl Transport,
    session: &mut Option<ReplSessionState>,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    load: &mut ArtifactLoad,
) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    serve_repl_frame(transport, frame, session, corlib, configure, load)?;
    Ok(true)
}

/// The runner's request handler: a chunked [`load::LOAD_PE`] places a program, an [`exec::EXEC`]
/// runs it (against `corlib_bytes`) and answers [`debug::EVT_STOPPED`] with its output already
/// streamed as [`debug::EVT_OUTPUT`]. Returns whether a request was handled.
///
/// `load` is the transfer arena: a transfer is chunked, so it has to survive between frames.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn serve_one(
    transport: &mut impl Transport,
    corlib_bytes: &[u8],
    load: &mut ArtifactLoad,
) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    match frame.msg_type {
        load::LOAD_PE | load::LOAD_IMAGE => {
            let status = load.chunk(frame.msg_type, &frame.payload);
            send_xfer_result(transport, frame.seq, status, load.crc32())?;
        }
        load::LOAD_CLEAR => {
            load.clear();
            send_xfer_result(transport, frame.seq, lamella_wire::msg::xfer::MATCHED, 0)?;
        }
        exec::EXEC => {
            let Some((kind, artifact)) = load.held() else {
                transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::NOTHING_TO_RUN])?;
                return Ok(true);
            };
            if frame.payload.get(1).copied().unwrap_or(0) & exec::exec_flags::START_HALTED != 0 {
                transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::HALTED_UNSUPPORTED])?;
                return Ok(true);
            }
            #[cfg(not(feature = "baked-image"))]
            if kind == load::LOAD_IMAGE {
                transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::NO_SUCH_SOURCE])?;
                return Ok(true);
            }
            transport.send(exec::EXEC_ACK, frame.seq, &[exec::ack::STARTED])?;
            #[cfg(feature = "baked-image")]
            let result = if kind == load::LOAD_IMAGE {
                run_image(artifact.to_vec())
            } else {
                run_program(corlib_bytes, artifact)
            };
            #[cfg(not(feature = "baked-image"))]
            let result = run_program(corlib_bytes, artifact);
            send_output(transport, debug::output::STDOUT, &result.stdout)?;
            let why = if result.exit == 70 { debug::reason::TRAP } else { debug::reason::DONE };
            send_stopped_result(transport, frame.seq, why, result.exit)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A terminal [`debug::EVT_STOPPED`] with a result tail, available without the `baked-image` tier.
///
/// The gated [`send_stopped`] carries the debug session's stop SITE as well; this is the same
/// message from a path that has no session to have a site in, so the location is zero -- which is
/// what a `DONE` reports there too.
fn send_stopped_result(
    transport: &mut impl Transport,
    seq: u16,
    why: u8,
    exit: i32,
) -> Result<(), TransportError> {
    let mut payload = Vec::with_capacity(14);
    payload.push(why);
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&exit.to_le_bytes());
    payload.push(0);
    transport.send(debug::EVT_STOPPED, seq, &payload)
}

/// Host driver: send a compiled `program` to the target and start it -- a chunked
/// [`load::LOAD_PE`] followed by an [`exec::EXEC`].
///
/// Every chunk and the `EXEC` carry `seq`, so one round trip's worth of frames is attributable to
/// one request. The reply to each chunk is a [`load::XFER_RESULT`]; a [`RunCollector`] reads them
/// along with the output and the terminal stop.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_program(transport: &mut impl Transport, seq: u16, program: &[u8]) -> Result<(), TransportError> {
    send_artifact(transport, seq, load::LOAD_PE, program)?;
    transport.send(exec::EXEC, seq, &[exec::exec_source::LOADED, 0])
}

/// Host driver: send a baked `.lmli` `image` to the target and start it. Unconditional (no
/// interpreter feature is needed to DRIVE a device).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_image(transport: &mut impl Transport, seq: u16, image: &[u8]) -> Result<(), TransportError> {
    send_artifact(transport, seq, load::LOAD_IMAGE, image)?;
    transport.send(exec::EXEC, seq, &[exec::exec_source::LOADED, 0])
}

/// One artifact as chunks of `offset(u32 LE)`, `total(u32 LE)`, bytes.
///
/// An EMPTY artifact still sends one chunk, because the completion rule is `offset + len == total`
/// and a transfer that sent nothing would leave the arena holding the artifact before last. A
/// single-frame artifact is the degenerate one-chunk case rather than a second code path.
fn send_artifact(
    transport: &mut impl Transport,
    seq: u16,
    kind: u8,
    artifact: &[u8],
) -> Result<(), TransportError> {
    use lamella_wire::msg::{CHUNK_HEADER_LEN, MAX_CHUNK_DATA};
    let total = artifact.len();
    let mut offset = 0usize;
    loop {
        let end = total.min(offset + MAX_CHUNK_DATA);
        let body = &artifact[offset..end];
        let mut payload = Vec::with_capacity(CHUNK_HEADER_LEN + body.len());
        payload.extend_from_slice(&(offset as u32).to_le_bytes());
        payload.extend_from_slice(&(total as u32).to_le_bytes());
        payload.extend_from_slice(body);
        transport.send(kind, seq, &payload)?;
        offset = end;
        if offset >= total {
            return Ok(());
        }
    }
}

/// Host driver: ask the target to RESET back into serve mode -- the only in-band way to reclaim an
/// exhausted interpreter arena.
///
/// The target answers [`repl::REPL_RESETTING`] and then reboots, so expect the link to drop
/// immediately: a caller should re-`HELLO` before opening a new session, and should treat a missing
/// reply as "reset anyway, probably" rather than an error, since the reply races the reboot.
///
/// Reach for this when a session reports [`repl::REPL_DELTA_RESULT`] status 4 (out of memory).
/// Reopening the session does NOT reclaim on the constrained serve's allocator -- see
/// [`repl::REPL_RESET`] for the measurement behind that.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_repl_reset(transport: &mut impl Transport, seq: u16) -> Result<(), TransportError> {
    transport.send(repl::REPL_RESET, seq, &[])
}

/// Host driver: assembles ONE execution's answer out of the frames that carry it -- the output
/// streamed as [`debug::EVT_OUTPUT`] while it runs, and the [`debug::EVT_STOPPED`] that ends it.
///
/// # Why a collector and not a function
///
/// **A run's output no longer arrives with its result.** It is streamed as it happens, so anything a
/// stateless poll did not happen to be looking at when a frame landed is gone. That is the trap this
/// type exists to make impossible: a consumer that kept reading the terminal frame would not FAIL
/// when the tail went away -- it would go quiet, and a program that printed nothing and a host that
/// discarded what it printed are the same observation.
///
/// One collector per execution. It holds the streams apart, because the wire does; [`RunCollector`]
/// merges them only at [`RunCollector::finish`], for callers whose one field is a terminal column.
#[derive(Clone, Debug, Default)]
pub struct RunCollector {
    /// The sequence number of the execution this collector is following.
    seq: u16,
    /// What the program wrote to standard output.
    pub stdout: String,
    /// What the program, or the runner reporting on it, wrote to standard error.
    pub stderr: String,
    /// What the program wrote to the debug channel (`Debug`/`Trace`), kept apart so a client can
    /// show it in its own pane.
    pub debug: String,
    /// The exit code from the terminal stop, once it has arrived.
    exit: Option<i32>,
    /// Whether the execution ended -- with or without an exit code, since an ABORT carries none.
    ended: bool,
}

impl RunCollector {
    /// A collector following the execution tagged `seq`.
    #[must_use]
    pub fn new(seq: u16) -> Self {
        Self { seq, ..Self::default() }
    }

    /// Fold every pending frame into this collector, and report whether the execution has ENDED.
    ///
    /// Output is folded wherever it lands, not only where output is expected: it is unsolicited, it
    /// arrives during a request that has not been answered yet, and a loop that drops what it is not
    /// waiting for drops all of it.
    ///
    /// # Errors
    /// Propagates a [`TransportError`] from the carrier; [`TransportError::Refused`] when the target
    /// answered [`lamella_wire::msg::ERROR`] for this sequence.
    pub fn poll(&mut self, transport: &mut impl Transport) -> Result<bool, TransportError> {
        while let Some(frame) = transport.poll()? {
            match frame.msg_type {
                debug::EVT_OUTPUT => self.take_output(&frame.payload),
                debug::EVT_STOPPED if frame.seq == self.seq => {
                    self.ended = true;
                    self.exit = stop_exit(&frame.payload).map(|(exit, _flags)| exit);
                }
                lamella_wire::msg::ERROR if frame.seq == self.seq => {
                    return Err(TransportError::Refused {
                        reason: frame.payload.first().copied().unwrap_or(0),
                        msg_type: lamella_wire::error::refused_message_type(&frame.payload)
                            .unwrap_or(0),
                    });
                }
                exec::EXEC_ACK if frame.seq == self.seq => {
                    match frame.payload.first().copied() {
                        Some(exec::ack::STARTED | exec::ack::RUNNING) => {}
                        _ => self.ended = true,
                    }
                }
                _ => {}
            }
        }
        Ok(self.ended)
    }

    /// One [`debug::EVT_OUTPUT`] payload -- `stream(u8)`, `flags(u8)`, bytes -- into the stream it
    /// names.
    fn take_output(&mut self, payload: &[u8]) {
        let Some((&stream, rest)) = payload.split_first() else { return };
        let Some((_flags, bytes)) = rest.split_first() else { return };
        let text = String::from_utf8_lossy(bytes);
        match stream {
            debug::output::STDERR => self.stderr.push_str(&text),
            debug::output::DEBUG => self.debug.push_str(&text),
            _ => self.stdout.push_str(&text),
        }
    }

    /// The assembled result, or `None` when the execution has not ended.
    ///
    /// `stdout` and `stderr` are merged in that order for the one field [`RunResult`] carries; a
    /// caller that wants them apart reads the fields above.
    #[must_use]
    pub fn finish(self) -> Option<RunResult> {
        if !self.ended {
            return None;
        }
        let exit = self.exit.unwrap_or(-2);
        Some(RunResult { exit, stdout: self.stdout + &self.stderr })
    }
}

/// The session's out-of-room guard, which decides whether a submission is attempted at all. Its
/// whole purpose is that exhaustion arrives as a REFUSAL rather than an abort, so the boundary is
/// pinned in both directions -- including the un-probed host, which must never refuse.
#[cfg(all(test, feature = "repl-session"))]
mod headroom_tests {
    use super::{SUBMISSION_HEADROOM, Vm, has_submission_headroom};

    fn nearly_empty() -> usize {
        512
    }

    fn exactly_enough() -> usize {
        SUBMISSION_HEADROOM + 1024
    }

    #[test]
    fn a_target_with_no_probe_never_refuses() {
        let vm = Vm::default();
        assert!(has_submission_headroom(&vm, 1024));
        assert!(has_submission_headroom(&vm, 64 * 1024));
    }

    #[test]
    fn a_session_out_of_room_refuses_before_allocating() {
        let mut vm = Vm::default();
        vm.set_heap_headroom(nearly_empty);
        assert!(!has_submission_headroom(&vm, 1024));
    }

    #[test]
    fn a_submission_that_fits_within_the_guard_band_is_attempted() {
        let mut vm = Vm::default();
        vm.set_heap_headroom(exactly_enough);
        assert!(has_submission_headroom(&vm, 1024));
        assert!(!has_submission_headroom(&vm, 1025));
    }

    #[test]
    fn a_pathological_delta_length_cannot_wrap_the_check_into_admitting_it() {
        let mut vm = Vm::default();
        vm.set_heap_headroom(exactly_enough);
        assert!(!has_submission_headroom(&vm, usize::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The LIVE agent's refusals, the shape of them, and the one that is a bounds check.
    ///
    /// Every case here is stated against the RULE functions rather than the statics: the window is
    /// process-wide, and a test that wrote it would decide the outcome of every other test in the
    /// process depending on which ran first.
    #[cfg(feature = "baked-image")]
    mod live_agent {
        use super::super::{
            LiveMemory, checked_window, deploy_caps_with, live, live_span_ok, serve_live_frame,
        };
        use alloc::vec;
        use alloc::vec::Vec;
        use lamella_wire::{Capabilities, Frame, MemTransport, Transport};

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

        #[test]
        fn the_capability_bit_follows_the_window_not_the_code() {
            let base = Capabilities(Capabilities::BAKED_IMAGE);
            assert!(!deploy_caps_with(base, 0).has(Capabilities::LIVE_MEMORY));
            assert!(deploy_caps_with(base, 1024).has(Capabilities::LIVE_MEMORY));
            assert!(deploy_caps_with(base, 0).has(Capabilities::DEBUG_BOOT_DEPLOYED));
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
            assert!(!live::is_request(super::super::debug::DBG_PAUSE));
        }
    }

    /// Serve every frame the runner has pending on the baked-only loop, and report how many.
    ///
    /// An artifact now crosses as one or more LOAD chunks and then an EXEC, so a test that served
    /// exactly once would answer the first chunk and start nothing -- and its assertions would then
    /// be about a target that never ran anything.
    #[cfg(feature = "baked-image")]
    fn drain_baked(runner: &mut lamella_wire::MemTransport, arena: &mut ArtifactLoad) -> usize {
        let mut served = 0;
        while serve_one_baked(runner, arena).unwrap() {
            served += 1;
        }
        served
    }

    /// Load an artifact into the target's arena and start it, halted or running.
    #[cfg(feature = "baked-image")]
    fn load_then_exec(
        driver: &mut lamella_wire::MemTransport,
        seq: u16,
        kind: u8,
        artifact: &[u8],
        flags: u8,
    ) {
        send_artifact(driver, seq, kind, artifact).unwrap();
        driver.send(exec::EXEC, seq, &[exec::exec_source::LOADED, flags]).unwrap();
    }

    /// The next frame that is not a transfer acknowledgement or a start acknowledgement.
    ///
    /// A run is no longer one request and one reply: the artifact crosses as chunks, each answered,
    /// and the start is answered too. A test reading the FIRST frame back would read a transfer
    /// result and report it as a missing stop event, which says nothing about what it was checking.
    #[cfg(feature = "baked-image")]
    fn next_event(driver: &mut lamella_wire::MemTransport) -> Option<lamella_wire::Frame> {
        while let Some(frame) = driver.poll().unwrap() {
            if frame.msg_type != load::XFER_RESULT
                && frame.msg_type != exec::EXEC_ACK
                && frame.msg_type != debug::EVT_OUTPUT
            {
                return Some(frame);
            }
        }
        None
    }

    /// What the REAL corlib declares about itself, read the way a firmware reads it.
    ///
    /// The source-level gates above prove the declarations are WRITTEN. This proves they SURVIVE the
    /// compile and decode to the values a board puts on the wire -- which is a different claim, and
    /// the one that fails if an emitter drops an assembly-level attribute or a blob decoder reads a
    /// string the wrong way.
    ///
    #[cfg(feature = "baked-image")]
    #[test]
    fn the_built_corlib_declares_a_generation_a_build_and_its_surface() {
        let Ok(corlib) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-load/tests/fixtures/corlib.dll"
        )) else {
            return;
        };
        let declared = ResidentLibrary::of(&corlib);

        let era = declared.surface & lamella_wire::surface::NETFX_MASK;
        assert_ne!(era, 0, "the fixture corlib declares at least one era");
        let expected_generation = if era & lamella_wire::surface::NETFX_4_5 != 0 {
            (4, 5)
        } else if era & lamella_wire::surface::NETFX_4_0 != 0 {
            (4, 0)
        } else if era & lamella_wire::surface::NETFX_2_0 != 0 {
            (2, 0)
        } else {
            (1, 1)
        };
        assert_eq!(
            (declared.lib_version[0], declared.lib_version[1]),
            expected_generation,
            "the assembly version must state the generation its era bits describe"
        );
        assert_eq!(
            (declared.lib_file_version[0], declared.lib_file_version[1]),
            expected_generation,
            "the file version leads with the same generation as the assembly version"
        );
        assert_ne!(declared.lib_file_version[2], 0, "the file version carries a day count");
        assert_ne!(
            declared.lib_file_version, declared.lib_version,
            "two fields, never one spliced: a file version that equalled the assembly version would \
             be describing nothing the assembly version does not already say"
        );
        for expected in [
            lamella_wire::surface::FLOAT,
            lamella_wire::surface::GENERICS,
            lamella_wire::surface::REFLECTION,
        ] {
            assert_ne!(declared.surface & expected, 0, "the full profile declares its own symbols");
        }

        let claimed = lamella_wire::Surface {
            tier: lamella_wire::msg::tier::CIL,
            ..lamella_wire::Surface::default()
        };
        assert_eq!(claimed.unreadable_version(true), Some("lib_version"));
        assert_eq!(claimed.unreadable_version(false), None);
        let real = lamella_wire::Surface {
            tier: lamella_wire::msg::tier::CIL,
            lib_version: declared.lib_version,
            lib_file_version: declared.lib_file_version,
            ..lamella_wire::Surface::default()
        };
        assert_eq!(real.unreadable_version(true), None);
        let half = lamella_wire::Surface { lib_file_version: [0; 4], ..real };
        assert_eq!(half.unreadable_version(true), Some("lib_file_version"));
        let python = lamella_wire::Surface {
            tier: lamella_wire::msg::tier::PYTHON,
            ..lamella_wire::Surface::default()
        };
        assert_eq!(python.unreadable_version(true), None);
    }

    /// The result tail of a terminal stop, and the three reasons that do NOT carry one.
    ///
    /// The pair matters because `None` here means two different things a caller must not merge: a
    /// stop with no result (a breakpoint, a pause) and a stop whose result was truncated. Both leave
    /// the exit code unknown, and the first is normal while the second is a broken target.
    #[test]
    fn a_stop_carries_an_exit_code_only_where_one_exists() {
        let mut done = alloc::vec![debug::reason::DONE];
        done.extend_from_slice(&0u32.to_le_bytes());
        done.extend_from_slice(&0u32.to_le_bytes());
        done.extend_from_slice(&7i32.to_le_bytes());
        done.push(0);
        assert_eq!(stop_exit(&done), Some((7, 0)));

        let mut at_break = alloc::vec![debug::reason::BREAKPOINT];
        at_break.extend_from_slice(&11u32.to_le_bytes());
        at_break.extend_from_slice(&22u32.to_le_bytes());
        assert_eq!(stop_exit(&at_break), None);

        assert_eq!(stop_exit(&[debug::reason::ABORTED, 0, 0, 0, 0, 0, 0, 0, 0]), None);

        assert_eq!(stop_exit(&[debug::reason::DONE, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2]), None);
        assert_eq!(stop_exit(&[]), None);
    }

    /// A collector assembles ONE execution out of the frames that carry it, and keeps the streams
    /// apart on the way.
    ///
    /// The row that matters is the LAST one: output arrives before the stop, so a consumer that only
    /// looked when it was waiting for a result would see none of it -- and would not fail, it would
    /// go quiet.
    #[test]
    fn a_collector_folds_streamed_output_into_the_run_it_belongs_to() {
        use lamella_wire::MemTransport;

        let mut target = MemTransport::new();
        let mut driver = MemTransport::new();
        send_output(&mut target, debug::output::STDOUT, "hi\n").unwrap();
        send_output(&mut target, debug::output::DEBUG, "trace\n").unwrap();
        send_output(&mut target, debug::output::STDERR, "TRAP: x").unwrap();
        driver.feed(&target.take_sent());

        let mut run = RunCollector::new(9);
        assert!(!run.poll(&mut driver).unwrap(), "output alone does not end an execution");
        assert_eq!(run.stdout, "hi\n");
        assert_eq!(run.debug, "trace\n", "the debug channel is NOT standard output");
        assert_eq!(run.stderr, "TRAP: x");
        assert!(run.clone().finish().is_none(), "an unfinished run has no result");

        send_stopped_result(&mut target, 9, debug::reason::TRAP, 70).unwrap();
        driver.feed(&target.take_sent());
        assert!(run.poll(&mut driver).unwrap(), "the stop ends it");
        let result = run.finish().expect("a finished run has one");
        assert_eq!(result.exit, 70);
        assert_eq!(result.stdout, "hi\nTRAP: x");
    }

    /// THREE states a driver must NOT collapse into "keep waiting", each fed as the frame a target
    /// really sends. Only the first may leave the run unfinished; collapsing the other two polls to
    /// the caller's deadline and reports a timeout, pointing the reader at a link that is fine.
    #[test]
    fn a_collector_separates_nothing_yet_from_a_refusal_and_from_a_refused_start() {
        use lamella_wire::{MemTransport, error, msg};

        let mut quiet = MemTransport::new();
        let mut run = RunCollector::new(4);
        assert!(!run.poll(&mut quiet).unwrap());

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        target.send(msg::ERROR, 4, &error::unknown_message_type(exec::EXEC)).unwrap();
        driver.feed(&target.take_sent());
        assert_eq!(
            RunCollector::new(4).poll(&mut driver),
            Err(TransportError::Refused {
                reason: error::UNKNOWN_MESSAGE_TYPE,
                msg_type: exec::EXEC,
            })
        );

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        target.send(exec::EXEC_ACK, 4, &[exec::ack::NOTHING_TO_RUN]).unwrap();
        driver.feed(&target.take_sent());
        let mut run = RunCollector::new(4);
        assert!(run.poll(&mut driver).unwrap(), "a refused start ends the run");
        assert_eq!(run.finish().expect("a result").exit, -2, "and reports no exit value");

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        target.send(msg::ERROR, 5, &error::unknown_message_type(exec::EXEC)).unwrap();
        driver.feed(&target.take_sent());
        assert!(!RunCollector::new(4).poll(&mut driver).unwrap());
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn an_artifact_loads_then_execs_over_the_wire() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        let mut arena = ArtifactLoad::new();
        send_image(&mut driver, 9, &image).unwrap();
        runner.feed(&driver.take_sent());
        let mut served = 0;
        while serve_one(&mut runner, &[], &mut arena).unwrap() {
            served += 1;
        }
        assert!(served >= 2, "at least one chunk and the exec");
        driver.feed(&runner.take_sent());

        let mut run = RunCollector::new(9);
        assert!(run.poll(&mut driver).unwrap(), "the execution ended");
        let result = run.finish().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    /// A chunked transfer refuses what it cannot assemble, rather than assembling something nobody
    /// sent.
    ///
    /// Each row is a way an artifact could arrive WRONG and still look complete: a gap, a kind that
    /// changed mid-transfer, and a chunk that overruns the declared total. The last row is the
    /// control -- the same bytes in order do land.
    #[test]
    fn a_transfer_refuses_a_gap_a_switched_kind_and_an_overrun() {
        use lamella_wire::msg::xfer;

        fn chunk(offset: u32, total: u32, body: &[u8]) -> Vec<u8> {
            let mut payload = offset.to_le_bytes().to_vec();
            payload.extend_from_slice(&total.to_le_bytes());
            payload.extend_from_slice(body);
            payload
        }

        let mut arena = ArtifactLoad::new();
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(0, 6, b"ab")), xfer::MATCHED);
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(4, 6, b"ef")), xfer::RANGE_REJECTED);
        assert!(arena.held().is_none(), "a refused chunk must not complete the transfer");

        let mut arena = ArtifactLoad::new();
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(0, 4, b"ab")), xfer::MATCHED);
        assert_eq!(arena.chunk(load::LOAD_IMAGE, &chunk(2, 4, b"cd")), xfer::RANGE_REJECTED);

        let mut arena = ArtifactLoad::new();
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(0, 2, b"abcd")), xfer::RANGE_REJECTED);

        let mut arena = ArtifactLoad::new();
        assert_eq!(arena.chunk(load::LOAD_PE, &[0, 0, 0]), xfer::RANGE_REJECTED);

        let mut arena = ArtifactLoad::new();
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(0, 9, b"junk")), xfer::MATCHED);
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(0, 4, b"ab")), xfer::MATCHED);
        assert_eq!(arena.chunk(load::LOAD_PE, &chunk(2, 4, b"cd")), xfer::MATCHED);
        assert_eq!(arena.held(), Some((load::LOAD_PE, &b"abcd"[..])));
        arena.clear();
        assert!(arena.held().is_none(), "a clear reclaims the completed artifact too");
    }

    /// Output streams as a DELTA, never re-sent, never split mid-character, and on the stream it
    /// was written to.
    ///
    /// The four ways this can be quietly wrong are a missed delta (output that never arrives), a
    /// re-sent one (the console repeats itself), a chunk cut through a surrogate pair (the host
    /// decodes a replacement character for text the program did write), and the debug channel
    /// arriving as standard output (which is what a device debugger saw for as long as the wire
    /// carried one stream). Each gets a row.
    #[cfg(feature = "baked-image")]
    #[test]
    fn output_streams_as_a_delta_on_its_own_stream_and_never_splits_a_surrogate_pair() {
        use lamella_wire::{MemTransport, Transport};

        fn taken(host: &mut MemTransport) -> Vec<(u8, u8, String)> {
            let mut peer = MemTransport::new();
            peer.feed(&host.take_sent());
            let mut frames = Vec::new();
            while let Some(frame) = peer.poll().unwrap() {
                assert_eq!(frame.msg_type, debug::EVT_OUTPUT);
                frames.push((
                    frame.payload[0],
                    frame.payload[1],
                    String::from_utf8_lossy(&frame.payload[2..]).into_owned(),
                ));
            }
            frames
        }

        let mut vm = Vm::new();
        let mut host = MemTransport::new();
        let mut sent = OutputCursors::default();

        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert!(taken(&mut host).is_empty(), "an empty delta must send nothing at all");

        vm.write(&"first\n".encode_utf16().collect::<Vec<u16>>());
        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert_eq!(
            taken(&mut host),
            alloc::vec![(
                debug::output::STDOUT,
                debug::output::ENDS_ON_LINE_BOUNDARY,
                "first\n".to_string()
            )]
        );

        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert!(taken(&mut host).is_empty(), "an unchanged buffer must not be re-sent");

        vm.debug_write(&"trace".encode_utf16().collect::<Vec<u16>>());
        vm.write(&"second".encode_utf16().collect::<Vec<u16>>());
        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert_eq!(
            taken(&mut host),
            alloc::vec![
                (debug::output::STDOUT, 0, "second".to_string()),
                (debug::output::DEBUG, 0, "trace".to_string()),
            ],
            "two streams, two frames, and neither flagged as ending a line"
        );

        vm.write(&[0xD83D]);
        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert!(taken(&mut host).is_empty(), "a trailing lead surrogate must wait for its trail");

        vm.write(&[0xDE00]);
        stream_output(&mut host, &vm, &mut sent).unwrap();
        let frames = taken(&mut host);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].2.as_bytes(), [0xF0, 0x9F, 0x98, 0x80], "the pair arrives as one character");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn serve_one_baked_negotiates_then_runs_an_image() {
        use lamella_wire::{Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg};

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();

        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE | Capabilities::REPL_RUN),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 1, "the target handled the HELLO");
        driver.feed(&runner.take_sent());
        let ack_frame = next_event(&mut driver).expect("a HELLO_ACK arrived");
        assert_eq!(ack_frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack_frame.payload).expect("decodes");
        assert_eq!(ack.chosen, PROTOCOL_VERSION);
        assert!(ack.caps.has(Capabilities::BAKED_IMAGE));

        send_image(&mut driver, 2, &image).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 2, "the target loaded then ran the image");
        driver.feed(&runner.take_sent());
        let mut run = RunCollector::new(2);
        assert!(run.poll(&mut driver).unwrap(), "the execution ended");
        let result = run.finish().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    /// **A message type no target implements is REFUSED, not dropped.** The refusal names the type, so
    /// a host learns which thing is missing rather than that something is.
    ///
    /// This is the property a host's feature detection assumes. The alternative -- dropping the
    /// frame -- makes the host wait out a timeout it cannot tell from a board that has stopped
    /// answering.
    #[cfg(feature = "baked-image")]
    #[test]
    fn an_unimplemented_message_type_is_refused_and_the_refusal_names_it() {
        use lamella_wire::{MemTransport, error, msg};

        let asked = load::LOAD_JS;
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        driver.send(asked, 11, b"a bundle this target cannot load").unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 1, "the target handled the frame");
        driver.feed(&runner.take_sent());

        let reply = next_event(&mut driver).expect("the target answered rather than going quiet");
        assert_eq!(reply.msg_type, msg::ERROR);
        assert_eq!(reply.seq, 11, "the refusal answers the frame that caused it");
        assert_eq!(
            error::refused_message_type(&reply.payload),
            Some(asked),
            "the refusal names the type it refused"
        );
    }

    /// **TWO FIRMWARES DIFFERING ONLY IN THE CORLIB THEY HOLD RESIDENT MUST NOT ADVERTISE THE SAME
    /// SURFACE HASH.** A host cannot detect that difference any other way, and getting it wrong is silent -- a corlib declaring a seam the firmware compiled out still
    /// loads, and the method keeps a placeholder body that returns zero.
    ///
    /// Exercised on the hash function rather than through two serve loops, because the statics behind it
    /// are set once per process and a test cannot un-set them without making the others order-dependent.
    #[cfg(feature = "baked-image")]
    #[test]
    fn the_surface_hash_distinguishes_two_different_resident_corlibs() {
        let registry = lamella_cil_runtime::intrinsic_registry::registry_fingerprint();
        let one = fnv1a(FNV_OFFSET, b"a corlib");
        let other = fnv1a(FNV_OFFSET, b"a DIFFERENT corlib");
        assert_ne!(one, other, "different bytes hash differently");

        let with_one = resident_surface_hash_of(Some(one));
        let with_other = resident_surface_hash_of(Some(other));
        assert_ne!(with_one, with_other, "the surface hash must follow the resident corlib");
        assert_ne!(with_one, registry, "a resident corlib must change the advertised surface");
        assert_ne!(with_other, registry);
        assert_eq!(resident_surface_hash_of(None), registry);
    }

    /// A target holding a resident corlib says so, so a host chooses the bare-program path only where it
    /// is provably available instead of sending one speculatively and falling back on every board that
    /// has none -- which is most of them.
    #[cfg(feature = "baked-image")]
    #[test]
    fn a_resident_corlib_is_advertised_and_only_when_there_is_one() {
        use lamella_wire::Capabilities;
        assert!(serve_caps_with(true, false).has(Capabilities::RESIDENT_CORLIB));
        assert!(!serve_caps_with(false, false).has(Capabilities::RESIDENT_CORLIB));
        for resident in [true, false] {
            assert!(serve_caps_with(resident, false).has(Capabilities::BAKED_IMAGE));
            assert!(serve_caps_with(resident, false).has(Capabilities::PROFILE_CHIPID));
        }
    }

    /// The clock bit is advertised only on a board that OBSERVED its counter moving, and the two
    /// answers are independent of the resident-corlib one -- a board can have either, both or
    /// neither, and folding two facts into one advertisement must not let one imply the other.
    ///
    /// The bit exists because this seam cannot report its own failure: a dead clock is a
    /// `fn() -> u64` returning a well-formed constant, which is why a self-timing benchmark on a
    /// frozen board reported 0 ms with its checksum gate passing.
    #[cfg(feature = "baked-image")]
    #[test]
    fn a_moving_clock_is_advertised_and_only_when_it_was_seen_to_move() {
        use lamella_wire::Capabilities;
        assert!(serve_caps_with(false, true).has(Capabilities::MONOTONIC_CLOCK));
        assert!(!serve_caps_with(false, false).has(Capabilities::MONOTONIC_CLOCK));
        assert!(!serve_caps_with(true, false).has(Capabilities::MONOTONIC_CLOCK));
        assert!(!serve_caps_with(false, true).has(Capabilities::RESIDENT_CORLIB));
        assert!(serve_caps_with(true, true).has(Capabilities::RESIDENT_CORLIB));
        assert!(serve_caps_with(true, true).has(Capabilities::MONOTONIC_CLOCK));
    }

    /// A type the target DOES implement still gets its own reply -- so the refusal above was added at the
    /// terminal arm and did not swallow the dispatch above it.
    #[cfg(feature = "baked-image")]
    #[test]
    fn an_implemented_message_type_is_still_answered_normally() {
        use lamella_wire::{Capabilities, Hello, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg};

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 12, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 1);
        driver.feed(&runner.take_sent());
        let reply = next_event(&mut driver).expect("a reply arrived");
        assert_eq!(reply.msg_type, msg::HELLO_ACK, "not a refusal");
    }

    /// **The refusal must not spread to the drops that are DELIBERATE.** A frame arriving while a
    /// program runs is dropped on purpose -- the host contract is one in-flight resume, and the type is
    /// usually one this target implements perfectly well, so refusing it would be a lie about the type
    /// rather than a fact about the moment.
    ///
    /// Asserted on the source rather than by running a program mid-flight, because reaching that arm
    /// needs a live run and the property being protected is which arm the refusal was added to.
    #[test]
    fn the_deliberate_mid_run_drop_stays_silent() {
        let source = include_str!("lib.rs");
        let service = source
            .split_once("fn service_wire(")
            .expect("service_wire is where a mid-run frame is dropped")
            .1;
        let body = service.split_once("\n}\n").expect("its body ends").0;
        assert!(
            body.contains("_ => {}"),
            "service_wire must keep dropping a mid-run frame silently"
        );
        assert!(
            !body.contains("msg::ERROR"),
            "a mid-run drop must not be reported as an unimplemented type"
        );
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn hello_ack_and_get_profile_report_the_resident_surface() {
        use lamella_cil_runtime::intrinsic_registry;
        use lamella_wire::{
            Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProfileManifest,
            ProtocolRange, msg,
        };

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        driver.send(profile::PROFILE_GET, 2, &0u32.to_le_bytes()).unwrap();
        runner.feed(&driver.take_sent());
        assert_eq!(drain_baked(&mut runner, &mut arena), 2, "the HELLO and the PROFILE_GET");
        driver.feed(&runner.take_sent());

        let ack = HelloAck::decode(&next_event(&mut driver).expect("a HELLO_ACK").payload)
            .expect("the ack decodes");
        let identity = ack.identity.clone();
        let surface = *identity.surfaces.first().expect("the ack advertises a resident runtime");
        assert_eq!(surface.tier, exec::tier::CIL);
        assert_eq!(surface.abi, intrinsic_registry::INTRINSIC_ABI);
        assert_eq!(surface.caps, 0, "the per-runtime claim is RESERVED and must be zero");
        assert_eq!(ack.unreadable_surface_version(), None);
        assert_eq!(surface.hash, resident_surface_hash());

        let frame = next_event(&mut driver).expect("a manifest reply");
        assert_eq!(frame.msg_type, profile::PROFILE_MANIFEST);
        let offset = u32::from_le_bytes(frame.payload[0..4].try_into().unwrap());
        let total = u32::from_le_bytes(frame.payload[4..8].try_into().unwrap());
        assert_eq!(offset, 0);
        assert_eq!(total as usize, frame.payload.len() - 8, "one chunk carries the whole manifest");
        let manifest = ProfileManifest::decode(&frame.payload[8..]).expect("the manifest decodes");
        assert_eq!(manifest.name, intrinsic_registry::profile_name());
        assert_eq!(manifest.identity, identity);
        assert_eq!(manifest.intrinsic_ids.len(), intrinsic_registry::registry_ids().count());
        assert!(
            manifest
                .intrinsic_ids
                .contains(&intrinsic_registry::intrinsic_id("console_write_line_empty")),
            "a known intrinsic id is listed"
        );
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_hello_answers_without_stopping_an_infinite_program_and_an_abort_ends_it() {
        use lamella_wire::{
            Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg,
        };

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/spin.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");
        let image: &'static [u8] = Box::leak(image.into_boxed_slice());

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        driver.send(debug::DBG_RESUME, 7, &[]).unwrap();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 8, &hello.encode()).unwrap();
        driver.send(debug::ABORT, 9, &[]).unwrap();
        target.feed(&driver.take_sent());

        run_debug_session_static(&mut target, image, None, 3, serve_caps(), &mut |_| {})
            .expect("the session ends instead of spinning forever");
        driver.feed(&target.take_sent());

        let stopped = next_event(&mut driver).expect("the entry stop report");
        assert_eq!(stopped.msg_type, debug::EVT_STOPPED);
        let ack_frame = next_event(&mut driver).expect("the HELLO_ACK");
        assert_eq!(ack_frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack_frame.payload).expect("the ack decodes");
        assert!(ack.caps.has(Capabilities::BAKED_IMAGE));
        let aborted = next_event(&mut driver).expect("the abort stop");
        assert_eq!(aborted.msg_type, debug::EVT_STOPPED);
        assert_eq!(aborted.payload[0], debug::reason::ABORTED);
        assert_eq!(aborted.seq, 9, "the stop answers the ABORT, not the resume");
        assert_eq!(stop_exit(&aborted.payload), None, "an aborted run produced no exit value");
        assert!(next_event(&mut driver).is_none(), "nothing else in flight");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_breakpoint_set_while_running_fires_on_the_next_hit() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/spin.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let stopped = |frame: &lamella_wire::Frame| {
            assert_eq!(frame.msg_type, debug::EVT_STOPPED, "expected a stop event");
            let method = u32::from_le_bytes(frame.payload[1..5].try_into().unwrap());
            let offset = u32::from_le_bytes(frame.payload[5..9].try_into().unwrap());
            (frame.payload[0], method, offset)
        };

        const STEPS: u16 = 12;
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        load_then_exec(&mut driver, 1, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 2 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_DETACH, 2 + STEPS, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 2, "the target served the steps");
        driver.feed(&runner.take_sent());
        let (why, _, _) = stopped(&next_event(&mut driver).expect("entry stop"));
        assert_eq!(why, debug::reason::ENTRY);
        let mut loop_method = 0;
        let mut loop_offset = 0;
        for _ in 0..STEPS {
            let (why, method, offset) = stopped(&next_event(&mut driver).expect("a step stop"));
            assert_eq!(why, debug::reason::STEP);
            loop_method = method;
            loop_offset = offset;
        }
        assert_eq!(next_event(&mut driver).expect("phase-1 detach ack").msg_type, debug::DBG_ACK);

        let mut break_payload = Vec::new();
        break_payload.extend_from_slice(&1u16.to_le_bytes());
        break_payload.extend_from_slice(&loop_method.to_le_bytes());
        break_payload.extend_from_slice(&loop_offset.to_le_bytes());
        load_then_exec(&mut driver, 4, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        driver.send(debug::DBG_RESUME, 5, &[]).unwrap();
        driver.send(debug::DBG_BREAK, 6, &break_payload).unwrap();
        driver.send(debug::DBG_DETACH, 7, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(
            drain_baked(&mut runner, &mut arena) >= 2,
            "the session stops at the mid-run breakpoint instead of spinning forever"
        );
        driver.feed(&runner.take_sent());
        assert_eq!(
            stopped(&next_event(&mut driver).expect("phase-2 entry stop")).0,
            debug::reason::ENTRY
        );

        let ack = next_event(&mut driver).expect("the mid-run break ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);
        let (why, hit_method, hit_offset) = stopped(&next_event(&mut driver).expect("breakpoint stop"));
        assert_eq!(why, debug::reason::BREAKPOINT, "the mid-run breakpoint fired");
        assert_eq!((hit_method, hit_offset), (loop_method, loop_offset));
        let detach = next_event(&mut driver).expect("the detach ack");
        assert_eq!(detach.msg_type, debug::DBG_ACK);
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn dbg_locals_and_expand_report_frame_variables() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/locals.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        fn split_values(bytes: &[u8], count: usize, at: &mut usize) -> Vec<(u8, Vec<u8>)> {
            let mut out = Vec::new();
            for _ in 0..count {
                let tag = bytes[*at];
                *at += 1;
                let size = match tag {
                    debug::val::NULL => 0,
                    debug::val::INT32 | debug::val::SINGLE => 4,
                    debug::val::INT64 | debug::val::NATIVE_INT | debug::val::FLOAT => 8,
                    debug::val::OBJECT => 12,
                    debug::val::STRUCT => 10,
                    debug::val::BYREF => 13,
                    debug::val::TYPED_REF => 21,
                    other => panic!("unknown val tag {other}"),
                };
                out.push((tag, bytes[*at..*at + size].to_vec()));
                *at += size;
            }
            out
        }
        fn vars(payload: &[u8]) -> (Vec<(u8, Vec<u8>)>, Vec<(u8, Vec<u8>)>) {
            let locals_n = u16::from_le_bytes([payload[0], payload[1]]) as usize;
            let mut at = 2;
            let locals = split_values(payload, locals_n, &mut at);
            let args_n = u16::from_le_bytes([payload[at], payload[at + 1]]) as usize;
            at += 2;
            let args = split_values(payload, args_n, &mut at);
            (locals, args)
        }
        fn object_token(raw: &[u8]) -> u64 {
            u64::from_le_bytes(raw[4..12].try_into().unwrap())
        }

        const STEPS: u16 = 300;
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        load_then_exec(&mut driver, 1, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 2 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_LOCALS, 900, &0u16.to_le_bytes()).unwrap();
        driver.send(debug::DBG_LOCALS, 901, &1u16.to_le_bytes()).unwrap();
        driver.send(debug::DBG_LOCALS, 902, &9u16.to_le_bytes()).unwrap();
        driver.send(debug::DBG_DETACH, 903, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 2, "the locals session served");
        driver.feed(&runner.take_sent());
        let mut replies = Vec::new();
        while let Some(frame) = next_event(&mut driver) {
            if frame.msg_type == debug::DBG_VARS {
                replies.push(frame.payload);
            }
        }
        assert_eq!(replies.len(), 3, "three DBG_LOCALS requests, three DBG_VARS replies");

        let (spin_locals, spin_args) = vars(&replies[0]);
        assert!(
            spin_locals.iter().any(|(tag, _)| *tag == debug::val::INT32),
            "Spin has at least the int guard local"
        );
        assert_eq!(spin_args.len(), 4);
        assert_eq!(spin_args[0].0, debug::val::INT32);
        assert_eq!(i32::from_le_bytes(spin_args[0].1[..].try_into().unwrap()), 18);
        assert_eq!(spin_args[1].0, debug::val::OBJECT);
        assert_ne!(object_token(&spin_args[1].1), 0, "a declared class instance carries its type handle");
        assert_eq!(spin_args[2].0, debug::val::OBJECT);
        assert_eq!(object_token(&spin_args[2].1), 0, "an array has no recoverable type token");
        assert_eq!(spin_args[3].0, debug::val::BYREF);
        assert_eq!(spin_args[3].1[0], 0, "the ref parameter points at a local slot");

        let (main_locals, main_args) = vars(&replies[1]);
        assert!(main_args.is_empty(), "Main takes no arguments");
        let has = |tag: u8, raw: Option<&[u8]>| {
            main_locals
                .iter()
                .any(|(t, payload)| *t == tag && raw.is_none_or(|bytes| payload.as_slice() == bytes))
        };
        assert!(has(debug::val::INT32, Some(&7i32.to_le_bytes()[..])));
        assert!(has(debug::val::INT64, Some(&1_234_567_890_123i64.to_le_bytes()[..])));
        assert!(has(debug::val::FLOAT, Some(&1.5f64.to_le_bytes()[..])));
        assert!(has(debug::val::NULL, None));
        let box_slot = main_locals
            .iter()
            .position(|(tag, raw)| *tag == debug::val::OBJECT && object_token(raw) != 0)
            .expect("the Box2 local");
        let arr_slot = main_locals
            .iter()
            .position(|(tag, raw)| *tag == debug::val::OBJECT && object_token(raw) == 0)
            .expect("the array local");
        let pair_slot = main_locals
            .iter()
            .position(|(tag, raw)| {
                *tag == debug::val::STRUCT && raw[0..2] == 2u16.to_le_bytes()
            })
            .expect("the Pair local");

        assert_eq!(replies[2], alloc::vec![0u8, 0, 0, 0]);

        fn expand_request(frame: u16, kind: u8, slot: u16, path: &[u16]) -> Vec<u8> {
            ranged_expand_request(frame, kind, slot, path, None)
        }
        fn ranged_expand_request(
            frame: u16,
            kind: u8,
            slot: u16,
            path: &[u16],
            window: Option<(u16, u16)>,
        ) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.extend_from_slice(&frame.to_le_bytes());
            payload.push(kind);
            payload.extend_from_slice(&slot.to_le_bytes());
            payload.push(path.len() as u8);
            for step in path {
                payload.extend_from_slice(&step.to_le_bytes());
            }
            if let Some((first, max)) = window {
                payload.extend_from_slice(&first.to_le_bytes());
                payload.extend_from_slice(&max.to_le_bytes());
            }
            payload
        }
        /// The children a reply carries, and the TOTAL the value has -- which is what tells a host
        /// what it has not asked for.
        fn expansion(payload: &[u8]) -> (usize, Vec<(String, u8, Vec<u8>)>) {
            let total = u16::from_le_bytes([payload[0], payload[1]]) as usize;
            (total, children(payload))
        }
        fn children(payload: &[u8]) -> Vec<(String, u8, Vec<u8>)> {
            let count = u16::from_le_bytes([payload[2], payload[3]]) as usize;
            let mut at = 4;
            let mut out = Vec::new();
            for _ in 0..count {
                let len = payload[at] as usize;
                at += 1;
                let name = String::from_utf8(payload[at..at + len].to_vec()).unwrap();
                at += len;
                let mut one = split_values(payload, 1, &mut at);
                let (tag, raw) = one.remove(0);
                out.push((name, tag, raw));
            }
            out
        }

        load_then_exec(&mut driver, 20, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 21 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_EXPAND, 950, &expand_request(1, 0, box_slot as u16, &[])).unwrap();
        driver.send(debug::DBG_EXPAND, 951, &expand_request(1, 0, arr_slot as u16, &[])).unwrap();
        driver.send(debug::DBG_EXPAND, 952, &expand_request(1, 0, pair_slot as u16, &[])).unwrap();
        driver.send(debug::DBG_EXPAND, 953, &expand_request(1, 0, box_slot as u16, &[0])).unwrap();
        driver.send(debug::DBG_EXPAND, 954, &expand_request(1, 0, 999, &[])).unwrap();
        driver
            .send(debug::DBG_EXPAND, 955, &ranged_expand_request(1, 0, arr_slot as u16, &[], Some((1, 1))))
            .unwrap();
        driver
            .send(debug::DBG_EXPAND, 956, &ranged_expand_request(1, 0, arr_slot as u16, &[], Some((9, 4))))
            .unwrap();
        driver.send(debug::DBG_DETACH, 957, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 2, "the expand session served");
        driver.feed(&runner.take_sent());
        let mut expansions = Vec::new();
        while let Some(frame) = next_event(&mut driver) {
            if frame.msg_type == debug::DBG_CHILDREN {
                expansions.push(frame.payload);
            }
        }
        assert_eq!(expansions.len(), 7);
        let box_children = children(&expansions[0]);
        assert_eq!(box_children.len(), 2);
        assert_eq!(box_children[0].0, "field0");
        assert_eq!(box_children[0].1, debug::val::INT32);
        assert_eq!(box_children[0].2, 40i32.to_le_bytes().to_vec());
        assert_eq!(box_children[1].2, 2i32.to_le_bytes().to_vec());
        let arr_children = children(&expansions[1]);
        assert_eq!(arr_children.len(), 3);
        assert_eq!(arr_children[0].0, "[0]");
        assert_eq!(arr_children[0].2, 10i32.to_le_bytes().to_vec());
        assert_eq!(arr_children[1].2, 0i32.to_le_bytes().to_vec());
        assert_eq!(arr_children[2].2, 30i32.to_le_bytes().to_vec());
        let pair_children = children(&expansions[2]);
        assert_eq!(pair_children.len(), 2);
        assert_eq!(pair_children[0].0, "field0");
        assert_eq!(pair_children[0].2, 5i32.to_le_bytes().to_vec());
        assert_eq!(pair_children[1].2, 6i32.to_le_bytes().to_vec());
        assert!(children(&expansions[3]).is_empty(), "a scalar leaf expands to nothing");
        assert!(children(&expansions[4]).is_empty(), "a bad selector answers the empty expansion");

        let (total, page) = expansion(&expansions[5]);
        assert_eq!(total, 3, "the total is what the VALUE has, not what was sent");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].0, "[1]");
        assert_eq!(page[0].2, 0i32.to_le_bytes().to_vec());
        let (total, page) = expansion(&expansions[6]);
        assert_eq!((total, page.len()), (3, 0));
        assert_eq!(expansion(&expansions[1]).0, 3);
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn dbg_attach_debugs_the_deployed_image_without_resending_it() {
        use lamella_wire::{
            Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg,
        };

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        struct Deployed(&'static [u8]);
        impl FlashSink for Deployed {
            fn image_slice(&self) -> &'static [u8] {
                self.0
            }
            fn erase(&mut self) {}
            fn program(&mut self, _image: &[u8]) -> bool {
                false
            }
            fn program_chunk(&mut self, _offset: usize, _chunk: &[u8], _total: usize) -> bool {
                false
            }
        }
        let mut flash = Deployed(Box::leak(image.into_boxed_slice()));

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();

        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE | Capabilities::DEBUG_BOOT_DEPLOYED),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut flash, &mut arena).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());
        let ack = next_event(&mut driver).expect("a HELLO_ACK");
        assert_eq!(ack.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack.payload).expect("the ack decodes");
        assert!(
            ack.caps.has(Capabilities::DEBUG_BOOT_DEPLOYED),
            "the deploy serve advertises debugging what it booted"
        );

        driver
            .send(exec::EXEC, 2, &[exec::exec_source::DEPLOYED, exec::exec_flags::START_HALTED])
            .unwrap();
        driver.send(debug::DBG_STEP, 3, &[]).unwrap();
        driver.send(debug::DBG_STACK, 4, &[]).unwrap();
        driver.send(debug::DBG_DETACH, 5, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut flash, &mut arena).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());

        let entry = next_event(&mut driver).expect("an entry stop");
        assert_eq!(entry.msg_type, debug::EVT_STOPPED);
        assert_eq!(entry.payload[0], debug::reason::ENTRY);
        let step = next_event(&mut driver).expect("a step stop");
        assert_eq!(step.payload[0], debug::reason::STEP);
        let frames = next_event(&mut driver).expect("a stack reply");
        assert_eq!(frames.msg_type, debug::DBG_FRAMES);
        let ack = next_event(&mut driver).expect("a detach ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);

        let mut empty = Deployed(&[0xFF; 64]);
        driver
            .send(exec::EXEC, 6, &[exec::exec_source::DEPLOYED, exec::exec_flags::START_HALTED])
            .unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut empty, &mut arena).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());
        let stop = next_event(&mut driver).expect("a trap stop");
        assert_eq!(stop.msg_type, debug::EVT_STOPPED);
        assert_eq!(stop.payload[0], debug::reason::TRAP);
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn debug_session_steps_breaks_and_completes_over_the_wire() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let stopped = |frame: &lamella_wire::Frame| {
            assert_eq!(frame.msg_type, debug::EVT_STOPPED, "expected a stop event");
            let method = u32::from_le_bytes(frame.payload[1..5].try_into().unwrap());
            let offset = u32::from_le_bytes(frame.payload[5..9].try_into().unwrap());
            (frame.payload[0], method, offset)
        };

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        load_then_exec(&mut driver, 1, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        driver.send(debug::DBG_STEP, 2, &[]).unwrap();
        driver.send(debug::DBG_STEP, 3, &[]).unwrap();
        driver.send(debug::DBG_STACK, 4, &[]).unwrap();
        driver.send(debug::DBG_DETACH, 5, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 2, "the target served the debug session");
        driver.feed(&runner.take_sent());

        let entry_stop = next_event(&mut driver).expect("an entry stop");
        let (why, entry_method, entry_offset) = stopped(&entry_stop);
        assert_eq!(why, debug::reason::ENTRY);
        let step1 = next_event(&mut driver).expect("a step stop");
        let (why, method_1, offset_1) = stopped(&step1);
        assert_eq!(why, debug::reason::STEP);
        assert_eq!(method_1, entry_method);
        assert_ne!((method_1, offset_1), (entry_method, entry_offset), "the step advanced");
        let step2 = next_event(&mut driver).expect("a second step stop");
        let (why, _, _) = stopped(&step2);
        assert_eq!(why, debug::reason::STEP);
        let frames = next_event(&mut driver).expect("a stack reply");
        assert_eq!(frames.msg_type, debug::DBG_FRAMES);
        let count = u16::from_le_bytes(frames.payload[0..2].try_into().unwrap());
        assert!(count >= 1, "at least the entry frame");
        let top_method = u32::from_le_bytes(frames.payload[2..6].try_into().unwrap());
        assert_eq!(top_method, entry_method, "innermost frame first");
        let ack = next_event(&mut driver).expect("a detach ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);

        let mut break_payload = Vec::new();
        break_payload.extend_from_slice(&1u16.to_le_bytes());
        break_payload.extend_from_slice(&method_1.to_le_bytes());
        break_payload.extend_from_slice(&offset_1.to_le_bytes());
        load_then_exec(&mut driver, 6, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        driver.send(debug::DBG_BREAK, 7, &break_payload).unwrap();
        driver.send(debug::DBG_RESUME, 8, &[]).unwrap();
        driver.send(debug::DBG_RESUME, 9, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(drain_baked(&mut runner, &mut arena) >= 2, "the target served session B");
        driver.feed(&runner.take_sent());

        let (why, _, _) = stopped(&next_event(&mut driver).expect("entry stop B"));
        assert_eq!(why, debug::reason::ENTRY);
        let ack = next_event(&mut driver).expect("a breakpoint ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);
        let hit = next_event(&mut driver).expect("a breakpoint stop");
        let (why, hit_method, hit_offset) = stopped(&hit);
        assert_eq!(why, debug::reason::BREAKPOINT);
        assert_eq!((hit_method, hit_offset), (method_1, offset_1));
        let done = next_event(&mut driver).expect("a done stop");
        let (why, _, _) = stopped(&done);
        assert_eq!(why, debug::reason::DONE);
        let (exit, _flags) = stop_exit(&done.payload).expect("a run result tail");
        assert_eq!(exit, 7);
        assert_eq!(done.payload.len(), 14, "reason, site, exit and flags -- and no output tail");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_hello_mid_debug_session_ends_it_for_the_new_host() {
        use lamella_wire::{Capabilities, Hello, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg};

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        load_then_exec(&mut driver, 1, load::LOAD_IMAGE, &image, exec::exec_flags::START_HALTED);
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 2, &hello.encode()).unwrap();
        send_image(&mut driver, 3, &image).unwrap();
        runner.feed(&driver.take_sent());

        assert!(drain_baked(&mut runner, &mut arena) >= 3, "the stale session ended and the fresh run served");
        driver.feed(&runner.take_sent());

        let entry = next_event(&mut driver).expect("the stale session's entry stop");
        assert_eq!(entry.msg_type, debug::EVT_STOPPED);
        let ack = next_event(&mut driver).expect("the successor's HELLO_ACK");
        assert_eq!(ack.msg_type, msg::HELLO_ACK);
        let mut run = RunCollector::new(3);
        assert!(run.poll(&mut driver).unwrap(), "the fresh run ended");
        let result = run.finish().expect("a fresh run result");
        assert_eq!(result.exit, 7);
    }

    #[cfg(feature = "baked-image")]
    #[derive(Default)]
    struct MockFlash {
        data: Vec<u8>,
    }

    #[cfg(feature = "baked-image")]
    impl FlashSink for MockFlash {
        fn image_slice(&self) -> &'static [u8] {
            &[]
        }
        fn erase(&mut self) {
            self.data.clear();
        }
        fn program(&mut self, image: &[u8]) -> bool {
            self.data = image.to_vec();
            true
        }
        fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
            if offset == 0 {
                self.data = vec![0xff; total];
            }
            if offset + chunk.len() > self.data.len() {
                return false;
            }
            self.data[offset..offset + chunk.len()].copy_from_slice(chunk);
            true
        }
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn chunked_deploy_reassembles_an_over_64k_image() {
        use lamella_wire::MemTransport;

        let image: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let total = image.len();
        let chunk_len = 32_768usize;
        let mut sink = MockFlash::default();

        let mut offset = 0;
        let mut seq = 0u16;
        while offset < total {
            let end = (offset + chunk_len).min(total);
            let mut payload = Vec::new();
            payload.extend_from_slice(&(offset as u32).to_le_bytes());
            payload.extend_from_slice(&(total as u32).to_le_bytes());
            payload.extend_from_slice(&image[offset..end]);

            let mut driver = MemTransport::new();
            let mut runner = MemTransport::new();
            let mut arena = ArtifactLoad::new();
            driver.send(deploy::DEPLOY_IMAGE, seq, &payload).unwrap();
            runner.feed(&driver.take_sent());
            assert_eq!(
                serve_one_deploy(&mut runner, &mut sink, &mut arena).unwrap(),
                Served::Handled,
                "the target handled a chunk"
            );
            driver.feed(&runner.take_sent());
            let ack = driver.poll().unwrap().expect("a chunk ack");
            assert_eq!(ack.msg_type, deploy::XFER_RESULT);
            assert_eq!(ack.payload[0], deploy::xfer::MATCHED, "the chunk verified");

            offset = end;
            seq += 1;
        }
        assert_eq!(sink.data, image, "the reassembled image matches the original");
    }

    #[cfg(feature = "baked-image")]
    #[derive(Default)]
    struct MockWincFlasher {
        begun: Option<(usize, usize)>,
        data: Vec<u8>,
        finished: bool,
    }

    #[cfg(feature = "baked-image")]
    impl WincFlasher for MockWincFlasher {
        fn begin(&mut self, offset: usize, total: usize) -> bool {
            self.begun = Some((offset, total));
            self.data = vec![0xff; offset + total];
            true
        }
        fn program(&mut self, offset: usize, data: &[u8]) -> bool {
            if offset + data.len() > self.data.len() {
                return false;
            }
            self.data[offset..offset + data.len()].copy_from_slice(data);
            true
        }
        fn finish(&mut self) -> bool {
            self.finished = true;
            true
        }
    }

    /// One `EXTENDED` payload: `ns(u16 LE)`, `op(u16 LE)`, then the op's own bytes.
    #[cfg(feature = "baked-image")]
    fn extended(op: u16, body: &[u8]) -> Vec<u8> {
        use lamella_wire::msg::ext;
        let mut payload = ext::NS_MODULE_FIRMWARE.to_le_bytes().to_vec();
        payload.extend_from_slice(&op.to_le_bytes());
        payload.extend_from_slice(body);
        payload
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn module_firmware_streams_through_the_flasher_hook() {
        use lamella_wire::MemTransport;
        use lamella_wire::msg::{self, ext};

        let firmware: Vec<u8> = (0..5000u32).map(|i| (i % 241) as u8).collect();
        let base = 4096usize;
        let chunk_len = 1024usize;
        let mut sink = MockFlash::default();
        let mut flasher = MockWincFlasher::default();

        let mut exchange = |op: u16, seq: u16, body: &[u8], flasher: &mut MockWincFlasher| {
            let mut driver = MemTransport::new();
            let mut runner = MemTransport::new();
            let mut arena = ArtifactLoad::new();
            driver.send(msg::EXTENDED, seq, &extended(op, body)).unwrap();
            runner.feed(&driver.take_sent());
            assert_eq!(
                serve_one_deploy_with(
                    &mut runner,
                    &mut sink,
                    &mut |_vm| {},
                    Some(flasher as &mut dyn WincFlasher),
                    &mut arena,
                )
                .unwrap(),
                Served::Handled,
            );
            driver.feed(&runner.take_sent());
            let ack = next_event(&mut driver).expect("a module-firmware result");
            assert_eq!(ack.msg_type, msg::EXTENDED);
            assert_eq!(ack.payload, extended(ext::MODULE_FW_RESULT, &[1]), "the step succeeded");
        };

        let mut start = Vec::new();
        start.extend_from_slice(&(base as u32).to_le_bytes());
        start.extend_from_slice(&(firmware.len() as u32).to_le_bytes());
        exchange(ext::MODULE_FW_START, 1, &start, &mut flasher);
        assert_eq!(flasher.begun, Some((base, firmware.len())));

        let mut offset = 0;
        let mut seq = 2u16;
        while offset < firmware.len() {
            let end = (offset + chunk_len).min(firmware.len());
            let mut payload = Vec::new();
            payload.extend_from_slice(&((base + offset) as u32).to_le_bytes());
            payload.extend_from_slice(&firmware[offset..end]);
            exchange(ext::MODULE_FW_CHUNK, seq, &payload, &mut flasher);
            offset = end;
            seq += 1;
        }
        exchange(ext::MODULE_FW_END, seq, &[], &mut flasher);

        assert!(flasher.finished, "END reached finish");
        assert_eq!(&flasher.data[base..], &firmware[..], "the programmed image matches");
    }

    /// A module-firmware transfer HOLDS the flash section, and an extension that writes nothing does
    /// not.
    ///
    /// Treating EVERY extension frame as a write holds a session against a physically attached
    /// cable for the length of any extension at all. The rule lives with the namespace that defines
    /// the ops, and the arbiter reads only the protocol's own four-byte header to reach it.
    #[test]
    fn only_the_module_firmware_ops_that_write_hold_the_flash_section() {
        use lamella_wire::msg::ext;

        assert!(ext::writes_flash(ext::NS_MODULE_FIRMWARE, ext::MODULE_FW_START));
        assert!(ext::writes_flash(ext::NS_MODULE_FIRMWARE, ext::MODULE_FW_CHUNK));
        assert!(ext::writes_flash(ext::NS_MODULE_FIRMWARE, ext::MODULE_FW_END));
        assert!(!ext::writes_flash(ext::NS_MODULE_FIRMWARE, ext::MODULE_FW_RESULT));
        assert!(!ext::writes_flash(ext::NS_LAMELLA, 1));
        assert!(ext::writes_flash(0xBEEF, 1));
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_target_without_a_module_flasher_refuses_by_name() {
        use lamella_wire::MemTransport;
        use lamella_wire::msg::{self, ext};

        let mut sink = MockFlash::default();
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let mut arena = ArtifactLoad::new();
        driver.send(msg::EXTENDED, 1, &extended(ext::MODULE_FW_START, &[0, 0, 0, 0, 16, 0, 0, 0]))
            .unwrap();
        runner.feed(&driver.take_sent());
        assert_eq!(
            serve_one_deploy_with(&mut runner, &mut sink, &mut |_vm| {}, None, &mut arena).unwrap(),
            Served::Handled,
        );
        driver.feed(&runner.take_sent());
        let ack = next_event(&mut driver).expect("a refusal");
        assert_eq!(ack.msg_type, msg::ERROR);
        assert_eq!(lamella_wire::error::refused_message_type(&ack.payload), Some(msg::EXTENDED));
    }
}
