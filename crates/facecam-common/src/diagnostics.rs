use crate::device::DeviceFingerprint;
use crate::types::{ControlValue, DaemonStatus, DiagnosticEvent};
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

/// A complete diagnostics bundle for remote debugging
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsBundle {
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub system: SystemInfo,
    pub device: Option<DeviceFingerprint>,
    pub daemon_status: Option<DaemonStatus>,
    pub controls: Vec<ControlValue>,
    pub usb_topology: Option<serde_json::Value>,
    pub v4l2_devices: Vec<String>,
    pub kernel_modules: KernelModuleInfo,
    pub recent_events: Vec<DiagnosticEvent>,
    pub config_files: Vec<ConfigFile>,
    #[serde(default)]
    pub format_probes: Vec<crate::formats::FormatProbeResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub kernel_version: String,
    pub os_release: String,
    pub ubuntu_version: String,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelModuleInfo {
    pub uvcvideo_loaded: bool,
    pub uvcvideo_version: Option<String>,
    pub v4l2loopback_loaded: bool,
    pub v4l2loopback_version: Option<String>,
    pub v4l2loopback_params: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub path: String,
    pub content: String,
}

/// Collect system information
pub fn collect_system_info() -> SystemInfo {
    let hostname = fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();

    let kernel_version = fs::read_to_string("/proc/version")
        .unwrap_or_default()
        .trim()
        .to_string();

    let os_release = fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("PRETTY_NAME="))
        .map(|l| {
            l.trim_start_matches("PRETTY_NAME=")
                .trim_matches('"')
                .to_string()
        })
        .unwrap_or_default();

    let ubuntu_version = fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VERSION_ID="))
        .map(|l| {
            l.trim_start_matches("VERSION_ID=")
                .trim_matches('"')
                .to_string()
        })
        .unwrap_or_default();

    let uptime_secs = fs::read_to_string("/proc/uptime")
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0);

    SystemInfo {
        hostname,
        kernel_version,
        os_release,
        ubuntu_version,
        uptime_secs,
    }
}

/// Check kernel module status
pub fn collect_kernel_module_info() -> KernelModuleInfo {
    let modules = fs::read_to_string("/proc/modules").unwrap_or_default();

    let uvcvideo_loaded = modules.lines().any(|l| l.starts_with("uvcvideo "));
    let v4l2loopback_loaded = modules.lines().any(|l| l.starts_with("v4l2loopback "));

    let uvcvideo_version = read_module_param("/sys/module/uvcvideo/version");
    let v4l2loopback_version = read_module_param("/sys/module/v4l2loopback/version");

    let v4l2loopback_params = if v4l2loopback_loaded {
        collect_module_params("/sys/module/v4l2loopback/parameters")
    } else {
        None
    };

    KernelModuleInfo {
        uvcvideo_loaded,
        uvcvideo_version,
        v4l2loopback_loaded,
        v4l2loopback_version,
        v4l2loopback_params,
    }
}

fn read_module_param(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn collect_module_params(dir: &str) -> Option<String> {
    let path = Path::new(dir);
    if !path.exists() {
        return None;
    }

    let mut params = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(value) = fs::read_to_string(entry.path()) {
                params.push(format!("{}={}", name, value.trim()));
            }
        }
    }
    params.sort();
    Some(params.join("\n"))
}

/// List all V4L2 device nodes
pub fn list_v4l2_devices() -> Vec<String> {
    let mut devices = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("video") {
                devices.push(format!("/dev/{}", name));
            }
        }
    }
    devices.sort();
    devices
}

/// Export a diagnostics bundle to a JSON file
pub fn export_bundle(bundle: &DiagnosticsBundle) -> Result<PathBuf> {
    let dir = bundle_dir();
    fs::create_dir_all(&dir)?;

    let filename = format!("facecam-diag-{}.json", Utc::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(&filename);

    let json = serde_json::to_string_pretty(bundle)?;
    fs::write(&path, json)?;

    info!(path = %path.display(), "Diagnostics bundle exported");
    Ok(path)
}

fn bundle_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("facecam")
        .join("diagnostics")
}

/// Create a minimal diagnostics bundle from available information
pub fn create_bundle(
    device: Option<DeviceFingerprint>,
    status: Option<DaemonStatus>,
    controls: Vec<ControlValue>,
    events: Vec<DiagnosticEvent>,
) -> DiagnosticsBundle {
    DiagnosticsBundle {
        generated_at: Utc::now(),
        system: collect_system_info(),
        device,
        daemon_status: status,
        controls,
        usb_topology: None,
        v4l2_devices: list_v4l2_devices(),
        kernel_modules: collect_kernel_module_info(),
        recent_events: events,
        config_files: collect_config_files(),
        format_probes: Vec::new(),
    }
}

fn collect_config_files() -> Vec<ConfigFile> {
    let mut files = Vec::new();

    let paths = [
        "/etc/modprobe.d/v4l2loopback.conf",
        "/etc/modules-load.d/v4l2loopback.conf",
        "/etc/udev/rules.d/99-facecam.rules",
    ];

    for path in &paths {
        if let Ok(content) = fs::read_to_string(path) {
            files.push(ConfigFile {
                path: path.to_string(),
                content,
            });
        }
    }

    // Also include user config
    let home = std::env::var("HOME").unwrap_or_default();
    let user_config = PathBuf::from(&home)
        .join(".config")
        .join("facecam")
        .join("daemon.toml");
    if let Ok(content) = fs::read_to_string(&user_config) {
        files.push(ConfigFile {
            path: user_config.to_string_lossy().to_string(),
            content,
        });
    }

    files
}
