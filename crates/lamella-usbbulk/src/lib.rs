//! Cross-platform USB *bulk* transport for CMSIS-DAP v2 debug probes -- the v2 sibling of
//! `lamella-usbhid`. Same small shape (enumerate, open by vendor/product id, exchange packets),
//! but over a vendor-specific interface's bulk IN/OUT pipes instead of HID reports, so there is no
//! report id or padding. Implemented directly against each OS's native USB API -- WinUSB + SetupAPI on
//! Windows, IOKit IOUSBLib on macOS, sysfs + usbfs on Linux -- with no external USB crates. Enumeration,
//! open-by-VID/PID, serial/product strings, bulk I/O, and a bounded control transfer to an interface of
//! any class are supported on all three.
#![allow(unsafe_code)]

use std::time::Duration;

#[cfg(any(target_os = "linux", test))]
mod open_error;

/// An error enumerating, opening, or exchanging data with a USB device.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No connected device matched the request.
    NotFound,
    /// The operating system's USB layer failed; carries a description.
    Os(String),
    /// A transfer, a read, a write or a control transfer, did not complete within its timeout.
    Timeout,
    /// This operating system's backend is not implemented yet.
    Unsupported,
    /// The request cannot be sent as asked, and nothing was sent; carries why.
    InvalidRequest(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no matching USB device"),
            Error::Os(msg) => write!(f, "USB error: {msg}"),
            Error::Timeout => write!(f, "USB transfer timed out"),
            Error::Unsupported => write!(f, "USB backend not implemented on this platform"),
            Error::InvalidRequest(why) => write!(f, "USB request not sent: {why}"),
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

/// Whether a device's reported serial is `wanted` whole, without regard to case -- the rule a
/// [`ControlInterface`] is opened by.
///
/// It differs from [`candidate_satisfies`] in one thing: a device whose serial only CONTAINS the one
/// asked for is not the device asked for. `wanted` NONE is a YES for everything, and `reported` NONE
/// with a serial requested is a NO, as there.
///
pub(crate) fn serial_is(wanted: Option<&str>, reported: Option<&str>) -> bool {
    match wanted {
        None => true,
        Some(wanted) => reported.is_some_and(|actual| actual.eq_ignore_ascii_case(wanted)),
    }
}

/// Picks the first candidate whose serial is `wanted` whole ([`serial_is`]), or REFUSES.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn select_by_whole_serial<T>(
    candidates: impl IntoIterator<Item = T>,
    wanted: Option<&str>,
    serial_of: impl Fn(&T) -> Option<&str>,
) -> Result<T> {
    candidates.into_iter().find(|c| serial_is(wanted, serial_of(c))).ok_or(Error::NotFound)
}

/// Moves into `buf` as much of `left` as fits, in order, and returns how much.
pub(crate) fn take_left(left: &mut Vec<u8>, buf: &mut [u8]) -> usize {
    let count = left.len().min(buf.len());
    buf[..count].copy_from_slice(&left[..count]);
    left.drain(..count);
    count
}

/// What a finished read delivered, handed to the read collecting it: into `buf`, and what `buf` has no
/// room for into `left`, for the next read. Returns how much went into `buf`.
pub(crate) fn hand_over(delivered: &[u8], buf: &mut [u8], left: &mut Vec<u8>) -> usize {
    let count = delivered.len().min(buf.len());
    buf[..count].copy_from_slice(&delivered[..count]);
    left.extend_from_slice(&delivered[count..]);
    count
}

/// A bulk USB device discovered by [`enumerate`].
#[derive(Debug, Clone, Default)]
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
    /// Linux.
    ///
    /// **Two ways of having no name, and they are different answers.** An empty string means the OS
    /// said the interface has no name: its `iInterface` is zero, or on Windows its node holds no such
    /// string. `None` means the OS did not say, because a read was refused or the device tree does not
    /// carry it. On Windows a device of one interface has no interface node, so that interface's name
    /// is `None` there, and [`product`](Self::product) is where such a device names itself.
    ///
    /// With several vendor-class interfaces this is one name for all of them, as
    /// [`vendor_interfaces`](Self::vendor_interfaces) has each.
    pub interface_name: Option<String>,
    /// What the OS's device tree says of each of the device's vendor-class interfaces, in no particular
    /// order. [`enumerate`] fills it, and [`enumerate_interface`], which lists what a driver registered,
    /// leaves it empty.
    pub vendor_interfaces: Vec<VendorInterface>,
    /// Whether the device has more than one interface, which is what the CMSIS-DAP specification's
    /// "USB composite device" comes to. `None` where the OS did not say, and from
    /// [`enumerate_interface`].
    pub composite: Option<bool>,
    /// Whether the device is known to have no product string, its `iProduct` being zero. With
    /// [`product`](Self::product) `None` and this `false`, the OS did not say.
    pub product_is_absent: bool,
    /// From [`enumerate`], the id Windows made from the device's place on the bus, where the device reports
    /// no serial. It tells two such devices apart for as long as they stay in their ports, and it is not a
    /// serial: no interface of a composite device opens by it, and no other backend or transport reports the
    /// same text. [`enumerate_interface`] leaves it empty and lists such an interface under its own path's
    /// id, which an open of that interface matches.
    pub port_id: Option<String>,
}

/// What an OS's device tree says of one vendor-class interface, with nothing opened.
///
/// Each field is what the OS can say without a handle, and `None` is what it cannot. Which of them an
/// OS fills differs, so a reader takes `None` as no answer and never as a no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorInterface {
    /// `bInterfaceSubClass` and `bInterfaceProtocol`.
    pub subclass_protocol: Option<(u8, u8)>,
    /// The interface's own name (`iInterface`): empty when it is known to have none, and `None` when
    /// the OS did not say.
    pub name: Option<String>,
    /// Whether the interface has a bulk IN and a bulk OUT endpoint. `None` on Windows, which publishes
    /// no endpoint without an open.
    pub bulk_pair: Option<bool>,
    /// Whether WinUSB registered this interface under the interface class the CMSIS-DAP specification
    /// gives a v2 probe. `None` off Windows, which has no such registry.
    pub spec_guid: Option<bool>,
}

/// Whether `subclass` and `protocol` are the ones the CMSIS-DAP specification requires of a v2 probe's
/// vendor-class interface (zero and zero).
pub(crate) fn is_spec_shape(subclass: u8, protocol: u8) -> bool {
    (subclass, protocol) == (0, 0)
}

/// What the OS's device tree says of one interface's name, from the string it published and the
/// `iInterface` index it published: the string; an empty string when the index is zero, which is an
/// interface with no name; and `None` for an interface whose index promises a string the OS did not
/// publish, or whose index it did not publish either.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn named_or_unnamed(published: Option<String>, string_index: Option<u16>) -> Option<String> {
    match (published, string_index) {
        (Some(name), _) => Some(name),
        (None, Some(0)) => Some(String::new()),
        (None, _) => None,
    }
}

/// The bulk pair to drive an interface through, as `(in, out)` endpoint addresses, zero for a direction
/// it has none in, from its bulk endpoints in descriptor order.
///
/// **THE SPECIFICATION GIVES A v2 PROBE'S ENDPOINTS IN ORDER:** Bulk Out, Bulk In, then an optional Bulk
/// In for SWO, so an interface of that shape takes the first of each direction, and an SWO endpoint with
/// the lower address is not mistaken for the replies. Any other interface takes the lowest address in
/// each direction, by the usual convention that the command pair comes first: an ST-Link's command reply
/// `0x81` beside its trace `0x82`, a WCH-Link's command pair `0x01`/`0x81` beside its data pair.
pub(crate) fn bulk_pair(endpoints_in_descriptor_order: &[u8], spec_shape: bool) -> (u8, u8) {
    let of_direction = |input: bool| {
        let mut matching = endpoints_in_descriptor_order.iter().copied().filter(|address| (address & 0x80 != 0) == input);
        if spec_shape { matching.next() } else { matching.min() }.unwrap_or(0)
    };
    (of_direction(true), of_direction(false))
}

/// Which of a device's vendor-class interfaces to try first: the lower sorts earlier.
///
/// The interface that can be driven comes before one that cannot, then one naming itself CMSIS-DAP, then
/// one of the shape the specification requires. A caller breaks a tie by the lowest interface number, so
/// that the order the OS lists the interfaces in decides nothing.
pub(crate) fn open_rank(has_pair: Option<bool>, name: Option<&str>, spec_shape: bool) -> (bool, bool, bool) {
    (has_pair == Some(false), !name.is_some_and(is_named_cmsis_dap), !spec_shape)
}

/// What trying to open one interface came to.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) enum Attempt<T> {
    Opened(T),
    /// The OS would not give it up, and said this.
    Refused(String),
    /// Not one to use, which says nothing about the device's others: another serial, or no bulk pair.
    Passed,
}

/// The first of `interfaces` that opens, best first, each given with the device it belongs to: the open
/// when one does, `None` when none would be used, and what the OS said when one refused and none opened.
///
/// **A REFUSED INTERFACE ENDS ITS OWN DEVICE AND NO OTHER.** Another program holding the best interface of a
/// device does not make the one beside it what was asked for, whatever that one calls itself: a caller
/// would write its protocol into it, and the reason it could not have the one it wanted would be lost.
/// Another device is another matter, and it may be the one that was named. Linux gets the same answer by
/// choosing one interface for each device before it opens anything.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn first_that_opens<D: PartialEq, I, T>(
    interfaces: impl IntoIterator<Item = (D, I)>,
    mut attempt: impl FnMut(I) -> Attempt<T>,
) -> std::result::Result<Option<T>, String> {
    let mut refused: Vec<D> = Vec::new();
    let mut why = None;
    for (device, interface) in interfaces {
        if refused.contains(&device) {
            continue;
        }
        match attempt(interface) {
            Attempt::Opened(opened) => return Ok(Some(opened)),
            Attempt::Refused(reason) => {
                refused.push(device);
                why = Some(reason);
            }
            Attempt::Passed => {}
        }
    }
    why.map_or(Ok(None), Err)
}

/// One interface of a configuration, as its descriptors state it.
#[cfg_attr(target_os = "windows", allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DescribedInterface {
    pub(crate) number: u8,
    pub(crate) alternate: u8,
    pub(crate) class: u8,
    pub(crate) subclass: u8,
    pub(crate) protocol: u8,
    pub(crate) string_index: u8,
    /// Its bulk endpoints' addresses, in descriptor order.
    pub(crate) bulk_endpoints: Vec<u8>,
}

/// Every interface of the configuration whose `bConfigurationValue` is `configuration` (the first one
/// `raw` holds, when `None`), each alternate setting its own entry, among a device's raw descriptors:
/// perhaps a device descriptor, then each configuration and what follows it (USB 2.0, Tables 9-10, 9-12
/// and 9-13).
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn described_interfaces(raw: &[u8], configuration: Option<u8>) -> Vec<DescribedInterface> {
    const CONFIGURATION: u8 = 2;
    const INTERFACE: u8 = 4;
    const ENDPOINT: u8 = 5;
    const BULK: u8 = 2;
    let mut out: Vec<DescribedInterface> = Vec::new();
    let mut wanted = configuration;
    let mut inside = false;
    for descriptor in descriptors(raw) {
        match *descriptor {
            [_, CONFIGURATION, _, _, _, value, ..] => inside = *wanted.get_or_insert(value) == value,
            [_, INTERFACE, number, alternate, _, class, subclass, protocol, string_index, ..] if inside => {
                out.push(DescribedInterface {
                    number,
                    alternate,
                    class,
                    subclass,
                    protocol,
                    string_index,
                    bulk_endpoints: Vec::new(),
                });
            }
            [_, ENDPOINT, address, attributes, ..] if inside && attributes & 0x03 == BULK => {
                if let Some(interface) = out.last_mut() {
                    interface.bulk_endpoints.push(address);
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether `endpoints` hold a bulk endpoint in each direction.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn has_bulk_pair(endpoints: &[u8]) -> bool {
    endpoints.iter().any(|address| address & 0x80 != 0) && endpoints.iter().any(|address| address & 0x80 == 0)
}

/// A configuration's vendor-class interfaces at their first alternate settings, read from its descriptors
/// alone, and whether it has more than one interface of any class. An interface's name is not in the
/// descriptors, only its string index: zero is an interface with no name, and any other number is a
/// string this walk has not read.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn vendor_interfaces_of_configuration(set: &[u8]) -> (Vec<VendorInterface>, bool) {
    let all = described_interfaces(set, None);
    let vendor = all
        .iter()
        .filter(|interface| interface.class == 0xFF && interface.alternate == 0)
        .map(|interface| VendorInterface {
            subclass_protocol: Some((interface.subclass, interface.protocol)),
            name: named_or_unnamed(None, Some(u16::from(interface.string_index))),
            bulk_pair: Some(has_bulk_pair(&interface.bulk_endpoints)),
            spec_guid: None,
        })
        .collect();
    (vendor, all.iter().any(|interface| interface.number != all[0].number))
}

/// [`DeviceInfo::interface_name`] for a device, from what the OS's device tree says of the name of
/// each of its vendor-class interfaces, as [`named_or_unnamed`] gives it: a name holding `CMSIS-DAP`;
/// otherwise `None` if any of them could not be read; otherwise the lowest of their names; otherwise an
/// empty string, when every one of them is known to have none.
///
/// **THE ORDER THE OS LISTS THE INTERFACES IN DECIDES NOTHING.** A device with another vendor-class
/// interface beside a CMSIS-DAP one is a probe, whether the other has a name or not, and an interface
/// that could not be read may be the CMSIS-DAP one.
pub(crate) fn vendor_interfaces_name(readings: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let mut unread = false;
    let mut lowest: Option<String> = None;
    let mut unnamed = false;
    for reading in readings {
        match reading {
            Some(name) if is_named_cmsis_dap(&name) => {
                return Some(name);
            }
            Some(name) if !name.is_empty() => {
                lowest = Some(lowest.map_or_else(|| name.clone(), |held| held.min(name.clone())));
            }
            Some(_) => unnamed = true,
            None => unread = true,
        }
    }
    if unread {
        return None;
    }
    lowest.or_else(|| unnamed.then(String::new))
}

/// Whether an interface's name holds the token CMSIS-DAP requires, spelled as some platforms sanitize
/// it. `lamella-probe`, which decides what is a probe, tests a name the same way.
pub(crate) fn is_named_cmsis_dap(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("CMSIS-DAP") || upper.contains("CMSIS_DAP")
}

/// Lists every connected device exposing a vendor-specific class-0xFF interface -- a CMSIS-DAP v2 probe OR
/// e.g. a Lamella Link board -- with its ids and, where the OS reports
/// them, serial/product strings. No VID filter: a caller keeps the vendor id(s) it wants (a probe consumer
/// filters to probe vendors; the Lamella Link picker keeps its own VID). Cross-platform (Windows/macOS/Linux).
///
/// **The interface class is the whole test.** A listed device is not checked for a bulk IN/OUT pair, so carrying
/// one is an expectation of the vendor-bulk convention rather than something this function establishes.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    imp::enumerate()
}

/// Lists the devices registered under a caller-supplied WinUSB device-interface GUID (a
/// `"{...}"` string) -- e.g. every attached Lamella Link board -- with product and serial
/// strings where the OS can report them. Windows only ([`Error::Unsupported`]
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

    /// The bulk pipe identifiers negotiated at open time, as `(in, out)`.
    ///
    /// Exposed because probing endpoints blindly is not a viable diagnostic: reading an endpoint a
    /// device does not have can block rather than fail, so a tool that needs to know which pipes
    /// exist must ask instead of sweep.
    ///
    /// **These are USB endpoint addresses on Linux and Windows, and IOKit pipe reference numbers on
    /// macOS**, where the framework addresses a pipe by index rather than by address. Treat them as
    /// opaque identifiers for diagnostics, not as addresses that mean the same thing everywhere.
    pub fn endpoints(&self) -> (u8, u8) {
        self.0.endpoints()
    }

    /// Clears any stall on both pipes, so one failed transfer does not contaminate the next.
    ///
    /// **Windows only today** -- the Linux and macOS backends do nothing here, so a diagnostic that
    /// resets between attempts gets a reset on Windows alone.
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

    /// Sends one bulk OUT packet (raw -- no report id or padding) on the primary OUT endpoint.
    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_packet(data)
    }

    /// Reads one bulk IN packet into `buf` from the primary IN endpoint, returning its length.
    ///
    /// **A device with nothing to send in `timeout` is [`Error::Timeout`], on every platform,** and
    /// the device is still there. Any other failure, one that has gone away among them, is
    /// [`Error::Os`].
    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_packet(buf, timeout)
    }

    /// [`read_packet`](Self::read_packet) for a caller that polls a device which may send at any
    /// moment, and so expects most reads to time out.
    ///
    /// **THE TRANSFER STAYS OUTSTANDING ACROSS A TIMEOUT, AND THE NEXT POLL COLLECTS IT, ON EVERY OS.** A
    /// read that is given a time limit is cancelled at it, and what a cancelled transfer held is lost on
    /// macOS and Linux and unspecified on Windows, so bytes arriving just as a poll times out would be
    /// acknowledged to the device and thrown away, without a sign. Here nothing is cancelled until the
    /// device is closed. A timeout says only that nothing finished in `timeout`.
    ///
    /// It is not how every read goes because a reply to a request is not polled for: its timeout is a
    /// failure, and a transfer left behind it would take the next reply.
    pub fn poll_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.poll_packet(buf, timeout)
    }

    /// Sends one bulk OUT packet on a specific endpoint address. A single-pair probe uses only its
    /// primary endpoint ([`write_packet`](Self::write_packet)); a device with more than one bulk pair --
    /// e.g. a WCH-Link, whose command pair is `0x01`/`0x81` and whose flash-stream data pair is
    /// `0x02`/`0x82` -- reaches the others here.
    pub fn write_endpoint(&mut self, endpoint: u8, data: &[u8]) -> Result<()> {
        self.0.write_endpoint(endpoint, data)
    }

    /// Reads one bulk IN packet from a specific endpoint address into `buf`, returning its length --
    /// the companion to [`write_endpoint`](Self::write_endpoint). A timeout is reported as it is by
    /// [`read_packet`](Self::read_packet).
    pub fn read_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_endpoint(endpoint, buf, timeout)
    }
}

/// The type of a control request: bits 6 and 5 of `bmRequestType` (USB 2.0, Table 9-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// A request the USB specification defines for every device.
    Standard,
    /// A request a device class defines.
    Class,
    /// A request the device's vendor defines.
    Vendor,
}

/// Who a control request is addressed to: bits 4 to 0 of `bmRequestType` (USB 2.0, Table 9-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipient {
    /// The device.
    Device,
    /// An interface, whose number is the low byte of `wIndex` (USB 2.0, Figure 9-3).
    Interface,
    /// An endpoint, whose direction and number are the low byte of `wIndex` (USB 2.0, Figure 9-2).
    Endpoint,
    /// Another recipient.
    Other,
}

/// A control request's setup stage, less its direction and its length (USB 2.0, 9.3).
///
/// The direction is set by whichever of [`ControlInterface::control_in`] and
/// [`ControlInterface::control_out`] sends the request, and `wLength` is the length of the buffer
/// that call is given, so neither can disagree with the transfer that carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRequest {
    /// The request's type.
    pub kind: RequestKind,
    /// The request's recipient.
    pub recipient: Recipient,
    /// `bRequest`: which request this is.
    pub request: u8,
    /// `wValue`: a parameter whose meaning depends on the request.
    pub value: u16,
    /// `wIndex`: a parameter whose meaning depends on the request; for a request addressed to an
    /// interface or an endpoint, the one it is addressed to.
    pub index: u16,
}

impl ControlRequest {
    /// The setup stage this request is sent with, its data stage `length` bytes long and running from
    /// device to host when `device_to_host` (USB 2.0, Table 9-2).
    ///
    /// # Errors
    /// [`Error::InvalidRequest`] for a data stage longer than `wLength`'s sixteen bits can state.
    pub(crate) fn setup(&self, device_to_host: bool, length: usize) -> Result<Setup> {
        let length = u16::try_from(length).map_err(|_| {
            Error::InvalidRequest(format!(
                "a control transfer's data stage is at most 65,535 bytes, since wLength is sixteen \
                 bits, and this one is {length}"
            ))
        })?;
        let direction = if device_to_host { 0x80 } else { 0x00 };
        let kind = match self.kind {
            RequestKind::Standard => 0x00,
            RequestKind::Class => 0x20,
            RequestKind::Vendor => 0x40,
        };
        let recipient = match self.recipient {
            Recipient::Device => 0,
            Recipient::Interface => 1,
            Recipient::Endpoint => 2,
            Recipient::Other => 3,
        };
        Ok(Setup {
            request_type: direction | kind | recipient,
            request: self.request,
            value: self.value,
            index: self.index,
            length,
        })
    }
}

/// A setup stage as a backend sends it: the five fields of USB 2.0, Table 9-2, its length already
/// checked against the buffer that goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Setup {
    pub(crate) request_type: u8,
    pub(crate) request: u8,
    pub(crate) value: u16,
    pub(crate) index: u16,
    pub(crate) length: u16,
}

/// A request the device stalled, worded once so that every backend reports a stall the same way.
///
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn stalled(setup: Setup) -> Error {
    Error::Os(format!(
        "the device stalled request {:#04x} (bmRequestType {:#04x}, wValue {:#06x}, wIndex {:#06x})",
        setup.request, setup.request_type, setup.value, setup.index
    ))
}

/// A control transfer's timeout in whole milliseconds, never zero.
///
pub(crate) fn bounded_milliseconds(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).max(1)
}

/// An interface's class, subclass and protocol, as its interface descriptor states them (USB 2.0,
/// Table 9-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceClass {
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub subclass: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
}

/// An interface of an attached device, found by [`enumerate_class`].
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number string, if the OS reported one.
    pub serial_number: Option<String>,
    /// Product string, if the OS reported one.
    pub product: Option<String>,
    /// The interface's `bInterfaceNumber`.
    pub interface_number: u8,
    /// The interface's own name (`iInterface`), where the OS publishes it.
    pub interface_name: Option<String>,
}

/// Lists every attached device that has an interface of `class`, with that interface's number and
/// name -- one such interface, where a device has several, and the same one each time. Listing opens
/// nothing.
pub fn enumerate_class(class: InterfaceClass) -> Result<Vec<InterfaceInfo>> {
    imp::enumerate_class(class)
}

/// Splits a configuration's descriptors -- what GET_DESCRIPTOR returns for a configuration -- into
/// one slice per descriptor, in the order the device sent them: the configuration descriptor, each
/// interface descriptor with the endpoint descriptors after it, and each class- or vendor-specific
/// descriptor after the standard descriptor it extends (USB 2.0, 9.4.3).
///
/// Each slice is `bLength` bytes, with `bDescriptorType` at index 1. The walk ends at the end of
/// `set`, or early at a descriptor whose `bLength` is below two or runs past the end, so a malformed
/// set is cut short rather than read past.
pub fn descriptors(set: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = set;
    std::iter::from_fn(move || {
        let length = usize::from(*rest.first()?);
        if length < 2 || length > rest.len() {
            return None;
        }
        let (descriptor, tail) = rest.split_at(length);
        rest = tail;
        Some(descriptor)
    })
}

/// The first interface descriptor among a configuration's descriptors whose class is `class`, as its
/// `bInterfaceNumber` and `iInterface` (USB 2.0, Tables 9-5 and 9-12).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn interface_of_class(set: &[u8], class: InterfaceClass) -> Option<(u8, u8)> {
    const INTERFACE: u8 = 4;
    let wanted = (class.class, class.subclass, class.protocol);
    descriptors(set).find_map(|descriptor| match *descriptor {
        [_, INTERFACE, number, _, _, c, s, p, name, ..] if (c, s, p) == wanted => Some((number, name)),
        _ => None,
    })
}

/// An interface opened to be driven through its device's default control pipe -- all a DFU
/// interface has, since it uses no endpoint of its own (DFU 1.1, Table 4.4).
pub struct ControlInterface(imp::ControlInterface);

impl ControlInterface {
    /// Opens the first attached device with `vendor_id` and `product_id` -- and `serial`, when one is
    /// named -- that has an interface of `class`: on Linux by claiming that interface, on macOS by
    /// opening the whole device, and on Windows through the device interface WinUSB registered for it.
    ///
    /// A named serial is matched whole, without regard to case, as [`enumerate_class`] reports it: a
    /// device whose serial only contains it is another device, and a serial that no attached device
    /// reports is [`Error::NotFound`], never another device of the same vendor and product.
    pub fn open(
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
        class: InterfaceClass,
    ) -> Result<Self> {
        imp::ControlInterface::open(vendor_id, product_id, serial, class).map(ControlInterface)
    }

    /// The claimed interface's `bInterfaceNumber`, which a request addressed to it carries in
    /// `wIndex`.
    pub fn interface_number(&self) -> u8 {
        self.0.interface_number()
    }

    /// Sends `request` with a data stage from device to host into `buffer`, and answers how many
    /// bytes the device returned; `wLength` is the length of `buffer`, and a device may return fewer.
    ///
    /// `timeout` bounds the transfer. One shorter than a millisecond is a millisecond, so no transfer
    /// waits without a bound, and a transfer that reaches it is cancelled before this returns.
    ///
    /// # Errors
    /// [`Error::InvalidRequest`], before anything is sent, for a buffer longer than a data stage can be
    /// here: 65,535 bytes, all that `wLength` can state, and on Windows 4 KB, the most
    /// `WinUsb_ControlTransfer` takes. Linux usbfs takes at most one page (`PAGE_SIZE`) and refuses a
    /// longer data stage itself, which comes back as [`Error::Os`]. [`Error::Timeout`] for a transfer
    /// that did not complete in time. [`Error::Os`] for any other failure, a request the device stalled
    /// among them: its text names the stalled request on Linux and macOS, and carries WinUSB's error
    /// code on Windows.
    pub fn control_in(
        &mut self,
        request: ControlRequest,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<usize> {
        let setup = request.setup(true, buffer.len())?;
        self.0.control_in(setup, buffer, bounded_milliseconds(timeout))
    }

    /// Sends `request` with `data` as its data stage from host to device, or with no data stage when
    /// `data` is empty.
    ///
    /// `timeout` bounds the transfer as it does for [`control_in`](Self::control_in).
    ///
    /// # Errors
    /// Those of [`control_in`](Self::control_in), and [`Error::Os`] when the device took fewer bytes
    /// than were sent.
    pub fn control_out(&mut self, request: ControlRequest, data: &[u8], timeout: Duration) -> Result<()> {
        let setup = request.setup(false, data.len())?;
        let taken = self.0.control_out(setup, data, bounded_milliseconds(timeout))?;
        if taken == data.len() {
            Ok(())
        } else {
            Err(Error::Os(format!("the device took {taken} of the {} bytes sent", data.len())))
        }
    }
}

#[cfg(any(target_os = "macos", test))]
mod iokit_status;
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
    use super::{
        Attempt, Error, Result, candidate_satisfies, first_that_opens, select_by_whole_serial, select_requested,
        serial_is, serial_matches,
    };

    /// The walk over two devices' interfaces, each `(device, interface, what trying it comes to)`: what it
    /// returned, and which interfaces it tried.
    fn walked(interfaces: &[(u32, &'static str, Option<&'static str>)]) -> (std::result::Result<Option<&'static str>, String>, Vec<&'static str>) {
        let mut tried = Vec::new();
        let outcome = first_that_opens(interfaces.iter().map(|&(device, name, refusal)| (device, (name, refusal))), |(name, refusal)| {
            tried.push(name);
            match refusal {
                Some("passed") => Attempt::Passed,
                Some(why) => Attempt::Refused(why.to_owned()),
                None => Attempt::Opened(name),
            }
        });
        (outcome, tried)
    }

    /// **A REFUSED INTERFACE ENDS ITS OWN DEVICE, WHATEVER RANKS THE ONE BESIDE IT.** Two interfaces of one
    /// rank are the ST-LINK/V3's Debug and Bridge, and a lower one is a composite's "Bridge" beside its
    /// "CMSIS-DAP v2". The caller is told what the OS said, and writes nothing into the sibling.
    #[test]
    fn a_refused_interface_ends_its_device() {
        let (outcome, tried) = walked(&[(1, "Debug", Some("held")), (1, "Bridge", None)]);
        assert_eq!(outcome, Err("held".to_owned()));
        assert_eq!(tried, ["Debug"], "the sibling is not tried");
    }

    /// **AND NO OTHER.** A held board is no reason to refuse the one that was named, whatever it ranks, and a
    /// second held board stops its own sibling as the first did.
    #[test]
    fn a_refused_interface_ends_no_other_device() {
        let (outcome, tried) = walked(&[(1, "A's DAP", Some("held")), (2, "B's nameless", None)]);
        assert_eq!(outcome, Ok(Some("B's nameless")));
        assert_eq!(tried, ["A's DAP", "B's nameless"]);

        let (outcome, tried) = walked(&[
            (1, "A's DAP", Some("A held")),
            (2, "B's DAP", Some("B held")),
            (1, "A's Bridge", None),
            (2, "B's Bridge", None),
        ]);
        assert_eq!(outcome, Err("B held".to_owned()));
        assert_eq!(tried, ["A's DAP", "B's DAP"]);
    }

    /// An interface that was merely passed over, for a serial or for having no bulk pair, says nothing about
    /// the next, and with nothing opened and nothing refused the device is not there.
    #[test]
    fn a_passed_interface_ends_nothing() {
        let (outcome, tried) = walked(&[(1, "no pair", Some("passed")), (1, "DAP", None)]);
        assert_eq!(outcome, Ok(Some("DAP")));
        assert_eq!(tried, ["no pair", "DAP"]);
        assert_eq!(walked(&[(1, "another serial", Some("passed"))]).0, Ok(None));
    }


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
    /// An interface opened to be driven is chosen by its whole serial: a board whose serial contains the
    /// one named, or is contained in it, is another board, whichever is listed first.
    fn the_whole_serial_rule_takes_only_the_board_named() {
        assert!(serial_is(Some("ABC"), Some("ABC")));
        assert!(serial_is(Some("abc"), Some("ABC")), "without regard to case");
        assert!(!serial_is(Some("ABC"), Some("ABC1")), "a longer serial that contains it");
        assert!(!serial_is(Some("ABC1"), Some("ABC")), "a shorter one it contains");
        assert!(!serial_is(Some("ABC"), None), "a device that reports none");
        assert!(serial_is(None, None) && serial_is(None, Some("ABC")), "nothing named takes any");
        let nested = [(Some("ABC1"), "listed-first"), (Some("ABC"), "named")];
        let pick = |wanted: Option<&str>| {
            select_by_whole_serial(nested.iter().copied(), wanted, |(serial, _)| *serial).map(|(_, tag)| tag)
        };
        assert_eq!(pick(Some("ABC")).unwrap(), "named");
        assert!(matches!(pick(Some("BC")), Err(Error::NotFound)), "a part of a serial names no board");
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

#[cfg(test)]
mod control_tests {
    use super::{
        ControlRequest, Error, InterfaceClass, Recipient, RequestKind, bounded_milliseconds,
        descriptors, interface_of_class,
    };
    use std::time::Duration;

    fn request(kind: RequestKind, recipient: Recipient) -> ControlRequest {
        ControlRequest { kind, recipient, request: 0, value: 0, index: 0 }
    }

    #[test]
    /// DFU 1.1, section 3: every DFU request is a class request to an interface, `00100001b` from
    /// host to device and `10100001b` from device to host.
    fn a_class_request_to_an_interface_is_0x21_out_and_0xa1_in() {
        let dfu = request(RequestKind::Class, Recipient::Interface);
        assert_eq!(dfu.setup(false, 0).unwrap().request_type, 0x21);
        assert_eq!(dfu.setup(true, 6).unwrap().request_type, 0xA1);
    }

    #[test]
    /// USB 2.0, Table 9-3: GET_DESCRIPTOR is `10000000B`, and SET_INTERFACE is `00000001B`.
    fn the_standard_requests_carry_the_request_types_the_specification_lists() {
        let get_descriptor = ControlRequest {
            request: 6,
            value: 0x0200,
            ..request(RequestKind::Standard, Recipient::Device)
        };
        let setup = get_descriptor.setup(true, 255).unwrap();
        assert_eq!(setup.request_type, 0b1000_0000);
        assert_eq!((setup.request, setup.value, setup.index, setup.length), (6, 0x0200, 0, 255));
        let set_interface = request(RequestKind::Standard, Recipient::Interface);
        assert_eq!(set_interface.setup(false, 0).unwrap().request_type, 0b0000_0001);
    }

    #[test]
    /// USB 2.0, Table 9-2: the type is bits 6 and 5 and the recipient bits 4 to 0, whichever way
    /// the data stage runs.
    fn every_type_and_recipient_takes_its_own_bits() {
        let out = |kind, recipient| request(kind, recipient).setup(false, 0).unwrap().request_type;
        assert_eq!(out(RequestKind::Vendor, Recipient::Device), 0b0100_0000);
        assert_eq!(out(RequestKind::Class, Recipient::Endpoint), 0b0010_0010);
        assert_eq!(out(RequestKind::Standard, Recipient::Other), 0b0000_0011);
        let vendor_in = request(RequestKind::Vendor, Recipient::Other).setup(true, 1).unwrap();
        assert_eq!(vendor_in.request_type, 0b1100_0011);
    }

    #[test]
    /// `wLength` is two bytes (USB 2.0, Table 9-2), so a longer data stage has no setup stage that
    /// can describe it, and it is refused before anything is sent.
    fn a_data_stage_longer_than_wlength_can_state_is_refused() {
        let vendor = request(RequestKind::Vendor, Recipient::Device);
        assert_eq!(vendor.setup(false, 65_535).unwrap().length, 65_535);
        assert!(matches!(vendor.setup(false, 65_536), Err(Error::InvalidRequest(_))));
        assert!(matches!(vendor.setup(true, usize::MAX), Err(Error::InvalidRequest(_))));
    }

    #[test]
    fn a_timeout_is_whole_milliseconds_and_never_zero() {
        assert_eq!(bounded_milliseconds(Duration::ZERO), 1);
        assert_eq!(bounded_milliseconds(Duration::from_micros(999)), 1);
        assert_eq!(bounded_milliseconds(Duration::from_millis(1_500)), 1_500);
        assert_eq!(bounded_milliseconds(Duration::MAX), u32::MAX);
    }

    /// A DFU-mode configuration laid out as DFU 1.1 section 4.2 describes it: the configuration
    /// descriptor (USB 2.0, Table 9-10), an interface descriptor for each of two alternate settings
    /// (DFU 1.1, Table 4.4), and the functional descriptor (DFU 1.1, Table 4.2).
    const DFU_CONFIGURATION: [u8; 36] = [
        0x09, 0x02, 0x24, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32,
        0x09, 0x04, 0x00, 0x00, 0x00, 0xFE, 0x01, 0x02, 0x04,
        0x09, 0x04, 0x00, 0x01, 0x00, 0xFE, 0x01, 0x02, 0x05,
        0x09, 0x21, 0x0B, 0xFF, 0x00, 0x00, 0x08, 0x1A, 0x01,
    ];

    #[test]
    /// The walk yields each descriptor whole, by its `bLength`, in the order the set holds them.
    fn a_configuration_splits_into_its_descriptors_in_order() {
        let types: Vec<u8> = descriptors(&DFU_CONFIGURATION).map(|descriptor| descriptor[1]).collect();
        assert_eq!(types, [0x02, 0x04, 0x04, 0x21]);
        assert!(descriptors(&DFU_CONFIGURATION).all(|descriptor| descriptor.len() == 9));
    }

    #[test]
    /// A descriptor whose `bLength` is below two, or longer than what is left, ends the walk
    /// instead of being read past.
    fn a_malformed_descriptor_ends_the_walk() {
        let mut zero_length = DFU_CONFIGURATION;
        zero_length[9] = 0;
        assert_eq!(descriptors(&zero_length).count(), 1);
        let truncated = &DFU_CONFIGURATION[..DFU_CONFIGURATION.len() - 1];
        assert_eq!(descriptors(truncated).count(), 3);
        assert_eq!(descriptors(&[]).count(), 0);
        assert_eq!(descriptors(&[0x01]).count(), 0);
    }

    #[test]
    /// An interface is found by its class, subclass and protocol together, and answered as its
    /// number and its first alternate setting's string index.
    fn an_interface_is_found_by_its_class_subclass_and_protocol() {
        let dfu_mode = InterfaceClass { class: 0xFE, subclass: 0x01, protocol: 0x02 };
        assert_eq!(interface_of_class(&DFU_CONFIGURATION, dfu_mode), Some((0, 4)));
        let run_time = InterfaceClass { protocol: 0x01, ..dfu_mode };
        assert_eq!(interface_of_class(&DFU_CONFIGURATION, run_time), None);
        let vendor = InterfaceClass { class: 0xFF, subclass: 0x00, protocol: 0x00 };
        assert_eq!(interface_of_class(&DFU_CONFIGURATION, vendor), None);
    }

    #[test]
    /// Only a descriptor whose type is INTERFACE is read as one, whatever bytes sit where an
    /// interface descriptor keeps its class.
    fn only_an_interface_descriptor_is_read_as_an_interface() {
        let functional_with_class_bytes = [0x09, 0x21, 0x00, 0x00, 0x00, 0xFE, 0x01, 0x02, 0x00];
        let dfu_mode = InterfaceClass { class: 0xFE, subclass: 0x01, protocol: 0x02 };
        assert_eq!(interface_of_class(&functional_with_class_bytes, dfu_mode), None);
    }

    /// An index of zero says the interface has no name; a non-zero index with no string, or no index
    /// at all, says nothing.
    #[test]
    fn an_interface_with_string_index_zero_has_no_name_and_anything_unpublished_is_unknown() {
        let named = super::named_or_unnamed(Some("CMSIS-DAP v2 Interface".to_owned()), Some(5));
        assert_eq!(named.as_deref(), Some("CMSIS-DAP v2 Interface"));
        assert_eq!(super::named_or_unnamed(None, Some(0)).as_deref(), Some(""));
        assert_eq!(super::named_or_unnamed(None, Some(5)), None);
        assert_eq!(super::named_or_unnamed(None, None), None);
    }

    /// **THE ORDER THE OS LISTS THE INTERFACES IN DECIDES NOTHING**, and "no name" is only as settled
    /// as the least settled interface: a device is known to have no name only when all of its
    /// vendor-class interfaces are known to have none.
    #[test]
    fn a_device_has_no_name_only_when_every_vendor_interface_is_known_to_have_none() {
        let name = || Some("CMSIS-DAP v2 Interface".to_owned());
        let none = || Some(String::new());
        let unread = || None;
        let of = |readings: Vec<Option<String>>| super::vendor_interfaces_name(readings);
        assert_eq!(of(vec![none(), name()]), name());
        assert_eq!(of(vec![name(), none()]), name());
        assert_eq!(of(vec![unread(), name()]), name());
        let other = || Some("Acme Trace".to_owned());
        assert_eq!(of(vec![other(), name()]), name(), "two names, and the one naming a probe is the answer");
        assert_eq!(of(vec![name(), other()]), name());
        let lower = || Some("Acme Bridge".to_owned());
        assert_eq!(of(vec![other(), lower()]), lower(), "and with none naming one, the order still decides nothing");
        assert_eq!(of(vec![lower(), other()]), lower());
        assert_eq!(of(vec![other(), none()]), other());
        assert_eq!(of(vec![other(), unread()]), unread(), "the unread one may be the probe");
        assert_eq!(of(vec![unread(), other()]), unread());
        assert_eq!(of(vec![none()]), none());
        assert_eq!(of(vec![none(), none()]), none());
        assert_eq!(of(vec![none(), unread()]), unread());
        assert_eq!(of(vec![unread(), none()]), unread());
        assert_eq!(of(vec![]), unread());
    }

    /// **A v2 PROBE'S ENDPOINTS ARE IN THE ORDER THE SPECIFICATION GIVES THEM,** so the first of each
    /// direction is the pair, whatever the addresses, and an SWO endpoint with the lower address is not
    /// taken for the replies. Any other interface keeps the lowest address in each direction.
    #[test]
    fn the_spec_shape_takes_the_first_of_each_direction_and_any_other_the_lowest_address() {
        let swo_below_the_replies = [0x02, 0x83, 0x81];
        assert_eq!(super::bulk_pair(&swo_below_the_replies, true), (0x83, 0x02), "commands, then replies, then SWO");
        assert_eq!(super::bulk_pair(&swo_below_the_replies, false), (0x81, 0x02), "the lowest address");
        let st_link = [0x01, 0x81, 0x82];
        assert_eq!(super::bulk_pair(&st_link, false), (0x81, 0x01), "the command reply, not the trace");
        let wch_link = [0x82, 0x02, 0x81, 0x01];
        assert_eq!(super::bulk_pair(&wch_link, false), (0x81, 0x01), "the command pair, not the data pair");
        assert_eq!(super::bulk_pair(&[0x81], true), (0x81, 0), "no OUT endpoint");
        assert_eq!(super::bulk_pair(&[], false), (0, 0));
        assert!(super::has_bulk_pair(&st_link));
        assert!(!super::has_bulk_pair(&[0x81, 0x82]));
        assert!(!super::has_bulk_pair(&[]));
    }

    /// The interface to open is the one that can be driven and names itself; a name alone does not
    /// outrank a pair, since a name is all a control-only WebUSB interface has.
    #[test]
    fn an_interface_that_can_be_driven_and_names_itself_is_tried_first() {
        let rank = super::open_rank;
        let token = Some("CMSIS-DAP v2 Interface");
        let bridge = Some("Bridge");
        assert!(rank(Some(true), token, true) < rank(Some(true), bridge, true));
        assert!(rank(Some(true), token, true) < rank(Some(true), token, false));
        assert!(rank(Some(true), token, false) < rank(Some(true), bridge, true), "a name outranks the shape");
        assert!(rank(Some(true), bridge, true) < rank(Some(true), bridge, false));
        assert!(rank(Some(true), bridge, false) < rank(Some(false), token, true), "a pair outranks a name");
        assert!(rank(None, token, true) < rank(Some(false), token, true), "a pair not known is not a pair known absent");
        assert!(rank(None, token, true) < rank(None, None, true));
        assert_eq!(rank(None, token, true), rank(Some(true), token, true), "not known costs nothing against known");
    }

    /// Each interface with its own bulk endpoints in descriptor order, whatever sits between, from the
    /// configuration asked for.
    #[test]
    fn a_configurations_interfaces_are_read_with_their_bulk_endpoints_in_order() {
        let mut raw = vec![18, 1];
        raw.resize(18, 0);
        raw.extend_from_slice(&[9, 2, 0, 0, 2, 1, 0, 0x80, 50]);
        raw.extend_from_slice(&[9, 4, 0, 0, 3, 0xFF, 0, 0, 7]);
        raw.extend_from_slice(&[7, 5, 0x02, 2, 64, 0, 0]);
        raw.extend_from_slice(&[4, 0x21, 0, 0]);
        raw.extend_from_slice(&[7, 5, 0x83, 2, 64, 0, 0]);
        raw.extend_from_slice(&[7, 5, 0x81, 2, 64, 0, 0]);
        raw.extend_from_slice(&[9, 4, 1, 0, 1, 0xFF, 0x10, 0xFF, 0]);
        raw.extend_from_slice(&[7, 5, 0x84, 3, 8, 0, 1]);
        raw.extend_from_slice(&[9, 2, 0, 0, 1, 2, 0, 0x80, 50]);
        raw.extend_from_slice(&[9, 4, 0, 0, 2, 0xFF, 0x03, 0, 9]);
        raw.extend_from_slice(&[7, 5, 0x01, 2, 64, 0, 0]);
        raw.extend_from_slice(&[7, 5, 0x81, 2, 64, 0, 0]);

        let first = super::described_interfaces(&raw, Some(1));
        assert_eq!(first.len(), 2);
        assert_eq!((first[0].number, first[0].class, first[0].subclass, first[0].protocol), (0, 0xFF, 0, 0));
        assert_eq!(first[0].string_index, 7);
        assert_eq!(first[0].bulk_endpoints, [0x02, 0x83, 0x81]);
        assert_eq!((first[1].number, first[1].subclass, first[1].protocol), (1, 0x10, 0xFF));
        assert_eq!(first[1].string_index, 0);
        assert!(first[1].bulk_endpoints.is_empty(), "an interrupt endpoint is not a bulk one");

        let second = super::described_interfaces(&raw, Some(2));
        assert_eq!(second.len(), 1);
        assert_eq!((second[0].subclass, second[0].string_index), (0x03, 9));
        assert_eq!(second[0].bulk_endpoints, [0x01, 0x81]);

        assert!(super::described_interfaces(&raw, Some(3)).is_empty());
        assert_eq!(super::described_interfaces(&raw, None), first, "none named takes the first");
        assert_eq!(super::described_interfaces(&raw[18..], None), first, "with no device descriptor before it");
    }

    /// **A DEVICE THE OS NEVER CONFIGURED STILL SAYS WHAT IT IS, IN ITS DESCRIPTORS:** a string index of
    /// zero is an interface with no name, which is a settled answer, and one that is not zero is a
    /// string nothing has read yet.
    #[test]
    fn a_configurations_descriptors_say_what_they_can_of_its_vendor_interfaces() {
        let mut set = vec![9, 2, 0, 0, 3, 1, 0, 0x80, 50];
        set.extend_from_slice(&[9, 4, 0, 0, 2, 0xFF, 0, 0, 0]);
        set.extend_from_slice(&[7, 5, 0x01, 2, 64, 0, 0]);
        set.extend_from_slice(&[7, 5, 0x81, 2, 64, 0, 0]);
        set.extend_from_slice(&[9, 4, 1, 0, 1, 0xFF, 0x10, 0xFF, 6]);
        set.extend_from_slice(&[7, 5, 0x82, 2, 64, 0, 0]);
        set.extend_from_slice(&[9, 4, 2, 0, 0, 0x02, 0x02, 0x01, 7]);
        set.extend_from_slice(&[9, 4, 0, 1, 0, 0xFF, 0x42, 0x01, 8]);
        let (vendor, composite) = super::vendor_interfaces_of_configuration(&set);
        assert!(composite);
        assert_eq!(vendor.len(), 2);
        assert_eq!(vendor[0].subclass_protocol, Some((0, 0)));
        assert_eq!(vendor[0].name.as_deref(), Some(""), "a string index of zero");
        assert_eq!(vendor[0].bulk_pair, Some(true));
        assert_eq!(vendor[1].subclass_protocol, Some((0x10, 0xFF)));
        assert_eq!(vendor[1].name, None, "a string nothing has read");
        assert_eq!(vendor[1].bulk_pair, Some(false));

        let mut one = vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50];
        one.extend_from_slice(&[9, 4, 0, 0, 0, 0xFF, 0, 0, 0]);
        one.extend_from_slice(&[9, 4, 0, 1, 0, 0xFF, 0, 0, 0]);
        let (vendor, composite) = super::vendor_interfaces_of_configuration(&one);
        assert!(!composite, "two settings of one interface");
        assert_eq!(vendor.len(), 1);
        assert_eq!(super::vendor_interfaces_of_configuration(&[]), (Vec::new(), false));
    }

    /// An interface's string index is its own configuration's and alternate setting's, through a device
    /// descriptor and whatever sits between.
    #[test]
    fn an_interfaces_string_index_is_read_from_its_own_configuration_and_setting() {
        let mut raw = vec![18, 1];
        raw.resize(18, 0);
        raw.extend_from_slice(&[9, 2, 0, 0, 1, 1, 0, 0x80, 50]);
        raw.extend_from_slice(&[9, 4, 0, 0, 2, 0xFF, 0, 0, 7]);
        raw.extend_from_slice(&[7, 5, 0x81, 2, 64, 0, 0]);
        raw.extend_from_slice(&[9, 4, 0, 1, 2, 0xFF, 0, 0, 8]);
        raw.extend_from_slice(&[9, 2, 0, 0, 1, 2, 0, 0x80, 50]);
        raw.extend_from_slice(&[9, 4, 0, 0, 2, 0xFF, 0, 0, 9]);

        let index = |configuration, number, alternate| {
            super::described_interfaces(&raw, Some(configuration))
                .into_iter()
                .find(|interface| (interface.number, interface.alternate) == (number, alternate))
                .map(|interface| interface.string_index)
        };
        assert_eq!(index(1, 0, 0), Some(7));
        assert_eq!(index(1, 0, 1), Some(8));
        assert_eq!(index(2, 0, 0), Some(9));
        assert_eq!(index(1, 1, 0), None);
        assert_eq!(index(3, 0, 0), None);
    }
}
