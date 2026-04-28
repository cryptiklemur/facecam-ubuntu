use anyhow::Result;
use facecam_common::{
    diagnostics,
    ipc::{self, DaemonCommand, DaemonResponse},
    profiles,
    types::DaemonStatus,
    v4l2,
};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{debug, error, info, warn};

/// Run the IPC server on a Unix domain socket
pub async fn run(
    status_rx: watch::Receiver<DaemonStatus>,
    status_tx: Arc<Mutex<watch::Sender<DaemonStatus>>>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    let socket_path = ipc::socket_path();

    // Remove stale socket
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!(path = %socket_path.display(), "IPC server listening");

    // Set permissions so non-root users in video group can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;
    }

    let mut shutdown_rx = shutdown_tx.subscribe();

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let status_rx = status_rx.clone();
                        let status_tx = status_tx.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, status_rx, status_tx, shutdown_tx).await {
                                debug!(error = %e, "Client handler error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "Accept error");
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                info!("IPC server shutting down");
                break;
            }
        }
    }

    // Cleanup socket
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    status_rx: watch::Receiver<DaemonStatus>,
    status_tx: Arc<Mutex<watch::Sender<DaemonStatus>>>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read one command per line (JSON-encoded)
    while reader.read_line(&mut line).await? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let command: DaemonCommand = match serde_json::from_str(trimmed) {
            Ok(cmd) => cmd,
            Err(e) => {
                let response = DaemonResponse::Error(format!("Invalid command: {}", e));
                let resp_json = serde_json::to_string(&response)? + "\n";
                writer.write_all(resp_json.as_bytes()).await?;
                line.clear();
                continue;
            }
        };

        let response = handle_command(command, &status_rx, &status_tx, &shutdown_tx).await;
        let resp_json = serde_json::to_string(&response)? + "\n";
        writer.write_all(resp_json.as_bytes()).await?;
        line.clear();
    }

    Ok(())
}

async fn handle_command(
    command: DaemonCommand,
    status_rx: &watch::Receiver<DaemonStatus>,
    status_tx: &Arc<Mutex<watch::Sender<DaemonStatus>>>,
    shutdown_tx: &broadcast::Sender<()>,
) -> DaemonResponse {
    match command {
        DaemonCommand::Status => {
            let status = status_rx.borrow().clone();
            DaemonResponse::Status(status)
        }

        DaemonCommand::ApplyProfile { name } => {
            let family = match status_rx.borrow().product_family.clone() {
                Some(f) => f,
                None => {
                    warn!(
                        "ApplyProfile called before product family was detected; falling back to 'facecam'"
                    );
                    "facecam".to_string()
                }
            };
            match profiles::load_profile_for_family(&name, &family) {
                Ok(profile) => {
                    // Apply controls if we have a source device
                    let status = status_rx.borrow().clone();
                    if let Some(ref dev_path) = status.source_device {
                        match v4l2::open_device(dev_path) {
                            Ok(file) => {
                                let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
                                let mut applied = 0;
                                for (name, value) in &profile.controls {
                                    if let Some(ctrl_id) = v4l2::control_name_to_id(name) {
                                        if v4l2::set_control(fd, ctrl_id, *value as i32).is_ok() {
                                            applied += 1;
                                        }
                                    }
                                }
                                // Update active profile in status
                                if let Ok(guard) = status_tx.try_lock() {
                                    guard.send_modify(|s| {
                                        s.active_profile = Some(name.clone());
                                    });
                                }
                                DaemonResponse::Ok(Some(format!(
                                    "Profile '{}' applied ({} controls set)",
                                    name, applied
                                )))
                            }
                            Err(e) => {
                                DaemonResponse::Error(format!("Failed to open device: {}", e))
                            }
                        }
                    } else {
                        DaemonResponse::Error("No source device connected".to_string())
                    }
                }
                Err(e) => {
                    DaemonResponse::Error(format!("Failed to load profile '{}': {}", name, e))
                }
            }
        }

        DaemonCommand::SetControl { name, value } => {
            let status = status_rx.borrow().clone();
            if let Some(ref dev_path) = status.source_device {
                match v4l2::open_device(dev_path) {
                    Ok(file) => {
                        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
                        if let Some(ctrl_id) = v4l2::control_name_to_id(&name) {
                            match v4l2::set_control(fd, ctrl_id, value as i32) {
                                Ok(()) => DaemonResponse::Ok(Some(format!("{}={}", name, value))),
                                Err(e) => {
                                    DaemonResponse::Error(format!("Failed to set {}: {}", name, e))
                                }
                            }
                        } else {
                            DaemonResponse::Error(format!("Unknown control: {}", name))
                        }
                    }
                    Err(e) => DaemonResponse::Error(format!("Failed to open device: {}", e)),
                }
            } else {
                DaemonResponse::Error("No source device connected".to_string())
            }
        }

        DaemonCommand::GetControl { name } => {
            let status = status_rx.borrow().clone();
            if let Some(ref dev_path) = status.source_device {
                match v4l2::open_device(dev_path) {
                    Ok(file) => {
                        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
                        if let Some(ctrl_id) = v4l2::control_name_to_id(&name) {
                            match v4l2::get_control(fd, ctrl_id) {
                                Ok(value) => DaemonResponse::ControlValue { name, value },
                                Err(e) => {
                                    DaemonResponse::Error(format!("Failed to get {}: {}", name, e))
                                }
                            }
                        } else {
                            DaemonResponse::Error(format!("Unknown control: {}", name))
                        }
                    }
                    Err(e) => DaemonResponse::Error(format!("Failed to open device: {}", e)),
                }
            } else {
                DaemonResponse::Error("No source device connected".to_string())
            }
        }

        DaemonCommand::GetAllControls => {
            let status = status_rx.borrow().clone();
            if let Some(ref dev_path) = status.source_device {
                match v4l2::open_device(dev_path) {
                    Ok(file) => {
                        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
                        match v4l2::enumerate_controls(fd) {
                            Ok(controls) => DaemonResponse::Controls(controls),
                            Err(e) => DaemonResponse::Error(format!(
                                "Failed to enumerate controls: {}",
                                e
                            )),
                        }
                    }
                    Err(e) => DaemonResponse::Error(format!("Failed to open device: {}", e)),
                }
            } else {
                DaemonResponse::Error("No source device connected".to_string())
            }
        }

        DaemonCommand::ExportDiagnostics => {
            let status = status_rx.borrow().clone();

            // Collect device info
            let device = facecam_common::usb::enumerate_elgato_devices()
                .ok()
                .and_then(|devs| devs.into_iter().next());

            let bundle = diagnostics::create_bundle(device, Some(status), Vec::new(), Vec::new());

            match diagnostics::export_bundle(&bundle) {
                Ok(path) => DaemonResponse::DiagnosticsExported(path.to_string_lossy().to_string()),
                Err(e) => DaemonResponse::Error(format!("Failed to export diagnostics: {}", e)),
            }
        }

        DaemonCommand::ForceReset => {
            info!("Force reset requested via IPC");
            let product = facecam_common::usb::detect_product_or_default();
            match facecam_common::recovery::usb_reset_product(product) {
                Ok(_) => DaemonResponse::Ok(Some("USB reset completed".to_string())),
                Err(e) => DaemonResponse::Error(format!("USB reset failed: {}", e)),
            }
        }

        DaemonCommand::RestartPipeline => {
            // Signal pipeline to restart by updating state
            // The pipeline loop will detect this and restart
            if let Ok(guard) = status_tx.try_lock() {
                guard.send_modify(|s| {
                    s.state = facecam_common::types::PipelineState::Recovering;
                });
            }
            DaemonResponse::Ok(Some("Pipeline restart requested".to_string()))
        }

        DaemonCommand::Shutdown => {
            info!("Shutdown requested via IPC");
            let _ = shutdown_tx.send(());
            DaemonResponse::Ok(Some("Shutting down".to_string()))
        }
    }
}
