use serde::{Deserialize, Serialize};
use std::fmt;

/// V4L2 pixel format fourcc codes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    Yuyv,
    Uyvy,
    Nv12,
    Yu12,
    Mjpeg,
    H264,
    Unknown(u32),
}

impl PixelFormat {
    pub fn from_fourcc(fourcc: u32) -> Self {
        match &fourcc.to_le_bytes() {
            b"YUYV" => Self::Yuyv,
            b"UYVY" => Self::Uyvy,
            b"NV12" => Self::Nv12,
            b"YU12" => Self::Yu12,
            b"MJPG" => Self::Mjpeg,
            b"H264" => Self::H264,
            _ => Self::Unknown(fourcc),
        }
    }

    pub fn to_fourcc(&self) -> u32 {
        match self {
            Self::Yuyv => u32::from_le_bytes(*b"YUYV"),
            Self::Uyvy => u32::from_le_bytes(*b"UYVY"),
            Self::Nv12 => u32::from_le_bytes(*b"NV12"),
            Self::Yu12 => u32::from_le_bytes(*b"YU12"),
            Self::Mjpeg => u32::from_le_bytes(*b"MJPG"),
            Self::H264 => u32::from_le_bytes(*b"H264"),
            Self::Unknown(fourcc) => *fourcc,
        }
    }

    pub fn fourcc_str(&self) -> String {
        let bytes = self.to_fourcc().to_le_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// Bytes per pixel for uncompressed formats
    pub fn bytes_per_pixel(&self) -> Option<f32> {
        match self {
            Self::Yuyv | Self::Uyvy => Some(2.0),
            Self::Nv12 | Self::Yu12 => Some(1.5),
            Self::Mjpeg | Self::H264 => None,
            Self::Unknown(_) => None,
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fourcc_str())
    }
}

/// A specific video mode (format + resolution + framerate)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VideoMode {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
}

impl VideoMode {
    pub fn fps(&self) -> f64 {
        if self.fps_numerator == 0 {
            return 0.0;
        }
        self.fps_denominator as f64 / self.fps_numerator as f64
    }

    /// Bandwidth in bytes/sec for uncompressed, estimated for compressed
    pub fn bandwidth_bytes_per_sec(&self) -> Option<u64> {
        self.format.bytes_per_pixel().map(|bpp| {
            (self.width as u64
                * self.height as u64
                * (bpp * 100.0) as u64
                * self.fps_denominator as u64)
                / (self.fps_numerator as u64 * 100)
        })
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} @ {:.1} fps ({})",
            self.width,
            self.height,
            self.fps(),
            self.format
        )
    }
}

/// Result of probing a specific format for actual frame delivery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatProbeResult {
    pub mode: VideoMode,
    pub negotiation_ok: bool,
    pub stream_started: bool,
    pub frames_received: u32,
    pub first_frame_nonzero: bool,
    pub frame_size_consistent: bool,
    pub avg_frame_interval_ms: Option<f64>,
    pub error: Option<String>,
    pub verdict: FormatVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatVerdict {
    /// Format works correctly
    Working,
    /// Format negotiates but produces garbage
    GarbageFrames,
    /// Format fails to negotiate
    NegotiationFailed,
    /// Stream starts but no frames arrive
    NoFrames,
    /// Stream is unstable (variable frame sizes, timing jitter)
    Unstable,
    /// Not tested
    Untested,
}

impl fmt::Display for FormatVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Working => write!(f, "WORKING"),
            Self::GarbageFrames => write!(f, "GARBAGE FRAMES"),
            Self::NegotiationFailed => write!(f, "NEGOTIATION FAILED"),
            Self::NoFrames => write!(f, "NO FRAMES"),
            Self::Unstable => write!(f, "UNSTABLE"),
            Self::Untested => write!(f, "UNTESTED"),
        }
    }
}

#[cfg(test)]
mod h264_tests {
    use super::*;

    #[test]
    fn h264_round_trip_via_fourcc() {
        let h264 = PixelFormat::from_fourcc(u32::from_le_bytes(*b"H264"));
        assert_eq!(h264, PixelFormat::H264);
        assert_eq!(h264.to_fourcc(), u32::from_le_bytes(*b"H264"));
        assert_eq!(h264.fourcc_str(), "H264");
    }

    #[test]
    fn h264_has_no_static_bytes_per_pixel() {
        // Compressed format: bandwidth depends on content, not a fixed bpp.
        assert!(PixelFormat::H264.bytes_per_pixel().is_none());
    }

    #[test]
    fn h264_displays_as_fourcc() {
        assert_eq!(format!("{}", PixelFormat::H264), "H264");
    }
}
