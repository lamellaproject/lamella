//! The native flasher, and the instruments that make its answers trustworthy.

use std::process::ExitCode;

use lamella_esp_serial::session::Dialect;
use lamella_esp_serial::{deflate, Connector, FlashParams, ResetInto, Session, StatusLen};
use lamella_esp_serial_host::{drive, Outcome, Port, PortError, WindowsPort};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(verb) = args.first().map(String::as_str) else {
        usage();
        return ExitCode::FAILURE;
    };
    if verb == "-h" || verb == "--help" || verb == "help" {
        usage();
        return ExitCode::SUCCESS;
    }
    let options = match Options::parse(&args[1..]) {
        Ok(options) => options,
        Err(problem) => {
            eprintln!("lamella-esp-flash: {problem}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = match verb {
        "listen" => listen(&options),
        "signals" => signals(&options),
        "pulse" => pulse(&options),
        "sync" => sync(&options),
        "flash" => flash(&options),
        other => {
            eprintln!("lamella-esp-flash: unknown command '{other}'");
            usage();
            return ExitCode::FAILURE;
        }
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(problem) => {
            eprintln!("lamella-esp-flash: {problem}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    println!(
        "\
lamella-esp-flash -- program an Espressif part over a serial line, with no external tool

USAGE
    lamella-esp-flash <command> [options]

COMMANDS
    listen      Read the line and print what arrives. Proves the read path.
    signals     Pulse the control signals and report what the board says. Proves the signal
                path, and identifies which signal drives reset and which the boot strap.
    pulse       Perform one EXPLICIT signal script and report the resulting boot banner, so a
                candidate reset sequence is settled by the board rather than by argument.
    sync        Reset into the bootloader and establish the byte stream.
    flash       Write an image and verify it against the target's own read-back.

OPTIONS
    --port <name>        Serial port (e.g. COM25). REQUIRED -- never auto-detected, because
                         auto-detection is how the wrong board gets written.
    --baud <rate>        Line rate (default 115200).
    --connector <which>  'bridge' (a board's USB-to-UART chip) or 'chip' (the part's own USB
                         serial device). Default 'bridge'. They are different devices with
                         different wiring, so the reset sequence differs.
    --image <path>       The file to write (flash only).
    --offset <n>         Where to write it, decimal or 0x-prefixed (flash only).
    --flash-size <n>     The attached flash chip's size in bytes (default 8388608).
    --status-len <n>     Trailing status bytes in a response: 2 or 4 (default 4).
    --seconds <n>        How long to listen (listen only, default 3).
    --script <steps>     A comma-separated signal script (pulse only): D1/D0 set or clear
                         data-terminal-ready, R1/R0 set or clear request-to-send, W<ms> waits.
                         Example: D0,R1,W100,D1,R0,W50,D0
    --compress <how>     Send the image compressed and let the target's ROM inflate it (flash
                         only). 'off' (default), 'fixed' to compress, or 'stored' for a
                         conformant stream that compresses NOTHING -- which is the way to tell a
                         wrong command sequence apart from a wrong compressor, because the two
                         are refused identically.
    --quiet              Do not trace each action."
    );
}

/// Everything the verbs take, parsed once.
struct Options {
    port: String,
    baud: u32,
    connector: Connector,
    image: Option<String>,
    offset: u32,
    flash_size: u32,
    status_len: StatusLen,
    seconds: u32,
    script: Option<String>,
    /// Which compressed encoder to use, or `None` for a plain write.
    ///
    /// Off by default, so that a compression problem cannot present itself as a flasher that no longer
    /// works: the two paths differ in three commands and one argument's meaning, and only one of them
    /// needs a compressor to be right.
    compress: Option<deflate::Method>,
    quiet: bool,
}

impl Options {
    /// Parses `--name value` pairs and the bare `--quiet` flag.
    ///
    /// Home-grown rather than a parsing library, which is this project's standing choice: the UX is
    /// conventional, the dependency is not taken.
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            port: String::new(),
            baud: 115_200,
            connector: Connector::UartBridge,
            image: None,
            offset: 0,
            flash_size: 8 * 1024 * 1024,
            status_len: StatusLen::FOUR,
            seconds: 3,
            script: None,
            compress: None,
            quiet: false,
        };
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            if flag == "--quiet" {
                options.quiet = true;
                continue;
            }
            let value = rest
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--port" => options.port = value.clone(),
                "--baud" => options.baud = number(value)?,
                "--connector" => {
                    options.connector = match value.as_str() {
                        "bridge" => Connector::UartBridge,
                        "chip" => Connector::ChipUsbSerial,
                        other => return Err(format!("--connector must be bridge or chip, not {other}")),
                    }
                }
                "--image" => options.image = Some(value.clone()),
                "--offset" => options.offset = number(value)?,
                "--flash-size" => options.flash_size = number(value)?,
                "--status-len" => {
                    options.status_len = match number(value)? {
                        2 => StatusLen::TWO,
                        4 => StatusLen::FOUR,
                        other => return Err(format!("--status-len must be 2 or 4, not {other}")),
                    }
                }
                "--seconds" => options.seconds = number(value)?,
                "--script" => options.script = Some(value.clone()),
                "--compress" => {
                    options.compress = match value.as_str() {
                        "off" => None,
                        "stored" => Some(deflate::Method::Stored),
                        "fixed" => Some(deflate::Method::Fixed),
                        other => {
                            return Err(format!(
                                "--compress must be off, stored or fixed, not {other}"
                            ))
                        }
                    }
                }
                other => return Err(format!("unknown option {other}")),
            }
        }
        if options.port.is_empty() {
            return Err(String::from(
                "--port is required. It is never auto-detected: with more than one board attached, \
                 auto-detection is exactly how the wrong one gets written.",
            ));
        }
        Ok(options)
    }

    /// The port, opened.
    fn open(&self) -> Result<WindowsPort, PortError> {
        WindowsPort::open(&self.port, self.baud)
    }

    /// The per-part protocol variations to speak.
    ///
    /// Defaults to the one this project has MEASURED, with the status length overridable because it is
    /// the field a user is most likely to be told by a datasheet. A part whose write declaration takes a
    /// different argument count needs its own measured constant rather than a command-line switch --
    /// a flag there would invite guessing at exactly the fact that must not be guessed.
    fn dialect(&self) -> Dialect {
        Dialect { status_len: self.status_len, ..Dialect::ESP32C6 }
    }
}

/// A decimal or `0x`-prefixed number.
fn number(text: &str) -> Result<u32, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("'{text}' is not a number"))
}

/// Reads for a while and prints what arrives, both as text and as a byte count.
///
/// **This is the negative control for every later "the target said nothing".** A port can open, every
/// read can succeed, and every read can return zero -- which is indistinguishable from a silent board
/// unless something has been shown to arrive.
fn listen(options: &Options) -> Result<bool, PortError> {
    let mut port = options.open()?;
    println!("listening on {} for {} s", port.describe(), options.seconds);
    let text = collect(&mut port, u64::from(options.seconds) * 1_000)?;
    let total = text.len();
    println!("{total} bytes");
    if total > 0 {
        println!("---");
        print!("{}", String::from_utf8_lossy(&text));
        println!("\n---");
    }
    match total {
        0 => {
            println!(
                "VERDICT: nothing arrived. This instrument has NOT been shown capable of receiving, \
                 so a later 'the target did not answer' would mean nothing. Try resetting the board \
                 while listening, or a different rate."
            );
            Ok(false)
        }
        _ => {
            println!("VERDICT: the read path works -- {total} bytes arrived on this port.");
            Ok(true)
        }
    }
}

/// Pulses the control signals in each of the arrangements that matter and reports what the board said.
///
/// # What makes this a measurement rather than a poke
///
/// The vendor documents that a board's bridge has its request-to-send wired to the part's enable and
/// its data-terminal-ready to the boot strap, both active low, but publishes no step table. **The
/// observable that settles it comes from the architecture, not from a comment: a part released from
/// reset narrates its boot over this very line.** So a pulse that produces a fresh banner performed a
/// reset, and one that produces silence did not -- and the banner's TEXT says whether the part came up
/// in the bootloader or in flash, which is the second half of the question in the same reading.
fn signals(options: &Options) -> Result<bool, PortError> {
    let mut port = options.open()?;
    println!("signal probe on {}", port.describe());
    println!(
        "Each trial leaves both signals clear, performs the described pulse, then reads the line.\n\
         A fresh boot banner means the pulse RESET the part. Its text says WHERE it came up.\n"
    );

    /// One trial: a name, and the signal writes to perform.
    type Trial = (&'static str, &'static [(char, bool)]);
    let trials: &[Trial] = &[
        ("baseline -- no signal touched", &[]),
        ("DTR alone: assert, hold, clear", &[('D', true), ('W', false), ('D', false)]),
        ("RTS alone: assert, hold, clear", &[('R', true), ('W', false), ('R', false)]),
        (
            "both together: assert both, hold, clear both",
            &[('D', true), ('R', true), ('W', false), ('D', false), ('R', false)],
        ),
        (
            "RTS pulsed while DTR is held (the download-mode shape)",
            &[('D', true), ('R', true), ('W', false), ('R', false), ('W', false), ('D', false)],
        ),
    ];

    let mut any_banner = false;
    for (name, steps) in trials {
        port.set_dtr(false)?;
        port.set_rts(false)?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        port.discard_buffers()?;

        for (signal, on) in *steps {
            match signal {
                'D' => port.set_dtr(*on)?,
                'R' => port.set_rts(*on)?,
                _ => std::thread::sleep(std::time::Duration::from_millis(120)),
            }
        }

        let got = collect(&mut port, 1_500)?;
        let text = String::from_utf8_lossy(&got);
        let banner = !got.is_empty();
        any_banner |= banner;
        println!("{name}");
        println!("    {} bytes -- {}", got.len(), if banner { "SPOKE" } else { "silent" });
        if banner {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                println!("      | {line}");
            }
        }
        println!();
    }

    port.set_dtr(false)?;
    port.set_rts(false)?;

    match any_banner {
        false => {
            println!(
                "VERDICT: no trial produced any output. Either these signal writes do not reach the \
                 part (this port may be the part's own USB serial device rather than a bridge), or \
                 the read path is not working -- run `listen` first to tell those two apart."
            );
            Ok(false)
        }
        true => {
            println!(
                "VERDICT: at least one pulse made the part speak, so these signal writes DO reach it. \
                 Compare the trials above: the one that resets identifies the enable line, and a \
                 banner mentioning download/waiting identifies the boot strap."
            );
            Ok(true)
        }
    }
}

/// One step of a signal script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptStep {
    /// Set or clear data-terminal-ready.
    Dtr(bool),
    /// Set or clear request-to-send.
    Rts(bool),
    /// Wait this many milliseconds.
    Wait(u32),
}

/// Parses `D0,R1,W100,D1` into steps.
fn parse_script(text: &str) -> Result<Vec<ScriptStep>, String> {
    let mut steps = Vec::new();
    for token in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (kind, rest) = token.split_at(1);
        let step = match (kind, rest) {
            ("D" | "d", "1") => ScriptStep::Dtr(true),
            ("D" | "d", "0") => ScriptStep::Dtr(false),
            ("R" | "r", "1") => ScriptStep::Rts(true),
            ("R" | "r", "0") => ScriptStep::Rts(false),
            ("W" | "w", ms) => ScriptStep::Wait(number(ms)?),
            _ => return Err(format!("'{token}' is not a script step (want D0 D1 R0 R1 W<ms>)")),
        };
        steps.push(step);
    }
    if steps.is_empty() {
        return Err(String::from("--script is empty"));
    }
    Ok(steps)
}

/// Performs one explicit signal script and reports the resulting boot banner.
///
/// # Why this verb exists rather than more reasoning
///
/// A board's reset circuit is a BOARD fact, and the part's vendor documents the chip. Between the two
/// there is a small circuit whose behaviour decides which signal arrangements produce a reset and which
/// produce nothing -- and a sequence derived from a wiring description is a hypothesis about that
/// circuit. **This verb makes the board answer instead**: the part narrates its boot, and the banner's
/// own boot-mode line says whether it came up in flash or in the serial bootloader. So a candidate
/// sequence is settled by reading what the silicon says it did.
///
/// The reported evidence is deliberately the RAW banner rather than a matched keyword: a filter would
/// decide in advance what counts as download mode, which is the question.
fn pulse(options: &Options) -> Result<bool, PortError> {
    let Some(text) = &options.script else {
        eprintln!("lamella-esp-flash: pulse needs --script (e.g. --script D0,R1,W100,D1,R0,W50,D0)");
        return Ok(false);
    };
    let steps = match parse_script(text) {
        Ok(steps) => steps,
        Err(problem) => {
            eprintln!("lamella-esp-flash: {problem}");
            return Ok(false);
        }
    };
    let mut port = options.open()?;
    println!("pulse on {}", port.describe());
    println!("script: {text}");

    port.set_dtr(false)?;
    port.set_rts(false)?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    port.discard_buffers()?;

    for step in &steps {
        match step {
            ScriptStep::Dtr(on) => {
                println!("  DTR {}", u8::from(*on));
                port.set_dtr(*on)?;
            }
            ScriptStep::Rts(on) => {
                println!("  RTS {}", u8::from(*on));
                port.set_rts(*on)?;
            }
            ScriptStep::Wait(ms) => {
                println!("  wait {ms} ms");
                std::thread::sleep(std::time::Duration::from_millis(u64::from(*ms)));
            }
        }
    }

    let got = collect(&mut port, 2_000)?;
    let text = String::from_utf8_lossy(&got);
    println!("--- {} bytes ---", got.len());
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        println!("| {line}");
    }
    println!("---");
    match text.lines().find(|l| l.contains("boot:")) {
        Some(line) => println!("BOOT MODE LINE: {}", line.trim()),
        None if got.is_empty() => println!(
            "NO OUTPUT -- this script did not reset the part. (A part held in reset, or never \
             released, says nothing; so does one that was never reset at all.)"
        ),
        None => println!("OUTPUT BUT NO BOOT LINE -- the part spoke without rebooting."),
    }
    Ok(!got.is_empty())
}

/// Resets into the bootloader and establishes the byte stream -- the smallest test of the sequence.
///
/// **This probe stops before anything can modify flash.** It drives the session's prologue and returns
/// at the write declaration: reaching that command means the reset worked, the byte stream was
/// established, the flash chip attached and its geometry was accepted. Driving one step further would
/// erase a range, which is not something a diagnostic should do to a board somebody else is using.
fn sync(options: &Options) -> Result<bool, PortError> {
    use lamella_esp_serial::{Command, Step};

    let mut port = options.open()?;
    println!("sync on {} via {:?}", port.describe(), options.connector);
    let mut session = Session::write_flash(
        options.connector,
        options.dialect(),
        FlashParams::serial_nor(options.flash_size),
        0,
        Vec::new(),
    );
    let mut trace = tracer(options.quiet);
    let mut buffer = [0u8; 4096];
    let mut reached = Vec::new();
    let mut verdict = false;
    loop {
        match session.poll() {
            Step::Do(lamella_esp_serial::Action::Write(bytes)) => {
                let body = lamella_esp_serial::unescape(&bytes[1..bytes.len() - 1]);
                if body[1] == Command::FlashBegin as u8 {
                    reached.dedup();
                    println!("REACHED THE WRITE DECLARATION -- stopping before anything erases.");
                    println!("Established: {}", reached.join(", "));
                    verdict = true;
                    break;
                }
                reached.push(name_of(body[1]));
                trace(&format!("  -> {} ({} bytes)", name_of(body[1]), bytes.len()));
                if let Err(problem) = port.write(&bytes) {
                    return Err(problem);
                }
            }
            Step::Do(action) => {
                if let Err(problem) = perform_bare(&action, &mut port) {
                    return Err(problem);
                }
                trace(&format!("  {action:?}"));
            }
            Step::Await { timeout_ms } => match port.read(&mut buffer, timeout_ms)? {
                0 => {
                    trace(&format!("  <- nothing within {timeout_ms} ms"));
                    session.timeout();
                }
                got => {
                    trace(&format!("  <- {got} bytes"));
                    session.feed(&buffer[..got]);
                }
            },
            Step::Done { .. } => break,
            Step::Failed(why) => {
                report(&Outcome::Refused(why), options);
                break;
            }
        }
    }
    for action in lamella_esp_serial::reset_sequence(options.connector, ResetInto::Flash) {
        let _ = perform_bare(&action, &mut port);
    }
    Ok(verdict)
}

/// A command's name, for a report that says which step was reached rather than which opcode.
fn name_of(opcode: u8) -> String {
    use lamella_esp_serial::Command;
    let name = match opcode {
        o if o == Command::Sync as u8 => "byte stream established",
        o if o == Command::SpiAttach as u8 => "flash chip attached",
        o if o == Command::SpiSetParams as u8 => "flash geometry accepted",
        o if o == Command::FlashBegin as u8 => "write declared",
        o if o == Command::FlashDeflBegin as u8 => "compressed write declared",
        o if o == Command::FlashDeflData as u8 => "compressed data accepted",
        o if o == Command::FlashDeflEnd as u8 => "compressed write closed",
        _ => return format!("opcode {opcode:#04x}"),
    };
    String::from(name)
}

/// What the ROM loader's error byte means, in its own words.
///
/// Worth translating rather than printing raw, because **this error space is the compressed path's only
/// diagnostic and three of its codes are about compression specifically.** A stream whose wrapper is
/// wrong and a stream whose contents are wrong are told apart here and nowhere else.
///
/// Only the ROM loader's codes: this crate speaks to no other loader, and the RAM loader's codes are a
/// disjoint space, so printing a name from the wrong table would be worse than printing the byte.
fn rom_error(code: u8) -> &'static str {
    match code {
        0x00 => "undefined error",
        0x01 => "the input parameter is invalid",
        0x02 => "failed to allocate memory",
        0x03 => "failed to send a message",
        0x04 => "failed to receive a message",
        0x05 => "the received message's format is invalid -- often an argument COUNT",
        0x06 => "the message was understood and the result was wrong",
        0x07 => "checksum error",
        0x08 => "flash write error -- the target read back what it wrote and disagreed",
        0x09 => "flash read error",
        0x0A => "flash read length error",
        0x0B => "the compressed stream could not be inflated",
        0x0C => "the compressed stream's checksum failed -- the wrapper's trailer",
        0x0D => "a compression parameter is invalid -- the wrapper's header",
        0x0E => "invalid RAM binary size",
        0x0F => "invalid RAM binary address",
        _ => "not a code this loader documents",
    }
}

/// Writes an image and verifies it.
fn flash(options: &Options) -> Result<bool, PortError> {
    let Some(path) = &options.image else {
        eprintln!("lamella-esp-flash: flash needs --image <path>");
        return Ok(false);
    };
    let image = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(problem) => {
            eprintln!("lamella-esp-flash: cannot read {path}: {problem}");
            return Ok(false);
        }
    };
    let mut port = options.open()?;
    println!(
        "writing {} ({} bytes) to {:#x} on {} via {:?}",
        path,
        image.len(),
        options.offset,
        port.describe(),
        options.connector
    );
    println!(
        "expected digest {}",
        String::from_utf8_lossy(&lamella_esp_serial::md5_hex(&image))
    );
    let uncompressed = image.len();
    let params = FlashParams::serial_nor(options.flash_size);
    let mut session = match options.compress {
        None => Session::write_flash(options.connector, options.dialect(), params, options.offset, image),
        Some(method) => Session::write_flash_compressed(
            options.connector,
            options.dialect(),
            params,
            options.offset,
            image,
            method,
        ),
    };
    let sent = session.transfer_len();
    println!(
        "sending {sent} bytes in {} packets ({:.1}% of the image)",
        session.total_blocks(),
        if uncompressed == 0 { 100.0 } else { sent as f64 * 100.0 / uncompressed as f64 }
    );
    let mut trace = tracer(options.quiet);
    let outcome = drive(&mut session, &mut port, &mut trace);
    report(&outcome, options);
    Ok(matches!(outcome, Outcome::Verified { .. }))
}

/// Reads everything that arrives on `port` for `window` milliseconds.
///
/// # Why this is bounded by the CLOCK and not by a count of reads
///
/// A capture window is a length of TIME -- "what does the board say in the second after its reset" --
/// and a loop of N reads only measures that if each read takes its whole bound. **It does not: a read
/// returns as soon as anything has arrived, which is what makes a write fast, and it means a chatty
/// board is captured for far less time than a silent one.** So a fixed-count loop shortens exactly
/// when there is the most to hear, and a truncated banner reads as a board that said less.
///
/// Found by measuring: the same reset captured 1,336 bytes of boot log before the read bound was
/// corrected and 603 after, with the board unchanged. **The instrument got quieter, not the target.**
fn collect(port: &mut dyn Port, window_ms: u64) -> Result<Vec<u8>, PortError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(window_ms);
    let mut buffer = [0u8; 4096];
    let mut got = Vec::new();
    while std::time::Instant::now() < deadline {
        let n = port.read(&mut buffer, 100)?;
        got.extend_from_slice(&buffer[..n]);
    }
    Ok(got)
}

/// One action, with no tracing -- for the tidy-up reset after a probe.
fn perform_bare(
    action: &lamella_esp_serial::Action,
    port: &mut dyn Port,
) -> Result<(), PortError> {
    use lamella_esp_serial::Action;
    match action {
        Action::Write(bytes) => port.write(bytes),
        Action::SetDtr(on) => port.set_dtr(*on),
        Action::SetRts(on) => port.set_rts(*on),
        Action::Delay(ms) => {
            std::thread::sleep(std::time::Duration::from_millis(u64::from(*ms)));
            Ok(())
        }
        Action::Reopen { baud } => port.reopen(*baud),
    }
}

/// The trace sink the driver narrates to.
fn tracer(quiet: bool) -> impl FnMut(&str) {
    move |line: &str| {
        if !quiet {
            println!("{line}");
        }
    }
}

/// Prints an outcome, saying what to do about it rather than only what happened.
fn report(outcome: &Outcome, options: &Options) {
    match outcome {
        Outcome::Verified { device_digest, encoding } => {
            println!(
                "VERIFIED -- the target's own read-back of flash matches the image.\n\
                 device digest {} ({encoding:?})",
                String::from_utf8_lossy(device_digest)
            );
        }
        Outcome::Refused(lamella_esp_serial::Error::NoResponse) => {
            println!(
                "NO RESPONSE -- the target never answered the establishing command.\n\
                 That is almost always a part not in download mode rather than a protocol fault. \
                 With --connector {:?}, the next thing to check is whether these control signals \
                 reach the part at all: run `signals` on this port.",
                options.connector
            );
        }
        Outcome::Refused(lamella_esp_serial::Error::NotVerified { device, expected }) => {
            println!(
                "NOT VERIFIED -- every block was acknowledged and the flash contents still differ.\n\
                 device   {}\n expected {}",
                String::from_utf8_lossy(device),
                String::from_utf8_lossy(expected)
            );
        }
        Outcome::Refused(lamella_esp_serial::Error::Rejected { command, error }) => {
            println!(
                "REJECTED at {} -- error {error:#04x}: {}",
                name_of(*command),
                rom_error(*error)
            );
        }
        Outcome::Refused(why) => println!("REFUSED -- {why:?}"),
        Outcome::Transport(problem) => println!("TRANSPORT FAILURE -- {problem}"),
    }
}
