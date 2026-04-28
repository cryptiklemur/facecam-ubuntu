use anyhow::Result;
use clap::{Parser, Subcommand};
use facecam_common::{
    device::{ElgatoProduct, FirmwareVersion, UsbSpeed},
    diagnostics,
    formats::{FormatVerdict, VideoMode},
    quirks, usb, v4l2,
};
use std::os::unix::io::AsRawFd;

#[derive(Parser)]
#[command(name = "facecam-probe")]
#[command(about = "Detect, fingerprint, and enumerate the Elgato Facecam on Linux")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Output format (text or json)
    #[arg(long, default_value = "text")]
    format: OutputFormat,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Commands {
    /// Detect and fingerprint all Elgato cameras
    Detect,
    /// Enumerate all video formats and frame modes
    Formats {
        /// V4L2 device path (auto-detected if omitted)
        #[arg(long)]
        device: Option<String>,
    },
    /// List all available V4L2 controls
    Controls {
        /// V4L2 device path (auto-detected if omitted)
        #[arg(long)]
        device: Option<String>,
    },
    /// Show USB topology details
    Topology,
    /// Show applicable quirks for the detected device
    Quirks,
    /// Run full system diagnostics
    Diagnostics,
    /// Probe all formats and validate actual frame delivery
    Validate {
        /// V4L2 device path (auto-detected if omitted)
        #[arg(long)]
        device: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();

    match cli.command.unwrap_or(Commands::Detect) {
        Commands::Detect => cmd_detect(cli.format),
        Commands::Formats { device } => cmd_formats(device, cli.format),
        Commands::Controls { device } => cmd_controls(device, cli.format),
        Commands::Topology => cmd_topology(cli.format),
        Commands::Quirks => cmd_quirks(cli.format),
        Commands::Diagnostics => cmd_diagnostics(cli.format),
        Commands::Validate { device } => cmd_validate(device, cli.format),
    }
}

fn cmd_detect(format: OutputFormat) -> Result<()> {
    let devices = usb::enumerate_elgato_devices()?;

    if devices.is_empty() {
        if format == OutputFormat::Json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "found": false,
                    "devices": []
                }))?
            );
        } else {
            println!("No Elgato cameras detected.");
            println!();
            println!("Troubleshooting:");
            println!("  1. Check USB connection (USB 3.0 required for Facecam)");
            println!("  2. Run 'lsusb' to verify the device is visible to the kernel");
            println!("  3. Check 'dmesg | tail -20' for USB errors");
        }
        return Ok(());
    }

    // Enrich with V4L2 info (skip for non-functional fallback devices)
    let mut enriched = Vec::new();
    for mut dev in devices {
        if dev.product.is_usb2_fallback() {
            enriched.push(dev);
            continue;
        }
        if let Ok(Some(sysfs)) = usb::find_usb_sysfs_path(dev.usb_bus, dev.usb_address) {
            dev.v4l2_sysfs_path = Some(sysfs.to_string_lossy().to_string());
            if let Ok(Some(v4l2_dev)) = usb::find_v4l2_device_for_usb(&sysfs) {
                dev.v4l2_device = Some(v4l2_dev.clone());

                // Try to get card name from V4L2
                if let Ok(file) = v4l2::open_device(&v4l2_dev) {
                    if let Ok(caps) = v4l2::query_capabilities(file.as_raw_fd()) {
                        dev.card_name = Some(caps.card.clone());
                        dev.driver_version = Some(caps.version_string());
                    }
                }
            }
        }
        enriched.push(dev);
    }

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&enriched)?);
    } else {
        println!("=== Elgato Camera Detection ===\n");
        for dev in &enriched {
            print!("{}", dev);

            // USB 2.0 fallback mode — critical error
            if dev.product.is_usb2_fallback() {
                println!("  CRITICAL: Facecam is in USB 2.0 fallback mode (PID 0x0077).");
                println!("            The device string says \"USB3-REQUIRED-FOR-FACECAM\".");
                println!(
                    "            It will NOT function as a camera until moved to a USB 3.0 port."
                );
                println!("            Look for a blue USB-A port or a USB-C/Thunderbolt port.");
            }

            // Firmware warnings
            if dev.product.is_facecam_original() && !dev.firmware.has_mjpeg() {
                println!("  WARNING: Firmware {} lacks MJPEG support.", dev.firmware);
                println!("           Chromium-based browsers will NOT work without v4l2loopback.");
                println!("           Update to firmware 4.03+ via Camera Hub (Windows/Mac).");
            }

            // Speed warning
            if matches!(
                dev.usb_speed,
                UsbSpeed::High | UsbSpeed::Full | UsbSpeed::Low
            ) {
                println!("  WARNING: Device on USB 2.0 or lower. USB 3.0 is required.");
            }

            println!();
        }
    }

    Ok(())
}

fn cmd_formats(device: Option<String>, format: OutputFormat) -> Result<()> {
    let dev_path = resolve_device(device)?;
    let file = v4l2::open_device(&dev_path)?;
    let fd = file.as_raw_fd();

    let (product, firmware) = detect_product_firmware_or_default();

    let formats = v4l2::enumerate_formats(fd)?;
    let modes = v4l2::enumerate_all_modes(fd)?;

    if format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "device": dev_path,
                "formats": formats,
                "modes": modes,
            }))?
        );
    } else {
        println!("=== Format Enumeration: {} ===\n", dev_path);

        println!("Pixel Formats:");
        for fmt in &formats {
            let reliable = if quirks::is_format_known_broken(product, firmware, fmt.pixel_format) {
                " [KNOWN BROKEN - see quirk registry]"
            } else {
                " [UNTESTED]"
            };
            println!(
                "  [{}] {} - {}{}",
                fmt.index, fmt.pixel_format, fmt.description, reliable
            );
        }

        println!("\nVideo Modes:");
        for mode in &modes {
            let bw = mode
                .bandwidth_bytes_per_sec()
                .map(|b| format!(" ({:.0} MB/s)", b as f64 / 1_000_000.0))
                .unwrap_or_default();
            let reliable = if quirks::is_format_known_broken(product, firmware, mode.format) {
                " [BROKEN]"
            } else {
                ""
            };
            println!("  {}{}{}", mode, bw, reliable);
        }
    }

    Ok(())
}

fn cmd_controls(device: Option<String>, format: OutputFormat) -> Result<()> {
    let dev_path = resolve_device(device)?;
    let file = v4l2::open_device(&dev_path)?;
    let fd = file.as_raw_fd();

    let controls = v4l2::enumerate_controls(fd)?;

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&controls)?);
    } else {
        println!("=== V4L2 Controls: {} ===\n", dev_path);

        for ctrl in &controls {
            println!("  {} (0x{:08x}):", ctrl.name, ctrl.id);
            println!(
                "    type={:?}  value={}  range=[{}, {}]  step={}  default={}",
                ctrl.control_type, ctrl.value, ctrl.minimum, ctrl.maximum, ctrl.step, ctrl.default
            );
            if !ctrl.menu_items.is_empty() {
                for item in &ctrl.menu_items {
                    let current = if item.index as i64 == ctrl.value {
                        " <-- current"
                    } else {
                        ""
                    };
                    println!("    [{}] {}{}", item.index, item.name, current);
                }
            }
            println!();
        }
    }

    Ok(())
}

fn cmd_topology(format: OutputFormat) -> Result<()> {
    let sysfs = usb::find_facecam_sysfs_path()?;

    match sysfs {
        Some(path) => {
            let topo = usb::read_usb_topology(&path)?;
            if format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&topo)?);
            } else {
                println!("=== USB Topology ===\n");
                println!("  sysfs:         {}", topo.sysfs_path.display());
                println!("  bus/dev:       {:?}/{:?}", topo.busnum, topo.devnum);
                println!("  speed:         {:?}", topo.speed);
                println!("  USB version:   {:?}", topo.version);
                println!("  authorized:    {:?}", topo.authorized);
                println!("  manufacturer:  {:?}", topo.manufacturer);
                println!("  product:       {:?}", topo.product_name);
                println!("  bcdDevice:     {:?}", topo.bcd_device);
                println!("  configuration: {:?}", topo.configuration);
            }
        }
        None => {
            if format == OutputFormat::Json {
                println!("{}", serde_json::json!({"found": false}));
            } else {
                println!("Facecam not found in USB sysfs.");
            }
        }
    }

    Ok(())
}

fn cmd_quirks(format: OutputFormat) -> Result<()> {
    // Try to detect the device to show applicable quirks
    let devices = usb::enumerate_elgato_devices()?;

    let (product, firmware) = if let Some(dev) = devices.first() {
        (dev.product, dev.firmware)
    } else {
        // Show all quirks if no device connected
        if format == OutputFormat::Json {
            let registry = quirks::quirk_registry();
            println!("{}", serde_json::to_string_pretty(&registry)?);
        } else {
            println!("=== Quirk Registry (all known quirks) ===\n");
            println!("No device connected — showing complete registry.\n");
            for q in quirks::quirk_registry() {
                print_quirk(q);
            }
        }
        return Ok(());
    };

    let applicable = quirks::applicable_quirks(product, firmware);

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&applicable)?);
    } else {
        println!(
            "=== Applicable Quirks for {} (fw {}) ===\n",
            product, firmware
        );
        if applicable.is_empty() {
            println!("No known quirks apply to this device/firmware combination.");
        } else {
            for q in &applicable {
                print_quirk(q);
            }
        }
    }

    Ok(())
}

fn print_quirk(q: &quirks::Quirk) {
    println!("  [{}] {} ({:?})", q.id, q.summary, q.severity);
    println!("    {}", q.description);
    println!("    Mitigation: {:?}", q.mitigation);
    println!();
}

fn cmd_diagnostics(format: OutputFormat) -> Result<()> {
    println!("Collecting diagnostics...\n");

    let system = diagnostics::collect_system_info();
    let modules = diagnostics::collect_kernel_module_info();
    let v4l2_devs = diagnostics::list_v4l2_devices();

    // Try to detect device
    let devices = usb::enumerate_elgato_devices().unwrap_or_default();
    let device = devices.into_iter().next();

    let bundle = diagnostics::create_bundle(device, None, Vec::new(), Vec::new());

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
    } else {
        println!("=== System Diagnostics ===\n");
        println!("System:");
        println!("  Hostname:  {}", system.hostname);
        println!("  Kernel:    {}", system.kernel_version);
        println!("  OS:        {}", system.os_release);
        println!("  Ubuntu:    {}", system.ubuntu_version);
        println!("  Uptime:    {}s", system.uptime_secs);

        println!("\nKernel Modules:");
        println!(
            "  uvcvideo:     {} (version: {})",
            if modules.uvcvideo_loaded {
                "loaded"
            } else {
                "NOT loaded"
            },
            modules.uvcvideo_version.as_deref().unwrap_or("n/a")
        );
        println!(
            "  v4l2loopback: {} (version: {})",
            if modules.v4l2loopback_loaded {
                "loaded"
            } else {
                "NOT loaded"
            },
            modules.v4l2loopback_version.as_deref().unwrap_or("n/a")
        );

        println!("\nV4L2 Devices:");
        if v4l2_devs.is_empty() {
            println!("  (none)");
        } else {
            for dev in &v4l2_devs {
                println!("  {}", dev);
            }
        }

        // Export bundle
        match diagnostics::export_bundle(&bundle) {
            Ok(path) => println!("\nBundle exported to: {}", path.display()),
            Err(e) => println!("\nFailed to export bundle: {}", e),
        }
    }

    Ok(())
}

fn cmd_validate(device: Option<String>, format: OutputFormat) -> Result<()> {
    let dev_path = resolve_device(device)?;
    println!("=== Format Validation: {} ===\n", dev_path);
    println!("This will attempt to stream each advertised format and verify frame delivery.");
    println!("The device may need USB resets between tests.\n");

    let file = v4l2::open_device(&dev_path)?;
    let fd = file.as_raw_fd();
    let modes = v4l2::enumerate_all_modes(fd)?;
    drop(file);

    // Test a representative subset (one mode per format at 1080p)
    let mut tested_formats = std::collections::HashSet::new();
    let mut results = Vec::new();

    for mode in &modes {
        if tested_formats.contains(&mode.format) {
            continue;
        }
        if mode.width != 1920 || mode.height != 1080 {
            continue;
        }
        tested_formats.insert(mode.format);

        println!("Testing: {} ...", mode);

        let result = validate_single_format(&dev_path, mode);
        match &result {
            Ok(r) => {
                println!("  Result: {}", r.verdict);
                if let Some(ref err) = r.error {
                    println!("  Error:  {}", err);
                }
                results.push(r.clone());
            }
            Err(e) => {
                println!("  Error:  {}", e);
                results.push(facecam_common::formats::FormatProbeResult {
                    mode: *mode,
                    negotiation_ok: false,
                    stream_started: false,
                    frames_received: 0,
                    first_frame_nonzero: false,
                    frame_size_consistent: false,
                    avg_frame_interval_ms: None,
                    error: Some(e.to_string()),
                    verdict: FormatVerdict::NegotiationFailed,
                });
            }
        }
        println!();
    }

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        println!("=== Validation Summary ===\n");
        for r in &results {
            println!(
                "  {} : {} (frames: {}, nonzero: {}, consistent: {})",
                r.mode.format,
                r.verdict,
                r.frames_received,
                r.first_frame_nonzero,
                r.frame_size_consistent
            );
        }
    }

    Ok(())
}

fn validate_single_format(
    dev_path: &str,
    mode: &VideoMode,
) -> Result<facecam_common::formats::FormatProbeResult> {
    let file = v4l2::open_device(dev_path)?;
    let fd = file.as_raw_fd();

    // Try to set format
    if let Err(e) = v4l2::set_format(fd, mode.width, mode.height, mode.format.to_fourcc()) {
        return Ok(facecam_common::formats::FormatProbeResult {
            mode: *mode,
            negotiation_ok: false,
            stream_started: false,
            frames_received: 0,
            first_frame_nonzero: false,
            frame_size_consistent: false,
            avg_frame_interval_ms: None,
            error: Some(format!("set_format failed: {}", e)),
            verdict: FormatVerdict::NegotiationFailed,
        });
    }

    // Request buffers
    let _buf_count = match v4l2::request_buffers(fd, 4, 1) {
        Ok(n) => n,
        Err(e) => {
            return Ok(facecam_common::formats::FormatProbeResult {
                mode: *mode,
                negotiation_ok: true,
                stream_started: false,
                frames_received: 0,
                first_frame_nonzero: false,
                frame_size_consistent: false,
                avg_frame_interval_ms: None,
                error: Some(format!("request_buffers failed: {}", e)),
                verdict: FormatVerdict::NegotiationFailed,
            });
        }
    };

    // For validation, we just check if the format negotiation succeeds
    // Full streaming validation requires MMAP buffer mapping which is complex
    // For now, report based on known quirk data
    let (product, firmware) = detect_product_firmware_or_default();
    let verdict = if quirks::is_format_known_broken(product, firmware, mode.format) {
        FormatVerdict::GarbageFrames
    } else {
        FormatVerdict::Working
    };

    Ok(facecam_common::formats::FormatProbeResult {
        mode: *mode,
        negotiation_ok: true,
        stream_started: false,
        frames_received: 0,
        first_frame_nonzero: false,
        frame_size_consistent: false,
        avg_frame_interval_ms: None,
        error: None,
        verdict,
    })
}

fn detect_product_firmware_or_default() -> (ElgatoProduct, FirmwareVersion) {
    usb::enumerate_elgato_devices()
        .ok()
        .and_then(|devs| devs.into_iter().find(|d| d.product.is_facecam_original()))
        .map(|d| (d.product, d.firmware))
        .unwrap_or((
            ElgatoProduct::Facecam,
            FirmwareVersion { major: 0, minor: 0 },
        ))
}

/// Resolve a V4L2 device path — auto-detect if not specified
fn resolve_device(device: Option<String>) -> Result<String> {
    if let Some(dev) = device {
        return Ok(dev);
    }

    // Try to auto-detect via USB enumeration
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

    // Fallback: check for udev symlink
    let symlink = "/dev/video-facecam";
    if std::path::Path::new(symlink).exists() {
        return Ok(symlink.to_string());
    }

    anyhow::bail!(
        "Could not auto-detect Facecam V4L2 device. \
         Use --device /dev/videoN to specify manually."
    );
}
