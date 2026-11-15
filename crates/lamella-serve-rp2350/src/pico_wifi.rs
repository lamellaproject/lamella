//! The Wi-Fi of a board carrying the CYW43439 radio on the RP2350's four gSPI lines (GPIO 23, 24, 25
//! and 29, the same on the Pico 2 W, the Pico Plus 2 W and the Pico LiPo 2 XL W): the radio's
//! controller, made once and kept in a static for every program the firmware runs, as the network
//! device a program's stack drives and the Wi-Fi seam a program's classes reach. The radio stays off
//! until something asks for it: the board's start, when the network it stores -- or, in a
//! development build that names one, the network named at build time -- joins in the background, or
//! a program's join. Nothing waits for it: the program starts at once, and the passes of its stack
//! bring the radio up and join beside it. The driver, its PIO engine, the station and the controller
//! that makes them one are their own crates; this file is only what is specific to the firmware --
//! the images the build provided, the build-time credentials, the clock, the random seed, the slot
//! the driver reads a credential from, and the static it all lives in.

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, Ordering};

use lamella_net_core::wifi::{security, BootConnection, CredentialSource, Reconnection, WifiControl};
use lamella_wifi_cyw4343x::gspi::Gspi;
use lamella_wifi_cyw4343x::rp2350::PioWire;
use lamella_wifi_cyw4343x::{Clock, Micros};
use lamella_wifi_cyw4343x_smoltcp::controller::{Controller, CredentialSlot};
use lamella_wifi_cyw4343x_smoltcp::record::RecordStore;
use lamella_wifi_cyw4343x_smoltcp::{Chip, Images, Station};

#[path = "rp2350_wifi_seed.rs"]
mod seed;

/// The hardware generator's registers, reached only when the radio starts.
struct Part;

impl crate::rp2350_trng::Registers for Part {
    fn read(&mut self, address: usize) -> u32 {
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }

    fn write(&mut self, address: usize, value: u32) {
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }
}

fn hardware_seed() -> Option<u32> {
    seed::read(&mut Part, &mut crate::systick_clock::now_ms)
}

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

/// The hardware address a stack is made with before the radio has read its own: locally
/// administered and unicast. The stack takes the radio's address before it sends a frame.
const PLACEHOLDER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0];

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

/// Make the radio's controller and start what the board's stored network asks for at start, exactly
/// once, waiting for nothing: a network that joins in the background powers the radio here, and the
/// passes of the program's stack bring it up and join beside the program; any other start leaves the
/// radio off until a program joins a network. `clk_sys_hz` is the system clock the PIO engine runs
/// on, as the firmware read it off the clock tree; `records` is the board's storage for its network.
/// State is available through the controller; setup writes no console output. Later calls return
/// at once, whatever the first one found.
pub fn start_wifi(clk_sys_hz: u32, records: Box<dyn RecordStore>) {
    static ATTEMPTED: AtomicBool = AtomicBool::new(false);
    if ATTEMPTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let start = crate::systick_clock::now_us();
    let mut spins = 0u32;
    while crate::systick_clock::now_us() == start {
        spins += 1;
        if spins > 100_000 {
            return;
        }
    }
    let wifi = unsafe {
        (*core::ptr::addr_of_mut!(WIFI)).insert(
            Controller::new(
                Station::new(Chip::new(Gspi::new(PioWire::new(clk_sys_hz))), BoardClock),
                records,
                StaticSlot,
            )
            .with_images(IMAGES, BRING_UP_US),
        )
    };
    wifi.station_mut().seed_with(hardware_seed);
    boot_join(wifi);
}

/// The join the board starts with its first program: the stored network as its boot setting says,
/// or the network a development build names when none is stored. It powers the radio; nothing waits
/// for it. Any other start leaves the radio off. A firmware that keeps no board settings reads no
/// stored network, whatever the board holds.
fn boot_join(wifi: &mut WifiController) {
    match wifi.stored_boot() {
        Some(BootConnection::OnConnect) => {}
        Some(_) => {
            wifi.begin_stored();
        }
        None if wifi.stored_unusable() => {
        }
        None => {
            let Some(ssid) = SSID else {
                return;
            };
            let (secret, kinds) = build_time_network();
            wifi.begin(ssid.as_bytes(), secret, kinds, Reconnection::Automatic, CredentialSource::BuildTime);
        }
    }
}

/// The radio's controller, once [`start_wifi`] has made it.
fn controller() -> Option<&'static WifiController> {
    unsafe { (*core::ptr::addr_of!(WIFI)).as_ref() }
}

/// Whether [`start_wifi`] made the radio's controller, whatever the radio is doing: off, starting,
/// up or failed. A program's stack is built over it whenever it exists.
pub fn fitted() -> bool {
    controller().is_some()
}

/// The address a program's stack is made with: the chip's own once the radio has read it, else a
/// placeholder the stack replaces with the chip's before it sends a frame.
pub fn mac() -> [u8; 6] {
    controller().and_then(|wifi| wifi.station().address()).unwrap_or(PLACEHOLDER_MAC)
}

/// The chip's own address, once the radio has read it.
pub fn chip_address() -> Option<[u8; 6]> {
    controller().and_then(|wifi| wifi.station().address())
}

/// The radio as a program's network device. Only once [`fitted`]; the firmware runs one program at a
/// time and drops that program's stack before the next is built, so at most one borrow is ever live.
pub fn device() -> &'static mut WifiController {
    unsafe {
        (*core::ptr::addr_of_mut!(WIFI))
            .as_mut()
            .expect("the radio's controller is made by start_wifi before a device is asked for")
    }
}

/// The Wi-Fi seam inside the device a program's stack holds, for its backend to lend.
pub fn view<'a>(device: &'a mut &'static mut WifiController) -> &'a mut dyn WifiControl {
    &mut **device
}
