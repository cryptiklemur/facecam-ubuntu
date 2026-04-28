use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::{watch, Mutex};
use tracing::info;

use facecam_common::types::{DaemonStatus, HealthStatus, PipelineState};
use facecam_daemon::{config, ipc_server, pipeline, watchdog};

#[derive(Parser)]
#[command(name = "facecam-daemon")]
#[command(
    about = "Facecam normalization daemon — captures from physical device, outputs to v4l2loopback"
)]
struct Cli {
    /// Path to daemon config file
    #[arg(long)]
    config: Option<String>,

    /// Source V4L2 device (auto-detected if omitted)
    #[arg(long)]
    source: Option<String>,

    /// Sink v4l2loopback device (default: /dev/video10)
    #[arg(long, default_value = "/dev/video10")]
    sink: String,

    /// Profile to apply on startup
    #[arg(long, default_value = "default")]
    profile: String,

    /// Run in foreground (don't daemonize)
    #[arg(long)]
    foreground: bool,

    /// Log format (text or json)
    #[arg(long, default_value = "json")]
    log_format: LogFormat,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    match cli.log_format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();
        }
        LogFormat::Text => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_target(false)
                .init();
        }
    }

    info!(
        version = env!("CARGO_PKG_VERSION"),
        source = ?cli.source,
        sink = %cli.sink,
        profile = %cli.profile,
        "Facecam daemon starting"
    );

    // Load configuration
    let daemon_config = config::load_config(cli.config.as_deref())?;

    // Create shared state
    let initial_status = DaemonStatus {
        state: PipelineState::Idle,
        health: HealthStatus::Disconnected,
        uptime_secs: 0,
        device_connected: false,
        active_mode: None,
        frames_captured: 0,
        frames_written: 0,
        frames_dropped: 0,
        recovery_count: 0,
        last_error: None,
        source_device: cli.source.clone(),
        sink_device: Some(cli.sink.clone()),
        active_profile: Some(cli.profile.clone()),
    };

    let (status_tx, status_rx) = watch::channel(initial_status);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    let status_tx = Arc::new(Mutex::new(status_tx));
    let start_time = std::time::Instant::now();

    // Create default profiles if needed
    facecam_common::profiles::create_default_profiles()?;

    // Spawn IPC server
    let ipc_status_rx = status_rx.clone();
    let ipc_shutdown_tx = shutdown_tx.clone();
    let ipc_status_tx = status_tx.clone();
    let ipc_handle = tokio::spawn(async move {
        if let Err(e) = ipc_server::run(ipc_status_rx, ipc_status_tx, ipc_shutdown_tx).await {
            tracing::error!(error = %e, "IPC server error");
        }
    });

    // Spawn watchdog
    let wd_status_tx = status_tx.clone();
    let wd_shutdown = shutdown_tx.subscribe();
    let watchdog_handle = tokio::spawn(async move {
        watchdog::run(wd_status_tx, wd_shutdown, start_time).await;
    });

    // Run the main pipeline loop
    let pipeline_shutdown = shutdown_tx.subscribe();
    let pipeline_config = pipeline::PipelineConfig {
        source_device: cli.source,
        sink_device: cli.sink,
        profile_name: cli.profile,
        max_recovery_attempts: daemon_config.max_recovery_attempts,
        frame_timeout_ms: daemon_config.frame_timeout_ms,
    };

    let pipeline_status_tx = status_tx.clone();

    // Pipeline runs in a blocking thread since V4L2 I/O is synchronous
    let pipeline_handle = tokio::task::spawn_blocking(move || {
        pipeline::run(pipeline_config, pipeline_status_tx, pipeline_shutdown)
    });

    // Wait for shutdown signal
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down");
        }
        _ = shutdown_rx.recv() => {
            info!("Shutdown signal received");
        }
    }

    let _ = shutdown_tx.send(());

    // Wait for tasks to complete
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let _ = ipc_handle.await;
        let _ = watchdog_handle.await;
        let _ = pipeline_handle.await;
    })
    .await;

    info!("Daemon shutdown complete");
    Ok(())
}
