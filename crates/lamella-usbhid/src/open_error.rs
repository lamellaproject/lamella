//! Actionable reasons for a refused USB open.

use crate::Error;

#[cfg(any(target_os = "linux", test))]
pub(crate) fn linux_open_failure(path: &str, error: &std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return Error::Os(format!(
            "permission denied for {path}; ask your administrator to add a udev rule granting your account \
             access, then reconnect the device"
        ));
    }
    Error::Os(format!("open {path}: {error}"))
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_open_failure(status: i32) -> Error {
    let advice = if status == 0xe000_02c5_u32 as i32 {
        "another program holds it; close that program and try again"
    } else if matches!(status as u32, 0xe000_02c1 | 0xe000_02e2) {
        "macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks"
    } else {
        return Error::Os(format!("IOHIDDeviceOpen returned {:#010x}", status as u32));
    };
    Error::Os(format!("{advice} (IOHIDDeviceOpen: {:#010x})", status as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_permission_refusal_says_how_to_get_access() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let said = linux_open_failure("/dev/fixture", &error).to_string();
        assert_eq!(said, "HID error: permission denied for /dev/fixture; ask your administrator to add a udev rule granting your account access, then reconnect the device");
    }

    #[test]
    fn a_linux_open_failure_without_permission_denied_keeps_its_reason() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        let said = linux_open_failure("/dev/fixture", &error).to_string();
        assert!(said.contains("/dev/fixture") && said.contains(&error.to_string()), "{said}");
        assert!(!said.contains("udev"), "{said}");
    }

    #[test]
    fn a_macos_permission_refusal_says_what_to_do_and_keeps_the_status() {
        assert_eq!(macos_open_failure(0xe000_02c1_u32 as i32).to_string(), "HID error: macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks (IOHIDDeviceOpen: 0xe00002c1)");
        assert_eq!(macos_open_failure(0xe000_02e2_u32 as i32).to_string(), "HID error: macOS refused access; close apps using the device, reconnect it, and allow the accessory if macOS asks (IOHIDDeviceOpen: 0xe00002e2)");
    }

    #[test]
    fn other_macos_open_failures_do_not_claim_a_permission_refusal() {
        for status in [0xe000_02d6_u32, 0xe000_02c0, 0xe000_02ed, 0xe000_02bc] {
            assert_eq!(macos_open_failure(status as i32).to_string(), format!("HID error: IOHIDDeviceOpen returned {status:#010x}"));
        }
    }

    #[test]
    fn a_macos_held_device_says_to_close_its_owner() {
        assert_eq!(macos_open_failure(0xe000_02c5_u32 as i32).to_string(), "HID error: another program holds it; close that program and try again (IOHIDDeviceOpen: 0xe00002c5)");
    }
}
