//! The attached serial ports on Linux, read from sysfs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{Error, PortInfo, UsbIdentity};

/// Lists the serial ports the kernel reports.
pub(crate) fn list() -> Result<Vec<PortInfo>, Error> {
    list_under(Path::new("/sys"))
}

/// Lists the serial ports in the sysfs tree at `sys`.
fn list_under(sys: &Path) -> Result<Vec<PortInfo>, Error> {
    let class = sys.join("class").join("tty");
    let devices = sys.join("devices");
    let failed = |error: io::Error| Error::List {
        what: "reading the tty class in sysfs",
        code: os_code(&error),
    };
    let entries = fs::read_dir(&class).map_err(failed)?;
    let mut ports = Vec::new();
    for entry in entries {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(device) = fs::canonicalize(entry.path()) else { continue };
        if !device.starts_with(&devices)
            || (device.starts_with(devices.join("virtual")) && !is_bound_bluetooth_port(&device))
        {
            continue;
        }
        if is_unknown_uart(&device) {
            continue;
        }
        ports.push(PortInfo { name: format!("/dev/{name}"), usb: usb_device(&device, &devices) });
    }
    Ok(ports)
}

/// Whether `device` is a UART slot with no port in it.
///
/// A serial core driver registers a fixed number of ports whether or not hardware answers at each,
/// and publishes each one's type (sysfs-tty `type`, the `TIOCGSERIAL` type); `PORT_UNKNOWN`, zero
/// (include/uapi/linux/serial.h), is a slot with nothing found. A tty with no `type` attribute
/// is not a serial core port and is kept.
fn is_unknown_uart(device: &Path) -> bool {
    fs::read_to_string(device.join("type")).is_ok_and(|text| text.trim() == "0")
}

/// Whether `device` is a Bluetooth serial port bound to a remote device and not yet connected.
///
/// The rfcomm driver registers its tty with no parent, which puts it under "virtual" until a
/// connection moves it under the link's device, and gives it `address` and `channel` attributes
/// (net/bluetooth/rfcomm/tty.c, rfcomm_dev_add and rfcomm_reparent_device). Opening it makes the
/// connection (rfcomm_dev_activate), so it is a port a program can use, as the Bluetooth ports
/// Windows and macOS list are.
fn is_bound_bluetooth_port(device: &Path) -> bool {
    device.join("address").is_file() && device.join("channel").is_file()
}

/// The last element of the `subsystem` link of `directory`, its subsystem's name.
fn subsystem(directory: &Path) -> Option<String> {
    let target = fs::read_link(directory.join("subsystem")).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// An attribute of `directory`, without its trailing newline, or `None` when it is absent or empty.
fn attribute(directory: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(directory.join(name)).ok()?;
    let text = text.trim_end_matches('\n');
    (!text.is_empty()).then(|| text.to_string())
}

/// A hexadecimal id attribute, which the USB core writes as four digits.
fn hex_attribute(directory: &Path, name: &str) -> Option<u16> {
    u16::from_str_radix(&attribute(directory, name)?, 16).ok()
}

/// The USB device `device` belongs to: the first ancestor in the `usb` subsystem that carries
/// `idVendor`, which a USB device does and a USB interface does not, reached through USB's own
/// devices alone.
///
/// Its strings are its own: `serial`, `product`, absent when the device reported none. Nothing is
/// taken from a device further up, so a device without a serial number is not given its hub's.
///
/// **The walk stops at a device of any other subsystem**, so a tty under a device of another kind
/// is not given the identity of a USB device above that one: a Bluetooth link's rfcomm port sits
/// under the USB radio that carries the link, and it is the remote device's port, not the radio's.
/// (The Windows listing refuses the same case: its walk stops at the first node above the port that
/// is not USB.)
fn usb_device(device: &Path, devices: &Path) -> Option<UsbIdentity> {
    let mut directory: PathBuf = device.parent()?.to_path_buf();
    while directory.starts_with(devices) && directory != devices {
        match subsystem(&directory).as_deref() {
            Some("usb") if directory.join("idVendor").is_file() => {
                return Some(UsbIdentity {
                    vendor_id: hex_attribute(&directory, "idVendor")?,
                    product_id: hex_attribute(&directory, "idProduct")?,
                    serial_number: attribute(&directory, "serial"),
                    product: attribute(&directory, "product"),
                });
            }
            Some("usb" | "usb-serial") | None => {}
            Some(_) => return None,
        }
        directory = directory.parent()?.to_path_buf();
    }
    None
}

/// The operating system's code for an I/O failure, as the error variants carry it.
fn os_code(error: &io::Error) -> u32 {
    error.raw_os_error().unwrap_or(0).unsigned_abs()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A sysfs-shaped tree in a fresh directory, removed when dropped.
    struct FakeSys {
        root: PathBuf,
    }

    impl FakeSys {
        fn new(tag: &str) -> FakeSys {
            let root = std::env::temp_dir()
                .join(format!("lamella-serial-port-sysfs-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("class/tty")).expect("a temporary directory");
            fs::create_dir_all(root.join("bus/usb")).expect("a temporary directory");
            fs::create_dir_all(root.join("bus/platform")).expect("a temporary directory");
            FakeSys { root }
        }

        /// A device directory at `path` under `devices`, in `subsystem`, with these attributes.
        fn device(&self, path: &str, subsystem: &str, attributes: &[(&str, &str)]) -> PathBuf {
            let directory = self.root.join("devices").join(path);
            fs::create_dir_all(&directory).expect("a device directory");
            symlink(self.root.join("bus").join(subsystem), directory.join("subsystem"))
                .expect("a subsystem link");
            for (name, value) in attributes {
                fs::write(directory.join(name), format!("{value}\n")).expect("an attribute");
            }
            directory
        }

        /// A tty class entry `name` linking to the device directory at `path`.
        fn tty(&self, name: &str, path: &str, attributes: &[(&str, &str)]) {
            let directory = self.root.join("devices").join(path);
            fs::create_dir_all(&directory).expect("a tty directory");
            for (attribute, value) in attributes {
                fs::write(directory.join(attribute), format!("{value}\n")).expect("an attribute");
            }
            symlink(&directory, self.root.join("class/tty").join(name)).expect("a class link");
        }
    }

    impl Drop for FakeSys {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// The shapes a real machine has: a USB CDC port under its interface, a USB serial converter's
    /// port two levels down, a motherboard UART, an empty UART slot, and a virtual console.
    #[test]
    fn each_kind_of_tty_is_listed_or_left_out_by_what_is_behind_it() {
        let sys = FakeSys::new("kinds");
        sys.device(
            "pci0000:00/usb1/1-2",
            "usb",
            &[("idVendor", "2e8a"), ("idProduct", "000c"), ("serial", "0123456789ABCDEF"), ("product", "Debug Probe (CMSIS-DAP)")],
        );
        sys.device("pci0000:00/usb1/1-2/1-2:1.1", "usb", &[("bInterfaceClass", "0a")]);
        sys.tty("ttyACM0", "pci0000:00/usb1/1-2/1-2:1.1/tty/ttyACM0", &[]);
        sys.device("pci0000:00/usb1/1-3", "usb", &[("idVendor", "0403"), ("idProduct", "6001")]);
        sys.device("pci0000:00/usb1/1-3/1-3:1.0", "usb", &[("bInterfaceClass", "ff")]);
        sys.device("pci0000:00/usb1/1-3/1-3:1.0/ttyUSB0", "usb-serial", &[]);
        sys.tty("ttyUSB0", "pci0000:00/usb1/1-3/1-3:1.0/ttyUSB0/tty/ttyUSB0", &[]);
        sys.tty("ttyS0", "pnp0/00:05/tty/ttyS0", &[("type", "4")]);
        sys.tty("ttyS3", "platform/serial8250/tty/ttyS3", &[("type", "0")]);
        sys.tty("tty0", "virtual/tty/tty0", &[]);

        let mut ports = list_under(&sys.root).expect("the fake tree lists");
        ports.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            ports,
            vec![
                PortInfo {
                    name: String::from("/dev/ttyACM0"),
                    usb: Some(UsbIdentity {
                        vendor_id: 0x2e8a,
                        product_id: 0x000c,
                        serial_number: Some(String::from("0123456789ABCDEF")),
                        product: Some(String::from("Debug Probe (CMSIS-DAP)")),
                    }),
                },
                PortInfo { name: String::from("/dev/ttyS0"), usb: None },
                PortInfo {
                    name: String::from("/dev/ttyUSB0"),
                    usb: Some(UsbIdentity {
                        vendor_id: 0x0403,
                        product_id: 0x6001,
                        serial_number: None,
                        product: None,
                    }),
                },
            ]
        );
    }

    /// **A device without a serial number is not given its hub's.** The walk stops at the first USB
    /// device, so a hub above it that does report one is never read.
    #[test]
    fn a_device_without_a_serial_number_does_not_borrow_one_from_its_hub() {
        let sys = FakeSys::new("hub");
        sys.device("pci0000:00/usb1/1-1", "usb", &[("idVendor", "05e3"), ("idProduct", "0610"), ("serial", "HUBSERIAL")]);
        sys.device("pci0000:00/usb1/1-1/1-1.4", "usb", &[("idVendor", "0d28"), ("idProduct", "0204")]);
        sys.device("pci0000:00/usb1/1-1/1-1.4/1-1.4:1.1", "usb", &[]);
        sys.tty("ttyACM1", "pci0000:00/usb1/1-1/1-1.4/1-1.4:1.1/tty/ttyACM1", &[]);
        let ports = list_under(&sys.root).expect("the fake tree lists");
        let usb = ports[0].usb.as_ref().expect("a USB port");
        assert_eq!((usb.vendor_id, usb.product_id), (0x0d28, 0x0204));
        assert_eq!(usb.serial_number, None, "the hub's serial is not the device's");
    }

    /// **A Bluetooth port under a USB radio is not given the radio's identity.** The walk from the
    /// rfcomm tty meets the Bluetooth link's own devices before the radio, and stops there.
    #[test]
    fn a_bluetooth_port_under_a_usb_radio_is_not_given_the_radios_identity() {
        let sys = FakeSys::new("rfcomm");
        sys.device("pci0000:00/usb1/1-4", "usb", &[("idVendor", "8087"), ("idProduct", "0a2b")]);
        sys.device("pci0000:00/usb1/1-4/1-4:1.0", "usb", &[("bInterfaceClass", "e0")]);
        sys.device("pci0000:00/usb1/1-4/1-4:1.0/bluetooth/hci0", "bluetooth", &[]);
        sys.device("pci0000:00/usb1/1-4/1-4:1.0/bluetooth/hci0/hci0:12", "bluetooth", &[]);
        sys.tty("rfcomm0", "pci0000:00/usb1/1-4/1-4:1.0/bluetooth/hci0/hci0:12/tty/rfcomm0", &[]);
        let ports = list_under(&sys.root).expect("the fake tree lists");
        assert_eq!(ports, vec![PortInfo { name: String::from("/dev/rfcomm0"), usb: None }]);
    }

    /// **A Bluetooth port bound and waiting for its connection is listed**, though it sits under
    /// "virtual" with the consoles until the connection moves it; a virtual tty without its two
    /// attributes still is not.
    #[test]
    fn a_bluetooth_port_waiting_for_its_connection_is_listed() {
        let sys = FakeSys::new("bound");
        sys.tty("rfcomm0", "virtual/tty/rfcomm0", &[("address", "00:1a:7d:da:71:13"), ("channel", "1")]);
        sys.tty("tty1", "virtual/tty/tty1", &[]);
        let ports = list_under(&sys.root).expect("the fake tree lists");
        assert_eq!(ports, vec![PortInfo { name: String::from("/dev/rfcomm0"), usb: None }]);
    }

    /// A tree with no tty class cannot be listed, and says why rather than answering "no ports".
    #[test]
    fn a_tree_without_a_tty_class_is_an_error_rather_than_an_empty_list() {
        let missing = std::env::temp_dir().join(format!("lamella-serial-port-nosys-{}", std::process::id()));
        assert!(matches!(list_under(&missing), Err(Error::List { .. })));
    }
}
