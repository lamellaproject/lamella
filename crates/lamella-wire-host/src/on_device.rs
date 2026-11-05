//! A program sent to firmware already running on a board: stored in the board's flash, started,
//! and followed to its end. The `lamella` command's `deploy --device` and `run --device` take this
//! route, and so do the MCP server's `lamella_deploy` and `lamella_run_on_device`, so a program
//! reaches a board the same way from either front end and its run is reported in the same words.
//!
//! Nothing here names a command. A failure is a sentence that a front end puts its own name in
//! front of, and how a run ended is a [`RunEnd`] that a front end reports.

use crate::terminal::TerminalOutput;
use crate::{
    Aborted, DeployProgress, RunCollector, TransferAck, abort_blocking, baked_image_checksum,
    board_name, classify_target, deploy_chunk_len, deploy_image_reporting, deploy_status_blocking,
    exec, start_execution,
};
use lamella_wire::{Capabilities, Negotiated, Transport, TransportError};
use std::io::Write;
use std::time::{Duration, Instant};

/// How long each step of a store waits for the board's answer. Generous: a deploy erases and
/// writes flash on the board, which is slow in a way a timeout should not be racing.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// What a run says before clearing the program's flash area.
pub const ERASING: &str = "Erasing the program area of flash...";
/// What a run says when its host stops following the board.
pub const DISCONNECTED: &str = "Disconnected from the board.";

/// How much progress a caller shows around a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wording {
    /// Transfer details for an interactive tool.
    Full,
    /// The erase notice and the program for a project run.
    Brief,
}

/// Store an image with the caller's choice of progress detail.
///
/// # Errors
/// The same storage refusals as [`store_image`].
pub fn store_image_worded(
    transport: &mut impl Transport,
    session: &Negotiated,
    target: &str,
    image: &[u8],
    out: &mut dyn Write,
    wording: Wording,
) -> Result<Stored, String> {
    store_image_within(transport, session, target, image, out, TIMEOUT, wording)
}

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

pub use crate::describe_refusal;

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
    /// The image was written: the board acknowledged every chunk of it, or, asked again after an
    /// acknowledgement did not arrive, it holds the image, verified.
    Written,
}

/// The sequence numbers [`store_image`]'s steps go out under. Each step waits for the answer to its
/// own, so no two steps of one session may share one -- and none of these is a start's, which
/// follows them on the same session.
const TAKE_BACK_SEQ: u16 = 7;
const STORED_SEQ: u16 = 8;
const DEPLOY_SEQ: u16 = 1;
/// The question asked again, after a chunk's acknowledgement did not arrive: what the board holds.
const ASKED_AGAIN_SEQ: u16 = 10;

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
///    A board that reports its deploy window clears it before it answers the first chunk, so
///    where that clear is expected to take long enough to notice, over about 10 s, a line says so
///    before the wait, and how long the wait may last.
/// 4. A chunk whose acknowledgement does not arrive is not yet a failure: the board is asked once
///    what it stores, and one that holds this image, verified, has stored it, which a line says.
///
/// # Errors
/// A board that answered the HELLO and then not the take-back, or one that would not take the
/// image, or one that did not acknowledge a chunk and, asked again, does not hold the image or does
/// not answer: each in a sentence of its own, which names no command.
pub fn store_image(
    transport: &mut impl Transport,
    session: &Negotiated,
    target: &str,
    image: &[u8],
    out: &mut dyn Write,
) -> Result<Stored, String> {
    store_image_worded(transport, session, target, image, out, Wording::Full)
}

/// [`store_image`], waiting `timeout` for each of the board's answers.
#[allow(clippy::too_many_arguments)]
fn store_image_within(
    transport: &mut impl Transport,
    session: &Negotiated,
    target: &str,
    image: &[u8],
    out: &mut dyn Write,
    timeout: Duration,
    wording: Wording,
) -> Result<Stored, String> {
    let name = board_name(session.identity.product_model);
    let board = the_board(name);
    match abort_blocking(transport, TAKE_BACK_SEQ, timeout) {
        Ok(Aborted::StoppedAProgram) => {
            if wording == Wording::Full {
                let _ = writeln!(out, "Stopped the program {board} was running.");
            }
        }
        Ok(Aborted::NothingWasRunning) => {}
        Err(TransportError::Refused { .. }) => {}
        Err(error) => {
            if wording == Wording::Brief {
                return Err(format!("{} stopped answering before the program could be sent.", capitalized(&board)));
            }
            return Err(format!(
                "{board} answered the connection and then did not answer an ABORT within {} s \
                 ({}):\nit is neither waiting for a host nor running a program this tool can \
                 stop. Nothing was sent.",
                timeout.as_secs(),
                crate::link_failure(error)
            ));
        }
    }
    let held = deploy_status_blocking(transport, STORED_SEQ, timeout)
        .ok()
        .and_then(|status| status.checksum);
    if held.is_some() && held == baked_image_checksum(image) {
        if wording == Wording::Full {
            let _ = writeln!(out, "{} already stores this program, so nothing was sent.", capitalized(&board));
        }
        return Ok(Stored::AlreadyThere);
    }
    let chunk_len = deploy_chunk_len(session, classify_target(target));
    if wording == Wording::Full {
        let _ = writeln!(out, "Deploying {} B to {target}, {chunk_len} B per chunk", image.len());
    } else {
        let _ = writeln!(out, "{ERASING}");
    }
    let _ = out.flush();
    let started = Instant::now();
    let mut quarters = Quarters::of(image.len());
    let mut first_wait = None;
    let mut acknowledged = 0;
    let sent = deploy_image_reporting(
        transport,
        DEPLOY_SEQ,
        image,
        chunk_len,
        timeout,
        session.caps,
        &mut |event| match event {
            DeployProgress::Clearing { wait, expected } => {
                first_wait = Some(wait);
                if wording == Wording::Full && expected > NOTICEABLE_CLEAR {
                    let _ = writeln!(
                        out,
                        "{} clears its program area first; this can take up to {} s.",
                        capitalized(&board),
                        seconds_rounded_up(wait)
                    );
                    let _ = out.flush();
                }
            }
            DeployProgress::Acknowledged(written) => {
                acknowledged = written;
                if let Some(quarter) = quarters.passed(written) && wording == Wording::Full {
                    let _ = writeln!(
                        out,
                        "  {:>3}%  {written} B  {:.1} s",
                        quarter * 25,
                        started.elapsed().as_secs_f64()
                    );
                    let _ = out.flush();
                }
            }
        },
    );
    match sent {
        Ok(TransferAck::Accepted) => {
            if wording == Wording::Full {
                let _ = writeln!(out, "Deployed {} B to {target} in {:.1} s", image.len(), started.elapsed().as_secs_f64());
            }
            Ok(Stored::Written)
        }
        Ok(not_accepted) => Err(format!("{}.", not_accepted.describe(name))),
        Err(TransportError::Refused { reason, .. }) => {
            Err(format!("{target} refused the deploy: {}", describe_refusal(reason)))
        }
        Err(TransportError::Closed) => {
            let wanted = baked_image_checksum(image);
            let asked = deploy_status_blocking(transport, ASKED_AGAIN_SEQ, timeout);
            let waited = match first_wait {
                Some(wait) if acknowledged == 0 => wait,
                _ => timeout,
            };
            let seconds = seconds_rounded_up(waited);
            match asked.map(|status| wanted.is_some() && status.checksum == wanted) {
                Ok(true) => {
                    if wording == Wording::Full {
                        let _ = writeln!(
                        out,
                        "{} did not acknowledge a chunk of the deploy within {seconds} s; asked \
                         again, it stores this program, verified.",
                        capitalized(&board)
                        );
                    }
                    Ok(Stored::Written)
                }
                Ok(false) if wording == Wording::Brief => Err(format!("{} did not store the program.", capitalized(&board))),
                Ok(false) => Err(format!(
                    "deploy to {target} failed: {board} did not acknowledge a chunk of the deploy \
                     within {seconds} s, and asked again, it does not store this program."
                )),
                Err(_) if wording == Wording::Brief => Err(format!("{} stopped answering while the program was being sent.", capitalized(&board))),
                Err(_) => Err(format!(
                    "deploy to {target} failed: {board} stopped answering during the deploy and \
                     did not answer when asked again what it stores, so whether it stores this \
                     program is not known."
                )),
            }
        }
        Err(error) => Err(format!("deploy to {target} failed: {}.", crate::link_failure(error))),
    }
}

/// How long a board's deploy window must be expected to take to clear before a store says that the
/// first chunk waits for the clear. A shorter clear passes among the deploy's other lines, and a
/// line naming the first chunk's whole wait before an answer that comes at once makes a quick
/// deploy look slow.
const NOTICEABLE_CLEAR: Duration = Duration::from_secs(10);

/// `wait` in whole seconds, rounded up, so a wait said in seconds is never said shorter than it is.
fn seconds_rounded_up(wait: Duration) -> u64 {
    wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
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
#[derive(Clone, Copy)]
pub struct Watch<'a> {
    /// How long the program may be quiet before the board is asked.
    pub quiet: Duration,
    /// How long after the start the watch ends, the program still running; `None` watches it to
    /// its end.
    pub limit: Option<Duration>,
    /// A host request to stop following, without sending any command to the board.
    pub stop: Option<&'a dyn Fn() -> bool>,
}

impl std::fmt::Debug for Watch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch").field("quiet", &self.quiet).field("limit", &self.limit)
            .field("stoppable", &self.stop.is_some()).finish()
    }
}

impl Watch<'_> {
    /// A watch that lasts until the run ends, however long that is.
    #[must_use]
    pub fn to_the_end() -> Self {
        Self { quiet: QUIET_BEFORE_ASKING, limit: None, stop: None }
    }

    /// A watch that lasts until the run ends or until `limit` has passed since it started,
    /// whichever comes first.
    #[must_use]
    pub fn for_at_most(limit: Duration) -> Self {
        Self { quiet: QUIET_BEFORE_ASKING, limit: Some(limit), stop: None }
    }
}

/// How a watched run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// The host stopped following; no command was sent to the board.
    Interrupted,
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
        capitalized(&match self {
            Self::Interrupted => DISCONNECTED.to_owned(),
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
            Self::Dropped(_) => format!(
                "the connection to {target} dropped while the program was running, so how it\nended is not \
                 known. A board's own USB leaves the bus when the board resets -- when its firmware\naborts, \
                 out of memory or on a fault, or when it loses power."
            ),
            Self::StillRunning(limit) => format!(
                "the program was still running after {} s, and it goes on running on the board, which \
                 stores it\nand runs it at every reset until another is deployed. The next connection \
                 to the board takes it back.",
                limit.as_secs()
            ),
        })
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
/// What the store and the start say goes to `say`; the program's standard output goes to `out`,
/// and its standard error and debug channel to `err`.
///
/// # Errors
/// The store's refusal ([`store_image`]), or a start the board did not make: each a sentence that
/// names no command.
#[allow(clippy::too_many_arguments)]
pub fn run_scheduled(
    transport: &mut impl Transport,
    session: &Negotiated,
    target: &str,
    image: &[u8],
    say: &mut dyn Write,
    out: &mut dyn Write,
    err: &mut dyn Write,
    watch: Watch<'_>,
) -> Result<RunEnd, String> {
    store_image(transport, session, target, image, say)?;
    start_scheduled(transport, target, say, out, err, watch)
}

/// The second half of [`run_scheduled`], for a program [`store_image`] has stored on the board at
/// the other end of `transport`: started under the board's scheduler without a reset, and followed
/// as [`run_scheduled`] follows it. A front end that says something of its own once the program is
/// on the board says it between the two.
///
/// # Errors
/// A start the board did not make, in a sentence that names no command.
pub fn start_scheduled(
    transport: &mut impl Transport,
    target: &str,
    say: &mut dyn Write,
    out: &mut dyn Write,
    err: &mut dyn Write,
    watch: Watch<'_>,
) -> Result<RunEnd, String> {
    start_scheduled_worded(transport, target, say, out, err, watch, Wording::Full)
}

/// Start and follow a stored program with the caller's choice of progress detail.
///
/// # Errors
/// The same start refusals as [`start_scheduled`].
#[allow(clippy::too_many_arguments)]
pub fn start_scheduled_worded(
    transport: &mut impl Transport,
    target: &str,
    say: &mut dyn Write,
    out: &mut dyn Write,
    err: &mut dyn Write,
    watch: Watch<'_>,
    wording: Wording,
) -> Result<RunEnd, String> {
    if watch.stop.is_some_and(|stop| stop()) {
        return Ok(RunEnd::Interrupted);
    }
    if let Err(failure) = start_execution(
        transport,
        RUN_SEQ,
        exec::exec_source::DEPLOYED,
        exec::exec_flags::NO_RESET,
        START_PATIENCE,
    ) {
        if wording == Wording::Brief {
            return Err(format!("The program could not start on {target}: {failure}."));
        }
        return Err(format!(
            "the program is stored on {target}, and {failure}. It starts at the board's next reset."
        ));
    }
    if wording == Wording::Full {
        let _ = writeln!(say, "Running on {target}; output follows.");
    }
    let _ = writeln!(say);
    let _ = say.flush();
    Ok(follow(transport, RUN_SEQ, say, out, err, watch))
}

/// Write what the run at `seq` prints until it ends, or until `watch` does, and say how it ended.
///
/// **IT ASKS WHEN THE PROGRAM HAS BEEN QUIET A WHILE.** A board whose firmware reset under the
/// program -- a fault, or running out of memory -- sends no stop, and over a serial line its carrier
/// stays open. The question is asked at the run's own sequence number, and a board still running the
/// program answers that it is and carries on. A question the carrier cannot send ends the run as
/// [`RunEnd::Dropped`].
///
/// **A BOARD THAT WILL NOT TAKE THE QUESTION HAS NOT GONE AWAY.** Busy enough to stop reading its USB, it
/// may go on printing and read again, so the run says so once and goes on being followed, and the
/// question is asked again. A board that has gone away says so through the read, which ends the run.
///
/// **BYTES THAT MADE NO MESSAGE ARE SAID, TO `say`, AS THEY ARE DROPPED.** A frame the carrier lost part of
/// is thrown away whole, and it may have held a line the program printed, so without this the line is
/// simply gone, with exit 0. What arrived before the run started is not counted: a carrier's first bytes can
/// be the end of something older.
fn follow(
    transport: &mut impl Transport,
    seq: u16,
    say: &mut dyn Write,
    out: &mut dyn Write,
    err: &mut dyn Write,
    watch: Watch<'_>,
) -> RunEnd {
    let mut run = RunCollector::new(seq);
    let mut terminal = TerminalOutput::new(out, err);
    let mut heard = Instant::now();
    let until = watch.limit.map(|limit| heard + limit);
    let mut asked: Option<Instant> = None;
    let mut dropped = transport.discarded_bytes();
    let mut said_it_stopped_taking_data = false;
    loop {
        if watch.stop.is_some_and(|stop| stop()) {
            let _ = terminal.end_open_line();
            return RunEnd::Interrupted;
        }
        let mut printed = false;
        let polled = run.poll_streaming(transport, &mut |chunk| {
            printed = true;
            let _ = terminal.show(chunk, None);
        });
        if printed {
            heard = Instant::now();
        }
        if let (Some(before), Some(now)) = (dropped, transport.discarded_bytes())
            && now > before
        {
            let _ = terminal.end_open_line();
            let _ = writeln!(say, "{}", bytes_dropped(now - before));
            let _ = say.flush();
            dropped = Some(now);
        }
        match polled {
            Ok(true) => {
                let _ = terminal.end_open_line();
                return ended(&run);
            }
            Ok(false) => {
                if watch.stop.is_some_and(|stop| stop()) {
                    let _ = terminal.end_open_line();
                    return RunEnd::Interrupted;
                }
                let now = Instant::now();
                if let (Some(until), Some(limit)) = (until, watch.limit)
                    && now >= until
                {
                    let _ = terminal.end_open_line();
                    return RunEnd::StillRunning(limit);
                }
                let quiet_long = now.duration_since(heard) >= watch.quiet;
                let asked_lately = asked.is_some_and(|at| now.duration_since(at) < watch.quiet);
                if quiet_long && !asked_lately {
                    match transport.send(exec::EXEC_STATUS, seq, &[]) {
                        Ok(()) => {}
                        Err(TransportError::Stalled) => {
                            if !said_it_stopped_taking_data {
                                let _ = terminal.end_open_line();
                                let _ = writeln!(say, "{STOPPED_TAKING_DATA}");
                                let _ = say.flush();
                                said_it_stopped_taking_data = true;
                            }
                        }
                        Err(error) => {
                            let _ = terminal.end_open_line();
                            return RunEnd::Dropped(error);
                        }
                    }
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

/// What a run says, once, of a board that has stopped taking data.
const STOPPED_TAKING_DATA: &str =
    "The board has stopped taking data, so it cannot be asked how the program is doing. Its output is still being followed.";

/// What a run says of `count` bytes from the board that made no message.
fn bytes_dropped(count: u64) -> String {
    format!(
        "{count} bytes from the board arrived damaged or incomplete and were dropped, so some of the program's \
         output may be missing here."
    )
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

    /// A board that stores what a deploy sends it and answers each step of a store, except that
    /// its acknowledgement of chunk `lost` (from 0) never reaches the host. It reports the program
    /// stored, by its header's checksum, once every chunk is in. With `gone`, the carrier fails
    /// from then on, as a board's own USB does when it is unplugged. With `slow`, the board answers
    /// the chunk it names only after the time it names, as a board does whose first chunk clears
    /// its deploy window. It reports a deploy window of `window` bytes, or none.
    ///
    /// With `watching`, it keeps in `heard` what that writer held when the board sent the answer
    /// `slow` delayed: what the host had said before that answer came.
    struct Storing {
        replies: lamella_wire::MemTransport,
        image: Vec<u8>,
        total: usize,
        chunks: usize,
        lost: usize,
        gone: bool,
        dropped: bool,
        slow: Option<(usize, Duration)>,
        held: Option<(Instant, Vec<u8>)>,
        window: Option<u32>,
        watching: Option<Shared>,
        heard: Option<String>,
    }

    impl Storing {
        fn losing(lost: usize, gone: bool) -> Self {
            Self {
                replies: lamella_wire::MemTransport::new(),
                image: Vec::new(),
                total: 0,
                chunks: 0,
                lost,
                gone,
                dropped: false,
                slow: None,
                held: None,
                window: Some(64 * 1024),
                watching: None,
                heard: None,
            }
        }

        /// A board that loses no acknowledgement, and answers chunk `chunk` only `after` it came.
        fn slow_to_answer(chunk: usize, after: Duration) -> Self {
            Self { slow: Some((chunk, after)), ..Self::losing(usize::MAX, false) }
        }

        /// A board that loses no acknowledgement, and whose answer about what it stores carries no
        /// deploy window.
        fn reporting_no_window() -> Self {
            Self { window: None, ..Self::losing(usize::MAX, false) }
        }

        fn framed(msg_type: u8, seq: u16, payload: &[u8]) -> Vec<u8> {
            let mut board = lamella_wire::MemTransport::new();
            Transport::send(&mut board, msg_type, seq, payload).unwrap();
            board.take_sent()
        }

        fn reply(&mut self, msg_type: u8, seq: u16, payload: &[u8]) {
            self.replies.feed(&Self::framed(msg_type, seq, payload));
        }
    }

    impl Transport for Storing {
        fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
            if self.gone && self.dropped {
                return Err(TransportError::Closed);
            }
            match msg_type {
                crate::debug::ABORT => self.reply(exec::EXEC_ACK, seq, &[exec::ack::IDLE]),
                crate::deploy::DEPLOY_STATUS => {
                    let mut answer = vec![0u8; 10];
                    if self.total > 0 && self.image.len() == self.total {
                        answer[0] = crate::deploy::deploy_state::VERIFIED;
                        let held = baked_image_checksum(&self.image).unwrap_or(0);
                        answer[2..10].copy_from_slice(&held.to_le_bytes());
                    }
                    if let Some(window) = self.window {
                        answer.extend_from_slice(&window.to_le_bytes());
                    }
                    self.reply(crate::deploy::DEPLOY_STATUS_RESULT, seq, &answer);
                }
                crate::deploy::DEPLOY_IMAGE => {
                    let word =
                        |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
                    if word(0) == 0 {
                        self.image.clear();
                    }
                    self.image.extend_from_slice(&payload[8..]);
                    self.total = word(4) as usize;
                    let chunk = self.chunks;
                    self.chunks += 1;
                    if chunk == self.lost {
                        self.dropped = true;
                    } else {
                        let crc = lamella_wire::crc32::update(0, &self.image);
                        let mut ack = vec![crate::deploy::xfer::MATCHED];
                        ack.extend_from_slice(&crc.to_le_bytes());
                        match self.slow {
                            Some((slow, after)) if slow == chunk => {
                                let framed = Self::framed(crate::deploy::XFER_RESULT, seq, &ack);
                                self.held = Some((Instant::now() + after, framed));
                            }
                            _ => self.reply(crate::deploy::XFER_RESULT, seq, &ack),
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
            if self.gone && self.dropped {
                return Err(TransportError::Closed);
            }
            if let Some((at, framed)) = self.held.take() {
                if Instant::now() >= at {
                    self.heard = self.watching.as_ref().map(Shared::text);
                    self.replies.feed(&framed);
                } else {
                    self.held = Some((at, framed));
                }
            }
            Transport::poll(&mut self.replies)
        }
    }

    /// A writer whose text can be read while the store writing to it is still under way.
    #[derive(Clone, Default)]
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The session a host has with [`Storing`]: a baked-image board, over its own USB.
    fn storing_session() -> Negotiated {
        Negotiated {
            version: lamella_wire::PROTOCOL_VERSION,
            caps: Capabilities(Capabilities::BAKED_IMAGE),
            target_caps: Capabilities(Capabilities::BAKED_IMAGE),
            identity: lamella_wire::TargetIdentity::default(),
            max_inbound_payload: None,
        }
    }

    /// `len` bytes that a host reads as a baked image whose header records `checksum`: the fixed
    /// header [`baked_image_checksum`] reads, which is the magic, the version and the checksum
    /// word.
    fn headed(checksum: u64, len: usize) -> Vec<u8> {
        let mut image = vec![0u8; len];
        image[..4].copy_from_slice(b"LML1");
        image[4..8].copy_from_slice(&1u32.to_le_bytes());
        image[168..176].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(baked_image_checksum(&image), Some(checksum), "the header as a host reads it");
        image
    }

    /// **AN ACKNOWLEDGEMENT THAT NEVER ARRIVED IS NOT A PROGRAM THAT WAS NEVER STORED.** The board
    /// took every chunk and stores the program; only its answer to the last was lost. Asked once
    /// what it holds, it names this program, verified, and the store is reported as made.
    #[test]
    fn a_store_whose_last_acknowledgement_is_lost_asks_again_and_keeps_the_stored_program() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let mut board = Storing::losing(1, false);
        let mut out = Vec::new();
        let quick = Duration::from_millis(300);
        let session = storing_session();
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, quick, Wording::Full);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(stored, Ok(Stored::Written), "{out}");
        assert_eq!(board.chunks, 2, "10,000 bytes is two chunks of 8 KB or fewer");
        let said = "The board did not acknowledge a chunk of the deploy within";
        assert!(out.contains(said), "{out}");
        assert!(out.contains("asked again, it stores this program, verified"), "{out}");
    }

    /// A board that does not hold the program when asked again leaves the failure standing, and
    /// says so: here the first chunk's answer was lost, so the host sent no more.
    #[test]
    fn a_store_whose_acknowledgement_is_lost_fails_when_the_board_does_not_hold_the_program() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let mut board = Storing::losing(0, false);
        let mut out = Vec::new();
        let quick = Duration::from_millis(300);
        let session = storing_session();
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, quick, Wording::Full);
        let refused = stored.expect_err("only the first chunk reached the board");
        assert_eq!(board.chunks, 1, "the host stopped at the chunk it heard nothing about");
        let failed = "deploy to usb failed: the board did not acknowledge";
        assert!(refused.starts_with(failed), "{refused}");
        assert!(refused.contains("asked again, it does not store this program"), "{refused}");
    }

    /// **A BOARD THAT IS GONE FAILS THE STORE AT ONCE**: the question asked again cannot be sent,
    /// so nothing waits for its answer, and the failure says the board's state is not known.
    #[test]
    fn a_store_whose_board_is_gone_after_a_lost_acknowledgement_fails_at_once() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let mut board = Storing::losing(1, true);
        let mut out = Vec::new();
        let began = Instant::now();
        let patient = Duration::from_secs(5);
        let session = storing_session();
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, patient, Wording::Full);
        assert!(began.elapsed() < Duration::from_secs(1), "it waited {:?}", began.elapsed());
        let refused = stored.expect_err("the board is gone");
        let failed = "deploy to usb failed: the board stopped answering";
        assert!(refused.starts_with(failed), "{refused}");
        assert!(refused.contains("so whether it stores this program is not known"), "{refused}");
    }

    /// The deploy [`store_image`] makes, of `image` to `board` in 8 KiB chunks, each chunk given
    /// `wait`.
    fn deployed(
        board: &mut Storing,
        image: &[u8],
        wait: Duration,
    ) -> Result<TransferAck, TransportError> {
        let caps = Capabilities(Capabilities::BAKED_IMAGE);
        crate::deploy_image_with_progress(board, DEPLOY_SEQ, image, 8192, wait, caps, &mut |_| {})
    }

    /// **THE FIRST CHUNK's ACKNOWLEDGEMENT WAITS FOR THE WINDOW's CLEAR.** A board clears its
    /// deploy window before it writes a deploy's first chunk, so it may answer that chunk after the
    /// wait every chunk is given: this one reports a 64 KiB window and answers the first chunk
    /// after 300 ms, against a 100 ms wait, and the deploy is accepted.
    #[test]
    fn the_first_chunk_is_given_the_time_its_window_takes_to_clear() {
        let mut board = Storing::slow_to_answer(0, Duration::from_millis(300));
        let sent = deployed(&mut board, &[0x5A; 10_000], Duration::from_millis(100));
        assert!(matches!(sent, Ok(TransferAck::Accepted)), "{sent:?}");
        assert_eq!(board.chunks, 2, "and the second chunk followed");
    }

    /// Only the first chunk: a later one answered as late is a chunk left unacknowledged.
    #[test]
    fn a_later_chunk_is_given_only_the_wait_every_chunk_has() {
        let mut board = Storing::slow_to_answer(1, Duration::from_millis(300));
        let sent = deployed(&mut board, &[0x5A; 10_000], Duration::from_millis(100));
        assert!(matches!(sent, Err(TransportError::Closed)), "{sent:?}");
    }

    /// **A BOARD GONE AT THE FIRST CHUNK FAILS AT ONCE**, however long its window may take to
    /// clear: a carrier that fails is not waited on.
    #[test]
    fn a_board_gone_at_the_first_chunk_fails_at_once() {
        let mut board = Storing::losing(0, true);
        let began = Instant::now();
        let sent = deployed(&mut board, &[0x5A; 10_000], Duration::from_secs(5));
        assert!(began.elapsed() < Duration::from_secs(1), "it waited {:?}", began.elapsed());
        assert!(matches!(sent, Err(TransportError::Closed)), "{sent:?}");
    }

    /// **A FIRST CHUNK THAT MAY WAIT LONG IS SAID BEFORE THE WAIT**, once, with its bound: a board
    /// clears its deploy window before it answers a deploy's first chunk, which on a large flash
    /// takes most of a minute, and a silence that long reads as a hang. The bound is the first
    /// chunk's whole wait, rounded up to a second, so the line names the host's own patience. This
    /// board reports the 14,671,872 B window a 16 MB board has, and answers the first chunk 300 ms
    /// after it came.
    #[test]
    fn a_store_whose_first_chunk_may_wait_long_says_so_once_before_the_wait() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let out = Shared::default();
        let slow = Storing::slow_to_answer(0, Duration::from_millis(300));
        let window = 14_671_872;
        let mut board = Storing { watching: Some(out.clone()), window: Some(window), ..slow };
        let wait = Duration::from_secs(1);
        let session = storing_session();
        let mut written = out.clone();
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut written, wait, Wording::Full);
        let said = out.text();
        assert_eq!(stored, Ok(Stored::Written), "{said}");
        let first = wait + crate::first_chunk_allowance(Some(window as usize));
        let bound = first.as_secs() + u64::from(first.subsec_nanos() > 0);
        let line =
            format!("The board clears its program area first; this can take up to {bound} s.\n");
        assert_eq!(said.matches(&line).count(), 1, "{said}");
        let heard = board.heard.expect("the board answered the first chunk late");
        assert!(heard.ends_with(&line), "the last line before the first chunk's answer: {heard}");
    }

    /// **A CLEAR TOO SHORT TO NOTICE IS NOT SAID**: a window expected to clear in under about 10 s
    /// still lengthens the first chunk's wait, but a line naming that wait before an answer that
    /// comes at once makes a quick deploy look slow. A Pico 2's window, about 2 MiB, is expected to
    /// clear in under 8 s.
    #[test]
    fn a_store_whose_window_clears_too_soon_to_notice_says_nothing_of_the_clear() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let answering = Storing::losing(usize::MAX, false);
        let mut board = Storing { window: Some(2 * 1024 * 1024), ..answering };
        let mut out = Vec::new();
        let session = storing_session();
        let quick = Duration::from_millis(300);
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, quick, Wording::Full);
        let said = String::from_utf8(out).unwrap();
        assert_eq!(stored, Ok(Stored::Written), "{said}");
        assert!(!said.contains(" clears its "), "{said}");
    }

    /// **A FIRST CHUNK GIVEN ONLY THE WAIT EVERY CHUNK HAS IS NOT SAID**: a board that reports no
    /// deploy window is given no time to clear one, and an image too large for the window a board
    /// reports is refused before anything is sent, so neither store waits longer than its own wait.
    #[test]
    fn a_store_whose_first_chunk_waits_no_longer_says_nothing_of_a_clear() {
        let session = storing_session();
        let quick = Duration::from_millis(300);
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let mut out = Vec::new();
        let mut board = Storing::reporting_no_window();
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, quick, Wording::Full);
        let said = String::from_utf8(out).unwrap();
        assert_eq!(stored, Ok(Stored::Written), "{said}");
        assert!(!said.contains(" clears its "), "{said}");

        let large = headed(0x1122_3344_5566_7788, 70_000);
        let mut out = Vec::new();
        let mut board = Storing::losing(usize::MAX, false);
        let stored = store_image_within(&mut board, &session, "usb", &large, &mut out, quick, Wording::Full);
        let said = String::from_utf8(out).unwrap();
        assert!(stored.is_err(), "70,000 B do not fit a 64 KiB window: {said}");
        assert_eq!(board.chunks, 0, "nothing was sent");
        assert!(!said.contains(" clears its "), "{said}");
    }

    /// **AN UNANSWERED CHUNK IS REPORTED WITH THE WAIT IT HAD**: a first chunk's is longer where
    /// the board reports its deploy window, and the line said before that wait named it, so a
    /// failure naming the wait every chunk has would contradict it. Every chunk here has 1 s and
    /// the first also what a 64 KiB window is allowed, so a lost first answer is reported with 2 s
    /// and a lost second answer with 1 s.
    #[test]
    fn an_unanswered_chunk_is_reported_with_the_wait_it_had() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let session = storing_session();
        let wait = Duration::from_secs(1);
        let first = wait + crate::first_chunk_allowance(Some(64 * 1024));
        let first = first.as_secs() + u64::from(first.subsec_nanos() > 0);

        let mut out = Vec::new();
        let mut board = Storing::losing(0, false);
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, wait, Wording::Full);
        let refused = stored.expect_err("only the first chunk reached the board");
        let said = format!("did not acknowledge a chunk of the deploy within {first} s, and asked");
        assert!(refused.contains(&said), "{refused}");

        let mut out = Vec::new();
        let mut board = Storing::losing(1, false);
        let stored = store_image_within(&mut board, &session, "usb", &image, &mut out, wait, Wording::Full);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(stored, Ok(Stored::Written), "{out}");
        let said = "did not acknowledge a chunk of the deploy within 1 s; asked again";
        assert!(out.contains(said), "{out}");
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

        fn discarded_bytes(&self) -> Option<u64> {
            self.wire.discarded_bytes()
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
    fn quiet(quiet: Duration) -> Watch<'static> {
        Watch { quiet, limit: None, stop: None }
    }

    /// An SDK run announces the erase before a delayed answer, without transfer chatter.
    #[test]
    fn a_brief_store_announces_only_the_erase_before_the_wait() {
        let image = headed(0x1122_3344_5566_7788, 10_000);
        let out = Shared::default();
        let mut board = Storing {
            watching: Some(out.clone()),
            ..Storing::slow_to_answer(0, Duration::from_millis(30))
        };
        let session = storing_session();
        assert_eq!(store_image_worded(&mut board, &session, "usb", &image, &mut out.clone(), Wording::Brief), Ok(Stored::Written));
        assert_eq!(out.text(), "Erasing the program area of flash...\n");
        assert_eq!(board.heard.as_deref(), Some("Erasing the program area of flash...\n"));
        let mut again = Vec::new();
        assert_eq!(store_image_worded(&mut board, &session, "usb", &image, &mut again, Wording::Brief), Ok(Stored::AlreadyThere));
        assert!(again.is_empty(), "a stored image needs no erase");
    }

    /// A store that needs to send an image announces the erase even without a reported window.
    #[test]
    fn a_brief_store_without_a_window_still_announces_the_erase() {
        let mut board = Storing::reporting_no_window();
        let mut out = Vec::new();
        let result = store_image_worded(&mut board, &storing_session(), "usb", &headed(42, 256), &mut out, Wording::Brief);
        assert_eq!(result, Ok(Stored::Written));
        assert_eq!(out, b"Erasing the program area of flash...\n");
    }

    /// A project-run failure names the missing program without a protocol timeout.
    #[test]
    fn a_brief_store_failure_has_no_time_bound() {
        let mut board = Storing::losing(0, false);
        let mut out = Vec::new();
        let result = store_image_within(&mut board, &storing_session(), "usb", &headed(42, 10_000), &mut out, Duration::from_millis(5), Wording::Brief);
        assert_eq!(result, Err("The board did not store the program.".to_owned()));
    }

    /// A project run keeps the start's blank line and the program's two streams separate.
    #[test]
    fn a_brief_start_says_only_a_blank_line_before_the_program() {
        let mut board = Answering::with(|peer| {
            Transport::send(peer, exec::EXEC_ACK, RUN_SEQ, &[exec::ack::STARTED]).unwrap();
            for (kind, seq, payload) in [printed("sdk-program-marker\n"), ended_with(RUN_SEQ, 7)] {
                Transport::send(peer, kind, seq, &payload).unwrap();
            }
        });
        let (mut say, mut out, mut err) = (Vec::new(), Vec::new(), Vec::new());
        let ended = start_scheduled_worded(&mut board, "usb", &mut say, &mut out, &mut err, Watch::for_at_most(Duration::from_millis(100)), Wording::Brief);
        assert_eq!(ended, Ok(RunEnd::Done(7)));
        assert_eq!(say, b"\n");
        assert_eq!(out, b"sdk-program-marker\n");
        assert!(err.is_empty());
    }

    /// A host cancellation neither reads nor sends a command to a board.
    #[test]
    fn a_stopped_watch_does_not_touch_the_board() {
        struct Untouched;
        impl Transport for Untouched {
            fn send(&mut self, _: u8, _: u16, _: &[u8]) -> Result<(), TransportError> { panic!("sent after stop") }
            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> { panic!("read after stop") }
        }
        let watch = Watch { stop: Some(&|| true), ..Watch::to_the_end() };
        let (mut say, mut out, mut err) = (Vec::new(), Vec::new(), Vec::new());
        assert_eq!(follow(&mut Untouched, RUN_SEQ, &mut say, &mut out, &mut err, watch), RunEnd::Interrupted);
        assert!(say.is_empty() && out.is_empty() && err.is_empty());
    }

    /// Cancellation keeps the output already received and does not query the board again.
    #[test]
    fn a_watch_stopped_after_output_preserves_that_output() {
        let mut board = Board::sending(&[printed("stop-program-marker\n")], None);
        let calls = std::cell::Cell::new(0);
        let stop = || { calls.set(calls.get() + 1); calls.get() > 1 };
        let watch = Watch { stop: Some(&stop), quiet: Duration::ZERO, ..Watch::for_at_most(Duration::from_millis(20)) };
        let (mut say, mut out, mut err) = (Vec::new(), Vec::new(), Vec::new());
        assert_eq!(follow(&mut board, RUN_SEQ, &mut say, &mut out, &mut err, watch), RunEnd::Interrupted);
        assert_eq!(out, b"stop-program-marker\n");
        assert_eq!(board.asked, 0);
    }

    /// **A RUN IS FOLLOWED TO THE STOP AT ITS OWN SEQUENCE NUMBER**, and only that stop ends it: a
    /// stop the board sent for something else is not this program's end.
    #[test]
    fn a_run_is_followed_to_its_own_stop_and_its_exit_code_is_reported() {
        let mut board =
            Board::sending(&[printed("one\n"), ended_with(0, 9), printed("two\n"), ended_with(RUN_SEQ, 3)], None);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = follow(&mut board, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, quiet(Duration::from_secs(60)));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(end, RunEnd::Done(3));
        assert!(!end.succeeded(), "exit 3 is reported, and is not success");
        assert_eq!(end.describe("loopback"), "The program ended, exit code 3.");
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
        let end = follow(&mut board, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, quiet(Duration::from_secs(60)));
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
        let end = follow(&mut board, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, quiet(Duration::from_millis(20)));
        assert_eq!(end, RunEnd::NoLongerRunning);
        assert!(String::from_utf8(out).unwrap().contains("started\n"));
        assert!(end.describe("loopback").contains("no longer running on the board"), "{}", end.describe("loopback"));
        assert_eq!(board.asked, 1, "asked once, and answered");
    }

    /// **A LINE THE CARRIER TORE IS SAID TO BE MISSING,** and the run goes on: the lines on each side of it
    /// still show, and how the run ended is still what the board said. A clean run says nothing, and nor do
    /// bytes from before the run began.
    #[test]
    fn bytes_that_made_no_message_are_said_to_be_dropped() {
        const TAIL: &[u8] = b"the tail of a line whose head never arrived";
        let mut board = Board::sending(&[], None);
        board.wire.feed(b"older noise");
        assert!(Transport::poll(&mut board.wire).unwrap().is_none());
        let mut wire = lamella_wire::MemTransport::new();
        let (t, s, p) = printed("one\n");
        Transport::send(&mut wire, t, s, &p).unwrap();
        board.wire.feed(&wire.take_sent());
        board.wire.feed(TAIL);
        for (t, s, p) in [printed("two\n"), ended_with(RUN_SEQ, 0)] {
            Transport::send(&mut wire, t, s, &p).unwrap();
        }
        board.wire.feed(&wire.take_sent());
        let (mut say, mut out, mut err) = (Vec::new(), Vec::new(), Vec::new());
        let end = follow(&mut board, RUN_SEQ, &mut say, &mut out, &mut err, quiet(Duration::from_secs(60)));
        assert_eq!(end, RunEnd::Done(0), "how it ended is what the board said");
        assert_eq!(String::from_utf8(out).unwrap(), "one\ntwo\n", "the lines on each side of it show");
        assert_eq!(String::from_utf8(say).unwrap(), format!("{}\n", bytes_dropped(TAIL.len() as u64)));
        assert!(bytes_dropped(43).starts_with("43 bytes from the board"));

        let mut clean = Board::sending(&[printed("one\n"), ended_with(RUN_SEQ, 0)], None);
        let (mut say, mut out, mut err) = (Vec::new(), Vec::new(), Vec::new());
        assert_eq!(follow(&mut clean, RUN_SEQ, &mut say, &mut out, &mut err, quiet(Duration::from_secs(60))), RunEnd::Done(0));
        assert!(say.is_empty(), "a clean run says nothing");
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
            follow(&mut ending, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, quiet(spell))
        };
        assert_eq!(end, RunEnd::Done(0), "the run ended with exit 0");
        assert!((1..=10).contains(&board.asked), "asked once per quiet spell, not on every poll: {}", board.asked);
    }

    /// **A WATCH WITH A LIMIT ENDS THERE, AND SAYS THE PROGRAM IS STILL RUNNING**: what it printed so
    /// far is shown, and the program is said to go on running on the board, which stores it.
    #[test]
    fn a_watch_with_a_limit_ends_there_and_says_the_program_runs_on() {
        let mut board = Board::sending(&[printed("tick")], Some(exec::ack::RUNNING));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let watch = Watch { quiet: Duration::from_millis(20), limit: Some(Duration::from_millis(150)), stop: None };
        let began = Instant::now();
        let end = follow(&mut board, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, watch);
        assert!(began.elapsed() >= Duration::from_millis(150), "it watched until the limit: {:?}", began.elapsed());
        assert_eq!(end, RunEnd::StillRunning(Duration::from_millis(150)));
        assert_eq!(String::from_utf8(out).unwrap(), "tick\n", "what it printed, its open line ended");
        let said = end.describe("loopback");
        assert!(said.starts_with("The program was still running after 0 s, and it goes on running"), "{said}");
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
        let end = follow(&mut Gone, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, quiet(Duration::from_secs(60)));
        assert_eq!(end, RunEnd::Dropped(TransportError::Closed));
        assert!(end.describe("usb").starts_with("The connection to usb dropped while the program was running, so how"));
    }

    /// **A QUESTION THE CARRIER CANNOT SEND ENDS THE RUN AS DROPPED.** A line that has gone away can
    /// read as silence, so the failed question is the one sign of it, and a follower that let it
    /// pass would wait forever.
    #[test]
    fn a_run_whose_question_cannot_be_sent_ends_as_dropped() {
        struct Mute {
            sends: usize,
        }
        impl Transport for Mute {
            fn send(&mut self, _: u8, _: u16, _: &[u8]) -> Result<(), TransportError> {
                self.sends += 1;
                Err(TransportError::Carrier)
            }
            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
                if self.sends > 3 { Err(TransportError::Closed) } else { Ok(None) }
            }
        }
        let mut mute = Mute { sends: 0 };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = follow(&mut mute, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, quiet(Duration::from_millis(20)));
        assert_eq!(end, RunEnd::Dropped(TransportError::Carrier), "the failed question ended the run");
        assert_eq!(mute.sends, 1, "at the first question");
    }

    /// **A BOARD THAT WILL NOT TAKE THE QUESTION HAS NOT GONE AWAY.** It is said once, the run goes on being
    /// followed, and the question is asked again; a board that has gone says so through the read.
    #[test]
    fn a_board_that_stopped_taking_data_is_said_once_and_still_followed() {
        struct Busy {
            sends: usize,
        }
        impl Transport for Busy {
            fn send(&mut self, _: u8, _: u16, _: &[u8]) -> Result<(), TransportError> {
                self.sends += 1;
                Err(TransportError::Stalled)
            }
            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
                if self.sends >= 3 { Err(TransportError::Carrier) } else { Ok(None) }
            }
        }
        let mut busy = Busy { sends: 0 };
        let (mut say, mut out, mut err) = (Vec::new(), Vec::new(), Vec::new());
        let end = follow(&mut busy, RUN_SEQ, &mut say, &mut out, &mut err, quiet(Duration::from_millis(5)));
        assert_eq!(end, RunEnd::Dropped(TransportError::Carrier), "it ended when the read said the board had gone");
        assert!(busy.sends >= 3, "the question was asked again");
        let said = String::from_utf8(say).unwrap();
        assert_eq!(said.matches("stopped taking data").count(), 1, "{said}");
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

    /// A board whose program prints without pause until `until`, and is silent after it.
    struct Printing {
        until: Instant,
    }

    impl Transport for Printing {
        fn send(&mut self, _msg_type: u8, _seq: u16, _payload: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
            Ok((Instant::now() < self.until).then(|| {
                let (msg_type, seq, payload) = printed(".");
                lamella_wire::Frame { msg_type, seq, payload }
            }))
        }
    }

    /// **A WATCH WITH A LIMIT ENDS THERE WHILE THE PROGRAM GOES ON PRINTING.** The collector it polls
    /// read until the board fell silent, so a program printing without pause held the watch inside
    /// one poll, past its limit, for as long as it printed.
    #[test]
    fn a_watch_with_a_limit_ends_there_while_the_program_goes_on_printing() {
        let mut board = Printing { until: Instant::now() + Duration::from_secs(3) };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let limit = Duration::from_millis(600);
        let began = Instant::now();
        let end = follow(&mut board, RUN_SEQ, &mut Vec::new(), &mut out, &mut err, Watch::for_at_most(limit));
        assert_eq!(end, RunEnd::StillRunning(limit));
        assert!(began.elapsed() < Duration::from_secs(1), "took {:?}", began.elapsed());
    }

    /// The tests that need a board's own deploy serve, in this process.
    #[cfg(feature = "baked")]
    pub(super) mod against_the_firmware_serve {
        use super::super::*;
        use crate::loopback::{LoopbackBoard, RamFlash};

        /// The HELLO a host storing a program makes, answered by `board`.
        pub(crate) fn hello(board: &mut LoopbackBoard, caps: Capabilities) -> Negotiated {
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
            assert!(out.contains("Deploying 40000 B to usb, 8192 B per chunk"), "{out}");
            for quarter in ["   25%  16384 B", "   50%  24576 B", "   75%  32768 B"] {
                assert!(out.contains(quarter), "{quarter}: {out}");
            }
            assert!(out.contains("Deployed 40000 B to usb in "), "{out}");
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
            assert!(out.contains("Deploying 1000 B to COM8, 256 B per chunk"), "{out}");
            assert_eq!(board.chunks, 4, "1000 bytes is four chunks of 256 bytes or fewer");
        }
    }
}

