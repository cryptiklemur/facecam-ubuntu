use crate::formats::{PixelFormat, VideoMode};
use crate::types::{ControlType, ControlValue, MenuItem};
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::io::RawFd;

// V4L2 ioctl numbers and structures
// These are stable kernel ABI — using raw values is intentional to avoid
// pulling in large bindgen-generated crate dependencies.
// Some constants are reserved for future MMAP streaming support.
mod consts {
    pub const VIDIOC_QUERYCAP: u64 = 0x80685600;
    pub const VIDIOC_ENUM_FMT: u64 = 0xC0405602;
    pub const VIDIOC_S_FMT: u64 = 0xC0D05605;
    pub const VIDIOC_REQBUFS: u64 = 0xC0145608;
    pub const VIDIOC_STREAMON: u64 = 0x40045612;
    pub const VIDIOC_STREAMOFF: u64 = 0x40045613;
    pub const VIDIOC_QUERYCTRL: u64 = 0xC0445624;
    pub const VIDIOC_G_CTRL: u64 = 0xC008561B;
    pub const VIDIOC_S_CTRL: u64 = 0xC008561C;
    pub const VIDIOC_QUERYMENU: u64 = 0xC0445625;
    pub const VIDIOC_ENUM_FRAMESIZES: u64 = 0xC02C564A;
    pub const VIDIOC_ENUM_FRAMEINTERVALS: u64 = 0xC034564B;
    pub const VIDIOC_QUERYBUF: u64 = 0xC0585609;
    pub const VIDIOC_QBUF: u64 = 0xC058560F;
    pub const VIDIOC_DQBUF: u64 = 0xC0585611;

    pub const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
    pub const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
    pub const V4L2_MEMORY_MMAP: u32 = 1;

    pub const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x00000001;
    pub const V4L2_CAP_VIDEO_OUTPUT: u32 = 0x00000002;
    pub const V4L2_CAP_STREAMING: u32 = 0x04000000;
    pub const V4L2_CAP_READWRITE: u32 = 0x01000000;

    // V4L2 control IDs (base + offset)
    pub const V4L2_CID_BASE: u32 = 0x00980900;
    pub const V4L2_CID_BRIGHTNESS: u32 = V4L2_CID_BASE;
    pub const V4L2_CID_CONTRAST: u32 = V4L2_CID_BASE + 1;
    pub const V4L2_CID_SATURATION: u32 = V4L2_CID_BASE + 2;
    pub const V4L2_CID_SHARPNESS: u32 = V4L2_CID_BASE + 27;

    pub const V4L2_CID_CAMERA_CLASS_BASE: u32 = 0x009A0900;
    pub const V4L2_CID_EXPOSURE_AUTO: u32 = V4L2_CID_CAMERA_CLASS_BASE + 1;
    pub const V4L2_CID_EXPOSURE_ABSOLUTE: u32 = V4L2_CID_CAMERA_CLASS_BASE + 2;
    pub const V4L2_CID_PAN_ABSOLUTE: u32 = V4L2_CID_CAMERA_CLASS_BASE + 8;
    pub const V4L2_CID_TILT_ABSOLUTE: u32 = V4L2_CID_CAMERA_CLASS_BASE + 9;
    pub const V4L2_CID_ZOOM_ABSOLUTE: u32 = V4L2_CID_CAMERA_CLASS_BASE + 13;

    pub const V4L2_CTRL_FLAG_DISABLED: u32 = 0x0001;
    pub const V4L2_CTRL_FLAG_NEXT_CTRL: u32 = 0x80000000;

    pub const V4L2_CTRL_TYPE_INTEGER: u32 = 1;
    pub const V4L2_CTRL_TYPE_BOOLEAN: u32 = 2;
    pub const V4L2_CTRL_TYPE_MENU: u32 = 3;
    pub const V4L2_CTRL_TYPE_BUTTON: u32 = 4;
    pub const V4L2_CTRL_TYPE_INTEGER64: u32 = 5;
    pub const V4L2_CTRL_TYPE_CTRL_CLASS: u32 = 6;
    pub const V4L2_CTRL_TYPE_STRING: u32 = 7;
    pub const V4L2_CTRL_TYPE_BITMASK: u32 = 8;
    pub const V4L2_CTRL_TYPE_INTEGER_MENU: u32 = 9;

    // Frame size type
    pub const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
    pub const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;
}
use consts::*;

/// Safe wrapper for ioctl calls
unsafe fn v4l2_ioctl(fd: RawFd, request: u64, arg: *mut u8) -> Result<()> {
    let ret = libc::ioctl(fd, request as libc::c_ulong, arg);
    if ret < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

/// Open a V4L2 device and return its file descriptor
pub fn open_device(path: &str) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open V4L2 device: {}", path))
}

/// Query device capabilities
pub fn query_capabilities(fd: RawFd) -> Result<DeviceCapabilities> {
    let mut cap = [0u8; 104]; // struct v4l2_capability
    unsafe { v4l2_ioctl(fd, VIDIOC_QUERYCAP, cap.as_mut_ptr())? };

    let driver = read_fixed_string(&cap[0..16]);
    let card = read_fixed_string(&cap[16..48]);
    let bus_info = read_fixed_string(&cap[48..80]);
    let version = u32::from_ne_bytes(cap[80..84].try_into()?);
    let capabilities = u32::from_ne_bytes(cap[84..88].try_into()?);
    let device_caps = u32::from_ne_bytes(cap[88..92].try_into()?);

    // Use device_caps if available (V4L2_CAP_DEVICE_CAPS), otherwise capabilities
    let effective_caps = if capabilities & 0x80000000 != 0 {
        device_caps
    } else {
        capabilities
    };

    Ok(DeviceCapabilities {
        driver,
        card,
        bus_info,
        version,
        capabilities: effective_caps,
        has_capture: effective_caps & V4L2_CAP_VIDEO_CAPTURE != 0,
        has_output: effective_caps & V4L2_CAP_VIDEO_OUTPUT != 0,
        has_streaming: effective_caps & V4L2_CAP_STREAMING != 0,
        has_readwrite: effective_caps & V4L2_CAP_READWRITE != 0,
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceCapabilities {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub version: u32,
    pub capabilities: u32,
    pub has_capture: bool,
    pub has_output: bool,
    pub has_streaming: bool,
    pub has_readwrite: bool,
}

impl DeviceCapabilities {
    pub fn version_string(&self) -> String {
        format!(
            "{}.{}.{}",
            (self.version >> 16) & 0xFF,
            (self.version >> 8) & 0xFF,
            self.version & 0xFF
        )
    }
}

/// Enumerate all supported pixel formats
pub fn enumerate_formats(fd: RawFd) -> Result<Vec<FormatDescription>> {
    let mut formats = Vec::new();
    let mut index: u32 = 0;

    loop {
        let mut fmtdesc = [0u8; 64]; // struct v4l2_fmtdesc
                                     // Set index
        fmtdesc[0..4].copy_from_slice(&index.to_ne_bytes());
        // Set type = VIDEO_CAPTURE
        fmtdesc[4..8].copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes());

        match unsafe { v4l2_ioctl(fd, VIDIOC_ENUM_FMT, fmtdesc.as_mut_ptr()) } {
            Ok(()) => {
                let flags = u32::from_ne_bytes(fmtdesc[8..12].try_into()?);
                let description = read_fixed_string(&fmtdesc[12..44]);
                let pixelformat = u32::from_ne_bytes(fmtdesc[44..48].try_into()?);

                formats.push(FormatDescription {
                    index,
                    flags,
                    description,
                    pixel_format: PixelFormat::from_fourcc(pixelformat),
                    fourcc_raw: pixelformat,
                });
                index += 1;
            }
            Err(_) => break,
        }
    }

    Ok(formats)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FormatDescription {
    pub index: u32,
    pub flags: u32,
    pub description: String,
    pub pixel_format: PixelFormat,
    pub fourcc_raw: u32,
}

/// Enumerate frame sizes for a pixel format
pub fn enumerate_frame_sizes(fd: RawFd, fourcc: u32) -> Result<Vec<FrameSize>> {
    let mut sizes = Vec::new();
    let mut index: u32 = 0;

    loop {
        let mut frmsizeenum = [0u8; 44]; // struct v4l2_frmsizeenum
        frmsizeenum[0..4].copy_from_slice(&index.to_ne_bytes());
        frmsizeenum[4..8].copy_from_slice(&fourcc.to_ne_bytes());

        match unsafe { v4l2_ioctl(fd, VIDIOC_ENUM_FRAMESIZES, frmsizeenum.as_mut_ptr()) } {
            Ok(()) => {
                let frmsize_type = u32::from_ne_bytes(frmsizeenum[8..12].try_into()?);
                if frmsize_type == V4L2_FRMSIZE_TYPE_DISCRETE {
                    let width = u32::from_ne_bytes(frmsizeenum[12..16].try_into()?);
                    let height = u32::from_ne_bytes(frmsizeenum[16..20].try_into()?);
                    sizes.push(FrameSize { width, height });
                }
                index += 1;
            }
            Err(_) => break,
        }
    }

    Ok(sizes)
}

#[derive(Debug, Clone, Copy)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}

/// Enumerate frame intervals for a format+size
pub fn enumerate_frame_intervals(
    fd: RawFd,
    fourcc: u32,
    width: u32,
    height: u32,
) -> Result<Vec<FrameInterval>> {
    let mut intervals = Vec::new();
    let mut index: u32 = 0;

    loop {
        let mut frmivalenum = [0u8; 52]; // struct v4l2_frmivalenum
        frmivalenum[0..4].copy_from_slice(&index.to_ne_bytes());
        frmivalenum[4..8].copy_from_slice(&fourcc.to_ne_bytes());
        frmivalenum[8..12].copy_from_slice(&width.to_ne_bytes());
        frmivalenum[12..16].copy_from_slice(&height.to_ne_bytes());

        match unsafe { v4l2_ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, frmivalenum.as_mut_ptr()) } {
            Ok(()) => {
                let frmival_type = u32::from_ne_bytes(frmivalenum[16..20].try_into()?);
                if frmival_type == V4L2_FRMIVAL_TYPE_DISCRETE {
                    let numerator = u32::from_ne_bytes(frmivalenum[20..24].try_into()?);
                    let denominator = u32::from_ne_bytes(frmivalenum[24..28].try_into()?);
                    intervals.push(FrameInterval {
                        numerator,
                        denominator,
                    });
                }
                index += 1;
            }
            Err(_) => break,
        }
    }

    Ok(intervals)
}

#[derive(Debug, Clone, Copy)]
pub struct FrameInterval {
    pub numerator: u32,
    pub denominator: u32,
}

/// Build the complete list of video modes supported by a device
pub fn enumerate_all_modes(fd: RawFd) -> Result<Vec<VideoMode>> {
    let mut modes = Vec::new();
    let formats = enumerate_formats(fd)?;

    for fmt in &formats {
        let sizes = enumerate_frame_sizes(fd, fmt.fourcc_raw)?;
        for size in &sizes {
            let intervals = enumerate_frame_intervals(fd, fmt.fourcc_raw, size.width, size.height)?;
            for interval in &intervals {
                modes.push(VideoMode {
                    format: fmt.pixel_format,
                    width: size.width,
                    height: size.height,
                    fps_numerator: interval.numerator,
                    fps_denominator: interval.denominator,
                });
            }
        }
    }

    Ok(modes)
}

/// Enumerate all V4L2 controls on a device
pub fn enumerate_controls(fd: RawFd) -> Result<Vec<ControlValue>> {
    let mut controls = Vec::new();
    // Start at 0 so NEXT_CTRL returns the very first control (brightness = V4L2_CID_BASE)
    let mut id: u32 = 0;

    loop {
        let mut queryctrl = [0u8; 68]; // struct v4l2_queryctrl
        queryctrl[0..4].copy_from_slice(&(id | V4L2_CTRL_FLAG_NEXT_CTRL).to_ne_bytes());

        match unsafe { v4l2_ioctl(fd, VIDIOC_QUERYCTRL, queryctrl.as_mut_ptr()) } {
            Ok(()) => {
                let ctrl_id = u32::from_ne_bytes(queryctrl[0..4].try_into()?);
                let ctrl_type_raw = u32::from_ne_bytes(queryctrl[4..8].try_into()?);
                let name = read_fixed_string(&queryctrl[8..40]);
                let minimum = i32::from_ne_bytes(queryctrl[40..44].try_into()?) as i64;
                let maximum = i32::from_ne_bytes(queryctrl[44..48].try_into()?) as i64;
                let step = i32::from_ne_bytes(queryctrl[48..52].try_into()?) as i64;
                let default = i32::from_ne_bytes(queryctrl[52..56].try_into()?) as i64;
                let flags = u32::from_ne_bytes(queryctrl[56..60].try_into()?);

                // NEXT_CTRL returns the next control with ID > the query ID.
                // Use the returned ctrl_id as the next query — do NOT add 1.
                id = ctrl_id;

                if flags & V4L2_CTRL_FLAG_DISABLED != 0 {
                    continue;
                }

                let ctrl_type = match ctrl_type_raw {
                    V4L2_CTRL_TYPE_INTEGER => ControlType::Integer,
                    V4L2_CTRL_TYPE_BOOLEAN => ControlType::Boolean,
                    V4L2_CTRL_TYPE_MENU => ControlType::Menu,
                    V4L2_CTRL_TYPE_BUTTON => ControlType::Button,
                    V4L2_CTRL_TYPE_INTEGER64 => ControlType::Integer64,
                    V4L2_CTRL_TYPE_CTRL_CLASS => ControlType::CtrlClass,
                    V4L2_CTRL_TYPE_STRING => ControlType::String,
                    V4L2_CTRL_TYPE_BITMASK => ControlType::Bitmask,
                    V4L2_CTRL_TYPE_INTEGER_MENU => ControlType::IntegerMenu,
                    other => ControlType::Unknown(other),
                };

                // Skip control class entries
                if ctrl_type == ControlType::CtrlClass {
                    continue;
                }

                // Get current value
                let value = get_control(fd, ctrl_id).unwrap_or(0);

                // Get menu items if applicable
                let menu_items =
                    if ctrl_type == ControlType::Menu || ctrl_type == ControlType::IntegerMenu {
                        enumerate_menu_items(fd, ctrl_id, minimum as u32, maximum as u32)
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };

                controls.push(ControlValue {
                    name,
                    id: ctrl_id,
                    control_type: ctrl_type,
                    value,
                    minimum,
                    maximum,
                    step,
                    default,
                    flags,
                    menu_items,
                });
            }
            Err(_) => break,
        }
    }

    Ok(controls)
}

fn enumerate_menu_items(fd: RawFd, ctrl_id: u32, min: u32, max: u32) -> Result<Vec<MenuItem>> {
    let mut items = Vec::new();

    for i in min..=max {
        let mut querymenu = [0u8; 68]; // struct v4l2_querymenu
        querymenu[0..4].copy_from_slice(&ctrl_id.to_ne_bytes());
        querymenu[4..8].copy_from_slice(&i.to_ne_bytes());

        if unsafe { v4l2_ioctl(fd, VIDIOC_QUERYMENU, querymenu.as_mut_ptr()) }.is_ok() {
            let name = read_fixed_string(&querymenu[8..40]);
            items.push(MenuItem { index: i, name });
        }
    }

    Ok(items)
}

/// Get a single control's current value
pub fn get_control(fd: RawFd, id: u32) -> Result<i64> {
    let mut ctrl = [0u8; 8]; // struct v4l2_control { id, value }
    ctrl[0..4].copy_from_slice(&id.to_ne_bytes());
    unsafe { v4l2_ioctl(fd, VIDIOC_G_CTRL, ctrl.as_mut_ptr())? };
    Ok(i32::from_ne_bytes(ctrl[4..8].try_into()?) as i64)
}

/// Set a single control's value
pub fn set_control(fd: RawFd, id: u32, value: i32) -> Result<()> {
    let mut ctrl = [0u8; 8];
    ctrl[0..4].copy_from_slice(&id.to_ne_bytes());
    ctrl[4..8].copy_from_slice(&value.to_ne_bytes());
    unsafe { v4l2_ioctl(fd, VIDIOC_S_CTRL, ctrl.as_mut_ptr())? };
    Ok(())
}

/// Set capture format on a device
pub fn set_format(fd: RawFd, width: u32, height: u32, fourcc: u32) -> Result<()> {
    // v4l2_format struct layout (verified via offsetof on x86_64 Linux 6.17):
    //   type=0, pix.width=8, pix.height=12, pix.pixelformat=16
    let mut fmt = [0u8; 208]; // struct v4l2_format
    fmt[0..4].copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes()); // type
    fmt[8..12].copy_from_slice(&width.to_ne_bytes()); // pix.width
    fmt[12..16].copy_from_slice(&height.to_ne_bytes()); // pix.height
    fmt[16..20].copy_from_slice(&fourcc.to_ne_bytes()); // pix.pixelformat

    unsafe { v4l2_ioctl(fd, VIDIOC_S_FMT, fmt.as_mut_ptr())? };
    Ok(())
}

/// Set output format on a v4l2loopback device
pub fn set_output_format(fd: RawFd, width: u32, height: u32, fourcc: u32) -> Result<()> {
    let mut fmt = [0u8; 208];
    fmt[0..4].copy_from_slice(&V4L2_BUF_TYPE_VIDEO_OUTPUT.to_ne_bytes());
    fmt[8..12].copy_from_slice(&width.to_ne_bytes());
    fmt[12..16].copy_from_slice(&height.to_ne_bytes());
    fmt[16..20].copy_from_slice(&fourcc.to_ne_bytes());

    unsafe { v4l2_ioctl(fd, VIDIOC_S_FMT, fmt.as_mut_ptr())? };
    Ok(())
}

/// Request MMAP buffers for capture
pub fn request_buffers(fd: RawFd, count: u32, buf_type: u32) -> Result<u32> {
    let mut reqbufs = [0u8; 20]; // struct v4l2_requestbuffers
    reqbufs[0..4].copy_from_slice(&count.to_ne_bytes());
    reqbufs[4..8].copy_from_slice(&buf_type.to_ne_bytes());
    reqbufs[8..12].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes());

    unsafe { v4l2_ioctl(fd, VIDIOC_REQBUFS, reqbufs.as_mut_ptr())? };
    Ok(u32::from_ne_bytes(reqbufs[0..4].try_into()?))
}

/// Start streaming
pub fn stream_on(fd: RawFd, buf_type: u32) -> Result<()> {
    let mut btype = buf_type.to_ne_bytes();
    unsafe { v4l2_ioctl(fd, VIDIOC_STREAMON, btype.as_mut_ptr())? };
    Ok(())
}

/// Stop streaming
pub fn stream_off(fd: RawFd, buf_type: u32) -> Result<()> {
    let mut btype = buf_type.to_ne_bytes();
    unsafe { v4l2_ioctl(fd, VIDIOC_STREAMOFF, btype.as_mut_ptr())? };
    Ok(())
}

/// Helper to read a null-terminated string from a fixed-size byte buffer
fn read_fixed_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).to_string()
}

/// Open a V4L2 device with O_NONBLOCK set so poll(2) drives timing.
pub fn open_device_nonblocking(path: &str) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("Failed to open V4L2 device (nonblocking): {}", path))
}

/// QUERYBUF — returns (length, offset) for one MMAP buffer.
/// Uses struct v4l2_buffer (88 bytes on x86_64 Linux 6.x). Field offsets
/// verified via offsetof against linux/videodev2.h on Linux 6.19:
/// index=0 type=4 bytesused=8 flags=12 field=16 timestamp=24 timecode=40
/// sequence=56 memory=60 m.offset=64 length=72.
pub fn query_buffer(fd: RawFd, index: u32, buf_type: u32) -> Result<(u32, u32)> {
    let mut buf = [0u8; 88];
    buf[0..4].copy_from_slice(&index.to_ne_bytes()); // index
    buf[4..8].copy_from_slice(&buf_type.to_ne_bytes()); // type
    buf[60..64].copy_from_slice(&consts::V4L2_MEMORY_MMAP.to_ne_bytes()); // memory
    unsafe { v4l2_ioctl(fd, consts::VIDIOC_QUERYBUF, buf.as_mut_ptr())? };
    let offset = u32::from_ne_bytes(buf[64..68].try_into()?); // m.offset (mmap)
    let length = u32::from_ne_bytes(buf[72..76].try_into()?); // length
    Ok((length, offset))
}

/// QBUF — enqueue a buffer for capture.
pub fn queue_buffer(fd: RawFd, index: u32, buf_type: u32) -> Result<()> {
    let mut buf = [0u8; 88];
    buf[0..4].copy_from_slice(&index.to_ne_bytes()); // index
    buf[4..8].copy_from_slice(&buf_type.to_ne_bytes()); // type
    buf[60..64].copy_from_slice(&consts::V4L2_MEMORY_MMAP.to_ne_bytes()); // memory
    unsafe { v4l2_ioctl(fd, consts::VIDIOC_QBUF, buf.as_mut_ptr())? };
    Ok(())
}

/// DQBUF — dequeue a filled buffer; returns (index, bytesused, sequence, ts_ms).
pub fn dequeue_buffer(fd: RawFd, buf_type: u32) -> Result<(u32, u32, u32, u64)> {
    let mut buf = [0u8; 88];
    buf[4..8].copy_from_slice(&buf_type.to_ne_bytes()); // type
    buf[60..64].copy_from_slice(&consts::V4L2_MEMORY_MMAP.to_ne_bytes()); // memory
    unsafe { v4l2_ioctl(fd, consts::VIDIOC_DQBUF, buf.as_mut_ptr())? };
    let index = u32::from_ne_bytes(buf[0..4].try_into()?); // index
    let bytesused = u32::from_ne_bytes(buf[8..12].try_into()?); // bytesused
    let secs = i64::from_ne_bytes(buf[24..32].try_into()?); // timestamp.tv_sec
    let sequence = u32::from_ne_bytes(buf[56..60].try_into()?); // sequence
    let ts_ms = (secs as u64) * 1000;
    Ok((index, bytesused, sequence, ts_ms))
}

/// mmap a buffer returned by QUERYBUF. Returns a *mut u8 pointing at length bytes.
/// Caller is responsible for munmap_buffer when done.
pub fn mmap_buffer(fd: RawFd, length: u32, offset: u32) -> Result<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            offset as i64,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("mmap");
    }
    Ok(ptr as *mut u8)
}

pub fn munmap_buffer(ptr: *mut u8, length: u32) -> Result<()> {
    let r = unsafe { libc::munmap(ptr as *mut libc::c_void, length as usize) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("munmap");
    }
    Ok(())
}

/// Common control name to ID mapping
pub fn control_name_to_id(name: &str) -> Option<u32> {
    match name.to_lowercase().as_str() {
        "brightness" => Some(V4L2_CID_BRIGHTNESS),
        "contrast" => Some(V4L2_CID_CONTRAST),
        "saturation" => Some(V4L2_CID_SATURATION),
        "sharpness" => Some(V4L2_CID_SHARPNESS),
        "exposure_auto" | "auto_exposure" => Some(V4L2_CID_EXPOSURE_AUTO),
        "exposure_absolute" | "exposure" => Some(V4L2_CID_EXPOSURE_ABSOLUTE),
        "zoom_absolute" | "zoom" => Some(V4L2_CID_ZOOM_ABSOLUTE),
        "pan_absolute" | "pan" => Some(V4L2_CID_PAN_ABSOLUTE),
        "tilt_absolute" | "tilt" => Some(V4L2_CID_TILT_ABSOLUTE),
        "white_balance_temperature_auto" | "auto_white_balance" => Some(V4L2_CID_BASE + 12),
        "white_balance_temperature" => Some(V4L2_CID_BASE + 26),
        "power_line_frequency" | "anti_flicker" => Some(V4L2_CID_BASE + 24),
        "gain" => Some(V4L2_CID_BASE + 19),
        _ => None,
    }
}

#[cfg(test)]
mod mmap_helper_tests {
    use super::*;

    #[test]
    fn open_device_nonblocking_returns_o_nonblock_fd() {
        use std::os::unix::io::AsRawFd;
        let f = open_device_nonblocking("/dev/null").expect("open");
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_NONBLOCK, libc::O_NONBLOCK);
    }
}
