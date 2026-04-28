use crate::device::{DeviceFingerprint, ElgatoProduct, FirmwareVersion, UsbSpeed, ELGATO_VID};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

/// Discover Elgato cameras via libusb
pub fn enumerate_elgato_devices() -> Result<Vec<DeviceFingerprint>> {
    let mut devices = Vec::new();

    for device in rusb::devices()?.iter() {
        let desc = device.device_descriptor()?;
        if desc.vendor_id() != ELGATO_VID {
            continue;
        }

        let product = ElgatoProduct::from_pid(desc.product_id());
        let ver = desc.device_version();
        // bcdDevice 0x0409 -> major=4, minor=0, sub_minor=9 in rusb
        // We need major as the firmware major, and (minor*10 + sub_minor) as firmware minor
        let bcd =
            ((ver.major() as u16) << 8) | ((ver.minor() as u16) * 10 + ver.sub_minor() as u16);
        let speed: UsbSpeed = device.speed().into();

        let firmware = FirmwareVersion::from_bcd(bcd);

        // Skip USB descriptor reads for devices in USB2 fallback mode —
        // they hang on open() because the device is non-functional.
        let serial = if product.is_usb2_fallback() {
            String::new()
        } else {
            match device.open() {
                Ok(handle) => handle
                    .read_string_descriptor_ascii(desc.serial_number_string_index().unwrap_or(0))
                    .unwrap_or_default(),
                Err(_) => String::new(),
            }
        };

        let port_numbers = device.port_numbers().unwrap_or_default();

        let fingerprint = DeviceFingerprint {
            product,
            firmware,
            serial,
            usb_bus: device.bus_number(),
            usb_address: device.address(),
            usb_port_numbers: port_numbers,
            usb_speed: speed,
            v4l2_device: None,
            v4l2_sysfs_path: None,
            driver_version: None,
            card_name: None,
        };

        info!(
            product = %fingerprint.product,
            firmware = %fingerprint.firmware,
            bus = fingerprint.usb_bus,
            addr = fingerprint.usb_address,
            speed = %fingerprint.usb_speed,
            "Found Elgato device"
        );

        devices.push(fingerprint);
    }

    Ok(devices)
}

/// Find the sysfs path for a USB device by bus and address
pub fn find_usb_sysfs_path(bus: u8, addr: u8) -> Result<Option<PathBuf>> {
    let sysfs_base = Path::new("/sys/bus/usb/devices");
    if !sysfs_base.exists() {
        return Ok(None);
    }

    for entry in fs::read_dir(sysfs_base)? {
        let entry = entry?;
        let path = entry.path();

        let busnum_path = path.join("busnum");
        let devnum_path = path.join("devnum");

        if busnum_path.exists() && devnum_path.exists() {
            let busnum: u8 = fs::read_to_string(&busnum_path)?
                .trim()
                .parse()
                .unwrap_or(0);
            let devnum: u8 = fs::read_to_string(&devnum_path)?
                .trim()
                .parse()
                .unwrap_or(0);

            if busnum == bus && devnum == addr {
                return Ok(Some(path));
            }
        }
    }

    Ok(None)
}

/// Enumerate Elgato devices that expose a UVC capture interface.
/// Production callers should use this rather than enumerate_elgato_devices,
/// which returns every Elgato (including Stream Deck, Wave XLR).
pub fn enumerate_uvc_capture_devices() -> Result<Vec<DeviceFingerprint>> {
    use crate::device::ProductDescriptor;
    let mut out = Vec::new();
    for mut fp in enumerate_elgato_devices()? {
        if !fp.product.is_uvc_capture() {
            continue;
        }
        if let Ok(Some(sysfs)) = find_usb_sysfs_path(fp.usb_bus, fp.usb_address) {
            fp.v4l2_sysfs_path = Some(sysfs.to_string_lossy().to_string());
            if let Ok(Some(node)) = find_v4l2_device_for_usb(&sysfs) {
                fp.v4l2_device = Some(node);
            } else {
                // Sysfs is there but no /dev/video node yet — skip; the device
                // descriptor says UVC but no capture node exists for us to use.
                continue;
            }
        } else {
            continue;
        }
        out.push(fp);
    }
    Ok(out)
}

/// Detect the first connected Elgato UVC capture camera, falling back to
/// Facecam if none found. Filters to UVC capture devices to avoid issuing
/// USB resets against non-camera Elgato peripherals (Stream Deck, Wave XLR).
pub fn detect_product_or_default() -> crate::device::ElgatoProduct {
    enumerate_uvc_capture_devices()
        .ok()
        .and_then(|devs| devs.into_iter().next().map(|d| d.product))
        .unwrap_or(crate::device::ElgatoProduct::Facecam)
}

/// Find the sysfs path for any specific Elgato product by VID:PID.
pub fn find_elgato_sysfs_path(product: crate::device::ElgatoProduct) -> Result<Option<PathBuf>> {
    let sysfs_base = Path::new("/sys/bus/usb/devices");
    if !sysfs_base.exists() {
        return Ok(None);
    }

    let want_pid = format!("{:04x}", product.pid());
    let want_vid = format!("{:04x}", ELGATO_VID);

    for entry in fs::read_dir(sysfs_base)? {
        let entry = entry?;
        let path = entry.path();
        let vid_path = path.join("idVendor");
        let pid_path = path.join("idProduct");
        if vid_path.exists() && pid_path.exists() {
            let vid = fs::read_to_string(&vid_path)?.trim().to_string();
            let pid = fs::read_to_string(&pid_path)?.trim().to_string();
            if vid == want_vid && pid == want_pid {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// Find the V4L2 device node associated with a USB device sysfs path
pub fn find_v4l2_device_for_usb(usb_sysfs: &Path) -> Result<Option<String>> {
    // Walk the USB device tree looking for video4linux subdirectories
    find_v4l2_node_recursive(usb_sysfs)
}

fn find_v4l2_node_recursive(path: &Path) -> Result<Option<String>> {
    let v4l_path = path.join("video4linux");
    if v4l_path.exists() {
        for entry in fs::read_dir(&v4l_path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("video") {
                continue;
            }
            let node_path = entry.path();
            let index_path = node_path.join("index");
            let is_primary = if index_path.exists() {
                fs::read_to_string(&index_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .map(|i| i == 0)
                    .unwrap_or(false)
            } else {
                true
            };
            if !is_primary {
                continue;
            }
            // Validate that this V4L2 node actually reports VIDEO_CAPTURE
            // capability — otherwise non-UVC interfaces (HID, audio control)
            // can sneak through.
            let dev = format!("/dev/{}", name);
            if !v4l2_node_has_capture_capability(&dev) {
                continue;
            }
            return Ok(Some(dev));
        }
    }

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries {
            let entry = entry?;
            let child = entry.path();
            if child.is_dir() {
                if let Some(dev) = find_v4l2_node_recursive(&child)? {
                    return Ok(Some(dev));
                }
            }
        }
    }

    Ok(None)
}

/// Open the V4L2 device read-only and check QUERYCAP for VIDEO_CAPTURE.
/// On any error (permission, device busy), returns false rather than failing
/// the whole enumeration.
fn v4l2_node_has_capture_capability(dev: &str) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(dev) else {
        return false;
    };
    let Ok(caps) = crate::v4l2::query_capabilities(file.as_raw_fd()) else {
        return false;
    };
    caps.has_capture
}

/// Read USB topology details for diagnostics
pub fn read_usb_topology(sysfs_path: &Path) -> Result<UsbTopology> {
    let read_file = |name: &str| -> Option<String> {
        fs::read_to_string(sysfs_path.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    };

    Ok(UsbTopology {
        sysfs_path: sysfs_path.to_path_buf(),
        busnum: read_file("busnum").and_then(|s| s.parse().ok()),
        devnum: read_file("devnum").and_then(|s| s.parse().ok()),
        speed: read_file("speed"),
        version: read_file("version"),
        maxchild: read_file("maxchild").and_then(|s| s.parse().ok()),
        authorized: read_file("authorized").and_then(|s| s.parse().ok()),
        manufacturer: read_file("manufacturer"),
        product_name: read_file("product"),
        bcd_device: read_file("bcdDevice"),
        configuration: read_file("configuration"),
    })
}

#[cfg(test)]
mod uvc_filter_tests {
    use super::*;

    #[test]
    fn enumerate_uvc_capture_devices_returns_a_vec() {
        // Smoke test: function exists with the right signature and returns
        // without panicking. Result may be empty in CI.
        let _ = enumerate_uvc_capture_devices();
    }

    // The following tests run against the live USB bus on the development
    // machine. They are gated with #[ignore] so CI stays hardware-free; run
    // locally with `cargo test -p facecam-common -- --ignored`.

    #[test]
    #[ignore = "requires live USB bus"]
    fn enumerate_uvc_capture_devices_excludes_hid() {
        let cams = enumerate_uvc_capture_devices().expect("enumeration");
        for c in &cams {
            // No Stream Deck Plus (0x0084), no Wave XLR (0x007d).
            assert_ne!(c.product.pid(), 0x0084, "Stream Deck must not appear");
            assert_ne!(c.product.pid(), 0x007d, "Wave XLR must not appear");
        }
    }

    #[test]
    #[ignore = "requires Facecam Pro at /dev/video0"]
    fn pro_is_discoverable_as_uvc_capture() {
        let cams = enumerate_uvc_capture_devices().expect("enumeration");
        let pro = cams
            .iter()
            .find(|c| c.product.pid() == 0x0079)
            .expect("Facecam Pro PID 0x0079 must be enumerable");
        assert_eq!(
            pro.v4l2_device.as_deref(),
            Some("/dev/video0"),
            "Pro should map to /dev/video0 on this machine"
        );
    }

    #[test]
    #[ignore = "requires Facecam Pro at /dev/video0"]
    fn find_elgato_sysfs_path_resolves_pro() {
        let p =
            find_elgato_sysfs_path(crate::device::ElgatoProduct::FacecamPro).expect("sysfs scan");
        let path = p.expect("Pro sysfs path");
        assert!(path.exists(), "{} must exist", path.display());
        let auth = std::fs::read_to_string(path.join("authorized")).expect("read authorized");
        assert_eq!(auth.trim(), "1");
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsbTopology {
    pub sysfs_path: PathBuf,
    pub busnum: Option<u8>,
    pub devnum: Option<u8>,
    pub speed: Option<String>,
    pub version: Option<String>,
    pub maxchild: Option<u8>,
    pub authorized: Option<u8>,
    pub manufacturer: Option<String>,
    pub product_name: Option<String>,
    pub bcd_device: Option<String>,
    pub configuration: Option<String>,
}
