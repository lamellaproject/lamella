//! `lamella wifi`: the network a board stores and joins -- written with `set`, erased with `clear`,
//! shown with `show` -- through a debug probe wired to the board, or through the board's own USB port
//! while it waits in BOOTSEL, at the place the board's facts give.
//!
//! The passphrase is never a command-line argument and never read from a project: `set` asks for
//! it at a prompt that does not echo, or reads it from standard input with `--passphrase-stdin`. The
//! board keeps it in its flash unencrypted, where a debug probe can read it, and so can anything on
//! its USB port while it waits in BOOTSEL.

use crate::args::{self, Positional, Spec};
use lamella_bsp_gen::strata::StoredSettings;
use lamella_flash_routes::settings::{self as route, SettingsPlace};
use lamella_net_core::wifi::{BootConnection, RecordWrite, Reconnection, SECRET_MAX, SSID_MAX, security};
use lamella_stored_settings::{Held, Record, RegionFlash, RegionStore};
use std::io::{BufRead, IsTerminal as _, Write as _};
use std::process::ExitCode;

/// What `lamella wifi --help` prints, and `--help` on each of its commands.
const WIFI_USAGE: &str = "\
Usage: lamella wifi set <network> --device <device> [--passphrase-stdin]
                        [--wpa3-only] [--at-start join|on-request]
       lamella wifi set <network> --device <device> --open
                        [--at-start join|on-request]
       lamella wifi set --device <device> --at-start join|on-request
       lamella wifi clear --device <device>
       lamella wifi show --device <device>

Store, erase or show the Wi-Fi network a board keeps in its flash, which the
board joins without its program naming it.

The board is reached through a debug probe wired to it, or through its own USB
port while it waits in BOOTSEL, which it does when it is plugged in with its
BOOTSEL button held:
  --device probe:<serial>/<board>
  --device bootsel:<serial>/<board>
<serial> is the probe's serial number, or the board's while it waits in
BOOTSEL, as `lamella devices` lists it, and <board> is the board, as
`lamella boards` lists it. Through a probe, each command stops the board's
program while it works; either way, the board starts again after.

Commands:
  set    Store <network>, the network's name, 1 to 32 bytes. Its passphrase
         is asked for at a prompt that does not show what is typed. The
         network replaces any stored before it, and keeps that network's
         setting for when the board starts, unless --at-start says
         otherwise. When the link is lost, the board joins the network
         again by itself. With no <network>, set changes that setting of the
         stored network alone, and asks for no passphrase.
         --passphrase-stdin  Read the passphrase from standard input instead:
                             its first line.
         --wpa3-only         The board joins the network with WPA3 alone,
                             never with WPA2. The passphrase is then 1 to 128
                             bytes.
         --open              The network has no passphrase.
         --at-start join|on-request
                             When the board starts: join, it joins the
                             network in the background while the program
                             starts; on-request, it joins when the program
                             asks. A board that stored no network takes join.
  clear  Erase the stored network.
  show   Print the stored network's name and settings, never its passphrase.

The board keeps the passphrase in its flash unencrypted, where a debug probe
can read it, and so can anything on its USB port while it waits in BOOTSEL.

Examples:
  lamella wifi set \"Home Net\" --device probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w
  lamella wifi set --device probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w --at-start on-request
  lamella wifi show --device probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w
  lamella wifi show --device bootsel:9B1F2A3C4D5E6F70/raspberrypi-pico-2-w
";

/// What storing a network did.
#[derive(Debug, PartialEq, Eq)]
enum Stored {
    /// The network is stored, and the one it replaced is erased.
    Written,
    /// The board already stored the network as given, so nothing was written.
    Unchanged,
    /// The write stopped at `error`. `replaced` says whether the new network is the stored one even
    /// so: written and read back, with the network it replaced not yet erased.
    Failed { error: String, replaced: bool },
}

/// What the board stored before an erase.
#[derive(Debug, PartialEq, Eq)]
enum Had {
    /// No network.
    Nothing,
    /// The network of this name, as it is shown.
    Network(String),
    /// A network a newer version stored, with a setting this version cannot read.
    Unusable,
}

/// The setting `set` was given, `None` when it was left out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// `--at-start`: what the board does with the network when it starts.
    pub at_start: Option<BootConnection>,
}

impl Settings {
    /// Whether any setting was given.
    fn any(&self) -> bool {
        self.at_start.is_some()
    }

    /// `record`'s settings, with the one given laid over it. When the link is lost, the board always
    /// joins the stored network again by itself.
    fn lay_over(&self, record: &mut Record) {
        if let Some(boot) = self.at_start {
            record.boot = boot;
        }
        record.reconnection = Reconnection::Automatic;
    }
}

/// What changing the stored network's settings alone did.
#[derive(Debug, PartialEq, Eq)]
enum Changed {
    /// The settings are stored. The network's name, as it is shown.
    Written(String),
    /// The network held these settings already, so nothing was written.
    Unchanged(String),
    /// The board stores no network.
    NoNetwork,
    /// A newer version stored the network, with a setting this version cannot read.
    Unusable,
    /// The write stopped at this error.
    Failed(String),
}

/// What erasing the stored network did.
#[derive(Debug, PartialEq, Eq)]
struct Cleared {
    /// What the board stored before.
    had: Had,
    /// Whether both slots read erased after.
    erased: bool,
    /// The flash's error, when it gave one.
    error: Option<String>,
}

/// The two slots at `place` over `flash`, both read.
fn open_slots<'a>(
    flash: &'a mut dyn RegionFlash,
    place: &SettingsPlace,
) -> Result<RegionStore<'a, dyn RegionFlash + 'a>, String> {
    RegionStore::open(flash, place.offset, place.erase_unit)
        .map_err(|why| format!("the stored settings could not be read ({why})"))
}

/// Stores `record` in the slots at `place` over `flash`, with the setting `given`, or when it is left
/// out, the stored network's setting for when the board starts, where one is stored; the board joins
/// the network again by itself when the link is lost.
fn store_network(
    flash: &mut dyn RegionFlash,
    place: &SettingsPlace,
    given: Settings,
    mut record: Record,
) -> Result<Stored, String> {
    let mut slots = open_slots(flash, place)?;
    if let Some(held) = lamella_stored_settings::peek(&slots) {
        record.boot = held.boot;
    }
    given.lay_over(&mut record);
    Ok(match lamella_stored_settings::store(&mut slots, &record) {
        RecordWrite::Written => Stored::Written,
        RecordWrite::Unchanged => Stored::Unchanged,
        RecordWrite::NoRecord | RecordWrite::Failed => {
            let error = slots.error().unwrap_or("the flash did not read back what was written").to_owned();
            let replaced = lamella_stored_settings::peek(&slots).is_some_and(|now| {
                now.ssid() == record.ssid() && now.secret() == record.secret() && now.security == record.security
            });
            Stored::Failed { error, replaced }
        }
    })
}

/// Changes the settings `given` of the network stored at `place` over `flash`, keeping its name, its
/// passphrase and its security as they are: the record is written again through the record's own
/// writer, which copies its later records forward. The passphrase is read from the board for the
/// write alone, and is never shown.
fn change_settings(flash: &mut dyn RegionFlash, place: &SettingsPlace, given: Settings) -> Result<Changed, String> {
    let mut slots = open_slots(flash, place)?;
    let mut record = match lamella_stored_settings::peek_held(&slots) {
        Held::Nothing => return Ok(Changed::NoNetwork),
        Held::Unusable(_) => return Ok(Changed::Unusable),
        Held::Record(_, record) => record,
    };
    given.lay_over(&mut record);
    let name = shown(record.ssid());
    Ok(match lamella_stored_settings::store(&mut slots, &record) {
        RecordWrite::Written => Changed::Written(name),
        RecordWrite::Unchanged => Changed::Unchanged(name),
        RecordWrite::NoRecord | RecordWrite::Failed => {
            Changed::Failed(slots.error().unwrap_or("the flash did not read back what was written").to_owned())
        }
    })
}

/// The settings the board at `place` over `flash` holds now, in `show`'s words, read back after a
/// write; `None` when it holds no network this version can read.
fn settings_now(flash: &mut dyn RegionFlash, place: &SettingsPlace) -> Option<String> {
    match show_network(flash, place).ok()? {
        Held::Record(_, record) => Some(settings_lines(record.boot, record.reconnection)),
        Held::Nothing | Held::Unusable(_) => None,
    }
}

/// The two settings as `show` prints them, each on its own indented line.
fn settings_lines(boot: BootConnection, reconnection: Reconnection) -> String {
    format!("  When the board starts:  {}\n  When the link is lost:  {}", start_text(boot), lost_text(reconnection))
}

/// What the board does with the network when it starts, as `show` says it.
fn start_text(boot: BootConnection) -> &'static str {
    match boot {
        BootConnection::Background | BootConnection::BeforeMain => "it joins in the background while the program starts",
        BootConnection::OnConnect => "it joins when the program asks",
    }
}

/// What the board does when the link is lost, as `show` says it.
fn lost_text(reconnection: Reconnection) -> &'static str {
    match reconnection {
        Reconnection::Automatic => "it joins again by itself",
        Reconnection::Manual => "it joins again when the program asks",
    }
}

/// Erases the stored network at `place` over `flash`.
fn clear_network(flash: &mut dyn RegionFlash, place: &SettingsPlace) -> Result<Cleared, String> {
    let mut slots = open_slots(flash, place)?;
    let had = match lamella_stored_settings::peek_held(&slots) {
        Held::Nothing => Had::Nothing,
        Held::Record(_, held) => Had::Network(shown(held.ssid())),
        Held::Unusable(_) => Had::Unusable,
    };
    let erased = lamella_stored_settings::clear(&mut slots);
    Ok(Cleared { had, erased, error: slots.error().map(str::to_owned) })
}

/// What the stored settings at `place` over `flash` hold, read without writing.
fn show_network(flash: &mut dyn RegionFlash, place: &SettingsPlace) -> Result<Held, String> {
    Ok(lamella_stored_settings::peek_held(&open_slots(flash, place)?))
}

/// What `show` prints of what `board` stores: the network's name and settings, never its passphrase,
/// or that a newer version stored it with a setting this version cannot read.
fn describe_held(board: &str, held: &Held) -> String {
    match held {
        Held::Nothing => describe(board, None),
        Held::Record(_, record) => describe(board, Some(record)),
        Held::Unusable(_) => format!(
            "{board} stores a network that a newer version stored, with a setting this version of Lamella cannot \
             read.\n  Update Lamella to show it, or store the network again with `lamella wifi set`.\n  A firmware \
             that cannot read it either joins no stored network."
        ),
    }
}

/// What `show` prints of `record`, the network `board` stores: its name and settings, never its
/// passphrase.
fn describe(board: &str, record: Option<&Record>) -> String {
    let Some(record) = record else {
        return format!("{board} stores no network.");
    };
    let kinds = match record.security {
        security::OPEN => "open, with no passphrase",
        security::WPA2 => "WPA2",
        security::WPA3 => "WPA3",
        kinds if kinds == security::WPA2 | security::WPA3 => "WPA2 or WPA3",
        _ => "a combination this tool does not name",
    };
    format!(
        "{board} stores the network \"{}\".\n  Security:               {kinds}\n{}",
        shown(record.ssid()),
        settings_lines(record.boot, record.reconnection)
    )
}

/// A network's name as it is printed: its bytes as text, with anything that would not print plainly
/// escaped.
fn shown(name: &[u8]) -> String {
    String::from_utf8_lossy(name).escape_debug().to_string()
}

/// A passphrase held in memory, wiped when dropped.
#[derive(Default)]
struct Secret(Vec<u8>);

impl Drop for Secret {
    fn drop(&mut self) {
        let capacity = self.0.capacity();
        self.0.clear();
        self.0.resize(capacity, 0);
        std::hint::black_box(&self.0);
    }
}

/// The passphrase on the first line `input` holds, its line ending removed.
fn passphrase_line(input: &mut dyn BufRead) -> Result<Secret, String> {
    let mut secret = Secret(Vec::with_capacity(256));
    input.read_until(b'\n', &mut secret.0).map_err(|why| format!("the passphrase could not be read ({why})"))?;
    if secret.0.last() == Some(&b'\n') {
        secret.0.pop();
        if secret.0.last() == Some(&b'\r') {
            secret.0.pop();
        }
    }
    if secret.0.is_empty() {
        return Err(String::from("no passphrase was given; a network without one is stored with --open"));
    }
    Ok(secret)
}

/// Why the passphrase is not asked for at a terminal that cannot hide what is typed.
const CANNOT_HIDE: &str = "this terminal cannot hide what is typed, so the passphrase is not asked for here: \
                           give --passphrase-stdin to read it from standard input";

/// Asks for `network`'s passphrase at the terminal, without showing what is typed.
fn prompt(network: &str) -> Result<Secret, String> {
    if !std::io::stdin().is_terminal() {
        return Err(String::from(
            "standard input is not a terminal to ask for the passphrase at: give --passphrase-stdin to read it \
             from standard input",
        ));
    }
    eprint!("Passphrase for \"{}\": ", shown(network.as_bytes()));
    let _ = std::io::stderr().flush();
    let read = hidden::read_line();
    eprintln!();
    read
}

/// A line read at the console without showing what is typed.
#[cfg(windows)]
#[allow(unsafe_code)]
mod hidden {
    use super::Secret;
    use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
    use windows_sys::Win32::Foundation::{FALSE, INVALID_HANDLE_VALUE, TRUE};
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode,
    };
    use windows_sys::core::BOOL;

    /// The console input whose mode the prompt changes, for [`restore`].
    static INPUT: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
    /// Its mode before the prompt, for [`restore`].
    static MODE: AtomicU32 = AtomicU32::new(0);

    /// Puts the console's input mode back on a control event, such as Ctrl+C, before the handlers
    /// after this one end the process, so the shell does not go on without showing what is typed.
    unsafe extern "system" fn restore(_event: u32) -> BOOL {
        unsafe { SetConsoleMode(INPUT.load(Ordering::SeqCst), MODE.load(Ordering::SeqCst)) };
        FALSE
    }

    /// One line of the console's input, read without showing it.
    pub(super) fn read_line() -> Result<Secret, String> {
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0;
        if input.is_null() || input == INVALID_HANDLE_VALUE || unsafe { GetConsoleMode(input, &mut mode) } == 0 {
            return Err(super::CANNOT_HIDE.to_owned());
        }
        INPUT.store(input, Ordering::SeqCst);
        MODE.store(mode, Ordering::SeqCst);
        unsafe { SetConsoleCtrlHandler(Some(restore), TRUE) };
        let read = if unsafe { SetConsoleMode(input, mode & !ENABLE_ECHO_INPUT) } == 0 {
            Err(super::CANNOT_HIDE.to_owned())
        } else {
            super::passphrase_line(&mut std::io::stdin().lock())
        };
        unsafe {
            SetConsoleMode(input, mode);
            SetConsoleCtrlHandler(Some(restore), FALSE);
        }
        read
    }
}

/// A line read at the terminal without showing what is typed.
#[cfg(unix)]
#[allow(unsafe_code)]
mod hidden {
    use super::Secret;
    use std::sync::OnceLock;

    /// The terminal's settings before the prompt, for [`restore`].
    static SAVED: OnceLock<libc::termios> = OnceLock::new();

    /// The signals that end a process at a terminal, on which the settings are put back first.
    const ENDING: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

    /// Puts the terminal's settings back, then ends the process by `signal` as it would have ended,
    /// so the shell does not go on without showing what is typed.
    extern "C" fn restore(signal: libc::c_int) {
        if let Some(saved) = SAVED.get() {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
        }
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// Installs [`restore`] for `signal` and returns the handler it replaced; `None` where the signal
    /// was ignored on entry and stays ignored, as for a program a shell runs in the background.
    fn install(signal: libc::c_int) -> Option<libc::sigaction> {
        unsafe {
            let mut previous: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut previous) != 0 || previous.sa_sigaction == libc::SIG_IGN {
                return None;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = restore as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            (libc::sigaction(signal, &action, std::ptr::null_mut()) == 0).then_some(previous)
        }
    }

    /// One line of the terminal's input, read without showing it.
    pub(super) fn read_line() -> Result<Secret, String> {
        let mut before: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut before) } != 0 {
            return Err(super::CANNOT_HIDE.to_owned());
        }
        let _ = SAVED.set(before);
        let replaced = ENDING.map(install);
        let mut hidden = before;
        hidden.c_lflag &= !libc::ECHO;
        let read = if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &hidden) } != 0 {
            Err(super::CANNOT_HIDE.to_owned())
        } else {
            super::passphrase_line(&mut std::io::stdin().lock())
        };
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &before) };
        for (signal, previous) in ENDING.iter().zip(replaced.iter()) {
            if let Some(previous) = previous {
                unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
            }
        }
        read
    }
}

/// How `set` takes the passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passphrase {
    /// Asked for at a prompt that does not echo.
    Prompt,
    /// Read from standard input, one line.
    Stdin,
    /// None: the network is open.
    Open,
}

/// The board a `wifi` command reaches, and the way it reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// A debug probe by its serial, and the board it is wired to, which a probe cannot name itself.
    Probe { serial: String, board: String },
    /// A board waiting in BOOTSEL by the serial number its boot ROM answers with, and the board, which
    /// BOOTSEL does not name: it says what chip waits, never which board.
    Bootsel { serial: String, board: String },
}

impl Reach {
    /// The serial number the device is named by, and the board.
    fn parts(&self) -> (&str, &str) {
        match self {
            Reach::Probe { serial, board } | Reach::Bootsel { serial, board } => (serial, board),
        }
    }
}

/// What `lamella wifi set` was asked to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRequest {
    /// The network's name; `None` when only the stored network's settings change.
    pub network: Option<String>,
    /// The board, and how it is reached.
    pub reach: Reach,
    /// Where the passphrase comes from.
    pub passphrase: Passphrase,
    /// Whether the board joins the network with WPA3 alone, never with WPA2.
    pub wpa3_only: bool,
    /// The settings given.
    pub settings: Settings,
}

/// What a command did once it reached the board.
struct Said {
    /// Its result: a line for standard output, or the failure for standard error.
    result: Result<String, String>,
    /// Why the board did not start again, when it did not.
    restart: Option<String>,
}

/// Runs `lamella wifi` with `rest`.
pub fn wifi_command(rest: &[String]) -> ExitCode {
    if rest.iter().any(|word| word == "--help" || word == "-h") {
        println!("{}", WIFI_USAGE.trim_end());
        return ExitCode::SUCCESS;
    }
    let done = match rest.split_first() {
        Some((command, words)) if command == "set" => set(words),
        Some((command, words)) if command == "clear" => clear(words),
        Some((command, words)) if command == "show" => show(words),
        Some((word, _)) if word.starts_with('-') => Err(args::refusal(
            "wifi",
            &format!("unknown option \"{word}\": a command comes first, set, clear or show"),
        )),
        Some((word, _)) => {
            Err(args::refusal("wifi", &format!("unknown command \"{word}\": the commands are set, clear and show")))
        }
        None => Err(args::refusal("wifi", "give a command: set, clear or show")),
    };
    let said = match done {
        Ok(said) => said,
        Err(refusal) => {
            eprintln!("{refusal}");
            return ExitCode::FAILURE;
        }
    };
    let code = match said.result {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::FAILURE
        }
    };
    if let Some(why) = said.restart {
        eprintln!("The board did not start again ({why}). Unplug it and plug it back in to start it.");
    }
    code
}

/// `lamella wifi set`.
fn set(words: &[String]) -> Result<Said, String> {
    let request = parse_set(words)?;
    let Some(network) = request.network.as_deref() else {
        return set_settings(&request);
    };
    let (serial, board) = request.reach.parts();
    let place = place_of("set", &request.reach)?;
    let secret = match request.passphrase {
        Passphrase::Open => Secret::default(),
        Passphrase::Stdin => passphrase_line(&mut std::io::stdin().lock()).map_err(|why| failed("set", &why))?,
        Passphrase::Prompt => prompt(network).map_err(|why| failed("set", &why))?,
    };
    let kinds = kinds_of(&request, &secret.0).map_err(|why| failed("set", &why))?;
    let record =
        Record::new(network.as_bytes(), &secret.0, kinds, Reconnection::Automatic, BootConnection::Background)
            .ok_or_else(|| failed("set", "the network cannot be stored as given"))?;
    drop(secret);
    let name = shown(record.ssid());
    let given = request.settings;
    let ((stored, now), started) = route::with_flash(&place, Some(serial), |flash| {
        let stored = store_network(flash, &place, given, record);
        let now = settings_now(flash, &place).map(|lines| format!("\n{lines}")).unwrap_or_default();
        (stored, now)
    })
    .map_err(|why| failed("set", &why))?;
    let result = match stored {
        Ok(Stored::Written) => Ok(format!("Stored the network \"{name}\" on {board}.{now}")),
        Ok(Stored::Unchanged) => {
            Ok(format!("{board} already stores the network \"{name}\" as given, so nothing was written.{now}"))
        }
        Ok(Stored::Failed { error, replaced: true }) => Err(failed(
            "set",
            &format!(
                "the network \"{name}\" is stored, but the network it replaces could not be erased ({error}); run \
                 the same command again to erase it"
            ),
        )),
        Ok(Stored::Failed { error, replaced: false }) => Err(failed(
            "set",
            &format!("the network was not stored ({error}); {board} keeps the network it stored before, if any"),
        )),
        Err(why) => Err(failed("set", &why)),
    };
    Ok(Said { result, restart: started.err() })
}

/// `lamella wifi set` with no network: the stored network's settings changed alone.
fn set_settings(request: &SetRequest) -> Result<Said, String> {
    let (serial, board) = request.reach.parts();
    let place = place_of("set", &request.reach)?;
    let given = request.settings;
    let ((changed, now), started) = route::with_flash(&place, Some(serial), |flash| {
        let changed = change_settings(flash, &place, given);
        let now = settings_now(flash, &place).map(|lines| format!("\n{lines}")).unwrap_or_default();
        (changed, now)
    })
    .map_err(|why| failed("set", &why))?;
    let result = match changed {
        Ok(Changed::Written(name)) => Ok(format!("Changed the settings of the network \"{name}\" on {board}.{now}")),
        Ok(Changed::Unchanged(name)) => Ok(format!(
            "The network \"{name}\" on {board} already has these settings, so nothing was written.{now}"
        )),
        Ok(Changed::NoNetwork) => Err(failed(
            "set",
            &format!("{board} stores no network whose settings could change; store one with `lamella wifi set <network>`"),
        )),
        Ok(Changed::Unusable) => Err(failed(
            "set",
            &format!(
                "{board} stores a network that a newer version stored, with a setting this version of Lamella cannot \
                 read, so its settings are not changed; update Lamella, or store the network again with `lamella wifi \
                 set <network>`"
            ),
        )),
        Ok(Changed::Failed(error)) => Err(failed(
            "set",
            &format!("the settings were not stored completely ({error}); run the same command again"),
        )),
        Err(why) => Err(failed("set", &why)),
    };
    Ok(Said { result, restart: started.err() })
}

/// `lamella wifi clear`.
fn clear(words: &[String]) -> Result<Said, String> {
    let reach = parse_device("clear", words, &CLEAR)?;
    let (serial, board) = reach.parts();
    let place = place_of("clear", &reach)?;
    let (cleared, started) = route::with_flash(&place, Some(serial), |flash| clear_network(flash, &place))
        .map_err(|why| failed("clear", &why))?;
    let result = match cleared {
        Ok(Cleared { erased: true, had: Had::Network(name), .. }) => {
            Ok(format!("Erased the network \"{name}\" from {board}."))
        }
        Ok(Cleared { erased: true, had: Had::Unusable, .. }) => {
            Ok(format!("Erased the network a newer version stored on {board}."))
        }
        Ok(Cleared { erased: true, had: Had::Nothing, .. }) => Ok(format!("{board} stores no network.")),
        Ok(Cleared { erased: false, error, .. }) => Err(failed(
            "clear",
            &format!(
                "the stored network was not erased completely ({})",
                error.as_deref().unwrap_or("a slot did not read erased after its erase")
            ),
        )),
        Err(why) => Err(failed("clear", &why)),
    };
    Ok(Said { result, restart: started.err() })
}

/// `lamella wifi show`.
fn show(words: &[String]) -> Result<Said, String> {
    let reach = parse_device("show", words, &SHOW)?;
    let (serial, board) = reach.parts();
    let place = place_of("show", &reach)?;
    let (held, started) = route::with_flash(&place, Some(serial), |flash| show_network(flash, &place))
        .map_err(|why| failed("show", &why))?;
    let result = held.map(|held| describe_held(board, &held)).map_err(|why| failed("show", &why));
    Ok(Said { result, restart: started.err() })
}

/// What `set` takes besides the options that would carry a passphrase, which it refuses first.
const SET: Spec = Spec {
    verb: "wifi set",
    usage: Some(WIFI_USAGE),
    values: &["--device", "--at-start"],
    repeats: &[],
    flags: &["--passphrase-stdin", "--wpa3-only", "--open"],
    positional: Positional::One("network"),
};

/// What `clear` takes.
const CLEAR: Spec = Spec {
    verb: "wifi clear",
    usage: Some(WIFI_USAGE),
    values: &["--device"],
    repeats: &[],
    flags: &[],
    positional: Positional::None,
};

/// What `show` takes.
const SHOW: Spec = Spec {
    verb: "wifi show",
    usage: Some(WIFI_USAGE),
    values: &["--device"],
    repeats: &[],
    flags: &[],
    positional: Positional::None,
};

/// Options another tool takes a passphrase in, refused for what they are rather than as unknown.
const SECRET_OPTIONS: &[&str] = &["--passphrase", "--password", "--psk", "--key"];

/// `lamella wifi <command>`'s refusal of `wrong`, with that command's own line of the usage.
fn refusal(command: &str, wrong: &str) -> String {
    args::refusal(&format!("wifi {command}"), wrong)
}

/// `lamella wifi <command>`'s failure, `why`, once its command line was read.
fn failed(command: &str, why: &str) -> String {
    let why = if why.ends_with(['.', '?', '!']) { why.to_owned() } else { format!("{why}.") };
    format!("lamella wifi {command}: {why}")
}

/// The request `set`'s words make, or the refusal to print.
fn parse_set(words: &[String]) -> Result<SetRequest, String> {
    let carries_secret =
        |word: &String| SECRET_OPTIONS.iter().any(|option| word == option || word.starts_with(&format!("{option}=")));
    if words.iter().any(carries_secret) {
        return Err(refusal(
            "set",
            "a passphrase is never given on the command line, where a shell's history keeps it: `lamella wifi set` \
             asks for it at a prompt that does not echo, or reads it from standard input with --passphrase-stdin",
        ));
    }
    let parsed = args::parse(words, &SET)?;
    let network = parsed.positionals().first().map(String::as_str);
    if let Some(network) = network
        && (network.is_empty() || network.len() > SSID_MAX)
    {
        return Err(refusal("set", &format!("the network's name \"{network}\" is {} bytes; a name is 1 to 32 bytes", network.len())));
    }
    let settings = Settings {
        at_start: match parsed.value("--at-start") {
            None => None,
            Some("join") => Some(BootConnection::Background),
            Some("on-request") => Some(BootConnection::OnConnect),
            Some(other) => return Err(refusal("set", &format!("--at-start takes join or on-request, not \"{other}\""))),
        },
    };
    if network.is_none() {
        if !settings.any() {
            return Err(refusal(
                "set",
                "give the network's name to store it, or --at-start to change the stored network's setting for when the board starts",
            ));
        }
        if let Some(describing) = ["--open", "--passphrase-stdin", "--wpa3-only"].into_iter().find(|flag| parsed.flag(flag)) {
            return Err(refusal(
                "set",
                &format!("{describing} describes a network to store, and no network is named: give its name, or leave {describing} out to change the stored network's settings alone"),
            ));
        }
    }
    let device = parsed.value("--device").ok_or_else(|| refusal("set", NO_DEVICE))?;
    let reach = parse_reach("set", device)?;
    let passphrase = match (parsed.flag("--open"), parsed.flag("--passphrase-stdin")) {
        (true, true) => {
            return Err(refusal("set", "--open and --passphrase-stdin cannot be given together: an open network has no passphrase"));
        }
        (true, false) => Passphrase::Open,
        (false, true) => Passphrase::Stdin,
        (false, false) => Passphrase::Prompt,
    };
    let wpa3_only = parsed.flag("--wpa3-only");
    if wpa3_only && passphrase == Passphrase::Open {
        return Err(refusal("set", "--wpa3-only and --open cannot be given together: an open network is joined with no security at all"));
    }
    Ok(SetRequest { network: network.map(str::to_owned), reach, passphrase, wpa3_only, settings })
}

/// What a command that names no `--device` is told.
const NO_DEVICE: &str = "names no --device: the debug probe wired to the board, as --device probe:<serial>/<board>, or the \
                         board waiting in BOOTSEL, as --device bootsel:<serial>/<board>";

/// The board `clear` or `show` reaches, and how, from `words`, or the refusal to print.
fn parse_device(command: &str, words: &[String], spec: &Spec) -> Result<Reach, String> {
    let parsed = args::parse(words, spec)?;
    let device = parsed.value("--device").ok_or_else(|| refusal(command, NO_DEVICE))?;
    parse_reach(command, device)
}

/// The board `--device` names and how it is reached, or the refusal of `command`'s command line.
fn parse_reach(command: &str, device: &str) -> Result<Reach, String> {
    let verb = format!("wifi {command}");
    let (named, through_probe) = if let Some(named) = device.strip_prefix("probe:") {
        (named, true)
    } else if let Some(named) = device.strip_prefix("bootsel:") {
        (named, false)
    } else {
        return Err(args::refusal_trying(
            &verb,
            &format!(
                "--device \"{device}\" is neither a debug probe nor a board waiting in BOOTSEL. A board's stored \
                 settings are written through a debug probe wired to it, --device probe:<serial>/<board>, or through \
                 the board's own USB port while it waits in BOOTSEL, --device bootsel:<serial>/<board>"
            ),
            "lamella devices",
        ));
    };
    let Some((serial, board)) = named.split_once('/').filter(|(serial, board)| !serial.is_empty() && !board.is_empty())
    else {
        let why = if through_probe {
            "A probe can be wired to any board, so the device names the one it is wired to"
        } else {
            "A board waiting in BOOTSEL does not say which board it is, so the device names it"
        };
        return Err(args::refusal_trying(
            &verb,
            &format!("--device \"{device}\" names no board. {why}: --device {device}/<board>"),
            "lamella boards",
        ));
    };
    if lamella_catalog::load_board(board).is_none() {
        return Err(args::refusal_trying(
            &verb,
            &format!("--device names the board \"{board}\", which is not a board this build knows"),
            "lamella boards",
        ));
    }
    let (serial, board) = (serial.to_owned(), board.to_owned());
    Ok(if through_probe { Reach::Probe { serial, board } } else { Reach::Bootsel { serial, board } })
}

/// Where `board` keeps its stored settings, or the refusal that names the board.
fn place(command: &str, board: &str) -> Result<StoredSettings, String> {
    let table = lamella_catalog::load_board(board).ok_or_else(|| {
        format!("lamella wifi {command}: \"{board}\" is not a board this build knows.\nTry: lamella boards")
    })?;
    table.stored_settings.ok_or_else(|| {
        format!("lamella wifi {command}: {board} keeps no stored settings: its board facts state no place for them.")
    })
}

/// Where `board` keeps its stored settings as its probe route reaches them, or the failure that
/// names the board.
fn reach_place(command: &str, board: &str) -> Result<SettingsPlace, String> {
    let settings = place(command, board)?;
    route::place(board, &settings).map_err(|why| failed(command, &why))
}

/// Where the board `reach` names keeps its stored settings, as the way `reach` takes reaches them, or the
/// failure that names the board.
fn place_of(command: &str, reach: &Reach) -> Result<SettingsPlace, String> {
    match reach {
        Reach::Probe { board, .. } => reach_place(command, board),
        Reach::Bootsel { board, .. } => {
            let settings = place(command, board)?;
            route::place_in_bootsel(board, &settings).map_err(|why| failed(command, &why))
        }
    }
}

/// The security kinds a join from the stored record accepts, for `passphrase`, or why it is not one:
/// WPA2 or WPA3, for 8 to 63 printable characters or the 64-digit hexadecimal key.
fn kinds_for(passphrase: &[u8]) -> Result<u8, String> {
    let printable = passphrase.iter().all(|&byte| (0x20..=0x7E).contains(&byte));
    let key = passphrase.len() == 64 && passphrase.iter().all(u8::is_ascii_hexdigit);
    if key || (printable && (8..=63).contains(&passphrase.len())) {
        Ok(security::WPA2 | security::WPA3)
    } else {
        Err(String::from(
            "a WPA2 or WPA3 passphrase is 8 to 63 printable characters, or the 64-digit hexadecimal key",
        ))
    }
}

/// The security kinds `request` stores for `secret`, or why `secret` cannot be stored with them.
fn kinds_of(request: &SetRequest, secret: &[u8]) -> Result<u8, String> {
    match request.passphrase {
        Passphrase::Open => Ok(security::OPEN),
        Passphrase::Stdin | Passphrase::Prompt if request.wpa3_only => wpa3_kinds_for(secret),
        Passphrase::Stdin | Passphrase::Prompt => kinds_for(secret),
    }
}

/// The security kinds a join from the stored record accepts when it takes WPA3 alone, for
/// `passphrase`, or why it is not one: a WPA3 password is 1 to 128 bytes, as `WiFiSecurity.Wpa3`
/// takes it.
fn wpa3_kinds_for(passphrase: &[u8]) -> Result<u8, String> {
    if (1..=SECRET_MAX).contains(&passphrase.len()) {
        Ok(security::WPA3)
    } else {
        Err(format!("a WPA3 passphrase is 1 to {SECRET_MAX} bytes; this one is {} bytes", passphrase.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &[&str]) -> Vec<String> {
        text.iter().map(|word| (*word).to_owned()).collect()
    }

    const PROBE: &str = "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w";

    /// No setting given: each one keeps what is stored.
    const KEEP: Settings = Settings { at_start: None };

    #[test]
    fn set_takes_the_network_and_a_probe_that_names_its_board() {
        let request = parse_set(&words(&["Home Net", "--device", PROBE])).expect("parses");
        assert_eq!(request.network.as_deref(), Some("Home Net"));
        assert_eq!(
            request.reach,
            Reach::Probe { serial: "E660C0D1A2B3C4D5".into(), board: "raspberrypi-pico-2-w".into() }
        );
        assert_eq!(request.passphrase, Passphrase::Prompt);
        let stdin = parse_set(&words(&["Home Net", "--device", PROBE, "--passphrase-stdin"])).expect("parses");
        assert_eq!(stdin.passphrase, Passphrase::Stdin);
        let open = parse_set(&words(&["Cafe", "--open", "--device", PROBE])).expect("parses");
        assert_eq!(open.passphrase, Passphrase::Open);
        assert!(!request.wpa3_only && !stdin.wpa3_only && !open.wpa3_only, "WPA3 alone only when asked for");
    }

    /// **`--wpa3-only` STORES WPA3 ALONE**, with the passphrase from the prompt or from standard
    /// input, and is refused beside `--open`, which joins with no security at all.
    #[test]
    fn wpa3_only_is_taken_with_a_passphrase_and_refused_beside_open() {
        let prompt = parse_set(&words(&["Home Net", "--device", PROBE, "--wpa3-only"])).expect("parses");
        assert_eq!((prompt.passphrase, prompt.wpa3_only), (Passphrase::Prompt, true));
        let stdin = parse_set(&words(&["Home Net", "--wpa3-only", "--passphrase-stdin", "--device", PROBE])).expect("parses");
        assert_eq!((stdin.passphrase, stdin.wpa3_only), (Passphrase::Stdin, true));
        for order in [["--open", "--wpa3-only"], ["--wpa3-only", "--open"]] {
            let mut given = words(&["Cafe", "--device", PROBE]);
            given.extend(words(&order));
            let refusal = parse_set(&given).expect_err("refused");
            assert!(refusal.contains("--wpa3-only and --open cannot be given together"), "{refusal}");
        }
        assert!(WIFI_USAGE.contains("--wpa3-only"), "the usage offers it");
    }

    /// A passphrase a join taking WPA3 alone takes: 1 to 128 bytes, as `WiFiSecurity.Wpa3` takes it,
    /// with neither WPA2's 8 to 63 characters nor its printable rule.
    #[test]
    fn a_wpa3_only_passphrase_is_1_to_128_bytes() {
        for taken in [&b"short"[..], "a".repeat(64).as_bytes(), "b".repeat(128).as_bytes(), "caf\u{e9}-passphrase".as_bytes()] {
            assert_eq!(wpa3_kinds_for(taken), Ok(security::WPA3), "{} bytes", taken.len());
        }
        let refusal = wpa3_kinds_for("c".repeat(129).as_bytes()).expect_err("refused");
        assert_eq!(refusal, "a WPA3 passphrase is 1 to 128 bytes; this one is 129 bytes");
        assert!(wpa3_kinds_for(b"").is_err(), "an empty passphrase is refused");
    }

    /// `set` stores WPA3 alone only when asked, both secured kinds otherwise, and open for `--open`;
    /// WPA2's passphrase rule holds whenever WPA2 is accepted.
    #[test]
    fn set_stores_wpa3_alone_only_when_asked() {
        let both = parse_set(&words(&["Home Net", "--device", PROBE, "--passphrase-stdin"])).expect("parses");
        let alone = parse_set(&words(&["Home Net", "--device", PROBE, "--passphrase-stdin", "--wpa3-only"])).expect("parses");
        let open = parse_set(&words(&["Cafe", "--device", PROBE, "--open"])).expect("parses");
        assert_eq!(kinds_of(&both, b"hunter2hunter2"), Ok(security::WPA2 | security::WPA3));
        assert_eq!(kinds_of(&alone, b"hunter2hunter2"), Ok(security::WPA3));
        assert_eq!(kinds_of(&open, b""), Ok(security::OPEN));
        assert!(kinds_of(&both, b"short").is_err(), "without --wpa3-only, WPA2's rule holds");
        assert_eq!(kinds_of(&alone, b"short"), Ok(security::WPA3));
    }

    /// The passphrase never reaches the command line, where a shell's history and a process list
    /// keep it: an option that would carry it is refused by what it is, before anything opens.
    #[test]
    fn a_passphrase_given_on_the_command_line_is_refused_for_what_it_is() {
        for spelling in [&["--passphrase", "hunter2hunter2"][..], &["--passphrase=hunter2hunter2"], &["--password", "x"]] {
            let mut given = words(&["Home Net", "--device", PROBE]);
            given.extend(words(spelling));
            let refusal = parse_set(&given).expect_err("refused");
            assert!(refusal.contains("never given on the command line"), "{refusal}");
            assert!(refusal.contains("--passphrase-stdin"), "{refusal}");
            assert!(!refusal.contains("hunter2"), "the refusal does not repeat it: {refusal}");
        }
    }

    #[test]
    fn set_refuses_an_open_network_with_a_passphrase_and_a_name_of_the_wrong_length() {
        let both = parse_set(&words(&["Net", "--open", "--passphrase-stdin", "--device", PROBE])).expect_err("refused");
        assert!(both.contains("--open") && both.contains("--passphrase-stdin"), "{both}");
        let long = "n".repeat(33);
        let refusal = parse_set(&words(&[&long, "--device", PROBE])).expect_err("refused");
        assert!(refusal.contains("1 to 32 bytes"), "{refusal}");
        let missing = parse_set(&words(&["Net"])).expect_err("refused");
        assert!(missing.contains("--device"), "{missing}");
    }

    /// A probe can be wired to any board, so its id must name the board; and the stored settings are
    /// reached through a probe or the board's boot ROM, so another kind of device is refused by name.
    #[test]
    fn the_device_is_a_probe_that_names_its_board() {
        let unnamed = parse_reach("set", "probe:E660C0D1A2B3C4D5").expect_err("refused");
        assert!(unnamed.contains("names no board"), "{unnamed}");
        assert!(unnamed.contains("probe:E660C0D1A2B3C4D5/<board>"), "{unnamed}");
        let port = parse_reach("set", "COM8").expect_err("refused");
        assert!(port.contains("through a debug probe"), "{port}");
        let unknown = parse_reach("set", "probe:E660C0D1A2B3C4D5/no-such-board").expect_err("refused");
        assert!(unknown.contains("no-such-board"), "{unknown}");
    }

    /// The place is the board's fact, never this tool's constant: a board whose facts state none is
    /// refused by name, and one that states it is answered with it.
    #[test]
    fn the_place_is_the_boards_and_a_board_that_states_none_is_refused_by_name() {
        let refusal = place("set", "raspberrypi-pico-2").expect_err("refused");
        assert!(refusal.contains("raspberrypi-pico-2") && refusal.contains("keeps no stored settings"), "{refusal}");
        let settings = place("set", "raspberrypi-pico-2-w").expect("the Pico 2 W states its place");
        assert_eq!((settings.offset, settings.size, settings.erase_unit, settings.slots), (0x3F_E000, 0x2000, 0x1000, 2));
        assert_eq!(place("set", "pimoroni-pico-plus-2-w").expect("states it").offset, 0xFF_E000);
    }

    const BOOTSEL: &str = "bootsel:9B1F2A3C4D5E6F70/raspberrypi-pico-2-w";

    /// The Pico 2 W waiting in BOOTSEL, as [`BOOTSEL`] names it.
    fn waiting() -> Reach {
        Reach::Bootsel { serial: "9B1F2A3C4D5E6F70".into(), board: "raspberrypi-pico-2-w".into() }
    }

    /// A board waiting in BOOTSEL is reached through its own USB port by the serial number it answers
    /// with there, and the device names its board as a probe's does: BOOTSEL says what chip waits,
    /// never which board.
    #[test]
    fn the_device_may_be_a_board_waiting_in_bootsel_by_its_serial_and_its_board() {
        assert_eq!(parse_reach("set", BOOTSEL), Ok(waiting()));
        assert_eq!(parse_set(&words(&["Home Net", "--device", BOOTSEL])).expect("parses").reach, waiting());
        assert_eq!(parse_device("clear", &words(&["--device", BOOTSEL]), &CLEAR), Ok(waiting()));
        assert_eq!(parse_device("show", &words(&["--device", BOOTSEL]), &SHOW), Ok(waiting()));
        let unnamed = parse_reach("show", "bootsel:9B1F2A3C4D5E6F70").expect_err("refused");
        assert!(unnamed.contains("names no board"), "{unnamed}");
        assert!(unnamed.contains("--device bootsel:9B1F2A3C4D5E6F70/<board>"), "{unnamed}");
        let unknown = parse_reach("clear", "bootsel:9B1F2A3C4D5E6F70/no-such-board").expect_err("refused");
        assert!(unknown.contains("no-such-board"), "{unknown}");
        let port = parse_reach("set", "COM8").expect_err("refused");
        for form in ["--device probe:<serial>/<board>", "--device bootsel:<serial>/<board>"] {
            assert!(port.contains(form), "{port}");
        }
    }

    /// Through BOOTSEL a board's settings are where its probe route reaches them, in the same flash;
    /// the board's boot ROM is the way there. A board that keeps none is refused as it is through a
    /// probe.
    #[test]
    fn a_board_waiting_in_bootsel_is_reached_at_the_place_its_probe_route_reaches() {
        let through_rom = place_of("set", &waiting()).expect("a place");
        let through_probe = reach_place("set", "raspberrypi-pico-2-w").expect("a place");
        assert_eq!((through_rom.offset, through_rom.erase_unit), (through_probe.offset, through_probe.erase_unit));
        assert_ne!(through_rom.route, through_probe.route, "the boot ROM's way, not the probe's");
        let settings = place("set", "raspberrypi-pico-2-w").expect("states it");
        assert_eq!(Ok(through_rom), route::place_in_bootsel("raspberrypi-pico-2-w", &settings));
        let probe = Reach::Probe { serial: "E660C0D1A2B3C4D5".into(), board: "raspberrypi-pico-2-w".into() };
        assert_eq!(place_of("set", &probe), Ok(through_probe));
        let none = Reach::Bootsel { serial: "9B1F2A3C4D5E6F70".into(), board: "raspberrypi-pico-2".into() };
        let refusal = place_of("show", &none).expect_err("refused");
        assert!(refusal.contains("raspberrypi-pico-2") && refusal.contains("keeps no stored settings"), "{refusal}");
    }

    /// The usage names both ways to a board and no chip: the command is the same whatever a board's
    /// family, and the way follows the device given and the board's facts.
    #[test]
    fn the_usage_names_both_ways_to_a_board_and_no_chip() {
        for form in ["--device probe:<serial>/<board>", "--device bootsel:<serial>/<board>"] {
            assert!(WIFI_USAGE.contains(form), "{form}");
        }
        for chip in ["RP2350", "RP2040", "rp2350", "rp2040"] {
            assert!(!WIFI_USAGE.contains(chip), "the usage names the chip {chip}");
        }
    }

    /// Every board whose facts state no place for the stored settings -- one with no radio, or one
    /// whose firmware does not run the radio it carries -- is refused by each command by name, in
    /// plain words. Each command asks for the place first, before a passphrase is asked for or a
    /// probe is opened.
    #[test]
    fn every_board_that_states_no_place_is_refused_by_each_command_by_name() {
        let without: Vec<&str> = lamella_catalog::BOARDS
            .iter()
            .map(|(board, _)| *board)
            .filter(|board| lamella_catalog::load_board(board).is_some_and(|table| table.stored_settings.is_none()))
            .collect();
        assert!(without.contains(&"raspberrypi-pico-2"), "{without:?}");
        for board in without {
            for command in ["set", "clear", "show"] {
                let refusal = reach_place(command, board).expect_err("refused");
                assert_eq!(
                    refusal,
                    format!(
                        "lamella wifi {command}: {board} keeps no stored settings: its board facts state no place for them."
                    )
                );
            }
        }
    }

    /// A passphrase a WPA2 or WPA3 network takes: 8 to 63 printable characters, or the 64-digit
    /// hexadecimal key. Anything else is refused before the board is reached.
    #[test]
    fn a_passphrase_is_8_to_63_printable_characters_or_64_hexadecimal_digits() {
        let both = security::WPA2 | security::WPA3;
        assert_eq!(kinds_for(b"12345678"), Ok(both));
        assert_eq!(kinds_for("a".repeat(63).as_bytes()), Ok(both));
        assert_eq!(kinds_for("0123456789abcdef".repeat(4).as_bytes()), Ok(both));
        for wrong in [&b"1234567"[..], "g".repeat(64).as_bytes(), "a".repeat(65).as_bytes(), b"tab\there-1", "caf\u{e9}-passphrase".as_bytes()] {
            let refusal = kinds_for(wrong).expect_err("refused");
            assert!(refusal.contains("8 to 63"), "{refusal}");
        }
    }

    use lamella_net_core::wifi::{BootConnection, RecordWrite, Reconnection};
    use lamella_stored_settings::RegionStore;

    /// Where a Pico 2 W keeps its stored settings, as its probe route reaches them.
    fn pico_place() -> SettingsPlace {
        let settings = place("set", "raspberrypi-pico-2-w").expect("the Pico 2 W states its place");
        lamella_flash_routes::settings::place("raspberrypi-pico-2-w", &settings).expect("its probe route reaches it")
    }

    /// The two slots of a Pico 2 W's stored settings in memory, reached at the offsets its probe
    /// route uses, with switches that make every later program or erase fail.
    struct Board {
        bytes: Vec<u8>,
        fail_program: bool,
        fail_erase: bool,
    }

    impl Board {
        fn new() -> Self {
            Board { bytes: vec![0xFF; 0x2000], fail_program: false, fail_erase: false }
        }

        /// Where `offset` falls in the slots, for `len` bytes.
        fn at(offset: u32, len: usize) -> std::ops::Range<usize> {
            let start = (offset - 0x3F_E000) as usize;
            start..start + len
        }
    }

    impl RegionFlash for Board {
        fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
            Ok(self.bytes[Board::at(offset, len)].to_vec())
        }

        fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
            if self.fail_erase {
                return Err(String::from("erasing: the probe stopped answering"));
            }
            self.bytes[Board::at(offset, len as usize)].fill(0xFF);
            Ok(())
        }

        fn program(&mut self, offset: u32, page: &[u8; 256]) -> Result<(), String> {
            if self.fail_program {
                return Err(String::from("programming the page: the probe stopped answering"));
            }
            self.bytes[Board::at(offset, 256)].copy_from_slice(page);
            Ok(())
        }
    }

    /// The record the stored settings at `place` hold, read as `show` reads them, or `None` when they
    /// hold none this version can read.
    fn stored_record(board: &mut Board, place: &SettingsPlace) -> Option<Record> {
        match show_network(board, place).expect("reads") {
            Held::Record(_, record) => Some(record),
            Held::Nothing | Held::Unusable(_) => None,
        }
    }

    /// A secured network with the settings `set` gives a new one.
    fn secured(name: &[u8], secret: &[u8]) -> Record {
        Record::new(name, secret, security::WPA2 | security::WPA3, Reconnection::Automatic, BootConnection::Background)
            .expect("a network")
    }

    /// **`set` STORES THE NETWORK AND KEEPS THE STORED NETWORK's START SETTING**: what the board does
    /// when it starts belongs to the board's program, so a new network takes it over, stored to
    /// rejoin by itself; and the same network given again writes nothing.
    #[test]
    fn set_stores_the_network_and_keeps_the_start_setting_of_the_one_it_replaces() {
        let place = pico_place();
        let mut board = Board::new();
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!((shown.ssid(), shown.boot, shown.reconnection), (&b"Home Net"[..], BootConnection::Background, Reconnection::Automatic));

        let chosen = Record::new(b"Home Net", b"hunter2hunter2", security::WPA2 | security::WPA3, Reconnection::Manual, BootConnection::OnConnect)
            .expect("a network");
        {
            let mut slots = RegionStore::open(&mut board, place.offset, place.erase_unit).expect("opens");
            assert_eq!(lamella_stored_settings::store(&mut slots, &chosen), RecordWrite::Written);
        }
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")), Ok(Stored::Written));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!(shown.ssid(), b"Cafe");
        assert_eq!((shown.boot, shown.reconnection), (BootConnection::OnConnect, Reconnection::Automatic), "the start setting kept");
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")), Ok(Stored::Unchanged));
    }

    /// CRC-32 as the record's check computes it: IEEE 802.3, reflected, polynomial `0xEDB88320`.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Rewrites byte `at` of the slot that holds the record as `value`, and makes the slot's check
    /// again so it verifies.
    fn with_byte(board: &mut Board, place: &SettingsPlace, at: usize, value: u8) {
        let unit = place.erase_unit as usize;
        let first = Board::at(place.offset, unit).start;
        let start = if board.bytes[first..first + 4] == *b"LWFR" { first } else { first + unit };
        board.bytes[start + at] = value;
        let check = crc32(&board.bytes[start..start + 188]);
        board.bytes[start + 188..start + 192].copy_from_slice(&check.to_le_bytes());
    }

    /// Rewrites the boot byte of the slot that holds the record as 1, the retired "join before the
    /// program starts", and makes the slot's check again so it verifies.
    fn retire_boot(board: &mut Board, place: &SettingsPlace) {
        with_byte(board, place, 173, 1);
    }

    /// **A NETWORK A NEWER VERSION STORED, WITH A SETTING OR IN A FORMAT THIS ONE CANNOT READ**:
    /// `show` says so and what to do, `clear` erases it and says what it erased, and `set` stores a
    /// network over it. Reconnection 2 at offset 172, and format 2 at offset 4.
    #[test]
    fn a_network_stored_with_a_setting_this_version_cannot_read_is_shown_cleared_and_replaced_in_words() {
        let place = pico_place();
        let board_name = "raspberrypi-pico-2-w";
        for (at, value) in [(172, 2), (4, 2)] {
            let mut board = Board::new();
            assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));
            with_byte(&mut board, &place, at, value);
            let held = show_network(&mut board, &place).expect("reads");
            assert!(matches!(held, Held::Unusable(_)), "byte {at} as {value}: {held:?}");
            let text = describe_held(board_name, &held);
            assert!(text.starts_with("raspberrypi-pico-2-w stores a network that a newer version stored"), "{text}");
            assert!(text.contains("Update Lamella") && text.contains("`lamella wifi set`"), "{text}");
            assert!(!text.contains("Home Net") && !text.contains("hunter2"), "nothing read from it is shown: {text}");
            assert_eq!(clear_network(&mut board, &place), Ok(Cleared { had: Had::Unusable, erased: true, error: None }));

            assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));
            with_byte(&mut board, &place, at, value);
            assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")), Ok(Stored::Written));
            let shown = stored_record(&mut board, &place).expect("the new network");
            assert_eq!((shown.ssid(), shown.reconnection), (&b"Cafe"[..], Reconnection::Automatic), "stored with a new network's settings");
        }
    }

    /// **A NETWORK HELD WITH THE RETIRED BOOT VALUE IS SHOWN AND STORED AS BACKGROUND**: a slot whose
    /// boot byte is 1 is shown as joining in the background, which is what the board does with it,
    /// and `set` stores 0 in its place, for the same network as for another.
    #[test]
    fn a_network_held_with_the_retired_boot_value_is_shown_and_stored_as_background() {
        let place = pico_place();
        let mut board = Board::new();
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));
        retire_boot(&mut board, &place);
        let held = stored_record(&mut board, &place).expect("still a network");
        assert_eq!(held.boot, BootConnection::BeforeMain, "the slot is valid and reads as the value it holds");
        let text = describe("raspberrypi-pico-2-w", Some(&held));
        assert!(text.contains("When the board starts:  it joins in the background while the program starts"), "{text}");
        for (name, secret) in [(&b"Home Net"[..], &b"hunter2hunter2"[..]), (&b"Cafe"[..], &b"espresso-please"[..])] {
            retire_boot(&mut board, &place);
            assert_eq!(store_network(&mut board, &place, KEEP, secured(name, secret)), Ok(Stored::Written), "a held 1 is written over");
            let shown = stored_record(&mut board, &place).expect("a network");
            assert_eq!((shown.ssid(), shown.boot), (name, BootConnection::Background));
        }
    }

    /// **A FAILED `set` SAYS WHICH NETWORK THE BOARD NOW STORES**: a program that fails leaves the
    /// network stored before, and an erase that fails after the new network was written and read
    /// back leaves the new one stored, with the one it replaced not yet erased.
    #[test]
    fn a_set_that_fails_says_whether_the_new_network_is_the_stored_one() {
        let place = pico_place();
        let mut board = Board::new();
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));

        board.fail_program = true;
        match store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")) {
            Ok(Stored::Failed { error, replaced: false }) => assert!(error.contains("programming the page"), "{error}"),
            other => panic!("{other:?}"),
        }
        board.fail_program = false;
        assert_eq!(stored_record(&mut board, &place).expect("a network").ssid(), b"Home Net");

        board.fail_erase = true;
        match store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")) {
            Ok(Stored::Failed { error, replaced: true }) => assert!(error.contains("erasing"), "{error}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(stored_record(&mut board, &place).expect("a network").ssid(), b"Cafe");
    }

    /// `clear` erases the stored network and says which it was; with none stored, it says so.
    #[test]
    fn clear_erases_the_stored_network_and_says_which_it_was() {
        let place = pico_place();
        let mut board = Board::new();
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));
        assert_eq!(
            clear_network(&mut board, &place),
            Ok(Cleared { had: Had::Network(String::from("Home Net")), erased: true, error: None })
        );
        assert!(board.bytes.iter().all(|&byte| byte == 0xFF), "both slots erased");
        assert!(stored_record(&mut board, &place).is_none());
        assert_eq!(clear_network(&mut board, &place), Ok(Cleared { had: Had::Nothing, erased: true, error: None }));
    }

    /// **`show` PRINTS THE NAME AND SETTINGS AND NEVER THE PASSPHRASE**, and an open network as one.
    #[test]
    fn show_prints_the_name_and_settings_and_never_the_passphrase() {
        let board = "raspberrypi-pico-2-w";
        let text = describe(board, Some(&secured(b"Home Net", b"hunter2hunter2")));
        assert!(text.starts_with("raspberrypi-pico-2-w stores the network \"Home Net\"."), "{text}");
        assert!(text.contains("WPA2 or WPA3"), "{text}");
        assert!(text.contains("joins in the background"), "{text}");
        assert!(!text.contains("hunter2"), "never the passphrase: {text}");
        let open = Record::new(b"Cafe", b"", security::OPEN, Reconnection::Manual, BootConnection::OnConnect).expect("open");
        let text = describe(board, Some(&open));
        assert!(text.contains("open, with no passphrase") && text.contains("when the program asks"), "{text}");
        let wpa3 = Record::new(b"Lab", b"short", security::WPA3, Reconnection::Automatic, BootConnection::Background).expect("a network");
        assert!(describe(board, Some(&wpa3)).contains("Security:               WPA3\n"), "WPA3 alone is named");
        assert_eq!(describe(board, None), "raspberrypi-pico-2-w stores no network.");
        let odd = Record::new(b"a\"b\x07", b"", security::OPEN, Reconnection::Automatic, BootConnection::Background).expect("open");
        assert!(describe(board, Some(&odd)).contains("\"a\\\"b\\u{7}\""), "a name is shown escaped");
    }

    /// The passphrase from standard input is its first line, its line ending removed and nothing else:
    /// a passphrase may end in a space.
    #[test]
    fn the_passphrase_from_standard_input_is_its_first_line_without_its_line_ending() {
        let read = |text: &str| passphrase_line(&mut text.as_bytes()).map(|secret| secret.0.clone());
        assert_eq!(read("hunter2hunter2\r\nthe next line"), Ok(b"hunter2hunter2".to_vec()));
        assert_eq!(read("hunter2hunter2\n"), Ok(b"hunter2hunter2".to_vec()));
        assert_eq!(read("ends in a space \n"), Ok(b"ends in a space ".to_vec()));
        assert_eq!(read("no line ending"), Ok(b"no line ending".to_vec()));
        for empty in ["", "\n", "\r\n"] {
            let refusal = read(empty).expect_err("refused");
            assert!(refusal.contains("no passphrase"), "{refusal}");
        }
    }

    /// **`set` TAKES THE START SETTING**, named for the line `show` prints, a word from a fixed set;
    /// left out, it keeps what is stored, and any other word is refused by naming the ones taken.
    #[test]
    fn set_takes_the_start_setting_by_shows_words() {
        let request = parse_set(&words(&["Home Net", "--device", PROBE, "--at-start", "on-request"])).expect("parses");
        assert_eq!(request.settings, Settings { at_start: Some(BootConnection::OnConnect) });
        let request = parse_set(&words(&["Home Net", "--at-start", "join", "--device", PROBE])).expect("parses");
        assert_eq!(request.settings, Settings { at_start: Some(BootConnection::Background) });
        assert_eq!(parse_set(&words(&["Home Net", "--device", PROBE])).expect("parses").settings, KEEP, "left out, it keeps what is stored");
        let refusal = parse_set(&words(&["Home Net", "--device", PROBE, "--at-start", "wait"])).expect_err("refused");
        assert!(refusal.contains("--at-start takes join or on-request, not \"wait\""), "{refusal}");
        for option in ["--at-start join|on-request", "lamella wifi set --device <device> --at-start join|on-request"] {
            assert!(WIFI_USAGE.contains(option), "the usage offers {option}");
        }
    }

    /// **`set` WITH NO NETWORK CHANGES THE START SETTING ALONE**: it asks for no passphrase, so it is
    /// refused with no setting to change, and beside an option that describes a network to store.
    #[test]
    fn set_with_no_network_changes_the_start_setting_alone_and_needs_it() {
        let request = parse_set(&words(&["--device", PROBE, "--at-start", "on-request"])).expect("parses");
        assert_eq!(request.network, None);
        assert_eq!(request.settings, Settings { at_start: Some(BootConnection::OnConnect) });
        let refusal = parse_set(&words(&["--device", PROBE])).expect_err("refused");
        assert!(
            refusal.contains("or --at-start to change the stored network's setting for when the board starts"),
            "{refusal}"
        );
        for describing in ["--open", "--passphrase-stdin", "--wpa3-only"] {
            let refusal = parse_set(&words(&["--device", PROBE, "--at-start", "join", describing])).expect_err("refused");
            assert!(refusal.contains(&format!("{describing} describes a network to store, and no network is named")), "{refusal}");
        }
    }

    /// The start setting given with a network is stored with it; left out, it keeps the stored
    /// network's, or, on a board that stored none, the default.
    #[test]
    fn the_start_setting_given_with_a_network_is_stored_and_left_out_is_kept() {
        let place = pico_place();
        let mut board = Board::new();
        let given = Settings { at_start: Some(BootConnection::OnConnect) };
        assert_eq!(store_network(&mut board, &place, given, secured(b"Home Net", b"hunter2hunter2")), Ok(Stored::Written));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!((shown.boot, shown.reconnection), (BootConnection::OnConnect, Reconnection::Automatic), "given, and rejoin");
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")), Ok(Stored::Written));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!(
            (shown.ssid(), shown.boot, shown.reconnection),
            (&b"Cafe"[..], BootConnection::OnConnect, Reconnection::Automatic),
            "kept from the network it replaced, and rejoin"
        );
    }

    /// **THE SETTINGS CHANGE WITHOUT THE PASSPHRASE**: the stored network is written again with its
    /// own name, passphrase and security and the settings given; the same settings write nothing; a
    /// board that stores no network, or one a newer version stored, is told so and keeps what it holds.
    #[test]
    fn the_settings_change_alone_with_the_stored_name_passphrase_and_security_kept() {
        let place = pico_place();
        let mut board = Board::new();
        let given = Settings { at_start: Some(BootConnection::OnConnect) };
        assert_eq!(change_settings(&mut board, &place, given), Ok(Changed::NoNetwork));
        assert!(board.bytes.iter().all(|&byte| byte == 0xFF), "nothing written");
        store_network(&mut board, &place, KEEP, secured(b"Home Net", b"hunter2hunter2")).expect("stored");
        assert_eq!(change_settings(&mut board, &place, given), Ok(Changed::Written(String::from("Home Net"))));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!(
            (shown.ssid(), shown.secret(), shown.security),
            (&b"Home Net"[..], &b"hunter2hunter2"[..], security::WPA2 | security::WPA3),
            "its name, passphrase and security kept"
        );
        assert_eq!((shown.boot, shown.reconnection), (BootConnection::OnConnect, Reconnection::Automatic), "the setting given");
        assert_eq!(change_settings(&mut board, &place, given), Ok(Changed::Unchanged(String::from("Home Net"))));
        let back = Settings { at_start: Some(BootConnection::Background) };
        assert_eq!(change_settings(&mut board, &place, back), Ok(Changed::Written(String::from("Home Net"))));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!((shown.boot, shown.reconnection), (BootConnection::Background, Reconnection::Automatic), "changed back");
        with_byte(&mut board, &place, 172, 2);
        let kept = board.bytes.clone();
        assert_eq!(change_settings(&mut board, &place, given), Ok(Changed::Unusable));
        assert_eq!(board.bytes, kept, "a newer version's copy is kept as it is");
    }

    /// What `set` prints after a write: the settings the board now holds, in `show`'s words.
    #[test]
    fn set_says_the_settings_the_board_now_holds_in_shows_words() {
        let place = pico_place();
        let mut board = Board::new();
        assert_eq!(settings_now(&mut board, &place), None, "no network, no settings");
        let given = Settings { at_start: Some(BootConnection::OnConnect) };
        store_network(&mut board, &place, given, secured(b"Home Net", b"hunter2hunter2")).expect("stored");
        let text = settings_now(&mut board, &place).expect("the settings");
        assert_eq!(text, "  When the board starts:  it joins when the program asks\n  When the link is lost:  it joins again by itself");
        assert!(!text.contains("hunter2"), "never the passphrase");
    }

    /// **`set` TAKES NO SETTING FOR A LOST LINK**: the board always joins the stored network again by
    /// itself when the link is lost, so `--when-lost` is refused as any option `set` does not take,
    /// with a network or without one, and the usage does not name it.
    #[test]
    fn set_refuses_when_lost_as_an_option_it_does_not_take() {
        for value in ["rejoin", "on-request"] {
            for given in [&["Home Net", "--device", PROBE, "--when-lost", value][..], &["--device", PROBE, "--when-lost", value][..]] {
                let refusal = parse_set(&words(given)).expect_err("refused");
                assert!(refusal.contains("unknown option \"--when-lost\""), "{refusal}");
            }
        }
        assert!(!WIFI_USAGE.contains("--when-lost"), "the usage does not offer --when-lost");
    }

    /// **`set` ALWAYS STORES THE NETWORK TO JOIN AGAIN BY ITSELF**: a network stored over one kept to
    /// rejoin only when its program asks is stored to rejoin by itself, with the start setting kept;
    /// and changing the start setting alone stores it to rejoin too.
    #[test]
    fn set_always_stores_the_network_to_join_again_by_itself() {
        let place = pico_place();
        let mut board = Board::new();
        let older = Record::new(b"Home Net", b"hunter2hunter2", security::WPA2 | security::WPA3, Reconnection::Manual, BootConnection::OnConnect)
            .expect("a network");
        let store_older = |board: &mut Board| {
            let mut slots = RegionStore::open(&mut *board, place.offset, place.erase_unit).expect("opens");
            assert_eq!(lamella_stored_settings::store(&mut slots, &older), RecordWrite::Written);
        };
        store_older(&mut board);
        assert_eq!(store_network(&mut board, &place, KEEP, secured(b"Cafe", b"espresso-please")), Ok(Stored::Written));
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!((shown.ssid(), shown.boot, shown.reconnection), (&b"Cafe"[..], BootConnection::OnConnect, Reconnection::Automatic));

        store_older(&mut board);
        let start = Settings { at_start: Some(BootConnection::OnConnect) };
        assert_eq!(change_settings(&mut board, &place, start), Ok(Changed::Written(String::from("Home Net"))), "the reconnection changes");
        let shown = stored_record(&mut board, &place).expect("a network");
        assert_eq!((shown.boot, shown.reconnection), (BootConnection::OnConnect, Reconnection::Automatic));
    }
}
