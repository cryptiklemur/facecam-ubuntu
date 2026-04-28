use crate::capture::CaptureSession;
use anyhow::{bail, Context, Result};
use facecam_common::{
    device::{ElgatoProduct, FirmwareVersion, ProductDescriptor},
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

pub fn run(
    config: PipelineConfig,
    status_tx: Arc<Mutex<watch::Sender<DaemonStatus>>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let product = match detect_source(&config.source_device) {
        Ok((_, p, _)) => p,
        Err(e) => {
            error!(error = %e, "Initial source detection failed; entering Failed state");
            update_state(&status_tx, PipelineState::Failed, HealthStatus::Unhealthy);
            update_error(&status_tx, Some(e.to_string()));
            return;
        }
    };

    let mut recovery_count: u32 = 0;
    let mut consecutive_failures: u32 = 0;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            info!("Pipeline received shutdown signal");
            update_state(
                &status_tx,
                PipelineState::ShuttingDown,
                HealthStatus::Healthy,
            );
            return;
        }

        match run_pipeline_once(&config, &status_tx, &mut shutdown_rx) {
            Ok(()) => {
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
                    loop {
                        if shutdown_rx.try_recv().is_ok() {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }

                update_state(
                    &status_tx,
                    PipelineState::Recovering,
                    HealthStatus::Degraded,
                );
                recovery_count += 1;
                update_recovery_count(&status_tx, recovery_count);

                if consecutive_failures == 1 {
                    info!("Rung 2 recovery: reopening pipeline (no USB reset)");
                    // Brief settle before reopen — the source fd was just dropped, give the
                    // kernel a moment to release v4l2 state before we re-probe.
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }

                info!(attempt = consecutive_failures, "Rung 3 recovery: USB reset");
                // Re-detect on every rung-3 cycle so a hot-plugged different product gets
                // the right reset. Fall back to the startup product if re-detection fails
                // (device unplugged mid-recovery is the obvious case).
                let reset_product = detect_source(&config.source_device)
                    .map(|(_, p, _)| p)
                    .unwrap_or_else(|e| {
                        warn!(error = %e, fallback = %product, "Re-detect for reset failed; using startup product");
                        product
                    });
                match recovery::usb_reset_product(reset_product) {
                    Ok(reset) => {
                        info!(
                            sysfs = %reset.sysfs_path.display(),
                            warnings = reset.warnings.len(),
                            "USB reset successful"
                        );
                        for w in &reset.warnings {
                            warn!(warning = %w, "USB reset warning");
                        }
                        // USB reset triggers re-enumeration. The kernel needs ~1-2s to
                        // re-bind the UVC driver and (re)create /dev/videoN.
                        std::thread::sleep(Duration::from_secs(2));
                        consecutive_failures = 0;
                    }
                    Err(reset_err) => {
                        error!(error = %reset_err, "USB reset failed");
                        // Back off longer on reset failure to avoid hammering sysfs while
                        // udev or the bus is still settling from a prior reset attempt.
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        }
    }
}

fn run_pipeline_once(
    config: &PipelineConfig,
    status_tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<()> {
    update_state(status_tx, PipelineState::Probing, HealthStatus::Degraded);
    let (source_path, product, firmware) = detect_source(&config.source_device)?;
    info!(source = %source_path, product = %product, firmware = %firmware, "Source device detected");
    update_source(status_tx, Some(source_path.clone()));

    let source_file = v4l2::open_device_nonblocking(&source_path)
        .with_context(|| format!("Failed to open {} ({})", product, source_path))?;
    let source_fd = source_file.as_raw_fd();

    let caps = v4l2::query_capabilities(source_fd)
        .with_context(|| format!("Failed to query capabilities of {}", product))?;
    if !caps.has_capture {
        bail!("{} does not advertise VIDEO_CAPTURE", product);
    }
    if !caps.has_streaming {
        bail!(
            "{} does not advertise STREAMING (MMAP unsupported)",
            product
        );
    }

    let modes = v4l2::enumerate_all_modes(source_fd)?;
    let reliable_modes: Vec<_> = modes
        .iter()
        .filter(|m| !quirks::is_format_known_broken(product, firmware, m.format))
        .collect();
    if reliable_modes.is_empty() {
        bail!(
            "No usable video modes for {} (all formats marked broken in quirk DB)",
            product
        );
    }

    let profile = profiles::load_profile(&config.profile_name).unwrap_or_else(|_| {
        warn!(profile = %config.profile_name, "Failed to load profile, using built-in default");
        profiles::Profile {
            name: "fallback".into(),
            description: "Auto-generated fallback".into(),
            video_mode: None,
            controls: Default::default(),
        }
    });

    let target_mode = if let Some(ref pvm) = profile.video_mode {
        let want_format = facecam_common::formats::PixelFormat::from_fourcc(u32::from_le_bytes(
            pvm.format.as_bytes().try_into().unwrap_or(*b"MJPG"),
        ));
        reliable_modes
            .iter()
            .find(|m| {
                m.width == pvm.width
                    && m.height == pvm.height
                    && m.format == want_format
                    && m.fps() >= pvm.fps as f64 - 1.0
            })
            .copied()
            .or_else(|| {
                reliable_modes
                    .iter()
                    .find(|m| m.format == want_format)
                    .copied()
            })
            .or_else(|| reliable_modes.first().copied())
            .ok_or_else(|| anyhow::anyhow!("No matching video mode found"))?
    } else {
        reliable_modes
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("No reliable modes"))?
    };

    info!(mode = %target_mode, "Selected video mode");
    v4l2::set_format(
        source_fd,
        target_mode.width,
        target_mode.height,
        target_mode.format.to_fourcc(),
    )
    .context("Failed to set source format")?;

    for (name, value) in &profile.controls {
        let actual_name = product.control_alias(name).unwrap_or(name);
        if let Some(ctrl_id) = v4l2::control_name_to_id(actual_name) {
            match v4l2::set_control(source_fd, ctrl_id, *value as i32) {
                Ok(()) => debug!(control = %actual_name, value, "Applied control"),
                Err(e) => {
                    warn!(control = %actual_name, value, error = %e, "Failed to apply control")
                }
            }
        } else {
            debug!(control = %actual_name, "Unknown control name, skipping");
        }
    }

    update_state(status_tx, PipelineState::Starting, HealthStatus::Degraded);
    let sink_file = v4l2::open_device(&config.sink_device)
        .context("Failed to open sink device. Is v4l2loopback loaded?")?;
    let sink_fd = sink_file.as_raw_fd();
    v4l2::set_output_format(
        sink_fd,
        target_mode.width,
        target_mode.height,
        target_mode.format.to_fourcc(),
    )
    .context("Failed to set sink output format")?;

    update_state(status_tx, PipelineState::Streaming, HealthStatus::Healthy);
    update_mode(status_tx, Some(*target_mode));
    update_connected(status_tx, true);

    let mut session = CaptureSession::start(source_fd, *target_mode)?;
    let frame_timeout = Duration::from_millis(config.frame_timeout_ms);

    let mut frames_acquired: u64 = 0;
    let mut frames_written: u64 = 0;
    let mut frames_dropped: u64 = 0;
    let mut rung1_attempts: u32 = 0;
    let mut last_stats = Instant::now();

    loop {
        if shutdown_rx.try_recv().is_ok() {
            info!("Shutdown signal in capture loop");
            session.stop()?;
            return Ok(());
        }

        // Acquire frame, then immediately drop FrameRef to release the session borrow
        // before any recovery path that needs to mutate session.
        let frame_result: Result<Vec<u8>, anyhow::Error> =
            session.next_frame(frame_timeout).map(|f| f.bytes.to_vec());

        match frame_result {
            Ok(bytes) => {
                use std::io::Write;
                rung1_attempts = 0;
                frames_acquired += 1;
                let mut sink_writer = &sink_file;
                match sink_writer.write_all(&bytes) {
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
            Err(e) => {
                let mut rung1_streamon_err: Option<anyhow::Error> = None;
                let mut rung1_session_err: Option<anyhow::Error> = None;
                if rung1_attempts == 0 {
                    debug!(error = %e, "Rung 1 recovery: stream_off_then_on");
                    rung1_attempts += 1;
                    let _ = session.stop();
                    if let Err(start_err) = recovery::stream_off_then_on(source_fd) {
                        debug!(error = %start_err, "Rung 1 STREAMON failed; falling through to rung 2");
                        rung1_streamon_err = Some(start_err);
                    }
                    match CaptureSession::start(source_fd, *target_mode) {
                        Ok(s) => {
                            session = s;
                            continue;
                        }
                        Err(start_err) => {
                            debug!(error = %start_err, "CaptureSession::start after rung 1 failed; rung 2");
                            rung1_session_err = Some(start_err);
                        }
                    }
                }
                let _ = session.stop();
                let detail = match (rung1_streamon_err, rung1_session_err) {
                    (Some(s), Some(r)) => {
                        format!(
                            "{} (after rung 1 STREAMON: {}; session restart: {})",
                            e, s, r
                        )
                    }
                    (Some(s), None) => format!("{} (after rung 1 STREAMON: {})", e, s),
                    (None, Some(r)) => format!("{} (after rung 1 session restart: {})", e, r),
                    (None, None) => format!("{}", e),
                };
                bail!(
                    "Capture failed twice in succession; reopening pipeline: {}",
                    detail
                );
            }
        }

        if last_stats.elapsed() > Duration::from_secs(5) {
            let fps = frames_acquired as f64 / last_stats.elapsed().as_secs_f64();
            debug!(
                frames_acquired,
                frames_written,
                frames_dropped,
                fps = format!("{:.1}", fps),
                "Pipeline stats"
            );
            update_frame_counts(status_tx, frames_acquired, frames_written, frames_dropped);
            last_stats = Instant::now();
        }
    }
}

fn detect_source(explicit: &Option<String>) -> Result<(String, ElgatoProduct, FirmwareVersion)> {
    if let Some(dev) = explicit {
        if std::path::Path::new(dev).exists() {
            for cam in usb::enumerate_uvc_capture_devices()? {
                if cam.v4l2_device.as_deref() == Some(dev.as_str()) {
                    return Ok((dev.clone(), cam.product, cam.firmware));
                }
            }
            return Ok((
                dev.clone(),
                ElgatoProduct::Facecam,
                FirmwareVersion { major: 0, minor: 0 },
            ));
        }
        bail!("Specified source device {} does not exist", dev);
    }

    let cams = usb::enumerate_uvc_capture_devices()?;
    if let Some(cam) = cams.into_iter().find(|c| c.product.is_uvc_capture()) {
        if let Some(dev) = cam.v4l2_device {
            return Ok((dev, cam.product, cam.firmware));
        }
    }

    let symlink = "/dev/video-facecam";
    if std::path::Path::new(symlink).exists() {
        return Ok((
            symlink.to_string(),
            ElgatoProduct::Facecam,
            FirmwareVersion { major: 0, minor: 0 },
        ));
    }

    bail!("No Elgato UVC capture device found. Use --device /dev/videoN to override.")
}

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
