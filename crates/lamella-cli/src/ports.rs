//! The serial ports attached, as the operating system reports them: no port is opened to list one.

use lamella_wire_host::SerialPortDesc;
#[cfg(any(target_os = "linux", all(test, unix)))]
use std::path::{Path, PathBuf};

/// Every serial port the operating system reports, with its USB identity where it has one: on Linux,
/// the USB ones alone.
#[cfg(target_os = "linux")]
pub(crate) fn attached() -> Vec<SerialPortDesc> {
    usb_ttys(Path::new("/sys/class/tty"), Path::new("/dev"))
}

/// Every serial port the operating system reports, with its USB identity where it has one.
#[cfg(not(target_os = "linux"))]
pub(crate) fn attached() -> Vec<SerialPortDesc> {
    lamella_wire_host::list_serial()
}

/// The port `target` names among `ports`: for `usb-serial:<id>`, the one port whose USB serial number
/// contains `<id>`, ignoring case; any other target is a port name already.
///
/// **A SERIAL THAT MATCHES SEVERAL PORTS IS REFUSED, NOT RESOLVED BY PICKING ONE**, since a write to
/// the wrong board succeeds. The rule is `lamella-wire-host`'s `resolve_target`, over this list.
///
/// # Errors
/// A serial number that matches no port, or more than one, each as the sentence a refusal says.
pub(crate) fn resolve(target: &str, ports: &[SerialPortDesc]) -> Result<String, String> {
    let Some(wanted) = target.strip_prefix("usb-serial:") else {
        return Ok(target.to_owned());
    };
    let needle = wanted.to_ascii_lowercase();
    let matching: Vec<&str> = ports
        .iter()
        .filter(|port| {
            port.serial_number.as_deref().is_some_and(|serial| serial.to_ascii_lowercase().contains(&needle))
        })
        .map(|port| port.port.as_str())
        .collect();
    match matching.as_slice() {
        [one] => Ok((*one).to_owned()),
        [] => Err(format!("{target} is not attached.")),
        many => Err(format!(
            "cannot open {target}: {wanted:?} is in the USB serial number of {} attached ports ({}); give more of \
             it, or the port's name.",
            many.len(),
            many.join(", ")
        )),
    }
}

/// The ttys under `class` (sysfs's `/sys/class/tty`) that are part of a USB device, each named by
/// its node under `dev` and given that device's ids, serial number and product string. Every value is
/// read from a sysfs file; nothing under `dev` is opened.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn usb_ttys(class: &Path, dev: &Path) -> Vec<SerialPortDesc> {
    let Ok(entries) = std::fs::read_dir(class) else {
        return Vec::new();
    };
    let mut ports: Vec<SerialPortDesc> = entries
        .flatten()
        .filter_map(|entry| {
            let device = entry.path().join("device").canonicalize().ok()?;
            let usb = usb_device_of(&device)?;
            let read = |file: &str| std::fs::read_to_string(usb.join(file)).ok().map(|text| text.trim().to_owned());
            let hex = |file: &str| read(file).and_then(|text| u16::from_str_radix(&text, 16).ok());
            let (vid, pid) = (hex("idVendor")?, hex("idProduct")?);
            let node = dev.join(entry.file_name());
            node.exists().then(|| SerialPortDesc {
                port: node.to_string_lossy().into_owned(),
                vid: Some(vid),
                pid: Some(pid),
                serial_number: read("serial"),
                product: read("product"),
            })
        })
        .collect();
    ports.sort_by(|one, other| one.port.cmp(&other.port));
    ports
}

/// The USB device a tty's `device` belongs to: the parent of its interface. A CDC port's `device` is
/// the interface itself; a USB-serial converter's is a port device of its own, one level below it.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn usb_device_of(device: &Path) -> Option<PathBuf> {
    let subsystem = device.join("subsystem").canonicalize().ok()?;
    let interface = match subsystem.file_name()?.to_str()? {
        "usb" => device,
        "usb-serial" => device.parent()?,
        _ => return None,
    };
    Some(interface.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, serial: Option<&str>) -> SerialPortDesc {
        SerialPortDesc {
            port: name.to_owned(),
            vid: Some(0x2e8a),
            pid: Some(0x000c),
            serial_number: serial.map(str::to_owned),
            product: None,
        }
    }

    /// **`usb-serial:` NAMES ONE PORT OR NONE.** A part of a serial number, in either case, finds the
    /// port carrying it; a port with no serial number is never found; a serial two ports share is
    /// refused with both names; and any other target is a port name already.
    #[test]
    fn a_serial_target_resolves_to_one_port_or_is_refused() {
        let ports = [
            port("COM75", Some("E6640000000000A1")),
            port("COM11", Some("PROBEALPHA001")),
            port("COM76", Some("PROBEALPHA001")),
            port("COM3", None),
        ];
        assert_eq!(resolve("usb-serial:e6640000", &ports), Ok("COM75".to_owned()));
        assert_eq!(resolve("COM3", &ports), Ok("COM3".to_owned()));
        assert_eq!(resolve("/dev/ttyACM0", &ports), Ok("/dev/ttyACM0".to_owned()));
        let refusal = resolve("usb-serial:PROBEALPHA", &ports).expect_err("two ports share it");
        assert!(refusal.contains("COM11, COM76"), "{refusal}");
        let refusal = resolve("usb-serial:NOSUCH", &ports).expect_err("nothing carries it");
        assert_eq!(refusal, "usb-serial:NOSUCH is not attached.");
    }

    /// **ON LINUX THE PORTS ARE READ FROM SYSFS, AND ONLY USB ONES**: a CDC port and a USB-serial
    /// converter's port, each with its device's ids, serial number and product string, and neither
    /// the computer's own UART nor a tty with no device. Built from a sysfs-shaped tree of the test's
    /// own, which needs symbolic links, so it runs on the Linux and macOS hosts.
    #[cfg(unix)]
    #[test]
    fn the_linux_ports_are_the_usb_ttys_sysfs_reports() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("lamella-ports-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sys = root.join("sys");
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
            std::fs::write(path, text).expect("a file");
        };
        for bus in ["usb", "usb-serial", "pnp"] {
            std::fs::create_dir_all(sys.join("bus").join(bus)).expect("a bus");
        }
        let probe = sys.join("devices/usb1/1-1");
        write(probe.join("idVendor"), "2e8a\n");
        write(probe.join("idProduct"), "000c\n");
        write(probe.join("serial"), "E6640000000000A1\n");
        write(probe.join("product"), "Debugprobe on Pico (CMSIS-DAP)\n");
        std::fs::create_dir_all(probe.join("1-1:1.1")).expect("an interface");
        symlink(sys.join("bus/usb"), probe.join("1-1:1.1/subsystem")).expect("a link");
        let bridge = sys.join("devices/usb1/1-2");
        write(bridge.join("idVendor"), "0403\n");
        write(bridge.join("idProduct"), "6001\n");
        std::fs::create_dir_all(bridge.join("1-2:1.0/ttyUSB0")).expect("a port device");
        symlink(sys.join("bus/usb-serial"), bridge.join("1-2:1.0/ttyUSB0/subsystem")).expect("a link");
        std::fs::create_dir_all(sys.join("devices/pnp0/00:01")).expect("a UART");
        symlink(sys.join("bus/pnp"), sys.join("devices/pnp0/00:01/subsystem")).expect("a link");
        let class = sys.join("class/tty");
        for (tty, device) in [
            ("ttyACM0", Some(probe.join("1-1:1.1"))),
            ("ttyUSB0", Some(bridge.join("1-2:1.0/ttyUSB0"))),
            ("ttyS0", Some(sys.join("devices/pnp0/00:01"))),
            ("tty0", None),
        ] {
            std::fs::create_dir_all(class.join(tty)).expect("a class entry");
            if let Some(device) = device {
                symlink(device, class.join(tty).join("device")).expect("a link");
            }
            write(root.join("dev").join(tty), "");
        }

        let ports = usb_ttys(&class, &root.join("dev"));
        let names: Vec<&str> = ports.iter().map(|port| port.port.rsplit('/').next().unwrap_or_default()).collect();
        assert_eq!(names, ["ttyACM0", "ttyUSB0"], "the USB ttys alone: {ports:?}");
        assert_eq!((ports[0].vid, ports[0].pid), (Some(0x2e8a), Some(0x000c)));
        assert_eq!(ports[0].serial_number.as_deref(), Some("E6640000000000A1"));
        assert_eq!(ports[0].product.as_deref(), Some("Debugprobe on Pico (CMSIS-DAP)"));
        assert_eq!((ports[1].vid, ports[1].pid), (Some(0x0403), Some(0x6001)));
        assert_eq!(ports[1].serial_number, None, "a converter with no serial number has none");
        let _ = std::fs::remove_dir_all(&root);
    }
}
