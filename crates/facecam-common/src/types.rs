use serde::{Deserialize, Serialize};
use std::fmt;

/// Overall health status of the camera system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    /// Everything operational
    Healthy,
    /// Degraded but functional
    Degraded,
    /// Not operational
    Unhealthy,
    /// Device not connected
    Disconnected,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Unhealthy => write!(f, "unhealthy"),
            Self::Disconnected => write!(f, "disconnected"),
        }
    }
}

/// Pipeline state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineState {
    /// No device detected
    Idle,
    /// Device found, probing capabilities
    Probing,
    /// Attempting to start capture stream
    Starting,
    /// Actively capturing and forwarding frames
    Streaming,
    /// Recovering from error (USB reset in progress)
    Recovering,
    /// Stopped due to unrecoverable error
    Failed,
    /// Graceful shutdown in progress
    ShuttingDown,
}

impl fmt::Display for PipelineState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Probing => write!(f, "probing"),
            Self::Starting => write!(f, "starting"),
            Self::Streaming => write!(f, "streaming"),
            Self::Recovering => write!(f, "recovering"),
            Self::Failed => write!(f, "failed"),
            Self::ShuttingDown => write!(f, "shutting_down"),
        }
    }
}

/// Daemon status snapshot for IPC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub state: PipelineState,
    pub health: HealthStatus,
    pub uptime_secs: u64,
    pub device_connected: bool,
    pub active_mode: Option<crate::formats::VideoMode>,
    pub frames_captured: u64,
    pub frames_written: u64,
    pub frames_dropped: u64,
    pub recovery_count: u32,
    pub last_error: Option<String>,
    pub source_device: Option<String>,
    pub sink_device: Option<String>,
    pub active_profile: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub product_family: Option<String>,
}

/// V4L2 control value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlValue {
    pub name: String,
    pub id: u32,
    pub control_type: ControlType,
    pub value: i64,
    pub minimum: i64,
    pub maximum: i64,
    pub step: i64,
    pub default: i64,
    pub flags: u32,
    pub menu_items: Vec<MenuItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlType {
    Integer,
    Boolean,
    Menu,
    Button,
    Integer64,
    CtrlClass,
    String,
    Bitmask,
    IntegerMenu,
    Unknown(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuItem {
    pub index: u32,
    pub name: String,
}

/// A structured event for machine-readable logging
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub level: DiagnosticLevel,
    pub category: String,
    pub message: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticLevel {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}
