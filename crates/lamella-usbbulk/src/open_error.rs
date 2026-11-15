//! Actionable reasons for a refused USB open.

use crate::Error;

#[cfg(any(target_os = "linux", test))]
pub(crate) fn linux_open_failure(node: &str, error: &std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return Error::Os(format!(
            "permission denied for {node}; ask your administrator to add a udev rule granting your account \
             access, then reconnect the device"
        ));
    }
    Error::Os(format!("open {node}: {error}"))
}

/// What a refused claim of a usbfs interface comes to: [`Error::Held`] for `EBUSY`, which Linux gives
/// when the interface is already bound, to another program's claim or to a kernel driver, and
/// [`Error::Os`] for any other.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn claim_refusal(error: &std::io::Error) -> Error {
    /// `EBUSY`, as Linux's `include/uapi/asm-generic/errno-base.h` numbers it.
    const EBUSY: i32 = 16;
    let said = format!("USBDEVFS_CLAIMINTERFACE: {error}");
    if error.raw_os_error() == Some(EBUSY) { Error::Held(said) } else { Error::Os(said) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_refused_as_busy_is_held_and_keeps_its_words() {
        let busy = std::io::Error::from_raw_os_error(16);
        let typed = claim_refusal(&busy);
        assert!(matches!(typed, Error::Held(_)), "{typed:?}");
        assert_eq!(typed.to_string(), format!("USB error: USBDEVFS_CLAIMINTERFACE: {busy}"));
        let other = std::io::Error::from_raw_os_error(19);
        let typed = claim_refusal(&other);
        assert!(matches!(typed, Error::Os(_)), "{typed:?}");
        assert_eq!(typed.to_string(), format!("USB error: USBDEVFS_CLAIMINTERFACE: {other}"));
    }

    #[test]
    fn a_linux_permission_refusal_says_how_to_get_access() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let said = linux_open_failure("/dev/fixture", &error).to_string();
        assert_eq!(said, "USB error: permission denied for /dev/fixture; ask your administrator to add a udev rule granting your account access, then reconnect the device");
    }

    #[test]
    fn a_linux_open_failure_without_permission_denied_keeps_its_reason() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        let said = linux_open_failure("/dev/fixture", &error).to_string();
        assert!(said.contains("/dev/fixture") && said.contains(&error.to_string()), "{said}");
        assert!(!said.contains("udev"), "{said}");
    }
}
