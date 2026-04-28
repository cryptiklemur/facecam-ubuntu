use anyhow::{bail, Context, Result};
use facecam_common::{
    profiles, quirks, recovery,
    types::{DaemonStatus, HealthStatus, PipelineState},
    usb, v4l2,
};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{debug, error, info, warn};

#[allow(dead_code)]
pub struct PipelineConfig {
    pub source_device: Option<String>,
    pub sink_device: String,
    pub profile_name: String,
    pub max_recovery_attempts: u32,
    pub frame_timeout_ms: u64,
}

/// Main pipeline loop — runs on a blocking thread.
///
/// Architecture:
///   Physical Facecam (/dev/videoN) -> [capture] -> [frame copy] -> [output] -> v4l2loopback (/dev/video10)
///
/// The daemon keeps the physical device open continuously to avoid the open/close lockup bug.
/// Consumer applications open/close the v4l2loopback device freely.
pub fn run(
    config: PipelineConfig,
    status_tx: Arc<Mutex<watch::Sender<DaemonStatus>>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut recovery_count: u32 = 0;
    let mut consecutive_failures: u32 = 0;

    loop {
        // Check for shutdown
        if shutdown_rx.try_recv().is_ok() {
            info!("Pipeline received shutdown signal");
            update_state(
                &status_tx,
                PipelineState::ShuttingDown,
                HealthStatus::Healthy,
            );
            return;
        }

        // Run one pipeline lifecycle
        match run_pipeline_once(&config, &status_tx, &mut shutdown_rx) {
            Ok(()) => {
                // Clean shutdown or signal
                info!("Pipeline exited cleanly");
                return;
            }
            Err(e) => {
                error!(error = %e, recovery_count, "Pipeline error");
                consecutive_failures += 1;

                if consecutive_failures > config.max_recovery_attempts {
                    error!(
                        attempts = consecutive_failures,
                        max = config.max_recovery_attempts,
                        "Max recovery attempts exceeded, entering failed state"
                    );
                    update_state(&status_tx, PipelineState::Failed, HealthStatus::Unhealthy);
                    update_error(&status_tx, Some(e.to_string()));

                    // Wait for external intervention or shutdown
                    loop {
                        if shutdown_rx.try_recv().is_ok() {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }

                // Attempt recovery
                update_state(
                    &status_tx,
                    PipelineState::Recovering,
                    HealthStatus::Degraded,
                );
                recovery_count += 1;
                update_recovery_count(&status_tx, recovery_count);

                info!(
                    attempt = consecutive_failures,
                    "Attempting USB reset recovery"
                );
                match recovery::usb_reset_facecam() {
                    Ok(reset) => {
                        info!(sysfs = %reset.sysfs_path.display(), "USB reset successful");
                        // Wait for device to stabilize
                        std::thread::sleep(Duration::from_secs(2));
                        // Reset consecutive failures on successful recovery
                        consecutive_failures = 0;
                    }
                    Err(reset_err) => {
                        error!(error = %reset_err, "USB reset failed");
                        // Wait before retrying
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        }
    }
}

/// Run a single pipeline lifecycle — returns on error or shutdown
fn run_pipeline_once(
    config: &PipelineConfig,
    status_tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<()> {
    // Phase 1: Detect device
    update_state(status_tx, PipelineState::Probing, HealthStatus::Degraded);
    let source_path = detect_source(&config.source_device)?;
    info!(source = %source_path, "Source device detected");
    update_source(status_tx, Some(source_path.clone()));

    // Stopgap until Task 5 plumbs (path, product, fw) through detect_source.
    let (product, firmware) = usb::enumerate_elgato_devices()?
        .into_iter()
        .find(|d| d.product.is_facecam_original())
        .map(|d| (d.product, d.firmware))
        .unwrap_or((
            facecam_common::device::ElgatoProduct::Facecam,
            facecam_common::device::FirmwareVersion { major: 0, minor: 0 },
        ));

    // Phase 2: Probe and configure
    let source_file = v4l2::open_device(&source_path).context("Failed to open source device")?;
    let source_fd = source_file.as_raw_fd();

    let caps =
        v4l2::query_capabilities(source_fd).context("Failed to query source capabilities")?;
    info!(driver = %caps.driver, card = %caps.card, "Source device capabilities");

    if !caps.has_capture {
        bail!("Source device does not support video capture");
    }

    // Enumerate available modes and pick the best one
    let modes = v4l2::enumerate_all_modes(source_fd)?;
    // Filter out modes whose format is in the quirk DB as known-broken
    // for the detected (product, firmware). Modes not in the DB pass through —
    // the daemon does not speculate about untested formats.
    let reliable_modes: Vec<_> = modes
        .iter()
        .filter(|m| !quirks::is_format_known_broken(product, firmware, m.format))
        .collect();

    if reliable_modes.is_empty() {
        bail!("No reliable video modes available on source device");
    }

    // Load profile to determine preferred mode
    let profile = profiles::load_profile(&config.profile_name).unwrap_or_else(|_| {
        warn!(profile = %config.profile_name, "Failed to load profile, using defaults");
        profiles::load_profile("default").unwrap_or_else(|_| facecam_common::profiles::Profile {
            name: "fallback".to_string(),
            description: "Auto-generated fallback".to_string(),
            video_mode: None,
            controls: Default::default(),
        })
    });

    // Select video mode
    let target_mode = if let Some(ref pvm) = profile.video_mode {
        // Find matching mode from reliable set
        reliable_modes
            .iter()
            .find(|m| {
                m.width == pvm.width && m.height == pvm.height && m.fps() >= pvm.fps as f64 - 1.0
            })
            .copied()
            .or_else(|| reliable_modes.first().copied())
            .ok_or_else(|| anyhow::anyhow!("No matching video mode found"))?
    } else {
        reliable_modes
            .first()
            .ok_or_else(|| anyhow::anyhow!("No reliable modes"))?
    };

    info!(mode = %target_mode, "Selected video mode");

    // Set format on source
    v4l2::set_format(
        source_fd,
        target_mode.width,
        target_mode.height,
        target_mode.format.to_fourcc(),
    )
    .context("Failed to set source format")?;

    // Apply control values from profile
    for (name, value) in &profile.controls {
        if let Some(ctrl_id) = v4l2::control_name_to_id(name) {
            match v4l2::set_control(source_fd, ctrl_id, *value as i32) {
                Ok(()) => debug!(control = %name, value, "Applied control"),
                Err(e) => warn!(control = %name, value, error = %e, "Failed to apply control"),
            }
        } else {
            debug!(control = %name, "Unknown control name, skipping");
        }
    }

    // Phase 3: Open sink (v4l2loopback)
    update_state(status_tx, PipelineState::Starting, HealthStatus::Degraded);
    let sink_file = v4l2::open_device(&config.sink_device)
        .context("Failed to open sink device. Is v4l2loopback loaded?")?;
    let sink_fd = sink_file.as_raw_fd();

    // Set output format on sink to match source
    v4l2::set_output_format(
        sink_fd,
        target_mode.width,
        target_mode.height,
        target_mode.format.to_fourcc(),
    )
    .context("Failed to set sink output format")?;

    info!(sink = %config.sink_device, "Sink device configured");

    // Phase 4: Start streaming
    update_state(status_tx, PipelineState::Streaming, HealthStatus::Healthy);
    update_mode(status_tx, Some(*target_mode));
    update_connected(status_tx, true);

    info!("Pipeline streaming");

    // Frame forwarding loop
    // Using read/write I/O (simpler than MMAP for the normalization use case)
    let frame_size = (target_mode.width * target_mode.height) as usize
        * target_mode.format.bytes_per_pixel().unwrap_or(2.0) as usize;
    let mut frame_buf = vec![0u8; frame_size];

    let mut frames_captured: u64 = 0;
    let mut frames_written: u64 = 0;
    let mut frames_dropped: u64 = 0;
    let mut last_stats = Instant::now();

    // Request MMAP buffers on source for capture
    // We use a simpler approach: try read() first, fall back to buffer-based streaming
    // The v4l2loopback sink supports write() directly

    loop {
        // Check for shutdown
        if shutdown_rx.try_recv().is_ok() {
            info!("Shutdown signal in frame loop");
            return Ok(());
        }

        // Read frame from source using raw read (if READWRITE cap is available)
        // Otherwise we'd need full MMAP buffer management
        if caps.has_readwrite {
            use std::io::Read;
            let mut source_reader = &source_file;
            match source_reader.read(&mut frame_buf) {
                Ok(n) if n > 0 => {
                    frames_captured += 1;

                    // Write frame to sink
                    use std::io::Write;
                    let mut sink_writer = &sink_file;
                    match sink_writer.write_all(&frame_buf[..n]) {
                        Ok(()) => {
                            frames_written += 1;
                        }
                        Err(e) => {
                            frames_dropped += 1;
                            if frames_dropped % 100 == 1 {
                                warn!(error = %e, dropped = frames_dropped, "Sink write failed");
                            }
                        }
                    }
                }
                Ok(_) => {
                    warn!("Zero-length read from source");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    // Check if it's a temporary error
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted
                    {
                        continue;
                    }
                    bail!("Source read error: {}", e);
                }
            }
        } else {
            // Streaming I/O path (MMAP)
            // For devices that don't support read(), we need buffer-based streaming.
            // This is more complex but necessary for most UVC cameras.
            bail!(
                "Device does not support read() I/O. \
                 MMAP streaming required (not yet implemented in this daemon version). \
                 Consider using ffmpeg as an interim bridge."
            );
        }

        // Periodic stats update
        if last_stats.elapsed() > Duration::from_secs(5) {
            let fps = frames_captured as f64 / last_stats.elapsed().as_secs_f64();
            debug!(
                frames_captured,
                frames_written,
                frames_dropped,
                fps = format!("{:.1}", fps),
                "Pipeline stats"
            );

            // Update shared status
            update_frame_counts(status_tx, frames_captured, frames_written, frames_dropped);
            last_stats = Instant::now();
        }
    }
}

/// Detect the source V4L2 device path
fn detect_source(explicit: &Option<String>) -> Result<String> {
    if let Some(dev) = explicit {
        if std::path::Path::new(dev).exists() {
            return Ok(dev.clone());
        }
        bail!("Specified source device {} does not exist", dev);
    }

    // Auto-detect via USB enumeration
    let devices = usb::enumerate_elgato_devices()?;
    for dev in &devices {
        if !dev.product.is_facecam_original() {
            continue;
        }
        if let Ok(Some(sysfs)) = usb::find_usb_sysfs_path(dev.usb_bus, dev.usb_address) {
            if let Ok(Some(v4l2_dev)) = usb::find_v4l2_device_for_usb(&sysfs) {
                return Ok(v4l2_dev);
            }
        }
    }

    // Try udev symlink
    let symlink = "/dev/video-facecam";
    if std::path::Path::new(symlink).exists() {
        return Ok(symlink.to_string());
    }

    bail!("Could not detect Facecam. Is it connected?")
}

// Status update helpers — these acquire the mutex briefly to update specific fields
fn update_state(
    tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>,
    state: PipelineState,
    health: HealthStatus,
) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| {
            s.state = state;
            s.health = health;
        });
    }
}

fn update_error(tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>, error: Option<String>) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| s.last_error = error);
    }
}

fn update_source(tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>, source: Option<String>) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| s.source_device = source);
    }
}

fn update_connected(tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>, connected: bool) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| s.device_connected = connected);
    }
}

fn update_mode(
    tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>,
    mode: Option<facecam_common::formats::VideoMode>,
) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| s.active_mode = mode);
    }
}

fn update_recovery_count(tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>, count: u32) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| s.recovery_count = count);
    }
}

fn update_frame_counts(
    tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>,
    captured: u64,
    written: u64,
    dropped: u64,
) {
    if let Ok(guard) = tx.try_lock() {
        guard.send_modify(|s| {
            s.frames_captured = captured;
            s.frames_written = written;
            s.frames_dropped = dropped;
        });
    }
}
