//! The Wi-Fi of a board carrying the CYW43439 radio on the RP2350's four gSPI lines (GPIO 23, 24, 25
//! and 29, the same on the Pico 2 W and the Pico Plus 2 W): the station brought up once, joined to
//! the network the build names, and kept in a static for every program the firmware runs. The driver,
//! its PIO engine and the station that makes them one network device are their own crates; this file
//! is only what is specific to the firmware -- the images the build provided, the build-time
//! credentials, the clock, the narration, and the static the station lives in.

extern crate alloc;

use alloc::format;
use core::sync::atomic::{AtomicBool, Ordering};

use lamella_wifi_cyw4343x::gspi::Gspi;
use lamella_wifi_cyw4343x::rp2350::PioWire;
use lamella_wifi_cyw4343x::{Clock, Credential, Micros, Outcome};
use lamella_wifi_cyw4343x_smoltcp::{Chip, Images, Station, Stop};

/// The station this firmware runs: the driver over the PIO engine, timed by the board's clock.
pub type WifiStation = Station<Chip<'static, Gspi<PioWire>>, BoardClock>;

/// The radio's images, downloaded by the build from the project that publishes them and checked
/// against their pinned hashes before this file was compiled.
const IMAGES: Images<'static> = Images {
    firmware: include_bytes!(concat!(env!("OUT_DIR"), "/wifi-images/wifi-fw.bin")),
    settings: include_bytes!(concat!(env!("OUT_DIR"), "/wifi-images/wifi-settings.img")),
    regulatory: include_bytes!(concat!(env!("OUT_DIR"), "/wifi-images/wifi-clm.blob")),
    version: env!("LAMELLA_WIFI_VERSION").as_bytes(),
};

/// The network, named at build time; no secret is ever in this tree. `LAMELLA_WIFI_SSID` alone joins
/// an open network; with `LAMELLA_WIFI_PSK` it joins by WPA2 passphrase, or by WPA3 password when
/// `LAMELLA_WIFI_SECURITY` is `wpa3`. With no SSID the radio comes up and joins nothing.
const SSID: Option<&str> = option_env!("LAMELLA_WIFI_SSID");
const SECRET: Option<&str> = option_env!("LAMELLA_WIFI_PSK");
const SECURITY: Option<&str> = option_env!("LAMELLA_WIFI_SECURITY");

/// How long the bring-up may take: about a second is usual (attach, upload with the bus tuned, the
/// control path, the radio).
const BRING_UP_US: Micros = 10_000_000;
/// How long a join is waited for. A WPA3 join takes about 1.5 s and a WPA2 one about 3 s; past this
/// the driver goes on trying in the background, and the link comes up if the network appears.
const JOIN_US: Micros = 20_000_000;

/// The board's microsecond clock: the SysTick fold every clock reading on this board goes through.
pub struct BoardClock;

impl Clock for BoardClock {
    fn now_us(&mut self) -> Micros {
        crate::systick_clock::now_us()
    }

    fn delay_us(&mut self, us: u32) {
        let end = crate::systick_clock::now_us().saturating_add(Micros::from(us));
        while crate::systick_clock::now_us() < end {}
    }
}

static mut STATION: Option<WifiStation> = None;

/// The network's credential as the build named it.
fn credential() -> Credential<'static> {
    match (SECRET, SECURITY) {
        (None, _) => Credential::Open,
        (Some(secret), Some("wpa3")) => Credential::SaePassword(secret.as_bytes()),
        (Some(secret), _) => Credential::Passphrase(secret.as_bytes()),
    }
}

/// An address as six hex pairs.
fn mac_text(address: [u8; 6]) -> alloc::string::String {
    let [a, b, c, d, e, f] = address;
    format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
}

/// One line for each outcome worth a reader's time, with the milliseconds since the step began.
fn narrate(print: &mut dyn FnMut(&str), outcome: Outcome, us: Micros) {
    let ms = us / 1000;
    let line = match outcome {
        Outcome::Attached { chip_id } => format!("wifi: chip {:04x} attached at {ms} ms", chip_id & 0xffff),
        Outcome::Uploaded { .. } => format!("wifi: firmware running at {ms} ms"),
        Outcome::Ready { version } => format!(
            "wifi: control path open, firmware {} at {ms} ms",
            core::str::from_utf8(version.as_bytes()).unwrap_or("(not text)")
        ),
        Outcome::Up { address } => format!("wifi: radio up, address {} at {ms} ms", mac_text(address)),
        Outcome::Joined { bssid } => format!("wifi: joined through {} at {ms} ms", mac_text(bssid)),
        Outcome::JoinFailed(failure) => format!("wifi: a join attempt failed at {ms} ms: {failure:?}"),
        Outcome::LinkLost(event) => format!("wifi: the link was lost at {ms} ms (event {})", event.number),
        Outcome::Refused(refusal) => format!(
            "wifi: stopped at {} (status {:#010x}) at {ms} ms",
            refusal.stage, refusal.status
        ),
        _ => return,
    };
    print(&line);
}

/// Why a step stopped short, as one line.
fn stopped(step: &str, stop: Stop) -> alloc::string::String {
    match stop {
        Stop::Rejected(request) => format!("wifi: {step} stopped: the driver did not take `{request}`"),
        Stop::Refused(refusal) => {
            format!("wifi: {step} stopped at {} (status {:#010x})", refusal.stage, refusal.status)
        }
        Stop::JoinFailed(failure) => format!("wifi: {step} failed and is not retried: {failure:?}"),
        Stop::TimedOut => format!("wifi: {step} did not finish in time; the driver keeps trying"),
    }
}

/// Bring the radio up and join the network the build names, exactly once. `clk_sys_hz` is the
/// system clock the PIO engine runs on, as the firmware read it off the clock tree. Every step is
/// narrated through `print`. Later calls return at once, whatever the first one found.
pub fn ensure_wifi(clk_sys_hz: u32, print: &mut dyn FnMut(&str)) {
    static ATTEMPTED: AtomicBool = AtomicBool::new(false);
    if ATTEMPTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let start = crate::systick_clock::now_us();
    let mut spins = 0u32;
    while crate::systick_clock::now_us() == start {
        spins += 1;
        if spins > 100_000 {
            print("wifi: the board's clock is not running, so the radio is not brought up");
            return;
        }
    }
    let station = unsafe {
        (*core::ptr::addr_of_mut!(STATION))
            .insert(Station::new(Chip::new(Gspi::new(PioWire::new(clk_sys_hz))), BoardClock))
    };
    let address = match station.bring_up(IMAGES, BRING_UP_US, &mut |o, us| narrate(print, o, us)) {
        Ok(address) => address,
        Err(stop) => {
            print(&stopped("bring-up", stop));
            return;
        }
    };
    let Some(ssid) = SSID else {
        print(&format!(
            "wifi: radio up as {}; no network was named at build time (LAMELLA_WIFI_SSID)",
            mac_text(address)
        ));
        return;
    };
    if let Err(stop) = station.join(ssid.as_bytes(), credential(), JOIN_US, &mut |o, us| narrate(print, o, us)) {
        print(&stopped("the join", stop));
    }
}

/// The station, once [`ensure_wifi`] has made it.
fn station() -> Option<&'static WifiStation> {
    unsafe { (*core::ptr::addr_of!(STATION)).as_ref() }
}

/// Whether the link is up.
pub fn wifi_up() -> bool {
    station().is_some_and(|station| station.link_up())
}

/// The chip's own address, once the radio came up.
pub fn mac() -> [u8; 6] {
    station().and_then(|station| station.address()).unwrap_or([0; 6])
}

/// The station as a program's network device. Only after [`ensure_wifi`] found the link up; the
/// firmware runs one program at a time and drops that program's stack before the next is built, so
/// at most one borrow is ever live.
pub fn device() -> &'static mut WifiStation {
    unsafe {
        (*core::ptr::addr_of_mut!(STATION))
            .as_mut()
            .expect("the station is made by ensure_wifi before a device is asked for")
    }
}
