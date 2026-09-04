//! The message-type space: every type byte this protocol allocates, in ONE place. The byte is
//! divided into blocks by concern, each request beside its reply and each block sized with room to
//! grow; `0x00` and `0xFF` are permanently invalid, because `0xFF` is what erased flash reads as and
//! `0x00` what zeroed RAM reads as, and a decoder that accepted either would treat an unprogrammed
//! or uninitialized region as a message. [`ALL`] enumerates the space and [`BLOCKS`] divides it.

/// The blocks the type space is divided into, as `(name, first, last)` inclusive.
///
/// Each block is a CONCERN rather than a capability: membership says where an op is filed, never
/// that a target implementing one op in a block implements the rest of it. Two ops in the debug
/// block -- the stop event and the output event -- are required of every target that executes
/// anything at all, including one that advertises no debug capability.
pub const BLOCKS: &[(&str, u8, u8)] = &[
    ("SESSION", 0x01, 0x0F),
    ("DEBUG", 0x10, 0x2F),
    ("LOAD", 0x30, 0x3F),
    ("DEPLOY", 0x40, 0x4F),
    ("PROFILE", 0x50, 0x5F),
    ("REPL", 0x60, 0x6F),
    ("TELEMETRY", 0x70, 0x7F),
    ("LIVE", 0x80, 0x8F),
    ("DEVICE", 0x90, 0x9F),
    ("EXTENSION", 0xF0, 0xFE),
];


/// Host -> target: a [`super::Hello`] -- the version range and capabilities the host offers.
pub const HELLO: u8 = 0x01;
/// Target -> host: a [`super::HelloAck`] -- the chosen version, the target's capabilities, and the
/// target's [`super::TargetIdentity`].
pub const HELLO_ACK: u8 = 0x02;
/// Target -> host: a [`super::HelloNak`] -- no version overlap, carrying the target's own range so
/// the host can say which side has to move.
///
/// It answers a [`HELLO`] and nothing else, and no session begins. [`ERROR`] is the refusal that
/// happens INSIDE a session that negotiated fine, about one frame, leaving the rest usable. The two
/// were one name once, and one name for two answers is what let two implementations disagree about
/// which they were sending.
pub const HELLO_NAK: u8 = 0x03;
/// Either way: one frame was refused. Payload = a reason byte and a reason-dependent tail
/// (see [`super::error`]).
pub const ERROR: u8 = 0x04;
/// Liveness probe. Empty payload.
pub const PING: u8 = 0x05;
/// Liveness reply. Empty payload.
pub const PONG: u8 = 0x06;
/// Target -> host, unsolicited: the session this carrier held has been taken by another one.
/// Payload = the new holder's [`super::session::ChannelClass`] as one byte.
///
/// # Why the loser is told rather than left to find out
///
/// A target that can be reached on several carriers at once has one session to give, and a carrier
/// can lose it while its host believes it still has it. Without this, that host discovers the loss
/// from its next operation being refused -- or, if it was only listening, never discovers it at all
/// and simply reports a target that went quiet.
///
/// Naming the new holder's CLASS is the useful part: *a cable took it* tells a remote user that
/// somebody is at the board, which is a situation to wait out rather than a fault to investigate.
pub const SESSION_REVOKED: u8 = 0x07;
/// Host -> target: acknowledge a latched link supervisor, so a target that stopped its outputs
/// because the host went quiet can be told to start again. Empty payload; the tail is RESERVED.
///
/// RESERVED: the number is allocated so that nothing else takes it, and a target that does not
/// implement it refuses it by name. It is a SESSION op rather than a REPL one because a target can arm a link supervisor
/// without holding an interpreter, a heap or a loaded module -- and a board that can latch has to be
/// reachable to be unlatched. [`SESSION_REVOKED`] one byte earlier is the same shape: a statement
/// about the link rather than about what runs over it.
pub const LINK_ACK: u8 = 0x08;
/// Target -> host: what a [`LINK_ACK`] did. RESERVED alongside it.
pub const LINK_ACKED: u8 = 0x09;


/// Target -> host: a command that changes no execution state completed.
pub const DBG_ACK: u8 = 0x10;
/// Host -> target: run until a breakpoint, completion, a trap, or a [`DBG_PAUSE`]. Answered by
/// [`EVT_STOPPED`] when execution stops.
pub const DBG_RESUME: u8 = 0x11;
/// Host -> target: stop a running execution at the next poll boundary. Answered by [`EVT_STOPPED`]
/// with reason [`stop_reason::PAUSED`]; an already-halted target acknowledges the same way.
///
/// Legal against a program THIS host did not start, which is what lets a host connect to a board
/// that has been running for hours and take a look at it.
pub const DBG_PAUSE: u8 = 0x12;
/// Host -> target: step. Payload = `mode(u8)` (see [`step_mode`]). Answered by [`EVT_STOPPED`].
///
/// The depth predicate for step-over and step-out is the target's: it is one more condition in the
/// loop [`DBG_RESUME`] already runs, and the target replies once. A host that synthesizes the same
/// two verbs out of repeated single steps pays a round trip per instruction, which on a serial
/// carrier is the difference between a keypress and a minute.
pub const DBG_STEP: u8 = 0x13;
/// Host -> target: end the debug session; the target discards it and returns to serving. Answered
/// by [`DBG_ACK`].
pub const DBG_DETACH: u8 = 0x14;
/// Host -> target: evaluate a compiled fragment against one frame of the paused program. Payload =
/// `frame(u16 LE)` then the fragment. Answered by [`DBG_EVAL_RESULT`].
///
/// RESERVED, and refused by name where it is not implemented. It is the delta-loading
/// mechanism the REPL block already provides, pointed at the paused application's own interpreter
/// rather than at a session's -- which is what makes a watch expression read the value the program
/// actually holds instead of one from a separate world.
pub const DBG_EVAL: u8 = 0x15;
/// Target -> host: the evaluation's result. Payload = `status(u8)` then the rendered value.
/// RESERVED alongside [`DBG_EVAL`].
pub const DBG_EVAL_RESULT: u8 = 0x16;
/// Host -> target: abort the innermost execution -- a runaway program, or a runaway evaluation.
/// Empty payload. Answered by [`EVT_STOPPED`], which names the state the target is now in.
///
/// RESERVED, and refused by name where it is not implemented. One op rather than
/// three, because a host needs to know what state it is now in more than it needs three verbs, and
/// one op cannot be aimed at the wrong layer.
pub const ABORT: u8 = 0x17;
/// Host -> target: replace ALL breakpoints. Payload = `count(u16 LE)` then `count` x
/// `(method_id: u32 LE, offset: u32 LE)`. Answered by [`DBG_ACK`].
///
/// Accepted both while halted and mid-run, so an editor can add a breakpoint without first pausing;
/// one set while the program runs takes effect on its next hit.
pub const DBG_BREAK: u8 = 0x18;
/// Host -> target: request the call stack. Answered by [`DBG_FRAMES`].
pub const DBG_STACK: u8 = 0x1C;
/// Target -> host: the call stack, innermost first. Payload = `count(u16 LE)` then `count` x
/// `(method_id: u32 LE, offset: u32 LE)`.
pub const DBG_FRAMES: u8 = 0x1D;
/// Host -> target: request one frame's variables. Payload = `frame_index(u16 LE)` in the
/// [`DBG_FRAMES`] order, 0 = innermost. Answered by [`DBG_VARS`]. Accepted while HALTED, because
/// between stops the values are in motion.
pub const DBG_LOCALS: u8 = 0x1E;
/// Target -> host: one frame's variables, POSITIONAL. Payload = `locals(u16 LE)` then that many
/// [`val`]-tagged values, then `args(u16 LE)` and that many more. An unknown frame index answers
/// `0, 0`.
///
/// Slot NAMES are not here. They live host-side in the source map, and a target that carried them
/// would be carrying a second copy of something the host already has.
pub const DBG_VARS: u8 = 0x1F;
/// Host -> target: expand one value's children -- an object's fields, an array's elements, a box's
/// content, an inline structure's fields. Payload = `frame_index(u16 LE)`, `root_kind(u8)`
/// (0 = local, 1 = argument), `root_slot(u16 LE)`, `path_len(u8)`, `path_len` x `child_index(u16
/// LE)`, then `first_child(u16 LE)` and `max_children(u16 LE)`. Answered by [`DBG_CHILDREN`].
///
/// The selector is STATELESS: the target re-walks it from the frame root on every request, so no
/// device-side handle table exists to invalidate on resume. The range preserves that exactly -- it
/// selects a window of an expansion the target computes fresh each time, rather than a cursor the
/// target has to remember.
///
/// A range is what makes a large aggregate expandable at all. The whole of a hundred-thousand-element
/// array is about 1.3 MB, which no frame can carry and which is therefore lost entirely rather than
/// truncated; a variables pane wants a page of it in any case.
pub const DBG_EXPAND: u8 = 0x20;
/// Target -> host: the expanded children. Payload = `total(u16 LE)` -- how many children the value
/// has, so a host knows what it has not asked for -- then `count(u16 LE)` and `count` x
/// `(name_len(u8), name(UTF-8), <val>)`.
///
/// The names here are runtime TYPE metadata (`fieldN` by slot, `[i]`, a box's `value`), not source
/// local names.
pub const DBG_CHILDREN: u8 = 0x21;
/// Target -> host: execution stopped. Payload = `reason(u8)`, `method_id(u32 LE)`, `offset(u32 LE)`,
/// and for [`stop_reason::DONE`] and [`stop_reason::TRAP`] the result tail `exit(i32 LE)`,
/// `flags(u8)`.
///
/// EVERY execution ends here, whether it was started halted or running, so a target that advertises
/// no debug capability at all still sends this one. Its home in the debug block is filing, not a
/// capability gate. On a `DONE` there is no stop site, so `method_id` and `offset` are zero -- eight
/// bytes bought to keep one parse everywhere, which is the right trade on a part that parses this
/// inside a serve loop.
///
/// There is NO output tail. Output is [`EVT_OUTPUT`] and only [`EVT_OUTPUT`]: a tail on the one
/// event no execution can avoid is a tail that gets clipped by a frame boundary, silently, on
/// exactly the message a host cannot do without.
pub const EVT_STOPPED: u8 = 0x28;
/// Target -> host: output produced so far, while the program is still running. Payload =
/// `stream(u8)`, `flags(u8)`, then bytes to the end of the payload (see [`output`]).
///
/// UNSOLICITED and sequence-independent: it arrives during a resume that has not answered yet, so it
/// carries that request's `seq` and is distinguished by its message TYPE. A host must not mistake it
/// for the reply.
///
/// NOT debug-gated, for the same reason [`EVT_STOPPED`] is not: it is how a program's output reaches
/// a host at all, on every path, and a target that advertises no debug capability still sends it.
///
/// A CHUNK NEVER SPLITS A CHARACTER. The target holds back a trailing high surrogate rather than
/// encoding half a pair, so a host decodes each frame independently and never has to join across
/// frames to find a code point. That decision is the target's because it is nearly free there and
/// would be a buffering rule on every host otherwise.
pub const EVT_OUTPUT: u8 = 0x29;


/// Host -> target: load an assembly into RAM. Chunked (see [`MAX_CHUNK_DATA`]).
pub const LOAD_PE: u8 = 0x30;
/// Host -> target: load a baked image into RAM. Chunked.
pub const LOAD_IMAGE: u8 = 0x31;
/// Host -> target: load a Python bundle into RAM. Chunked.
pub const LOAD_BUNDLE: u8 = 0x32;
/// Host -> target: load ECMAScript bytecode into RAM. Chunked.
///
/// RESERVED, and refused by name where it is not implemented. The slot was decided
/// while it was free, which is the whole point of reallocating the space once.
pub const LOAD_JS: u8 = 0x33;
/// Target -> host: the result of one chunk, of a LOAD or a DEPLOY alike. Payload = `status(u8)`
/// (see [`xfer`]), `crc32(u32 LE)`.
///
/// ONE reply for both halves, because a reply carries its request's `seq` and the host therefore
/// already knows what it sent -- the type byte was redundant for attribution. That holds whether or
/// not a load and a deploy can be in flight at once, which is why it is the reason to prefer over an
/// argument from exclusivity.
///
/// The CRC's MEANING is keyed on the REQUEST, not on this reply: over the RAM as assembled for a
/// load, over the flash as read back for a deploy.
pub const XFER_RESULT: u8 = 0x38;
/// Host -> target: discard the loaded artifact -- partial or complete -- and reclaim the arena.
/// Empty payload. Answered by [`XFER_RESULT`] with `crc32 = 0`.
///
/// It exists because abandoning a transfer otherwise meant sending something, and the something was
/// wrong: a zero-length transfer satisfies the completion rule immediately, so it would complete as
/// a loaded EMPTY artifact rather than discard anything.
pub const LOAD_CLEAR: u8 = 0x39;


/// Host -> target: persist an assembly so it boots on reset. Chunked. `LOAD_PE + 0x10`.
pub const DEPLOY_PE: u8 = 0x40;
/// Host -> target: persist a baked image so it boots on reset. Chunked. `LOAD_IMAGE + 0x10`.
pub const DEPLOY_IMAGE: u8 = 0x41;
/// Host -> target: persist a Python bundle so it boots on reset. Chunked. `LOAD_BUNDLE + 0x10`.
pub const DEPLOY_BUNDLE: u8 = 0x42;
/// Host -> target: persist ECMAScript bytecode so it boots on reset. Chunked. `LOAD_JS + 0x10`.
/// RESERVED alongside [`LOAD_JS`].
pub const DEPLOY_JS: u8 = 0x43;
/// Host -> target: erase the persisted artifact. Empty payload. Answered by [`XFER_RESULT`].
///
/// The mirror of [`LOAD_CLEAR`], and the reason that op sits where it does: `DEPLOY_CLEAR - 0x10`.
pub const DEPLOY_CLEAR: u8 = 0x49;
/// Host -> target: what is deployed? Empty payload. Answered by [`DEPLOY_STATUS_RESULT`].
pub const DEPLOY_STATUS: u8 = 0x4A;
/// Target -> host: `state(u8)` (see [`deploy_state`]), `tier(u8)` (see [`tier`]),
/// `checksum(u64 LE)`.
///
/// `tier` rides along so a host learns WHAT is installed without a second round trip, which is the
/// case a board holding more than one runtime exists for.
pub const DEPLOY_STATUS_RESULT: u8 = 0x4B;
/// Host -> target: what is EXECUTING, and in what state? Empty payload. Answered by [`EVT_STOPPED`]
/// when something is stopped, paused or trapped, and by a running form when it is not.
///
/// RESERVED, and refused by name where it is not implemented. It disturbs nothing,
/// and it is what a freshly connected host needs: [`DEPLOY_STATUS`] answers what is INSTALLED, and
/// without this nothing answers what is RUNNING.
pub const EXEC_STATUS: u8 = 0x4C;
/// Host -> target: start something. Payload = `source(u8)` (see [`exec_source`]), `flags(u8)`
/// (see [`exec_flags`]). Answered by [`EXEC_ACK`]; completion arrives later as [`EVT_STOPPED`].
///
/// RESERVED, and refused by name where it is not implemented. One op replaces a
/// two-by-two -- where the code comes from, and whether it starts halted -- that had been written as
/// four message types.
pub const EXEC: u8 = 0x4D;
/// Target -> host: `status(u8)` -- STARTED, not finished. Flushed before any reset the start
/// implies. RESERVED alongside [`EXEC`].
pub const EXEC_ACK: u8 = 0x4E;


/// Host -> target: fetch the resident-profile manifest. Payload = `offset(u32 LE)`, 0 for the whole
/// thing or the point to resume from. Answered by [`PROFILE_MANIFEST`].
///
/// Named block-first like every other op. The verb-first spelling it replaces was the only one in
/// the protocol facing the other way, and it put two prefixes in a two-op block.
pub const PROFILE_GET: u8 = 0x50;
/// Target -> host: the manifest. Payload = `offset(u32 LE)`, `total(u32 LE)`, then bytes -- the
/// same chunk shape every other transfer uses.
///
/// Chunking is what makes the manifest's own promise of an unconstrained profile description true
/// rather than aspirational: a single frame's length field caps at 65,535 bytes, and the manifest
/// grows with the intrinsic count and with each resident runtime a board carries.
pub const PROFILE_MANIFEST: u8 = 0x51;


/// Host -> target: open a live session. Payload = `heartbeat_ms(u32 LE)`, `config_len(u16 LE)`,
/// `config[config_len]`. Answered by [`REPL_OPENED`].
///
/// `heartbeat_ms` is the interval a link supervisor on the target measures silence against; `0`
/// disables it. `config` is RESERVED and carries nothing today -- it is here so a session can be
/// opened with settings without a change to this shape, and what it will carry is not decided here.
///
/// What a target does when a machine it is driving must be left in a safe condition is the TARGET's
/// to decide and to report. A host asking for it over the wire would be commanding a policy from
/// the end of a cable that has just been shown to be unreliable, which is the situation the policy
/// exists for.
pub const REPL_OPEN: u8 = 0x60;
/// Target -> host: the session opened, or did not. Payload = `status(u8)`; on `status == 0` then
/// `session_id(u32 LE)`, `max_fields(u16 LE)`, `max_methods(u16 LE)`, `heap_budget(u32 LE)`, where a
/// 0 limit means unspecified. On a nonzero status the tail is the reason, UTF-8.
pub const REPL_OPENED: u8 = 0x61;
/// Host -> target: a compiled submission DELTA to load into the live session and run. Chunked.
/// Answered by [`REPL_DELTA_RESULT`].
///
/// A delta binds to variables and types that already exist in the session, which is what makes it a
/// different operation from a LOAD: a LOAD places a standalone artifact and has no concept of a
/// session at all.
pub const REPL_DELTA: u8 = 0x62;
/// Target -> host: the submission's result. Payload = `status(u8)`, `new_fields(u16 LE)`,
/// `display_len(u16 LE)`, `display[display_len]`, then this submission's output.
///
/// `status`: 0 ok, 1 no open session, 2 the delta did not load, 3 the submission trapped. On a
/// nonzero status `display` is empty and the tail is the reason.
pub const REPL_DELTA_RESULT: u8 = 0x63;
/// Host -> target: close the live session cleanly. Empty payload; the tail is RESERVED for a
/// teardown reason. Answered by [`REPL_CLOSED`].
pub const REPL_CLOSE: u8 = 0x64;
/// Target -> host: the session was closed. Payload = `ok(u8)`. Idempotent -- closing when no session
/// is open still answers 1.
pub const REPL_CLOSED: u8 = 0x65;
/// Host -> target: a session heartbeat. Empty payload, and not answered, because any frame counts as
/// contact.
pub const REPL_PING: u8 = 0x66;
/// Host -> target: reset the TARGET, which is the only thing that reclaims an exhausted arena.
/// Empty payload. Answered by [`REPL_RESETTING`], then the target performs a system reset back into
/// serve mode.
///
/// # Why a whole-target reset rather than a session reset
///
/// The constrained serve allocates from a segregated-fit heap whose bump frontier never retreats and
/// which never splits or coalesces across size classes. Once a session has carved the arena,
/// dropping it returns its blocks to per-class free lists a fresh session cannot spend, so reopening
/// yields a session refused its first submission. Nothing short of a reset reclaims, which is why
/// this op exists and why it is honest about being a reboot.
pub const REPL_RESET: u8 = 0x67;
/// Target -> host: the reset was accepted and is imminent. Payload = `ok(u8)`. Sent BEFORE the reset
/// and flushed, so a host can tell an accepted reset from a target that simply stopped answering.
pub const REPL_RESETTING: u8 = 0x68;


/// Host -> target: subscribe to a signal. RESERVED -- the number is allocated and the payload shape
/// belongs with the implementation.
pub const SCOPE_SUBSCRIBE: u8 = 0x70;
/// Host -> target: unsubscribe from a signal. RESERVED alongside [`SCOPE_SUBSCRIBE`].
pub const SCOPE_UNSUBSCRIBE: u8 = 0x71;
/// Target -> host: an asynchronous sample batch. RESERVED alongside [`SCOPE_SUBSCRIBE`].
pub const SCOPE_SAMPLE: u8 = 0x72;


/// Host -> target: read target memory WITHOUT stopping the running program. Payload =
/// `addr(u32 LE)`, `len(u16 LE)`. Answered by [`LIVE_DATA`].
///
/// Served both while a program runs -- the point of the op -- and while the target is serving, so a
/// host gets the same answer either way, which is what lets it tell a program that has stopped from
/// an agent that has.
pub const LIVE_READ: u8 = 0x80;
/// Target -> host: `status(u8)` (see [`live_status`]) then, on success, the bytes that were read. A
/// nonzero status carries no bytes.
pub const LIVE_DATA: u8 = 0x81;
/// Host -> target: write target memory WITHOUT stopping the running program. Payload =
/// `addr(u32 LE)` then the bytes. Answered by [`LIVE_WROTE`].
pub const LIVE_WRITE: u8 = 0x82;
/// Target -> host: `status(u8)`, `written(u16 LE)` -- 0 on any nonzero status. A partial write never
/// happens: the whole span is checked before the first byte.
pub const LIVE_WROTE: u8 = 0x83;


/// Host -> target: enter the silicon vendor's own bootloader. Empty payload.
///
/// RESERVED, and refused by name where it is not implemented. The device CHANGES CLASS
/// when it arrives -- mass storage, or a device-firmware-upgrade interface -- at which point this
/// protocol is gone and the host finishes over whatever the vendor's bootloader speaks.
pub const ENTER_HW_BOOTLOADER: u8 = 0x90;
/// Host -> target: enter the installed Lamella bootloader. Empty payload. RESERVED alongside
/// [`ENTER_HW_BOOTLOADER`].
///
/// Stays on the SAME transport, which is what makes a board exposing only a USB socket updatable
/// without a probe: enter, [`FW_WRITE`], [`FW_COMMIT`], run. Something has to be listening for that
/// to be true, so getting the FIRST bootloader onto such a board is a vendor route rather than an
/// operation of this protocol -- every subsequent update is one.
pub const ENTER_SW_BOOTLOADER: u8 = 0x91;
/// Host -> target: write a firmware or native image. Chunked. Answered by [`FW_RESULT`]. RESERVED.
pub const FW_WRITE: u8 = 0x92;
/// Target -> host: `status(u8)` (see [`xfer`]), `crc32(u32 LE)` over the flash AS READ BACK.
/// RESERVED alongside [`FW_WRITE`].
pub const FW_RESULT: u8 = 0x93;
/// Host -> target: verify the whole written range. Empty payload. Answered by
/// [`FW_COMMIT_RESULT`]. RESERVED.
pub const FW_COMMIT: u8 = 0x94;
/// Target -> host: `status(u8)`, `digest(u64 LE)`. Sent and flushed BEFORE anything the commit
/// implies, and it does not jump on a failed verify. RESERVED alongside [`FW_COMMIT`].
pub const FW_COMMIT_RESULT: u8 = 0x95;
/// Host -> target: what firmware is installed? Empty payload. Answered by [`FW_STATUS_RESULT`].
/// RESERVED.
pub const FW_STATUS: u8 = 0x96;
/// Target -> host: `state(u8)` (see [`fw_state`]), `region_size(u32 LE)`, `image_crc(u32 LE)`,
/// `key_id(u32 LE)`. RESERVED alongside [`FW_STATUS`].
pub const FW_STATUS_RESULT: u8 = 0x97;
/// Host -> target: choose which installed image boots next. Payload = `slot(u8)` (see
/// [`fw_slot`]), `intent(u8)` (see [`fw_intent`]). Answered by [`FW_ACTIVATE_RESULT`]. RESERVED.
///
/// Its own op rather than a flag on [`FW_COMMIT`], because commit means *I just wrote something,
/// verify it*, and selecting an image that is already installed involves no write at all.
///
/// It takes effect at the NEXT boot: activation records the choice and the running image keeps
/// running. One op, one effect -- a reset is a separate thing to ask for, and the paths that do it
/// already exist.
pub const FW_ACTIVATE: u8 = 0x98;
/// Target -> host: `status(u8)` (see [`fw_activate_status`]), `active_slot(u8)`, `next_slot(u8)`.
/// RESERVED alongside [`FW_ACTIVATE`].
pub const FW_ACTIVATE_RESULT: u8 = 0x99;


/// Either way: an op outside this protocol's own vocabulary. Payload = `ns(u16 LE)`, `op(u16 LE)`,
/// then the extension's own payload (see [`ext`]).
///
/// A board family with something to say that no other board has -- a radio module with its own
/// firmware to load, a carrier with its own controls -- says it here rather than taking a byte out
/// of a space every board shares. The namespace is advertised in the profile manifest, so a host
/// discovers what a board understands instead of guessing.
pub const EXTENDED: u8 = 0xF0;


/// Every allocated message type, as its byte and its name.
///
/// It exists so the space can be CHECKED rather than believed: two ops claiming one byte is the
/// failure this whole allocation is arranged to prevent, and it is invisible in a list of `const`
/// declarations spread over a file. It also lets a host print a type byte it did not expect, which
/// is the difference between a diagnosable trace and a column of hexadecimal.
///
/// A byte absent here is unallocated and RESERVED: a target refuses it by name rather than dropping
/// it, so a host learns in one round trip that the far end does not implement something, instead of
/// waiting out a timeout it cannot tell from a board that has stopped answering.
pub const ALL: &[(u8, &str)] = &[
    (HELLO, "HELLO"),
    (HELLO_ACK, "HELLO_ACK"),
    (HELLO_NAK, "HELLO_NAK"),
    (ERROR, "ERROR"),
    (PING, "PING"),
    (PONG, "PONG"),
    (SESSION_REVOKED, "SESSION_REVOKED"),
    (LINK_ACK, "LINK_ACK"),
    (LINK_ACKED, "LINK_ACKED"),
    (DBG_ACK, "DBG_ACK"),
    (DBG_RESUME, "DBG_RESUME"),
    (DBG_PAUSE, "DBG_PAUSE"),
    (DBG_STEP, "DBG_STEP"),
    (DBG_DETACH, "DBG_DETACH"),
    (DBG_EVAL, "DBG_EVAL"),
    (DBG_EVAL_RESULT, "DBG_EVAL_RESULT"),
    (ABORT, "ABORT"),
    (DBG_BREAK, "DBG_BREAK"),
    (DBG_STACK, "DBG_STACK"),
    (DBG_FRAMES, "DBG_FRAMES"),
    (DBG_LOCALS, "DBG_LOCALS"),
    (DBG_VARS, "DBG_VARS"),
    (DBG_EXPAND, "DBG_EXPAND"),
    (DBG_CHILDREN, "DBG_CHILDREN"),
    (EVT_STOPPED, "EVT_STOPPED"),
    (EVT_OUTPUT, "EVT_OUTPUT"),
    (LOAD_PE, "LOAD_PE"),
    (LOAD_IMAGE, "LOAD_IMAGE"),
    (LOAD_BUNDLE, "LOAD_BUNDLE"),
    (LOAD_JS, "LOAD_JS"),
    (XFER_RESULT, "XFER_RESULT"),
    (LOAD_CLEAR, "LOAD_CLEAR"),
    (DEPLOY_PE, "DEPLOY_PE"),
    (DEPLOY_IMAGE, "DEPLOY_IMAGE"),
    (DEPLOY_BUNDLE, "DEPLOY_BUNDLE"),
    (DEPLOY_JS, "DEPLOY_JS"),
    (DEPLOY_CLEAR, "DEPLOY_CLEAR"),
    (DEPLOY_STATUS, "DEPLOY_STATUS"),
    (DEPLOY_STATUS_RESULT, "DEPLOY_STATUS_RESULT"),
    (EXEC_STATUS, "EXEC_STATUS"),
    (EXEC, "EXEC"),
    (EXEC_ACK, "EXEC_ACK"),
    (PROFILE_GET, "PROFILE_GET"),
    (PROFILE_MANIFEST, "PROFILE_MANIFEST"),
    (REPL_OPEN, "REPL_OPEN"),
    (REPL_OPENED, "REPL_OPENED"),
    (REPL_DELTA, "REPL_DELTA"),
    (REPL_DELTA_RESULT, "REPL_DELTA_RESULT"),
    (REPL_CLOSE, "REPL_CLOSE"),
    (REPL_CLOSED, "REPL_CLOSED"),
    (REPL_PING, "REPL_PING"),
    (REPL_RESET, "REPL_RESET"),
    (REPL_RESETTING, "REPL_RESETTING"),
    (SCOPE_SUBSCRIBE, "SCOPE_SUBSCRIBE"),
    (SCOPE_UNSUBSCRIBE, "SCOPE_UNSUBSCRIBE"),
    (SCOPE_SAMPLE, "SCOPE_SAMPLE"),
    (LIVE_READ, "LIVE_READ"),
    (LIVE_DATA, "LIVE_DATA"),
    (LIVE_WRITE, "LIVE_WRITE"),
    (LIVE_WROTE, "LIVE_WROTE"),
    (ENTER_HW_BOOTLOADER, "ENTER_HW_BOOTLOADER"),
    (ENTER_SW_BOOTLOADER, "ENTER_SW_BOOTLOADER"),
    (FW_WRITE, "FW_WRITE"),
    (FW_RESULT, "FW_RESULT"),
    (FW_COMMIT, "FW_COMMIT"),
    (FW_COMMIT_RESULT, "FW_COMMIT_RESULT"),
    (FW_STATUS, "FW_STATUS"),
    (FW_STATUS_RESULT, "FW_STATUS_RESULT"),
    (FW_ACTIVATE, "FW_ACTIVATE"),
    (FW_ACTIVATE_RESULT, "FW_ACTIVATE_RESULT"),
    (EXTENDED, "EXTENDED"),
];

/// The name of `msg_type`, or `None` when nothing is allocated at that byte.
///
/// A host printing an unrecognized byte as a number leaves its reader to look it up; printing
/// `None` as *unallocated* is the answer they were going to arrive at anyway.
#[must_use]
pub fn name(msg_type: u8) -> Option<&'static str> {
    let mut index = 0;
    while index < ALL.len() {
        if ALL[index].0 == msg_type {
            return Some(ALL[index].1);
        }
        index += 1;
    }
    None
}

/// Whether `msg_type` could be a message type at all.
///
/// `0x00` and `0xFF` are not, permanently: `0xFF` is what erased flash reads as and `0x00` what
/// zeroed RAM reads as, so accepting either means a run of unprogrammed memory arriving on a carrier
/// can present as a message. This says nothing about whether the type is ALLOCATED -- an
/// unallocated type is a refusal a target answers by name, which is a different and much more useful
/// outcome than a frame that never should have been assembled.
#[must_use]
pub const fn is_valid_type(msg_type: u8) -> bool {
    msg_type != 0x00 && msg_type != 0xFF
}

/// The 8-byte `offset(u32 LE), total(u32 LE)` header every chunked op carries.
pub const CHUNK_HEADER_LEN: usize = 8;

/// The most artifact bytes one chunk can carry: the frame's own payload cap less
/// [`CHUNK_HEADER_LEN`].
///
/// A transfer is complete when `offset + len == total`, and a chunk at `offset == 0` discards
/// whatever partial transfer that destination held. A single-frame artifact is the degenerate
/// one-chunk case rather than a second code path, which is what keeps the artifact KIND on every
/// frame: an interrupted transfer cannot then be misread as a partial artifact of another kind.
pub const MAX_CHUNK_DATA: usize = super::MAX_PAYLOAD - CHUNK_HEADER_LEN;

/// The chunk size a host SHOULD send a multiple of, so a chunk boundary never falls inside a flash
/// write unit on any part in this set.
///
/// A target MUST accept any size up to [`MAX_CHUNK_DATA`] regardless, and buffer the remainder. That
/// is what makes [`xfer::WRITTEN_NOT_READ_BACK`] an honest answer rather than a shrug: a target
/// holding back a partial write unit says so, instead of reporting a read-back match over bytes that
/// are not in flash yet.
pub const CHUNK_ALIGN_HINT: usize = 256;

/// Which runtime an artifact is for, in a `tier` field. Boot dispatch on a board carrying more than
/// one runtime reads it, and `0` is deliberately not a value: erased flash and zeroed RAM both
/// present as one, so a tier read out of either has to be wrong rather than plausible.
pub mod tier {
    /// A common intermediate language artifact.
    pub const CIL: u8 = 1;
    /// A Python artifact.
    pub const PYTHON: u8 = 2;
    /// An ECMAScript artifact.
    pub const ECMASCRIPT: u8 = 3;
    /// Native machine code -- an artifact that names NO interpreter, where the others name which.
    pub const NATIVE: u8 = 4;
}

/// Why an execution stopped. Byte 0 of [`EVT_STOPPED`].
///
/// Additive: a new reason is a new value in a field that already exists, so one arrives without a
/// change to any shape here. A host that meets a reason it does not know reports the stop rather
/// than the reason, which is the half that matters.
pub mod stop_reason {
    /// Booted and halted at the entry point.
    pub const ENTRY: u8 = 0;
    /// A step completed.
    pub const STEP: u8 = 1;
    /// Execution arrived at a breakpoint.
    pub const BREAKPOINT: u8 = 2;
    /// A pause took effect, or acknowledged an already-halted target.
    pub const PAUSED: u8 = 3;
    /// The program completed. The result tail follows.
    pub const DONE: u8 = 4;
    /// The program trapped, or an artifact failed to start. The result tail follows.
    pub const TRAP: u8 = 5;
    /// An [`super::ABORT`] ended the execution. NO result tail: the program produced no value, and an
    /// invented exit code is the thing this whole field exists to avoid.
    ///
    /// Distinct from [`PAUSED`] because a pause is resumable and this is not -- a host that read an
    /// abort as a pause would offer a resume for an execution that no longer exists. Distinct from
    /// [`DONE`] because the program did not finish, and "returned 0" and "was killed" must not be
    /// the same observation. It carries the stop SITE, which is the one thing worth having after an
    /// abort: where a runaway program actually was.
    pub const ABORTED: u8 = 6;
}

/// How far a [`DBG_STEP`] goes. Byte 0 of its payload.
pub mod step_mode {
    /// Into a call.
    pub const IN: u8 = 1;
    /// Over a call. RESERVED -- see the note below before sending it.
    pub const OVER: u8 = 2;
    /// Out of the current method. RESERVED -- see the note below before sending it.
    pub const OUT: u8 = 3;

}

/// The `<val>` encoding [`DBG_VARS`] and [`DBG_CHILDREN`] carry: one tag byte, then the payload the
/// tag implies, all little-endian.
///
/// A tier compiled without a value's feature -- floating point, typed references -- never produces
/// its tag, so a host meeting one has learned something true about the far end.
pub mod val {
    /// The null reference. No payload.
    pub const NULL: u8 = 0x00;
    /// A 32-bit integer, `i32`. Also booleans, characters and the small integer types, widened as
    /// the execution model requires.
    pub const INT32: u8 = 0x01;
    /// A 64-bit integer, `i64`.
    pub const INT64: u8 = 0x02;
    /// A native-sized integer, carried as `i64` regardless of the target's width.
    pub const NATIVE_INT: u8 = 0x03;
    /// A 64-bit binary floating-point value.
    pub const FLOAT: u8 = 0x04;
    /// A 32-bit binary floating-point value.
    pub const SINGLE: u8 = 0x05;
    /// An object reference: `handle(u32 LE)` -- the heap slot, a display and correlation id that is
    /// stale after a resume -- then `type_token(u64 LE)`, 0 when the value has no recoverable type
    /// identity.
    pub const OBJECT: u8 = 0x06;
    /// An inline value-type instance: `field_count(u16 LE)`, `type_token(u64 LE)`.
    pub const STRUCT: u8 = 0x07;
    /// A managed pointer: `kind(u8)`, `a(u32 LE)`, `b(u32 LE)`, `c(u32 LE)`.
    pub const BYREF: u8 = 0x08;
    /// A typed reference: `type_token(u64 LE)` then a [`BYREF`] location descriptor.
    pub const TYPED_REF: u8 = 0x09;
}

/// The result of one artifact chunk. Byte 0 of [`XFER_RESULT`] and of [`FW_RESULT`].
///
/// ONE ladder for both halves of the transfer, deliberately. A load never emits
/// [`xfer::WRITTEN_NOT_READ_BACK`] -- that is a fact about loading, not a reason for a second table, and
/// two ladders over one shape is how two implementations come to disagree about a value.
pub mod xfer {
    /// Written and read back, and the read-back matched.
    pub const MATCHED: u8 = 0;
    /// Written, and it could not be read back to check -- a target holding a partial flash write
    /// unit says so rather than claiming a match over bytes that are not in flash yet.
    pub const WRITTEN_NOT_READ_BACK: u8 = 1;
    /// The write failed. On a load, this is the arena refusing.
    pub const WRITE_FAILED: u8 = 2;
    /// The range was rejected.
    pub const RANGE_REJECTED: u8 = 3;
}

/// What is deployed. Byte 0 of [`DEPLOY_STATUS_RESULT`].
///
/// *Present but unverifiable* is the state a host most needs to tell apart from *nothing there*, and
/// a boolean cannot express it.
pub mod deploy_state {
    /// Nothing is deployed.
    pub const NONE: u8 = 0;
    /// Present and verified, so the checksum in the same reply is meaningful.
    pub const VERIFIED: u8 = 1;
    /// Present but unverifiable -- metadata that did not parse, or a checksum that did not match.
    pub const UNVERIFIABLE: u8 = 2;
    /// Present, but built for a different runtime than the one asking.
    pub const TIER_MISMATCH: u8 = 3;
    /// Present, but built against a different partition layout.
    pub const LAYOUT_MISMATCH: u8 = 4;
}

/// Where an [`EXEC`] takes its artifact from. Byte 0 of its payload.
pub mod exec_source {
    /// The artifact in RAM, put there by a LOAD.
    pub const LOADED: u8 = 0;
    /// The persisted artifact, put there by a DEPLOY.
    pub const DEPLOYED: u8 = 1;
}

/// How an [`EXEC`] starts. Byte 1 of its payload.
pub mod exec_flags {
    /// Start HALTED at the entry point rather than running. Refused unless the target advertises a
    /// debug capability, since nothing would be able to resume it.
    pub const START_HALTED: u8 = 1 << 0;
}

/// What an [`EXEC_ACK`] reports. Byte 0 of its payload.
///
/// It answers TWO ops -- an [`EXEC`] that started something, and an [`EXEC_STATUS`] asking what is
/// executing -- because both questions have the same answer space: what is running, or why nothing
/// is. A second reply type would have been a second spelling of these five values.
///
/// **An acknowledgement is not a result.** [`STARTED`][exec_ack::STARTED] means the execution began; how it ENDED
/// arrives later as [`EVT_STOPPED`], and a host that read this as completion would report success
/// for a program that had not run yet.
pub mod exec_ack {
    /// The execution STARTED. Completion arrives later as [`super::EVT_STOPPED`].
    pub const STARTED: u8 = 0;
    /// Nothing is executing -- the answer to an [`super::EXEC_STATUS`] on an idle target.
    pub const IDLE: u8 = 1;
    /// An execution is RUNNING, and answering did not disturb it.
    pub const RUNNING: u8 = 2;
    /// Refused: there is nothing at the requested source to start. An empty arena, or a deploy
    /// region holding nothing.
    pub const NOTHING_TO_RUN: u8 = 3;
    /// Refused: [`super::exec_flags::START_HALTED`] needs a debug capability this target does not offer,
    /// and starting halted without one would leave an execution nothing could resume.
    pub const HALTED_UNSUPPORTED: u8 = 4;
    /// Refused: this target does not run artifacts from the requested [`super::exec_source`].
    pub const NO_SUCH_SOURCE: u8 = 5;
}

/// The header [`EVT_OUTPUT`] carries: which stream, and what is true about this chunk of it.
pub mod output {
    /// The program's own standard output.
    pub const STDOUT: u8 = 0;
    /// The program's own standard error.
    pub const STDERR: u8 = 1;
    /// The debugger's channel -- diagnostic output a program writes for a tool rather than for its
    /// user. Kept apart from standard output so a client can show it in its own pane.
    pub const DEBUG: u8 = 2;
    /// The first stream number a board family may define for itself. Everything below is this
    /// protocol's.
    pub const FIRST_VENDOR_STREAM: u8 = 16;

    /// This chunk ends on a line boundary.
    ///
    /// It exists because two streams create a rendering problem one stream did not: a host
    /// interleaving standard output and standard error into one terminal has to know whether it is
    /// mid-line, and only the target knows where its line boundaries are.
    pub const ENDS_ON_LINE_BOUNDARY: u8 = 1 << 0;
    /// Output was DROPPED before this chunk -- the target overran its transmit buffer.
    ///
    /// Per-chunk rather than per-execution, so it says WHERE in the stream the loss happened rather
    /// than only that it happened somewhere.
    pub const OUTPUT_DROPPED: u8 = 1 << 1;
}

/// Why a [`LIVE_READ`] or [`LIVE_WRITE`] was refused. Byte 0 of [`LIVE_DATA`] and [`LIVE_WROTE`].
///
/// A refusal here is per-request and in-band, and distinct from [`ERROR`]: the op IS implemented,
/// and the target is saying that this particular address or length is not one it will touch. The two
/// failures need different repairs -- a different firmware against a different address -- so they
/// are different answers.
pub mod live_status {
    /// The request was served.
    pub const OK: u8 = 0;
    /// This firmware declares no live window, so it carries no agent -- the difference between a
    /// refusal and a bus fault at an address a host asked for.
    pub const NO_WINDOW: u8 = 1;
    /// The requested span is not entirely inside the declared window.
    pub const OUT_OF_WINDOW: u8 = 2;
    /// The payload is malformed, the length is zero, or the read is larger than the target serves.
    pub const BAD_REQUEST: u8 = 3;
}

/// What firmware is installed. Byte 0 of [`FW_STATUS_RESULT`].
pub mod fw_state {
    /// Nothing is installed.
    pub const NONE: u8 = 0;
    /// Present and verified.
    pub const VERIFIED: u8 = 1;
    /// Present but unverifiable.
    pub const UNVERIFIABLE: u8 = 2;
}

/// Which image a [`FW_ACTIVATE`] selects. Byte 0 of its payload.
pub mod fw_slot {
    /// The one that is not running. It is the whole of the ordinary flip-try-flip-back loop, and it
    /// means a host does not have to know or track which side it is on.
    pub const OTHER: u8 = 0xFF;
}

/// How long a [`FW_ACTIVATE`] lasts. Byte 1 of its payload.
pub mod fw_intent {
    /// Until something else changes it.
    pub const PERMANENT: u8 = 0;
    /// For ONE boot, then revert unless confirmed.
    pub const ONE_BOOT: u8 = 1;
}

/// What a [`FW_ACTIVATE`] did. Byte 0 of [`FW_ACTIVATE_RESULT`].
pub mod fw_activate_status {
    /// Activated; it takes effect at the next boot.
    pub const ACTIVATED: u8 = 0;
    /// Activated for ONE boot, and a confirmation is required to keep it.
    pub const ACTIVATED_ONE_BOOT: u8 = 1;
    /// Refused: there is no such slot.
    pub const NO_SUCH_SLOT: u8 = 2;
    /// Refused: that slot is empty or could not be verified.
    pub const SLOT_UNUSABLE: u8 = 3;
    /// Refused: it would go BACKWARD, and this image was not built to permit that.
    ///
    /// Distinct from [`DOWNGRADE_IMPOSSIBLE`] because the two are different problems with different
    /// answers, and a reader must not have to guess which they hit. This one is a rebuild.
    pub const DOWNGRADE_REFUSED: u8 = 4;
    /// Refused: going backward is not possible on this silicon at all.
    ///
    /// Where the anti-rollback record is a monotonic counter or a fuse, it has already advanced and
    /// nothing can undo that. This one is a different board.
    pub const DOWNGRADE_IMPOSSIBLE: u8 = 5;
}

/// How a [`FW_WRITE`] chunk landed. Byte 0 of [`FW_RESULT`].
pub mod fw_write_status {
    /// Programmed, and read back into the running checksum.
    pub const WRITTEN: u8 = 0;
    /// Refused: the chunk's offset is not on a program-granule boundary.
    ///
    /// A part programs a whole granule at a time, so a chunk starting mid-granule cannot be written
    /// without rewriting bytes that are already there -- which on most parts is not permitted twice
    /// between erases. The granule is reported in a firmware status so a host never has to guess it.
    pub const MISALIGNED: u8 = 1;
    /// Refused: the chunk runs past the end of the region.
    pub const OUT_OF_REGION: u8 = 2;
    /// Refused: the region has not been prepared, or a previous transfer was not finished.
    pub const NOT_READY: u8 = 3;
    /// The program operation itself reported failure.
    pub const PROGRAM_FAILED: u8 = 4;
    /// Programmed, but reading it back did not return what was written.
    ///
    /// Distinct from [`PROGRAM_FAILED`] because the two say different things about the part: one is
    /// a controller refusing the operation, the other is a controller accepting it over flash that
    /// did not take it. The second is the one that means the part is wearing out.
    pub const READBACK_MISMATCH: u8 = 5;
}

/// What a [`FW_COMMIT`] concluded. Byte 0 of [`FW_COMMIT_RESULT`].
pub mod fw_commit_status {
    /// The written image matches what the host said it wrote, and is now a candidate.
    pub const COMMITTED: u8 = 0;
    /// Refused: fewer bytes were written than the host claims it sent.
    pub const SHORT: u8 = 1;
    /// Refused: the right number of bytes, and not the right bytes.
    ///
    /// Separate from [`SHORT`] because the causes do not overlap -- a truncated transfer and a
    /// corrupted one need different things looked at, and one number cannot say which happened.
    pub const CHECKSUM_MISMATCH: u8 = 2;
    /// Refused: the image was rejected by whatever verifies images on this target.
    pub const REJECTED: u8 = 3;
    /// Refused: nothing had been written to commit.
    pub const NOTHING_WRITTEN: u8 = 4;
}

/// The namespaces an [`EXTENDED`] frame can carry, and the ops inside them.
///
/// A namespace is a u16 advertised in the profile manifest, so a host learns which extensions a
/// board understands rather than probing for them. Namespace `0` is this protocol's own; the rest
/// belong to a board family or a module, and an op number is meaningful only inside its namespace.
pub mod ext {
    /// This protocol's own extension namespace, for ops that are ours but do not deserve a byte out
    /// of a space every board shares.
    pub const NS_LAMELLA: u16 = 0x0000;

    /// The namespace of the module-firmware transfer for boards carrying a networking module with
    /// its own separate firmware.
    ///
    /// It is an extension rather than a message type because it is a property of a handful of board
    /// families, and the ops below have no meaning on a board without such a module. RESERVED: the
    /// namespace is allocated so nothing else takes it.
    pub const NS_MODULE_FIRMWARE: u16 = 0x0001;

    /// Begin a module-firmware write. Payload = `offset(u32 LE)`, `total(u32 LE)`: the target
    /// initializes the module into its download mode and erases the span. Inside
    /// [`NS_MODULE_FIRMWARE`].
    ///
    /// Erasing a full module image takes seconds, so a host waits generously for the answer.
    pub const MODULE_FW_START: u16 = 0x0001;
    /// One chunk of the module image. Payload = `offset(u32 LE)` as an absolute module address, then
    /// the bytes; the target programs and verifies each chunk by read-back, so a small part can
    /// update a module image many times its own memory. Inside [`NS_MODULE_FIRMWARE`].
    pub const MODULE_FW_CHUNK: u16 = 0x0002;
    /// The module image is complete. Empty payload; the target runs its final check and parks the
    /// module for a clean restart into the new firmware. Inside [`NS_MODULE_FIRMWARE`].
    pub const MODULE_FW_END: u16 = 0x0003;
    /// The result of a start, a chunk or an end. Payload = `ok(u8)`. Inside
    /// [`NS_MODULE_FIRMWARE`].
    pub const MODULE_FW_RESULT: u16 = 0x0004;

    /// Whether one extension op PROGRAMS FLASH, and therefore holds a transfer open against another
    /// carrier claiming the session mid-erase.
    ///
    /// # Why the namespace answers this and not the arbiter
    ///
    /// A carrier arbiter classifies frames by their type byte, and every extension shares one. It
    /// cannot see which extension a frame carries without reading the payload, and reading further
    /// than the four-byte `ns, op` header would mean the transport layer parsing an extension's
    /// private encoding -- which is the one thing an extension namespace exists to avoid.
    ///
    /// So the header is public by construction and the ANSWER lives here, beside the ops. The
    /// alternative that was in place -- treat every extension as a write -- is safe in the direction
    /// that matters and wrong in the other: it holds a session against a physically attached cable
    /// for the length of any extension frame, including ones that write nothing at all.
    ///
    /// **An unknown namespace answers `true`**, because the cost is asymmetric and this is the one
    /// place it can be stated: holding a transfer open too long costs a session, and releasing it
    /// too early costs a half-written flash on a part that was mid-erase.
    #[must_use]
    pub const fn writes_flash(namespace: u16, op: u16) -> bool {
        match namespace {
            NS_MODULE_FIRMWARE => {
                matches!(op, MODULE_FW_START | MODULE_FW_CHUNK | MODULE_FW_END)
            }
            NS_LAMELLA => false,
            _ => true,
        }
    }
}
