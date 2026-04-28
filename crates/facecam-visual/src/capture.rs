/// V4L2 MMAP streaming capture — the correct way to read frames from UVC cameras.
///
/// UVC cameras (including the Elgato Facecam) do NOT support read() I/O.
/// They require the MMAP streaming protocol:
///   1. VIDIOC_REQBUFS — allocate kernel buffers
///   2. VIDIOC_QUERYBUF + mmap() — map each buffer to userspace
///   3. VIDIOC_QBUF — enqueue buffers for capture
///   4. VIDIOC_STREAMON — start capture
///   5. poll() + VIDIOC_DQBUF — wait for and dequeue filled frames
///   6. Process frame, then VIDIOC_QBUF to recycle the buffer
use anyhow::{bail, Result};
use std::os::unix::io::RawFd;
use std::ptr;

const VIDIOC_REQBUFS: u64 = 0xC0145608;
const VIDIOC_QUERYBUF: u64 = 0xC0585609;
const VIDIOC_QBUF: u64 = 0xC058560F;
const VIDIOC_DQBUF: u64 = 0xC0585611;
const VIDIOC_STREAMON: u64 = 0x40045612;
const VIDIOC_STREAMOFF: u64 = 0x40045613;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;

// struct v4l2_buffer offsets — verified via offsetof() on x86_64 Linux 6.17
const V4L2_BUF_SIZE: usize = 88; // sizeof(struct v4l2_buffer)
const BUF_INDEX: usize = 0; // __u32 index
const BUF_TYPE: usize = 4; // __u32 type
const BUF_BYTESUSED: usize = 8; // __u32 bytesused
const BUF_MEMORY: usize = 60; // __u32 memory
const BUF_LENGTH: usize = 72; // __u32 length
const BUF_M_OFFSET: usize = 64; // union m.offset (__u32)

unsafe fn ioctl(fd: RawFd, request: u64, arg: *mut u8) -> Result<()> {
    let ret = libc::ioctl(fd, request as libc::c_ulong, arg);
    if ret < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

struct MappedBuffer {
    ptr: *mut u8,
    length: usize,
}

pub struct MmapCapture {
    fd: RawFd,
    buffers: Vec<MappedBuffer>,
    streaming: bool,
}

impl MmapCapture {
    /// Set up MMAP capture with `num_buffers` kernel buffers
    pub fn new(fd: RawFd, num_buffers: u32) -> Result<Self> {
        // 1. Request buffers
        let mut reqbufs = [0u8; 20];
        reqbufs[0..4].copy_from_slice(&num_buffers.to_ne_bytes()); // count
        reqbufs[4..8].copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes()); // type
        reqbufs[8..12].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes()); // memory

        unsafe { ioctl(fd, VIDIOC_REQBUFS, reqbufs.as_mut_ptr())? };

        let granted = u32::from_ne_bytes(reqbufs[0..4].try_into()?);
        if granted < 2 {
            bail!("Need at least 2 buffers, got {}", granted);
        }

        // 2. Query and mmap each buffer
        let mut buffers = Vec::new();
        for i in 0..granted {
            let mut v4l2_buf = [0u8; V4L2_BUF_SIZE];
            v4l2_buf[BUF_INDEX..BUF_INDEX + 4].copy_from_slice(&i.to_ne_bytes());
            v4l2_buf[BUF_TYPE..BUF_TYPE + 4]
                .copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes());
            v4l2_buf[BUF_MEMORY..BUF_MEMORY + 4].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes());

            unsafe { ioctl(fd, VIDIOC_QUERYBUF, v4l2_buf.as_mut_ptr())? };

            let length =
                u32::from_ne_bytes(v4l2_buf[BUF_LENGTH..BUF_LENGTH + 4].try_into()?) as usize;
            let offset = u32::from_ne_bytes(v4l2_buf[BUF_M_OFFSET..BUF_M_OFFSET + 4].try_into()?);

            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    offset as libc::off_t,
                )
            };

            if ptr == libc::MAP_FAILED {
                bail!("mmap failed for buffer {}", i);
            }

            buffers.push(MappedBuffer {
                ptr: ptr as *mut u8,
                length,
            });
        }

        // 3. Enqueue all buffers
        for i in 0..granted {
            let mut v4l2_buf = [0u8; V4L2_BUF_SIZE];
            v4l2_buf[BUF_INDEX..BUF_INDEX + 4].copy_from_slice(&i.to_ne_bytes());
            v4l2_buf[BUF_TYPE..BUF_TYPE + 4]
                .copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes());
            v4l2_buf[BUF_MEMORY..BUF_MEMORY + 4].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes());

            unsafe { ioctl(fd, VIDIOC_QBUF, v4l2_buf.as_mut_ptr())? };
        }

        Ok(Self {
            fd,
            buffers,
            streaming: false,
        })
    }

    /// Start streaming
    pub fn start(&mut self) -> Result<()> {
        let mut buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes();
        unsafe { ioctl(self.fd, VIDIOC_STREAMON, buf_type.as_mut_ptr())? };
        self.streaming = true;
        Ok(())
    }

    /// Recovery rung 1: STREAMOFF, re-enqueue all buffers, STREAMON.
    /// Used to clear the Facecam Pro's PRO_STREAM_START_RACE — the first
    /// STREAMON after open returns no frames roughly half the time. A clean
    /// streamoff/requeue/streamon cycle reliably restores frame flow.
    pub fn restart_stream(&mut self) -> Result<()> {
        if self.streaming {
            let mut buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes();
            let _ = unsafe { ioctl(self.fd, VIDIOC_STREAMOFF, buf_type.as_mut_ptr()) };
            self.streaming = false;
        }
        for i in 0..self.buffers.len() as u32 {
            let mut v4l2_buf = [0u8; V4L2_BUF_SIZE];
            v4l2_buf[BUF_INDEX..BUF_INDEX + 4].copy_from_slice(&i.to_ne_bytes());
            v4l2_buf[BUF_TYPE..BUF_TYPE + 4]
                .copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes());
            v4l2_buf[BUF_MEMORY..BUF_MEMORY + 4].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes());
            unsafe { ioctl(self.fd, VIDIOC_QBUF, v4l2_buf.as_mut_ptr())? };
        }
        let mut buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes();
        unsafe { ioctl(self.fd, VIDIOC_STREAMON, buf_type.as_mut_ptr())? };
        self.streaming = true;
        Ok(())
    }

    /// Poll for an incoming frame without dequeuing. Returns `Ok(true)` if a
    /// frame is ready, `Ok(false)` on timeout, and `Err` if the poll itself
    /// fails. Used by warm-up paths that want to detect "no frames flowing"
    /// before committing to the steady-state loop.
    pub fn frame_ready(&self, timeout_ms: i32) -> Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(ret > 0)
    }

    /// Wait for and dequeue a frame. Returns (buffer_data, bytes_used).
    /// The data is valid until the next call to `dequeue_frame` with the same buffer index.
    pub fn dequeue_frame(&self) -> Result<(usize, &[u8])> {
        // Poll for readiness
        let mut pollfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pollfd, 1, 5000) }; // 5s timeout
        if ret <= 0 {
            bail!("poll timeout waiting for frame");
        }

        // Dequeue
        let mut v4l2_buf = [0u8; V4L2_BUF_SIZE];
        v4l2_buf[BUF_TYPE..BUF_TYPE + 4]
            .copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes());
        v4l2_buf[BUF_MEMORY..BUF_MEMORY + 4].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes());

        unsafe { ioctl(self.fd, VIDIOC_DQBUF, v4l2_buf.as_mut_ptr())? };

        let index = u32::from_ne_bytes(v4l2_buf[BUF_INDEX..BUF_INDEX + 4].try_into()?) as usize;
        let bytesused =
            u32::from_ne_bytes(v4l2_buf[BUF_BYTESUSED..BUF_BYTESUSED + 4].try_into()?) as usize;

        if index >= self.buffers.len() {
            bail!(
                "buffer index {} out of range (have {})",
                index,
                self.buffers.len()
            );
        }

        let buf = &self.buffers[index];

        // Use full buffer length if bytesused is 0 (some drivers do this for compressed formats)
        let effective_size = if bytesused > 0 && bytesused <= buf.length {
            bytesused
        } else {
            buf.length
        };

        let data = unsafe { std::slice::from_raw_parts(buf.ptr, effective_size) };

        // Find actual JPEG data start (search for SOI marker FFD8)
        // Some devices prepend padding or metadata before the JPEG data
        let jpeg_start = data
            .windows(2)
            .position(|w| w[0] == 0xFF && w[1] == 0xD8)
            .unwrap_or(0);

        Ok((index, &data[jpeg_start..]))
    }

    /// Re-enqueue a buffer after processing
    pub fn enqueue_buffer(&self, index: usize) -> Result<()> {
        let mut v4l2_buf = [0u8; V4L2_BUF_SIZE];
        v4l2_buf[BUF_INDEX..BUF_INDEX + 4].copy_from_slice(&(index as u32).to_ne_bytes());
        v4l2_buf[BUF_TYPE..BUF_TYPE + 4]
            .copy_from_slice(&V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes());
        v4l2_buf[BUF_MEMORY..BUF_MEMORY + 4].copy_from_slice(&V4L2_MEMORY_MMAP.to_ne_bytes());

        unsafe { ioctl(self.fd, VIDIOC_QBUF, v4l2_buf.as_mut_ptr())? };
        Ok(())
    }

    /// Stop streaming
    pub fn stop(&mut self) -> Result<()> {
        if self.streaming {
            let mut buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE.to_ne_bytes();
            let _ = unsafe { ioctl(self.fd, VIDIOC_STREAMOFF, buf_type.as_mut_ptr()) };
            self.streaming = false;
        }
        Ok(())
    }
}

impl Drop for MmapCapture {
    fn drop(&mut self) {
        let _ = self.stop();
        for buf in &self.buffers {
            unsafe {
                libc::munmap(buf.ptr as *mut libc::c_void, buf.length);
            }
        }
    }
}
