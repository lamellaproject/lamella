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

/// Lamella Link message types for the REPL (debug types live elsewhere).
pub mod repl {
    /// Host -> target: run a program. Payload = the program assembly (PE) bytes.
    pub const RUN_PROGRAM: u8 = 0x20;
    /// Target -> host: the program's result. Payload = `exit(i32 LE) | stdout(UTF-8)`.
    pub const RUN_RESULT: u8 = 0x21;
    /// Host -> target: run a BAKED image. Payload = one self-contained `.lmli` image's bytes
    /// (the host compiles + bakes per submission). The PE-less constrained-target path,
    /// advertised by `Capabilities::BAKED_IMAGE`; answered by the same [`RUN_RESULT`].
    pub const RUN_IMAGE: u8 = 0x22;

    /// The incremental-REPL SESSION channel (the 0x50 block): a device holds a LIVE session --
    /// a persistent interpreter Vm + heap + a growing `<repl>.__Repl` instance -- and accepts a
    /// compiled submission DELTA that loads into that live session and runs ONLY the new code
    /// (prior submissions run exactly once, never re-execute). This is a distinct channel from
    /// the whole-program 0x20 RUN ops above (a session ACCUMULATES; a RUN is transient) and from
    /// the 0x30 profile / 0x40 telemetry blocks, so a session-capable board can advertise it
    /// (`Capabilities::REPL_RUN`) without disturbing the deploy/run path. Served by
    /// [`crate::serve_one_repl`]; the reply ops flow target -> host on the same channel. (It sits
    /// one block above the reserved 0x40 telemetry/scope range -- the PLC "observe" half that
    /// pairs with this "adjust" half, [`crate::telemetry`].)
    ///
    /// Host -> target: open a live session. Payload = `heartbeat_ms(u32 LE) |
    /// config_len(u16 LE) | config[config_len] | bootstrap[tail]`. `heartbeat_ms` is the comms
    /// deadman interval, currently unarmed (0 = disabled); `config` is a RESERVED per-output
    /// safe-state blob, currently empty -- both are carried from the first frame
    /// so the fail-safe supervisor has its hook without a wire break. `bootstrap` is the empty
    /// `<repl>.__Repl` library the host emits once. Answered by [`REPL_OPENED`].
    pub const REPL_OPEN: u8 = 0x50;
    /// Target -> host: the session opened (or did not). Payload = `status(u8)`; on `status == 0`
    /// then `session_id(u32 LE) | max_fields(u16 LE) | max_methods(u16 LE) | heap_budget(u32 LE)`
    /// (a 0 limit = unspecified); on a
    /// nonzero status the tail is the failure reason (UTF-8).
    pub const REPL_OPENED: u8 = 0x51;
    /// Host -> target: a compiled submission DELTA to load into the live session and run. Payload
    /// = the delta assembly (PE) bytes -- one `Submit$N(__Repl)` that binds prior session
    /// variables/types by name and reads/writes the live `__Repl`. Answered by
    /// [`REPL_DELTA_RESULT`].
    pub const REPL_DELTA: u8 = 0x52;
    /// Target -> host: the submission's result. Payload = `status(u8) | new_fields(u16 LE) |
    /// display_len(u16 LE) | display[display_len] | output[tail]`. `status`: 0 ok, 1 no open
    /// session, 2 the delta did not load, 3 the submission trapped. `new_fields` is how many
    /// session variables this delta added (the live instance grew by that many). `display` is the
    /// submission's rendered value (`""` for a void statement); `output` is the console output
    /// THIS submission produced. On a nonzero status `display` is empty and `output` is the reason.
    pub const REPL_DELTA_RESULT: u8 = 0x53;
    /// Host -> target: close the live session cleanly (a graceful detach -- distinct from a lost
    /// link, which the comms deadman handles). Empty payload; the tail is RESERVED for a
    /// teardown reason. Answered by [`REPL_CLOSED`].
    pub const REPL_CLOSE: u8 = 0x54;
    /// Target -> host: the session was closed. Payload = `ok(u8)` (1). Idempotent: closing when no
    /// session is open still answers `ok = 1`.
    pub const REPL_CLOSED: u8 = 0x55;
    /// Host -> target: a session heartbeat (keepalive). Empty payload. RESERVED for the comms
    /// deadman armed from `REPL_OPEN`'s `heartbeat_ms`; it currently only refreshes the
    /// last-contact marker and is not answered (any frame counts as contact).
    pub const REPL_PING: u8 = 0x56;
    /// Host -> target: RESET THE TARGET, the only thing that reclaims an exhausted arena.
    ///
    /// Empty payload. The target acknowledges with [`REPL_RESETTING`] and then performs a SYSTEM
    /// reset back into serve mode -- so the session, its module, and every allocation any of it
    /// made are gone, and the host must re-`HELLO` before opening a new session.
    ///
    /// **Why a whole-target reset rather than a session reset.** The constrained serve allocates
    /// from a segregated-fit heap whose bump frontier never retreats and which never splits or
    /// coalesces across size classes. Once a session has carved the arena,
    /// dropping it returns its blocks to per-class free lists that a fresh session cannot spend, so
    /// reopening yields a session refused its first submission. Nothing short of a reset reclaims,
    /// which is why this op exists and why it is honest about being a reboot.
    ///
    /// Without it a host that exhausts a board has no in-band recovery at all -- it needs a debug
    /// probe or a power cycle, neither of which a REPL user has to hand.
    pub const REPL_RESET: u8 = 0x58;
    /// Target -> host: the reset was accepted and is imminent. Payload = `ok(u8)` (1). Sent BEFORE
    /// the reset and flushed, so a host can distinguish an accepted reset from a target that simply
    /// stopped answering. Expect the link to drop immediately after; re-`HELLO` to resume.
    pub const REPL_RESETTING: u8 = 0x59;
}

/// Lamella Link message types for the DEBUG channel (the reserved 0x10+ range). A code
/// location crosses the wire as `(method_id: u32, offset: u32)` in the TARGET's code-unit
/// domain -- on the `in_place` (baked) tier that is the CIL BYTE offset, the same domain
/// Portable-PDB sequence points use, so the host's source mapping needs no conversion.
/// All source/PDB knowledge stays host-side; the target only ever sees ids and offsets.
pub mod debug {
    /// Host -> target: a baked image to debug. The target boots it HALTED at the entry
    /// point and replies [`EVT_STOPPED`] (reason `Entry`).
    pub const DBG_IMAGE: u8 = 0x10;
    /// Host -> target: run until a breakpoint, completion, a trap, or a [`DBG_PAUSE`].
    /// Replies [`EVT_STOPPED`] when execution stops.
    pub const DBG_RESUME: u8 = 0x11;
    /// Host -> target: execute one step. Replies [`EVT_STOPPED`] (reason `Step`, or
    /// `Breakpoint`/`Done`/`Trap` if the step landed there).
    pub const DBG_STEP: u8 = 0x12;
    /// Host -> target: replace ALL breakpoints. Payload = `count(u16 LE)` then `count` x
    /// `(method_id: u32 LE, offset: u32 LE)`. Replies [`DBG_ACK`]. Accepted BOTH while halted
    /// AND mid-run (during a [`DBG_RESUME`]) -- a breakpoint set while the program runs takes
    /// effect on its next hit, so the IDE can add one without first pausing.
    pub const DBG_BREAK: u8 = 0x13;
    /// Host -> target: request the call stack. Replies [`DBG_FRAMES`].
    pub const DBG_STACK: u8 = 0x14;
    /// Target -> host: the call stack, innermost first. Payload = `count(u16 LE)` then
    /// `count` x `(method_id: u32 LE, offset: u32 LE)`.
    pub const DBG_FRAMES: u8 = 0x15;
    /// Host -> target: pause a running [`DBG_RESUME`] at the next poll boundary. Replies
    /// [`EVT_STOPPED`] (reason `Paused`); a no-op acknowledged the same way while halted.
    pub const DBG_PAUSE: u8 = 0x16;
    /// Target -> host: a command that changes no execution state completed ([`DBG_BREAK`]).
    pub const DBG_ACK: u8 = 0x17;
    /// Target -> host: execution stopped. Payload = `reason(u8)` + `method_id(u32 LE)` +
    /// `offset(u32 LE)`, and for `Done`/`Trap` additionally the run result tail:
    /// `exit(i32 LE)` + `stdout(UTF-8)`. Reasons: 0 Entry, 1 Step, 2 Breakpoint,
    /// 3 Paused, 4 Done, 5 Trap. `Done`/`Trap` END the debug session.
    pub const EVT_STOPPED: u8 = 0x18;
    /// Host -> target: end the debug session (the target discards it and returns to the
    /// serve loop). Replies [`DBG_ACK`].
    pub const DBG_DETACH: u8 = 0x19;
    /// Host -> target: debug the PERSISTENTLY DEPLOYED image (the flash region
    /// `DEPLOY_IMAGE`/`DEPLOY_CHUNK` programmed) IN PLACE -- no payload, so attaching to a
    /// ~190 KB deployed app costs nothing on the wire. The target boots that image HALTED
    /// at the entry and replies [`EVT_STOPPED`] (reason `Entry`); from there the session is
    /// identical to a [`DBG_IMAGE`] one. A missing/corrupt image answers [`EVT_STOPPED`]
    /// (reason `Trap`) carrying the boot error. Served by deploy-capable targets
    /// ([`crate::serve_one_deploy`]), advertised as `Capabilities::DEBUG_ATTACH`.
    pub const DBG_ATTACH: u8 = 0x1A;
    /// Host -> target: request one frame's variables (advertised as `Capabilities::LOCALS`).
    /// Payload = `frame_index(u16 LE)` in the [`DBG_FRAMES`] order (0 = innermost). Replies
    /// [`DBG_VARS`]. Accepted while HALTED (between stops the values are in motion).
    pub const DBG_LOCALS: u8 = 0x1B;
    /// Target -> host: one frame's variables, POSITIONAL (slot names live host-side in the
    /// srcmap's `local_variables`; the device carries none). Payload = `locals(u16 LE)` then
    /// that many [`val`]-tagged values, then `args(u16 LE)` and that many more. An unknown
    /// frame index answers `0, 0` (no locals, no args).
    pub const DBG_VARS: u8 = 0x1C;
    /// Host -> target: expand one value's children (an object's fields, an array's elements,
    /// a box's content, an inline struct's fields). Payload = `frame_index(u16 LE)` (the
    /// [`DBG_FRAMES`] order) + `root_kind(u8)` (0 = local, 1 = argument) + `root_slot(u16
    /// LE)` + `path_len(u8)` + `path_len` x `child_index(u16 LE)`. The selector is STATELESS:
    /// the target re-walks it from the frame root on every request (each index picks the
    /// N-th child of the previous step's expansion), so no device-side handle table exists
    /// to invalidate on resume. Replies [`DBG_CHILDREN`]; an unresolvable selector answers
    /// an empty one.
    pub const DBG_EXPAND: u8 = 0x1D;
    /// Target -> host: the expanded children. Payload = `count(u16 LE)` then `count` x
    /// `(name_len(u8), name(UTF-8), <val>)` -- the names here are runtime TYPE metadata
    /// (`fieldN` by slot, `[i]`, a box's `value`), not source local names.
    pub const DBG_CHILDREN: u8 = 0x1E;
    /// Target -> host: console output produced SO FAR, while the program is still running.
    /// Payload = `bytes(UTF-8)`, no header. UNSOLICITED and sequence-independent -- it arrives
    /// during a [`DBG_RESUME`] that has not answered yet, so it carries the resume's `seq` but is
    /// distinguished by its message TYPE. A host must not mistake it for the resume's reply.
    ///
    /// WHY IT EXISTS: every terminal frame ([`EVT_STOPPED`], `RUN_RESULT`) carries the WHOLE
    /// stdout, and by construction none of them exists until the program has finished. A program
    /// that prints, sleeps five seconds, then prints, therefore said nothing for five seconds and
    /// then said everything -- correct output, delivered in the one shape that makes a running
    /// program look hung.
    ///
    /// ADDITIVE ON PURPOSE, so there is no flag day: the terminal frames STILL carry the complete
    /// stdout. A host that ignores this type behaves exactly as it did before, and an old firmware
    /// that never sends one leaves a new host rendering the terminal buffer as it always has.
    ///
    /// A CHUNK NEVER SPLITS A CHARACTER. The target holds back a trailing high surrogate rather
    /// than encoding half a pair, so the host can decode each frame independently and never has to
    /// join across frames to find a code point. That decision is the target's because it is nearly
    /// free here and would be a buffering rule on every host otherwise.
    pub const EVT_OUTPUT: u8 = 0x1F;

    /// The `<val>` encoding [`DBG_VARS`]/[`DBG_CHILDREN`] carry: one tag byte, then the
    /// payload the tag implies (all little-endian). [`NULL`] is bare; [`INT32`] carries an
    /// `i32`; [`INT64`]/[`NATIVE_INT`] an `i64`; [`FLOAT`] an `f64`; [`SINGLE`] an `f32`;
    /// [`OBJECT`] a `handle(u32)` (the heap slot, a display/correlation id -- stale after a
    /// resume) + `type_token(u64)` (the asm-folded `TypeDef` handle, 0 when the value has no
    /// recoverable type identity -- an array, a box, a string); [`STRUCT`] a
    /// `field_count(u16)` + `type_token(u64)` (0 today: an inline value-type instance
    /// carries no type id at runtime); [`BYREF`] a location descriptor `kind(u8) + a(u32) +
    /// b(u32) + c(u32)` (kind 0 local`{frame,slot,-}`, 1 argument`{frame,slot,-}`, 2
    /// stackalloc`{frame,buffer,offset}`, 3 field`{object,slot,-}`, 4
    /// element`{array,index,byte_offset}`, 5 string-data`{string,byte_offset,-}`, 6
    /// local-bytes`{frame,slot,byte_offset}`, 7 arg-bytes`{frame,slot,byte_offset}`, 8
    /// static`{slot,-,-}`, 9 boxed`{object,-,-}`, 10 nested-field`{slot,base_kind,-}`);
    /// [`TYPED_REF`] a `type_token(u64)` + the same location descriptor. A tier compiled
    /// without a value's feature (float, typed references) never produces its tag.
    pub mod val {
        /// The null reference (no payload).
        pub const NULL: u8 = 0x00;
        /// A 32-bit integer (`i32 LE`) -- also `bool`/`char`/small ints, widened per III.1.1.1.
        pub const INT32: u8 = 0x01;
        /// A 64-bit integer (`i64 LE`).
        pub const INT64: u8 = 0x02;
        /// A native-sized integer (`i64 LE` on the wire regardless of target width).
        pub const NATIVE_INT: u8 = 0x03;
        /// A `System.Double` (`f64 LE`).
        pub const FLOAT: u8 = 0x04;
        /// A `System.Single` (`f32 LE`).
        pub const SINGLE: u8 = 0x05;
        /// An object reference: `handle(u32 LE) + type_token(u64 LE)`.
        pub const OBJECT: u8 = 0x06;
        /// An inline value-type instance: `field_count(u16 LE) + type_token(u64 LE)`.
        pub const STRUCT: u8 = 0x07;
        /// A managed pointer: `kind(u8) + a(u32 LE) + b(u32 LE) + c(u32 LE)`.
        pub const BYREF: u8 = 0x08;
        /// A typed reference: `type_token(u64 LE)` + the [`BYREF`] location descriptor.
        pub const TYPED_REF: u8 = 0x09;
    }

    /// [`EVT_STOPPED`] reasons.
    pub mod reason {
        /// Booted and halted at the entry point (the reply to [`super::DBG_IMAGE`]).
        pub const ENTRY: u8 = 0;
        /// A [`super::DBG_STEP`] completed.
        pub const STEP: u8 = 1;
        /// Execution arrived at a [`super::DBG_BREAK`] location.
        pub const BREAKPOINT: u8 = 2;
        /// A [`super::DBG_PAUSE`] took effect (or acknowledged an already-halted target).
        pub const PAUSED: u8 = 3;
        /// The program completed; the payload carries the run-result tail. Ends the session.
        pub const DONE: u8 = 4;
        /// The program trapped (or the image failed to boot); run-result tail attached.
        /// Ends the session.
        pub const TRAP: u8 = 5;
    }
}

/// Lamella Link message types for PROFILE INTROSPECTION (the 0x30 range): the board tells the IDE
/// what it is. The `HELLO_ACK` already carries the compact [`lamella_wire::ProfileIdentity`]
/// (abi level + surface hash + name) at zero extra round-trips; this pair pulls the FULL
/// resident manifest only when the host's cache misses that hash -- the resident
/// profile's identity/manifest split.
pub mod profile {
    /// Host -> target: request the resident-profile manifest. Empty payload. Answered by
    /// [`PROFILE_MANIFEST`].
    pub const GET_PROFILE: u8 = 0x30;
    /// Target -> host: the manifest ([`lamella_wire::ProfileManifest`] bytes -- the identity +
    /// the complete intrinsic-id listing of the resident surface).
    pub const PROFILE_MANIFEST: u8 = 0x31;
}

/// Lamella Link message types for on-device TELEMETRY / live-signal SCOPE (the 0x40 range).
/// RESERVED (the telemetry-scope "observe" half that
/// pairs with the incremental REPL's "adjust"): the host subscribes to device signals -- command
/// outputs, sensor traces, energy -- and the target streams samples asynchronously over the live
/// session. **No firmware implements this range; the identifiers are claimed here so that nothing
/// else takes them and breaks the wire when it lands.** The payload
/// shapes are not settled; only the type bytes + the `TELEMETRY`
/// capability ([`lamella_wire::Capabilities::TELEMETRY`]) are reserved here.
pub mod telemetry {
    /// Host -> target: subscribe to a signal. RESERVED (payload shape set at build).
    pub const SCOPE_SUBSCRIBE: u8 = 0x40;
    /// Host -> target: unsubscribe from a signal. RESERVED (payload shape set at build).
    pub const SCOPE_UNSUBSCRIBE: u8 = 0x41;
    /// Target -> host: an asynchronous sample batch for a subscribed signal. RESERVED (payload
    /// shape set at build).
    pub const SCOPE_SAMPLE: u8 = 0x42;
}

/// Lamella Link message types for PERSISTENT deploy (write a baked image to the target's flash
/// so it boots on reset), in the 0x20 REPL range beside [`repl::RUN_IMAGE`].
pub mod deploy {
    /// Host -> target: write a baked image to the persistent flash region and keep it, so
    /// the target boots it on the next reset / power-cycle. Payload = the image bytes.
    /// Answered by [`DEPLOY_RESULT`]. (Run-from-RAM stays [`super::repl::RUN_IMAGE`]; this
    /// is the durable "deploy the app" mode.)
    pub const DEPLOY_IMAGE: u8 = 0x23;
    /// Host -> target: erase the persistent image (un-deploy), so the next boot serves
    /// instead of running an app. Answered by [`DEPLOY_RESULT`].
    pub const DEPLOY_CLEAR: u8 = 0x24;
    /// Target -> host: `ok(u8)` -- 1 if the flash write verified (or the clear succeeded),
    /// 0 on a flash fault.
    pub const DEPLOY_RESULT: u8 = 0x25;
    /// Host -> target: write one CHUNK of a baked image to flash. Payload =
    /// `offset(u32 LE) | total(u32 LE) | chunk bytes`. Chunks arrive in ascending offset order;
    /// the target erases the region on the first chunk (`offset == 0`, sized from `total`) and
    /// writes each chunk straight to flash -- so an image larger than one 64 KB wire frame deploys
    /// without ever holding it whole in RAM. Each chunk is answered by [`DEPLOY_RESULT`]; the final
    /// chunk's reply is the deploy's overall result.
    pub const DEPLOY_CHUNK: u8 = 0x26;
    /// Host -> target: query the deployed image (no payload). Answered by [`DEPLOY_STATUS_RESULT`].
    /// Lets a host skip re-deploying an image the target already holds -- content-addressed deploy,
    /// the phone-style "already installed, just run it" flow.
    pub const DEPLOY_STATUS: u8 = 0x27;
    /// Target -> host: `present(u8) | checksum(u64 LE)` -- present=1 with the stored image's content
    /// checksum (`lamella_cil_runtime::baked_image_checksum`) if a valid image is deployed, else
    /// present=0 (checksum 0). The host compares it to a freshly-baked image's checksum.
    pub const DEPLOY_STATUS_RESULT: u8 = 0x28;
    /// Host -> target: boot the deployed image NOW -- a clean self-reset into the boot-run path, so
    /// deploy->run needs no debug probe. No reply and no payload (the target resets).
    pub const DEPLOY_RUN: u8 = 0x29;
    /// Host -> target: begin a WINC1500 module-firmware write. Payload =
    /// `offset(u32 LE) | total(u32 LE)`: the firmware initializes the module into its flash
    /// download mode and erases the module-flash span `[offset, offset + total)`. Answered by
    /// [`WINC_FW_RESULT`] (the erase of a full ~332 KB image takes seconds -- hosts wait
    /// generously). A target without a WINC module answers `ok = 0`.
    pub const WINC_FW_START: u8 = 0x2a;
    /// Host -> target: one chunk of the module-firmware image. Payload =
    /// `offset(u32 LE, absolute module-flash address) | chunk bytes`; the firmware programs the
    /// chunk into the erased span and verifies it by read-back. Answered by [`WINC_FW_RESULT`]
    /// per chunk, so the STREAM never needs the whole image in the target's RAM -- a 32 KB part
    /// updates a 332 KB module image.
    pub const WINC_FW_CHUNK: u8 = 0x2b;
    /// Host -> target: the module-firmware write is complete (no payload). The firmware runs its
    /// final sanity read and parks the module for a clean reboot into the new firmware. Answered
    /// by [`WINC_FW_RESULT`].
    pub const WINC_FW_END: u8 = 0x2c;
    /// Target -> host: `ok(u8)` -- 1 if the begin/program/finish step succeeded, 0 on any module
    /// wire fault, verify mismatch, or a target without a module flasher.
    pub const WINC_FW_RESULT: u8 = 0x2d;
}

/// Lamella Link message types for a BUNDLE -- an artifact a target loads through a different front end
/// than a baked image, in the same two shapes an image has: run it now, or persist it so it boots.
///
/// RESERVED: no firmware implements these yet. **The identifiers are claimed here so nothing else takes
/// them and breaks the wire when they land**, which is the same reason the telemetry range above is
/// claimed rather than left to whoever gets there first.
///
/// # Why a distinct OP rather than a kind byte on the image ops
///
/// A target that refuses an op it does not implement fails better than one that ACCEPTS a payload it
/// cannot interpret. The refusal happens at the op, before the payload crosses the wire at all, and it
/// names the thing that is missing; the alternative gets as far as a loader deciding it does not
/// recognize a header, on a target with no good way to say so. It is the same rule as a hardware
/// binding refusing to bind rather than simulating, and a runtime seam refusing rather than returning
/// zero.
///
/// # Two consequences worth stating rather than leaving to be inferred
///
/// * **[`lamella_wire::PROTOCOL_VERSION`] does NOT change.** Adding an op is additive: a target that
///   predates these answers the existing refusal, which is the designed behavior rather than a version
///   break. Reusing an existing op with a new payload shape WOULD have changed the meaning of bytes
///   already shipping under the current version.
/// * **A bundle NEVER travels under [`deploy::DEPLOY_CHUNK`].** [`DEPLOY_BUNDLE`] carries
///   `[offset u32][total u32][bytes]` ALWAYS, and a bundle that fits one frame is the degenerate
///   one-chunk case (`offset = 0`, `total = len`). It costs eight bytes on a small bundle and buys two
///   properties worth more than that: there is only ONE payload shape, so the single-frame and chunked
///   paths are the same code on both sides; and **the artifact kind rides EVERY frame rather than only
///   the committing one**, so a transfer interrupted partway cannot be misread as a partial baked image
///   and a target reassembling bytes never holds them in a kind-unknown state. The commit rule is the
///   image path's: complete when `offset + len == total`.
///
/// # A host gates on the CAPABILITY
///
/// [`lamella_wire::Capabilities::BUNDLE`] is the bit a target sets when it implements these, and it
/// rides in the `HELLO_ACK` a session already exchanges -- so gating on it costs no round-trip. Probing
/// by sending one also works now that an unimplemented type is refused rather than dropped
/// ([`lamella_wire::error`]), but it costs a round-trip per question and answers only one of them.
///
/// # These two FILL the 0x20 block
///
/// The block held run, deploy, and a module-firmware transfer, and 0x2e/0x2f were the last free pair
/// in it. **A third op in this family needs a range decision rather than the next number**, which is
/// worth knowing before the question is urgent.
pub mod bundle {
    /// Host -> target: run a bundle from RAM now, without persisting it. Payload = the bundle's bytes.
    /// The run-from-RAM counterpart of [`super::repl::RUN_IMAGE`], answered the same way by
    /// [`super::repl::RUN_RESULT`]. RESERVED.
    pub const RUN_BUNDLE: u8 = 0x2e;
    /// Host -> target: persist a bundle to the deploy region so the target boots it on reset. The
    /// counterpart of [`deploy::DEPLOY_IMAGE`], answered the same way by [`deploy::DEPLOY_RESULT`].
    ///
    /// **Payload is ALWAYS `[offset u32][total u32][bytes]`**, one shape for both the single-frame and
    /// the chunked case -- see the module docs for why that is worth eight bytes on a small bundle.
    ///
    /// **The asymmetry with [`RUN_BUNDLE`] is deliberate rather than an oversight:** running from RAM
    /// does not chunk today, so it takes the bare bytes, and giving it a header it does not need would
    /// be inventing a shape ahead of a use for it. If it ever chunks, this is the shape to copy.
    /// RESERVED.
    pub const DEPLOY_BUNDLE: u8 = 0x2f;
}

/// Lamella Link message types for the LIVE debug agent (the 0x60 range): read and write the
/// target's memory **while a deployed app is still running**, without stopping it.
///
/// This is the on-target half of a host-side REPL evaluating against a live program: the host runs
/// the interpreter and redirects its loads and stores over the wire to here. It is deliberately the
/// smallest thing that can answer that question -- an address and a length -- because that primitive
/// is the same on every tier. An interpreted app's state lives on a heap the host cannot name and an
/// AOT app's lives at addresses a symbol map does name, and neither changes what this op does.
///
/// # Why this is a distinct range from the 0x10 DEBUG ops rather than more of them
///
/// The 0x10 range is a HALTED channel. [`super::debug::DBG_LOCALS`] says so in its own contract:
/// accepted while halted, because "between stops the values are in motion". Every op there presumes
/// a program stopped at a known point, and several of them ([`super::debug::DBG_STEP`],
/// [`super::debug::DBG_RESUME`]) have no meaning otherwise. These two ops presume the opposite.
/// Mixing them would give one range two contracts, with nothing in a message type to say which one
/// a target is honoring.
///
/// # What a running target's answer does NOT promise, and why that is stated here
///
/// **A multi-word read is not atomic with respect to the program.** The agent is serviced between
/// the app's instructions, so a structure the app updates in more than one store can be read
/// half-updated. A host that renders such a value as though it were consistent is worse than one
/// that refuses: a REPL showing a torn value is a wrong answer presented as a right one. Nothing on
/// the target can fix this -- the target does not know which words belong together -- so the host
/// must either read something it knows is single-word, or read twice and compare, or say plainly
/// that the value was in motion.
///
/// **A write may not be what the program reads next.** On a compiled tier the app may hold the
/// location in a register across the write, so the store lands in memory and the program keeps using
/// the stale copy. That is a property of the app's code, not of this op.
///
/// Both are reasons for a host to be careful, not reasons for a target to refuse: the alternative to
/// an inexact live read is halting a controller, which for a machine that is actually running
/// something is the more expensive of the two.
pub mod live {
    /// Host -> target: read target memory WITHOUT stopping the running app. Payload =
    /// `addr(u32 LE) | len(u16 LE)`. Answered by [`LIVE_DATA`]. Served both while an app runs
    /// (the point of the op) and while the target is serving, so a host gets the same answer
    /// either way -- which is what lets it tell an app that has stopped from an agent that has.
    pub const LIVE_READ: u8 = 0x60;
    /// Target -> host: `status(u8)` then, on [`status::OK`], the `len` bytes that were read.
    /// A nonzero status carries no bytes.
    pub const LIVE_DATA: u8 = 0x61;
    /// Host -> target: write target memory WITHOUT stopping the running app. Payload =
    /// `addr(u32 LE) | bytes[tail]`. Answered by [`LIVE_WROTE`].
    pub const LIVE_WRITE: u8 = 0x62;
    /// Target -> host: `status(u8) | written(u16 LE)` -- the byte count written, 0 on any nonzero
    /// status. A partial write never happens: the whole span is checked before the first byte.
    pub const LIVE_WROTE: u8 = 0x63;

    /// Why a [`LIVE_READ`] or [`LIVE_WRITE`] was refused. Byte 0 of [`LIVE_DATA`] / [`LIVE_WROTE`].
    ///
    /// A refusal is per-request and in-band, distinct from [`lamella_wire::msg::ERROR`]: the op IS
    /// implemented, and the target is saying this particular address or length is not one it will
    /// touch. The two failures need different repairs -- a different firmware versus a different
    /// address -- so they are different answers.
    pub mod status {
        /// The request was served.
        pub const OK: u8 = 0;
        /// This firmware declares no live window, so it carries no agent. A build that never calls
        /// [`crate::set_live_window`] answers every request this way rather than dereferencing an
        /// address a host asked for -- the difference between a refusal and a bus fault.
        pub const NO_WINDOW: u8 = 1;
        /// The requested span is not entirely inside the declared window.
        pub const OUT_OF_WINDOW: u8 = 2;
        /// The payload is malformed, the length is zero, or a read exceeds [`super::MAX_READ`].
        pub const BAD_REQUEST: u8 = 3;
    }

    /// The most bytes one [`LIVE_READ`] may ask for.
    ///
    /// The bound is not about buffer space; it is about the app. Servicing a read is time the
    /// deployed program is not running, and that time is proportional to the length, so bounding the
    /// length is the only way the target bounds the stall it imposes on a program it is supposed to
    /// be leaving alone. A host inspecting a variable needs a handful of bytes; one that wants a
    /// region asks repeatedly and lets the app run in between.
    pub const MAX_READ: usize = 256;

    /// Whether `msg_type` is one of this range's REQUESTS (the two a target serves).
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
    /// This file's own text. Read rather than reasoned about, for the same reason [`op_numbers`]
    /// reads it: the mistake is one line that looks correct everywhere it is reviewed.
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

/// That no two message types claim the same byte.
///
/// # Why this is a test and not a convention
///
/// The ranges are grouped by family and allocated by whoever adds a feature, months apart, and a
/// collision is not a compile error -- two names for one byte is legal Rust. **It is also not visible in
/// review**: the tables are hundreds of lines apart, a new op is one line, and the obvious next number
/// after a family's last one is frequently already taken by a family that grew into the gap. This check
/// exists because that has now been proposed once.
#[cfg(test)]
mod op_numbers {
    use lamella_wire::msg;

    use super::deploy;

    /// This file's own text, so the check reads the TABLE rather than a copy of it.
    ///
    /// A list maintained by hand cannot catch the allocation that forgot to update the list, which is
    /// the only mistake worth catching here.
    const SOURCE: &str = include_str!("lib.rs");

    /// The modules that allocate MESSAGE TYPES.
    ///
    /// `debug::val` and `debug::reason` are deliberately absent: they are different namespaces -- a
    /// value's kind, a stop reason -- which legitimately reuse the same small numbers, and folding them
    /// in here would report collisions that are not collisions.
    const OP_MODULES: [&str; 7] =
        ["repl", "debug", "profile", "telemetry", "deploy", "bundle", "live"];

    /// Every message type this file allocates, as its name and byte.
    fn allocated() -> alloc::vec::Vec<(&'static str, u8)> {
        let mut found = alloc::vec::Vec::new();
        let mut module = "";
        for line in SOURCE.lines() {
            let text = line.trim();
            if let Some(rest) = text.strip_prefix("pub mod ") {
                module = rest.trim_end_matches('{').trim();
            }
            if !OP_MODULES.contains(&module) {
                continue;
            }
            let Some(rest) = text.strip_prefix("pub const ") else { continue };
            let Some((name, value)) = rest.split_once(": u8 = ") else { continue };
            let literal = value
                .split_once(';')
                .unwrap_or_else(|| panic!("{name}: no terminating semicolon"))
                .0
                .trim();
            let byte = literal
                .strip_prefix("0x")
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .unwrap_or_else(|| panic!("{name} = {literal}: not a hexadecimal byte"));
            found.push((name, byte));
        }
        found
    }

    #[test]
    fn no_two_message_types_claim_the_same_byte() {
        let ops = allocated();
        assert!(
            ops.len() >= 25,
            "only {} message types were extracted -- the reader is broken, not the table",
            ops.len()
        );
        for (index, (name, byte)) in ops.iter().enumerate() {
            for (other, other_byte) in ops.iter().skip(index + 1) {
                assert_ne!(byte, other_byte, "{name} and {other} both claim {byte:#04x}");
            }
        }
    }

    /// The core types belong to the transport rather than to any feature range, so a feature that
    /// allocated one of them would be answered as a handshake.
    #[test]
    fn no_feature_range_claims_a_core_type() {
        let core = [msg::HELLO, msg::HELLO_ACK, msg::NAK, msg::ERROR, msg::PING, msg::PONG];
        for (name, byte) in allocated() {
            assert!(!core.contains(&byte), "{name} claims the core type {byte:#04x}");
        }
    }

    /// **The block this family lives in is FULL**, which is a fact worth failing on rather than
    /// discovering: the next op in it has to go somewhere else, and the cheapest moment to know that is
    /// before someone picks the next number.
    #[test]
    fn the_run_and_deploy_block_has_no_room_left() {
        let taken: alloc::vec::Vec<u8> =
            allocated().into_iter().map(|(_, byte)| byte).filter(|byte| (0x20..=0x2f).contains(byte)).collect();
        assert_eq!(taken.len(), 16, "the 0x20 block is full; a new op here needs a new range");
        assert!(taken.contains(&deploy::DEPLOY_CHUNK));
    }
}

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// The program's exit code (its `Main` return, 0 if none, 70 on an unhandled trap).
    pub exit: i32,
    /// The program's captured console (`Console.Out`) output.
    pub stdout: String,
}

impl RunResult {
    /// `exit(i32 LE) | stdout(UTF-8)`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(4 + self.stdout.len());
        payload.extend_from_slice(&self.exit.to_le_bytes());
        payload.extend_from_slice(self.stdout.as_bytes());
        payload
    }

    /// Decode a [`repl::RUN_RESULT`] payload.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            exit: i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
            stdout: String::from_utf8_lossy(&payload[4..]).into_owned(),
        })
    }
}

/// Run a program assembly against `corlib_bytes`, capturing its console output + exit code. This is the
/// runner's actual work -- the host reference runner, the browser runner, and the device firmware all call
/// it. A bad assembly / load failure is reported as exit -1 with the reason in `stdout`; an unhandled
/// trap is exit 70 (matching the interpreter's abort convention).
#[must_use]
pub fn run_program(corlib_bytes: &[u8], program_bytes: &[u8]) -> RunResult {
    #[cfg(any(feature = "baked-image", feature = "corlib-lazy"))]
    let (corlib_bytes, program_bytes): (&'static [u8], &'static [u8]) = (
        Box::leak(corlib_bytes.to_vec().into_boxed_slice()),
        Box::leak(program_bytes.to_vec().into_boxed_slice()),
    );
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

/// Records the resident corlib a serve path was handed, so the advertisement can describe it.
///
/// Called at the entry of every serve path that answers a `HELLO`, which is what makes the identity
/// correct on the FIRST frame of a session -- a `HELLO` is often the first frame, and an identity that
/// gained the corlib only afterwards would advertise two different surfaces for one firmware.
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
    RESIDENT_CORLIB_PRESENT.store(true, Ordering::Relaxed);
}

/// The resident surface's content hash: the intrinsic registry's fingerprint, continued over the
/// resident corlib's digest when there is one.
///
/// One continued fold rather than two schemes stitched together -- the registry's fingerprint is
/// FNV-1a, so this is the same walk carried on. A target with no resident corlib reports the
/// fingerprint unchanged, which is what every such firmware reported before this existed.
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
/// `board_model` in the low half of the first word; the IDCODE and device-id whole. Statics
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
/// [`lamella_wire::board_model`] (0 for a custom board), the chip's SW-DP IDCODE, and the
/// vendor device-id register value the firmware read from its own silicon (each 0 =
/// unknown). Call once at boot, before serving.
#[cfg(feature = "baked-image")]
pub fn set_board_identity(board_model: u16, chip_idcode: u32, chip_devid: u32) {
    use core::sync::atomic::Ordering;
    BOARD_MODEL.store(u32::from(board_model), Ordering::Relaxed);
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
    lamella_wire::Capabilities(base.0 | lamella_wire::Capabilities::DEBUG_ATTACH | live)
}

/// This build's resident-profile identity: the intrinsic-ABI level + the resident surface's content
/// hash + the surface's name, mostly derived in `intrinsic_registry` from the one feature set that
/// shapes the registry.
///
/// **The surface hash folds in a resident corlib's contents when the target holds one** (see
/// [`resident_surface_hash`]), which this doc said a Tier-2 target would do and which nothing did until
/// there was a resident corlib to fold.
#[cfg(feature = "baked-image")]
fn profile_identity() -> lamella_wire::ProfileIdentity {
    use core::sync::atomic::Ordering;
    use lamella_cil_runtime::intrinsic_registry;
    lamella_wire::ProfileIdentity::new(
        intrinsic_registry::INTRINSIC_ABI,
        resident_surface_hash(),
        intrinsic_registry::profile_name(),
    )
    .with_chip(
        BOARD_MODEL.load(Ordering::Relaxed) as u16,
        CHIP_IDCODE.load(Ordering::Relaxed),
        CHIP_DEVID.load(Ordering::Relaxed),
    )
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
        Some(hello) => match target_respond(&hello, range, caps) {
            Ok(mut ack) => {
                ack.profile = Some(profile_identity());
                transport.send(msg::HELLO_ACK, frame.seq, &ack.encode())
            }
            Err(nak) => transport.send(msg::NAK, frame.seq, &nak.encode()),
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
    /// A mid-run `HELLO` reclaimed the target (already answered): the session is over.
    Reclaimed,
    /// A mid-run [`debug::DBG_DETACH`] (already acked): the session is over.
    Detached,
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

#[cfg(feature = "baked-image")]
fn send_stopped(
    transport: &mut impl Transport,
    seq: u16,
    why: u8,
    location: (u32, u32),
    tail: Option<&RunResult>,
) -> Result<(), TransportError> {
    let mut payload = Vec::with_capacity(9);
    payload.push(why);
    payload.extend_from_slice(&location.0.to_le_bytes());
    payload.extend_from_slice(&location.1.to_le_bytes());
    if let Some(result) = tail {
        payload.extend_from_slice(&result.encode());
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
    let empty = alloc::vec![0u8, 0u8];
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
    let children = session.expand(vm, &value);
    let mut payload = Vec::new();
    payload.extend_from_slice(&(children.len() as u16).to_le_bytes());
    for child in children {
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

/// Sends whatever console output the program has produced since `sent`, as a [`debug::EVT_OUTPUT`]
/// frame, and advances `sent` past what went out.
///
/// The delta is taken from the VM's own output buffer rather than a tap, because the buffer IS the
/// record and a cursor over it cannot lose a write or double-send one -- whereas a tap is a
/// `fn` pointer that cannot capture this transport, and a side buffer for it would be a second
/// copy of the same bytes with its own overflow question.
///
/// A TRAILING HIGH SURROGATE IS HELD BACK, so a frame never carries half of a pair: the host can
/// then decode each frame on its own. It costs one comparison here and saves every host a
/// cross-frame joining rule.
///
/// Nothing is sent when there is nothing new, so an idle or silent program adds no wire traffic.
#[cfg(feature = "baked-image")]
fn stream_output(
    transport: &mut impl Transport,
    vm: &Vm,
    sent: &mut usize,
) -> Result<(), TransportError> {
    let output = vm.output();
    let mut end = output.len();
    if end <= *sent {
        return Ok(());
    }
    if matches!(output[end - 1], 0xD800..=0xDBFF) {
        end -= 1;
        if end <= *sent {
            return Ok(());
        }
    }
    let text = String::from_utf16_lossy(&output[*sent..end]);
    *sent = end;
    transport.send(debug::EVT_OUTPUT, 0, text.as_bytes())
}

/// Run until a breakpoint, completion, a trap, or a [`debug::DBG_PAUSE`]: bounded bursts
/// of steps with a wire poll between bursts, so a running target stays pause-able.
/// A mid-run `HELLO` is answered with `caps` and ends the run ([`RunStop::Reclaimed`]);
/// a mid-run detach is acked and ends it ([`RunStop::Detached`]) -- without these, a
/// resume over a non-terminating program would leave the Lamella Link permanently deaf
/// on every carrier.
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
    let mut sent = vm.output().len();
    loop {
        for _ in 0..RUN_SERVICE_STEPS {
            match session.step(module, vm) {
                Ok(Status::Done(value)) => return Ok(RunStop::Done(run_result_of(vm, &value))),
                Err(trap) => {
                    return Ok(RunStop::Trap(RunResult {
                        exit: 70,
                        stdout: {
                            let mut text = String::from_utf16_lossy(vm.output());
                            text.push_str(&format!("TRAP: {trap:?}"));
                            text
                        },
                    }));
                }
                Ok(Status::Running | Status::Paused) => {
                    if session.is_at_breakpoint() {
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
                    return Ok(RunStop::Trap(RunResult {
                        exit: 70,
                        stdout: format!(
                            "{}{what} needs the scheduler, which a debug session does not run: \
                             attach steps ONE thread. Deploy and run it (DEPLOY_RUN) to use threads.",
                            String::from_utf16_lossy(vm.output())
                        ),
                    }));
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
        lamella_wire::msg::HELLO => {
            hello_reply_caps(transport, &frame, caps)?;
            return Ok(Some(RunStop::Reclaimed));
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
    use lamella_cil_runtime::{Session, Status};

    let (module, entry) = match load_deployed(image, corlib) {
        Ok(booted) => booted,
        Err(why) => {
            let result = failure(&why);
            return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
        }
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
            let result = failure(&format!("static constructor: {trap:?}"));
            return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
        }
    };
    let mut session = match Session::new(&module, entry, Vec::new()) {
        Ok(session) => session,
        Err(trap) => {
            let result = failure(&format!("session: {trap:?}"));
            return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
        }
    };
    let mut at_reported_breakpoint = false;
    send_stopped(transport, image_seq, reason::ENTRY, debug_location(&session), None)?;

    loop {
        let Some(frame) = transport.poll()? else {
            continue;
        };
        match frame.msg_type {
            debug::DBG_STEP => match session.step(&module, &mut vm) {
                Ok(Status::Done(value)) => {
                    let result = run_result_of(&vm, &value);
                    return send_stopped(transport, frame.seq, reason::DONE, (0, 0), Some(&result));
                }
                Err(trap) => {
                    let result = RunResult {
                        exit: 70,
                        stdout: format!("{}TRAP: {trap:?}", String::from_utf16_lossy(vm.output())),
                    };
                    return send_stopped(transport, frame.seq, reason::TRAP, (0, 0), Some(&result));
                }
                Ok(Status::Running | Status::Paused) => {
                    at_reported_breakpoint = session.is_at_breakpoint();
                    let why = if at_reported_breakpoint { reason::BREAKPOINT } else { reason::STEP };
                    send_stopped(transport, frame.seq, why, debug_location(&session), None)?;
                }
            },
            debug::DBG_RESUME => {
                if at_reported_breakpoint {
                    at_reported_breakpoint = false;
                    match session.step(&module, &mut vm) {
                        Ok(Status::Done(value)) => {
                            let result = run_result_of(&vm, &value);
                            return send_stopped(transport, frame.seq, reason::DONE, (0, 0), Some(&result));
                        }
                        Err(trap) => {
                            let result = RunResult {
                                exit: 70,
                                stdout: format!(
                                    "{}TRAP: {trap:?}",
                                    String::from_utf16_lossy(vm.output())
                                ),
                            };
                            return send_stopped(transport, frame.seq, reason::TRAP, (0, 0), Some(&result));
                        }
                        Ok(_) => {}
                    }
                }
                match run_until_stop(transport, &module, &mut vm, &mut session, caps)? {
                    RunStop::Breakpoint => {
                        at_reported_breakpoint = true;
                        send_stopped(
                            transport,
                            frame.seq,
                            reason::BREAKPOINT,
                            debug_location(&session),
                            None,
                        )?;
                    }
                    RunStop::Paused => {
                        send_stopped(transport, frame.seq, reason::PAUSED, debug_location(&session), None)?;
                    }
                    RunStop::Done(result) => {
                        return send_stopped(transport, frame.seq, reason::DONE, (0, 0), Some(&result));
                    }
                    RunStop::Trap(result) => {
                        return send_stopped(transport, frame.seq, reason::TRAP, (0, 0), Some(&result));
                    }
                    RunStop::Reclaimed | RunStop::Detached => return Ok(()),
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
    run_image_serviced(image, None, configure, &mut || {})
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
    service: &mut dyn FnMut(),
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
    service: &mut dyn FnMut(),
) -> RunResult {
    let (module, entry) = match load_deployed(image, corlib) {
        Ok(booted) => booted,
        Err(why) => return failure(&why),
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
    let Ok(entry) = lamella_cil_runtime::boot_baked(&module, &mut vm, entry) else {
        return RunResult { exit: 70, stdout: String::from_utf16_lossy(vm.output()) };
    };
    let outcome = lamella_cil_runtime::run_serviced(&module, &mut vm, entry, Vec::new(), service);
    let exit = match outcome {
        Ok(Some(Value::Int32(code))) => code,
        Ok(_) => 0,
        Err(_) => 70,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

/// Serve one pending request on a PE-less BAKED-IMAGE target: a `HELLO` gets a `HELLO_ACK`
/// advertising `Capabilities::BAKED_IMAGE` (or a `NAK` on version mismatch), and a
/// [`repl::RUN_IMAGE`] boots + runs the received image and replies with [`repl::RUN_RESULT`].
/// Returns whether a frame was handled. A device firmware's main loop is this call in a
/// loop, plus its own storage reset between evaluations (nothing survives a request by
/// design -- the host-stateless REPL re-sends whole sessions).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_baked(transport: &mut impl Transport) -> Result<bool, TransportError> {
    serve_one_baked_with(transport, &mut |_vm| {})
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
) -> Result<bool, TransportError> {
    serve_one_baked_with_residence(transport, configure, &mut LeakEachImage)
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
) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    serve_frame_baked(transport, frame, None, configure, residence)?;
    Ok(true)
}

/// Handle one already-polled frame on a baked-image target: `HELLO` -> `HELLO_ACK`,
/// `RUN_IMAGE` -> run-from-RAM + `RUN_RESULT`, `DBG_IMAGE` -> a debug session. Anything
/// else (e.g. `RUN_PROGRAM`: this target is PE-less) is consumed -- the host learns the
/// supported surface from the `HELLO_ACK` capabilities. Split out so a deploy-capable
/// serve loop can dispatch the deploy messages itself and delegate the rest here.
#[cfg(feature = "baked-image")]
fn serve_frame_baked(
    transport: &mut impl Transport,
    frame: lamella_wire::Frame,
    corlib: Option<&'static [u8]>,
    configure: &mut dyn FnMut(&mut Vm),
    residence: &mut dyn ImageResidence,
) -> Result<(), TransportError> {
    use lamella_wire::msg;
    note_resident_corlib(corlib);
    match frame.msg_type {
        msg::HELLO => hello_reply(transport, &frame)?,
        repl::RUN_IMAGE => {
            let result = match residence.admit(frame.payload) {
                Some(image) => run_image_static(image, corlib, configure, &mut || {
                    let _ = transport.poll();
                }),
                None => failure("image does not fit this target's reserved image buffer"),
            };
            transport.send(repl::RUN_RESULT, frame.seq, &result.encode())?;
        }
        debug::DBG_IMAGE => {
            let Some(image) = residence.admit(frame.payload) else {
                let payload = lamella_wire::error::unknown_message_type(debug::DBG_IMAGE);
                transport.send(msg::ERROR, frame.seq, &payload)?;
                return Ok(());
            };
            run_debug_session_static(transport, image, corlib, frame.seq, serve_caps(), configure)?;
        }
        profile::GET_PROFILE => {
            let manifest = lamella_wire::ProfileManifest {
                identity: profile_identity(),
                intrinsic_ids: lamella_cil_runtime::intrinsic_registry::registry_ids().collect(),
            };
            transport.send(profile::PROFILE_MANIFEST, frame.seq, &manifest.encode())?;
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
) -> Result<Served, TransportError> {
    serve_one_deploy_with(transport, flash, &mut |_vm| {}, None)
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
) -> Result<Served, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(Served::Nothing);
    };
    serve_deploy_frame(transport, frame, flash, configure, winc)
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
    mut winc: Option<&mut dyn WincFlasher>,
) -> Result<Served, TransportError> {
    note_resident_corlib(flash.resident_corlib());
    match frame.msg_type {
        deploy::DEPLOY_IMAGE => {
            let ok = flash.program(&frame.payload);
            transport.send(deploy::DEPLOY_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::DEPLOY_CLEAR => {
            flash.erase();
            transport.send(deploy::DEPLOY_RESULT, frame.seq, &[1])?;
        }
        deploy::DEPLOY_CHUNK => {
            let payload = &frame.payload;
            let ok = if payload.len() >= 8 {
                let offset = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                let total = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                flash.program_chunk(offset, &payload[8..], total)
            } else {
                false
            };
            transport.send(deploy::DEPLOY_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::WINC_FW_START => {
            let payload = &frame.payload;
            let ok = match (payload.len() >= 8, winc.as_deref_mut()) {
                (true, Some(flasher)) => {
                    let offset =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    let total =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                    flasher.begin(offset, total)
                }
                _ => false,
            };
            transport.send(deploy::WINC_FW_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::WINC_FW_CHUNK => {
            let payload = &frame.payload;
            let ok = match (payload.len() >= 4, winc.as_deref_mut()) {
                (true, Some(flasher)) => {
                    let offset =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    flasher.program(offset, &payload[4..])
                }
                _ => false,
            };
            transport.send(deploy::WINC_FW_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::WINC_FW_END => {
            let ok = match winc.as_deref_mut() {
                Some(flasher) => flasher.finish(),
                None => false,
            };
            transport.send(deploy::WINC_FW_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::DEPLOY_STATUS => {
            let (present, checksum) =
                match lamella_cil_runtime::verified_image_checksum(flash.image_slice()) {
                    Some(sum) => (1u8, sum),
                    None => (0u8, 0u64),
                };
            let mut payload = [0u8; 9];
            payload[0] = present;
            payload[1..].copy_from_slice(&checksum.to_le_bytes());
            transport.send(deploy::DEPLOY_STATUS_RESULT, frame.seq, &payload)?;
        }
        deploy::DEPLOY_RUN => {
            return Ok(Served::RunRequested);
        }
        lamella_wire::msg::HELLO => hello_reply_caps(transport, &frame, deploy_caps())?,
        live::LIVE_READ | live::LIVE_WRITE => {
            serve_live_frame(transport, &frame, live_window(), &mut TargetMemory)?;
        }
        debug::DBG_ATTACH => {
            run_debug_session_static(
                transport,
                flash.image_slice(),
                flash.resident_corlib(),
                frame.seq,
                deploy_caps(),
                configure,
            )?;
        }
        _ => serve_frame_baked(transport, frame, flash.resident_corlib(), configure, &mut LeakEachImage)?,
    }
    Ok(Served::Handled)
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
) -> Result<Served, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(Served::Nothing);
    };
    note_resident_corlib(corlib);
    match frame.msg_type {
        repl::REPL_OPEN | repl::REPL_DELTA | repl::REPL_CLOSE | repl::REPL_PING => {
            serve_repl_frame(transport, frame, session, corlib, configure)?;
            Ok(Served::Handled)
        }
        repl::REPL_RESET => {
            serve_repl_frame(transport, frame, session, corlib, configure)?;
            Ok(Served::ResetRequested)
        }
        lamella_wire::msg::HELLO => {
            hello_reply_caps(transport, &frame, deploy_repl_caps())?;
            Ok(Served::Handled)
        }
        _ => serve_deploy_frame(transport, frame, flash, configure, winc),
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
            return Ok(completed(
                transport,
                RunResult {
                    exit: 70,
                    stdout: format!(
                        "{}BOOT TRAP (static constructor): {trap:?}",
                        String::from_utf16_lossy(vm.output())
                    ),
                },
            ));
        }
    };
    let mut carrier: Result<(), TransportError> = Ok(());
    let outcome = lamella_cil_runtime::run_interruptible(module, &mut vm, entry, Vec::new(), &mut || {
        match transport.poll() {
            Ok(Some(frame)) if frame.msg_type == msg::HELLO => {
                carrier = hello_reply_caps(transport, &frame, deploy_caps());
                false
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
    match outcome {
        Ok(lamella_cil_runtime::Ran::Finished(value)) => {
            Ok(completed(transport, run_result_of(&vm, &value)))
        }
        Ok(lamella_cil_runtime::Ran::Interrupted) => Ok(Deployed::Interrupted),
        Err(trap) => Ok(completed(
            transport,
            RunResult {
                exit: 70,
                stdout: format!("{}TRAP: {trap:?}", String::from_utf16_lossy(vm.output())),
            },
        )),
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
/// It lives in the runner rather than in each firmware because it was written in each firmware and
/// **nine of the ten got it wrong.** One board sent the frame; the rest either printed the app's
/// output to their raw UART as human text -- which is not a wire frame and reaches no host driver --
/// or dropped the result on the floor. `DEPLOY_RUN` therefore never delivered a `RUN_RESULT` on
/// almost every board in the tree, and the one place a host had to wait was a 120-second timeout.
/// A firmware cannot forget a step it does not perform.
///
/// A carrier fault here is deliberately DROPPED rather than propagated. The run's outcome is the
/// return value, and it already happened; failing to announce it must not turn a completed run into
/// an error, nor lose the exit code the caller is about to act on.
#[cfg(feature = "baked-image")]
fn completed(transport: &mut impl Transport, result: RunResult) -> Deployed {
    let _ = transport.send(repl::RUN_RESULT, 0, &result.encode());
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

        let (module, name_index, type_index, first_delta_asm) = if corlib.is_some() {
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

        let root_slot = module.static_field_defaults().len();
        let mut storage = module.static_field_defaults().to_vec();
        storage.push(Value::Null);

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

        let new_fields = info.new_field_defaults.len() as u16;
        if !info.new_field_defaults.is_empty() {
            self.vm
                .heap_mut()
                .grow_instance(self.instance, &info.new_field_defaults)
                .ok_or_else(|| {
                    SubmitError::NotLoaded(String::from("live __Repl instance could not be grown"))
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
    match Hello::decode(&frame.payload) {
        Some(hello) => match target_respond(&hello, range, repl_caps()) {
            Ok(ack) => {
                #[cfg(feature = "baked-image")]
                let ack = {
                    let mut ack = ack;
                    ack.profile = Some(profile_identity());
                    ack
                };
                transport.send(msg::HELLO_ACK, frame.seq, &ack.encode())
            }
            Err(nak) => transport.send(msg::NAK, frame.seq, &nak.encode()),
        },
        None => Ok(()),
    }
}

/// Decodes a [`repl::REPL_OPEN`] payload into `(heartbeat_ms, bootstrap_bytes)`, skipping the
/// RESERVED per-output safe-state config blob. A short / garbled header yields
/// heartbeat 0 and an empty bootstrap, so `open` then fails cleanly with a parse error rather than
/// mis-reading the tail.
#[cfg(feature = "repl-session")]
fn decode_repl_open(payload: &[u8]) -> (u32, &[u8]) {
    let Some(head) = payload.get(0..6) else {
        return (0, &[]);
    };
    let heartbeat_ms = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let config_len = u16::from_le_bytes([head[4], head[5]]) as usize;
    let bootstrap = payload.get(6 + config_len..).unwrap_or(&[]);
    (heartbeat_ms, bootstrap)
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
) -> Result<(), TransportError> {
    match frame.msg_type {
        repl::REPL_OPEN => {
            let (heartbeat_ms, bootstrap) = decode_repl_open(&frame.payload);
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
            let payload = match session {
                Some(state) => match state.submit(&frame.payload) {
                    Ok(outcome) => repl_delta_result_ok(&outcome),
                    Err(error) => repl_delta_result_err(error.status(), error.reason()),
                },
                None => repl_delta_result_err(1, "no open REPL session"),
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
        #[cfg(feature = "baked-image")]
        _ => serve_frame_baked(transport, frame, corlib, configure, &mut LeakEachImage)?,
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
) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    serve_repl_frame(transport, frame, session, corlib, configure)?;
    Ok(true)
}

/// The runner's request handler: if a [`repl::RUN_PROGRAM`] is pending, run it (against `corlib_bytes`)
/// and reply with a [`repl::RUN_RESULT`] on the same seq. Returns whether a request was handled. A
/// device firmware's main loop is this call in a loop.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn serve_one(transport: &mut impl Transport, corlib_bytes: &[u8]) -> Result<bool, TransportError> {
    if let Some(frame) = transport.poll()? {
        if frame.msg_type == repl::RUN_PROGRAM {
            let result = run_program(corlib_bytes, &frame.payload);
            transport.send(repl::RUN_RESULT, frame.seq, &result.encode())?;
            return Ok(true);
        }
        #[cfg(feature = "baked-image")]
        if frame.msg_type == repl::RUN_IMAGE {
            let result = run_image(frame.payload);
            transport.send(repl::RUN_RESULT, frame.seq, &result.encode())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Host driver: send a compiled `program` to the target for execution.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_program(transport: &mut impl Transport, seq: u16, program: &[u8]) -> Result<(), TransportError> {
    transport.send(repl::RUN_PROGRAM, seq, program)
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

/// Host driver: send a baked `.lmli` `image` to the target for execution. Unconditional (no
/// interpreter feature needed to DRIVE a device): the target answers with the same
/// [`repl::RUN_RESULT`] that [`try_recv_result`] reads.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_image(transport: &mut impl Transport, seq: u16, image: &[u8]) -> Result<(), TransportError> {
    transport.send(repl::RUN_IMAGE, seq, image)
}

/// Host driver: poll for the [`repl::RUN_RESULT`] matching `seq` (non-blocking; `None` if not in yet).
///
/// `Ok(None)` means ONE thing -- no answer has arrived yet -- and the two other outcomes that would otherwise
/// share it now have their own. Both were indistinguishable from "keep waiting", so a caller polled
/// each to its deadline and reported a timeout: the most expensive reading, because it points at the
/// link when the link is fine.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier; [`TransportError::Refused`] when the target
/// answered [`lamella_wire::msg::ERROR`] for this sequence; [`TransportError::MalformedReply`] when
/// the result arrived at this sequence and did not decode.
pub fn try_recv_result(transport: &mut impl Transport, seq: u16) -> Result<Option<RunResult>, TransportError> {
    while let Some(frame) = transport.poll()? {
        if frame.msg_type == repl::RUN_RESULT && frame.seq == seq {
            return match RunResult::decode(&frame.payload) {
                Some(result) => Ok(Some(result)),
                None => Err(TransportError::MalformedReply { msg_type: frame.msg_type }),
            };
        }
        if frame.msg_type == lamella_wire::msg::ERROR && frame.seq == seq {
            return Err(TransportError::Refused {
                reason: frame.payload.first().copied().unwrap_or(0),
                msg_type: lamella_wire::error::refused_message_type(&frame.payload).unwrap_or(0),
            });
        }
    }
    Ok(None)
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
            assert!(deploy_caps_with(base, 0).has(Capabilities::DEBUG_ATTACH));
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

    #[test]
    fn run_result_round_trips() {
        let result = RunResult { exit: 7, stdout: "hi\n".to_string() };
        assert_eq!(RunResult::decode(&result.encode()), Some(result));
    }

    #[test]
    fn run_result_decode_rejects_a_short_payload() {
        assert_eq!(RunResult::decode(&[1, 2, 3]), None);
    }

    #[test]
    fn run_result_decode_tolerates_lossy_utf8() {
        let decoded = RunResult::decode(&[0, 0, 0, 0, 0xFF, 0xFE]).expect("decodes");
        assert_eq!(decoded.exit, 0);
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn run_image_round_trips_over_the_wire() {
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
        send_image(&mut driver, 9, &image).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one(&mut runner, &[]).unwrap(), "the runner handled a RUN_IMAGE");
        driver.feed(&runner.take_sent());

        let result = try_recv_result(&mut driver, 9).unwrap().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    /// Console output streams as a DELTA, never re-sent, and never split mid-character.
    ///
    /// The three ways this can be quietly wrong are a missed delta (output that never arrives), a
    /// re-sent one (the console repeats itself), and a chunk cut through a surrogate pair (the host
    /// decodes a replacement character for text the program did write). Each gets a row.
    #[cfg(feature = "baked-image")]
    #[test]
    fn console_output_streams_as_a_delta_and_never_splits_a_surrogate_pair() {
        use lamella_wire::{MemTransport, Transport};

        let mut vm = Vm::new();
        let mut host = MemTransport::new();
        let mut sent = 0usize;

        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert!(host.take_sent().is_empty(), "an empty delta must send nothing at all");

        vm.write(&"first\n".encode_utf16().collect::<Vec<u16>>());
        stream_output(&mut host, &vm, &mut sent).unwrap();
        let mut peer = MemTransport::new();
        peer.feed(&host.take_sent());
        let frame = peer.poll().unwrap().expect("an EVT_OUTPUT frame");
        assert_eq!(frame.msg_type, debug::EVT_OUTPUT);
        assert_eq!(String::from_utf8_lossy(&frame.payload), "first\n");

        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert!(host.take_sent().is_empty(), "an unchanged buffer must not be re-sent");

        vm.write(&"second\n".encode_utf16().collect::<Vec<u16>>());
        stream_output(&mut host, &vm, &mut sent).unwrap();
        let mut peer = MemTransport::new();
        peer.feed(&host.take_sent());
        let frame = peer.poll().unwrap().expect("a second EVT_OUTPUT frame");
        assert_eq!(String::from_utf8_lossy(&frame.payload), "second\n", "the delta, not the buffer");

        vm.write(&[0xD83D]);
        stream_output(&mut host, &vm, &mut sent).unwrap();
        assert!(host.take_sent().is_empty(), "a trailing lead surrogate must wait for its trail");

        vm.write(&[0xDE00]);
        stream_output(&mut host, &vm, &mut sent).unwrap();
        let mut peer = MemTransport::new();
        peer.feed(&host.take_sent());
        let frame = peer.poll().unwrap().expect("the completed pair");
        assert_eq!(frame.payload, vec![0xF0, 0x9F, 0x98, 0x80], "the pair must arrive as one character");
    }

    /// The THREE states `try_recv_result` used to collapse into `Ok(None)`, each fed as the frame a
    /// target really sends. Only the first may still be `Ok(None)`; the other two were polled to the
    /// caller's deadline and reported as timeouts, which points the reader at a link that is fine.
    #[test]
    fn try_recv_result_separates_nothing_yet_from_a_refusal_and_from_a_malformed_reply() {
        use lamella_wire::{MemTransport, error, msg};

        let mut quiet = MemTransport::new();
        assert_eq!(try_recv_result(&mut quiet, 4).unwrap(), None);

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        target.send(msg::ERROR, 4, &error::unknown_message_type(repl::RUN_IMAGE)).unwrap();
        driver.feed(&target.take_sent());
        assert_eq!(
            try_recv_result(&mut driver, 4),
            Err(TransportError::Refused {
                reason: error::UNKNOWN_MESSAGE_TYPE,
                msg_type: repl::RUN_IMAGE,
            })
        );

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        target.send(repl::RUN_RESULT, 4, &[1, 2, 3]).unwrap();
        driver.feed(&target.take_sent());
        assert_eq!(
            try_recv_result(&mut driver, 4),
            Err(TransportError::MalformedReply { msg_type: repl::RUN_RESULT })
        );

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        target.send(msg::ERROR, 5, &error::unknown_message_type(repl::RUN_IMAGE)).unwrap();
        driver.feed(&target.take_sent());
        assert_eq!(try_recv_result(&mut driver, 4).unwrap(), None);
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

        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE | Capabilities::REPL_RUN),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target handled the HELLO");
        driver.feed(&runner.take_sent());
        let ack_frame = driver.poll().unwrap().expect("a HELLO_ACK arrived");
        assert_eq!(ack_frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack_frame.payload).expect("decodes");
        assert_eq!(ack.chosen, PROTOCOL_VERSION);
        assert!(ack.caps.has(Capabilities::BAKED_IMAGE));

        send_image(&mut driver, 2, &image).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target ran the image");
        driver.feed(&runner.take_sent());
        let result = try_recv_result(&mut driver, 2).unwrap().expect("a result arrived");
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

        let asked = bundle::RUN_BUNDLE;
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        driver.send(asked, 11, b"a bundle this target cannot load").unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target handled the frame");
        driver.feed(&runner.take_sent());

        let reply = driver.poll().unwrap().expect("the target answered rather than going quiet");
        assert_eq!(reply.msg_type, msg::ERROR);
        assert_eq!(reply.seq, 11, "the refusal answers the frame that caused it");
        assert_eq!(
            error::refused_message_type(&reply.payload),
            Some(asked),
            "the refusal names the type it refused"
        );
    }

    /// **THE DEFECT THIS CLOSES, STATED AS A TEST: two firmwares differing only in the corlib they hold
    /// resident used to advertise the SAME surface hash.** A host cannot detect that difference any other
    /// way, and getting it wrong is silent -- a corlib declaring a seam the firmware compiled out still
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
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 12, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap());
        driver.feed(&runner.take_sent());
        let reply = driver.poll().unwrap().expect("a reply arrived");
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
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        driver.send(profile::GET_PROFILE, 2, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the HELLO was served");
        assert!(serve_one_baked(&mut runner).unwrap(), "the GET_PROFILE was served");
        driver.feed(&runner.take_sent());

        let ack = driver.poll().unwrap().expect("a HELLO_ACK");
        let identity = HelloAck::decode(&ack.payload)
            .expect("the ack decodes")
            .profile
            .expect("the ack advertises an identity");
        assert_eq!(identity.abi, intrinsic_registry::INTRINSIC_ABI);
        assert_eq!(identity.hash, resident_surface_hash());
        assert_eq!(identity.name(), intrinsic_registry::profile_name());

        let frame = driver.poll().unwrap().expect("a manifest reply");
        assert_eq!(frame.msg_type, profile::PROFILE_MANIFEST);
        let manifest = ProfileManifest::decode(&frame.payload).expect("the manifest decodes");
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
    fn a_hello_reclaims_a_debug_session_resumed_into_an_infinite_program() {
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
        target.feed(&driver.take_sent());

        run_debug_session_static(&mut target, image, None, 3, serve_caps(), &mut |_| {})
            .expect("the session ends instead of spinning forever");
        driver.feed(&target.take_sent());

        let stopped = driver.poll().unwrap().expect("the entry stop report");
        assert_eq!(stopped.msg_type, debug::EVT_STOPPED);
        let ack_frame = driver.poll().unwrap().expect("the reclaim HELLO_ACK");
        assert_eq!(ack_frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack_frame.payload).expect("the ack decodes");
        assert!(ack.caps.has(Capabilities::BAKED_IMAGE));
        assert!(driver.poll().unwrap().is_none(), "nothing else in flight");
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
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 2 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_DETACH, 2 + STEPS, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target served the steps");
        driver.feed(&runner.take_sent());
        let (why, _, _) = stopped(&driver.poll().unwrap().expect("entry stop"));
        assert_eq!(why, debug::reason::ENTRY);
        let mut loop_method = 0;
        let mut loop_offset = 0;
        for _ in 0..STEPS {
            let (why, method, offset) = stopped(&driver.poll().unwrap().expect("a step stop"));
            assert_eq!(why, debug::reason::STEP);
            loop_method = method;
            loop_offset = offset;
        }
        assert_eq!(driver.poll().unwrap().expect("phase-1 detach ack").msg_type, debug::DBG_ACK);

        let mut break_payload = Vec::new();
        break_payload.extend_from_slice(&1u16.to_le_bytes());
        break_payload.extend_from_slice(&loop_method.to_le_bytes());
        break_payload.extend_from_slice(&loop_offset.to_le_bytes());
        driver.send(debug::DBG_IMAGE, 4, &image).unwrap();
        driver.send(debug::DBG_RESUME, 5, &[]).unwrap();
        driver.send(debug::DBG_BREAK, 6, &break_payload).unwrap();
        driver.send(debug::DBG_DETACH, 7, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(
            serve_one_baked(&mut runner).unwrap(),
            "the session stops at the mid-run breakpoint instead of spinning forever"
        );
        driver.feed(&runner.take_sent());
        assert_eq!(
            stopped(&driver.poll().unwrap().expect("phase-2 entry stop")).0,
            debug::reason::ENTRY
        );

        let ack = driver.poll().unwrap().expect("the mid-run break ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);
        let (why, hit_method, hit_offset) = stopped(&driver.poll().unwrap().expect("breakpoint stop"));
        assert_eq!(why, debug::reason::BREAKPOINT, "the mid-run breakpoint fired");
        assert_eq!((hit_method, hit_offset), (loop_method, loop_offset));
        let detach = driver.poll().unwrap().expect("the detach ack");
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
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 2 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_LOCALS, 900, &0u16.to_le_bytes()).unwrap();
        driver.send(debug::DBG_LOCALS, 901, &1u16.to_le_bytes()).unwrap();
        driver.send(debug::DBG_LOCALS, 902, &9u16.to_le_bytes()).unwrap();
        driver.send(debug::DBG_DETACH, 903, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the locals session served");
        driver.feed(&runner.take_sent());
        let mut replies = Vec::new();
        while let Some(frame) = driver.poll().unwrap() {
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
            let mut payload = Vec::new();
            payload.extend_from_slice(&frame.to_le_bytes());
            payload.push(kind);
            payload.extend_from_slice(&slot.to_le_bytes());
            payload.push(path.len() as u8);
            for step in path {
                payload.extend_from_slice(&step.to_le_bytes());
            }
            payload
        }
        fn children(payload: &[u8]) -> Vec<(String, u8, Vec<u8>)> {
            let count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
            let mut at = 2;
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

        driver.send(debug::DBG_IMAGE, 20, &image).unwrap();
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 21 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_EXPAND, 950, &expand_request(1, 0, box_slot as u16, &[])).unwrap();
        driver.send(debug::DBG_EXPAND, 951, &expand_request(1, 0, arr_slot as u16, &[])).unwrap();
        driver.send(debug::DBG_EXPAND, 952, &expand_request(1, 0, pair_slot as u16, &[])).unwrap();
        driver.send(debug::DBG_EXPAND, 953, &expand_request(1, 0, box_slot as u16, &[0])).unwrap();
        driver.send(debug::DBG_EXPAND, 954, &expand_request(1, 0, 999, &[])).unwrap();
        driver.send(debug::DBG_DETACH, 955, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the expand session served");
        driver.feed(&runner.take_sent());
        let mut expansions = Vec::new();
        while let Some(frame) = driver.poll().unwrap() {
            if frame.msg_type == debug::DBG_CHILDREN {
                expansions.push(frame.payload);
            }
        }
        assert_eq!(expansions.len(), 5);
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

        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE | Capabilities::DEBUG_ATTACH),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut flash).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());
        let ack = driver.poll().unwrap().expect("a HELLO_ACK");
        assert_eq!(ack.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack.payload).expect("the ack decodes");
        assert!(ack.caps.has(Capabilities::DEBUG_ATTACH), "the deploy serve advertises attach");

        driver.send(debug::DBG_ATTACH, 2, &[]).unwrap();
        driver.send(debug::DBG_STEP, 3, &[]).unwrap();
        driver.send(debug::DBG_STACK, 4, &[]).unwrap();
        driver.send(debug::DBG_DETACH, 5, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut flash).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());

        let entry = driver.poll().unwrap().expect("an entry stop");
        assert_eq!(entry.msg_type, debug::EVT_STOPPED);
        assert_eq!(entry.payload[0], debug::reason::ENTRY);
        let step = driver.poll().unwrap().expect("a step stop");
        assert_eq!(step.payload[0], debug::reason::STEP);
        let frames = driver.poll().unwrap().expect("a stack reply");
        assert_eq!(frames.msg_type, debug::DBG_FRAMES);
        let ack = driver.poll().unwrap().expect("a detach ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);

        let mut empty = Deployed(&[0xFF; 64]);
        driver.send(debug::DBG_ATTACH, 6, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut empty).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());
        let stop = driver.poll().unwrap().expect("a trap stop");
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
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        driver.send(debug::DBG_STEP, 2, &[]).unwrap();
        driver.send(debug::DBG_STEP, 3, &[]).unwrap();
        driver.send(debug::DBG_STACK, 4, &[]).unwrap();
        driver.send(debug::DBG_DETACH, 5, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target served the debug session");
        driver.feed(&runner.take_sent());

        let entry_stop = driver.poll().unwrap().expect("an entry stop");
        let (why, entry_method, entry_offset) = stopped(&entry_stop);
        assert_eq!(why, debug::reason::ENTRY);
        let step1 = driver.poll().unwrap().expect("a step stop");
        let (why, method_1, offset_1) = stopped(&step1);
        assert_eq!(why, debug::reason::STEP);
        assert_eq!(method_1, entry_method);
        assert_ne!((method_1, offset_1), (entry_method, entry_offset), "the step advanced");
        let step2 = driver.poll().unwrap().expect("a second step stop");
        let (why, _, _) = stopped(&step2);
        assert_eq!(why, debug::reason::STEP);
        let frames = driver.poll().unwrap().expect("a stack reply");
        assert_eq!(frames.msg_type, debug::DBG_FRAMES);
        let count = u16::from_le_bytes(frames.payload[0..2].try_into().unwrap());
        assert!(count >= 1, "at least the entry frame");
        let top_method = u32::from_le_bytes(frames.payload[2..6].try_into().unwrap());
        assert_eq!(top_method, entry_method, "innermost frame first");
        let ack = driver.poll().unwrap().expect("a detach ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);

        let mut break_payload = Vec::new();
        break_payload.extend_from_slice(&1u16.to_le_bytes());
        break_payload.extend_from_slice(&method_1.to_le_bytes());
        break_payload.extend_from_slice(&offset_1.to_le_bytes());
        driver.send(debug::DBG_IMAGE, 6, &image).unwrap();
        driver.send(debug::DBG_BREAK, 7, &break_payload).unwrap();
        driver.send(debug::DBG_RESUME, 8, &[]).unwrap();
        driver.send(debug::DBG_RESUME, 9, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target served session B");
        driver.feed(&runner.take_sent());

        let (why, _, _) = stopped(&driver.poll().unwrap().expect("entry stop B"));
        assert_eq!(why, debug::reason::ENTRY);
        let ack = driver.poll().unwrap().expect("a breakpoint ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);
        let hit = driver.poll().unwrap().expect("a breakpoint stop");
        let (why, hit_method, hit_offset) = stopped(&hit);
        assert_eq!(why, debug::reason::BREAKPOINT);
        assert_eq!((hit_method, hit_offset), (method_1, offset_1));
        let done = driver.poll().unwrap().expect("a done stop");
        let (why, _, _) = stopped(&done);
        assert_eq!(why, debug::reason::DONE);
        let result = RunResult::decode(&done.payload[9..]).expect("a run result tail");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
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
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 2, &hello.encode()).unwrap();
        send_image(&mut driver, 3, &image).unwrap();
        runner.feed(&driver.take_sent());

        assert!(serve_one_baked(&mut runner).unwrap(), "the stale debug session served + ended");
        assert!(serve_one_baked(&mut runner).unwrap(), "the fresh RUN_IMAGE served");
        driver.feed(&runner.take_sent());

        let entry = driver.poll().unwrap().expect("the stale session's entry stop");
        assert_eq!(entry.msg_type, debug::EVT_STOPPED);
        let ack = driver.poll().unwrap().expect("the successor's HELLO_ACK");
        assert_eq!(ack.msg_type, msg::HELLO_ACK);
        let result = try_recv_result(&mut driver, 3).unwrap().expect("a fresh run result");
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
            driver.send(deploy::DEPLOY_CHUNK, seq, &payload).unwrap();
            runner.feed(&driver.take_sent());
            assert_eq!(
                serve_one_deploy(&mut runner, &mut sink).unwrap(),
                Served::Handled,
                "the target handled a chunk"
            );
            driver.feed(&runner.take_sent());
            let ack = driver.poll().unwrap().expect("a chunk ack");
            assert_eq!(ack.msg_type, deploy::DEPLOY_RESULT);
            assert_eq!(ack.payload, vec![1], "the chunk verified");

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

    #[cfg(feature = "baked-image")]
    #[test]
    fn winc_firmware_streams_through_the_flasher_hook() {
        use lamella_wire::MemTransport;

        let firmware: Vec<u8> = (0..5000u32).map(|i| (i % 241) as u8).collect();
        let base = 4096usize;
        let chunk_len = 1024usize;
        let mut sink = MockFlash::default();
        let mut flasher = MockWincFlasher::default();

        let mut exchange = |msg_type: u8, seq: u16, payload: &[u8], flasher: &mut MockWincFlasher| {
            let mut driver = MemTransport::new();
            let mut runner = MemTransport::new();
            driver.send(msg_type, seq, payload).unwrap();
            runner.feed(&driver.take_sent());
            assert_eq!(
                serve_one_deploy_with(
                    &mut runner,
                    &mut sink,
                    &mut |_vm| {},
                    Some(flasher as &mut dyn WincFlasher),
                )
                .unwrap(),
                Served::Handled,
            );
            driver.feed(&runner.take_sent());
            let ack = driver.poll().unwrap().expect("a WINC_FW_RESULT ack");
            assert_eq!(ack.msg_type, deploy::WINC_FW_RESULT);
            assert_eq!(ack.payload, vec![1], "the step succeeded");
        };

        let mut start = Vec::new();
        start.extend_from_slice(&(base as u32).to_le_bytes());
        start.extend_from_slice(&(firmware.len() as u32).to_le_bytes());
        exchange(deploy::WINC_FW_START, 1, &start, &mut flasher);
        assert_eq!(flasher.begun, Some((base, firmware.len())));

        let mut offset = 0;
        let mut seq = 2u16;
        while offset < firmware.len() {
            let end = (offset + chunk_len).min(firmware.len());
            let mut payload = Vec::new();
            payload.extend_from_slice(&((base + offset) as u32).to_le_bytes());
            payload.extend_from_slice(&firmware[offset..end]);
            exchange(deploy::WINC_FW_CHUNK, seq, &payload, &mut flasher);
            offset = end;
            seq += 1;
        }
        exchange(deploy::WINC_FW_END, seq, &[], &mut flasher);

        assert!(flasher.finished, "END reached finish");
        assert_eq!(&flasher.data[base..], &firmware[..], "the programmed image matches");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_target_without_a_winc_flasher_answers_not_ok() {
        use lamella_wire::MemTransport;

        let mut sink = MockFlash::default();
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        driver.send(deploy::WINC_FW_START, 1, &[0, 0, 0, 0, 16, 0, 0, 0]).unwrap();
        runner.feed(&driver.take_sent());
        assert_eq!(
            serve_one_deploy_with(&mut runner, &mut sink, &mut |_vm| {}, None).unwrap(),
            Served::Handled,
        );
        driver.feed(&runner.take_sent());
        let ack = driver.poll().unwrap().expect("a WINC_FW_RESULT ack");
        assert_eq!(ack.msg_type, deploy::WINC_FW_RESULT);
        assert_eq!(ack.payload, vec![0], "no flasher answers not-ok");
    }
}
