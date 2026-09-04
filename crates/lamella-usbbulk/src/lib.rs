//! Cross-platform USB *bulk* transport for CMSIS-DAP v2 debug probes -- the v2 sibling of
//! `lamella-usbhid`. Same small shape (enumerate, open by vendor/product id, exchange packets),
//! but over a vendor-specific interface's bulk IN/OUT pipes instead of HID reports, so there is no
//! report id or padding. Implemented directly against each OS's native USB API -- WinUSB + SetupAPI on
//! Windows, IOKit IOUSBLib on macOS, sysfs + usbfs on Linux -- with no external USB crates. Enumeration,
//! open-by-VID/PID, serial/product strings, and bulk I/O are supported on all three.
#![allow(unsafe_code)]

use std::time::Duration;

/// An error enumerating, opening, or exchanging packets with a bulk USB device.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No connected device matched the request.
    NotFound,
    /// The operating system's USB layer failed; carries a description.
    Os(String),
    /// A read returned no packet within the timeout.
    Timeout,
    /// This operating system's backend is not implemented yet.
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no matching USB bulk device"),
            Error::Os(msg) => write!(f, "USB error: {msg}"),
            Error::Timeout => write!(f, "USB bulk transfer timed out"),
            Error::Unsupported => write!(f, "USB bulk backend not implemented on this platform"),
        }
    }
}
impl std::error::Error for Error {}

/// A USB bulk operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// Whether a device's reported identity satisfies a requested serial.
///
/// A SUBSTRING test, and the direction is the whole point: `wanted` must appear in `actual`, never
/// the reverse. Windows names a device by an instance id that CONTAINS its serial among other
/// fields, so an equality test would reject the very device asked for -- while the reverse test
/// would accept a board whose serial is merely a prefix of the one asked for, which is a different
/// board. Case-insensitive because the same serial reaches us in either case depending on which
/// layer reported it.
pub(crate) fn serial_matches(wanted: &str, actual: &str) -> bool {
    actual.to_ascii_uppercase().contains(&wanted.to_ascii_uppercase())
}

/// Whether one candidate device is eligible for this open -- the single decision every backend
/// makes, so that they make it the same way.
///
/// `reported` NONE with a serial requested is a NO: a device that does not say who it is cannot be
/// the device you named, and a missing string is not a wildcard.
///
/// `wanted` NONE is a YES for everything. Choosing among several unnamed candidates belongs to the
/// layer that knows what the caller asked for, which is where an ambiguous unnamed open is refused.
///
pub(crate) fn candidate_satisfies(wanted: Option<&str>, reported: Option<&str>) -> bool {
    match wanted {
        None => true,
        Some(wanted) => reported.is_some_and(|actual| serial_matches(wanted, actual)),
    }
}

/// Picks the first eligible candidate, or REFUSES -- never falls back to a device that was not asked
/// for.
///
/// The refusal is the contract: a request that names no attached device fails, rather than returning
/// one that was not asked for.
///
/// A backend whose candidates are not values -- a live handle that must be released, or a device that
/// will not report its serial until it is opened -- calls [`candidate_satisfies`] in its own loop
/// instead and fails closed by running out of candidates.
///
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn select_requested<T>(
    candidates: impl IntoIterator<Item = T>,
    wanted: Option<&str>,
    serial_of: impl Fn(&T) -> Option<&str>,
) -> Result<T> {
    candidates
        .into_iter()
        .find(|c| candidate_satisfies(wanted, serial_of(c)))
        .ok_or(Error::NotFound)
}

/// A bulk USB device discovered by [`enumerate`].
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number string, if the OS reported one.
    pub serial_number: Option<String>,
    /// Product string, if the OS reported one.
    pub product: Option<String>,
    /// The vendor-class INTERFACE's own name (`iInterface`), where the OS publishes it.
    ///
    /// **THIS IS WHAT LETS A CALLER RECOGNIZE A PROBE BY THE SPECIFICATION RATHER THAN BY ITS
    /// VENDOR.** CMSIS-DAP v2 requires this string to contain `CMSIS-DAP`, which is how a debug
    /// interface is told apart from any other vendor-class interface -- a phone, a radio dongle, a
    /// Lamella Link board -- without a hand-kept list of vendor ids that goes stale the first time
    /// somebody ships a probe nobody wrote down.
    ///
    /// Every backend reads it from the OS's own device tree, so it costs no handle: the interface
    /// node's bus-reported name on Windows, `kUSBString` on macOS, `.../interface` in sysfs on
    /// Linux. `None` means the OS did not publish one, which is not the same as "not a probe" --
    /// see `lamella-probe`, where that distinction is made.
    pub interface_name: Option<String>,
}

/// Lists every connected vendor-bulk device -- a CMSIS-DAP v2 probe OR e.g. a Lamella Link board (both
/// expose a vendor-specific class-0xFF interface with bulk IN/OUT) -- with its ids and, where the OS reports
/// them, serial/product strings. No VID filter: a caller keeps the vendor id(s) it wants (a probe consumer
/// filters to probe vendors; the Lamella Link picker keeps its own VID). Cross-platform (Windows/macOS/Linux).
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    imp::enumerate()
}

/// Lists the devices registered under a caller-supplied WinUSB device-interface GUID (a
/// `"{...}"` string) -- e.g. every attached Lamella Link board -- with product and serial
/// strings where the OS can report them. Windows-only for now ([`Error::Unsupported`]
/// elsewhere): macOS and Linux have no interface-GUID registry, and their backends match by
/// VID/PID at open time instead.
pub fn enumerate_interface(interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    imp::enumerate_guid(interface_guid)
}

/// Whether a device is reachable, and if not, WHY -- the fact a caller needs to tell a user
/// something actionable instead of "not found".
///
/// The distinction is not pedantry. A probe whose vendor ships no MS-OS descriptors (an ST-Link,
/// for one) enumerates perfectly on the USB bus while its debug interface has NO driver bound, so
/// it cannot be opened. "Absent" and "present but unbound" then look identical through a plain
/// open -- both just fail -- yet the remedies could not be more different: plug the thing in
/// versus install a driver. Reporting the wrong one sends a user hunting the wrong problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// Nothing with that vendor/product id is on the USB bus.
    Absent,
    /// The device IS on the bus, but no interface is registered under the requested GUID -- on
    /// Windows, its interface has no (or the wrong) driver bound. This is the case worth a loud,
    /// specific message: the hardware is fine and one install away from working.
    PresentUnbound,
    /// An interface is registered under the requested GUID; the device should be openable. An open
    /// that still fails from here means something else holds it -- another debugger, typically.
    Bound,
}

/// Classifies why a device can or cannot be reached, so a failure can name its own remedy.
///
/// Deliberately reports the FACT and not the advice: what to tell the user ("install ST's driver",
/// "run pnputil") is vendor-specific and belongs to the crate that knows which probe it wanted,
/// not to a general USB layer. See [`Binding`].
///
/// Platform note: the unbound state is a Windows driver-model concept. macOS and Linux open the
/// device directly, so this reports [`Binding::Bound`] whenever the device is visible; the
/// analogous failure there is permissions (a missing udev rule), which surfaces as an open error.
pub fn diagnose(interface_guid: &str, vendor_id: u16, product_id: u16) -> Result<Binding> {
    imp::diagnose(interface_guid, vendor_id, product_id)
}

/// An open bulk USB device that exchanges raw packets with a CMSIS-DAP v2 probe.
pub struct Device(imp::Device);

impl Device {
    /// Opens the first connected device with `vendor_id` and `product_id` (optionally a specific
    /// `serial`) that exposes a CMSIS-DAP v2 vendor interface -- a bulk IN + bulk OUT pipe.
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        imp::Device::open(vendor_id, product_id, serial).map(Device)
    }

    /// Opens a WinUSB device that registered under a caller-supplied device-interface GUID (a
    /// `"{...}"` string) -- e.g. the Lamella Link carrier rather than a CMSIS-DAP v2 probe. On
    /// Windows the device is found by that interface GUID; macOS and Linux match by VID/PID + the
    /// vendor-specific (class 0xFF) interface, so there the GUID is ignored.
    pub fn open_interface(
        interface_guid: &str,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> Result<Self> {
        imp::Device::open_guid(interface_guid, vendor_id, product_id, serial).map(Device)
    }

    /// The bulk endpoint addresses negotiated at open time, as `(in, out)`.
    ///
    /// Exposed because probing endpoints blindly is not a viable diagnostic: reading an endpoint a
    /// device does not have can block rather than fail, so a tool that needs to know which pipes
    /// exist must ask instead of sweep.
    pub fn endpoints(&self) -> (u8, u8) {
        self.0.endpoints()
    }

    /// Clears any stall on both pipes, so one failed transfer does not contaminate the next.
    pub fn reset_pipes(&mut self) {
        self.0.reset_pipes();
    }

    /// Clears one named endpoint -- DIAGNOSTIC ONLY, and NOT harmless on every device.
    ///
    /// [`reset_pipes`](Self::reset_pipes) only ever touches the two command pipes, so when exactly
    /// those two misbehave there is no way to tell a reset that BROKE them from a reset that merely
    /// failed to fix them. Resetting a third, known-working pipe distinguishes the two.
    ///
    /// A reset is CLEAR_FEATURE(ENDPOINT_HALT) on the wire, and an ST-Link/V3 acknowledges it and
    /// then stops answering that endpoint altogether -- so use this where a pipe is known to be
    /// halted, not as a precaution. No-op where the backend has no pipe reset.
    pub fn reset_endpoint(&mut self, endpoint: u8) {
        self.0.reset_endpoint(endpoint);
    }

    /// A human-readable dump of the interface and its pipes -- DIAGNOSTIC ONLY, and the format is
    /// not stable.
    ///
    /// When a device opens cleanly but carries no traffic, the next question is what we are
    /// actually attached to -- the right interface, the right alternate setting, pipes of the
    /// expected type and size. Inferring that from a failing transfer is guesswork; read the
    /// descriptor instead. Currently detailed on Windows; elsewhere it reports the endpoints.
    pub fn describe_interface(&self) -> String {
        self.0.describe_interface()
    }

    /// Sends one bulk OUT packet (raw -- no report id or padding) on the primary (lowest-address) OUT
    /// endpoint.
    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_packet(data)
    }

    /// Reads one bulk IN packet into `buf` from the primary IN endpoint, returning its length (or
    /// [`Error::Timeout`]).
    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_packet(buf, timeout)
    }

    /// Sends one bulk OUT packet on a specific endpoint address. A single-pair probe uses only its
    /// primary endpoint ([`write_packet`](Self::write_packet)); a device with more than one bulk pair --
    /// e.g. a WCH-Link, whose command pair is `0x01`/`0x81` and whose flash-stream data pair is
    /// `0x02`/`0x82` -- reaches the others here.
    pub fn write_endpoint(&mut self, endpoint: u8, data: &[u8]) -> Result<()> {
        self.0.write_endpoint(endpoint, data)
    }

    /// Reads one bulk IN packet from a specific endpoint address into `buf`, returning its length (or
    /// [`Error::Timeout`]) -- the companion to [`write_endpoint`](Self::write_endpoint).
    pub fn read_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_endpoint(endpoint, buf, timeout)
    }
}

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod imp;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("lamella-usbbulk supports macOS, Linux, and Windows");

#[cfg(test)]
mod tests {
    use super::{Error, Result, candidate_satisfies, select_requested, serial_matches};


    /// Candidates are (serial, tag); the tag stands for whatever the platform hands back.
    fn pick<'a>(candidates: &[(Option<&'a str>, &'a str)], wanted: Option<&str>) -> Result<&'a str> {
        select_requested(candidates.iter().copied(), wanted, |(serial, _)| *serial)
            .map(|(_, tag)| tag)
    }

    #[test]
    fn a_serial_matching_nothing_refuses_rather_than_taking_another_device() {
        let bench = [(Some("AAAA1111"), "board-a"), (Some("BBBB2222"), "board-b")];
        assert!(matches!(pick(&bench, Some("CCCC3333")), Err(Error::NotFound)));
    }

    #[test]
    /// The ordinary case, and it must not depend on position: either board is reachable by name.
    fn a_serial_picks_its_own_board_from_several_of_one_vendor_and_product() {
        let bench = [
            (Some("AAAA1111"), "board-a"),
            (Some("BBBB2222"), "board-b"),
            (Some("CCCC3333"), "board-c"),
        ];
        assert_eq!(pick(&bench, Some("BBBB2222")).unwrap(), "board-b");
        assert_eq!(pick(&bench, Some("CCCC3333")).unwrap(), "board-c");
        assert_eq!(pick(&bench, Some("AAAA1111")).unwrap(), "board-a");
    }

    #[test]
    /// No serial requested keeps taking the first candidate. Refusing here would break every
    /// single-probe caller, and deciding between unnamed candidates belongs to the layer that knows
    /// what was asked for.
    fn no_serial_requested_takes_the_first_candidate() {
        let bench = [(Some("AAAA1111"), "board-a"), (Some("BBBB2222"), "board-b")];
        assert_eq!(pick(&bench, None).unwrap(), "board-a");
        assert!(matches!(pick(&[], None), Err(Error::NotFound)));
    }

    #[test]
    /// A device reporting no serial cannot satisfy a request for one -- it must be skipped rather
    /// than treated as a wildcard. It stays eligible when nothing was asked for.
    fn a_device_with_no_serial_satisfies_no_request_but_is_still_openable_unnamed() {
        let bench = [(None, "unnamed"), (Some("AAAA1111"), "board-a")];
        assert_eq!(pick(&bench, Some("AAAA1111")).unwrap(), "board-a");
        assert!(matches!(pick(&[(None, "unnamed")], Some("AAAA1111")), Err(Error::NotFound)));
        assert_eq!(pick(&bench, None).unwrap(), "unnamed");
    }

    #[test]
    fn the_substring_match_runs_one_way_only() {
        assert!(serial_matches("EEEE5555FFFF6666", "USB\\VID_39E9&PID_0001\\EEEE5555FFFF6666"));
        assert!(serial_matches("eeee5555ffff6666", "USB\\VID_39E9&PID_0001\\EEEE5555FFFF6666"));
        assert!(!serial_matches("EEEE5555FFFF6666", "EEEE5555"));
        assert!(!serial_matches("AAAA1111", "BBBB2222"));
    }

    #[test]
    fn the_candidate_decision_stands_alone_for_a_backend_that_cannot_hand_over_values() {
        assert!(candidate_satisfies(None, None));
        assert!(candidate_satisfies(None, Some("AAAA1111")));
        assert!(candidate_satisfies(Some("AAAA1111"), Some("AAAA1111")));
        assert!(!candidate_satisfies(Some("AAAA1111"), Some("BBBB2222")));
        assert!(!candidate_satisfies(Some("AAAA1111"), None));
    }

    #[test]
    /// Many boards sharing one vendor and product id, with the requested one enumerating last. A
    /// first-match search answers `board-a` for every one of these.
    fn the_requested_board_is_found_however_late_it_enumerates() {
        let bench: Vec<(Option<&str>, &str)> = (0..9)
            .map(|i| match i {
                8 => (Some("9999AAAA8888BBBB"), "the-one-asked-for"),
                _ => (Some("0000000000000000"), "another-lane's-board"),
            })
            .collect();
        assert_eq!(pick(&bench, Some("9999AAAA8888BBBB")).unwrap(), "the-one-asked-for");
    }
}
