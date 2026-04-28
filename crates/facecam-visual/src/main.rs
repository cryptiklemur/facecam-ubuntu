#![allow(clippy::too_many_arguments)]
/// facecam-visual — Live visual diagnostic harness for the Elgato Facecam
///
/// Opens the camera, captures frames, decodes them, and displays in a window
/// with real-time diagnostic overlay: FPS, frame timing, jitter, resolution,
/// format, controls, and device health.
///
/// Keyboard controls:
///   Q/Esc     — Quit
///   1-4       — Switch profile (default/streaming/lowlight/meeting)
///   M         — Toggle MJPEG / UYVY format
///   +/-       — Brightness up/down
///   [/]       — Contrast up/down
///   Z/X       — Zoom in/out
///   S         — Snapshot (save current frame + diagnostics)
///   R         — Force USB reset + recovery
///   Space     — Pause/unpause
///   F         — Toggle FPS overlay detail
mod capture;
mod overlays;

use anyhow::{bail, Context, Result};
use capture::MmapCapture;
use clap::Parser;
use facecam_common::device::ProductDescriptor;
use facecam_common::{usb, v4l2};
use minifb::{Key, Window, WindowOptions};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "facecam-visual")]
#[command(about = "Live visual diagnostic harness — see your Facecam with real-time stats")]
struct Cli {
    /// V4L2 device path (auto-detected if omitted)
    #[arg(long)]
    device: Option<String>,

    /// Start in MJPEG mode (default: auto-select best)
    #[arg(long)]
    mjpeg: bool,

    /// Target resolution: 1080, 720, or 540
    #[arg(long, default_value = "720")]
    resolution: u32,

    /// Target FPS: 30 or 60
    #[arg(long, default_value = "30")]
    fps: u32,
}

/// Rolling statistics tracker
struct FrameStats {
    frame_times: VecDeque<Duration>,
    frame_sizes: VecDeque<usize>,
    total_frames: u64,
    dropped_frames: u64,
    start_time: Instant,
    last_frame: Instant,
    max_window: usize,
}

impl FrameStats {
    fn new() -> Self {
        Self {
            frame_times: VecDeque::new(),
            frame_sizes: VecDeque::new(),
            total_frames: 0,
            dropped_frames: 0,
            start_time: Instant::now(),
            last_frame: Instant::now(),
            max_window: 120,
        }
    }

    fn record_frame(&mut self, size: usize) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame);
        self.last_frame = now;
        self.total_frames += 1;

        self.frame_times.push_back(dt);
        self.frame_sizes.push_back(size);
        if self.frame_times.len() > self.max_window {
            self.frame_times.pop_front();
            self.frame_sizes.pop_front();
        }
    }

    fn fps(&self) -> f64 {
        if self.frame_times.len() < 2 {
            return 0.0;
        }
        let total: Duration = self.frame_times.iter().sum();
        let avg = total.as_secs_f64() / self.frame_times.len() as f64;
        if avg > 0.0 {
            1.0 / avg
        } else {
            0.0
        }
    }

    fn avg_frame_time_ms(&self) -> f64 {
        if self.frame_times.is_empty() {
            return 0.0;
        }
        let total: Duration = self.frame_times.iter().sum();
        total.as_secs_f64() * 1000.0 / self.frame_times.len() as f64
    }

    fn jitter_ms(&self) -> f64 {
        if self.frame_times.len() < 2 {
            return 0.0;
        }
        let avg = self.avg_frame_time_ms();
        let variance: f64 = self
            .frame_times
            .iter()
            .map(|dt| {
                let diff = dt.as_secs_f64() * 1000.0 - avg;
                diff * diff
            })
            .sum::<f64>()
            / self.frame_times.len() as f64;
        variance.sqrt()
    }

    fn avg_frame_size(&self) -> usize {
        if self.frame_sizes.is_empty() {
            return 0;
        }
        self.frame_sizes.iter().sum::<usize>() / self.frame_sizes.len()
    }

    fn uptime_secs(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }
}

// Tiny 5x7 bitmap font for overlay rendering — no external font deps
mod font {
    // Each char is 5 wide x 7 tall, stored as 7 bytes (one per row, LSB=left)
    const FONT_DATA: &[u8] = include_bytes!("font5x7.bin");

    pub fn char_bitmap(c: char) -> &'static [u8] {
        let idx = c as usize;
        if (32..128).contains(&idx) {
            let offset = (idx - 32) * 7;
            if offset + 7 <= FONT_DATA.len() {
                return &FONT_DATA[offset..offset + 7];
            }
        }
        &[0; 7]
    }

    pub fn draw_string(buf: &mut [u32], buf_w: usize, x: usize, y: usize, text: &str, color: u32) {
        let mut cx = x;
        for ch in text.chars() {
            let bitmap = char_bitmap(ch);
            for (row, &bits) in bitmap.iter().enumerate() {
                let py = y + row;
                if py >= buf.len() / buf_w {
                    continue;
                }
                for col in 0..5 {
                    if bits & (1 << col) != 0 {
                        let px = cx + col;
                        if px < buf_w {
                            let idx = py * buf_w + px;
                            if idx < buf.len() {
                                buf[idx] = color;
                            }
                        }
                    }
                }
            }
            cx += 6; // 5px char + 1px gap
        }
    }

    pub fn draw_string_bg(
        buf: &mut [u32],
        buf_w: usize,
        x: usize,
        y: usize,
        text: &str,
        fg: u32,
        bg: u32,
    ) {
        // Draw background
        let text_w = text.len() * 6;
        for row in 0..9 {
            let py = y + row;
            if py >= buf.len() / buf_w {
                break;
            }
            for col in 0..text_w + 2 {
                let px = x + col;
                if px < buf_w {
                    let idx = py * buf_w + px;
                    if idx < buf.len() {
                        buf[idx] = bg;
                    }
                }
            }
        }
        draw_string(buf, buf_w, x + 1, y + 1, text, fg);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Detect device
    let dev_path = resolve_device(&cli.device)?;
    println!("Opening: {}", dev_path);

    // Determine format and resolution
    let (width, height) = match cli.resolution {
        1080 => (1920u32, 1080u32),
        720 => (1280, 720),
        540 => (960, 540),
        other => {
            eprintln!("Unsupported resolution: {}p. Using 720p.", other);
            (1280, 720)
        }
    };

    let use_mjpeg = cli.mjpeg;
    let fourcc = if use_mjpeg {
        u32::from_le_bytes(*b"MJPG")
    } else {
        u32::from_le_bytes(*b"UYVY")
    };
    let format_name = if use_mjpeg { "MJPEG" } else { "UYVY" };

    println!(
        "Mode: {}x{} @ {}fps {}",
        width, height, cli.fps, format_name
    );

    // Open device and set format
    let file = v4l2::open_device(&dev_path)?;
    let fd = file.as_raw_fd();

    let caps = v4l2::query_capabilities(fd)?;
    println!("Card: {}", caps.card);
    println!("Driver: {} v{}", caps.driver, caps.version_string());

    // Set format — the driver may negotiate a different resolution/format
    let _ = v4l2::set_format(fd, width, height, fourcc);

    // Read back actual negotiated format
    let (actual_w, actual_h, actual_fourcc) = get_actual_format(fd)?;
    let width = actual_w;
    let height = actual_h;
    let use_mjpeg = &actual_fourcc == b"MJPG";
    let format_name: String = if use_mjpeg {
        "MJPEG".to_string()
    } else {
        String::from_utf8_lossy(&actual_fourcc).to_string()
    };
    println!("Negotiated: {}x{} {}", width, height, format_name);

    // Read initial control values
    let controls = v4l2::enumerate_controls(fd).unwrap_or_default();
    let mut brightness = get_ctrl_value(&controls, "Brightness");
    let mut contrast = get_ctrl_value(&controls, "Contrast");
    let mut zoom = get_ctrl_value(&controls, "Zoom, Absolute");

    // Create window — video + expanded diagnostic panel
    // Layout: video (top) | waveform+histogram (128px) | stats (24px) | waterfall (32px)
    let panel_scopes_h: usize = 128;
    let panel_stats_h: usize = 24;
    let panel_waterfall_h: usize = 32;
    let panel_total_h = panel_scopes_h + panel_stats_h + panel_waterfall_h + 4; // +4 for separators
    let win_w = width as usize;
    let win_h = height as usize + panel_total_h;
    let mut window = Window::new(
        &format!(
            "Facecam Visual Diagnostic — {}x{} {}",
            width, height, format_name
        ),
        win_w,
        win_h,
        WindowOptions {
            resize: false,
            scale: minifb::Scale::X1,
            ..WindowOptions::default()
        },
    )?;

    // Limit update rate to ~120fps to not burn CPU on window updates
    window.set_target_fps(120);

    let mut framebuf = vec![0u32; win_w * win_h];
    let mut stats = FrameStats::new();
    let mut paused = false;
    let mut show_detail = true;
    let mut snapshot_count = 0u32;
    let mut rgb_buf = vec![0u8; (width * height * 3) as usize];

    // Overlay state
    let mut zebras_on = false;
    let mut focus_peak_on = false;
    let mut ab_compare = overlays::ABCompare::new();
    let mut waterfall = overlays::TimingWaterfall::new(win_w);

    // Set up MMAP streaming capture (the correct V4L2 I/O method for UVC cameras)
    let mut capture = MmapCapture::new(fd, 4).context("Failed to set up MMAP capture")?;
    capture.start().context("Failed to start streaming")?;
    println!("MMAP streaming started with 4 buffers.\n");

    let vid_h = height as usize;
    let vid_w = width as usize;

    println!("Streaming... Press Q or Esc to quit.\n");
    println!("Keys: +/- Bright  [/] Contrast  Z/X Zoom  W Zebras  E FocusPeak");
    println!("      A Capture-ref  D Clear-ref  </> Move-split  S Snap  R Reset");
    println!("      Space Pause  F Detail  Q Quit\n");

    while window.is_open() && !window.is_key_down(Key::Escape) && !window.is_key_down(Key::Q) {
        // Handle keyboard input
        handle_keys(
            &window,
            fd,
            &mut brightness,
            &mut contrast,
            &mut zoom,
            &mut paused,
            &mut show_detail,
            &mut snapshot_count,
            &framebuf,
            win_w,
            win_h,
            &mut zebras_on,
            &mut focus_peak_on,
            &mut ab_compare,
        )?;

        if !paused {
            match capture.dequeue_frame() {
                Ok((buf_idx, data)) => {
                    stats.record_frame(data.len());

                    // Feed waterfall
                    if let Some(last) = stats.frame_times.back() {
                        waterfall.push(last.as_secs_f64() * 1000.0);
                    }

                    // Convert frame to ARGB pixels
                    if use_mjpeg {
                        decode_mjpeg(data, &mut rgb_buf, &mut framebuf, vid_w, vid_h, win_w);
                    } else {
                        decode_uyvy(data, &mut framebuf, vid_w, vid_h, win_w);
                    }

                    // Return buffer to driver
                    if let Err(e) = capture.enqueue_buffer(buf_idx) {
                        eprintln!("Failed to re-enqueue buffer: {}", e);
                    }

                    // === Apply video overlays ===

                    // A/B split (draw reference on left side)
                    if ab_compare.has_reference() {
                        ab_compare.draw_split(&mut framebuf, vid_w, vid_h);
                    }

                    // Zebra stripes
                    if zebras_on {
                        overlays::draw_zebras(&mut framebuf, vid_w, vid_h, 235, stats.total_frames);
                    }

                    // Focus peaking
                    if focus_peak_on {
                        overlays::draw_focus_peaking(&mut framebuf, vid_w, vid_h, 30, 0xFF00FF);
                    }
                }
                Err(e) => {
                    stats.dropped_frames += 1;
                    if stats.dropped_frames % 100 == 1 {
                        eprintln!("Frame capture error: {}", e);
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(16));
        }

        // === Draw diagnostic panel ===
        let scope_y = vid_h + 1;
        let scope_h = panel_scopes_h;
        let stats_y = scope_y + scope_h + 1;
        let wf_y = stats_y + panel_stats_h + 1;

        // Separator line
        for x in 0..win_w {
            let idx = vid_h * win_w + x;
            if idx < framebuf.len() {
                framebuf[idx] = 0x00FF88;
            }
        }

        // Snapshot video region for scope analysis (avoids borrow conflict)
        let video_snapshot: Vec<u32> = framebuf[..vid_w * vid_h].to_vec();

        // Waveform (left half) + RGB Histogram (right half)
        let half_w = win_w / 2;
        overlays::draw_waveform(
            &mut framebuf,
            win_w,
            &video_snapshot,
            vid_w,
            vid_h,
            0,
            scope_y,
            half_w,
            scope_h,
        );
        overlays::draw_rgb_histogram(
            &mut framebuf,
            win_w,
            &video_snapshot,
            vid_w,
            vid_h,
            half_w,
            scope_y,
            half_w,
            scope_h,
        );

        // Stats text line
        let fps = stats.fps();
        let fps_color = if fps >= 28.0 {
            0x00FF88
        } else if fps >= 15.0 {
            0xFFCC00
        } else {
            0xFF4444
        };
        let ab_str = if ab_compare.has_reference() {
            " [A/B]"
        } else {
            ""
        };
        let overlay_str = format!(
            "{}{}",
            if zebras_on { " [ZEBRA]" } else { "" },
            if focus_peak_on { " [FOCUS]" } else { "" },
        );
        let stats_line = format!(
            " {:.1}fps {:.1}ms jit:{:.1}ms {}KB  Brt:{} Con:{} Zm:{}  {:.0}s{}{}{}",
            fps,
            stats.avg_frame_time_ms(),
            stats.jitter_ms(),
            stats.avg_frame_size() / 1024,
            brightness,
            contrast,
            zoom,
            stats.uptime_secs(),
            if paused { " PAUSED" } else { "" },
            ab_str,
            overlay_str,
        );
        // Clear stats row
        for x in 0..win_w {
            for dy in 0..panel_stats_h {
                let idx = (stats_y + dy) * win_w + x;
                if idx < framebuf.len() {
                    framebuf[idx] = 0x0D0D1A;
                }
            }
        }
        font::draw_string(&mut framebuf, win_w, 2, stats_y + 4, &stats_line, fps_color);

        // Help line
        if show_detail {
            let help =
                " +/- Brt  [/] Con  Z/X Zm  W Zebra  E Focus  A Ref  D Clear  S Snap  Q Quit";
            font::draw_string(&mut framebuf, win_w, 2, stats_y + 14, help, 0x666688);
        }

        // Waterfall
        let target_ms = if cli.fps >= 50 { 16.7 } else { 33.3 };
        waterfall.draw(
            &mut framebuf,
            win_w,
            0,
            wf_y,
            win_w,
            panel_waterfall_h,
            target_ms,
        );

        // FPS badge on video
        let badge = format!(" {:.0} FPS ", fps);
        font::draw_string_bg(&mut framebuf, win_w, 4, 4, &badge, fps_color, 0x000000);

        // Overlay indicators on video
        let mut indicator_y = 4;
        if zebras_on {
            font::draw_string_bg(
                &mut framebuf,
                win_w,
                win_w - 60,
                indicator_y,
                " ZEBRA ",
                0xFF4444,
                0x000000,
            );
            indicator_y += 12;
        }
        if focus_peak_on {
            font::draw_string_bg(
                &mut framebuf,
                win_w,
                win_w - 60,
                indicator_y,
                " FOCUS ",
                0xFF00FF,
                0x000000,
            );
            indicator_y += 12;
        }
        if ab_compare.has_reference() {
            font::draw_string_bg(
                &mut framebuf,
                win_w,
                win_w - 60,
                indicator_y,
                "  A/B  ",
                0x00CCFF,
                0x000000,
            );
        }

        // Update window
        window.update_with_buffer(&framebuf, win_w, win_h)?;
    }

    println!("\nSession summary:");
    println!("  Total frames: {}", stats.total_frames);
    println!("  Dropped:      {}", stats.dropped_frames);
    println!("  Avg FPS:      {:.1}", stats.fps());
    println!("  Avg frame:    {:.1}ms", stats.avg_frame_time_ms());
    println!("  Jitter:       {:.2}ms", stats.jitter_ms());
    println!("  Uptime:       {:.1}s", stats.uptime_secs());

    Ok(())
}

fn handle_keys(
    window: &Window,
    fd: i32,
    brightness: &mut i64,
    contrast: &mut i64,
    zoom: &mut i64,
    paused: &mut bool,
    show_detail: &mut bool,
    snap_count: &mut u32,
    framebuf: &[u32],
    win_w: usize,
    win_h: usize,
    zebras_on: &mut bool,
    focus_peak_on: &mut bool,
    ab_compare: &mut overlays::ABCompare,
) -> Result<()> {
    // Brightness +/-
    if window.is_key_pressed(Key::Equal, minifb::KeyRepeat::Yes)
        || window.is_key_pressed(Key::NumPadPlus, minifb::KeyRepeat::Yes)
    {
        *brightness = (*brightness + 10).min(255);
        let _ = v4l2::set_control(fd, 0x00980900, *brightness as i32);
    }
    if window.is_key_pressed(Key::Minus, minifb::KeyRepeat::Yes)
        || window.is_key_pressed(Key::NumPadMinus, minifb::KeyRepeat::Yes)
    {
        *brightness = (*brightness - 10).max(0);
        let _ = v4l2::set_control(fd, 0x00980900, *brightness as i32);
    }

    // Contrast [/]
    if window.is_key_pressed(Key::RightBracket, minifb::KeyRepeat::Yes) {
        *contrast = (*contrast + 1).min(10);
        let _ = v4l2::set_control(fd, 0x00980901, *contrast as i32);
    }
    if window.is_key_pressed(Key::LeftBracket, minifb::KeyRepeat::Yes) {
        *contrast = (*contrast - 1).max(0);
        let _ = v4l2::set_control(fd, 0x00980901, *contrast as i32);
    }

    // Zoom Z/X
    if window.is_key_pressed(Key::Z, minifb::KeyRepeat::Yes) {
        *zoom = (*zoom + 1).min(31);
        let _ = v4l2::set_control(fd, 0x009a090d, *zoom as i32);
    }
    if window.is_key_pressed(Key::X, minifb::KeyRepeat::Yes) {
        *zoom = (*zoom - 1).max(1);
        let _ = v4l2::set_control(fd, 0x009a090d, *zoom as i32);
    }

    // Pause
    if window.is_key_pressed(Key::Space, minifb::KeyRepeat::No) {
        *paused = !*paused;
    }

    // Detail toggle
    if window.is_key_pressed(Key::F, minifb::KeyRepeat::No) {
        *show_detail = !*show_detail;
    }

    // Snapshot
    if window.is_key_pressed(Key::S, minifb::KeyRepeat::No) {
        *snap_count += 1;
        let dir = format!(
            "{}/facecam-snapshots",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
        );
        let _ = fs::create_dir_all(&dir);
        let path = format!("{}/snap-{:04}.ppm", dir, snap_count);

        // Save as PPM (simple, no extra deps)
        if let Ok(mut f) = File::create(&path) {
            let w = win_w;
            let h = win_h;
            let _ = write!(f, "P6\n{} {}\n255\n", w, h);
            for &pixel in framebuf.iter() {
                let r = ((pixel >> 16) & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let b = (pixel & 0xFF) as u8;
                let _ = f.write_all(&[r, g, b]);
            }
            eprintln!("Snapshot saved: {}", path);
        }
    }

    // USB Reset
    if window.is_key_pressed(Key::R, minifb::KeyRepeat::No) {
        eprintln!("USB reset requested...");
        let product = facecam_common::usb::detect_product_or_default();
        match facecam_common::recovery::usb_reset_product(product) {
            Ok(_) => eprintln!("USB reset complete. Device should re-appear."),
            Err(e) => eprintln!("USB reset failed: {}", e),
        }
    }

    // Zebra stripes toggle
    if window.is_key_pressed(Key::W, minifb::KeyRepeat::No) {
        *zebras_on = !*zebras_on;
        eprintln!("Zebras: {}", if *zebras_on { "ON" } else { "OFF" });
    }

    // Focus peaking toggle
    if window.is_key_pressed(Key::E, minifb::KeyRepeat::No) {
        *focus_peak_on = !*focus_peak_on;
        eprintln!(
            "Focus peaking: {}",
            if *focus_peak_on { "ON" } else { "OFF" }
        );
    }

    // A/B compare — capture reference
    if window.is_key_pressed(Key::A, minifb::KeyRepeat::No) {
        ab_compare.capture_reference(framebuf, win_w, win_h);
        eprintln!("A/B reference captured");
    }

    // A/B compare — clear reference
    if window.is_key_pressed(Key::D, minifb::KeyRepeat::No) {
        ab_compare.clear_reference();
        eprintln!("A/B reference cleared");
    }

    // A/B split position
    if window.is_key_pressed(Key::Comma, minifb::KeyRepeat::Yes) {
        ab_compare.move_split(-0.05);
    }
    if window.is_key_pressed(Key::Period, minifb::KeyRepeat::Yes) {
        ab_compare.move_split(0.05);
    }

    Ok(())
}

/// Decode UYVY (YUV 4:2:2 packed) to ARGB framebuffer
fn decode_uyvy(data: &[u8], buf: &mut [u32], width: usize, height: usize, buf_w: usize) {
    // UYVY: [U0 Y0 V0 Y1] -> 2 pixels
    let expected = width * height * 2;
    let len = data.len().min(expected);

    let mut src = 0;
    let mut px = 0;
    while src + 3 < len && px + 1 < width * height {
        let u = data[src] as f64 - 128.0;
        let y0 = data[src + 1] as f64;
        let v = data[src + 2] as f64 - 128.0;
        let y1 = data[src + 3] as f64;

        let row0 = px / width;
        let col0 = px % width;

        // Pixel 0
        if row0 < height && col0 < width {
            let (r, g, b) = yuv_to_rgb(y0, u, v);
            let idx = row0 * buf_w + col0;
            if idx < buf.len() {
                buf[idx] = (r as u32) << 16 | (g as u32) << 8 | (b as u32);
            }
        }

        // Pixel 1
        let col1 = col0 + 1;
        if row0 < height && col1 < width {
            let (r, g, b) = yuv_to_rgb(y1, u, v);
            let idx = row0 * buf_w + col1;
            if idx < buf.len() {
                buf[idx] = (r as u32) << 16 | (g as u32) << 8 | (b as u32);
            }
        }

        src += 4;
        px += 2;
    }
}

fn yuv_to_rgb(y: f64, u: f64, v: f64) -> (u8, u8, u8) {
    let r = (y + 1.402 * v).clamp(0.0, 255.0) as u8;
    let g = (y - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0) as u8;
    let b = (y + 1.772 * u).clamp(0.0, 255.0) as u8;
    (r, g, b)
}

/// Decode MJPEG frame to ARGB framebuffer
fn decode_mjpeg(
    data: &[u8],
    _rgb_buf: &mut [u8],
    buf: &mut [u32],
    width: usize,
    height: usize,
    buf_w: usize,
) {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    // Force RGB output regardless of JPEG color space
    decoder.set_color_transform(jpeg_decoder::ColorTransform::RGB);
    match decoder.decode() {
        Ok(pixels) => {
            let info = decoder.info().unwrap();
            let decoded_w = info.width as usize;
            let decoded_h = info.height as usize;
            let bpp = match info.pixel_format {
                jpeg_decoder::PixelFormat::RGB24 => 3,
                jpeg_decoder::PixelFormat::L8 => 1,
                jpeg_decoder::PixelFormat::L16 => 2,
                jpeg_decoder::PixelFormat::CMYK32 => 4,
            };

            for y in 0..decoded_h.min(height) {
                for x in 0..decoded_w.min(width) {
                    let si = (y * decoded_w + x) * bpp;
                    let idx = y * buf_w + x;
                    if idx >= buf.len() {
                        continue;
                    }
                    match bpp {
                        3 => {
                            if si + 2 < pixels.len() {
                                buf[idx] = (pixels[si] as u32) << 16
                                    | (pixels[si + 1] as u32) << 8
                                    | (pixels[si + 2] as u32);
                            }
                        }
                        1 => {
                            if si < pixels.len() {
                                let v = pixels[si] as u32;
                                buf[idx] = v << 16 | v << 8 | v;
                            }
                        }
                        _ => {
                            // CMYK or L16 — just use first bytes as grayscale
                            if si < pixels.len() {
                                let v = pixels[si] as u32;
                                buf[idx] = v << 16 | v << 8 | v;
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            // Log first decode error only
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| eprintln!("MJPEG decode error: {:?}", e));
        }
    }
}

fn get_ctrl_value(controls: &[facecam_common::types::ControlValue], name: &str) -> i64 {
    controls
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.value)
        .unwrap_or(0)
}

/// Read back the actual format the driver negotiated (VIDIOC_G_FMT)
///
/// v4l2_format struct layout (verified via offsetof on x86_64 Linux 6.17):
///   type=0, pix.width=8, pix.height=12, pix.pixelformat=16
fn get_actual_format(fd: std::os::unix::io::RawFd) -> Result<(u32, u32, [u8; 4])> {
    let mut fmt = [0u8; 208];
    fmt[0..4].copy_from_slice(&1u32.to_ne_bytes()); // V4L2_BUF_TYPE_VIDEO_CAPTURE
    let ret = unsafe { libc::ioctl(fd, 0xC0D05604u64 as libc::c_ulong, fmt.as_mut_ptr()) };
    if ret < 0 {
        bail!("VIDIOC_G_FMT failed: {}", std::io::Error::last_os_error());
    }
    let width = u32::from_ne_bytes(fmt[8..12].try_into()?);
    let height = u32::from_ne_bytes(fmt[12..16].try_into()?);
    let fourcc: [u8; 4] = fmt[16..20].try_into()?;
    Ok((width, height, fourcc))
}

fn resolve_device(device: &Option<String>) -> Result<String> {
    if let Some(dev) = device {
        return Ok(dev.clone());
    }
    let devices = usb::enumerate_elgato_devices()?;
    for dev in &devices {
        if dev.product.is_usb2_fallback() {
            bail!("Facecam is in USB 2.0 fallback mode. Move to a USB 3.0 port.");
        }
        if !dev.product.is_uvc_capture() {
            continue;
        }
        if let Ok(Some(sysfs)) = usb::find_usb_sysfs_path(dev.usb_bus, dev.usb_address) {
            if let Ok(Some(v4l2_dev)) = usb::find_v4l2_device_for_usb(&sysfs) {
                return Ok(v4l2_dev);
            }
        }
    }
    bail!("Facecam not found. Is it connected to a USB 3.0 port?")
}
