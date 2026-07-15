//! Identity + USB descriptors for the native driverless-WinUSB Lamella Link carrier -- the fast
//! interpreter-flash path for a board with a native USB device peripheral.

/// Vendor id: the pid.codes open-source VID for development (swap for an allocated Lamella PID before
/// publish).
pub const VID: u16 = 0x1209;
/// Product id: the pid.codes prototype PID.
pub const PID: u16 = 0x0001;
/// The WinUSB device-interface GUID a host opens to reach the Lamella Link vendor interface. The device
/// advertises it in its Microsoft OS 2.0 `DeviceInterfaceGUIDs` registry property; the host matches on
/// it. Stable by contract -- changing it makes the device invisible to an existing host.
pub const WINUSB_INTERFACE_GUID: &str = "{1D4B2365-4749-5541-8D5E-9A0A0000C0DE}";
/// Bulk IN endpoint address (device-to-host), host perspective.
pub const BULK_IN_EP: u8 = 0x81;
/// Bulk OUT endpoint address (host-to-device).
pub const BULK_OUT_EP: u8 = 0x01;
/// Full-Speed bulk max packet size; frames are chunked to this on the wire.
pub const BULK_MAX_PACKET: u16 = 64;

/// The vendor request code (`bMS_VendorCode`) the device answers to return [`MS_OS_20_DESCRIPTOR_SET`].
pub const MS_VENDOR_CODE: u8 = 0x01;
/// The vendor request code (`bVendorCode`) advertised for WebUSB. Unused while there is no landing page.
pub const WEBUSB_VENDOR_CODE: u8 = 0x02;
/// `wIndex` of a `GET MS OS 2.0 DESCRIPTOR` vendor control request (the spec's `MS_OS_20_DESCRIPTOR_INDEX`).
pub const MS_OS_20_DESCRIPTOR_INDEX: u16 = 0x07;

const SET_HEADER: u16 = 0x00;
const FEATURE_COMPATIBLE_ID: u16 = 0x03;
const FEATURE_REG_PROPERTY: u16 = 0x04;
/// dwWindowsVersion for Windows 8.1+ (NTDDI_WIN8_1) -- the minimum that honors MS OS 2.0 descriptors.
const NTDDI_WIN8_1: u32 = 0x0603_0000;
/// `wPropertyDataType` = REG_MULTI_SZ (a NUL-separated, double-NUL-terminated list of GUID strings).
const REG_MULTI_SZ: u16 = 7;

/// USB device-capability descriptor types.
const DEVICE_CAPABILITY: u8 = 0x10;
const PLATFORM: u8 = 0x05;
const BOS: u8 = 0x0F;

/// The `DeviceInterfaceGUIDs` registry property name (registers the interface GUID for host lookup).
const PROPERTY_NAME: &str = "DeviceInterfaceGUIDs";

/// The Microsoft OS 2.0 platform-capability UUID `{D8DD60DF-4589-4CC7-9CD2-659D9E648A9F}`, in wire byte
/// order (first three fields little-endian, last two big-endian).
const MS_OS_20_UUID: [u8; 16] = [
    0xDF, 0x60, 0xDD, 0xD8, 0x89, 0x45, 0xC7, 0x4C, 0x9C, 0xD2, 0x65, 0x9D, 0x9E, 0x64, 0x8A, 0x9F,
];
/// The WebUSB platform-capability UUID `{3408B638-09A9-47A0-8BFD-A0768815B665}`, in wire byte order.
const WEBUSB_UUID: [u8; 16] = [
    0x38, 0xB6, 0x08, 0x34, 0xA9, 0x09, 0xA0, 0x47, 0x8B, 0xFD, 0xA0, 0x76, 0x88, 0x15, 0xB6, 0x65,
];

const SET_HEADER_LEN: usize = 10;
const COMPATIBLE_ID_LEN: usize = 20;
/// UTF-16LE bytes of the property NAME, including its NUL terminator.
const NAME_BYTES: usize = (PROPERTY_NAME.len() + 1) * 2;
/// UTF-16LE bytes of the GUID DATA, including the REG_MULTI_SZ double-NUL terminator.
const DATA_BYTES: usize = (WINUSB_INTERFACE_GUID.len() + 2) * 2;
/// The registry-property feature descriptor: 10 fixed bytes (5 `u16` fields) + name + data.
const REG_PROP_LEN: usize = 10 + NAME_BYTES + DATA_BYTES;

/// Byte length of [`MS_OS_20_DESCRIPTOR_SET`].
pub const MS_OS_20_SET_LEN: usize = SET_HEADER_LEN + COMPATIBLE_ID_LEN + REG_PROP_LEN;

const fn put16(d: &mut [u8], i: usize, v: u16) {
    d[i] = v as u8;
    d[i + 1] = (v >> 8) as u8;
}
const fn put32(d: &mut [u8], i: usize, v: u32) {
    d[i] = v as u8;
    d[i + 1] = (v >> 8) as u8;
    d[i + 2] = (v >> 16) as u8;
    d[i + 3] = (v >> 24) as u8;
}
/// Write ASCII `s` as UTF-16LE at `i` (each byte, then `0x00`); the caller sizes the region to include
/// any NUL terminator (left zero).
const fn put_ascii_utf16(d: &mut [u8], i: usize, s: &str) {
    let bytes = s.as_bytes();
    let mut j = 0;
    while j < bytes.len() {
        d[i + j * 2] = bytes[j];
        d[i + j * 2 + 1] = 0;
        j += 1;
    }
}
const fn copy16(d: &mut [u8], i: usize, src: &[u8; 16]) {
    let mut k = 0;
    while k < 16 {
        d[i + k] = src[k];
        k += 1;
    }
}

/// Build the MS OS 2.0 descriptor set, FLAT: set header -> `CompatibleID "WINUSB"` (so Windows
/// loads `winusb.sys`) -> a `DeviceInterfaceGUIDs` registry property carrying
/// [`WINUSB_INTERFACE_GUID`] (so a host finds the interface by GUID).
///
/// Flat is LOAD-BEARING for this single-interface, non-composite device: Windows applies only a
/// set's TOP-LEVEL features to a non-composite device -- configuration/function subsets exist for
/// the composite parser to route features per function, and features nested under them are
/// silently ignored here (hardware-observed: the nested form enumerated cleanly, answered this
/// exact request, and still bound no driver). Microsoft's own non-composite WinUSB sample set is
/// this flat shape. A future composite device re-introduces the subsets FOR ITS FUNCTIONS.
const fn build_ms_os_20() -> [u8; MS_OS_20_SET_LEN] {
    let mut d = [0u8; MS_OS_20_SET_LEN];

    put16(&mut d, 0, SET_HEADER_LEN as u16);
    put16(&mut d, 2, SET_HEADER);
    put32(&mut d, 4, NTDDI_WIN8_1);
    put16(&mut d, 8, MS_OS_20_SET_LEN as u16);

    let id = SET_HEADER_LEN;
    put16(&mut d, id, COMPATIBLE_ID_LEN as u16);
    put16(&mut d, id + 2, FEATURE_COMPATIBLE_ID);
    d[id + 4] = b'W';
    d[id + 5] = b'I';
    d[id + 6] = b'N';
    d[id + 7] = b'U';
    d[id + 8] = b'S';
    d[id + 9] = b'B';

    let r = id + COMPATIBLE_ID_LEN;
    put16(&mut d, r, REG_PROP_LEN as u16);
    put16(&mut d, r + 2, FEATURE_REG_PROPERTY);
    put16(&mut d, r + 4, REG_MULTI_SZ);
    put16(&mut d, r + 6, NAME_BYTES as u16);
    put_ascii_utf16(&mut d, r + 8, PROPERTY_NAME);
    let data_len_at = r + 8 + NAME_BYTES;
    put16(&mut d, data_len_at, DATA_BYTES as u16);
    put_ascii_utf16(&mut d, data_len_at + 2, WINUSB_INTERFACE_GUID);

    d
}

/// The MS OS 2.0 descriptor set the device returns for the vendor [`MS_VENDOR_CODE`] request
/// (`wIndex = ` [`MS_OS_20_DESCRIPTOR_INDEX`]).
pub const MS_OS_20_DESCRIPTOR_SET: [u8; MS_OS_20_SET_LEN] = build_ms_os_20();

/// Payload length of the Microsoft OS 2.0 platform capability -- the bytes after the 3-byte
/// capability header: bReserved + UUID + dwWindowsVersion + wMSOSDescriptorSetTotalLength +
/// bMS_VendorCode + bAltEnumCode.
pub const MS_OS_20_CAP_PAYLOAD_LEN: usize = 25;
/// Payload length of the WebUSB platform capability: bReserved + UUID + bcdVersion + bVendorCode +
/// iLandingPage.
pub const WEBUSB_CAP_PAYLOAD_LEN: usize = 21;

const fn build_ms_os_20_capability() -> [u8; MS_OS_20_CAP_PAYLOAD_LEN] {
    let mut d = [0u8; MS_OS_20_CAP_PAYLOAD_LEN];
    copy16(&mut d, 1, &MS_OS_20_UUID);
    put32(&mut d, 17, NTDDI_WIN8_1);
    put16(&mut d, 21, MS_OS_20_SET_LEN as u16);
    d[23] = MS_VENDOR_CODE;
    d
}

const fn build_webusb_capability() -> [u8; WEBUSB_CAP_PAYLOAD_LEN] {
    let mut d = [0u8; WEBUSB_CAP_PAYLOAD_LEN];
    copy16(&mut d, 1, &WEBUSB_UUID);
    put16(&mut d, 17, 0x0100);
    d[19] = WEBUSB_VENDOR_CODE;
    d
}

/// The Microsoft OS 2.0 platform-capability PAYLOAD (everything after `bDevCapabilityType`), for a
/// device stack whose BOS writer prepends the capability header itself (usb-device's
/// `BosWriter::capability`). [`BOS_DESCRIPTOR`] embeds these same bytes.
pub const MS_OS_20_PLATFORM_CAPABILITY: [u8; MS_OS_20_CAP_PAYLOAD_LEN] = build_ms_os_20_capability();
/// The WebUSB platform-capability PAYLOAD, same convention as [`MS_OS_20_PLATFORM_CAPABILITY`].
pub const WEBUSB_PLATFORM_CAPABILITY: [u8; WEBUSB_CAP_PAYLOAD_LEN] = build_webusb_capability();

/// Byte length of [`BOS_DESCRIPTOR`]: the BOS header + the MS OS 2.0 cap (28) + the WebUSB cap (24).
pub const BOS_LEN: usize = 5 + 28 + 24;

const fn build_bos() -> [u8; BOS_LEN] {
    let mut d = [0u8; BOS_LEN];

    d[0] = 5;
    d[1] = BOS;
    put16(&mut d, 2, BOS_LEN as u16);
    d[4] = 2;

    let m = 5;
    d[m] = 3 + MS_OS_20_CAP_PAYLOAD_LEN as u8;
    d[m + 1] = DEVICE_CAPABILITY;
    d[m + 2] = PLATFORM;
    let ms = build_ms_os_20_capability();
    let mut i = 0;
    while i < ms.len() {
        d[m + 3 + i] = ms[i];
        i += 1;
    }

    let w = m + 28;
    d[w] = 3 + WEBUSB_CAP_PAYLOAD_LEN as u8;
    d[w + 1] = DEVICE_CAPABILITY;
    d[w + 2] = PLATFORM;
    let web = build_webusb_capability();
    let mut j = 0;
    while j < web.len() {
        d[w + 3 + j] = web[j];
        j += 1;
    }

    d
}

/// The device's BOS descriptor: a Microsoft OS 2.0 platform capability (pointing Windows at
/// [`MS_OS_20_DESCRIPTOR_SET`]) + a WebUSB platform capability. `bcdUSB` must be `>= 0x0210` for the
/// host to request the BOS at all.
pub const BOS_DESCRIPTOR: [u8; BOS_LEN] = build_bos();

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
    fn u16_at(d: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([d[i], d[i + 1]])
    }

    #[test]
    fn ms_os_20_set_lengths_are_self_consistent() {
        let d = &MS_OS_20_DESCRIPTOR_SET;
        assert_eq!(d.len(), 162, "the FLAT layout math (10+20+132) must total 162");
        assert_eq!(u16_at(d, 0), 10, "set-header wLength");
        assert_eq!(u16_at(d, 8), d.len() as u16, "set-header wTotalLength == whole set");
        assert_eq!(u16_at(d, 10), 20, "compatible-id wLength");
        assert_eq!(u16_at(d, 12), FEATURE_COMPATIBLE_ID, "top-level feature: no subset headers");
        assert_eq!(u16_at(d, 30), REG_PROP_LEN as u16, "registry-property wLength");
        assert_eq!(u16_at(d, 30), 132);
    }

    #[test]
    fn ms_os_20_set_declares_winusb_and_the_interface_guid() {
        let d = &MS_OS_20_DESCRIPTOR_SET[..];
        assert_eq!(&d[14..20], b"WINUSB", "the CompatibleID Windows matches winusb.inf on");
        assert_eq!(u16_at(d, 34), REG_MULTI_SZ, "DeviceInterfaceGUIDs is a REG_MULTI_SZ");
        assert!(contains(d, &utf16(PROPERTY_NAME)), "the registry property name is present as UTF-16LE");
        assert!(contains(d, &utf16(WINUSB_INTERFACE_GUID)), "the interface GUID is present as UTF-16LE");
        assert_eq!(u16_at(d, 80), DATA_BYTES as u16);
        assert_eq!(&d[158..162], &[0, 0, 0, 0], "REG_MULTI_SZ double-NUL terminator");
    }

    #[test]
    fn bos_embeds_the_capability_payloads_at_their_documented_offsets() {
        assert_eq!(&BOS_DESCRIPTOR[8..33], &MS_OS_20_PLATFORM_CAPABILITY[..]);
        assert_eq!(&BOS_DESCRIPTOR[36..57], &WEBUSB_PLATFORM_CAPABILITY[..]);
        assert_eq!(BOS_DESCRIPTOR[5] as usize, 3 + MS_OS_20_PLATFORM_CAPABILITY.len());
        assert_eq!(BOS_DESCRIPTOR[33] as usize, 3 + WEBUSB_PLATFORM_CAPABILITY.len());
    }

    #[test]
    fn bos_advertises_ms_os_20_and_webusb() {
        let b = &BOS_DESCRIPTOR[..];
        assert_eq!(b[1], BOS);
        assert_eq!(b[4], 2, "two platform capabilities");
        assert_eq!(u16_at(b, 2), b.len() as u16, "BOS wTotalLength");
        assert_eq!(b[5], 28);
        assert!(contains(b, &MS_OS_20_UUID), "MS OS 2.0 platform UUID");
        assert_eq!(u16_at(b, 5 + 24), MS_OS_20_SET_LEN as u16, "wMSOSDescriptorSetTotalLength == the set");
        assert_eq!(b[5 + 26], MS_VENDOR_CODE);
        assert_eq!(b[33], 24);
        assert!(contains(b, &WEBUSB_UUID), "WebUSB platform UUID");
        assert_eq!(b[33 + 22], WEBUSB_VENDOR_CODE);
    }
}
