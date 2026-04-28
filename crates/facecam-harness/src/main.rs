use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use facecam_common::{
    device::{ElgatoProduct, FirmwareVersion},
    diagnostics,
    formats::PixelFormat,
    quirks, recovery, usb, v4l2,
};
use serde::{Deserialize, Serialize};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "facecam-harness")]
#[command(about = "Automated compatibility and stability testing for the Elgato Facecam")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// V4L2 device path (auto-detected if omitted)
    #[arg(long, global = true)]
    device: Option<String>,

    /// Output results as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the full compatibility test suite
    Full,
    /// Test format enumeration and negotiation
    Formats,
    /// Test open/close cycle stability
    OpenClose {
        /// Number of cycles to test
        #[arg(long, default_value = "10")]
        cycles: u32,
    },
    /// Test control enumeration and manipulation
    Controls,
    /// Test stream stability over time
    StreamStability {
        /// Duration in seconds
        #[arg(long, default_value = "30")]
        duration: u64,
    },
    /// Test USB recovery mechanism
    Recovery,
    /// Generate a compatibility report
    Report,
}

/// Results from a single test
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestResult {
    name: String,
    passed: bool,
    duration_ms: u64,
    details: serde_json::Value,
    error: Option<String>,
}

/// Results from a full test suite run
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SuiteResults {
    started_at: chrono::DateTime<chrono::Utc>,
    completed_at: chrono::DateTime<chrono::Utc>,
    device_path: String,
    system: diagnostics::SystemInfo,
    kernel_modules: diagnostics::KernelModuleInfo,
    tests: Vec<TestResult>,
    passed: usize,
    failed: usize,
    total: usize,
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

    match cli.command {
        Commands::Full => cmd_full(&cli),
        Commands::Formats => cmd_formats(&cli),
        Commands::OpenClose { cycles } => cmd_open_close(&cli, cycles),
        Commands::Controls => cmd_controls(&cli),
        Commands::StreamStability { duration } => cmd_stream_stability(&cli, duration),
        Commands::Recovery => cmd_recovery(&cli),
        Commands::Report => cmd_report(&cli),
    }
}

fn cmd_full(cli: &Cli) -> Result<()> {
    let started_at = Utc::now();
    let dev_path = resolve_device(&cli.device)?;
    let mut tests = Vec::new();

    println!("=== Facecam Compatibility Harness — Full Suite ===\n");
    println!("Device: {}\n", dev_path);

    // Test 1: Device detection
    tests.push(run_test("device_detection", test_device_detection));

    // Test 2: Format enumeration
    tests.push(run_test("format_enumeration", || {
        test_format_enumeration(&dev_path)
    }));

    // Test 3: Control enumeration
    tests.push(run_test("control_enumeration", || {
        test_control_enumeration(&dev_path)
    }));

    // Test 4: Format negotiation per format
    tests.push(run_test("format_negotiation", || {
        test_format_negotiation(&dev_path)
    }));

    // Test 5: Open/close cycles
    tests.push(run_test("open_close_cycles", || {
        test_open_close(&dev_path, 5)
    }));

    // Test 6: Control set/get roundtrip
    tests.push(run_test("control_roundtrip", || {
        test_control_roundtrip(&dev_path)
    }));

    // Test 7: USB topology validation
    tests.push(run_test("usb_topology", test_usb_topology));

    // Test 8: Kernel module status
    tests.push(run_test("kernel_modules", test_kernel_modules));

    let completed_at = Utc::now();
    let passed = tests.iter().filter(|t| t.passed).count();
    let failed = tests.iter().filter(|t| !t.passed).count();
    let total = tests.len();

    let suite = SuiteResults {
        started_at,
        completed_at,
        device_path: dev_path,
        system: diagnostics::collect_system_info(),
        kernel_modules: diagnostics::collect_kernel_module_info(),
        tests: tests.clone(),
        passed,
        failed,
        total,
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&suite)?);
    } else {
        println!("\n=== Results ===\n");
        for t in &tests {
            let status = if t.passed { "PASS" } else { "FAIL" };
            println!("  [{}] {} ({}ms)", status, t.name, t.duration_ms);
            if let Some(ref err) = t.error {
                println!("        Error: {}", err);
            }
        }
        println!("\n  {}/{} passed, {} failed", passed, total, failed);
    }

    // Export results
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let report_dir = std::path::PathBuf::from(&home).join(".local/share/facecam/harness");
    std::fs::create_dir_all(&report_dir)?;
    let report_path = report_dir.join(format!(
        "harness-{}.json",
        Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(&report_path, serde_json::to_string_pretty(&suite)?)?;
    println!("\nReport saved to: {}", report_path.display());

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_formats(cli: &Cli) -> Result<()> {
    let dev_path = resolve_device(&cli.device)?;
    let result = run_test("format_enumeration", || test_format_enumeration(&dev_path));
    print_single_result(&result, cli.json)
}

fn cmd_open_close(cli: &Cli, cycles: u32) -> Result<()> {
    let dev_path = resolve_device(&cli.device)?;
    let result = run_test("open_close_cycles", || test_open_close(&dev_path, cycles));
    print_single_result(&result, cli.json)
}

fn cmd_controls(cli: &Cli) -> Result<()> {
    let dev_path = resolve_device(&cli.device)?;
    let result = run_test("control_roundtrip", || test_control_roundtrip(&dev_path));
    print_single_result(&result, cli.json)
}

fn cmd_stream_stability(cli: &Cli, duration: u64) -> Result<()> {
    let dev_path = resolve_device(&cli.device)?;
    let result = run_test("stream_stability", || {
        test_stream_stability(&dev_path, Duration::from_secs(duration))
    });
    print_single_result(&result, cli.json)
}

fn cmd_recovery(cli: &Cli) -> Result<()> {
    let result = run_test("usb_recovery", test_usb_recovery);
    print_single_result(&result, cli.json)
}

fn cmd_report(_cli: &Cli) -> Result<()> {
    // List previous reports
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let report_dir = std::path::PathBuf::from(&home).join(".local/share/facecam/harness");

    if !report_dir.exists() {
        println!("No previous harness reports found.");
        return Ok(());
    }

    let mut reports: Vec<_> = std::fs::read_dir(&report_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();

    reports.sort_by_key(|e| e.file_name());

    if reports.is_empty() {
        println!("No previous harness reports found.");
    } else {
        println!("=== Harness Reports ===\n");
        for r in &reports {
            println!("  {}", r.path().display());
        }
        println!("\nLatest report:");
        if let Some(latest) = reports.last() {
            let content = std::fs::read_to_string(latest.path())?;
            let suite: SuiteResults = serde_json::from_str(&content)?;
            println!("  Date:   {}", suite.started_at);
            println!("  Device: {}", suite.device_path);
            println!("  Result: {}/{} passed", suite.passed, suite.total);
        }
    }

    Ok(())
}

// === Test implementations ===

fn test_device_detection() -> Result<serde_json::Value> {
    let devices = usb::enumerate_elgato_devices()?;
    let facecams: Vec<_> = devices
        .iter()
        .filter(|d| d.product.is_facecam_original())
        .collect();

    if facecams.is_empty() {
        anyhow::bail!("No Elgato Facecam detected via USB enumeration");
    }

    Ok(serde_json::json!({
        "count": facecams.len(),
        "devices": facecams,
    }))
}

fn test_format_enumeration(dev_path: &str) -> Result<serde_json::Value> {
    let file = v4l2::open_device(dev_path)?;
    let fd = file.as_raw_fd();

    let (product, firmware) = detect_product_firmware_or_default();

    let formats = v4l2::enumerate_formats(fd)?;
    let modes = v4l2::enumerate_all_modes(fd)?;

    if formats.is_empty() {
        anyhow::bail!("No formats enumerated from device");
    }

    // Check for known bogus formats
    let bogus_count = formats
        .iter()
        .filter(|f| quirks::is_format_known_broken(product, firmware, f.pixel_format))
        .count();

    Ok(serde_json::json!({
        "format_count": formats.len(),
        "mode_count": modes.len(),
        "bogus_format_count": bogus_count,
        "formats": formats,
        "has_yuyv": formats.iter().any(|f| f.pixel_format == PixelFormat::Yuyv),
        "has_mjpeg": formats.iter().any(|f| f.pixel_format == PixelFormat::Mjpeg),
    }))
}

fn test_control_enumeration(dev_path: &str) -> Result<serde_json::Value> {
    let file = v4l2::open_device(dev_path)?;
    let fd = file.as_raw_fd();
    let controls = v4l2::enumerate_controls(fd)?;

    if controls.is_empty() {
        anyhow::bail!("No controls enumerated from device");
    }

    Ok(serde_json::json!({
        "control_count": controls.len(),
        "controls": controls.iter().map(|c| &c.name).collect::<Vec<_>>(),
    }))
}

fn test_format_negotiation(dev_path: &str) -> Result<serde_json::Value> {
    let file = v4l2::open_device(dev_path)?;
    let fd = file.as_raw_fd();

    let (product, firmware) = detect_product_firmware_or_default();

    let formats = v4l2::enumerate_formats(fd)?;
    let mut results = Vec::new();

    for fmt in &formats {
        let success = v4l2::set_format(fd, 1920, 1080, fmt.fourcc_raw).is_ok();
        results.push(serde_json::json!({
            "format": fmt.pixel_format.fourcc_str(),
            "negotiation_ok": success,
            "expected_reliable": !quirks::is_format_known_broken(product, firmware, fmt.pixel_format),
        }));
    }

    Ok(serde_json::json!({
        "results": results,
    }))
}

fn test_open_close(dev_path: &str, cycles: u32) -> Result<serde_json::Value> {
    let mut successes = 0u32;
    let mut failures = Vec::new();

    for i in 0..cycles {
        match v4l2::open_device(dev_path) {
            Ok(file) => {
                let fd = file.as_raw_fd();
                // Try to query caps to verify the device is responsive
                match v4l2::query_capabilities(fd) {
                    Ok(_) => successes += 1,
                    Err(e) => {
                        failures.push(serde_json::json!({
                            "cycle": i,
                            "phase": "query_caps",
                            "error": e.to_string(),
                        }));
                    }
                }
                drop(file);
                // Brief pause between cycles
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                failures.push(serde_json::json!({
                    "cycle": i,
                    "phase": "open",
                    "error": e.to_string(),
                }));

                // Attempt USB reset to recover for next cycle
                if let Err(reset_err) = recovery::usb_reset_facecam() {
                    warn!(error = %reset_err, "USB reset failed during open/close test");
                } else {
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    if !failures.is_empty() {
        warn!(
            successes,
            failures = failures.len(),
            cycles,
            "Open/close cycles had failures"
        );
    }

    let passed = failures.is_empty();
    if !passed {
        anyhow::bail!("{} of {} open/close cycles failed", failures.len(), cycles);
    }

    Ok(serde_json::json!({
        "cycles": cycles,
        "successes": successes,
        "failures": failures,
    }))
}

fn test_control_roundtrip(dev_path: &str) -> Result<serde_json::Value> {
    let file = v4l2::open_device(dev_path)?;
    let fd = file.as_raw_fd();

    let controls = v4l2::enumerate_controls(fd)?;
    let mut roundtrips = Vec::new();

    // Test brightness if available — safe to modify and restore
    if let Some(ctrl) = controls.iter().find(|c| c.name == "brightness") {
        let original = ctrl.value;
        let test_value = ctrl.minimum + (ctrl.maximum - ctrl.minimum) / 2;

        let set_ok = v4l2::set_control(fd, ctrl.id, test_value as i32).is_ok();
        let readback = v4l2::get_control(fd, ctrl.id).unwrap_or(-1);
        let restored = v4l2::set_control(fd, ctrl.id, original as i32).is_ok();

        roundtrips.push(serde_json::json!({
            "control": ctrl.name,
            "original": original,
            "test_value": test_value,
            "set_ok": set_ok,
            "readback": readback,
            "readback_matches": readback == test_value,
            "restored": restored,
        }));
    }

    Ok(serde_json::json!({
        "roundtrips": roundtrips,
        "total_controls": controls.len(),
    }))
}

fn test_stream_stability(dev_path: &str, duration: Duration) -> Result<serde_json::Value> {
    // This test opens the device, sets YUYV 1080p30, and monitors
    // whether the device remains responsive over the given duration
    let file = v4l2::open_device(dev_path)?;
    let fd = file.as_raw_fd();

    v4l2::set_format(fd, 1920, 1080, PixelFormat::Yuyv.to_fourcc())?;

    let start = Instant::now();
    let mut check_count = 0u32;
    let mut failures = 0u32;

    while start.elapsed() < duration {
        // Periodically verify the device is responsive by querying a control
        match v4l2::query_capabilities(fd) {
            Ok(_) => check_count += 1,
            Err(_) => failures += 1,
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    if failures > 0 {
        anyhow::bail!(
            "Device became unresponsive {} times during {} checks",
            failures,
            check_count + failures
        );
    }

    Ok(serde_json::json!({
        "duration_secs": duration.as_secs(),
        "checks": check_count,
        "failures": failures,
    }))
}

fn test_usb_topology() -> Result<serde_json::Value> {
    let sysfs = usb::find_facecam_sysfs_path()?
        .ok_or_else(|| anyhow::anyhow!("Facecam not found in sysfs"))?;

    let topo = usb::read_usb_topology(&sysfs)?;

    // Warn if not on SuperSpeed
    let speed_ok = topo
        .speed
        .as_ref()
        .map(|s| s.contains("5000") || s.contains("10000") || s.contains("20000"))
        .unwrap_or(false);

    if !speed_ok {
        anyhow::bail!(
            "Device not on USB 3.0+ (speed: {:?}). USB 3.0 is required.",
            topo.speed
        );
    }

    Ok(serde_json::json!({
        "sysfs_path": topo.sysfs_path,
        "speed": topo.speed,
        "usb_version": topo.version,
        "speed_ok": speed_ok,
    }))
}

fn test_kernel_modules() -> Result<serde_json::Value> {
    let info = diagnostics::collect_kernel_module_info();

    let mut issues = Vec::new();
    if !info.uvcvideo_loaded {
        issues.push("uvcvideo module not loaded");
    }
    if !info.v4l2loopback_loaded {
        issues.push("v4l2loopback module not loaded (required for normalization pipeline)");
    }

    if !info.uvcvideo_loaded {
        anyhow::bail!("uvcvideo kernel module not loaded");
    }

    Ok(serde_json::json!({
        "uvcvideo_loaded": info.uvcvideo_loaded,
        "uvcvideo_version": info.uvcvideo_version,
        "v4l2loopback_loaded": info.v4l2loopback_loaded,
        "v4l2loopback_version": info.v4l2loopback_version,
        "issues": issues,
    }))
}

fn test_usb_recovery() -> Result<serde_json::Value> {
    info!("Testing USB reset recovery mechanism");

    let presence = recovery::check_device_present()?;
    if !presence.connected {
        anyhow::bail!("Facecam not connected — cannot test recovery");
    }

    let result = recovery::usb_reset_facecam()?;

    // Verify device came back
    std::thread::sleep(Duration::from_secs(2));
    let post_presence = recovery::check_device_present()?;

    if !post_presence.connected {
        anyhow::bail!("Device did not re-appear after USB reset");
    }

    Ok(serde_json::json!({
        "reset_success": result.success,
        "device_recovered": post_presence.connected,
        "device_authorized": post_presence.authorized,
    }))
}

// === Helpers ===

fn run_test<F>(name: &str, test_fn: F) -> TestResult
where
    F: FnOnce() -> Result<serde_json::Value>,
{
    println!("  Running: {} ...", name);
    let start = Instant::now();

    match test_fn() {
        Ok(details) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            println!("  [PASS] {} ({}ms)", name, duration_ms);
            TestResult {
                name: name.to_string(),
                passed: true,
                duration_ms,
                details,
                error: None,
            }
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            println!("  [FAIL] {} ({}ms): {}", name, duration_ms, e);
            TestResult {
                name: name.to_string(),
                passed: false,
                duration_ms,
                details: serde_json::Value::Null,
                error: Some(e.to_string()),
            }
        }
    }
}

fn print_single_result(result: &TestResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        let status = if result.passed { "PASS" } else { "FAIL" };
        println!("[{}] {} ({}ms)", status, result.name, result.duration_ms);
        if let Some(ref err) = result.error {
            println!("  Error: {}", err);
        }
    }
    Ok(())
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

fn resolve_device(device: &Option<String>) -> Result<String> {
    if let Some(dev) = device {
        return Ok(dev.clone());
    }

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

    anyhow::bail!("Could not auto-detect Facecam. Use --device /dev/videoN")
}
