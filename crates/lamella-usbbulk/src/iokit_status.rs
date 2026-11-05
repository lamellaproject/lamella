//! What an IOKit status from a bulk pipe call means to a caller: a transfer that timed out is
//! [`Error::Timeout`], and every other failure is [`Error::Os`], naming the call and the status. And
//! how a finished read's bytes reach the buffers of the reads that collect them.
//!
//! The codes are the values `IOReturn.h` and `USB.h` give them, in the macOS SDK. This file names no
//! macOS interface, so it builds and its tests run on every host.
#![allow(non_upper_case_globals)]

use crate::Error;

pub(crate) const kIOReturnTimeout: i32 = 0xe000_02d6_u32 as i32;
pub(crate) const kIOUSBTransactionTimeout: i32 = 0xe000_4051_u32 as i32;
pub(crate) const kIOUSBPipeStalled: i32 = 0xe000_404f_u32 as i32;
pub(crate) const kIOReturnUnderrun: i32 = 0xe000_02e7_u32 as i32;

pub(crate) const kIOReturnExclusiveAccess: i32 = 0xe000_02c5_u32 as i32;

/// What to do when macOS refuses an open, keeping the call and status for diagnosis.
pub(crate) fn open_refused(call: &str, status: i32) -> String {
    let advice = if status == kIOReturnExclusiveAccess {
        "another program holds it; close that program and try again"
    } else if matches!(status as u32, 0xe000_02c1 | 0xe000_02e2) {
        "macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks"
    } else {
        return format!("{call} returned {:#010x}", status as u32);
    };
    format!("{advice} ({call}: {:#010x})", status as u32)
}

/// Whether `status` says that a transfer ran out of time, and nothing else about the device.
pub(crate) fn is_timeout(status: i32) -> bool {
    status == kIOReturnTimeout || status == kIOUSBTransactionTimeout
}

/// The error for a bulk pipe call that returned `status`, which is not success.
///
/// **Only a transfer that timed out is [`Error::Timeout`]**, because a caller reads that as "the
/// device is there and had nothing to say" and keeps its session. A device that has gone away
/// answers with another code, and it must end the session at once: it is [`Error::Os`], with the
/// status kept, so a report says which failure it was.
pub(crate) fn pipe_failure(call: &str, status: i32) -> Error {
    if is_timeout(status) {
        Error::Timeout
    } else {
        Error::Os(format!("{call} returned {:#010x}", status as u32))
    }
}

/// Hands a finished read to the caller: on success, what it delivered goes into `buf`, and what `buf`
/// has no room for into `left`, for the next read. A read that ended on a short packet may report
/// `kIOReturnUnderrun`, and what it delivered is what there was, so that is success too.
///
/// **ANY OTHER STATUS IS A FAILURE THAT KEEPS ITS CODE, A TIMEOUT'S TOO.** This read was given no timer, so
/// it has no time to run out of: a status saying one ran out came from below, and reading it as a quiet
/// pipe would start another transfer behind it, again and again, until a session's question failed.
pub(crate) fn finished_read(status: i32, delivered: &[u8], buf: &mut [u8], left: &mut Vec<u8>) -> Result<usize, Error> {
    if status != 0 && status != kIOReturnUnderrun {
        return Err(Error::Os(format!("ReadPipeAsync returned {:#010x}", status as u32)));
    }
    Ok(crate::hand_over(delivered, buf, left))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device another program holds is said in words, and any other refusal keeps its code.
    #[test]
    fn a_refused_open_says_when_another_program_holds_the_device() {
        assert_eq!(open_refused("USBDeviceOpen", kIOReturnExclusiveAccess), "another program holds it; close that program and try again (USBDeviceOpen: 0xe00002c5)");
        assert_eq!(open_refused("USBInterfaceOpen", kIOReturnTimeout), "USBInterfaceOpen returned 0xe00002d6");
    }

    #[test]
    fn a_macos_permission_refusal_says_what_to_do_and_keeps_the_status() {
        assert_eq!(open_refused("USBInterfaceOpen", 0xe000_02c1_u32 as i32), "macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks (USBInterfaceOpen: 0xe00002c1)");
        assert_eq!(open_refused("USBInterfaceOpen", 0xe000_02e2_u32 as i32), "macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks (USBInterfaceOpen: 0xe00002e2)");
    }

    #[test]
    fn other_open_failures_do_not_claim_a_permission_refusal() {
        for status in [kIOReturnTimeout, kIOReturnNoDevice, kIOReturnNotResponding, 0xe000_02bc_u32 as i32] {
            assert_eq!(open_refused("USBInterfaceOpen", status), format!("USBInterfaceOpen returned {:#010x}", status as u32));
        }
    }

    /// A read that ended on a short packet delivers what it held, however the OS words that.
    #[test]
    fn a_short_read_reported_as_an_underrun_delivers_what_it_held() {
        let mut left = Vec::new();
        let mut buf = [0u8; 4];
        assert_eq!(finished_read(kIOReturnUnderrun, b"ab", &mut buf, &mut left).unwrap(), 2);
        assert_eq!(&buf[..2], b"ab");
    }

    /// A read that finished hands over its bytes, and a transfer longer than the buffer of the read
    /// that collects it loses nothing: the rest comes with the next, in order.
    #[test]
    fn a_finished_read_delivers_everything_in_order_across_buffers_of_any_size() {
        let mut left = Vec::new();
        let mut buf = [0u8; 4];
        assert_eq!(finished_read(0, b"abc", &mut buf, &mut left).unwrap(), 3);
        assert_eq!(&buf[..3], b"abc");
        assert!(left.is_empty());

        assert_eq!(finished_read(0, b"abcdefghij", &mut buf, &mut left).unwrap(), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(left, b"efghij");
        assert_eq!(crate::take_left(&mut left, &mut buf), 4);
        assert_eq!(&buf, b"efgh");
        assert_eq!(crate::take_left(&mut left, &mut buf), 2);
        assert_eq!(&buf[..2], b"ij");
        assert!(left.is_empty());

        assert_eq!(finished_read(0, b"", &mut buf, &mut left).unwrap(), 0);
    }

    /// **A READ THAT FAILED DELIVERS NOTHING AND ENDS AS AN ERROR THAT KEEPS ITS CODE.** A device that
    /// went away while a read was outstanding is reported by that read.
    #[test]
    fn a_read_that_finished_with_a_failure_is_that_failure() {
        let mut left = Vec::new();
        let mut buf = [0u8; 4];
        match finished_read(kIOReturnNoDevice, b"", &mut buf, &mut left) {
            Err(Error::Os(message)) => assert_eq!(message, "ReadPipeAsync returned 0xe00002c0"),
            other => panic!("should be an OS error, not {other:?}"),
        }
        assert!(left.is_empty());
    }

    /// **A READ WITH NO TIMER HAS NO TIME TO RUN OUT OF.** Either timeout code on a read given no timer
    /// is a failure that keeps its code, so a session ends on it instead of reading a quiet pipe.
    #[test]
    fn a_timeout_status_on_a_read_with_no_timer_is_that_failure() {
        for status in [kIOReturnTimeout, kIOUSBTransactionTimeout] {
            let mut left = Vec::new();
            let mut buf = [0u8; 4];
            match finished_read(status, b"", &mut buf, &mut left) {
                Err(Error::Os(message)) => assert_eq!(message, format!("ReadPipeAsync returned {:#010x}", status as u32)),
                other => panic!("{status:#x} should be an OS error, not {other:?}"),
            }
        }
    }

    const kIOReturnNoDevice: i32 = 0xe000_02c0_u32 as i32;
    const kIOReturnNotOpen: i32 = 0xe000_02cd_u32 as i32;
    const kIOReturnOffline: i32 = 0xe000_02d7_u32 as i32;
    const kIOReturnAborted: i32 = 0xe000_02eb_u32 as i32;
    const kIOReturnNotResponding: i32 = 0xe000_02ed_u32 as i32;

    /// A quiet pipe is not a dead one: either code a timed-out transfer can return is a timeout, in
    /// both directions.
    #[test]
    fn a_transfer_that_timed_out_is_a_timeout() {
        for call in ["ReadPipeTO", "WritePipeTO"] {
            for status in [kIOReturnTimeout, kIOUSBTransactionTimeout] {
                assert!(
                    matches!(pipe_failure(call, status), Error::Timeout),
                    "{call} {status:#x} should be a timeout"
                );
            }
        }
    }

    /// **A DEVICE THAT HAS GONE AWAY MUST NOT READ AS A QUIET ONE.** Were one of these a timeout, a
    /// session on an unplugged board would poll forever instead of ending. Each stays an error that
    /// names the call and the code.
    #[test]
    fn a_device_that_went_away_is_never_a_timeout_and_the_error_keeps_the_code() {
        for status in [
            kIOReturnNoDevice,
            kIOReturnNotOpen,
            kIOReturnOffline,
            kIOReturnAborted,
            kIOReturnNotResponding,
            kIOUSBPipeStalled,
        ] {
            for call in ["ReadPipeTO", "WritePipeTO"] {
                match pipe_failure(call, status) {
                    Error::Os(message) => {
                        assert!(message.starts_with(&format!("{call} returned 0xe")), "{message}");
                        assert!(message.ends_with(&format!("{:08x}", status as u32)), "{message}");
                    }
                    other => panic!("{call} {status:#x} should be an OS error, not {other:?}"),
                }
            }
        }
    }
}
