//! Debug-probe discovery, selection, and capability negotiation -- the transport-neutral layer
//! ABOVE the wire protocols. It enumerates every attached probe across transports, opens one by
//! identity (picking the right interface when a probe is a composite USB device), negotiates its
//! `DAP_Info` capabilities, and hands back a connected [`lamella_cmsis_dap::Dap`].

use std::fmt;
use std::time::Duration;

use lamella_cmsis_dap::{Dap, Transport};
pub use lamella_cmsis_dap::{DapError, TransportError};

/// The read budget for one probe packet exchange. A slow probe-side helper still answers within it;
/// a genuinely absent reply surfaces as a timeout rather than a hang.
const READ_TIMEOUT: Duration = Duration::from_millis(1000);

/// The wire a probe speaks. CMSIS-DAP defines two; both are the same command protocol over a
/// different USB transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// CMSIS-DAP v1 over USB HID. Reports are classically 64 bytes but may be larger (the NXP
    /// MCU-Link uses 1024-byte HID reports); the real size is the negotiated packet size.
    CmsisDapV1Hid,
    /// CMSIS-DAP v2 over USB bulk (a WinUSB vendor interface).
    CmsisDapV2Bulk,
}

impl Wire {
    /// The short name a tool prints for this wire.
    ///
    /// Here rather than at each print site so a listing and an opened session cannot describe the
    /// same transport with two different words -- and because the transport is worth printing at
    /// all: a probe's product string is NOT a reliable stand-in for it. One physical board can
    /// present both wires, a listing may represent it by one while `open` resolves it to the
    /// other, and the difference is invisible unless something says which was used.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Wire::CmsisDapV1Hid => "hid-v1",
            Wire::CmsisDapV2Bulk => "bulk-v2",
        }
    }
}

/// A discovered probe interface -- the transport-neutral identity a caller selects on. One physical
/// probe yields one of these per interface, so a composite probe appears several times (all sharing
/// vid/pid/serial); [`open`] resolves the set down to the one interface that speaks DAP.
#[derive(Debug, Clone)]
pub struct ProbeInfo {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number, if the OS reported one. Shared across every interface of one physical probe.
    pub serial: Option<String>,
    /// Product string, if the OS reported one.
    pub product: Option<String>,
    /// This interface's own USB string (`iInterface`): empty when it is known to have none, and `None`
    /// when the OS did not say.
    pub interface_name: Option<String>,
    /// The transport this interface is reached over.
    pub wire: Wire,
    /// The top-level HID usage page (`None` for bulk, or where a backend does not parse it).
    /// CMSIS-DAP v1 is the vendor-defined page `0xFF00`.
    pub usage_page: Option<u16>,
    /// The top-level HID usage within [`usage_page`](Self::usage_page) (CMSIS-DAP v1: `0x01`).
    pub usage: Option<u16>,
    /// The OS-reported input report byte length, when known -- informational (the negotiated
    /// [`Caps::packet_size`] is authoritative). Classically 64/65 but larger on some probes.
    pub report_len: Option<u16>,
    /// The id Windows made from the device's place on the bus, where it has no serial. It tells two such
    /// devices apart, and it is not a serial: no interface of a composite device opens by it.
    port_id: Option<String>,
    /// How [`open`] reaches this exact interface again. Backend-specific; not part of the model.
    locator: Locator,
}

/// How [`open`] reopens a specific discovered interface.
#[derive(Debug, Clone)]
enum Locator {
    /// A specific HID interface, by its [`lamella_usbhid::DeviceInfo::id`].
    Hid { id: String },
    /// A v2 bulk probe, by vid/pid/serial (a dedicated vendor interface, so no per-interface id).
    #[cfg(feature = "usbbulk")]
    Bulk {
        vendor_id: u16,
        product_id: u16,
        serial: Option<String>,
    },
}

impl ProbeInfo {
    /// This interface as the three fields the selection ladder decides on.
    ///
    /// The rest of this struct -- transport, HID usage, the backend locator -- says how to REACH an
    /// interface, and none of it bears on WHICH PHYSICAL BOARD was meant. Keeping the two apart is
    /// what lets one ladder serve probe families whose discovery types have nothing else in common.
    #[must_use]
    pub fn as_candidate(&self) -> Candidate {
        Candidate {
            vendor_id: self.vendor_id,
            product_id: self.product_id,
            serial: self.serial.clone(),
        }
    }

    fn from_hid(d: lamella_usbhid::DeviceInfo) -> ProbeInfo {
        ProbeInfo {
            vendor_id: d.vendor_id,
            product_id: d.product_id,
            serial: d.serial_number,
            product: hid_row_name(d.device_name, d.product),
            interface_name: d.interface_name,
            wire: Wire::CmsisDapV1Hid,
            usage_page: d.usage_page,
            usage: d.usage,
            report_len: d.input_report_len,
            port_id: None,
            locator: Locator::Hid { id: d.id },
        }
    }

    #[cfg(feature = "usbbulk")]
    fn from_bulk(d: lamella_usbbulk::DeviceInfo) -> ProbeInfo {
        ProbeInfo {
            vendor_id: d.vendor_id,
            product_id: d.product_id,
            serial: d.serial_number.clone(),
            product: d.product,
            interface_name: d.interface_name,
            wire: Wire::CmsisDapV2Bulk,
            usage_page: None,
            usage: None,
            report_len: None,
            port_id: d.port_id,
            locator: Locator::Bulk {
                vendor_id: d.vendor_id,
                product_id: d.product_id,
                serial: d.serial_number,
            },
        }
    }

    fn spelling(&self) -> Spelling<'_> {
        Spelling {
            vendor_id: self.vendor_id,
            product_id: self.product_id,
            serial: self.serial.as_deref(),
            port_id: self.port_id.as_deref(),
        }
    }

    /// This device in the words a listing names it by.
    fn device_named(&self) -> String {
        device_named(self.vendor_id, self.product_id, self.serial.as_deref())
    }
}

/// What a probe reported about itself during [`open`], from CMSIS-DAP `DAP_Info`. This is the
/// negotiated half of the data model (the identity half is [`ProbeInfo`]).
#[derive(Debug, Clone)]
pub struct Caps {
    /// The CMSIS-DAP protocol version string (`DAP_Info` 0x04); empty if unreported.
    pub protocol_version: String,
    /// The probe's product string (`DAP_Info` 0x02); empty if unreported.
    pub product: String,
    /// The capabilities bitfield (`DAP_Info` 0xF0): bit 0 SWD, bit 1 JTAG, bit 2 SWO-UART, and so on.
    pub capabilities: u8,
    /// The maximum command packet size in bytes (`DAP_Info` 0xFF); 64 for a v1 HID probe.
    pub packet_size: u16,
}

impl Caps {
    /// Whether the probe supports SWD (capabilities bit 0).
    pub fn supports_swd(&self) -> bool {
        self.capabilities & 0x01 != 0
    }
    /// Whether the probe supports JTAG (capabilities bit 1).
    pub fn supports_jtag(&self) -> bool {
        self.capabilities & 0x02 != 0
    }
}

pub use lamella_probe_core::selection::{Candidate, PROBE_SERIAL_ENV, Selector};
use lamella_probe_core::selection;

/// An error discovering, opening, or negotiating with a probe.
///
/// **`non_exhaustive` because this enum just grew and will again.** Adding [`Self::Ambiguous`]
/// meant hand-checking every crate that depends on this one for an exhaustive `match`; there were
/// none, but establishing that cost a build and would have to be redone for the next variant. The
/// attribute makes that a non-event, and it is free today precisely because nothing matches
/// exhaustively yet -- which is the only moment it can be added without breaking someone.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProbeError {
    /// No probe matched the selector, or no matching interface answered `DAP_Info`.
    NotFound,
    /// More than one PHYSICAL probe matched, so which board was meant is not decidable.
    ///
    /// **Refusing is the whole point: the alternative is a successful write to somebody else's
    /// target.** Carries every matching probe's serial, because the fix is to name one and the
    /// message should not make the user go and look them up.
    Ambiguous(Vec<String>),
    /// The device could not be opened, so nothing is known of what it would have answered.
    Unopened {
        /// Which device, in the words a listing names it by.
        device: String,
        /// What the transport said.
        why: String,
    },
    /// The device opened, and its first exchange failed: it never answered as a CMSIS-DAP probe.
    ///
    /// **THIS IS NOT A PROBE THAT STOPPED.** It is what a device does that is no probe, and one another
    /// program holds, and a replug cures neither, so the words differ from those of a probe that had
    /// answered this session.
    NoAnswer {
        /// Which device, in the words a listing names it by.
        device: String,
        /// How the exchange failed.
        why: String,
    },
    /// The probe was found, but it exposes no CMSIS-DAP v1 HID interface to open.
    ///
    /// Distinct from [`ProbeError::NotFound`] deliberately: the probe IS there and IS named, so
    /// "no matching probe found" would send the reader looking for a cable or a serial. A v2
    /// bulk-only probe reaches this, and so does one whose HID interface disappeared across a
    /// firmware update.
    NoHidDapInterface {
        /// The probe that resolved, so the message can name it.
        serial: String,
    },
    /// A DAP operation failed.
    Dap(DapError),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::NotFound => write!(f, "no matching probe found"),
            ProbeError::Ambiguous(serials) => f.write_str(&lamella_probe_core::ambiguous_words(serials)),
            ProbeError::Unopened { device, why } => {
                write!(f, "{device} could not be opened ({why}){}.", lamella_probe_core::hint_after_a_refusal(why))
            }
            ProbeError::NoAnswer { device, why } => write!(
                f,
                "{device} did not answer as a CMSIS-DAP probe ({why}). It may not be a probe, or another \
                 program may hold it."
            ),
            ProbeError::NoHidDapInterface { serial } => write!(
                f,
                "probe {serial} exposes no CMSIS-DAP v1 HID interface -- it may be a v2 bulk-only \
                 probe, or its firmware may have dropped the HID interface"
            ),
            ProbeError::Dap(DapError::Transport(why)) => f.write_str(&lamella_probe_core::probe_stopped_answering(&why.0)),
            ProbeError::Dap(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ProbeError {}

impl From<DapError> for ProbeError {
    fn from(e: DapError) -> Self {
        ProbeError::Dap(e)
    }
}

/// A connected probe: which probe it is ([`info`](Self::info)), what it negotiated
/// ([`caps`](Self::caps)), and the debug port to drive the target through.
pub struct Session {
    /// The probe that was opened.
    pub info: ProbeInfo,
    /// The capabilities it reported.
    pub caps: Caps,
    dap: Dap<AnyTransport>,
}

impl Session {
    /// The connected debug port, for issuing SWD/ADIv5 transactions.
    pub fn dap(&mut self) -> &mut Dap<AnyTransport> {
        &mut self.dap
    }

    /// Consumes the session and returns the raw connected `Dap` -- e.g. to hand to a device
    /// extension crate's flash routine (`lamella-cmsis-dap-sam`, ...).
    pub fn into_dap(self) -> Dap<AnyTransport> {
        self.dap
    }
}

/// A [`Transport`] over whichever native USB backend reached a probe -- HID reports for CMSIS-DAP
/// v1, bulk pipes for CMSIS-DAP v2 -- so one [`Dap`] type drives either.
pub enum AnyTransport {
    /// A CMSIS-DAP v1 probe over USB HID.
    Hid(lamella_usbhid::Device),
    /// A CMSIS-DAP v2 probe over USB bulk.
    #[cfg(feature = "usbbulk")]
    Bulk(lamella_usbbulk::Device),
}

impl Transport for AnyTransport {
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
        match self {
            AnyTransport::Hid(d) => d
                .write_report(data)
                .map_err(|e| TransportError(e.to_string())),
            #[cfg(feature = "usbbulk")]
            AnyTransport::Bulk(d) => d
                .write_packet(data)
                .map_err(|e| TransportError(e.to_string())),
        }
    }

    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self {
            AnyTransport::Hid(d) => d
                .read_report(buf, READ_TIMEOUT)
                .map_err(|e| TransportError(e.to_string())),
            #[cfg(feature = "usbbulk")]
            AnyTransport::Bulk(d) => d
                .read_packet(buf, READ_TIMEOUT)
                .map_err(|e| TransportError(e.to_string())),
        }
    }
}

/// Resolves WHICH probe of a given vendor/product a tool means, and returns its serial.
///
/// **The same ladder [`Selector::from_environment`] states, for the tools that cannot use
/// [`open`].** A device-specific flash routine wants a `lamella_usbhid::Device` of its own rather
/// than a [`Session`], so it cannot go through `open` -- but it must not therefore go through
/// `Device::open(vid, pid, None)`, which resolves to whichever interface the OS happens to hand
/// over. This gives those callers the rung ladder and a serial to open by.
///
/// `requested` first, then [`PROBE_SERIAL_ENV`], then the sole attached probe, then
/// [`ProbeError::Ambiguous`] naming every candidate. **No rung guesses.**
///
/// **Why this exists at all: every micro:bit is `0d28:0204` and every RPi Debug Probe is
/// `2e8a:000c`.** On a bench with more than one, "the first match" is decided by USB enumeration
/// order, which changes with plug order and across reboots -- and writing to the wrong board does
/// not announce itself. The flash SUCCEEDS, on someone else's target, and they find it running a
/// program nobody sent it with no error in any log.
pub fn resolve_serial(
    vendor_id: u16,
    product_id: u16,
    requested: Option<&str>,
) -> Result<String, ProbeError> {
    let selector = Selector::named_or_environment(requested).with_vid_pid(vendor_id, product_id);
    let mut passed_over = Vec::new();
    let probes = list_reporting(&mut passed_over);
    choose_serial_among(&known_by_their_ids(&probes, &passed_over), &selector)
}

/// Everything a caller can reach by naming its exact ids: the probes, and the devices the filter passed
/// over.
///
/// **A CALLER NAMING THE EXACT IDS HAS SAID WHAT THE DEVICE IS**, so what the device calls itself has no
/// say. The filter reads names to find CMSIS-DAP among devices nobody named, and a WCH-Link in its
/// RISC-V mode is not CMSIS-DAP and is asked for by its ids.
fn known_by_their_ids(probes: &[ProbeInfo], passed_over: &[PassedOver]) -> Vec<Candidate> {
    probes
        .iter()
        .map(ProbeInfo::as_candidate)
        .chain(passed_over.iter().map(|device| Candidate {
            vendor_id: device.vendor_id,
            product_id: device.product_id,
            serial: device.serial.clone(),
        }))
        .collect()
}

/// The DECISION half of [`resolve_serial`], over an explicit probe list.
///
/// Split out because the rung that matters -- refusing when several boards match -- is the one a
/// test can only reach by supplying the probes, and a selection rule that has never been shown to
/// REFUSE has not been shown to do its job.
#[cfg(test)]
fn choose_serial(probes: &[ProbeInfo], selector: &Selector) -> Result<String, ProbeError> {
    choose_serial_among(&known_by_their_ids(probes, &[]), selector)
}

/// [`choose_serial`] over candidates.
fn choose_serial_among(candidates: &[Candidate], selector: &Selector) -> Result<String, ProbeError> {
    match selection::choose(candidates, selector) {
        selection::Selection::Unique(Some(serial)) => Ok(serial),
        selection::Selection::Unique(None) | selection::Selection::NotFound => {
            Err(ProbeError::NotFound)
        }
        selection::Selection::Ambiguous(names) => Err(ProbeError::Ambiguous(names)),
    }
}

/// Opens a CMSIS-DAP v1 (HID) probe of this vendor/product, resolved through [`resolve_serial`].
///
/// **For the tools that build their own [`Dap`] rather than taking a [`Session`]** -- a
/// device-specific flash or diagnostic routine wants a concrete transport it can hand to its own
/// chip-family code. That is a fair reason not to use [`open`]; it is not a reason to open by
/// vendor/product alone and take whichever board answers.
/// IT SELECTS THE INTERFACE, NOT ONLY THE PROBE, and that is the whole point of the second half.
/// [`resolve_serial`] settles WHICH PHYSICAL PROBE; a composite probe then puts several HID
/// interfaces on that one vid/pid/serial, and opening by vid/pid/serial takes whichever the OS
/// enumerates first. On an MCU-Link Pro that is `LPCSIO`, not CMSIS-DAP -- every DAP request then
/// times out and `ArmDap::connect` reports *"no SWD response -- is the probe wired to the target?"*,
/// so a HOST-SIDE MIS-SELECTION IS REPORTED AS AN UNREACHABLE BOARD. That is the expensive direction
/// to be wrong in: it sends someone to a bench to re-check wiring that was never the problem.
///
/// Ranked by [`selection_rank`] -- the SAME rule [`open`] uses -- so "which interface is the DAP
/// one" has one spelling in this crate rather than one per caller.
pub fn open_hid(
    vendor_id: u16,
    product_id: u16,
    requested: Option<&str>,
) -> Result<lamella_usbhid::Device, ProbeError> {
    let serial = resolve_serial(vendor_id, product_id, requested)?;
    let id = choose_hid_interface(&all_candidates(), vendor_id, product_id, &serial)?;
    lamella_usbhid::Device::open_id(&id)
        .map_err(|why| ProbeError::Unopened { device: device_named(vendor_id, product_id, Some(&serial)), why: why.to_string() })
}

/// A device in the words a listing names it by.
/// What a row calls a HID device: the device's own USB string where the OS read one, and otherwise what the
/// OS calls the product, which on a composite device on Windows is the interface's string.
fn hid_row_name(device_name: Option<String>, product: Option<String>) -> Option<String> {
    device_name.filter(|name| !name.is_empty()).or(product)
}

fn device_named(vendor_id: u16, product_id: u16, serial: Option<&str>) -> String {
    match serial {
        Some(serial) => format!("{vendor_id:04x}:{product_id:04x} serial {serial}"),
        None => format!("{vendor_id:04x}:{product_id:04x}"),
    }
}

/// Resolves a probe across SEVERAL product ids of one vendor, weighing ambiguity over the WHOLE set.
///
/// # Why a loop over [`open_hid`] is not this
///
/// [`resolve_serial`] narrows the population with `with_vid_pid`, so its refusal -- *several match,
/// so name one* -- only ever weighs probes of ONE product id against each other. A caller that
/// iterates product ids therefore evaluates each population in isolation, and the refusal cannot
/// fire across the set: with two of a vendor's debugger models attached and no serial given, the
/// first id that opens wins and the second board is never weighed against it. **That is a first
/// match, not a decision**, and one of the callers doing it WRITES FLASH.
///
/// The population here is every probe of `vendor_id` whose product id appears in `product_ids`, and
/// the ladder runs ONCE over all of it. A model that is not attached costs nothing, so a caller
/// passes every id it can drive rather than guessing which is present.
///
/// Returns the winning product id beside the serial, because the caller still has to open it --
/// through [`open_hid`], which stays the only place in this crate that opens a HID probe.
pub fn resolve_among(
    vendor_id: u16,
    product_ids: &[u16],
    requested: Option<&str>,
) -> Result<(u16, String), ProbeError> {
    choose_among(&list(), vendor_id, product_ids, &Selector::named_or_environment(requested))
}

/// The DECISION half of [`resolve_among`], over an explicit probe list.
///
/// Split out for the reason [`choose_serial`] is: the rung that matters is the REFUSAL, and a test
/// can only reach it by supplying the probes. A selection rule that has never been shown to refuse
/// has not been shown to do its job, and refusing across a product-id set is the case that is easy
/// to get wrong.
fn choose_among(
    probes: &[ProbeInfo],
    vendor_id: u16,
    product_ids: &[u16],
    selector: &Selector,
) -> Result<(u16, String), ProbeError> {
    let population: Vec<&ProbeInfo> = probes
        .iter()
        .filter(|p| p.vendor_id == vendor_id && product_ids.contains(&p.product_id))
        .collect();
    let candidates: Vec<Candidate> = population.iter().map(|p| p.as_candidate()).collect();
    match selection::choose(&candidates, selector) {
        selection::Selection::Unique(Some(serial)) => {
            let product_id = population
                .iter()
                .find(|p| {
                    p.serial.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&serial))
                })
                .map(|p| p.product_id)
                .ok_or(ProbeError::NotFound)?;
            Ok((product_id, serial))
        }
        selection::Selection::Unique(None) | selection::Selection::NotFound => {
            Err(ProbeError::NotFound)
        }
        selection::Selection::Ambiguous(names) => Err(ProbeError::Ambiguous(names)),
    }
}

/// The DECISION half of [`open_hid`]: which HID interface of one physical probe speaks DAP.
///
/// Split out for the same reason [`choose_serial`] is -- **the rung that matters can only be
/// reached by supplying the interfaces.** The probe this exists for is not always attached, and
/// a selection rule that has never been shown to pass over a decoy has not been shown to work.
fn choose_hid_interface(
    candidates: &[ProbeInfo],
    vendor_id: u16,
    product_id: u16,
    serial: &str,
) -> Result<String, ProbeError> {
    let mut interfaces: Vec<&ProbeInfo> = candidates
        .iter()
        .filter(|p| {
            matches!(p.wire, Wire::CmsisDapV1Hid)
                && p.vendor_id == vendor_id
                && p.product_id == product_id
                && p.serial.as_deref().is_some_and(|own| own.eq_ignore_ascii_case(serial))
        })
        .collect();
    interfaces.sort_by_key(|p| selection_rank(p));
    let best = interfaces.into_iter().find(|p| selection_rank(p) <= 2);
    match best.map(|p| &p.locator) {
        Some(Locator::Hid { id }) => Ok(id.clone()),
        _ => Err(ProbeError::NoHidDapInterface {
            serial: serial.to_owned(),
        }),
    }
}

/// [`open_hid`]'s CMSIS-DAP v2 sibling, over USB bulk -- the RPi Debug Probe and its kin.
///
/// **Every RPi Debug Probe enumerates as `2e8a:000c`.** Where more than one is attached, "the
/// first that answers" is decided by USB enumeration order, and the write that lands on the wrong
/// board SUCCEEDS.
#[cfg(feature = "usbbulk")]
pub fn open_bulk(
    vendor_id: u16,
    product_id: u16,
    requested: Option<&str>,
) -> Result<lamella_usbbulk::Device, ProbeError> {
    let serial = resolve_serial(vendor_id, product_id, requested)?;
    lamella_usbbulk::Device::open(vendor_id, product_id, Some(&serial))
        .map_err(|why| ProbeError::Unopened { device: device_named(vendor_id, product_id, Some(&serial)), why: why.to_string() })
}

/// Lists the discovered probes -- ONE entry per physical probe, so a composite probe (several HID
/// interfaces on one vid/pid/serial) appears once, represented by its DAP interface. HID is always
/// searched; USB bulk is searched when the `usbbulk` feature is enabled. Interfaces that do not look
/// like a probe (standard desktop HID) are filtered out; whether a candidate truly speaks DAP is
/// confirmed at [`open`] time. For the raw per-interface view, enumerate the transports directly.
pub fn list() -> Vec<ProbeInfo> {
    list_reporting(&mut Vec::new())
}

/// A vendor-class device the probe filter declined, kept so a listing can SHOW it.
///
/// **A count alone hands the reader a bucket they cannot see into.** Telling somebody whose probe
/// is missing that it is "among 3 other vendor-class devices" is only useful if they can find out
/// what those three are -- otherwise the sentence names a place to look and then refuses to open
/// it, which is worse than saying nothing.
///
/// [`product`](Self::product) and [`interface_name`](Self::interface_name) are what the filter read, so
/// they are what a reader needs: the token `CMSIS-DAP` is in neither where the specification wants it, or
/// the device's interfaces are not of the shape it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassedOver {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number, if the OS reported one.
    pub serial: Option<String>,
    /// Product string, if the OS reported one.
    pub product: Option<String>,
    /// The vendor-class interface's own name: empty when it is known to have none, and `None` when the OS
    /// did not say. With several, [`lamella_usbbulk::DeviceInfo::interface_name`] says which is shown.
    pub interface_name: Option<String>,
    /// The id Windows made from the device's place on the bus, where it has no serial.
    pub port_id: Option<String>,
}

impl PassedOver {
    fn spelling(&self) -> Spelling<'_> {
        Spelling {
            vendor_id: self.vendor_id,
            product_id: self.product_id,
            serial: self.serial.as_deref(),
            port_id: self.port_id.as_deref(),
        }
    }
}

impl fmt::Display for PassedOver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor_id, self.product_id)?;
        if let Some(serial) = &self.serial {
            write!(f, " serial {serial}")?;
        }
        write!(f, "  {}", self.product.as_deref().unwrap_or("(no product string)"))?;
        match &self.interface_name {
            Some(name) => write!(f, "  interface {name:?}"),
            None => f.write_str("  (the OS published no interface name)"),
        }
    }
}

/// [`list`], also reporting the vendor-class devices that were recognized as NOT probes.
///
/// **FOR A CALLER THAT PRINTS A LISTING, SO THE FILTER IS ACCOUNTABLE.** `lamella devices` is the
/// command someone runs when they are already confused, and the worst thing it can do is quietly
/// leave out the probe they are looking for. This lets it say what it passed over rather than
/// silently presenting a filtered list as the whole bus -- the same discipline the release gate's
/// known-gaps list applies to a red: keep it visible, just do not let it block.
///
/// # What it does NOT account for, stated rather than left to be discovered
///
/// **The vendor-bulk half, and the HID interfaces that have what a probe's has.** A HID interface
/// on a vendor usage page with long reports that [`is_cmsis_dap_v1`] rejects is recorded, because
/// its strings or its bus are all that kept it off the list. **A device already listed as a probe is not
/// recorded for another of its interfaces,** so a probe's other vendor interface, an EDBG's data gateway,
/// is not a row under "not debug probes" and does not count as a second device. Two exclusions sit
/// outside the record:
///
/// * Any other HID interface. Widening to them would put every attached keyboard and mouse into
///   the listing -- their hotkey and media-key collections carry vendor usage pages with 1-3 byte
///   reports, which is the case that filter exists for. The narrower widening is the vendor-page
///   interfaces rejected ONLY on report length: a probe with an unusual descriptor is among those,
///   and so is an unknown number of ordinary devices. That number decides whether the widening
///   helps or buries the listing, and it is measurable. A backend that reads no usage page
///   (Linux) records no HID interface for the same reason.
/// * Without the `usbbulk` feature there is no bulk enumeration at all, so this is always empty --
///   which reads identically to "nothing was passed over". `lamella-cli` and `lamella-flash-routes`
///   both enable it; a consumer that does not will get a silence it did not ask for.
pub fn list_reporting(passed_over: &mut Vec<PassedOver>) -> Vec<ProbeInfo> {
    one_row_per_device(all_candidates_reporting(passed_over))
}

/// One entry per physical device, each the most DAP-like interface it has.
///
/// **`same_device` DOES NOT CHAIN.** A device with no serial, over HID, is the same device as either of two
/// port ids, and those two are not one device. So a row keeps the port id of whichever of its interfaces
/// had one, whichever is its representative, and the second port id starts a second row, in any order.
fn one_row_per_device(candidates: impl IntoIterator<Item = ProbeInfo>) -> Vec<ProbeInfo> {
    let mut rows: Vec<ProbeInfo> = Vec::new();
    for candidate in candidates {
        match rows.iter_mut().find(|row| same_device(&row.spelling(), &candidate.spelling())) {
            Some(row) => {
                let port_id = row.port_id.take().or_else(|| candidate.port_id.clone());
                if selection_rank(&candidate) < selection_rank(row) {
                    *row = candidate;
                }
                row.port_id = port_id;
            }
            None => rows.push(candidate),
        }
    }
    rows
}

/// One device as one listing spells its identity.
struct Spelling<'a> {
    vendor_id: u16,
    product_id: u16,
    serial: Option<&'a str>,
    /// The id Windows made from the device's place on the bus, where it has no serial.
    port_id: Option<&'a str>,
}

/// Whether two listings' spellings are one physical device.
///
/// **THE SAME DEVICE CAN BE SPELLED TWO WAYS BY TWO TRANSPORTS.** One probe's two backends can differ in
/// case -- a micro:bit's DAPLink reports lowercase over HID and uppercase over its WebUSB bulk interface --
/// and on Windows a device with no serial is none over HID and a port-derived id over bulk. Read as two
/// devices, either would show a bench that does not exist and turn a sole probe ambiguous.
fn same_device(a: &Spelling<'_>, b: &Spelling<'_>) -> bool {
    a.vendor_id == b.vendor_id
        && a.product_id == b.product_id
        && match (a.serial, b.serial) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (None, None) => match (a.port_id, b.port_id) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => true,
            },
            _ => false,
        }
}

/// How many PHYSICAL probes `candidates` covers, named for a diagnostic -- one entry per distinct
/// `(vendor, product, serial)`, the same identity [`list`] dedupes on.
///
/// **Counting candidates instead would be wrong and would look right.** [`all_candidates`] yields
/// one entry per INTERFACE, so a single composite probe legitimately appears several times -- that
/// is exactly why [`open`] tries them in turn. An ambiguity check over candidates would refuse the
/// one-probe bench it is supposed to leave alone.
///
/// **And the limit of the check, stated rather than left to be discovered:** two physical probes
/// reporting the SAME serial are indistinguishable here and read as one composite probe, so no
/// refusal fires -- and no selector could have separated them either. It is a gap in what USB tells
/// us, not one in this function.
fn distinct_probes(candidates: &[ProbeInfo]) -> Vec<String> {
    one_row_per_device(candidates.iter().cloned())
        .iter()
        .flat_map(|row| selection::distinct_names(&[row.as_candidate()]))
        .collect()
}

/// Every probe-like interface across the native transports -- one entry per interface, so a
/// composite probe appears several times. The candidate set [`open`] selects from.
fn all_candidates() -> Vec<ProbeInfo> {
    all_candidates_reporting(&mut Vec::new())
}

/// [`all_candidates`], also recording the devices it passed over: one entry for each, however many
/// of its interfaces were declined.
///
/// **THE RECORD EXISTS SO A FILTER CANNOT HIDE A PROBE IN SILENCE.** Recognizing a probe means
/// declining to list something, and the thing this project keeps paying for is a listing that
/// omits what it could not account for and reports the survivors as the whole set. A list a caller
/// can print costs nothing and makes an unrecognized probe visible as a discrepancy instead of as
/// an absence.
fn all_candidates_reporting(passed_over: &mut Vec<PassedOver>) -> Vec<ProbeInfo> {
    let mut out = Vec::new();
    let mut declined = Vec::new();
    if let Ok(hids) = lamella_usbhid::enumerate() {
        for hid in hids {
            match hid_verdict(&hid) {
                HidVerdict::Probe => out.push(ProbeInfo::from_hid(hid)),
                HidVerdict::PassedOver => declined.push(PassedOver {
                    vendor_id: hid.vendor_id,
                    product_id: hid.product_id,
                    serial: hid.serial_number,
                    product: hid_row_name(hid.device_name, hid.product),
                    interface_name: hid.interface_name,
                    port_id: None,
                }),
                HidVerdict::NotOfInterest => {}
            }
        }
    }
    #[cfg(feature = "usbbulk")]
    if let Ok(bulk) = lamella_usbbulk::enumerate() {
        let mut under_the_link_guid = None;
        for device in bulk {
            let verdict = bulk_verdict(&device);
            let shown_as_a_board = verdict == BulkVerdict::Board
                && board_listing_shows(
                    &device,
                    under_the_link_guid
                        .get_or_insert_with(|| lamella_usbbulk::enumerate_interface(lamella_wire::usb::WINUSB_INTERFACE_GUID)),
                );
            match verdict {
                BulkVerdict::Probe => out.push(ProbeInfo::from_bulk(device)),
                BulkVerdict::Board if shown_as_a_board => {}
                BulkVerdict::Board | BulkVerdict::PassedOver => declined.push(PassedOver {
                    vendor_id: device.vendor_id,
                    product_id: device.product_id,
                    serial: device.serial_number,
                    product: device.product,
                    interface_name: device.interface_name,
                    port_id: device.port_id,
                }),
            }
        }
    }
    record_declined(passed_over, declined, &out);
    out
}

/// Records what the filter `declined`, leaving out what is already listed among `probes`: a device with
/// one interface declined and another listed is a probe, and it is one row.
fn record_declined(passed_over: &mut Vec<PassedOver>, declined: Vec<PassedOver>, probes: &[ProbeInfo]) {
    for device in declined {
        if !probes.iter().any(|probe| same_device(&probe.spelling(), &device.spelling())) {
            record_passed_over(passed_over, device);
        }
    }
}

/// Whether a listing of boards shows `device`, which has a Lamella Link's ids. On a platform with an
/// interface-GUID registry that listing is what is registered under the Link's GUID; on one without, it
/// is every device with a Link's ids (`under_the_guid` is [`lamella_usbbulk::Error::Unsupported`]).
///
/// **A DEVICE NOT SHOWN THERE STAYS IN THE RECORD**: with no driver bound, a Link is under no GUID, and
/// neither is whatever else has the shared vendor id, and a list that hides them all reports "not
/// attached" for a device one driver install away from working.
#[cfg(feature = "usbbulk")]
fn board_listing_shows(
    device: &lamella_usbbulk::DeviceInfo,
    under_the_guid: &lamella_usbbulk::Result<Vec<lamella_usbbulk::DeviceInfo>>,
) -> bool {
    match under_the_guid {
        Ok(boards) => boards.iter().any(|board| {
            board.vendor_id == device.vendor_id
                && board.product_id == device.product_id
                && match (&board.serial_number, &device.serial_number) {
                    (Some(listed), Some(found)) => listed.eq_ignore_ascii_case(found),
                    (listed, found) => listed == found,
                }
        }),
        Err(lamella_usbbulk::Error::Unsupported) => true,
        Err(_) => false,
    }
}

/// Adds `device` to the record, unless the record already holds that physical device: a composite
/// device can have several interfaces declined, and a listing counts devices.
fn record_passed_over(passed_over: &mut Vec<PassedOver>, device: PassedOver) {
    if !passed_over.iter().any(|held| same_device(&held.spelling(), &device.spelling())) {
        passed_over.push(device);
    }
}

/// What a HID interface is to a search for debug probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HidVerdict {
    /// Listed as a probe.
    Probe,
    /// Recorded as passed over: it has a vendor usage page and long reports, as a probe's interface
    /// has, but its strings or its bus say it is not one.
    PassedOver,
    /// Neither: a keyboard, a mouse, and whatever else is not on a vendor usage page with long reports.
    NotOfInterest,
}

/// Decides what a HID interface is to a search for debug probes.
fn hid_verdict(d: &lamella_usbhid::DeviceInfo) -> HidVerdict {
    if is_cmsis_dap_v1(d) {
        HidVerdict::Probe
    } else if d.usage_page.is_some_and(|page| page >= 0xFF00) && !has_short_reports(d) {
        HidVerdict::PassedOver
    } else {
        HidVerdict::NotOfInterest
    }
}

/// What a vendor-bulk device is to a search for debug probes.
#[cfg(feature = "usbbulk")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BulkVerdict {
    /// Listed as a probe: it names itself CMSIS-DAP, or a string that would settle it could not be read
    /// and what else is known fits one.
    Probe,
    /// Recorded as passed over: anything else, its interfaces being of another shape, its strings
    /// holding no token, or it having no strings at all.
    PassedOver,
    /// A board's own Lamella Link, which is neither: it is not a probe, and it belongs in a listing of
    /// boards. [`board_listing_shows`] says whether one shows it.
    Board,
}

/// Decides what `device` is to a search for debug probes, from the identity the OS reports.
///
/// **A BOARD'S OWN LAMELLA LINK NEVER REACHES THE FILTER'S UNCERTAIN ARMS.** Its vendor interface is
/// nameless, so read as a probe it would appear in every listing beside the debug probe wired to the
/// same board, and a write to it fails with a USB error. Its ids say what it is before its name is
/// asked. Only a device naming itself CMSIS-DAP outranks them, because one of the Link's two vendor
/// ids is a shared one, and a probe made under it is a probe.
///
/// **A string the OS would not give is not a string that is absent.** Where the OS said the device has
/// no such string, the specification's own test has failed, so the device is passed over, and a listing
/// can still show it. Where it did not say, weaker evidence decides ([`has_a_v2_interfaces_shape`]): a
/// probe must not vanish for want of a permission, and the less is known of a device the more it takes
/// for it to be sent DAP commands. A caller that must choose among several still has to name one, and a
/// sole one is opened and confirmed by `DAP_Info`.
#[cfg(feature = "usbbulk")]
fn bulk_verdict(device: &lamella_usbbulk::DeviceInfo) -> BulkVerdict {
    match is_cmsis_dap_v2(device) {
        Some(true) => BulkVerdict::Probe,
        _ if lamella_wire::usb::identify(device.vendor_id, device.product_id).is_some() => BulkVerdict::Board,
        Some(false) => BulkVerdict::PassedOver,
        None if has_a_v2_interfaces_shape(device) => BulkVerdict::Probe,
        None => BulkVerdict::PassedOver,
    }
}

/// Opens the probe chosen by `selector`, connecting to the target over SWD and negotiating the
/// probe's capabilities. When several interfaces match (a composite probe), each is tried in
/// decreasing DAP likelihood and confirmed by `DAP_Info`, so the real DAP interface is selected --
/// and, in the common case, the sibling interfaces are never touched.
///
/// The returned [`Session`] is connected (`DAP_Connect` + line reset done) and ready for transactions
/// such as [`lamella_cmsis_dap::Dap::read_idcode`].
pub fn open(selector: &Selector) -> Result<Session, ProbeError> {
    let mut candidates: Vec<ProbeInfo> =
        all_candidates().into_iter().filter(|p| selector.matches(&p.as_candidate())).collect();
    if candidates.is_empty() {
        return Err(ProbeError::NotFound);
    }
    let matched = distinct_probes(&candidates);
    if matched.len() > 1 {
        return Err(ProbeError::Ambiguous(matched));
    }
    candidates.sort_by_key(selection_rank);
    first_that_answers(candidates, |info| {
        let mut dap = open_dap(&info)?;
        let caps = confirm_and_connect(&info, &mut dap)?;
        Ok(Session { info, caps, dap })
    })
}

/// What `attempt` gives for the first of `candidates` it succeeds on, or the most specific of their
/// failures.
///
/// **THE LAST FAILURE IS NOT THE ONE WORTH SAYING.** On a composite probe whose DAP interface reports a
/// busy device and whose sibling then times out, the last word would hide what was wrong with the
/// interface that mattered. [`how_specific`] says which to keep, and among equals the first stands, since
/// candidates are tried most likely first.
fn first_that_answers<C, T>(
    candidates: impl IntoIterator<Item = C>,
    mut attempt: impl FnMut(C) -> Result<T, ProbeError>,
) -> Result<T, ProbeError> {
    let mut kept = ProbeError::NotFound;
    for candidate in candidates {
        match attempt(candidate) {
            Ok(done) => return Ok(done),
            Err(failure) if how_specific(&failure) < how_specific(&kept) => kept = failure,
            Err(_) => {}
        }
    }
    Err(kept)
}

/// How much a failure says of the device it came from; the lower says more.
fn how_specific(failure: &ProbeError) -> u8 {
    match failure {
        ProbeError::Dap(_) => 0,
        ProbeError::Unopened { .. } => 1,
        ProbeError::NoAnswer { .. } => 2,
        _ => 3,
    }
}

/// Whether a vendor-bulk device has a CMSIS-DAP **v2** interface -- `Some(false)` for a settled no, and
/// `None` where what would settle it could not be read.
///
/// **THE TEST IS THE SPECIFICATION'S OWN.** A v2 interface is of class `0xFF`, subclass and protocol zero,
/// with a bulk pair, and the token `CMSIS-DAP` is in the product string, or in the interface string of a
/// composite device. So the test recognizes a probe nobody here has heard of -- which a vendor-id
/// allowlist cannot do, and the difference matters more than it looks: an unlisted vendor's probe would
/// not be mis-listed, it would be ABSENT, and a listing that omits a probe is indistinguishable from a
/// bench that has none. A shape or a pair the OS did not state is not held against an interface, since on
/// Windows a pair cannot be known without an open.
///
/// **ONE EXCLUSION IS A CHOICE OF OURS AND NOT THE SPECIFICATION'S.** On a composite device the letter of
/// the specification lets the product string carry the token for every interface. Here it speaks only for
/// an interface with no name of its own: an interface naming itself something else says what it is for,
/// as an EDBG's "EDBG Data Gateway" does under the product "EDBG CMSIS-DAP", and it is passed over.
///
/// **THE THREE-VALUED RETURN IS THE LOAD-BEARING PART.** A string known to be absent is a no, and a string
/// the OS could not read has said nothing, so folding it into `false` would disqualify a probe for want
/// of a permission. `None` means "cannot tell", and [`bulk_verdict`] takes the weaker evidence there.
///
/// A Lamella Link board is vendor-class and is deliberately NOT matched here. It is not a CMSIS-DAP
/// probe, and it is found by its Lamella vendor id instead -- which is a sound rule for a device
/// this project defines, and would not be one for somebody else's. [`bulk_verdict`] applies it.
#[cfg(feature = "usbbulk")]
fn is_cmsis_dap_v2(d: &lamella_usbbulk::DeviceInfo) -> Option<bool> {
    if d.vendor_interfaces.is_empty() {
        return None;
    }
    let shaped: Vec<&lamella_usbbulk::VendorInterface> = d
        .vendor_interfaces
        .iter()
        .filter(|interface| {
            interface.subclass_protocol.is_none_or(|shape| shape == (0, 0)) && interface.bulk_pair != Some(false)
        })
        .collect();
    if shaped.is_empty() {
        return Some(false);
    }
    if shaped.iter().filter_map(|interface| interface.name.as_deref()).any(names_cmsis_dap) {
        return Some(true);
    }
    let product_names_it = d.product.as_deref().is_some_and(names_cmsis_dap);
    let product_settled = d.product.is_some() || d.product_is_absent;
    let by_the_product = || match (product_names_it, product_settled) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
    };
    if d.composite == Some(false) {
        return by_the_product();
    }
    let has_no_name = shaped.iter().any(|interface| interface.name.as_deref() == Some(""));
    let unread = shaped.iter().any(|interface| interface.name.is_none());
    match (has_no_name, unread) {
        (true, _) if product_names_it => Some(true),
        (_, true) => None,
        (true, false) => by_the_product(),
        (false, false) => Some(false),
    }
}

/// What is left to go on for a vendor-bulk device [`is_cmsis_dap_v2`] cannot settle, a string it needs
/// being unreadable: Windows' registration under the interface class the specification gives a v2 probe;
/// and with no such registry, an interface of the shape the specification requires, with a bulk pair not
/// known to be missing.
#[cfg(feature = "usbbulk")]
fn has_a_v2_interfaces_shape(d: &lamella_usbbulk::DeviceInfo) -> bool {
    d.vendor_interfaces.iter().any(|interface| {
        interface.spec_guid.unwrap_or(interface.subclass_protocol == Some((0, 0)) && interface.bulk_pair != Some(false))
    })
}

/// Whether a HID interface is a CMSIS-DAP **v1** interface -- the transport's own signature, not a
/// guess about who made the device.
///
/// **NAMED FOR THE SPECIFICATION IT TESTS RATHER THAN AS A HEDGE.** A name that only claims
/// resemblance reads as a heuristic somebody may tighten with a vendor list, and a vendor list is
/// hand-kept -- so a probe from a vendor nobody wrote down becomes invisible, which is the failure
/// this crate exists to prevent, arriving through the door marked "tidy the listing".
///
/// Its v2 sibling is [`is_cmsis_dap_v2`]. The two answer the same question on the two transports,
/// and they are named so that a reader looking for one finds the other.
///
/// **THE TOKEN IS LOOKED FOR WHERE THE SPECIFICATION PUTS IT: THE PRODUCT STRING, OR THE INTERFACE
/// STRING OF A COMPOSITE DEVICE.** Which of those a HID backend's `product` holds differs by OS, so
/// every string the OS can read is asked, and a device naming itself in any of them is a probe.
///
/// **NO TOKEN IS A SETTLED NO WHEN EVERY STRING WAS READ, AND THAT INCLUDES A DEVICE WHOSE STRINGS ARE ALL
/// KNOWN TO BE ABSENT:** the specification requires the token, so such a device cannot be a probe. A string
/// the OS could not give may be the one holding it, and a real probe lost is worse than a device wrongly
/// listed, so then the usage page decides, and that is as much as is known. And CMSIS-DAP runs over USB,
/// so a device that names no probe and is on another bus is not one, which is what a laptop's touch
/// controller is, with its vendor page and long reports.
fn is_cmsis_dap_v1(d: &lamella_usbhid::DeviceInfo) -> bool {
    if has_short_reports(d) {
        return false;
    }
    let strings = [d.product.as_deref(), d.interface_name.as_deref(), d.device_name.as_deref()];
    if strings.iter().flatten().any(|name| names_cmsis_dap(name)) {
        return true;
    }
    if d.on_usb == Some(false) {
        return false;
    }
    let every_string_read = d.interface_name.is_some() && d.device_name.is_some();
    if every_string_read {
        return false;
    }
    d.usage_page.is_some_and(|page| page >= 0xFF00)
}

/// Whether a HID interface's reports are too short to carry a CMSIS-DAP command. A CMSIS-DAP interface
/// carries reports of 64 bytes or more, and keyboards and mice expose vendor usage pages for hotkeys and
/// media keys with reports of 1 to 3 bytes.
fn has_short_reports(d: &lamella_usbhid::DeviceInfo) -> bool {
    matches!(d.input_report_len, Some(len) if len < 63)
}

/// Whether a USB string contains the token the CMSIS-DAP specification requires of a probe.
///
/// Case-insensitive and a substring: the specification fixes the token and not the text around it,
/// and real probes wrap it ("CMSIS-DAP v2 Interface", "BBC micro:bit CMSIS-DAP"). The separator is
/// matched loosely, because some platforms sanitize punctuation out of descriptor strings ("Debug
/// Probe _CMSIS_DAP_").
fn names_cmsis_dap(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("CMSIS-DAP") || upper.contains("CMSIS_DAP")
}

/// Ranks a candidate by how likely it is the real DAP interface (lower is tried first), so the
/// genuine DAP interface of a composite probe wins before any sibling interface is poked.
///
/// The decisive signal is the CANONICAL CMSIS-DAP v1 HID usage -- vendor-defined usage page `0xFF00`,
/// usage `0x01`: the MCU-Link Pro puts its DAP interface there, while its LPCSIO bridge and
/// trace interfaces sit on `0xFFEA`/`0xFFEB`. Report length is NOT a reliable
/// signal (that DAP interface uses 1024-byte reports, its LPCSIO uses 64).
fn selection_rank(p: &ProbeInfo) -> u8 {
    let Wire::CmsisDapV1Hid = p.wire else {
        return 0;
    };
    let canonical = p.usage_page == Some(0xff00) && p.usage == Some(0x01);
    let own_name = p.interface_name.as_deref().filter(|name| !name.is_empty()).or(p.product.as_deref());
    let named = own_name.is_some_and(names_cmsis_dap);
    match (canonical, named) {
        (true, _) => 1,
        (false, true) => 2,
        _ => 3,
    }
}

/// Opens the transport for one candidate and wraps it in a `Dap` (not yet connected).
fn open_dap(info: &ProbeInfo) -> Result<Dap<AnyTransport>, ProbeError> {
    let unopened = |why: String| ProbeError::Unopened { device: info.device_named(), why };
    let transport = match &info.locator {
        Locator::Hid { id } => {
            AnyTransport::Hid(lamella_usbhid::Device::open_id(id).map_err(|why| unopened(why.to_string()))?)
        }
        #[cfg(feature = "usbbulk")]
        Locator::Bulk {
            vendor_id,
            product_id,
            serial,
        } => AnyTransport::Bulk(
            lamella_usbbulk::Device::open(*vendor_id, *product_id, serial.as_deref())
                .map_err(|why| unopened(why.to_string()))?,
        ),
    };
    Ok(Dap::new(transport))
}

/// Confirms a candidate really speaks DAP (via `DAP_Info`), reads its capabilities, and connects it
/// to the target over SWD. An interface that is not the DAP one fails the `DAP_Info` litmus here and
/// is rejected without connecting.
fn confirm_and_connect(info: &ProbeInfo, dap: &mut Dap<AnyTransport>) -> Result<Caps, ProbeError> {
    let capabilities = dap
        .info_bytes(0xF0)
        .map_err(|why| ProbeError::NoAnswer { device: info.device_named(), why: first_exchange_failure(&why) })?
        .first()
        .copied()
        .unwrap_or(0);
    let packet_size = dap
        .info_bytes(0xFF)
        .ok()
        .filter(|b| b.len() >= 2)
        .map_or(64, |b| u16::from_le_bytes([b[0], b[1]]));
    let caps = Caps {
        protocol_version: dap.info_string(0x04).unwrap_or_default(),
        product: dap.info_string(0x02).unwrap_or_default(),
        capabilities,
        packet_size,
    };
    dap.connect_swd()?;
    Ok(caps)
}

/// How a device's first exchange failed, in the words a reader gets, with the transport's prefix left off.
fn first_exchange_failure(why: &DapError) -> String {
    match why {
        DapError::Transport(transport) => transport.0.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device the rule named, as a failure says it.
    const A_DEVICE: &str = "1234:5678 serial ABC";

    fn unopened(why: &str) -> ProbeError {
        ProbeError::Unopened { device: A_DEVICE.to_owned(), why: why.to_owned() }
    }

    fn no_answer() -> ProbeError {
        ProbeError::NoAnswer { device: A_DEVICE.to_owned(), why: "USB transfer timed out".to_owned() }
    }

    /// **THE MOST SPECIFIC FAILURE IS KEPT, WHICHEVER CAME LAST,** so a busy interface is not hidden by its
    /// sibling's timeout, and the first that works ends the search.
    #[test]
    fn the_most_specific_failure_is_kept_whichever_came_last() {
        let of = |failures: Vec<ProbeError>| first_that_answers::<_, ()>(failures, Err).unwrap_err().to_string();
        let busy = "could not be opened (busy)";
        assert!(of(vec![unopened("busy"), no_answer()]).contains(busy));
        assert!(of(vec![no_answer(), unopened("busy")]).contains(busy));
        assert!(of(vec![no_answer(), no_answer()]).contains("did not answer"));
        assert!(of(vec![unopened("first"), unopened("second")]).contains("first"), "among equals the first stands");
        let answered = || ProbeError::Dap(DapError::Timeout("the target"));
        assert!(of(vec![unopened("busy"), answered()]).contains("the target"), "it answered, so it was the one");
        assert_eq!(of(Vec::new()), ProbeError::NotFound.to_string());
        let mut tried = 0;
        let worked = first_that_answers([1, 2, 3], |candidate| {
            tried += 1;
            if candidate == 2 { Ok(candidate) } else { Err(no_answer()) }
        });
        assert_eq!(worked.unwrap(), 2);
        assert_eq!(tried, 2, "nothing past the first that answers is touched");
    }

    /// **A DEVICE THAT NEVER ANSWERED IS NOT "A PROBE THAT STOPPED":** it is named, with what it may be, and
    /// not sent to a cable; a probe that answered and then fell silent is.
    #[test]
    fn a_device_that_never_answered_is_named_and_not_sent_to_a_cable() {
        let said = no_answer().to_string();
        assert!(said.contains(A_DEVICE), "{said}");
        assert!(said.contains("did not answer as a CMSIS-DAP probe"), "{said}");
        assert!(said.contains("may not be a probe") && said.contains("another program may hold it"), "{said}");
        assert!(!said.contains("Unplug"), "{said}");
        let stopped = ProbeError::Dap(DapError::Transport(TransportError("USB transfer timed out".to_owned()))).to_string();
        assert!(stopped.contains("stopped answering") && stopped.contains("Unplug"), "{stopped}");
        let said = unopened("Access is denied").to_string();
        assert!(said.contains(A_DEVICE) && said.contains("Access is denied"), "{said}");
        assert!(!said.contains("Unplug"), "{said}");
    }

    fn hid_probe(
        usage_page: Option<u16>,
        usage: Option<u16>,
        report_len: Option<u16>,
        product: Option<&str>,
    ) -> ProbeInfo {
        ProbeInfo {
            vendor_id: 0x1fc9,
            product_id: 0x0143,
            serial: Some("PROBE0000001".into()),
            product: product.map(Into::into),
            interface_name: None,
            wire: Wire::CmsisDapV1Hid,
            usage_page,
            usage,
            report_len,
            port_id: None,
            locator: Locator::Hid { id: "path".into() },
        }
    }

    /// A probe with a chosen serial, everything else fixed.
    fn probe_with_serial(serial: Option<&str>) -> ProbeInfo {
        ProbeInfo {
            serial: serial.map(Into::into),
            ..hid_probe(Some(0xff00), Some(0x01), Some(65), Some("CMSIS-DAP"))
        }
    }

    /// A micro:bit-shaped probe: every one of them carries the SAME vid/pid, which is the whole
    /// reason a serial has to decide.
    fn microbit(serial: &str) -> ProbeInfo {
        ProbeInfo {
            vendor_id: 0x0d28,
            product_id: 0x0204,
            serial: Some(serial.into()),
            ..hid_probe(Some(0xff00), Some(0x01), Some(65), Some("DAPLink"))
        }
    }

    /// One interface of the MCU-Link Pro, identified the way the OS identifies it.
    fn mcu_link_interface(usage_page: u16, product: &str, id: &str) -> ProbeInfo {
        ProbeInfo {
            locator: Locator::Hid { id: id.into() },
            ..hid_probe(Some(usage_page), Some(0x01), Some(1024), Some(product))
        }
    }

    /// A probe that puts several vendor-defined HID interfaces on ONE vid/pid/serial. Opening by
    /// vid/pid/serial takes whichever the OS enumerates first, and every DAP request then times
    /// out, which `ArmDap::connect` reports as "no SWD response -- is the probe wired to the
    /// target?". **A host-side mis-selection presenting as an unreachable board.**
    #[test]
    fn the_dap_interface_wins_over_a_probes_other_hid_interfaces() {
        let interfaces = [
            mcu_link_interface(0xffea, "LPCSIO", "path-lpcsio"),
            mcu_link_interface(0xffeb, "MCU-LINK NXP TRACE/POWER", "path-trace"),
            mcu_link_interface(0xff00, "MCU-LINK Pro CMSIS-DAP", "path-dap"),
        ];
        let chosen = choose_hid_interface(&interfaces, 0x1fc9, 0x0143, "PROBE0000001");
        assert_eq!(
            chosen.unwrap(),
            "path-dap",
            "the CMSIS-DAP interface must win regardless of enumeration order"
        );
    }

    /// A probe that resolved but has no HID DAP interface must SAY SO rather than report itself
    /// missing -- a v2 bulk-only probe, or one whose HID interface vanished across a firmware
    /// update. "No matching probe found" would send the reader looking for a cable.
    ///
    /// The MCU-Link Pro at firmware V3.172 is exactly this: bulk-only DAP, and its LPCSIO and
    /// TRACE/POWER HID interfaces still enumerate. Opening one of THOSE is what produced the
    /// original false finding, so "some HID interface exists" must not count as a DAP interface.
    #[test]
    fn a_probe_with_no_dap_interface_is_named_not_reported_missing() {
        let interfaces = [
            mcu_link_interface(0xffea, "LPCSIO", "path-lpcsio"),
            mcu_link_interface(0xffeb, "MCU-LINK NXP TRACE/POWER", "path-trace"),
        ];
        let err = choose_hid_interface(&interfaces, 0x1fc9, 0x0143, "PROBE0000001")
            .expect_err("a probe with only a decoy interface must not open the decoy");
        let ProbeError::NoHidDapInterface { serial } = err else {
            panic!("expected NoHidDapInterface, got {err:?}")
        };
        assert_eq!(serial, "PROBE0000001", "the refusal must name the probe");
    }

    /// A probe's two transports can spell its serial in two cases, and the serial a caller resolved may be
    /// the bulk interface's: the HID interface is still that probe's.
    #[test]
    fn a_hid_interface_is_found_by_a_serial_spelled_in_the_other_case() {
        let interfaces = [mcu_link_interface(0xff00, "MCU-LINK Pro CMSIS-DAP", "path-dap")];
        let chosen = choose_hid_interface(&interfaces, 0x1fc9, 0x0143, "probe0000001");
        assert_eq!(chosen.unwrap(), "path-dap");
    }

    #[test]
    fn one_attached_board_needs_no_serial() {
        let chosen = choose_serial(
            &[microbit("b0ard0001")],
            &Selector::any().with_vid_pid(0x0d28, 0x0204),
        );
        assert_eq!(chosen.unwrap(), "b0ard0001", "a one-board bench must be unaffected");
    }

    #[test]
    fn two_alike_boards_are_refused_rather_than_guessed_between() {
        let err = choose_serial(
            &[microbit("b0ard0001"), microbit("b0ard0002")],
            &Selector::any().with_vid_pid(0x0d28, 0x0204),
        )
        .unwrap_err();
        let ProbeError::Ambiguous(named) = err else { panic!("expected a refusal, got {err:?}") };
        assert_eq!(named.len(), 2, "the refusal must NAME the candidates, not just count them");
    }

    #[test]
    fn a_serial_picks_its_board_out_of_several() {
        let chosen = choose_serial(
            &[microbit("b0ard0001"), microbit("b0ard0002")],
            &Selector::by_serial("b0ard0002").with_vid_pid(0x0d28, 0x0204),
        );
        assert_eq!(chosen.unwrap(), "b0ard0002");
    }

    /// A Microchip on-board debugger. The EDBG (`0x2111`) and the Curiosity Nano's nEDBG
    /// (`0x2175`) are DIFFERENT product ids driven by the SAME tools, which is what makes the
    /// product-id set the population rather than one id at a time.
    fn microchip(product_id: u16, serial: &str) -> ProbeInfo {
        ProbeInfo {
            vendor_id: 0x03eb,
            product_id,
            serial: Some(serial.into()),
            ..hid_probe(Some(0xff00), Some(0x01), Some(65), Some("CMSIS-DAP"))
        }
    }

    const MICROCHIP_DEBUGGERS: [u16; 2] = [0x2111, 0x2175];

    #[test]
    fn two_models_of_one_vendor_are_refused_rather_than_first_match() {
        let err = choose_among(
            &[microchip(0x2111, "ATML0000000000000002"), microchip(0x2175, "MC000000000AAA000001")],
            0x03eb,
            &MICROCHIP_DEBUGGERS,
            &Selector::any(),
        )
        .unwrap_err();
        let ProbeError::Ambiguous(named) = err else { panic!("expected a refusal, got {err:?}") };
        assert_eq!(named.len(), 2, "the refusal must NAME both models, not just count them");
    }

    #[test]
    fn a_refusal_shows_every_candidate_and_not_just_the_first() {
        let err = choose_among(
            &[microchip(0x2111, "ATML0000000000000002"), microchip(0x2175, "MC000000000AAA000001")],
            0x03eb,
            &MICROCHIP_DEBUGGERS,
            &Selector::any(),
        )
        .unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("ATML0000000000000002") && rendered.contains("MC000000000AAA000001"),
            "the refusal must show BOTH candidates, got: {rendered}"
        );
    }

    #[test]
    fn the_single_id_rung_cannot_see_the_other_model_which_is_why_resolve_among_exists() {
        let mixed =
            [microchip(0x2111, "ATML0000000000000002"), microchip(0x2175, "MC000000000AAA000001")];

        assert_eq!(
            choose_serial(&mixed, &Selector::any().with_vid_pid(0x03eb, 0x2111)).unwrap(),
            "ATML0000000000000002",
            "the single-id rung answers uniquely here, which is the defect and not a feature"
        );

        assert!(
            matches!(
                choose_among(&mixed, 0x03eb, &MICROCHIP_DEBUGGERS, &Selector::any()),
                Err(ProbeError::Ambiguous(_))
            ),
            "over the whole product-id set the same bench must refuse"
        );
    }

    #[test]
    fn a_serial_picks_its_model_out_of_the_set() {
        let (product_id, serial) = choose_among(
            &[microchip(0x2111, "ATML0000000000000002"), microchip(0x2175, "MC000000000AAA000001")],
            0x03eb,
            &MICROCHIP_DEBUGGERS,
            &Selector::by_serial("MC000000000AAA000001"),
        )
        .expect("a named probe resolves out of a mixed set");
        assert_eq!(serial, "MC000000000AAA000001");
        assert_eq!(product_id, 0x2175, "the WINNING product id must come back, not the first tried");
    }

    #[test]
    fn a_sole_model_of_the_set_needs_no_serial() {
        let (product_id, serial) = choose_among(
            &[microchip(0x2175, "MC000000000AAA000001")],
            0x03eb,
            &MICROCHIP_DEBUGGERS,
            &Selector::any(),
        )
        .expect("a sole attached debugger needs no serial");
        assert_eq!((product_id, serial.as_str()), (0x2175, "MC000000000AAA000001"));
    }

    #[test]
    fn a_probe_outside_the_set_does_not_count_toward_ambiguity() {
        let (_, serial) = choose_among(
            &[microchip(0x2175, "MC000000000AAA000001"), microbit("b0ard0001")],
            0x03eb,
            &MICROCHIP_DEBUGGERS,
            &Selector::any(),
        )
        .expect("an unrelated probe must not make a sole debugger ambiguous");
        assert_eq!(serial, "MC000000000AAA000001");
    }

    #[test]
    fn a_vendor_id_outside_the_set_is_not_found_rather_than_taken() {
        let err = choose_among(&[microbit("b0ard0001")], 0x03eb, &MICROCHIP_DEBUGGERS, &Selector::any())
            .unwrap_err();
        assert!(
            matches!(err, ProbeError::NotFound),
            "a bench with none of the named models must report NotFound, got {err:?}"
        );
    }

    #[test]
    fn a_sibling_model_does_not_count_toward_ambiguity() {
        let chosen = choose_serial(
            &[microbit("b0ard0001"), probe_with_serial(Some("PROBE0000001"))],
            &Selector::any().with_vid_pid(0x0d28, 0x0204),
        );
        assert_eq!(chosen.unwrap(), "b0ard0001");
    }

    #[test]
    fn ambiguity_is_counted_over_physical_probes_and_not_over_interfaces() {
        let many = [
            probe_with_serial(Some("AAAA")),
            probe_with_serial(Some("BBBB")),
            probe_with_serial(Some("CCCC")),
        ];
        assert_eq!(distinct_probes(&many), ["AAAA", "BBBB", "CCCC"]);

        let one_composite = [
            probe_with_serial(Some("AAAA")),
            probe_with_serial(Some("AAAA")),
            probe_with_serial(Some("AAAA")),
        ];
        assert_eq!(distinct_probes(&one_composite), ["AAAA"]);
        assert_eq!(distinct_probes(&many[..1]), ["AAAA"]);

        assert_eq!(distinct_probes(&[probe_with_serial(None)]), ["1fc9:0143 (no serial)"]);
    }

    #[test]
    fn the_selector_ladder_prefers_the_environment_over_guessing() {
        assert_eq!(Selector::by_serial("EXPLICIT").serial.as_deref(), Some("EXPLICIT"));
        unsafe { std::env::set_var(PROBE_SERIAL_ENV, "FROM-ENV") };
        assert_eq!(Selector::from_environment().serial.as_deref(), Some("FROM-ENV"));
        unsafe { std::env::set_var(PROBE_SERIAL_ENV, "   ") };
        assert_eq!(Selector::from_environment().serial, None);
        unsafe { std::env::remove_var(PROBE_SERIAL_ENV) };
        assert_eq!(Selector::from_environment().serial, None);
    }

    #[cfg(feature = "usbbulk")]
    #[test]
    fn the_v2_rule_reads_the_specifications_own_string_and_says_when_it_cannot_tell() {
        let device = |name: Option<&str>| lamella_usbbulk::DeviceInfo {
            vendor_id: 0x2e8a,
            product_id: 0x000c,
            serial_number: Some("SERIAL0000000001".to_owned()),
            product: Some("Debug Probe".to_owned()),
            interface_name: name.map(str::to_owned),
            vendor_interfaces: vec![spec_interface(name)],
            composite: Some(true),
            ..lamella_usbbulk::DeviceInfo::default()
        };
        assert_eq!(is_cmsis_dap_v2(&device(Some("CMSIS-DAP v2 Interface"))), Some(true));
        assert_eq!(is_cmsis_dap_v2(&device(Some("Probe _CMSIS_DAP_"))), Some(true));
        assert_eq!(is_cmsis_dap_v2(&device(Some("Vendor Data Interface"))), Some(false));
        assert_eq!(is_cmsis_dap_v2(&device(None)), None);
        assert_eq!(is_cmsis_dap_v2(&device(Some(""))), Some(false));
    }

    /// A vendor interface of the shape the specification requires, and with no word on its pair.
    #[cfg(feature = "usbbulk")]
    fn spec_interface(name: Option<&str>) -> lamella_usbbulk::VendorInterface {
        lamella_usbbulk::VendorInterface {
            subclass_protocol: Some((0, 0)),
            name: name.map(str::to_owned),
            bulk_pair: None,
            spec_guid: None,
        }
    }

    /// One device of each kind, with a made-up serial: the verdict reads the ids, the interface and the
    /// strings, and with no product named, the product is known to be absent.
    #[cfg(feature = "usbbulk")]
    fn bulk_device(vendor_id: u16, product_id: u16, name: Option<&str>) -> lamella_usbbulk::DeviceInfo {
        lamella_usbbulk::DeviceInfo {
            vendor_id,
            product_id,
            serial_number: Some("SERIAL0000000002".to_owned()),
            product: None,
            interface_name: name.map(str::to_owned),
            vendor_interfaces: vec![spec_interface(name)],
            composite: Some(true),
            product_is_absent: true,
            port_id: None,
        }
    }

    /// A vendor-class device, with what the OS says of its interfaces and strings.
    #[cfg(feature = "usbbulk")]
    fn described(
        interfaces: Vec<lamella_usbbulk::VendorInterface>,
        composite: Option<bool>,
        product: Option<&str>,
    ) -> lamella_usbbulk::DeviceInfo {
        lamella_usbbulk::DeviceInfo {
            vendor_id: 0x0403,
            product_id: 0x6001,
            serial_number: Some("SERIAL0000000003".to_owned()),
            product: product.map(str::to_owned),
            vendor_interfaces: interfaces,
            composite,
            ..lamella_usbbulk::DeviceInfo::default()
        }
    }

    /// A vendor interface, with what the OS says of it.
    #[cfg(feature = "usbbulk")]
    fn interface(
        subclass_protocol: Option<(u8, u8)>,
        name: Option<&str>,
        bulk_pair: Option<bool>,
        spec_guid: Option<bool>,
    ) -> lamella_usbbulk::VendorInterface {
        lamella_usbbulk::VendorInterface { subclass_protocol, name: name.map(str::to_owned), bulk_pair, spec_guid }
    }

    /// **WINDOWS, AND A VENDOR-CLASS DEVICE WITH NO STRINGS IS NOT A PROBE.** A device of one interface
    /// has no name but its product string, and a node holding no such string is a device that has none:
    /// the bus reports nothing for an `iProduct` of zero, measured on a desk's Bluetooth radio. What the OS
    /// could not read is another matter.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_vendor_device_known_to_have_no_strings_is_passed_over_on_windows() {
        let windows_interface = |shape, registered| interface(shape, None, None, Some(registered));
        let single = |shape, absent: bool| lamella_usbbulk::DeviceInfo {
            product_is_absent: absent,
            ..described(vec![windows_interface(shape, false)], Some(false), None)
        };
        assert_eq!(is_cmsis_dap_v2(&single(Some((0x10, 0xFF)), true)), Some(false));
        assert_eq!(bulk_verdict(&single(Some((0x10, 0xFF)), true)), BulkVerdict::PassedOver);
        assert_eq!(is_cmsis_dap_v2(&single(Some((0, 0)), true)), Some(false));
        assert_eq!(bulk_verdict(&single(Some((0, 0)), true)), BulkVerdict::PassedOver);
        assert_eq!(is_cmsis_dap_v2(&single(Some((0, 0)), false)), None);
        assert_eq!(bulk_verdict(&single(Some((0, 0)), false)), BulkVerdict::PassedOver, "not under the spec's GUID");
        let registered = described(vec![windows_interface(Some((0, 0)), true)], Some(false), None);
        assert_eq!(bulk_verdict(&registered), BulkVerdict::Probe, "under the spec's GUID");
    }

    /// **THE SPECIFICATION'S SHAPE IS NEEDED BEFORE A NAME COUNTS, WHATEVER THE NAME SAYS,** so what names
    /// itself on a control-only WebUSB interface (`ff/03/00`) is not a v2 interface. An interface the OS
    /// says has no bulk pair is out the same way, and a shape or a pair it did not state is not held against one.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn only_an_interface_of_the_specifications_shape_can_make_a_device_a_v2_probe() {
        let of = |interfaces| is_cmsis_dap_v2(&described(interfaces, Some(true), Some("Acme CMSIS-DAP")));
        let name = Some("WebUSB: CMSIS-DAP");
        assert_eq!(of(vec![interface(Some((3, 0)), name, None, None)]), Some(false));
        assert_eq!(of(vec![interface(Some((0, 0)), name, Some(false), None)]), Some(false));
        assert_eq!(of(vec![interface(Some((0, 0)), name, Some(true), None)]), Some(true));
        assert_eq!(of(vec![interface(Some((0, 0)), name, None, None)]), Some(true));
        assert_eq!(of(vec![interface(None, name, None, None)]), Some(true));
        assert_eq!(of(vec![interface(Some((0xFF, 0xFF)), Some("ST-Link Debug"), None, None)]), Some(false));
        let webusb = interface(Some((3, 0)), name, None, None);
        let bulk = interface(Some((0, 0)), Some("CMSIS-DAP v2"), None, None);
        assert_eq!(of(vec![webusb.clone(), bulk.clone()]), Some(true));
        assert_eq!(of(vec![bulk, webusb]), Some(true));
    }

    /// **A DEVICE OF ONE INTERFACE IS NAMED BY ITS PRODUCT STRING, as the specification has it,** whatever
    /// its interface calls itself; the token anywhere the specification puts it is enough.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_device_of_one_interface_is_named_by_its_product_string() {
        let of = |name, product| is_cmsis_dap_v2(&described(vec![spec_interface(name)], Some(false), product));
        assert_eq!(of(Some("Debug"), Some("Acme CMSIS-DAP")), Some(true), "not stricter than the specification");
        assert_eq!(of(None, Some("Acme CMSIS-DAP")), Some(true));
        assert_eq!(of(Some("Debug"), Some("Acme Debug")), Some(false));
        assert_eq!(of(None, Some("USB API")), Some(false), "as a one-interface device reads on Windows");
        assert_eq!(of(Some("Acme CMSIS-DAP"), Some("Acme Debug")), Some(true));
        assert_eq!(of(Some("Debug"), None), None, "the product could not be read");
    }

    /// **ON A COMPOSITE DEVICE, THE TOKEN IS IN AN INTERFACE'S STRING;** the product string names a
    /// nameless interface, and an interface that names itself something else is out. That last is a choice
    /// of ours and not the specification's: an EDBG's data gateway has a product holding the token.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_composite_device_is_named_by_its_interfaces_and_the_product_names_only_a_nameless_one() {
        let of = |names: &[Option<&str>], product| {
            let interfaces = names.iter().map(|name| spec_interface(*name)).collect();
            is_cmsis_dap_v2(&described(interfaces, Some(true), product))
        };
        assert_eq!(of(&[Some("Bridge"), Some("CMSIS-DAP v2")], Some("Acme")), Some(true));
        assert_eq!(of(&[Some("CMSIS-DAP v2"), Some("Bridge")], Some("Acme")), Some(true));
        assert_eq!(of(&[Some("")], Some("Acme CMSIS-DAP")), Some(true), "a nameless interface");
        assert_eq!(of(&[Some("EDBG Data Gateway")], Some("EDBG CMSIS-DAP")), Some(false), "our choice");
        assert_eq!(of(&[Some("Bridge"), Some("")], Some("Acme CMSIS-DAP")), Some(true), "one of them is nameless");
        assert_eq!(of(&[Some("")], Some("Acme")), Some(false));
        assert_eq!(of(&[Some("Bridge"), None], Some("Acme")), None, "the unread one may hold it");
        assert_eq!(of(&[None], Some("Acme CMSIS-DAP")), None);
        assert_eq!(of(&[Some("")], None), None, "the product could not be read");
        let unknown = described(vec![spec_interface(Some(""))], None, Some("Acme CMSIS-DAP"));
        assert_eq!(is_cmsis_dap_v2(&unknown), Some(true), "whether it is composite does not change this");
        let absent = lamella_usbbulk::DeviceInfo {
            product_is_absent: true,
            ..described(vec![spec_interface(Some(""))], Some(true), None)
        };
        assert_eq!(is_cmsis_dap_v2(&absent), Some(false), "no string anywhere");
    }

    /// **WHAT COULD NOT BE READ IS WEAKER EVIDENCE, NOT NONE.** On Windows the specification's GUID decides;
    /// with no such registry, a device needs an interface of the specification's shape with a pair not
    /// known to be missing, and a device with no such interface stays out, whatever it might be called.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_device_whose_strings_could_not_be_read_takes_the_weaker_evidence() {
        let verdict = |one| bulk_verdict(&described(vec![one], None, None));
        assert_eq!(verdict(interface(Some((0, 0)), None, None, Some(true))), BulkVerdict::Probe);
        assert_eq!(verdict(interface(None, None, None, Some(true))), BulkVerdict::Probe);
        assert_eq!(verdict(interface(Some((0, 0)), None, None, Some(false))), BulkVerdict::PassedOver);
        assert_eq!(verdict(interface(Some((0, 0)), None, Some(true), None)), BulkVerdict::Probe);
        assert_eq!(verdict(interface(Some((0, 0)), None, None, None)), BulkVerdict::Probe, "a pair not ruled out");
        assert_eq!(verdict(interface(Some((0, 0)), None, Some(false), None)), BulkVerdict::PassedOver);
        assert_eq!(verdict(interface(None, None, Some(true), None)), BulkVerdict::PassedOver, "no shape stated");
        assert_eq!(
            bulk_verdict(&described(Vec::new(), None, None)),
            BulkVerdict::PassedOver,
            "nothing known of any interface"
        );
    }

    /// **A BOARD'S OWN LAMELLA LINK IS NEVER A PROBE, HOWEVER THE OS DESCRIBES ITS INTERFACE**, and it
    /// is not "another vendor-class device" either, since a listing shows it as a board. Its interface
    /// has no name, and read as a probe it would be listed beside the debug probe wired to the same board.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_boards_own_lamella_link_is_neither_a_probe_nor_passed_over() {
        use lamella_wire::usb::{LEGACY_VID, PID, VID};
        for vendor_id in [VID, LEGACY_VID] {
            for name in [None, Some(""), Some("Lamella Link (RP2350)")] {
                let verdict = bulk_verdict(&bulk_device(vendor_id, PID, name));
                assert_eq!(verdict, BulkVerdict::Board, "{vendor_id:04x}, {name:?}");
            }
        }
    }

    /// The Link's older vendor id is a shared one, so a device under it that names itself CMSIS-DAP
    /// is taken at its word.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_device_naming_itself_cmsis_dap_is_a_probe_whatever_its_ids() {
        use lamella_wire::usb::{LEGACY_VID, PID};
        let named = bulk_device(LEGACY_VID, PID, Some("CMSIS-DAP v2 Interface"));
        assert_eq!(bulk_verdict(&named), BulkVerdict::Probe);
    }

    /// **WHAT THE OS DID NOT SAY KEEPS A DEVICE; WHAT IT SAID DOES NOT.** An interface read and found
    /// nameless fails the specification's test and is passed over, where a listing can still show it.
    /// One whose name could not be read stays listed, so a probe is not lost to a refused read.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_nameless_interface_is_passed_over_and_an_unreadable_name_stays_listed() {
        let other = |name| bulk_device(0x0403, 0x6001, name);
        assert_eq!(bulk_verdict(&other(Some(""))), BulkVerdict::PassedOver);
        assert_eq!(bulk_verdict(&other(Some("Vendor Data Interface"))), BulkVerdict::PassedOver);
        assert_eq!(bulk_verdict(&other(None)), BulkVerdict::Probe, "the specification's shape, and no pair ruled out");
        assert_eq!(bulk_verdict(&other(Some("CMSIS-DAP v2 Interface"))), BulkVerdict::Probe);
    }

    /// **AN INTERFACE WITH NO NAME IS READ BY THE PRODUCT STRING**, which is where the specification puts
    /// the token for a device that is not composite; a named one is not, so a data gateway stays out
    /// although the product beside it names a probe.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_nameless_interface_is_named_by_the_product_string_and_a_named_one_is_not() {
        let device = |product: &str, name: &str| lamella_usbbulk::DeviceInfo {
            product: Some(product.to_owned()),
            product_is_absent: false,
            ..bulk_device(0x0403, 0x6001, Some(name))
        };
        assert_eq!(bulk_verdict(&device("Acme CMSIS-DAP", "")), BulkVerdict::Probe);
        assert_eq!(bulk_verdict(&device("Acme Debug", "")), BulkVerdict::PassedOver);
        assert_eq!(bulk_verdict(&device("EDBG CMSIS-DAP", "EDBG Data Gateway")), BulkVerdict::PassedOver);
        assert_eq!(bulk_verdict(&device("Acme Debug", "CMSIS-DAP v2 Interface")), BulkVerdict::Probe);
    }

    /// **WHERE THE BOARD LISTING IS WHAT THE LINK'S GUID HOLDS, A LINK NOT UNDER IT IS NOT HIDDEN**, and
    /// where there is no such registry the listing is every device with the ids.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_link_is_left_out_of_the_record_only_where_the_board_listing_shows_it() {
        let link = |serial: &str| lamella_usbbulk::DeviceInfo {
            serial_number: Some(serial.to_owned()),
            ..bulk_device(0x39e9, 0x0001, Some(""))
        };
        let bound = Ok(vec![link("abcd")]);
        assert!(board_listing_shows(&link("ABCD"), &bound), "the serial's case is the OS's");
        assert!(!board_listing_shows(&link("EFGH"), &bound), "no driver is bound to this one");
        assert!(!board_listing_shows(&link("ABCD"), &Ok(Vec::new())));
        assert!(board_listing_shows(&link("ABCD"), &Err(lamella_usbbulk::Error::Unsupported)));
        assert!(!board_listing_shows(&link("ABCD"), &Err(lamella_usbbulk::Error::Timeout)));
    }

    /// **NAMING THE EXACT IDS REACHES A DEVICE THE FILTER PASSED OVER**, which is how a WCH-Link in its
    /// RISC-V mode is opened; and that widens nothing for a caller naming other ids.
    #[test]
    fn a_device_named_by_its_ids_is_found_though_the_filter_passed_it_over() {
        let wch = |serial: &str| PassedOver {
            vendor_id: 0x1a86,
            product_id: 0x8010,
            serial: Some(serial.to_owned()),
            product: Some("WCH-Link".to_owned()),
            interface_name: Some(String::new()),
            port_id: None,
        };
        let known = known_by_their_ids(&[probe_with_serial(Some("PROBE0000001"))], &[wch("WCH000000001")]);
        assert_eq!(
            choose_serial_among(&known, &Selector::any().with_vid_pid(0x1a86, 0x8010)).unwrap(),
            "WCH000000001"
        );
        assert_eq!(
            choose_serial_among(&known, &Selector::any().with_vid_pid(0x1fc9, 0x0143)).unwrap(),
            "PROBE0000001"
        );
        let two = known_by_their_ids(&[], &[wch("WCH000000001"), wch("WCH000000002")]);
        assert!(
            matches!(
                choose_serial_among(&two, &Selector::any().with_vid_pid(0x1a86, 0x8010)),
                Err(ProbeError::Ambiguous(_))
            ),
            "two of them still refuse"
        );
    }

    /// **A PORT-DERIVED ID IS NOT A SERIAL.** Windows gives a device with no serial one, and it reaches a
    /// composite device's interface by nothing. So the row has no serial, the open asks for no serial, and
    /// the id only tells two such devices apart.
    #[cfg(feature = "usbbulk")]
    #[test]
    fn a_port_derived_id_is_not_a_serial() {
        let bulk = lamella_usbbulk::DeviceInfo {
            vendor_id: 0x1234,
            product_id: 0x5678,
            port_id: Some("5&2CF64626&0&10".to_owned()),
            ..lamella_usbbulk::DeviceInfo::default()
        };
        let probe = ProbeInfo::from_bulk(bulk);
        assert_eq!(probe.serial, None);
        assert_eq!(probe.port_id.as_deref(), Some("5&2CF64626&0&10"));
        assert!(matches!(probe.locator, Locator::Bulk { serial: None, .. }));
        assert!(
            matches!(choose_serial_among(&known_by_their_ids(&[probe], &[]), &Selector::any()), Err(ProbeError::NotFound)),
            "it cannot be named, so it is not named, and no HID interface is looked for under it"
        );
    }

    /// **THE LIST AND THE REFUSAL COUNT THE SAME DEVICES, IN ANY ORDER.** Two serial-less composite probes on
    /// Windows are two rows, each a port id and a HID interface, though a HID row with no id is the same
    /// device as either id. One such probe is one row.
    #[test]
    fn two_serial_less_probes_are_two_rows_and_one_is_one() {
        let hid = || listed_with(None, false);
        let at = |port| listed_with(Some(port), true);
        for order in [
            vec![hid(), hid(), at("5&A&0&1"), at("5&A&0&2")],
            vec![at("5&A&0&1"), at("5&A&0&2"), hid(), hid()],
            vec![hid(), at("5&A&0&1"), hid(), at("5&A&0&2")],
        ] {
            assert_eq!(one_row_per_device(order.clone()).len(), 2, "{order:?}");
            assert_eq!(distinct_probes(&order), ["1234:5678 (no serial)", "1234:5678 (no serial)"]);
        }
        assert_eq!(one_row_per_device(vec![hid(), at("5&A&0&1")]).len(), 1);
        let said = ProbeError::Ambiguous(distinct_probes(&[at("5&A&0&1"), at("5&A&0&2")])).to_string();
        assert!(said.contains("none can be named") && said.contains("leave one attached") && !said.contains("5&A"), "{said}");
    }

    /// **A REASON THAT ALREADY SAYS WHAT TO DO STANDS ALONE.** Telling someone with no udev rule that another
    /// program may hold the device sends them looking for a program. A bare refusal keeps the hint.
    #[test]
    fn an_unopened_device_is_told_of_another_program_only_where_that_could_be_it() {
        let unopened = |why: &str| ProbeError::Unopened { device: "2e8a:000c serial X".to_owned(), why: why.to_owned() }.to_string();
        let bare = unopened("USB error: Access is denied. (os error 5)");
        assert!(bare.ends_with("Another program may hold it."), "{bare}");
        for says in [
            "USB error: open /dev/bus/usb/001/002: permission denied. Linux gives a USB device to the root user until a udev rule grants it to you",
            "HID error: macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks (IOHIDDeviceOpen: 0xe00002c1)",
            "USB error: USBDeviceOpen: another program holds it",
        ] {
            let said = unopened(says);
            assert!(!said.contains("may hold it") && said.ends_with(&format!("({says}).")), "{said}");
        }
    }

    /// **A ROW NAMES THE DEVICE, THE SAME ON EVERY OS.** Windows' HID product string on a composite device
    /// is the interface's: an older micro:bit says "CMSIS-DAP" there where the device says "BBC micro:bit
    /// CMSIS-DAP". The device's own string was read with the row, and nothing more is opened for it. With
    /// no such string the OS's product string stands, and the interface keeps its own name either way.
    #[test]
    fn a_hid_row_says_the_devices_own_name_where_the_os_read_it() {
        let on_windows = ProbeInfo::from_hid(vendor_hid(Some("CMSIS-DAP"), Some("CMSIS-DAP"), Some("BBC micro:bit CMSIS-DAP")));
        assert_eq!(on_windows.product.as_deref(), Some("BBC micro:bit CMSIS-DAP"));
        assert_eq!(on_windows.interface_name.as_deref(), Some("CMSIS-DAP"));
        let none_on_the_device = ProbeInfo::from_hid(vendor_hid(Some("Acme CMSIS-DAP"), Some(""), Some("")));
        assert_eq!(none_on_the_device.product.as_deref(), Some("Acme CMSIS-DAP"));
        let not_said = ProbeInfo::from_hid(vendor_hid(Some("Acme CMSIS-DAP"), None, None));
        assert_eq!(not_said.product.as_deref(), Some("Acme CMSIS-DAP"));
    }

    /// A HID interface on a vendor usage page with long reports, on USB, with the strings given.
    fn vendor_hid(
        product: Option<&str>,
        interface_name: Option<&str>,
        device_name: Option<&str>,
    ) -> lamella_usbhid::DeviceInfo {
        lamella_usbhid::DeviceInfo {
            vendor_id: 0x1234,
            product_id: 0x5678,
            serial_number: None,
            product: product.map(str::to_owned),
            interface_name: interface_name.map(str::to_owned),
            device_name: device_name.map(str::to_owned),
            on_usb: Some(true),
            id: "v".into(),
            usage_page: Some(0xff00),
            usage: Some(0x01),
            input_report_len: Some(64),
            output_report_len: Some(64),
        }
    }

    /// **THE TOKEN COUNTS IN THE PRODUCT STRING OR IN A COMPOSITE DEVICE'S INTERFACE STRING**, as the
    /// specification has it, and whichever of them a backend reports as `product`. A probe with the
    /// token in only one must be listed on every host: the first two here are the same device as a
    /// Mac and as Windows report it.
    #[test]
    fn the_token_counts_in_any_string_the_os_can_read() {
        assert!(is_cmsis_dap_v1(&vendor_hid(Some("Acme Debug"), Some("Acme CMSIS-DAP"), Some("Acme Debug"))));
        assert!(is_cmsis_dap_v1(&vendor_hid(Some("Acme CMSIS-DAP"), Some("Acme CMSIS-DAP"), Some("Acme Debug"))));
        assert!(is_cmsis_dap_v1(&vendor_hid(Some("Acme HID"), Some("Acme HID"), Some("Acme CMSIS-DAP"))));
        for probe in ["BBC micro:bit CMSIS-DAP", "EDBG CMSIS-DAP", "Debug Probe _CMSIS_DAP_", "cmsis-dap"] {
            assert!(is_cmsis_dap_v1(&vendor_hid(Some(probe), None, None)), "{probe}");
        }
    }

    /// **NO TOKEN IS A SETTLED NO WHEN EVERY STRING WAS READ,** a device known to have none among them: the
    /// specification requires the token. Otherwise the one that could not be read may hold it, and the
    /// usage page decides.
    #[test]
    fn a_vendor_page_is_a_probe_only_when_a_string_that_could_not_be_read_may_hold_the_token() {
        assert!(!is_cmsis_dap_v1(&vendor_hid(Some("USB API"), Some(""), Some("USB API"))));
        assert!(!is_cmsis_dap_v1(&vendor_hid(Some("Vendor Update"), Some("Vendor Update"), Some("Acme"))));
        assert!(is_cmsis_dap_v1(&vendor_hid(Some("Acme Debug"), None, Some("Acme Debug"))), "the interface's unread");
        assert!(is_cmsis_dap_v1(&vendor_hid(Some("Acme Debug"), Some("Acme Debug"), None)), "the device's unread");
        assert!(is_cmsis_dap_v1(&vendor_hid(None, None, None)), "nothing was read");
        assert!(!is_cmsis_dap_v1(&vendor_hid(None, Some(""), Some(""))), "every string known to be absent");
        let gadget = vendor_hid(None, Some(""), Some(""));
        assert_eq!(hid_verdict(&gadget), HidVerdict::PassedOver, "and a listing still shows it");
    }

    /// **A DEVICE THAT NAMES NO PROBE AND IS NOT ON USB IS NOT A PROBE**, whatever else it has: a laptop's
    /// touch controller has the vendor page and the long reports. One that names itself stays, because
    /// a chain the OS reports oddly must not cost a probe its place.
    #[test]
    fn a_device_on_another_bus_is_no_probe_unless_it_names_itself() {
        let on_another_bus = |product: Option<&str>| lamella_usbhid::DeviceInfo {
            on_usb: Some(false),
            ..vendor_hid(product, None, None)
        };
        assert!(!is_cmsis_dap_v1(&on_another_bus(Some("HIDSPI Device"))));
        assert!(!is_cmsis_dap_v1(&on_another_bus(None)), "with no string, the bus is what is known");
        assert!(is_cmsis_dap_v1(&on_another_bus(Some("Acme CMSIS-DAP"))));
        let bus_unknown = lamella_usbhid::DeviceInfo { on_usb: None, ..vendor_hid(Some("HIDSPI Device"), None, None) };
        assert!(is_cmsis_dap_v1(&bus_unknown), "nothing settles it, so the page decides");
    }

    /// **WHAT THE RULE DECLINES ON ITS NAME IS RECORDED, SO THE FILTER CANNOT HIDE A PROBE IN SILENCE**, and
    /// a keyboard's hotkey collection and a desktop page are not.
    #[test]
    fn a_vendor_interface_declined_on_its_name_is_recorded_and_a_keyboard_is_not() {
        assert_eq!(hid_verdict(&vendor_hid(Some("Acme CMSIS-DAP"), Some(""), Some("Acme"))), HidVerdict::Probe);
        assert_eq!(hid_verdict(&vendor_hid(Some("USB API"), Some(""), Some("USB API"))), HidVerdict::PassedOver);
        let spi = lamella_usbhid::DeviceInfo { on_usb: Some(false), ..vendor_hid(Some("HIDSPI Device"), None, None) };
        assert_eq!(hid_verdict(&spi), HidVerdict::PassedOver);
        let hotkeys = lamella_usbhid::DeviceInfo { input_report_len: Some(3), ..vendor_hid(Some("Keyboard"), None, None) };
        assert_eq!(hid_verdict(&hotkeys), HidVerdict::NotOfInterest);
        let mouse = lamella_usbhid::DeviceInfo { usage_page: Some(0x01), ..vendor_hid(Some("Mouse"), Some(""), Some("Mouse")) };
        assert_eq!(hid_verdict(&mouse), HidVerdict::NotOfInterest);
        let no_page = lamella_usbhid::DeviceInfo { usage_page: None, ..vendor_hid(Some("Keyboard"), None, None) };
        assert_eq!(hid_verdict(&no_page), HidVerdict::NotOfInterest, "with no page, Linux records no keyboard");
    }

    /// A device the filter declined, with the serial given.
    fn declined_with(serial: Option<&str>, port_derived: bool) -> PassedOver {
        PassedOver {
            vendor_id: 0x1234,
            product_id: 0x5678,
            serial: serial.filter(|_| !port_derived).map(str::to_owned),
            product: None,
            interface_name: None,
            port_id: serial.filter(|_| port_derived).map(str::to_owned),
        }
    }

    /// A device with several interfaces declined is one device in the record.
    #[test]
    fn a_device_is_recorded_once_however_many_interfaces_were_declined() {
        let declined = |serial| declined_with(serial, false);
        let mut record = Vec::new();
        record_passed_over(&mut record, declined(Some("ABC")));
        record_passed_over(&mut record, declined(Some("abc")));
        record_passed_over(&mut record, declined(Some("DEF")));
        record_passed_over(&mut record, declined(None));
        record_passed_over(&mut record, declined(None));
        assert_eq!(record.len(), 3);
    }

    /// A probe of the ids [`declined_with`] uses, with the serial given.
    fn listed_with(serial: Option<&str>, port_derived: bool) -> ProbeInfo {
        ProbeInfo {
            vendor_id: 0x1234,
            product_id: 0x5678,
            serial: serial.filter(|_| !port_derived).map(str::to_owned),
            port_id: serial.filter(|_| port_derived).map(str::to_owned),
            ..hid_probe(Some(0xff00), Some(0x01), Some(65), Some("Acme CMSIS-DAP"))
        }
    }

    /// **ONE DEVICE IS ONE DEVICE, HOWEVER TWO LISTINGS SPELL IT.** Windows gives a device with no serial
    /// a port-derived id over bulk and none over HID, so the two are one; two port-derived ids are two
    /// places on the bus, and a real serial is not any device's lack of one.
    #[test]
    fn two_listings_spellings_of_one_device_are_one_device() {
        let spelled = |serial, port_derived| listed_with(serial, port_derived);
        let same = |a: &ProbeInfo, b: &ProbeInfo| (same_device(&a.spelling(), &b.spelling()), same_device(&b.spelling(), &a.spelling()));
        assert_eq!(same(&spelled(Some("ABC"), false), &spelled(Some("abc"), false)), (true, true));
        assert_eq!(same(&spelled(Some("ABC"), false), &spelled(Some("DEF"), false)), (false, false));
        assert_eq!(same(&spelled(None, false), &spelled(None, false)), (true, true));
        assert_eq!(same(&spelled(None, false), &spelled(Some("5&2CF64626&0&10"), true)), (true, true));
        assert_eq!(same(&spelled(None, false), &spelled(Some("ABC"), false)), (false, false), "a serial it reported");
        assert_eq!(
            same(&spelled(Some("5&2CF64626&0&10"), true), &spelled(Some("5&2CF64626&0&11"), true)),
            (false, false),
            "two places on the bus"
        );
        let other_ids = ProbeInfo { product_id: 0x9999, ..spelled(None, false) };
        assert_eq!(same(&other_ids, &spelled(None, false)), (false, false));
    }

    /// **A PROBE IS NOT ALSO A ROW OF "NOT DEBUG PROBES" FOR ITS OTHER INTERFACE,** an EDBG's data gateway
    /// among them, and with a serial spelled two ways a sibling is not a second probe. A sole serial-less
    /// probe on Windows is one device that cannot be named, not two to choose between.
    #[test]
    fn a_device_listed_as_a_probe_is_left_out_of_the_record() {
        let mut record = Vec::new();
        let probes = [listed_with(Some("ABC"), false)];
        record_declined(&mut record, vec![declined_with(Some("abc"), false), declined_with(Some("DEF"), false)], &probes);
        assert_eq!(record.len(), 1, "{record:?}");
        assert_eq!(record[0].serial.as_deref(), Some("DEF"));

        let mut record = Vec::new();
        let no_serial = [listed_with(None, false)];
        record_declined(&mut record, vec![declined_with(Some("5&2CF64626&0&10"), true)], &no_serial);
        assert!(record.is_empty(), "{record:?}");
        let known = known_by_their_ids(&no_serial, &record);
        assert!(
            matches!(choose_serial_among(&known, &Selector::any().with_vid_pid(0x1234, 0x5678)), Err(ProbeError::NotFound)),
            "one device that cannot be named, and not two to choose between"
        );

        let mut record = Vec::new();
        record_declined(&mut record, vec![declined_with(None, false)], &[listed_with(Some("5&2CF64626&0&10"), true)]);
        assert!(record.is_empty(), "the same, the other way about: {record:?}");
    }

    /// A sole probe seen over both transports with no serial is one probe to [`open`], and two of them
    /// on two ports are two.
    #[test]
    fn a_sole_probe_with_no_serial_is_not_ambiguous_with_itself() {
        let one = [listed_with(None, false), listed_with(Some("5&2CF64626&0&10"), true)];
        assert_eq!(distinct_probes(&one), ["1234:5678 (no serial)"]);
        let two = [
            listed_with(None, false),
            listed_with(Some("5&2CF64626&0&10"), true),
            listed_with(Some("5&2CF64626&0&11"), true),
        ];
        assert_eq!(distinct_probes(&two).len(), 2);
        assert_eq!(distinct_probes(&[listed_with(None, false)]), ["1234:5678 (no serial)"]);
    }

    /// **THE TEST OF THE TOKEN IS ONE TEST**, so a name the list accepts is one `open_hid` can pick; and an
    /// interface's own name outranks the product string its siblings share.
    #[test]
    fn the_rank_reads_the_token_as_the_list_does_and_this_interfaces_own_name_first() {
        let non_canonical = |product: &str, interface_name: Option<&str>| ProbeInfo {
            interface_name: interface_name.map(str::to_owned),
            ..hid_probe(Some(0xffea), Some(0x01), Some(64), Some(product))
        };
        for name in ["Debug Probe _CMSIS_DAP_", "cmsis-dap", "Acme CMSIS-DAP"] {
            assert_eq!(selection_rank(&non_canonical(name, None)), 2, "{name}");
        }
        assert_eq!(selection_rank(&non_canonical("Acme Debug", Some("Acme CMSIS-DAP"))), 2);
        assert_eq!(selection_rank(&non_canonical("Acme CMSIS-DAP", Some("LPCSIO"))), 3);
        assert_eq!(selection_rank(&non_canonical("Acme CMSIS-DAP", Some(""))), 2, "no name of its own");
    }

    #[test]
    fn probe_hid_filter_keeps_vendor_pages_and_drops_desktop_hid() {
        assert!(is_cmsis_dap_v1(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x1fc9,
            product_id: 0x0143,
            serial_number: None,
            product: None,
            interface_name: None,
            device_name: None,
            on_usb: None,
            id: "p".into(),
            usage_page: Some(0xff00),
            usage: Some(0x01),
            input_report_len: Some(65),
            output_report_len: Some(65),
        }));
        assert!(!is_cmsis_dap_v1(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x046d,
            product_id: 0xc52b,
            serial_number: None,
            product: Some("USB Receiver".into()),
            interface_name: None,
            device_name: None,
            on_usb: None,
            id: "m".into(),
            usage_page: Some(0x01),
            usage: Some(0x02),
            input_report_len: Some(8),
            output_report_len: Some(8),
        }));
        assert!(is_cmsis_dap_v1(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x0d28,
            product_id: 0x0204,
            serial_number: None,
            product: Some("DAPLink CMSIS-DAP".into()),
            interface_name: None,
            device_name: None,
            on_usb: None,
            id: "d".into(),
            usage_page: None,
            usage: None,
            input_report_len: None,
            output_report_len: None,
        }));
        assert!(!is_cmsis_dap_v1(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x17ef,
            product_id: 0x60ee,
            serial_number: None,
            product: Some("TrackPoint Keyboard II".into()),
            interface_name: None,
            device_name: None,
            on_usb: None,
            id: "k".into(),
            usage_page: Some(0xffa0),
            usage: Some(0x01),
            input_report_len: Some(3),
            output_report_len: Some(0),
        }));
    }

    #[test]
    fn dap_signature_interface_is_ranked_before_its_siblings() {
        let dap = hid_probe(Some(0xff00), Some(0x01), Some(1025), Some("MCU-LINK Pro (r0CF) CMSIS-DAP V2.241"));
        let lpcsio = hid_probe(Some(0xffea), Some(0x01), Some(65), Some("LPCSIO"));
        let trace = hid_probe(Some(0xffeb), Some(0x01), Some(1025), Some("MCU-LINK NXP TRACE/POWER"));
        assert!(selection_rank(&dap) < selection_rank(&lpcsio));
        assert!(selection_rank(&dap) < selection_rank(&trace));

        let mut set = [lpcsio, trace, dap.clone()];
        set.sort_by_key(selection_rank);
        assert_eq!(set[0].usage_page, dap.usage_page, "the DAP interface (0xFF00) sorts first");
    }

    #[test]
    fn selector_matches_on_set_fields_only() {
        let p = hid_probe(Some(0xff00), Some(0x01), Some(65), None).as_candidate();
        assert!(Selector::any().matches(&p));
        assert!(Selector::by_serial("PROBE0000001").matches(&p));
        assert!(!Selector::by_serial("OTHER").matches(&p));
        assert!(Selector::by_vid_pid(0x1fc9, 0x0143).matches(&p));
        assert!(!Selector::by_vid_pid(0x1fc9, 0x9999).matches(&p));
    }

    #[test]
    fn caps_decode_capability_bits() {
        let caps = Caps {
            protocol_version: "2.1.0".into(),
            product: "MCU-Link".into(),
            capabilities: 0b011,
            packet_size: 64,
        };
        assert!(caps.supports_swd());
        assert!(caps.supports_jtag());
    }
}
