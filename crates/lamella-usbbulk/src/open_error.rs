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

#[cfg(test)]
mod tests {
    use super::*;

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
