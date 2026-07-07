//! The Wi-Fi control group (HIF group 1): scan, WPA2-PSK connect, and the firmware's
//! asynchronous events (scan done / scan result / connection state / DHCP lease), decoded into
//! typed values.

use crate::hif::{self, HifError, ModuleBus};

/// `tenuM2mConfigCmd` (base 1).
pub const REQ_SCAN: u8 = 16;
pub const RESP_SCAN_DONE: u8 = 17;
pub const REQ_SCAN_RESULT: u8 = 18;
pub const RESP_SCAN_RESULT: u8 = 19;
/// `tenuM2mStaCmd` (base 40).
pub const REQ_CONNECT: u8 = 40;
pub const RESP_CON_STATE_CHANGED: u8 = 44;
/// Doubles as the firmware's DHCP-lease notification to the host.
pub const REQ_DHCP_CONF: u8 = 50;
/// The firmware's DHCP-timeout notification (no payload); the firmware disconnects after it.
pub const REQ_DHCP_FAILURE: u8 = 61;

/// `tenuM2mScanCh`: scan every RF channel.
pub const CHANNEL_ALL: u8 = 255;
/// `tenuM2mSecType`: WPA/WPA2 personal (passphrase).
pub const SEC_WPA_PSK: u8 = 2;
pub const SEC_OPEN: u8 = 1;

pub const MAX_SSID: usize = 32;
pub const MAX_PSK: usize = 63;

/// A decoded Wi-Fi group event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiEvent {
    /// The scan pass finished: `count` results are fetchable by index.
    ScanDone { count: u8, state: i8 },
    /// One scan result (request one at a time by index).
    ScanResult(ScanResult),
    /// Connection state changed: `connected` per `tenuM2mConnState` (1 = connected), with the
    /// firmware's error code on disconnect.
    Connection { connected: bool, error: u8 },
    /// The DHCP lease (all fields in network byte order on the wire, decoded here to octets).
    IpLease { ip: [u8; 4], gateway: [u8; 4], dns: [u8; 4], subnet: [u8; 4], lease_seconds: u32 },
    /// DHCP did not complete; the firmware drops the association after this.
    DhcpFailure,
    /// A message this layer does not decode (another group, or an unhandled opcode).
    Other { group: u8, opcode: u8, length: u16 },
}

/// One access point from the scan list (`tstrM2mWifiscanResult`, 44 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanResult {
    pub index: u8,
    pub rssi: i8,
    pub auth: u8,
    pub channel: u8,
    pub bssid: [u8; 6],
    ssid: [u8; 33],
}

impl ScanResult {
    pub fn ssid(&self) -> &str {
        let end = self.ssid.iter().position(|&b| b == 0).unwrap_or(self.ssid.len());
        core::str::from_utf8(&self.ssid[..end]).unwrap_or("<non-utf8>")
    }
}

/// A connect-request build failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiError {
    Hif(HifError),
    SsidTooLong,
    PskLength,
}

impl From<HifError> for WifiError {
    fn from(error: HifError) -> Self {
        WifiError::Hif(error)
    }
}

/// Requests an active scan of `channel` ([`CHANNEL_ALL`] = every channel): `tstrM2MScan`
/// { channel, reserved, passive-time (ignored for active) }.
pub fn request_scan<B: ModuleBus>(bus: &mut B, channel: u8) -> Result<(), HifError> {
    hif::send(bus, hif::GROUP_WIFI, REQ_SCAN, &[channel, 0, 0, 0], None)
}

/// Requests scan result `index` (one [`WifiEvent::ScanResult`] answers).
pub fn request_scan_result<B: ModuleBus>(bus: &mut B, index: u8) -> Result<(), HifError> {
    hif::send(bus, hif::GROUP_WIFI, REQ_SCAN_RESULT, &[index, 0, 0, 0], None)
}

/// Requests a WPA2-PSK association: `tstrM2mWifiConnect` -- the 68-byte security block (the
/// passphrase, NUL-terminated; the module derives the pairwise keys), channel-any, the SSID,
/// and credential saving on. The join + 4-way handshake run on-module;
/// [`WifiEvent::Connection`] and then [`WifiEvent::IpLease`] report the outcome.
pub fn connect<B: ModuleBus>(
    bus: &mut B,
    ssid: &str,
    passphrase: &str,
    channel: u8,
) -> Result<(), WifiError> {
    if ssid.len() > MAX_SSID {
        return Err(WifiError::SsidTooLong);
    }
    if passphrase.len() < 8 || passphrase.len() > MAX_PSK {
        return Err(WifiError::PskLength);
    }
    let mut request = [0u8; 108];
    request[..passphrase.len()].copy_from_slice(passphrase.as_bytes());
    request[65] = SEC_WPA_PSK;
    request[68] = channel;
    request[69] = 0;
    request[70..70 + ssid.len()].copy_from_slice(ssid.as_bytes());
    hif::send(bus, hif::GROUP_WIFI, REQ_CONNECT, &request, None)?;
    Ok(())
}

/// Requests an OPEN (no-security) association -- the same tstrM2mWifiConnect with SecType = OPEN
/// and an empty auth block. The bisect for an AUTH_FAIL: an open join skips WPA2 key derivation
/// entirely, so success here isolates a failure to the passphrase.
pub fn connect_open<B: ModuleBus>(bus: &mut B, ssid: &str, channel: u8) -> Result<(), WifiError> {
    if ssid.len() > MAX_SSID {
        return Err(WifiError::SsidTooLong);
    }
    let mut request = [0u8; 108];
    request[65] = SEC_OPEN;
    request[68] = channel;
    request[69] = 0;
    request[70..70 + ssid.len()].copy_from_slice(ssid.as_bytes());
    hif::send(bus, hif::GROUP_WIFI, REQ_CONNECT, &request, None)?;
    Ok(())
}

/// `tenuM2mStaCmd` (19.6+ drivers): the new-format connect request the current firmware line
/// is actively tested against; the legacy opcode 40 is nominally kept for compatibility.
pub const REQ_CONN: u8 = 59;

/// Builds the 48-byte `tstrM2mWifiConnHdr` (credentials header + common section) of the
/// new-format connect: total credential size, store flags (not stored), the ZERO-based
/// channel (255 = any -- the one value passed through unshifted), the length-prefixed SSID,
/// and the auth type.
fn conn_hdr(ssid: &str, auth_type: u8, auth_size: u16, channel: u8) -> [u8; 48] {
    let mut hdr = [0u8; 48];
    let cred_size = 44 + auth_size;
    hdr[0..2].copy_from_slice(&cred_size.to_le_bytes());
    hdr[2] = 0;
    hdr[3] = if channel == CHANNEL_ALL { CHANNEL_ALL } else { channel.saturating_sub(1) };
    hdr[4] = ssid.len() as u8;
    hdr[5..5 + ssid.len()].copy_from_slice(ssid.as_bytes());
    hdr[44] = auth_type;
    hdr
}

/// New-format WPA(2)-PSK association (`M2M_WIFI_REQ_CONN`): the credentials header as the
/// control part and the 108-byte `tstrM2mWifiPsk` block -- length-prefixed passphrase, the
/// module deriving the keys -- as the DATA part at offset 48, with the opcode's data marker.
pub fn connect_v2<B: ModuleBus>(
    bus: &mut B,
    ssid: &str,
    passphrase: &str,
    channel: u8,
) -> Result<(), WifiError> {
    if ssid.len() > MAX_SSID {
        return Err(WifiError::SsidTooLong);
    }
    if passphrase.len() < 8 || passphrase.len() > MAX_PSK {
        return Err(WifiError::PskLength);
    }
    let hdr = conn_hdr(ssid, SEC_WPA_PSK, 108, channel);
    let mut psk = [0u8; 108];
    psk[0] = passphrase.len() as u8;
    psk[1..1 + passphrase.len()].copy_from_slice(passphrase.as_bytes());
    hif::send(
        bus,
        hif::GROUP_WIFI,
        REQ_CONN | hif::OPCODE_DATA_BIT,
        &hdr,
        Some((&psk, 48)),
    )?;
    Ok(())
}

/// New-format OPEN association: the credentials header alone (no auth block).
pub fn connect_open_v2<B: ModuleBus>(
    bus: &mut B,
    ssid: &str,
    channel: u8,
) -> Result<(), WifiError> {
    if ssid.len() > MAX_SSID {
        return Err(WifiError::SsidTooLong);
    }
    let hdr = conn_hdr(ssid, SEC_OPEN, 0, channel);
    hif::send(bus, hif::GROUP_WIFI, REQ_CONN, &hdr, None)?;
    Ok(())
}

/// Reads the module's MAC address (the running firmware's view): general-purpose register 2
/// (0xc0008) points at a shared block whose first word locates the efuse-derived MAC in the
/// firmware's memory window (reference `nmi_get_mac_address`).
pub fn mac_address<B: ModuleBus>(bus: &mut B) -> Result<[u8; 6], HifError> {
    let gp2 = bus.read_reg(0xc0008)?;
    let mut shared = [0u8; 8];
    bus.read_block(gp2 | 0x30000, &mut shared)?;
    let mac_pos = u32::from_le_bytes([shared[0], shared[1], 0, 0]);
    let mut mac = [0u8; 6];
    bus.read_block(mac_pos | 0x30000, &mut mac)?;
    Ok(mac)
}

/// Polls for one firmware message and decodes the Wi-Fi group events this layer understands;
/// every message is acknowledged (receive-done) after its payload is read.
pub fn poll_event<B: ModuleBus>(bus: &mut B) -> Result<Option<WifiEvent>, HifError> {
    let Some(event) = hif::poll_event(bus)? else {
        return Ok(None);
    };
    let payload = event.address + u32::from(hif::HEADER_LEN);
    let decoded = if event.group == hif::GROUP_WIFI && event.opcode == RESP_SCAN_DONE {
        let mut raw = [0u8; 4];
        bus.read_block(payload, &mut raw)?;
        WifiEvent::ScanDone { count: raw[0], state: raw[1] as i8 }
    } else if event.group == hif::GROUP_WIFI && event.opcode == RESP_SCAN_RESULT {
        let mut raw = [0u8; 44];
        bus.read_block(payload, &mut raw)?;
        let mut bssid = [0u8; 6];
        bssid.copy_from_slice(&raw[4..10]);
        let mut ssid = [0u8; 33];
        ssid.copy_from_slice(&raw[10..43]);
        WifiEvent::ScanResult(ScanResult {
            index: raw[0],
            rssi: raw[1] as i8,
            auth: raw[2],
            channel: raw[3],
            bssid,
            ssid,
        })
    } else if event.group == hif::GROUP_WIFI && event.opcode == RESP_CON_STATE_CHANGED {
        let mut raw = [0u8; 4];
        bus.read_block(payload, &mut raw)?;
        WifiEvent::Connection { connected: raw[0] == 1, error: raw[1] }
    } else if event.group == hif::GROUP_WIFI && event.opcode == REQ_DHCP_FAILURE {
        WifiEvent::DhcpFailure
    } else if event.group == hif::GROUP_WIFI && event.opcode == REQ_DHCP_CONF {
        let mut raw = [0u8; 20];
        bus.read_block(payload, &mut raw)?;
        let octets = |o: usize| [raw[o], raw[o + 1], raw[o + 2], raw[o + 3]];
        WifiEvent::IpLease {
            ip: octets(0),
            gateway: octets(4),
            dns: octets(8),
            subnet: octets(12),
            lease_seconds: u32::from_le_bytes(octets(16)),
        }
    } else {
        WifiEvent::Other { group: event.group, opcode: event.opcode, length: event.length }
    };
    hif::set_receive_done(bus)?;
    Ok(Some(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hif::testing::FakeModule;

    const CTRL_0: u32 = 0x1070;
    const CTRL_1: u32 = 0x1084;

    fn stage_event(module: &mut FakeModule, group: u8, opcode: u8, payload: &[u8]) {
        let length = hif::HEADER_LEN + payload.len() as u16;
        module.regs.insert(CTRL_0, 1 | (u32::from(length) << 2));
        module.regs.insert(CTRL_1, 0x60000);
        module
            .write_block(0x60000, &[group, opcode, length as u8, (length >> 8) as u8])
            .unwrap();
        module.write_block(0x60000 + u32::from(hif::HEADER_LEN), payload).unwrap();
    }

    #[test]
    fn connect_request_places_every_field() {
        let mut module = FakeModule::with_alloc(0x40000);
        connect(&mut module, "default", "vdRLo3i2nUsodshqrwxxo2i3u1i1oppp", CHANNEL_ALL).expect("connect");
        let mut request = [0u8; 108];
        module.read_block(0x40000 + u32::from(hif::HEADER_LEN), &mut request).unwrap();
        assert_eq!(&request[..32], b"vdRLo3i2nUsodshqrwxxo2i3u1i1oppp");
        assert_eq!(request[32], 0);
        assert_eq!(request[65], SEC_WPA_PSK);
        assert_eq!([request[68], request[69]], [CHANNEL_ALL, 0]);
        assert_eq!(&request[70..77], b"default");
        assert_eq!(request[77], 0);
        assert_eq!(request[103], 0);
    }

    #[test]
    fn connect_rejects_bad_credential_lengths() {
        let mut module = FakeModule::with_alloc(0x40000);
        assert_eq!(connect(&mut module, "x", "short", CHANNEL_ALL), Err(WifiError::PskLength));
        let long_ssid = "123456789012345678901234567890123";
        assert_eq!(connect(&mut module, long_ssid, "12345678", CHANNEL_ALL), Err(WifiError::SsidTooLong));
    }

    #[test]
    fn scan_done_and_result_events_decode() {
        let mut module = FakeModule::with_alloc(0x40000);
        stage_event(&mut module, hif::GROUP_WIFI, RESP_SCAN_DONE, &[3, 0, 0, 0]);
        assert_eq!(
            poll_event(&mut module).expect("poll"),
            Some(WifiEvent::ScanDone { count: 3, state: 0 })
        );

        let mut payload = [0u8; 44];
        payload[0] = 1;
        payload[1] = (-52i8) as u8;
        payload[2] = SEC_WPA_PSK;
        payload[3] = 6;
        payload[4..10].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]);
        payload[10..17].copy_from_slice(b"default");
        stage_event(&mut module, hif::GROUP_WIFI, RESP_SCAN_RESULT, &payload);
        let Some(WifiEvent::ScanResult(result)) = poll_event(&mut module).expect("poll") else {
            panic!("expected a scan result");
        };
        assert_eq!(result.ssid(), "default");
        assert_eq!(result.rssi, -52);
        assert_eq!(result.channel, 6);
    }

    #[test]
    fn connection_and_lease_events_decode() {
        let mut module = FakeModule::with_alloc(0x40000);
        stage_event(&mut module, hif::GROUP_WIFI, RESP_CON_STATE_CHANGED, &[1, 0, 0, 0]);
        assert_eq!(
            poll_event(&mut module).expect("poll"),
            Some(WifiEvent::Connection { connected: true, error: 0 })
        );

        let mut lease = [0u8; 20];
        lease[..4].copy_from_slice(&[192, 168, 1, 77]);
        lease[4..8].copy_from_slice(&[192, 168, 1, 1]);
        lease[8..12].copy_from_slice(&[8, 8, 8, 8]);
        lease[12..16].copy_from_slice(&[255, 255, 255, 0]);
        lease[16..20].copy_from_slice(&86400u32.to_le_bytes());
        stage_event(&mut module, hif::GROUP_WIFI, REQ_DHCP_CONF, &lease);
        assert_eq!(
            poll_event(&mut module).expect("poll"),
            Some(WifiEvent::IpLease {
                ip: [192, 168, 1, 77],
                gateway: [192, 168, 1, 1],
                dns: [8, 8, 8, 8],
                subnet: [255, 255, 255, 0],
                lease_seconds: 86400,
            })
        );
    }
}
