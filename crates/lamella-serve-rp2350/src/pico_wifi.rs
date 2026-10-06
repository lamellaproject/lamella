//! The Wi-Fi of a board carrying the CYW43439 radio on the RP2350's four gSPI lines (GPIO 23, 24, 25
//! and 29, the same on the Pico 2 W and the Pico Plus 2 W): the radio brought up once, joined to the
//! network the board stores -- or, in a development build that names one, the network named at
//! build time -- and kept in a static for every program the firmware runs, as the network device a
//! program's stack drives and the Wi-Fi seam a program's classes reach. The driver, its PIO engine,
//! the station and the controller that makes them one are their own crates; this file is only what
//! is specific to the firmware -- the images the build provided, the build-time credentials, the
//! clock, the narration, the slot the driver reads a credential from, and the static it all lives in.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use core::sync::atomic::{AtomicBool, Ordering};

use lamella_net_core::wifi::{security, BootConnection, CredentialSource, Reconnection, WifiControl, WifiLink};
use lamella_wifi_cyw4343x::gspi::Gspi;
use lamella_wifi_cyw4343x::rp2350::PioWire;
use lamella_wifi_cyw4343x::{Clock, Micros, Outcome};
use lamella_wifi_cyw4343x_smoltcp::controller::{Controller, CredentialSlot};
use lamella_wifi_cyw4343x_smoltcp::record::RecordStore;
use lamella_wifi_cyw4343x_smoltcp::{Chip, Images, Station, Stop};

/// The radio this firmware runs: the driver over the PIO engine, timed by the board's clock, with
/// the board's settings for its stored network and a static slot for the credential it holds.
pub type WifiController =
    Controller<'static, Chip<'static, Gspi<PioWire>>, BoardClock, Box<dyn RecordStore>, StaticSlot>;

/// The radio's images, downloaded by the build from the project that publishes them and checked
/// against their pinned hashes before this file was compiled.
const IMAGES: Images<'static> = Images {
    firmware: include_bytes!(concat!(env!("OUT_DIR"), "/wifi-images/wifi-fw.bin")),
    settings: include_bytes!(concat!(env!("OUT_DIR"), "/wifi-images/wifi-settings.img")),
    regulatory: include_bytes!(concat!(env!("OUT_DIR"), "/wifi-images/wifi-clm.blob")),
    version: env!("LAMELLA_WIFI_VERSION").as_bytes(),
};

/// A network named at build time, which only a development build carries; no secret is ever in this
/// tree. `LAMELLA_WIFI_SSID` alone names an open network; with `LAMELLA_WIFI_PSK` it is a WPA2
/// passphrase, or a WPA3 password when `LAMELLA_WIFI_SECURITY` is `wpa3`. The board joins it at
/// start only when it stores no network of its own.
const SSID: Option<&str> = option_env!("LAMELLA_WIFI_SSID");
const SECRET: Option<&str> = option_env!("LAMELLA_WIFI_PSK");
const SECURITY: Option<&str> = option_env!("LAMELLA_WIFI_SECURITY");

/// How long the bring-up may take: about a second is usual (attach, upload with the bus tuned, the
/// control path, the radio).
const BRING_UP_US: Micros = 10_000_000;
/// How long the firmware's answer on its features is waited for after the bring-up.
const CAPABILITIES_US: Micros = 1_000_000;
/// How long a join made before the program starts is waited for. A WPA3 join takes about 1.5 s and a
/// WPA2 one about 3 s; past this the radio goes on trying in the background, and the link comes up if
/// the network appears.
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

/// The longest name and secret the slot holds.
const SLOT_SSID: usize = 32;
const SLOT_SECRET: usize = 128;

/// The bytes the driver reads the held network's name and secret from, while it holds it.
static mut SLOT_BYTES: [u8; SLOT_SSID + SLOT_SECRET] = [0; SLOT_SSID + SLOT_SECRET];

/// The slot the controller parks a join's credential in: one static buffer, overwritten with zeros
/// as soon as the driver lets the network go.
pub struct StaticSlot;

impl CredentialSlot<'static> for StaticSlot {
    fn park(&mut self, ssid: &[u8], secret: &[u8]) -> (&'static [u8], &'static [u8]) {
        let ssid_len = ssid.len().min(SLOT_SSID);
        let secret_len = secret.len().min(SLOT_SECRET);
        unsafe {
            let base = core::ptr::addr_of_mut!(SLOT_BYTES).cast::<u8>();
            core::ptr::write_bytes(base, 0, SLOT_SSID + SLOT_SECRET);
            core::ptr::copy_nonoverlapping(ssid.as_ptr(), base, ssid_len);
            core::ptr::copy_nonoverlapping(secret.as_ptr(), base.add(SLOT_SSID), secret_len);
            (
                core::slice::from_raw_parts(base, ssid_len),
                core::slice::from_raw_parts(base.add(SLOT_SSID), secret_len),
            )
        }
    }

    fn wipe(&mut self) {
        unsafe {
            let base = core::ptr::addr_of_mut!(SLOT_BYTES).cast::<u8>();
            for at in 0..SLOT_SSID + SLOT_SECRET {
                core::ptr::write_volatile(base.add(at), 0);
            }
        }
    }
}

static mut WIFI: Option<WifiController> = None;

/// The network named at build time: its secret and the kinds a join of it accepts.
fn build_time_network() -> (&'static [u8], u8) {
    match (SECRET, SECURITY) {
        (None, _) => (&[], security::OPEN),
        (Some(secret), Some("wpa3")) => (secret.as_bytes(), security::WPA3),
        (Some(secret), _) => (secret.as_bytes(), security::WPA2),
    }
}

/// An address as six hex pairs.
fn mac_text(address: [u8; 6]) -> alloc::string::String {
    let [a, b, c, d, e, f] = address;
    format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
}

/// One line for each bring-up outcome worth a reader's time, with the milliseconds since the step
/// began.
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
        Stop::TimedOut => format!("wifi: {step} did not finish in time"),
    }
}

/// Bring the radio up and join the network the board stores, as its boot setting says -- or, with
/// none stored, the network a development build names -- exactly once. `clk_sys_hz` is the system
/// clock the PIO engine runs on, as the firmware read it off the clock tree; `records` is the
/// board's storage for its network. Every step is narrated through `print`. Later calls return at
/// once, whatever the first one found.
pub fn ensure_wifi(clk_sys_hz: u32, records: Box<dyn RecordStore>, print: &mut dyn FnMut(&str)) {
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
    let wifi = unsafe {
        (*core::ptr::addr_of_mut!(WIFI)).insert(Controller::new(
            Station::new(Chip::new(Gspi::new(PioWire::new(clk_sys_hz))), BoardClock),
            records,
            StaticSlot,
        ))
    };
    let station = wifi.station_mut();
    let address = match station.bring_up(IMAGES, BRING_UP_US, &mut |o, us| narrate(print, o, us)) {
        Ok(address) => address,
        Err(stop) => {
            print(&stopped("bring-up", stop));
            return;
        }
    };
    if station.start_capabilities() {
        let deadline = crate::systick_clock::now_us().saturating_add(CAPABILITIES_US);
        while station.journal().capabilities.is_none() && crate::systick_clock::now_us() < deadline {
            station.service();
        }
    }
    let wpa3 = match wifi.radio().wpa3 {
        Some(true) => "runs WPA3",
        Some(false) => "does not run WPA3",
        None => "did not say whether it runs WPA3",
    };
    print(&format!("wifi: radio up as {}; its firmware {wpa3}", mac_text(address)));
    boot_join(wifi, print);
}

/// The join the board makes before its first program: the stored network as its boot setting says,
/// or the network a development build names when none is stored.
fn boot_join(wifi: &mut WifiController, print: &mut dyn FnMut(&str)) {
    let (join, wait) = match wifi.stored_boot() {
        Some(BootConnection::OnConnect) => {
            print("wifi: a network is stored, set to join when a program asks");
            return;
        }
        Some(boot) => {
            let Some(join) = wifi.begin_stored() else { return };
            let background = boot == BootConnection::Background;
            print(if background {
                "wifi: joining the stored network in the background"
            } else {
                "wifi: joining the stored network before the program starts"
            });
            (join, !background)
        }
        None => {
            let Some(ssid) = SSID else {
                print("wifi: no network is stored and none was named at build time (LAMELLA_WIFI_SSID)");
                return;
            };
            let (secret, kinds) = build_time_network();
            print("wifi: joining the network named at build time");
            (
                wifi.begin(ssid.as_bytes(), secret, kinds, Reconnection::Automatic, CredentialSource::BuildTime),
                true,
            )
        }
    };
    if wait {
        await_join(wifi, join, print);
    }
}

/// Pumps the radio until join `join` ends or [`JOIN_US`] passes, narrating each failed attempt and
/// the end.
fn await_join(wifi: &mut WifiController, join: u32, print: &mut dyn FnMut(&str)) {
    let start = crate::systick_clock::now_us();
    let mut reported: Option<alloc::string::String> = None;
    loop {
        wifi.service();
        let state = wifi.state();
        let ms = crate::systick_clock::now_us().saturating_sub(start) / 1000;
        if let Some(failure) = &state.last_failure {
            if reported.as_ref() != Some(&failure.detail) {
                print(&format!("wifi: a join attempt failed at {ms} ms: {}", failure.detail));
                reported = Some(failure.detail.clone());
            }
        }
        if state.join == join && state.join_outcome.is_some() {
            if state.link == WifiLink::Connected {
                let through = state.bssid.map_or(alloc::string::String::from("an access point"), mac_text);
                print(&format!("wifi: joined through {through} at {ms} ms"));
            }
            return;
        }
        if ms * 1000 >= JOIN_US {
            print("wifi: the join did not finish in time; the radio keeps trying in the background");
            return;
        }
    }
}

/// The radio, once [`ensure_wifi`] has made it.
fn controller() -> Option<&'static WifiController> {
    unsafe { (*core::ptr::addr_of!(WIFI)).as_ref() }
}

/// Whether a radio is running, whatever its link.
pub fn radio_up() -> bool {
    controller().is_some_and(|wifi| wifi.station().address().is_some() && wifi.station().refusal().is_none())
}

/// Whether the link is up.
pub fn wifi_up() -> bool {
    controller().is_some_and(|wifi| wifi.link() == WifiLink::Connected)
}

/// The chip's own address, once the radio came up.
pub fn mac() -> [u8; 6] {
    controller().and_then(|wifi| wifi.station().address()).unwrap_or([0; 6])
}

/// The radio as a program's network device. Only after [`radio_up`]; the firmware runs one program
/// at a time and drops that program's stack before the next is built, so at most one borrow is ever
/// live.
pub fn device() -> &'static mut WifiController {
    unsafe {
        (*core::ptr::addr_of_mut!(WIFI))
            .as_mut()
            .expect("the radio is made by ensure_wifi before a device is asked for")
    }
}

/// The Wi-Fi seam inside the device a program's stack holds, for its backend to lend.
pub fn view<'a>(device: &'a mut &'static mut WifiController) -> &'a mut dyn WifiControl {
    &mut **device
}
