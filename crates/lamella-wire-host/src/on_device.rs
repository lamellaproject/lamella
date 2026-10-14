//! A program sent to firmware already running on a board: stored in the board's flash, started,
//! and followed to its end. The `lamella` command's `deploy --device` and `run --device` take this
//! route, and so do the MCP server's `lamella_deploy` and `lamella_run_on_device`, so a program
//! reaches a board the same way from either front end and its run is reported in the same words.
//!
//! Nothing here names a command. A failure is a sentence that a front end puts its own name in
//! front of, and how a run ended is a [`RunEnd`] that a front end reports.

use crate::terminal::TerminalOutput;
use crate::{
    Aborted, RunCollector, TransferAck, abort_blocking, baked_image_checksum, board_name, classify_target,
    deploy_chunk_len, deploy_image_with_progress, deploy_status_blocking, exec, start_execution,
};
use lamella_wire::{Capabilities, Negotiated, Transport, TransportError};
use std::io::Write;
use std::time::{Duration, Instant};

/// How long each step of a store waits for the board's answer. Generous: a deploy erases and
/// writes flash on the board, which is slow in a way a timeout should not be racing.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// The capabilities a host offers when it stores a program on a board.
///
/// [`Capabilities::DEPLOY_PREFIX_CRC`] is among them so that each chunk's acknowledgement is
/// compared with what was sent: a capability works only where both ends offered it.
#[must_use]
pub fn host_caps() -> Capabilities {
    Capabilities(
        Capabilities::BAKED_IMAGE
            | Capabilities::REPL_RUN
            | Capabilities::PROFILE_CHIPID
            | Capabilities::DEPLOY_PREFIX_CRC,
    )
}

/// The capabilities a host offers when it stores a program and then runs it: [`host_caps`], and
/// starting a stored program without a reset ([`Capabilities::EXEC_NO_RESET`]).
#[must_use]
pub fn run_caps() -> Capabilities {
    Capabilities(host_caps().0 | Capabilities::EXEC_NO_RESET)
}

/// What a refusal reason MEANS, as a sentence somebody can act on.
///
/// A refusal is the target answering, not failing, and each reason has a different remedy: one says
/// stop asking, the other says wait for a colleague to unplug. Rendering the struct printed
/// `reason: 2, msg_type: 2` for the second, which reads as a protocol fault and sends the reader to
/// the cable -- the one direction that cannot help.
#[must_use]
pub fn describe_refusal(reason: u8) -> String {
    match reason {
        lamella_wire::error::SESSION_HELD => String::from(concat!(
            "another carrier already holds the debug session -- typically somebody at the ",
            "board with a cable, or a host that did not let go. The request was well formed ",
            "and this target implements it, so the answer changes when the other carrier ",
            "releases it; a reset clears a session whose host is gone.",
        )),
        lamella_wire::error::UNKNOWN_MESSAGE_TYPE => String::from(concat!(
            "this target does not implement that message. Stop asking rather than retrying ",
            "-- the answer will not change without different firmware.",
        )),
        other => format!("refusal reason {other}, which this build has no description for"),
    }
}

/// "the" and the board's product name, or "the board" when its HELLO named none.
#[must_use]
pub fn the_board(name: Option<&str>) -> String {
    name.map_or_else(|| "the board".to_owned(), |name| format!("the {name}"))
}

/// `text` with its first letter capitalized, for a board's name at the start of a sentence.
#[must_use]
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// What [`store_image`] found, or did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// The board already stored exactly this image, verified, so nothing was sent.
    AlreadyThere,
    /// The image was written, and the board acknowledged every chunk of it.
    Written,
}

/// The sequence numbers [`store_image`]'s steps go out under. Each step waits for the answer to its
/// own, so no two steps of one session may share one -- and none of these is a start's, which
/// follows them on the same session.
const TAKE_BACK_SEQ: u16 = 7;
const STORED_SEQ: u16 = 8;
const DEPLOY_SEQ: u16 = 1;

/// Put `image` in the flash of the board at the other end of `transport`, whose HELLO `session`
/// is, saying on `out` how it goes, or say why not.
///
/// 1. The board is taken back. A program it is running answers none of a deploy's questions, and
///    neither does a program a debugger left running when it disconnected, so a deploy that went
///    straight to them waited out its timeout and blamed the link.
/// 2. A board that already stores exactly this image, verified, is sent nothing.
/// 3. Anything else is written in chunks sized for the carrier ([`deploy_chunk_len`]), and each
///    acknowledgement's CRC over the flash read back is compared with the CRC of what was sent,
///    where the board offers that. A line is written each time another quarter of the image is in.
///
/// # Errors
/// A board that answered the HELLO and then not the take-back, or one that would not take the
/// image: each in a sentence of its own, which names no command.
pub fn store_image(
    transport: &mut impl Transport,
    session: &Negotiated,
    target: &str,
    image: &[u8],
    out: &mut dyn Write,
) -> Result<Stored, String> {
    let name = board_name(session.identity.product_model);
    let board = the_board(name);
    match abort_blocking(transport, TAKE_BACK_SEQ, TIMEOUT) {
        Ok(Aborted::StoppedAProgram) => {
            let _ = writeln!(out, "stopped the program {board} was running.");
        }
        Ok(Aborted::NothingWasRunning) => {}
        Err(TransportError::Refused { .. }) => {}
        Err(error) => {
            return Err(format!(
                "{board} answered the connection and then did not answer an ABORT within {} s \
                 ({error:?}):\nit is neither waiting for a host nor running a program this tool can \
                 stop. Nothing was sent.",
                TIMEOUT.as_secs()
            ));
        }
    }
    let held = deploy_status_blocking(transport, STORED_SEQ, TIMEOUT).ok().and_then(|status| status.checksum);
    if held.is_some() && held == baked_image_checksum(image) {
        let _ = writeln!(out, "{} already stores this program, so nothing was sent.", capitalized(&board));
        return Ok(Stored::AlreadyThere);
    }
    let chunk_len = deploy_chunk_len(session, classify_target(target));
    let _ = writeln!(out, "deploying {} B to {target}, {chunk_len} B per chunk", image.len());
    let _ = out.flush();
    let started = Instant::now();
    let mut quarters = Quarters::of(image.len());
    let sent = deploy_image_with_progress(
        transport,
        DEPLOY_SEQ,
        image,
        chunk_len,
        TIMEOUT,
        session.caps,
        &mut |written| {
            if let Some(quarter) = quarters.passed(written) {
                let _ = writeln!(
                    out,
                    "  {:>3}%  {written} B  {:.1} s",
                    quarter * 25,
                    started.elapsed().as_secs_f64()
                );
                let _ = out.flush();
            }
        },
    );
    match sent {
        Ok(TransferAck::Accepted) => {
            let _ = writeln!(
                out,
                "deployed {} B to {target} in {:.1} s",
                image.len(),
                started.elapsed().as_secs_f64()
            );
            Ok(Stored::Written)
        }
        Ok(not_accepted) => Err(format!("{}.", not_accepted.describe(name))),
        Err(TransportError::Refused { reason, .. }) => {
            Err(format!("{target} refused the deploy: {}", describe_refusal(reason)))
        }
        Err(error) => Err(format!("deploy to {target} failed: {error:?}")),
    }
}

/// Which quarters of an image a deploy has passed, so its progress is said four times however many
/// chunks it takes: a 326 KB image is forty chunks over USB and over a thousand over a serial line.
struct Quarters {
    total: usize,
    said: usize,
}

impl Quarters {
    fn of(total: usize) -> Self {
        Self { total, said: 0 }
    }

    /// The quarter `written` bytes has just reached, when it is one not said yet and not the last.
    ///
    /// The last is the deploy's own closing line, which says how long the whole of it took, so it
    /// is not said twice.
    fn passed(&mut self, written: usize) -> Option<usize> {
        if self.total == 0 {
            return None;
        }
        let quarter = written.min(self.total) * 4 / self.total;
        if quarter <= self.said || quarter >= 4 {
            return None;
        }
        self.said = quarter;
        Some(quarter)
    }
}

/// How long to wait for the board to acknowledge a start. It answers before the program runs, and
/// before the reset a start from a reset implies, so the answer is prompt.
pub const START_PATIENCE: Duration = Duration::from_secs(2);

/// Ask the board at the other end of `transport` to start the image just stored there, from a
/// reset, as it starts it at every power-up; answer what to say when it started, and why it did
/// not when it did not.
///
/// **THE BOARD's ACKNOWLEDGEMENT IS READ, AND ONLY IT SAYS "STARTED".** A board answers a start with
/// its reason when it will not make one, so a start sent and never read reported a refusal as a start.
///
/// # Errors
/// The board refused the start, did not acknowledge it, or the carrier failed: each says the image
/// is on the board, because the store succeeded and only the start did not.
pub fn start_deployed(transport: &mut impl Transport, patience: Duration) -> Result<&'static str, String> {
    use crate::StartFailure;
    match start_execution(transport, 2, exec::exec_source::DEPLOYED, 0, patience) {
        Ok(()) => Ok("started it."),
        Err(StartFailure::NoAnswer) => Err(
            "the image is on the board, and the board did not acknowledge the start, so whether it is \
             running is not known. Resetting the board runs it."
                .to_owned(),
        ),
        Err(failure @ (StartFailure::Refused(_) | StartFailure::Transport(TransportError::Refused { .. }))) => {
            Err(format!("the image is on the board, and {failure}."))
        }
        Err(failure @ StartFailure::Transport(_)) => {
            Err(format!("the image is on the board, and {failure}. Resetting the board runs it."))
        }
    }
}

/// The sequence number a run [`run_scheduled`] starts goes out under, and so the one its end comes
/// back at. None of [`store_image`]'s steps uses it.
const RUN_SEQ: u16 = 9;

/// How long a program may say nothing before the board is asked whether it is still running it.
///
/// A running program answers between two of its own steps and carries on, so asking costs one short
/// frame. A board that has left the program -- its firmware reset after a fault, or after running out
/// of memory -- answers that nothing is executing, and without the question a run would be waited on
/// forever.
pub const QUIET_BEFORE_ASKING: Duration = Duration::from_secs(2);

/// How a run is watched: how long it may say nothing before the board is asked whether it still
/// runs it, and how long the watch lasts when it does not last until the run ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    /// How long the program may be quiet before the board is asked.
    pub quiet: Duration,
    /// How long after the start the watch ends, the program still running; `None` watches it to
    /// its end.
    pub limit: Option<Duration>,
}

impl Watch {
    /// A watch that lasts until the run ends, however long that is.
    #[must_use]
    pub fn to_the_end() -> Self {
        Self { quiet: QUIET_BEFORE_ASKING, limit: None }
    }

    /// A watch that lasts until the run ends or until `limit` has passed since it started,
    /// whichever comes first.
    #[must_use]
    pub fn for_at_most(limit: Duration) -> Self {
        Self { quiet: QUIET_BEFORE_ASKING, limit: Some(limit) }
    }
}

/// How a watched run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// The program returned, with this exit code.
    Done(i32),
    /// The program trapped. The trap's own description arrived on its error stream just before the
    /// stop.
    Trapped,
    /// Something else connected to the board and aborted the program.
    Aborted,
    /// The program stopped, giving a reason this build has no word for.
    Stopped(u8),
    /// The program stopped without saying why.
    Unsaid,
    /// The board says nothing is executing and sent no stop: it reset under the program.
    NoLongerRunning,
    /// The connection dropped while the program ran, so how it ended is not known.
    Dropped(TransportError),
    /// The watch's limit, this long, passed with the program still running. It goes on running on
    /// the board, which stores it.
    StillRunning(Duration),
}

impl RunEnd {
    /// The sentence saying how the run ended, which names no command. `target` names the
    /// connection, for a run that lost it.
    #[must_use]
    pub fn describe(&self, target: &str) -> String {
        match self {
            Self::Done(code) => format!("the program ended, exit code {code}."),
            Self::Trapped => "the program stopped: it trapped, as reported above.".to_owned(),
            Self::Aborted => "the program stopped: something else connected to the board and aborted it.".to_owned(),
            Self::Stopped(other) => {
                format!("the program stopped, giving reason {other}, which this build has no word for.")
            }
            Self::Unsaid => "the program stopped without saying why.".to_owned(),
            Self::NoLongerRunning => "the program is no longer running on the board, and the board reported no stop: it \
                 says nothing is\nexecuting. A board that resets -- a firmware fault, or running out \
                 of memory -- ends a program\nthis way, with its output stopping where it ended."
                .to_owned(),
            Self::Dropped(error) => format!(
                "the connection to {target} dropped while the program was running ({error:?}), so how \
                 it\nended is not known. A board's own USB leaves the bus when the board resets -- when \
                 its firmware\naborts, out of memory or on a fault, or when it loses power."
            ),
            Self::StillRunning(limit) => format!(
                "the program was still running after {} s, and it goes on running on the board, which \
                 stores it\nand runs it at every reset until another is deployed. The next connection \
                 to the board takes it back.",
                limit.as_secs()
            ),
        }
    }

    /// Whether the program ran to its end and returned 0.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Done(0))
    }
}

/// Store `image` on the board at the other end of `transport`, whose HELLO `session` is, start it
/// under the board's scheduler without a reset, and write what it prints until it ends or `watch`
/// does.
///
/// **NO RESET, AND THAT IS WHAT KEEPS THE FIRST LINE.** A start by reset takes a board's own USB off
/// the bus, and whatever the program printed before a host found the board again was lost. Started
/// this way the board answers on the carrier this route is already reading. The session must have
/// [`Capabilities::EXEC_NO_RESET`] ([`run_caps`] offers it).
///
/// What the store says, and the program's standard output, go to `out`; the program's standard
/// error and debug channel go to `err`.
///
/// # Errors
/// The store's refusal ([`store_image`]), or a start the board did not make: each a sentence that
/// names no command.
pub fn run_scheduled(
    transport: &mut impl Transport,
    session: &Negotiated,
    target: &str,
    image: &[u8],
    out: &mut dyn Write,
    err: &mut dyn Write,
    watch: Watch,
) -> Result<RunEnd, String> {
    store_image(transport, session, target, image, out)?;
    if let Err(failure) = start_execution(
        transport,
        RUN_SEQ,
        exec::exec_source::DEPLOYED,
        exec::exec_flags::NO_RESET,
        START_PATIENCE,
    ) {
        return Err(format!(
            "the program is stored on {target}, and {failure}. It starts at the board's next reset."
        ));
    }
    let _ = writeln!(out, "running on {target}; output follows.\n");
    let _ = out.flush();
    Ok(follow(transport, RUN_SEQ, out, err, watch))
}

/// Write what the run at `seq` prints until it ends, or until `watch` does, and say how it ended.
///
/// **IT ASKS WHEN THE PROGRAM HAS BEEN QUIET A WHILE.** A board whose firmware reset under the
/// program -- a fault, or running out of memory -- sends no stop, and over a serial line its carrier
/// stays open. The question is asked at the run's own sequence number, and a board still running the
/// program answers that it is and carries on.
fn follow(transport: &mut impl Transport, seq: u16, out: &mut dyn Write, err: &mut dyn Write, watch: Watch) -> RunEnd {
    let mut run = RunCollector::new(seq);
    let mut terminal = TerminalOutput::new(out, err);
    let mut heard = Instant::now();
    let until = watch.limit.map(|limit| heard + limit);
    let mut asked: Option<Instant> = None;
    loop {
        let mut printed = false;
        let polled = run.poll_streaming(transport, &mut |chunk| {
            printed = true;
            let _ = terminal.show(chunk, None);
        });
        if printed {
            heard = Instant::now();
        }
        match polled {
            Ok(true) => {
                let _ = terminal.end_open_line();
                return ended(&run);
            }
            Ok(false) => {
                let now = Instant::now();
                if let (Some(until), Some(limit)) = (until, watch.limit)
                    && now >= until
                {
                    let _ = terminal.end_open_line();
                    return RunEnd::StillRunning(limit);
                }
                let quiet_long = now.duration_since(heard) >= watch.quiet;
                let asked_lately = asked.is_some_and(|at| now.duration_since(at) < watch.quiet);
                if quiet_long && !asked_lately && transport.send(exec::EXEC_STATUS, seq, &[]).is_ok() {
                    asked = Some(now);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = terminal.end_open_line();
                return RunEnd::Dropped(error);
            }
        }
    }
}

/// How the run `run` followed ended, from what its stop said.
fn ended(run: &RunCollector) -> RunEnd {
    use crate::debug::reason;
    match (run.stop_reason(), run.exit_value()) {
        (None, _) if run.start_refusal() == Some(exec::ack::IDLE) => RunEnd::NoLongerRunning,
        (Some(reason::DONE), Some(code)) => RunEnd::Done(code),
        (Some(reason::TRAP), _) => RunEnd::Trapped,
        (Some(reason::ABORTED), _) => RunEnd::Aborted,
        (Some(other), _) => RunEnd::Stopped(other),
        (None, _) => RunEnd::Unsaid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target that answers the first frame it is sent with the frames `answer` builds.
    struct Answering {
        wire: lamella_wire::MemTransport,
        answer: Option<Vec<u8>>,
    }

    impl Answering {
        fn with(answer: impl FnOnce(&mut lamella_wire::MemTransport)) -> Self {
            let mut peer = lamella_wire::MemTransport::new();
            answer(&mut peer);
            Answering { wire: lamella_wire::MemTransport::new(), answer: Some(peer.take_sent()) }
        }
    }

    impl Transport for Answering {
        fn send(&mut self, _msg_type: u8, _seq: u16, _payload: &[u8]) -> Result<(), TransportError> {
            if let Some(answer) = self.answer.take() {
                self.wire.feed(&answer);
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
            Transport::poll(&mut self.wire)
        }
    }

    /// A board answering a start with the acknowledgement `code`.
    fn acknowledging(code: u8) -> Answering {
        Answering::with(|peer| {
            Transport::send(peer, exec::EXEC_ACK, 2, &[code]).unwrap();
        })
    }

    /// A start is reported as made only when the board acknowledges it. A start the board refuses is
    /// reported with the board's reason, a board that does not answer has not been shown to start, and
    /// either way the store itself is said to have succeeded.
    #[test]
    fn a_start_is_reported_as_made_only_when_the_board_acknowledges_it() {
        let quick = Duration::from_millis(50);
        let started = start_deployed(&mut acknowledging(exec::ack::STARTED), quick);
        assert_eq!(started, Ok("started it."));

        let refused = start_deployed(&mut acknowledging(exec::ack::NOTHING_TO_RUN), quick)
            .expect_err("a refused start did not start");
        assert!(refused.contains("NOTHING_TO_RUN"), "names the board's reason: {refused}");
        assert!(refused.contains("the image is on the board"), "and that the store succeeded: {refused}");

        let silent = start_deployed(&mut Answering::with(|_| {}), quick).expect_err("silence is not a start");
        assert!(silent.contains("did not acknowledge"), "{silent}");
        assert!(silent.contains("the image is on the board"), "{silent}");
    }

    /// **A QUARTER IS SAID ONCE, AND THE LAST ONE NOT AT ALL**: the store's closing line says that,
    /// with the time the whole took.
    #[test]
    fn a_store_says_each_quarter_once_and_leaves_the_last_to_its_closing_line() {
        let mut quarters = Quarters::of(1000);
        let said: Vec<Option<usize>> = [100, 250, 260, 600, 760, 999, 1000].iter().map(|&at| quarters.passed(at)).collect();
        assert_eq!(said, [None, Some(1), None, Some(2), Some(3), None, None]);
        assert_eq!(Quarters::of(0).passed(0), None, "an empty image has no quarters");
        let mut one_chunk = Quarters::of(200);
        assert_eq!(one_chunk.passed(200), None, "an image of one chunk is said only by the closing line");
    }

    /// Each refusal reason has its own sentence, and one this build does not know is named.
    #[test]
    fn a_refusal_reason_is_a_sentence_with_its_remedy() {
        assert!(describe_refusal(lamella_wire::error::SESSION_HELD).contains("another carrier already holds"));
        assert!(describe_refusal(lamella_wire::error::UNKNOWN_MESSAGE_TYPE).contains("Stop asking"));
        assert_eq!(describe_refusal(200), "refusal reason 200, which this build has no description for");
        assert_eq!(the_board(Some("Pico 2 W")), "the Pico 2 W");
        assert_eq!(the_board(None), "the board");
        assert_eq!(capitalized("the board"), "The board");
    }

    /// A carrier carrying the frames a board sent, which records what the host sends and answers
    /// an `EXEC_STATUS` with `status` -- or with nothing.
    struct Board {
        wire: lamella_wire::MemTransport,
        status: Option<u8>,
        asked: usize,
    }

    impl Board {
        fn sending(frames: &[(u8, u16, Vec<u8>)], status: Option<u8>) -> Self {
            let mut board = lamella_wire::MemTransport::new();
            for (msg_type, seq, payload) in frames {
                Transport::send(&mut board, *msg_type, *seq, payload).unwrap();
            }
            let mut wire = lamella_wire::MemTransport::new();
            wire.feed(&board.take_sent());
            Self { wire, status, asked: 0 }
        }
    }

    impl Transport for Board {
        fn send(&mut self, msg_type: u8, seq: u16, _payload: &[u8]) -> Result<(), TransportError> {
            if msg_type == exec::EXEC_STATUS {
                self.asked += 1;
                if let Some(status) = self.status {
                    let mut board = lamella_wire::MemTransport::new();
                    Transport::send(&mut board, exec::EXEC_ACK, seq, &[status]).unwrap();
                    self.wire.feed(&board.take_sent());
                }
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
            Transport::poll(&mut self.wire)
        }
    }

    /// An `EVT_OUTPUT` of `text` on standard output.
    fn printed(text: &str) -> (u8, u16, Vec<u8>) {
        let mut payload = vec![crate::debug::output::STDOUT, 0];
        payload.extend_from_slice(text.as_bytes());
        (crate::debug::EVT_OUTPUT, 0, payload)
    }

    /// The `EVT_STOPPED` a program that returned `exit` ends with, at `seq`.
    fn ended_with(seq: u16, exit: i32) -> (u8, u16, Vec<u8>) {
        let mut payload = vec![crate::debug::reason::DONE];
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&exit.to_le_bytes());
        payload.push(0);
        (crate::debug::EVT_STOPPED, seq, payload)
    }

    /// A watch of `quiet` with no limit.
    fn quiet(quiet: Duration) -> Watch {
        Watch { quiet, limit: None }
    }

    /// **A RUN IS FOLLOWED TO THE STOP AT ITS OWN SEQUENCE NUMBER**, and only that stop ends it: a
    /// stop the board sent for something else is not this program's end.
    #[test]
    fn a_run_is_followed_to_its_own_stop_and_its_exit_code_is_reported() {
        let mut board =
            Board::sending(&[printed("one\n"), ended_with(0, 9), printed("two\n"), ended_with(RUN_SEQ, 3)], None);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = follow(&mut board, RUN_SEQ, &mut out, &mut err, quiet(Duration::from_secs(60)));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(end, RunEnd::Done(3));
        assert!(!end.succeeded(), "exit 3 is reported, and is not success");
        assert_eq!(end.describe("loopback"), "the program ended, exit code 3.");
        assert_eq!(out, "one\ntwo\n", "both lines");
        assert_eq!(board.asked, 0, "a program that keeps talking is not asked about");
    }

    /// **A STOP OUTRANKS THE ANSWER THAT FOLLOWS IT.** A board busy before the program -- joining its
    /// network -- answers "is it still running?" only after the program ends, right behind the stop,
    /// and the stop is how the program ended.
    #[test]
    fn a_stop_followed_by_an_idle_answer_is_reported_as_the_stop() {
        let idle = (exec::EXEC_ACK, RUN_SEQ, vec![exec::ack::IDLE]);
        let mut board = Board::sending(&[printed("hello\n"), ended_with(RUN_SEQ, 0), idle], None);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = follow(&mut board, RUN_SEQ, &mut out, &mut err, quiet(Duration::from_secs(60)));
        assert_eq!(end, RunEnd::Done(0));
        assert!(end.succeeded());
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    /// **A PROGRAM THE BOARD NO LONGER RUNS IS NOT WAITED ON FOREVER.** After a quiet spell the board
    /// is asked, and a board that says nothing is executing ends the run as one that went away.
    #[test]
    fn a_quiet_run_the_board_no_longer_executes_ends_as_gone() {
        let mut board = Board::sending(&[printed("started\n")], Some(exec::ack::IDLE));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = follow(&mut board, RUN_SEQ, &mut out, &mut err, quiet(Duration::from_millis(20)));
        assert_eq!(end, RunEnd::NoLongerRunning);
        assert!(String::from_utf8(out).unwrap().contains("started\n"));
        assert!(end.describe("loopback").contains("no longer running on the board"), "{}", end.describe("loopback"));
        assert_eq!(board.asked, 1, "asked once, and answered");
    }

    /// **A QUIET PROGRAM THE BOARD IS STILL RUNNING IS LEFT TO RUN**, asked about once per quiet
    /// spell rather than on every poll.
    #[test]
    fn a_quiet_run_the_board_still_executes_is_asked_about_once_per_spell() {
        let mut board = Board::sending(&[], Some(exec::ack::RUNNING));
        let spell = Duration::from_millis(40);
        let end = {
            let mut ending = Ending { board: &mut board, at: Instant::now() + spell * 9, sent: false };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            follow(&mut ending, RUN_SEQ, &mut out, &mut err, quiet(spell))
        };
        assert_eq!(end, RunEnd::Done(0), "the run ended with exit 0");
        assert!((4..=10).contains(&board.asked), "asked once per quiet spell, not on every poll: {}", board.asked);
    }

    /// **A WATCH WITH A LIMIT ENDS THERE, AND SAYS THE PROGRAM IS STILL RUNNING**: what it printed so
    /// far is shown, and the program is said to go on running on the board, which stores it.
    #[test]
    fn a_watch_with_a_limit_ends_there_and_says_the_program_runs_on() {
        let mut board = Board::sending(&[printed("tick")], Some(exec::ack::RUNNING));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let watch = Watch { quiet: Duration::from_millis(20), limit: Some(Duration::from_millis(150)) };
        let began = Instant::now();
        let end = follow(&mut board, RUN_SEQ, &mut out, &mut err, watch);
        assert!(began.elapsed() >= Duration::from_millis(150), "it watched until the limit: {:?}", began.elapsed());
        assert_eq!(end, RunEnd::StillRunning(Duration::from_millis(150)));
        assert_eq!(String::from_utf8(out).unwrap(), "tick\n", "what it printed, its open line ended");
        let said = end.describe("loopback");
        assert!(said.starts_with("the program was still running after 0 s, and it goes on running"), "{said}");
        assert!(said.contains("The next connection to the board takes it back."), "{said}");
    }

    /// **A CONNECTION THAT DROPS ENDS THE RUN, NAMING THE CONNECTION**, and not as a program that
    /// ended.
    #[test]
    fn a_connection_that_drops_ends_the_run_as_unknown() {
        struct Gone;
        impl Transport for Gone {
            fn send(&mut self, _: u8, _: u16, _: &[u8]) -> Result<(), TransportError> {
                Ok(())
            }
            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
                Err(TransportError::Closed)
            }
        }
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = follow(&mut Gone, RUN_SEQ, &mut out, &mut err, quiet(Duration::from_secs(60)));
        assert_eq!(end, RunEnd::Dropped(TransportError::Closed));
        assert!(end.describe("usb").starts_with("the connection to usb dropped while the program was running (Closed)"));
    }

    /// A [`Board`] that sends the run's ending stop once `at` has passed.
    struct Ending<'a> {
        board: &'a mut Board,
        at: Instant,
        sent: bool,
    }

    impl Transport for Ending<'_> {
        fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
            self.board.send(msg_type, seq, payload)
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
            if !self.sent && Instant::now() >= self.at {
                self.sent = true;
                let (msg_type, seq, payload) = ended_with(RUN_SEQ, 0);
                let mut board = lamella_wire::MemTransport::new();
                Transport::send(&mut board, msg_type, seq, &payload).unwrap();
                self.board.wire.feed(&board.take_sent());
            }
            self.board.poll()
        }
    }

    /// The tests that need a board's own deploy serve, in this process.
    #[cfg(feature = "baked")]
    mod against_the_firmware_serve {
        use super::super::*;
        use crate::loopback::{LoopbackBoard, RamFlash};

        /// The HELLO a host storing a program makes, answered by `board`.
        fn hello(board: &mut LoopbackBoard, caps: Capabilities) -> Negotiated {
            crate::hello_blocking(board, 0, caps, TIMEOUT).expect("the board answers a HELLO")
        }

        /// **A STORE AGAINST THE FIRMWARE'S OWN SERVE**: the image is written in chunks sized for the
        /// carrier, every chunk's CRC compared, with a line each quarter of the way.
        #[test]
        fn a_store_writes_in_chunks_for_its_carrier_and_says_each_quarter() {
            let image: Vec<u8> = (0..40_000u32).map(|at| (at % 251) as u8).collect();
            let mut board = LoopbackBoard::on(RamFlash::holding(&[]));
            let session = hello(&mut board, host_caps());
            assert!(
                session.caps.has(Capabilities::DEPLOY_PREFIX_CRC),
                "both ends offer the compared CRC, so every acknowledgement is checked"
            );
            let mut out = Vec::new();
            let stored = store_image(&mut board, &session, "usb", &image, &mut out);
            let out = String::from_utf8(out).unwrap();
            assert_eq!(stored, Ok(Stored::Written), "{out}");
            assert!(out.contains("deploying 40000 B to usb, 8192 B per chunk"), "{out}");
            for quarter in ["   25%  16384 B", "   50%  24576 B", "   75%  32768 B"] {
                assert!(out.contains(quarter), "{quarter}: {out}");
            }
            assert!(out.contains("deployed 40000 B to usb in "), "{out}");
            assert_eq!(board.chunks, 5, "40,000 bytes is five chunks of 8 KB or fewer");
            assert_eq!(&board.flash.region[..image.len()], &image[..], "the board's flash holds the image");
        }

        /// **A SERIAL LINE GETS THE SMALL CHUNKS**, which the smallest receive rings in this tree hold.
        #[test]
        fn a_store_over_a_serial_line_sends_256_byte_chunks() {
            let image = vec![0x5Au8; 1000];
            let mut board = LoopbackBoard::on(RamFlash::holding(&[]));
            let session = hello(&mut board, host_caps());
            let mut out = Vec::new();
            let stored = store_image(&mut board, &session, "COM8", &image, &mut out);
            let out = String::from_utf8(out).unwrap();
            assert_eq!(stored, Ok(Stored::Written), "{out}");
            assert!(out.contains("deploying 1000 B to COM8, 256 B per chunk"), "{out}");
            assert_eq!(board.chunks, 4, "1000 bytes is four chunks of 256 bytes or fewer");
        }

        /// **A BOARD THAT ALREADY STORES THE PROGRAM IS SENT NOTHING**: its answer about what it stores
        /// is verified against its flash, so a match is the program itself.
        #[test]
        fn a_board_that_already_stores_the_program_is_sent_nothing() {
            let Some(baked) = crate::bake::tests::baked_for(
                "class Program\n{\n    static int Main()\n    {\n        return 42;\n    }\n}\n",
                None,
            ) else {
                return;
            };
            let image = baked.expect("a program bakes").image;
            let mut board = LoopbackBoard::on(RamFlash::holding(&[]));
            let session = hello(&mut board, host_caps());
            let mut first = Vec::new();
            assert_eq!(store_image(&mut board, &session, "usb", &image, &mut first), Ok(Stored::Written));
            assert_eq!(
                lamella_cil_runtime::verified_image_checksum(board.flash.region),
                baked_image_checksum(&image),
                "the board's flash holds the program, verified"
            );
            let chunks = board.chunks;

            let mut again = Vec::new();
            let stored = store_image(&mut board, &session, "usb", &image, &mut again);
            let again = String::from_utf8(again).unwrap();
            assert_eq!(stored, Ok(Stored::AlreadyThere), "{again}");
            assert!(again.contains("already stores this program, so nothing was sent"), "{again}");
            assert_eq!(board.chunks, chunks, "and no chunk crossed");
        }

        /// **A RUN AGAINST THE FIRMWARE's OWN SERVE, WITH THREADS.** The board takes the program
        /// through the deploy serve and runs it without a reset under the scheduler: a second thread,
        /// a lock both take and a join, which the debugger's one-thread session refuses. Every line it
        /// prints and its exit code reach the host.
        #[test]
        fn a_program_that_starts_a_thread_runs_to_its_end_and_is_followed() {
            let Some(baked) = crate::bake::tests::baked_for(
                "using System;\nusing System.Threading;\n\npublic static class Program\n{\n    \
                 static readonly object Gate = new object();\n    static int count;\n\n    \
                 public static int Main()\n    {\n        Thread worker = new Thread(Work);\n        \
                 worker.Start();\n        Count(\"main\");\n        worker.Join();\n        \
                 Console.WriteLine(\"joined, count \" + count);\n        return count;\n    }\n\n    \
                 static void Work()\n    {\n        Count(\"worker\");\n    }\n\n    \
                 static void Count(string who)\n    {\n        for (int i = 0; i < 3; i++)\n        {\n            \
                 lock (Gate)\n            {\n                count++;\n            }\n            \
                 Console.WriteLine(who + \" \" + i);\n            Thread.Sleep(1);\n        }\n    }\n}\n",
                None,
            ) else {
                return;
            };
            let image = baked.expect("a program bakes").image;
            let mut board = LoopbackBoard::on(RamFlash::holding(&[]));
            let session = hello(&mut board, run_caps());
            assert!(session.caps.has(Capabilities::EXEC_NO_RESET), "the firmware's serve starts without a reset");

            let (mut out, mut err) = (Vec::new(), Vec::new());
            let end = run_scheduled(&mut board, &session, "usb", &image, &mut out, &mut err, Watch::to_the_end());
            let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
            assert!(!board.reset, "the board did not reset to start it: {out}{err}");
            assert!(out.contains("running on usb; output follows."), "{out}{err}");
            for line in ["main 0", "main 2", "worker 0", "worker 2", "joined, count 6"] {
                assert!(out.contains(line), "{line}: {out}{err}");
            }
            assert_eq!(end, Ok(RunEnd::Done(6)), "{out}{err}");
            assert_eq!(
                lamella_cil_runtime::verified_image_checksum(board.flash.region),
                baked_image_checksum(&image),
                "and the board stores the program it ran"
            );
        }
    }
}
