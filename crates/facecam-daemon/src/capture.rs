use anyhow::{Context, Result};
use facecam_common::formats::VideoMode;
use facecam_common::v4l2;
use std::os::unix::io::RawFd;
use std::time::Duration;

const BUF_TYPE_CAPTURE: u32 = 1;
const BUFFER_COUNT: u32 = 4;

struct MmapBuffer {
    ptr: *mut u8,
    length: u32,
}

unsafe impl Send for MmapBuffer {}

pub struct CaptureSession {
    fd: RawFd,
    buffers: Vec<MmapBuffer>,
    pub mode: VideoMode,
    streaming: bool,
}

impl CaptureSession {
    pub fn start(fd: RawFd, mode: VideoMode) -> Result<Self> {
        let allocated =
            v4l2::request_buffers(fd, BUFFER_COUNT, BUF_TYPE_CAPTURE).context("REQBUFS")?;
        if allocated == 0 {
            anyhow::bail!("REQBUFS allocated 0 buffers");
        }
        let mut buffers = Vec::with_capacity(allocated as usize);
        for i in 0..allocated {
            let (length, offset) = v4l2::query_buffer(fd, i, BUF_TYPE_CAPTURE)
                .with_context(|| format!("QUERYBUF index={}", i))?;
            let ptr = v4l2::mmap_buffer(fd, length, offset)
                .with_context(|| format!("mmap index={}", i))?;
            buffers.push(MmapBuffer { ptr, length });
            v4l2::queue_buffer(fd, i, BUF_TYPE_CAPTURE)
                .with_context(|| format!("QBUF index={}", i))?;
        }
        v4l2::stream_on(fd, BUF_TYPE_CAPTURE).context("STREAMON")?;
        Ok(Self {
            fd,
            buffers,
            mode,
            streaming: true,
        })
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<FrameRef<'_>> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            return Err(std::io::Error::last_os_error()).context("poll() in capture loop");
        }
        if n == 0 {
            anyhow::bail!("poll timeout waiting for frame ({}ms)", timeout_ms);
        }
        let (index, bytesused, sequence, ts_ms) =
            v4l2::dequeue_buffer(self.fd, BUF_TYPE_CAPTURE).context("DQBUF")?;
        let buf = &self.buffers[index as usize];
        let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, bytesused as usize) };
        Ok(FrameRef {
            _session: std::marker::PhantomData,
            fd: self.fd,
            streaming_at_construct: self.streaming,
            index,
            bytes,
            seq: sequence,
            timestamp: Duration::from_millis(ts_ms),
        })
    }

    pub fn stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        let _ = v4l2::stream_off(self.fd, BUF_TYPE_CAPTURE);
        for b in &self.buffers {
            let _ = v4l2::munmap_buffer(b.ptr, b.length);
        }
        self.buffers.clear();
        let _ = v4l2::request_buffers(self.fd, 0, BUF_TYPE_CAPTURE);
        self.streaming = false;
        Ok(())
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub struct FrameRef<'a> {
    _session: std::marker::PhantomData<&'a mut CaptureSession>,
    fd: RawFd,
    streaming_at_construct: bool,
    index: u32,
    pub bytes: &'a [u8],
    pub seq: u32,
    pub timestamp: Duration,
}

impl Drop for FrameRef<'_> {
    fn drop(&mut self) {
        if self.streaming_at_construct {
            if let Err(e) = v4l2::queue_buffer(self.fd, self.index, BUF_TYPE_CAPTURE) {
                tracing::warn!(error = %e, index = self.index, "QBUF on FrameRef drop failed");
            }
        }
    }
}
