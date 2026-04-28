use crate::device::{ElgatoProduct, FirmwareVersion};
use crate::formats::PixelFormat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Quirk {
    pub id: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    /// Products on which this quirk has been EMPIRICALLY OBSERVED.
    /// Never inferred — speculative entries violate the project's
    /// "no speculative mitigations" rule.
    pub products: &'static [ElgatoProduct],
    /// Inclusive lower bound on firmware where this quirk applies.
    pub firmware_min: Option<FirmwareVersion>,
    /// Exclusive upper bound on firmware where this quirk applies.
    pub firmware_max: Option<FirmwareVersion>,
    /// If set, this quirk only fires when the named pixel format is in use.
    pub format: Option<PixelFormat>,
    pub severity: QuirkSeverity,
    pub mitigation: QuirkMitigation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuirkSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QuirkMitigation {
    NormalizationPipeline,
    UsbReset,
    SkipFormat(PixelFormat),
    FirmwareUpdate(FirmwareVersion),
    ForceFormat(PixelFormat),
    /// Retry STREAMOFF/STREAMON once before resorting to USB reset.
    /// Used for the Pro's stream-start race.
    RetryStreamOn,
    /// Avoid this mitigation unless every cheaper recovery has failed.
    /// Used for USB reset on the Pro (destructive, multi-second cool-down).
    AvoidUnlessLastResort,
    Manual(&'static str),
}

pub fn quirk_registry() -> &'static [Quirk] {
    REGISTRY
}

static REGISTRY: &[Quirk] = &[
    Quirk {
        id: "BOGUS_NV12",
        summary: "NV12 advertised but produces 0-byte streams (original Facecam)",
        description: "The original Facecam (PID 0x0078, fw 4.09) advertises NV12 \
            in its UVC descriptors, but `v4l2-ctl --stream-mmap pixelformat=NV12` \
            returns zero bytes within a 5s timeout. Note: the Pro (PID 0x0079, \
            fw 0.06) was originally suspected of the same behavior, but on \
            2026-04-28 NV12 was empirically observed to deliver valid frames \
            after a single STREAM_START_RACE recovery cycle, so the Pro is \
            tracked under PRO_STREAM_START_RACE only and is no longer included \
            in this quirk's applicability list.",
        products: &[ElgatoProduct::Facecam],
        firmware_min: None,
        firmware_max: None,
        format: Some(PixelFormat::Nv12),
        severity: QuirkSeverity::Error,
        mitigation: QuirkMitigation::SkipFormat(PixelFormat::Nv12),
    },
    Quirk {
        id: "BOGUS_YU12",
        summary: "YU12 advertised but produces garbage frames (original Facecam)",
        description: "The original Facecam advertises YU12 (Planar YUV 4:2:0) \
            but streaming in YU12 produces green or empty frames. The Pro does \
            not advertise YU12 — this quirk is original-Facecam only.",
        products: &[ElgatoProduct::Facecam],
        firmware_min: None,
        firmware_max: None,
        format: Some(PixelFormat::Yu12),
        severity: QuirkSeverity::Error,
        mitigation: QuirkMitigation::SkipFormat(PixelFormat::Yu12),
    },
    Quirk {
        id: "OPEN_CLOSE_LOCKUP",
        summary: "Device locks up after consumer close/reopen cycle (original Facecam)",
        description: "After the first application closes the V4L2 device, \
            subsequent opens fail with EBUSY or produce no frames. The device \
            requires a USB reset to recover. The v4l2loopback normalization \
            pipeline mitigates this by keeping a single long-lived producer. \
            The Pro shows a structurally different alternation pattern, \
            tracked separately as PRO_STREAM_START_RACE.",
        products: &[ElgatoProduct::Facecam],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Critical,
        mitigation: QuirkMitigation::NormalizationPipeline,
    },
    Quirk {
        id: "STARTUP_UNRELIABILITY",
        summary: "~50% failure rate on initial stream start (original Facecam)",
        description: "The original Facecam fails to initialize the video \
            stream approximately half the time on first open. A retry \
            (typically with USB reset on persistent failure) resolves this. \
            The Pro shows a different failure mode (PRO_STREAM_START_RACE) \
            that does NOT require USB reset.",
        products: &[ElgatoProduct::Facecam],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Error,
        mitigation: QuirkMitigation::UsbReset,
    },
    Quirk {
        id: "NO_MJPEG_OLD_FW",
        summary: "No MJPEG support on original Facecam firmware below 4.00",
        description: "Original Facecam firmware versions below 4.00 only \
            support uncompressed formats. Chromium-based browsers require \
            MJPEG or cannot negotiate the camera. The v4l2loopback pipeline \
            resolves this by presenting a normalized output. (Earlier \
            comments cited 4.03; firmware 4.00 has been observed to ship \
            MJPEG.)",
        products: &[ElgatoProduct::Facecam],
        firmware_min: None,
        firmware_max: Some(FirmwareVersion { major: 4, minor: 0 }),
        format: None,
        severity: QuirkSeverity::Warning,
        mitigation: QuirkMitigation::NormalizationPipeline,
    },
    Quirk {
        id: "CHROMIUM_FORMAT_REJECT",
        summary: "Chromium rejects devices with both CAPTURE and OUTPUT caps",
        description: "Chromium-based browsers refuse to use V4L2 devices that \
            report both V4L2_CAP_VIDEO_CAPTURE and V4L2_CAP_VIDEO_OUTPUT in \
            their capabilities. v4l2loopback with exclusive_caps=1 resolves \
            this. Observed empirically with Facecam (PID 0x0078) and Facecam Pro \
            (PID 0x0079); not yet verified on MK.2 or CamLink families.",
        products: &[ElgatoProduct::Facecam, ElgatoProduct::FacecamPro],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Error,
        mitigation: QuirkMitigation::NormalizationPipeline,
    },
    Quirk {
        id: "USB2_FALLBACK_MODE",
        summary: "Facecam presents PID 0x0077 on USB 2.0 with no video capability",
        description: "When connected to a USB 2.0 port, the original Facecam \
            enumerates with PID 0x0077 instead of 0x0078 and a product string \
            of 'USB3-REQUIRED-FOR-FACECAM'. No UVC interface is exposed.",
        products: &[ElgatoProduct::FacecamUsb2Fallback],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Critical,
        mitigation: QuirkMitigation::Manual("Move the camera to a USB 3.0 (blue) port"),
    },
    Quirk {
        id: "USB3_REQUIRED",
        summary: "Device requires USB 3.0 SuperSpeed",
        description: "The Facecam (PID 0x0078) and Facecam Pro (PID 0x0079) \
            require USB 3.0 bandwidth (~249 MB/s for YUYV 1080p60; up to \
            ~3 Gbps for Pro 4Kp60 MJPG) — empirically tested. USB topology \
            must be validated during probe.",
        products: &[ElgatoProduct::Facecam, ElgatoProduct::FacecamPro],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Critical,
        mitigation: QuirkMitigation::Manual("Connect to a USB 3.0 port directly, avoid hubs"),
    },
    Quirk {
        id: "BANDWIDTH_STARVATION",
        summary: "USB hub/dock sharing can cause frame drops or freezing",
        description: "Both the original Facecam and the Pro use significant \
            USB 3.0 bandwidth. Sharing a controller with other high-bandwidth \
            devices causes instability. MJPEG mode (firmware 4.00+ on \
            original; default on Pro) reduces bandwidth.",
        products: &[ElgatoProduct::Facecam, ElgatoProduct::FacecamPro],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Warning,
        mitigation: QuirkMitigation::ForceFormat(PixelFormat::Mjpeg),
    },
    Quirk {
        id: "YUYV_UYVY_AMBIGUITY",
        summary: "Wire format may be UYVY despite V4L2 reporting YUYV (original Facecam)",
        description: "On the original Facecam, community workarounds use \
            uyvy422 as ffmpeg input format even though v4l2-ctl reports YUYV. \
            The Pro does not advertise YUYV or UYVY at all — this quirk does \
            not apply to it.",
        products: &[ElgatoProduct::Facecam],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Info,
        mitigation: QuirkMitigation::Manual("Verify empirically via frame byte inspection"),
    },
    Quirk {
        id: "PRO_STREAM_START_RACE",
        summary: "Pro: stream-start often fails on first attempt; retry succeeds",
        description: "On Elgato Facecam Pro (PID 0x0079, fw 0.06), \
            back-to-back STREAMON cycles show a strong alternation: every \
            other attempt returns zero frames within a 5s timeout, every \
            other attempt streams normally. Empirically: 20-of-20 alternating \
            cycles observed 2026-04-27 with no idle time between. Pattern \
            softens (some adjacent OK/OK pairs) when idle time or non-streaming \
            opens are interleaved. The recovery is cheap: a same-process retry \
            (STREAMOFF then STREAMON, or close+reopen) succeeds. USB reset is \
            NOT required — see PRO_USB_RESET_DESTRUCTIVE.",
        products: &[ElgatoProduct::FacecamPro],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Error,
        mitigation: QuirkMitigation::RetryStreamOn,
    },
    Quirk {
        id: "PRO_USB_RESET_DESTRUCTIVE",
        summary: "Pro: USB sysfs reset is destructive — last resort only",
        description: "On Elgato Facecam Pro (PID 0x0079, fw 0.06), \
            `echo 0 > /sys/bus/usb/devices/.../authorized` returns ETIMEDOUT \
            (the kernel disconnect-wait times out) but the device does \
            re-enumerate. Multiple consecutive stream attempts after the reset \
            return zero bytes; the device needs ~5 seconds of cool-down before \
            it accepts streaming again. Empirically observed 2026-04-27. \
            Recovery ladder must therefore use USB reset only after cheaper \
            recoveries (in-place STREAMOFF/STREAMON, then close+reopen) have \
            failed multiple times.",
        products: &[ElgatoProduct::FacecamPro],
        firmware_min: None,
        firmware_max: None,
        format: None,
        severity: QuirkSeverity::Warning,
        mitigation: QuirkMitigation::AvoidUnlessLastResort,
    },
];

/// Returns the quirks that apply to a given (product, firmware) pair.
pub fn applicable_quirks(product: ElgatoProduct, firmware: FirmwareVersion) -> Vec<&'static Quirk> {
    REGISTRY
        .iter()
        .filter(|q| {
            q.products.contains(&product)
                && q.firmware_min.is_none_or(|min| firmware >= min)
                && q.firmware_max.is_none_or(|max| firmware < max)
        })
        .collect()
}

/// True if the quirk DB knows this format produces broken output for this
/// product/firmware combo. Replaces the old static
/// PixelFormat::is_reliable_on_facecam list.
pub fn is_format_known_broken(
    product: ElgatoProduct,
    firmware: FirmwareVersion,
    format: PixelFormat,
) -> bool {
    applicable_quirks(product, firmware)
        .iter()
        .any(|q| q.format == Some(format))
}
