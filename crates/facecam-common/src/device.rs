use serde::{Deserialize, Serialize};
use std::fmt;

/// Elgato vendor ID
pub const ELGATO_VID: u16 = 0x0fd9;

/// Known Elgato camera product IDs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ElgatoProduct {
    Facecam,
    /// Facecam on USB 2.0 — non-functional, shows "USB3-REQUIRED-FOR-FACECAM"
    FacecamUsb2Fallback,
    FacecamPro,
    FacecamMk2,
    FacecamMk2Usb2,
    CamLink4K,
    Unknown(u16),
}

impl ElgatoProduct {
    pub fn from_pid(pid: u16) -> Self {
        match pid {
            0x0078 => Self::Facecam,
            0x0077 => Self::FacecamUsb2Fallback,
            0x0079 => Self::FacecamPro,
            0x0093 => Self::FacecamMk2,
            0x0094 => Self::FacecamMk2Usb2,
            0x0066 => Self::CamLink4K,
            other => Self::Unknown(other),
        }
    }

    pub fn pid(&self) -> u16 {
        match self {
            Self::Facecam => 0x0078,
            Self::FacecamUsb2Fallback => 0x0077,
            Self::FacecamPro => 0x0079,
            Self::FacecamMk2 => 0x0093,
            Self::FacecamMk2Usb2 => 0x0094,
            Self::CamLink4K => 0x0066,
            Self::Unknown(pid) => *pid,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Facecam => "Elgato Facecam",
            Self::FacecamUsb2Fallback => "Elgato Facecam (USB2 FALLBACK — NOT FUNCTIONAL)",
            Self::FacecamPro => "Elgato Facecam Pro",
            Self::FacecamMk2 => "Elgato Facecam MK.2",
            Self::FacecamMk2Usb2 => "Elgato Facecam MK.2 (USB2)",
            Self::CamLink4K => "Elgato Cam Link 4K",
            Self::Unknown(_) => "Unknown Elgato Device",
        }
    }

    pub fn is_facecam_original(&self) -> bool {
        matches!(self, Self::Facecam)
    }

    /// Device is a Facecam stuck in USB 2.0 fallback mode
    pub fn is_usb2_fallback(&self) -> bool {
        matches!(self, Self::FacecamUsb2Fallback)
    }
}

/// Static facts about an Elgato product that the device cannot self-report.
/// Anything that CAN be probed (formats, controls, capabilities) is read at
/// runtime via the v4l2 module — those do not belong here.
pub trait ProductDescriptor {
    /// True iff this product exposes a UVC video-capture interface.
    /// HID-only Elgato devices (Stream Deck, Wave XLR) return false.
    fn is_uvc_capture(&self) -> bool;

    /// Slug used to look up family-specific profile defaults and in logs.
    /// Siblings share a family: Facecam + USB2 fallback share "facecam";
    /// Mk.2 + Mk.2-USB2 share "facecam-mk2".
    fn family(&self) -> &'static str;

    /// Resolve a profile's generic control name to the device's actual V4L2
    /// control name. Returns None if the device uses the generic name as-is.
    fn control_alias(&self, generic: &str) -> Option<&'static str>;
}

impl ProductDescriptor for ElgatoProduct {
    // is_uvc_capture asymmetry note (2026-04-27):
    // - FacecamUsb2Fallback (PID 0x0077) is the original Facecam's degraded USB2
    //   mode: enumerates as a distinct PID with no UVC interface, only the
    //   "USB3-REQUIRED-FOR-FACECAM" string descriptor. Returns false.
    // - FacecamMk2Usb2 (PID 0x0094) is the MK.2's distinct USB2 operating PID.
    //   MK.2 hardware is not available for empirical verification in this session;
    //   inclusion here is provisional and should be re-checked when an MK.2 is
    //   tested. Returns true.
    fn is_uvc_capture(&self) -> bool {
        matches!(
            self,
            Self::Facecam
                | Self::FacecamPro
                | Self::FacecamMk2
                | Self::FacecamMk2Usb2
                | Self::CamLink4K
        )
    }

    fn family(&self) -> &'static str {
        match self {
            Self::Facecam | Self::FacecamUsb2Fallback => "facecam",
            Self::FacecamPro => "facecam-pro",
            Self::FacecamMk2 | Self::FacecamMk2Usb2 => "facecam-mk2",
            Self::CamLink4K => "cam-link-4k",
            Self::Unknown(_) => "unknown",
        }
    }

    fn control_alias(&self, generic: &str) -> Option<&'static str> {
        match self {
            Self::FacecamPro => match generic {
                // Empirically observed control-name differences on PID 0x0079, 2026-04-27
                "white_balance_auto" | "white_balance_temperature_auto" => {
                    Some("white_balance_automatic")
                }
                "exposure_absolute" => Some("exposure_time_absolute"),
                _ => None,
            },
            // Other products use the generic names as-is until empirical evidence
            // demands an alias.
            _ => None,
        }
    }
}

impl fmt::Display for ElgatoProduct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (PID 0x{:04x})", self.name(), self.pid())
    }
}

/// Firmware version parsed from bcdDevice USB descriptor
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FirmwareVersion {
    pub major: u8,
    pub minor: u8,
}

impl FirmwareVersion {
    pub fn from_bcd(bcd: u16) -> Self {
        Self {
            major: ((bcd >> 8) & 0xFF) as u8,
            minor: (bcd & 0xFF) as u8,
        }
    }

    /// Firmware 4.00+ has MJPEG support (empirically confirmed on 4.00,
    /// earlier research suggested 4.03 but real device shows MJPEG on 4.00)
    pub fn has_mjpeg(&self) -> bool {
        (self.major, self.minor) >= (4, 0)
    }

    /// Firmware 3.00+ added bulk/iso transfer mode selection
    pub fn has_transfer_mode_selection(&self) -> bool {
        (self.major, self.minor) >= (3, 0)
    }
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

/// Full device fingerprint collected during probe
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceFingerprint {
    pub product: ElgatoProduct,
    pub firmware: FirmwareVersion,
    pub serial: String,
    pub usb_bus: u8,
    pub usb_address: u8,
    pub usb_port_numbers: Vec<u8>,
    pub usb_speed: UsbSpeed,
    pub v4l2_device: Option<String>,
    pub v4l2_sysfs_path: Option<String>,
    pub driver_version: Option<String>,
    pub card_name: Option<String>,
}

impl fmt::Display for DeviceFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Device:     {}", self.product)?;
        writeln!(f, "Firmware:   {}", self.firmware)?;
        writeln!(f, "Serial:     {}", self.serial)?;
        writeln!(
            f,
            "USB:        bus {} addr {} ({})",
            self.usb_bus, self.usb_address, self.usb_speed
        )?;
        writeln!(f, "Port Path:  {:?}", self.usb_port_numbers)?;
        if let Some(ref dev) = self.v4l2_device {
            writeln!(f, "V4L2:       {}", dev)?;
        }
        if let Some(ref card) = self.card_name {
            writeln!(f, "Card:       {}", card)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbSpeed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
    Unknown,
}

impl fmt::Display for UsbSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "Low (1.5 Mbps)"),
            Self::Full => write!(f, "Full (12 Mbps)"),
            Self::High => write!(f, "High (480 Mbps)"),
            Self::Super => write!(f, "SuperSpeed (5 Gbps)"),
            Self::SuperPlus => write!(f, "SuperSpeed+ (10+ Gbps)"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

impl From<rusb::Speed> for UsbSpeed {
    fn from(speed: rusb::Speed) -> Self {
        match speed {
            rusb::Speed::Low => Self::Low,
            rusb::Speed::Full => Self::Full,
            rusb::Speed::High => Self::High,
            rusb::Speed::Super => Self::Super,
            rusb::Speed::SuperPlus => Self::SuperPlus,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;

    #[test]
    fn facecam_is_uvc_capture() {
        assert!(ElgatoProduct::Facecam.is_uvc_capture());
    }

    #[test]
    fn facecam_pro_is_uvc_capture() {
        assert!(ElgatoProduct::FacecamPro.is_uvc_capture());
    }

    #[test]
    fn mk2_pids_are_uvc_capture() {
        assert!(ElgatoProduct::FacecamMk2.is_uvc_capture());
        assert!(ElgatoProduct::FacecamMk2Usb2.is_uvc_capture());
    }

    #[test]
    fn cam_link_is_uvc_capture() {
        assert!(ElgatoProduct::CamLink4K.is_uvc_capture());
    }

    #[test]
    fn usb2_fallback_is_not_uvc_capture() {
        // USB2 fallback exposes no UVC interface per quirk USB2_FALLBACK_MODE
        assert!(!ElgatoProduct::FacecamUsb2Fallback.is_uvc_capture());
    }

    #[test]
    fn unknown_pids_are_not_uvc_capture() {
        // Stream Deck Plus (0x0084), Wave XLR (0x007d), etc.
        assert!(!ElgatoProduct::Unknown(0x0084).is_uvc_capture());
        assert!(!ElgatoProduct::Unknown(0x007d).is_uvc_capture());
    }

    #[test]
    fn family_slugs_group_siblings() {
        assert_eq!(ElgatoProduct::Facecam.family(), "facecam");
        assert_eq!(ElgatoProduct::FacecamUsb2Fallback.family(), "facecam");
        assert_eq!(ElgatoProduct::FacecamPro.family(), "facecam-pro");
        assert_eq!(ElgatoProduct::FacecamMk2.family(), "facecam-mk2");
        assert_eq!(ElgatoProduct::FacecamMk2Usb2.family(), "facecam-mk2");
        assert_eq!(ElgatoProduct::CamLink4K.family(), "cam-link-4k");
        assert_eq!(ElgatoProduct::Unknown(0x1234).family(), "unknown");
    }

    #[test]
    fn pro_remaps_white_balance_auto_alias() {
        // Profiles use the generic key "white_balance_auto"; on the Pro the
        // V4L2 control is named "white_balance_automatic".
        assert_eq!(
            ElgatoProduct::FacecamPro.control_alias("white_balance_auto"),
            Some("white_balance_automatic")
        );
    }

    #[test]
    fn original_facecam_uses_generic_names() {
        // Original profile keys match the device's V4L2 names. No alias needed.
        assert_eq!(
            ElgatoProduct::Facecam.control_alias("white_balance_auto"),
            None
        );
        assert_eq!(ElgatoProduct::Facecam.control_alias("brightness"), None);
    }

    #[test]
    fn pro_remaps_exposure_alias() {
        // Original V4L2 name: exposure_absolute. Pro V4L2 name: exposure_time_absolute.
        // Generic name (used in profiles): exposure_absolute (matches original).
        assert_eq!(
            ElgatoProduct::FacecamPro.control_alias("exposure_absolute"),
            Some("exposure_time_absolute")
        );
    }

    #[test]
    fn pro_remaps_white_balance_temperature_auto_alias() {
        // V4L2 control name on the original Facecam is `white_balance_temperature_auto`.
        // Profiles authored against the original Facecam carry that key; on the Pro the
        // equivalent control is `white_balance_automatic`. Both generic forms must alias
        // to the Pro's name.
        assert_eq!(
            ElgatoProduct::FacecamPro.control_alias("white_balance_temperature_auto"),
            Some("white_balance_automatic")
        );
    }
}
