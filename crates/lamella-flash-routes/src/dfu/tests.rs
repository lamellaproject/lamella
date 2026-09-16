
use super::scripted::{Scripted, Sent, status};
use super::*;

const DNLOAD: u8 = 1;
const UPLOAD: u8 = 2;
const GETSTATUS: u8 = 3;
const CLRSTATUS: u8 = 4;
const ABORT: u8 = 6;

const OK: u8 = 0x00;
const ERR_TARGET: u8 = 0x01;
const DFU_IDLE: u8 = 2;
const DFU_DNBUSY: u8 = 4;
const DFU_DNLOAD_IDLE: u8 = 5;
const DFU_MANIFEST: u8 = 7;
const DFU_ERROR: u8 = 10;

fn get_status() -> Sent {
    Sent::In { request: GETSTATUS, value: 0, length: 6 }
}

fn abort() -> Sent {
    Sent::Out { request: ABORT, value: 0, data: vec![] }
}

/// What one download costs on the wire (AN3156 Rev 18, 5.1 and 5.2): the download itself, a status
/// request the device must answer busy, the poll wait, and a second status request.
fn download(value: u16, bytes: &[u8], poll_timeout_ms: u32) -> Vec<Sent> {
    vec![
        Sent::Out { request: DNLOAD, value, data: bytes.to_vec() },
        get_status(),
        Sent::Pause(poll_timeout_ms),
        get_status(),
    ]
}

/// The two replies a download that succeeds receives.
fn busy_then_idle(poll_timeout_ms: u32) -> Vec<Vec<u8>> {
    vec![status(OK, poll_timeout_ms, DFU_DNBUSY), status(OK, 0, DFU_DNLOAD_IDLE)]
}

/// The poll timeout is three bytes, least significant first, between the status and the state.
#[test]
fn a_status_reply_decodes_its_three_byte_poll_timeout() {
    let report = StatusReport::parse(&[0x00, 0x34, 0x12, 0x01, 0x05, 0x00]).expect("six bytes");
    assert_eq!(report.status, Status::Ok);
    assert_eq!(report.poll_timeout_ms, 0x0001_1234);
    assert_eq!(report.state, State::DfuDnloadIdle);
    assert!(StatusReport::parse(&[0x00, 0x00, 0x00, 0x00, 0x02]).is_err(), "five bytes is no reply");
}

/// Every status and state DFU 1.1 section 6.1.2 lists decodes to its own variant, in the listed
/// order, and a value past the lists is kept rather than folded into a listed one.
#[test]
fn every_listed_status_and_state_decodes_by_its_value() {
    let statuses = [
        Status::Ok,
        Status::ErrTarget,
        Status::ErrFile,
        Status::ErrWrite,
        Status::ErrErase,
        Status::ErrCheckErased,
        Status::ErrProg,
        Status::ErrVerify,
        Status::ErrAddress,
        Status::ErrNotDone,
        Status::ErrFirmware,
        Status::ErrVendor,
        Status::ErrUsbr,
        Status::ErrPor,
        Status::ErrUnknown,
        Status::ErrStalledPkt,
    ];
    for (value, expected) in (0u8..).zip(statuses) {
        assert_eq!(Status::from_byte(value), expected, "status {value:#04x}");
    }
    assert_eq!(Status::from_byte(0x10), Status::Other(0x10));

    let states = [
        State::AppIdle,
        State::AppDetach,
        State::DfuIdle,
        State::DfuDnloadSync,
        State::DfuDnbusy,
        State::DfuDnloadIdle,
        State::DfuManifestSync,
        State::DfuManifest,
        State::DfuManifestWaitReset,
        State::DfuUploadIdle,
        State::DfuError,
    ];
    for (value, expected) in (0u8..).zip(states) {
        assert_eq!(State::from_byte(value), expected, "state {value}");
    }
    assert_eq!(State::from_byte(11), State::Other(11));
}

/// Set Address Pointer is `0x21` and the address least significant byte first, run by the status
/// requests that follow it (AN3156 Rev 18, 5.2).
#[test]
fn set_address_pointer_is_command_0x21_with_the_address_least_significant_byte_first() {
    let mut dfu = DfuSe::new(Scripted::answering(busy_then_idle(7)), 1024);
    dfu.set_address(0x0810_0000).expect("the device took the command");
    assert_eq!(dfu.into_pipe().sent, download(0, &[0x21, 0x00, 0x00, 0x10, 0x08], 7));
}

/// A page erase is `0x41` and the page address (AN3156 Rev 18, 5.3).
#[test]
fn a_page_erase_is_command_0x41_with_the_page_address() {
    let mut dfu = DfuSe::new(Scripted::answering(busy_then_idle(0)), 1024);
    dfu.erase_page(0x0802_0000).expect("the device took the command");
    assert_eq!(dfu.into_pipe().sent, download(0, &[0x41, 0x00, 0x00, 0x02, 0x08], 0));
}

/// Every block goes as block 2 at a pointer set for it, so the address the device computes --
/// `(wBlockNum - 2) * buffer length + pointer` (AN3156 Rev 18, 5.1) -- is the pointer, and a short
/// final block lands where it belongs.
#[test]
fn every_block_is_sent_as_block_two_at_a_pointer_set_for_it() {
    let replies = (0..4).flat_map(|_| busy_then_idle(0)).collect();
    let mut dfu = DfuSe::new(Scripted::answering(replies), 4);
    dfu.write(0x0800_0000, &[1, 2, 3, 4, 5, 6]).expect("both blocks taken");
    let mut expected = download(0, &[0x21, 0x00, 0x00, 0x00, 0x08], 0);
    expected.extend(download(2, &[1, 2, 3, 4], 0));
    expected.extend(download(0, &[0x21, 0x04, 0x00, 0x00, 0x08], 0));
    expected.extend(download(2, &[5, 6], 0));
    assert_eq!(dfu.into_pipe().sent, expected);
}

/// A write refuses what AN3156 Rev 18, 5.1 does not allow -- nothing to write, a block of one byte,
/// a transfer size past 2048 bytes -- before anything is sent.
#[test]
fn a_write_the_bootloader_cannot_take_is_refused_before_anything_is_sent() {
    let mut empty = DfuSe::new(Scripted::answering(vec![]), 4);
    assert!(empty.write(0x0800_0000, &[]).is_err(), "nothing to write");
    let mut one_byte_tail = DfuSe::new(Scripted::answering(vec![]), 4);
    let refusal = one_byte_tail.write(0x0800_0000, &[1, 2, 3, 4, 5]).expect_err("a one-byte block");
    assert!(refusal.to_string().contains("2 to 2048"), "names the allowed sizes: {refusal}");
    let mut too_large = DfuSe::new(Scripted::answering(vec![]), 4096);
    assert!(too_large.write(0x0800_0000, &[0; 8]).is_err(), "a transfer size past 2048");
    for dfu in [empty, one_byte_tail, too_large] {
        assert!(dfu.into_pipe().sent.is_empty(), "nothing reached the pipe");
    }
}

/// A command the device does not start is refused with what the device answered and what was
/// expected, named as the class specification names them.
#[test]
fn a_command_the_device_does_not_start_is_refused_naming_its_answer() {
    let mut dfu = DfuSe::new(Scripted::answering(vec![status(ERR_TARGET, 0, DFU_ERROR)]), 1024);
    let refusal = dfu.set_address(0x9000_0000).expect_err("the device refused the address");
    let text = refusal.to_string();
    assert!(text.contains("errTARGET"), "names the status: {text}");
    assert!(text.contains("dfuERROR"), "names the state: {text}");
    assert!(text.contains("dfuDNBUSY"), "names what was expected: {text}");
}

/// A device reporting an error is cleared, and one left mid-download is aborted, each until it
/// reports idle (DFU 1.1, 6.1.3, 6.1.4, A.2.6 and A.2.11).
#[test]
fn reaching_idle_clears_an_error_and_aborts_a_download() {
    let replies = vec![status(ERR_TARGET, 0, DFU_ERROR), status(OK, 0, DFU_IDLE)];
    let mut in_error = DfuSe::new(Scripted::answering(replies), 1024);
    in_error.to_idle().expect("cleared");
    let clear = Sent::Out { request: CLRSTATUS, value: 0, data: vec![] };
    assert_eq!(in_error.into_pipe().sent, vec![get_status(), clear, get_status()]);

    let replies = vec![status(OK, 0, DFU_DNLOAD_IDLE), status(OK, 0, DFU_IDLE)];
    let mut mid_download = DfuSe::new(Scripted::answering(replies), 1024);
    mid_download.to_idle().expect("aborted");
    assert_eq!(mid_download.into_pipe().sent, vec![get_status(), abort(), get_status()]);
}

/// A read sets the pointer for each block, aborts the download that set it, uploads the block as
/// block 2, and aborts the upload -- each request in a state DFU 1.1 A.2.3, A.2.6 and A.2.10 accept
/// it in.
#[test]
fn a_read_uploads_each_block_as_block_two_between_aborts() {
    let mut replies = busy_then_idle(0);
    replies.push(vec![1, 2, 3, 4]);
    replies.extend(busy_then_idle(0));
    replies.push(vec![5, 6]);
    let mut dfu = DfuSe::new(Scripted::answering(replies), 4);
    assert_eq!(dfu.read(0x0800_0000, 6).expect("both blocks read"), vec![1, 2, 3, 4, 5, 6]);
    let mut expected = download(0, &[0x21, 0x00, 0x00, 0x00, 0x08], 0);
    expected.extend([abort(), Sent::In { request: UPLOAD, value: 2, length: 4 }, abort()]);
    expected.extend(download(0, &[0x21, 0x04, 0x00, 0x00, 0x08], 0));
    expected.extend([abort(), Sent::In { request: UPLOAD, value: 2, length: 2 }, abort()]);
    assert_eq!(dfu.into_pipe().sent, expected);
}

/// A block that comes back shorter than asked is refused rather than padded or read past.
#[test]
fn a_short_upload_is_refused() {
    let mut replies = busy_then_idle(0);
    replies.push(vec![1, 2]);
    let mut dfu = DfuSe::new(Scripted::answering(replies), 4);
    assert!(dfu.read(0x0800_0000, 4).is_err());
}

/// Leaving sets the pointer to the image, sends a download of no data, and expects the device to
/// report that it is manifesting; a device that does not has not left (AN3156 Rev 18, 5.5).
#[test]
fn leaving_sets_the_pointer_then_downloads_nothing_and_expects_manifest() {
    let mut replies = busy_then_idle(0);
    replies.push(status(OK, 0, DFU_MANIFEST));
    let mut dfu = DfuSe::new(Scripted::answering(replies), 1024);
    dfu.leave(0x0800_0000).expect("the device manifests");
    let mut expected = download(0, &[0x21, 0x00, 0x00, 0x00, 0x08], 0);
    expected.extend([Sent::Out { request: DNLOAD, value: 0, data: vec![] }, get_status()]);
    assert_eq!(dfu.into_pipe().sent, expected);

    let mut replies = busy_then_idle(0);
    replies.push(status(OK, 0, DFU_DNLOAD_IDLE));
    let mut not_leaving = DfuSe::new(Scripted::answering(replies), 1024);
    assert!(not_leaving.leave(0x0800_0000).is_err(), "a device that does not manifest has not left");
}

/// The status request that runs a leave can fail as the device goes: the leave "is effectively
/// executed only when a DFU_GETSTATUS request is issued by the host", after which the device
/// disconnects and "is unable to respond to host requests after a manifestation phase is completed"
/// (AN3156 Rev 18, 5.5). A leave whose status request fails at the pipe is taken as made, and nothing
/// is sent after that request.
#[test]
fn a_leave_whose_status_request_fails_at_the_pipe_is_taken_as_made() {
    let mut replies: Vec<Result<Vec<u8>, DfuError>> =
        busy_then_idle(0).into_iter().map(Ok).collect();
    replies.push(Err(DfuError::Transport(
        "the device stalled request 0x03 (bmRequestType 0xa1, wValue 0x0000, wIndex 0x0000)"
            .to_owned(),
    )));
    let mut dfu = DfuSe::new(Scripted::answering_each(replies), 1024);
    dfu.leave(0x0800_0000)
        .expect("a leave the device answers by going");
    let mut expected = download(0, &[0x21, 0x00, 0x00, 0x00, 0x08], 0);
    expected.extend([
        Sent::Out {
            request: DNLOAD,
            value: 0,
            data: vec![],
        },
        get_status(),
    ]);
    assert_eq!(dfu.into_pipe().sent, expected);
}

/// A DFU-mode configuration: the configuration descriptor (USB 2.0, Table 9-10), the DFU interface
/// descriptor (DFU 1.1, Table 4.4), and a functional descriptor (DFU 1.1, Table 4.2) stating download
/// and upload, no manifestation tolerance, will-detach, a 255 ms detach timeout, a transfer size of
/// 2,048 bytes and version 0x011A.
const CONFIGURATION: [u8; 27] = [
    0x09, 0x02, 0x1B, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32,
    0x09, 0x04, 0x00, 0x00, 0x00, 0xFE, 0x01, 0x02, 0x04,
    0x09, 0x21, 0x0B, 0xFF, 0x00, 0x00, 0x08, 0x1A, 0x01,
];

/// Each field of the functional descriptor is read from its own offset, least significant byte
/// first, and each attribute from its own bit (DFU 1.1, Table 4.2).
#[test]
fn a_functional_descriptor_decodes_each_field_from_its_offset() {
    let functional = FunctionalDescriptor::find(&CONFIGURATION).expect("the configuration carries one");
    assert_eq!(
        functional,
        FunctionalDescriptor {
            can_download: true,
            can_upload: true,
            manifestation_tolerant: false,
            will_detach: true,
            detach_timeout_ms: 0x00FF,
            transfer_size: 2048,
            dfu_version: 0x011A,
        }
    );
}

/// A configuration with no functional descriptor, or with one shorter than nine bytes, is refused
/// rather than read past.
#[test]
fn a_configuration_without_a_whole_functional_descriptor_is_refused() {
    assert!(FunctionalDescriptor::find(&CONFIGURATION[..18]).is_err(), "no functional descriptor");
    let mut short = CONFIGURATION;
    short[18] = 7;
    let refusal = FunctionalDescriptor::find(&short[..25]).expect_err("a seven-byte functional descriptor");
    assert!(refusal.to_string().contains("nine bytes"), "{refusal}");
}

/// Every DFU request goes to the interface as a class request, with the interface's number in
/// `wIndex` (DFU 1.1, section 3).
#[test]
fn every_dfu_request_is_a_class_request_to_its_interface() {
    let request = class_request(DNLOAD, 2, 3);
    assert_eq!(request.kind, lamella_usbbulk::RequestKind::Class);
    assert_eq!(request.recipient, lamella_usbbulk::Recipient::Interface);
    assert_eq!((request.request, request.value, request.index), (DNLOAD, 2, 3));
}

const VENDOR: u16 = 0x0483;

/// An attached DFU interface as the listing reports it.
fn attached(vendor_id: u16, serial: Option<&str>) -> lamella_usbbulk::InterfaceInfo {
    lamella_usbbulk::InterfaceInfo {
        vendor_id,
        product_id: 0x0001,
        serial_number: serial.map(str::to_owned),
        product: Some("a bootloader".to_owned()),
        interface_number: 0,
        interface_name: None,
    }
}

/// A named serial picks the device reporting it, whole and without regard to case; a serial that is
/// only part of one names nothing, and the refusal lists what is attached.
#[test]
fn a_named_serial_picks_its_device_whole_and_without_regard_to_case() {
    let two = [attached(VENDOR, Some("AAAA1111")), attached(VENDOR, Some("BBBB2222"))];
    let chosen = choose_device(&two, VENDOR, Some("bbbb2222")).expect("named");
    assert_eq!(chosen.serial_number.as_deref(), Some("BBBB2222"));
    let refusal = choose_device(&two, VENDOR, Some("BBBB")).expect_err("a part of a serial names nothing");
    assert!(refusal.contains("serial BBBB."), "names the serial asked for: {refusal}");
    assert!(refusal.contains("AAAA1111") && refusal.contains("BBBB2222"), "lists what is attached: {refusal}");
    assert!(refusal.contains("--device <serial>"), "and says how to name one: {refusal}");
}

/// With no serial named, only a sole device is taken; several are refused with every one listed, and
/// none attached is refused.
#[test]
fn with_no_serial_named_only_a_sole_device_is_taken() {
    let one = [attached(VENDOR, Some("AAAA1111"))];
    assert!(choose_device(&one, VENDOR, None).is_ok());
    let two = [attached(VENDOR, Some("AAAA1111")), attached(VENDOR, None)];
    let refusal = choose_device(&two, VENDOR, None).expect_err("two attached, none named");
    assert!(refusal.contains("AAAA1111") && refusal.contains("(none reported)"), "{refusal}");
    assert!(refusal.contains("--device <serial>"), "and says how to name one: {refusal}");
    assert!(choose_device(&[], VENDOR, None).is_err(), "none attached");
}

/// A DFU interface under another vendor is not a candidate, named or not.
#[test]
fn another_vendors_dfu_interface_is_not_a_candidate() {
    let other = attached(VENDOR + 1, Some("CCCC3333"));
    let mixed = [other.clone(), attached(VENDOR, Some("AAAA1111"))];
    assert_eq!(choose_device(&mixed, VENDOR, None).expect("the sole one of this vendor").vendor_id, VENDOR);
    assert!(choose_device(&[other], VENDOR, Some("CCCC3333")).is_err(), "another vendor's serial");
}
