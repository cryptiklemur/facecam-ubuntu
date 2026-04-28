use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use facecam_common::ipc::{self, DaemonCommand, DaemonResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "facecam-ctl")]
#[command(about = "Control the Facecam daemon — manage profiles, controls, and diagnostics")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Show daemon status
    Status,

    /// Manage camera controls
    Control {
        #[command(subcommand)]
        action: ControlAction,
    },

    /// Manage camera profiles
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },

    /// Export diagnostics bundle
    Diagnostics,

    /// Force USB reset and device recovery
    Reset,

    /// Restart the capture pipeline
    Restart,

    /// Shut down the daemon
    Shutdown,
}

#[derive(Subcommand)]
enum ControlAction {
    /// Get a control value
    Get {
        /// Control name (e.g., brightness, contrast, saturation)
        name: String,
    },
    /// Set a control value
    Set {
        /// Control name
        name: String,
        /// Value to set
        value: i64,
    },
    /// List all controls and their current values
    List,
}

#[derive(Subcommand)]
enum ProfileAction {
    /// Apply a named profile
    Apply {
        /// Profile name
        name: String,
    },
    /// List available profiles
    List,
    /// Show profile details
    Show {
        /// Profile name
        name: String,
    },
    /// Create default profiles
    Init,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Status => {
            let resp = send_command(DaemonCommand::Status).await?;
            match resp {
                DaemonResponse::Status(status) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&status)?);
                    } else {
                        println!("=== Facecam Daemon Status ===\n");
                        println!("  State:          {}", status.state);
                        if let Some(ref p) = status.product {
                            println!("  Product:        {}", p);
                        }
                        println!("  Health:         {}", status.health);
                        println!("  Uptime:         {}s", status.uptime_secs);
                        println!("  Connected:      {}", status.device_connected);
                        println!(
                            "  Mode:           {}",
                            status
                                .active_mode
                                .map(|m| m.to_string())
                                .unwrap_or_else(|| "none".to_string())
                        );
                        println!(
                            "  Frames:         captured={} written={} dropped={}",
                            status.frames_captured, status.frames_written, status.frames_dropped
                        );
                        println!("  Recoveries:     {}", status.recovery_count);
                        println!(
                            "  Source:         {}",
                            status.source_device.as_deref().unwrap_or("none")
                        );
                        println!(
                            "  Sink:           {}",
                            status.sink_device.as_deref().unwrap_or("none")
                        );
                        println!(
                            "  Profile:        {}",
                            status.active_profile.as_deref().unwrap_or("none")
                        );
                        if let Some(ref err) = status.last_error {
                            println!("  Last Error:     {}", err);
                        }
                    }
                }
                DaemonResponse::Error(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::Control { action } => match action {
            ControlAction::Get { name } => {
                let resp = send_command(DaemonCommand::GetControl { name: name.clone() }).await?;
                match resp {
                    DaemonResponse::ControlValue { name, value } => {
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(
                                    &serde_json::json!({"name": name, "value": value})
                                )?
                            );
                        } else {
                            println!("{} = {}", name, value);
                        }
                    }
                    DaemonResponse::Error(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                    _ => {
                        eprintln!("Unexpected response");
                        std::process::exit(1);
                    }
                }
            }
            ControlAction::Set { name, value } => {
                let resp = send_command(DaemonCommand::SetControl { name, value }).await?;
                print_simple_response(resp, cli.json)?;
            }
            ControlAction::List => {
                let resp = send_command(DaemonCommand::GetAllControls).await?;
                match resp {
                    DaemonResponse::Controls(controls) => {
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&controls)?);
                        } else {
                            println!("=== Camera Controls ===\n");
                            for ctrl in &controls {
                                println!(
                                    "  {:30} = {:6}  (range [{}, {}], default {})",
                                    ctrl.name, ctrl.value, ctrl.minimum, ctrl.maximum, ctrl.default
                                );
                            }
                        }
                    }
                    DaemonResponse::Error(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                    _ => {
                        eprintln!("Unexpected response");
                        std::process::exit(1);
                    }
                }
            }
        },

        Commands::Profile { action } => match action {
            ProfileAction::Apply { name } => {
                let resp = send_command(DaemonCommand::ApplyProfile { name }).await?;
                print_simple_response(resp, cli.json)?;
            }
            ProfileAction::List => {
                let profiles = facecam_common::profiles::list_profiles()?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&profiles)?);
                } else {
                    println!("=== Available Profiles ===\n");
                    if profiles.is_empty() {
                        println!("  (none — run 'facecam-ctl profile init' to create defaults)");
                    } else {
                        let family = family_from_daemon().await;
                        for name in &profiles {
                            if let Ok(p) =
                                facecam_common::profiles::load_profile_for_family(name, &family)
                            {
                                println!("  {:15} — {}", name, p.description);
                            } else {
                                println!("  {}", name);
                            }
                        }
                    }
                }
            }
            ProfileAction::Show { name } => {
                let family = family_from_daemon().await;
                let profile = facecam_common::profiles::load_profile_for_family(&name, &family)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&profile)?);
                } else {
                    println!("=== Profile: {} ===\n", profile.name);
                    println!("  Description: {}", profile.description);
                    if let Some(ref vm) = profile.video_mode {
                        println!(
                            "  Video Mode:  {}x{} @ {} fps ({})",
                            vm.width, vm.height, vm.fps, vm.format
                        );
                    }
                    println!("  Controls:");
                    for (name, value) in &profile.controls {
                        println!("    {:30} = {}", name, value);
                    }
                }
            }
            ProfileAction::Init => {
                facecam_common::profiles::create_default_profiles()?;
                let dir = facecam_common::profiles::profiles_dir();
                println!("Default profiles created in {}", dir.display());
            }
        },

        Commands::Diagnostics => {
            let resp = send_command(DaemonCommand::ExportDiagnostics).await?;
            match resp {
                DaemonResponse::DiagnosticsExported(path) => {
                    println!("Diagnostics exported to: {}", path);
                }
                DaemonResponse::Error(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::Reset => {
            let resp = send_command(DaemonCommand::ForceReset).await?;
            print_simple_response(resp, cli.json)?;
        }

        Commands::Restart => {
            let resp = send_command(DaemonCommand::RestartPipeline).await?;
            print_simple_response(resp, cli.json)?;
        }

        Commands::Shutdown => {
            let resp = send_command(DaemonCommand::Shutdown).await?;
            print_simple_response(resp, cli.json)?;
        }
    }

    Ok(())
}

/// Ask the running daemon which product family it has resolved. Falls back to
/// `facecam` when the daemon is unreachable or has not yet detected a device,
/// preserving original-Facecam behavior on machines without a Pro.
async fn family_from_daemon() -> String {
    match send_command(DaemonCommand::Status).await {
        Ok(DaemonResponse::Status(s)) => s.product_family.unwrap_or_else(|| "facecam".to_string()),
        _ => "facecam".to_string(),
    }
}

/// Send a command to the daemon via Unix socket and read the response
async fn send_command(command: DaemonCommand) -> Result<DaemonResponse> {
    let socket_path = ipc::socket_path();

    let stream = UnixStream::connect(&socket_path).await.with_context(|| {
        format!(
            "Failed to connect to daemon at {}. Is facecam-daemon running?",
            socket_path.display()
        )
    })?;

    let (reader, mut writer) = stream.into_split();

    // Send command as JSON line
    let cmd_json = serde_json::to_string(&command)? + "\n";
    writer.write_all(cmd_json.as_bytes()).await?;
    writer.shutdown().await?;

    // Read response
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: DaemonResponse =
        serde_json::from_str(line.trim()).context("Failed to parse daemon response")?;

    Ok(response)
}

fn print_simple_response(resp: DaemonResponse, json: bool) -> Result<()> {
    match resp {
        DaemonResponse::Ok(msg) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": true,
                        "message": msg,
                    }))?
                );
            } else {
                if let Some(msg) = msg {
                    println!("{}", msg);
                } else {
                    println!("OK");
                }
            }
        }
        DaemonResponse::Error(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": false,
                        "error": e,
                    }))?
                );
            } else {
                eprintln!("Error: {}", e);
            }
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response");
            std::process::exit(1);
        }
    }
    Ok(())
}
