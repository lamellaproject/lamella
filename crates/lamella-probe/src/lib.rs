//! Debug-probe discovery, selection, and capability negotiation -- the transport-neutral layer
//! ABOVE the wire protocols. It enumerates every attached probe across transports, opens one by
//! identity (picking the right interface when a probe is a composite USB device), negotiates its
//! `DAP_Info` capabilities, and hands back a connected [`lamella_cmsis_dap::Dap`].

use std::time::Duration;

use lamella_cmsis_dap::{Dap, DapError, Transport, TransportError};

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
    fn from_hid(d: lamella_usbhid::DeviceInfo) -> ProbeInfo {
        ProbeInfo {
            vendor_id: d.vendor_id,
            product_id: d.product_id,
            serial: d.serial_number,
            product: d.product,
            wire: Wire::CmsisDapV1Hid,
            usage_page: d.usage_page,
            usage: d.usage,
            report_len: d.input_report_len,
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
            wire: Wire::CmsisDapV2Bulk,
            usage_page: None,
            usage: None,
            report_len: None,
            locator: Locator::Bulk {
                vendor_id: d.vendor_id,
                product_id: d.product_id,
                serial: d.serial_number,
            },
        }
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

/// Which probe to open. An unset field matches anything; a probe passes when every set field
/// matches. [`Selector::any`] takes the first probe found.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    /// Require this USB vendor id.
    pub vendor_id: Option<u16>,
    /// Require this USB product id.
    pub product_id: Option<u16>,
    /// Require this serial number -- the reliable way to pick one probe out of several alike.
    pub serial: Option<String>,
}

impl Selector {
    /// Matches any connected probe (the first one discovered).
    pub fn any() -> Self {
        Self::default()
    }

    /// Matches the probe with this serial number.
    pub fn by_serial(serial: impl Into<String>) -> Self {
        Self {
            serial: Some(serial.into()),
            ..Self::default()
        }
    }

    /// Matches probes with this vendor and product id.
    pub fn by_vid_pid(vendor_id: u16, product_id: u16) -> Self {
        Self {
            vendor_id: Some(vendor_id),
            product_id: Some(product_id),
            ..Self::default()
        }
    }

    /// Adds a serial-number constraint (builder style).
    pub fn with_serial(mut self, serial: impl Into<String>) -> Self {
        self.serial = Some(serial.into());
        self
    }

    fn matches(&self, p: &ProbeInfo) -> bool {
        self.vendor_id.is_none_or(|v| v == p.vendor_id)
            && self.product_id.is_none_or(|pid| pid == p.product_id)
            && self
                .serial
                .as_deref()
                .is_none_or(|s| p.serial.as_deref() == Some(s))
    }
}

/// An error discovering, opening, or negotiating with a probe.
#[derive(Debug)]
pub enum ProbeError {
    /// No probe matched the selector, or no matching interface answered `DAP_Info`.
    NotFound,
    /// Opening or exchanging with the probe's USB transport failed; carries a description.
    Transport(String),
    /// A DAP operation failed.
    Dap(DapError),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::NotFound => write!(f, "no matching probe found"),
            ProbeError::Transport(msg) => write!(f, "probe transport error: {msg}"),
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

/// A [`Transport`] over whichever native USB backend reached a probe -- HID reports for v1, bulk
/// pipes for v2 -- so one [`Dap`] type drives either.
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

/// Lists the discovered probes -- ONE entry per physical probe, so a composite probe (several HID
/// interfaces on one vid/pid/serial) appears once, represented by its DAP interface. HID is always
/// searched; USB bulk is searched when the `usbbulk` feature is enabled. Interfaces that do not look
/// like a probe (standard desktop HID) are filtered out; whether a candidate truly speaks DAP is
/// confirmed at [`open`] time. For the raw per-interface view, enumerate the transports directly.
pub fn list() -> Vec<ProbeInfo> {
    let mut best: Vec<ProbeInfo> = Vec::new();
    for cand in all_candidates() {
        match best.iter_mut().find(|p| {
            p.vendor_id == cand.vendor_id && p.product_id == cand.product_id && p.serial == cand.serial
        }) {
            Some(existing) if selection_rank(&cand) < selection_rank(existing) => *existing = cand,
            Some(_) => {}
            None => best.push(cand),
        }
    }
    best
}

/// Every probe-like interface across the native transports -- one entry per interface, so a
/// composite probe appears several times. The candidate set [`open`] selects from.
fn all_candidates() -> Vec<ProbeInfo> {
    let mut out = Vec::new();
    if let Ok(hids) = lamella_usbhid::enumerate() {
        out.extend(hids.into_iter().filter(looks_like_probe_hid).map(ProbeInfo::from_hid));
    }
    #[cfg(feature = "usbbulk")]
    if let Ok(bulk) = lamella_usbbulk::enumerate() {
        out.extend(bulk.into_iter().map(ProbeInfo::from_bulk));
    }
    out
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
        all_candidates().into_iter().filter(|p| selector.matches(p)).collect();
    if candidates.is_empty() {
        return Err(ProbeError::NotFound);
    }
    candidates.sort_by_key(selection_rank);

    let mut last = ProbeError::NotFound;
    for info in candidates {
        match open_dap(&info) {
            Ok(mut dap) => match confirm_and_connect(&mut dap) {
                Ok(caps) => return Ok(Session { info, caps, dap }),
                Err(e) => last = e,
            },
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Whether a HID interface is plausibly a debug probe (so [`list`] shows it and [`open`] may try it).
fn looks_like_probe_hid(d: &lamella_usbhid::DeviceInfo) -> bool {
    if matches!(d.input_report_len, Some(len) if len < 63) {
        return false;
    }
    match d.usage_page {
        Some(page) => page >= 0xFF00,
        None => d.product.as_deref().is_some_and(|s| s.contains("CMSIS-DAP")),
    }
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
    let named = p.product.as_deref().is_some_and(|s| s.contains("CMSIS-DAP"));
    match (canonical, named) {
        (true, _) => 1,
        (false, true) => 2,
        _ => 3,
    }
}

/// Opens the transport for one candidate and wraps it in a `Dap` (not yet connected).
fn open_dap(info: &ProbeInfo) -> Result<Dap<AnyTransport>, ProbeError> {
    let transport = match &info.locator {
        Locator::Hid { id } => AnyTransport::Hid(
            lamella_usbhid::Device::open_id(id).map_err(|e| ProbeError::Transport(e.to_string()))?,
        ),
        #[cfg(feature = "usbbulk")]
        Locator::Bulk {
            vendor_id,
            product_id,
            serial,
        } => AnyTransport::Bulk(
            lamella_usbbulk::Device::open(*vendor_id, *product_id, serial.as_deref())
                .map_err(|e| ProbeError::Transport(e.to_string()))?,
        ),
    };
    Ok(Dap::new(transport))
}

/// Confirms a candidate really speaks DAP (via `DAP_Info`), reads its capabilities, and connects it
/// to the target over SWD. An interface that is not the DAP one fails the `DAP_Info` litmus here and
/// is rejected without connecting.
fn confirm_and_connect(dap: &mut Dap<AnyTransport>) -> Result<Caps, ProbeError> {
    let capabilities = dap.info_bytes(0xF0)?.first().copied().unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hid_probe(
        usage_page: Option<u16>,
        usage: Option<u16>,
        report_len: Option<u16>,
        product: Option<&str>,
    ) -> ProbeInfo {
        ProbeInfo {
            vendor_id: 0x1fc9,
            product_id: 0x0143,
            serial: Some("STKVKH3CMA5YD".into()),
            product: product.map(Into::into),
            wire: Wire::CmsisDapV1Hid,
            usage_page,
            usage,
            report_len,
            locator: Locator::Hid { id: "path".into() },
        }
    }

    #[test]
    fn probe_hid_filter_keeps_vendor_pages_and_drops_desktop_hid() {
        assert!(looks_like_probe_hid(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x1fc9,
            product_id: 0x0143,
            serial_number: None,
            product: None,
            id: "p".into(),
            usage_page: Some(0xff00),
            usage: Some(0x01),
            input_report_len: Some(65),
            output_report_len: Some(65),
        }));
        assert!(!looks_like_probe_hid(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x046d,
            product_id: 0xc52b,
            serial_number: None,
            product: Some("USB Receiver".into()),
            id: "m".into(),
            usage_page: Some(0x01),
            usage: Some(0x02),
            input_report_len: Some(8),
            output_report_len: Some(8),
        }));
        assert!(looks_like_probe_hid(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x0d28,
            product_id: 0x0204,
            serial_number: None,
            product: Some("DAPLink CMSIS-DAP".into()),
            id: "d".into(),
            usage_page: None,
            usage: None,
            input_report_len: None,
            output_report_len: None,
        }));
        assert!(!looks_like_probe_hid(&lamella_usbhid::DeviceInfo {
            vendor_id: 0x17ef,
            product_id: 0x60ee,
            serial_number: None,
            product: Some("TrackPoint Keyboard II".into()),
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
        let p = hid_probe(Some(0xff00), Some(0x01), Some(65), None);
        assert!(Selector::any().matches(&p));
        assert!(Selector::by_serial("STKVKH3CMA5YD").matches(&p));
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
