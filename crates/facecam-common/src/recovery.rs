use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// USB device reset via sysfs authorized flag cycle.
///
/// This is the primary recovery mechanism for the open/close lockup bug.
/// Writing 0 to the `authorized` sysfs file deauthorizes the device,
/// causing the kernel to unbind the driver. Writing 1 reauthorizes it,
/// causing re-enumeration — equivalent to a physical unplug/replug.
pub fn usb_reset_product(product: crate::device::ElgatoProduct) -> Result<ResetResult> {
    let sysfs_path = crate::usb::find_elgato_sysfs_path(product)?
        .ok_or_else(|| anyhow::anyhow!("{} not found in sysfs", product))?;
    usb_reset_device(&sysfs_path)
}

/// Reset a USB device by its sysfs path
pub fn usb_reset_device(sysfs_path: &Path) -> Result<ResetResult> {
    let auth_path = sysfs_path.join("authorized");

    if !auth_path.exists() {
        bail!("sysfs authorized file not found at {}", auth_path.display());
    }

    info!(path = %sysfs_path.display(), "Performing USB reset via sysfs");

    let current = fs::read_to_string(&auth_path)
        .context("Failed to read authorized state")?
        .trim()
        .to_string();

    debug!(current_state = %current, "Current authorized state");

    let mut warnings = Vec::new();

    // Deauthorize. The Pro returns ETIMEDOUT here but the disconnect still
    // happens — record it as a warning rather than failing.
    if let Err(e) = fs::write(&auth_path, "0") {
        let kind = e.kind();
        if matches!(kind, std::io::ErrorKind::TimedOut) || e.raw_os_error() == Some(libc::ETIMEDOUT)
        {
            warnings.push(format!("authorized=0 returned ETIMEDOUT: {}", e));
        } else {
            return Err(e).context("Failed to deauthorize USB device");
        }
    }
    info!("Device deauthorized (or timed-out-but-disconnected), waiting for kernel cleanup");

    // Longer wait — the Pro needs ~1.5s post-deauth, original tolerates the same.
    thread::sleep(Duration::from_millis(1500));

    fs::write(&auth_path, "1").context("Failed to reauthorize USB device")?;
    info!("Device reauthorized, waiting for re-enumeration");

    // Longer wait — the Pro needs ~4s before it accepts streaming.
    thread::sleep(Duration::from_millis(4000));

    let new_state = fs::read_to_string(&auth_path)
        .context("Failed to read authorized state after reset")?
        .trim()
        .to_string();

    if new_state != "1" {
        bail!(
            "Device did not come back after reset (authorized = {})",
            new_state
        );
    }

    info!(warning_count = warnings.len(), "USB reset complete");

    Ok(ResetResult {
        sysfs_path: sysfs_path.to_path_buf(),
        success: true,
        previous_state: current,
        new_state,
        warnings,
    })
}

/// In-place STREAMOFF/STREAMON cycle for VIDEO_CAPTURE. Cheapest first-line
/// recovery for the Pro's stream-start race (PRO_STREAM_START_RACE).
pub fn stream_off_then_on(fd: std::os::unix::io::RawFd) -> Result<()> {
    use crate::v4l2;
    // EINVAL is expected if the device was not streaming; ignore.
    let _ = v4l2::stream_off(fd, 1 /* V4L2_BUF_TYPE_VIDEO_CAPTURE */);
    v4l2::stream_on(fd, 1).context("STREAMON after in-place recovery")?;
    Ok(())
}

/// Attempt to start a stream with retry-on-failure logic.
///
/// Due to the ~50% startup failure rate, this function:
/// 1. Attempts to open and start the stream
/// 2. On failure, performs a USB reset
/// 3. Waits for the device to re-appear
/// 4. Retries the open
///
/// Returns the number of attempts needed.
pub fn retry_with_reset<F, T>(
    product: crate::device::ElgatoProduct,
    max_attempts: u32,
    operation_name: &str,
    mut operation: F,
) -> Result<(T, u32)>
where
    F: FnMut(u32) -> Result<T>,
{
    let mut last_error = None;

    for attempt in 1..=max_attempts {
        info!(
            attempt,
            max_attempts,
            operation = operation_name,
            "Attempting operation"
        );

        match operation(attempt) {
            Ok(result) => {
                if attempt > 1 {
                    info!(
                        attempt,
                        operation = operation_name,
                        "Operation succeeded after retry"
                    );
                }
                return Ok((result, attempt));
            }
            Err(e) => {
                warn!(
                    attempt,
                    max_attempts,
                    error = %e,
                    operation = operation_name,
                    "Operation failed"
                );
                last_error = Some(e);

                if attempt < max_attempts {
                    info!("Performing USB reset before retry");
                    match usb_reset_product(product) {
                        Ok(reset) => {
                            info!(
                                sysfs = %reset.sysfs_path.display(),
                                "USB reset successful, waiting before retry"
                            );
                            thread::sleep(Duration::from_secs(1));
                        }
                        Err(reset_err) => {
                            error!(error = %reset_err, "USB reset failed");
                        }
                    }
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All {} attempts failed", max_attempts)))
}

/// Wait for an Elgato product to appear in sysfs after a reset or plug event
pub fn wait_for_device(
    product: crate::device::ElgatoProduct,
    timeout: Duration,
) -> Result<PathBuf> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(200);

    info!(
        timeout_ms = timeout.as_millis() as u64,
        "Waiting for {} to appear", product
    );

    loop {
        if start.elapsed() > timeout {
            bail!(
                "Timeout waiting for {} to appear ({}ms)",
                product,
                timeout.as_millis()
            );
        }

        match crate::usb::find_elgato_sysfs_path(product) {
            Ok(Some(path)) => {
                info!(path = %path.display(), elapsed_ms = start.elapsed().as_millis() as u64, "{} found", product);
                return Ok(path);
            }
            Ok(None) => {}
            Err(e) => {
                debug!(error = %e, "Error scanning sysfs");
            }
        }

        thread::sleep(poll_interval);
    }
}

#[derive(Debug, Clone)]
pub struct ResetResult {
    pub sysfs_path: PathBuf,
    pub success: bool,
    pub previous_state: String,
    pub new_state: String,
    /// Non-fatal anomalies during reset (e.g. ETIMEDOUT on authorized=0
    /// write, which the Pro produces but still re-enumerates afterward).
    pub warnings: Vec<String>,
}

/// Check if an Elgato product is currently connected and authorized
pub fn check_device_present(product: crate::device::ElgatoProduct) -> Result<DevicePresence> {
    match crate::usb::find_elgato_sysfs_path(product)? {
        Some(path) => {
            let auth_path = path.join("authorized");
            let authorized = if auth_path.exists() {
                fs::read_to_string(&auth_path)?
                    .trim()
                    .parse::<u8>()
                    .unwrap_or(0)
                    == 1
            } else {
                false
            };

            Ok(DevicePresence {
                connected: true,
                authorized,
                sysfs_path: Some(path),
            })
        }
        None => Ok(DevicePresence {
            connected: false,
            authorized: false,
            sysfs_path: None,
        }),
    }
}

#[derive(Debug, Clone)]
pub struct DevicePresence {
    pub connected: bool,
    pub authorized: bool,
    pub sysfs_path: Option<PathBuf>,
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::device::ElgatoProduct;

    #[test]
    fn reset_result_supports_warnings() {
        let r = ResetResult {
            sysfs_path: PathBuf::from("/dev/null"),
            success: true,
            previous_state: "1".into(),
            new_state: "1".into(),
            warnings: vec!["authorized=0 timed out".into()],
        };
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    #[ignore = "requires Facecam Pro hardware"]
    fn usb_reset_product_finds_pro() {
        let path = crate::usb::find_elgato_sysfs_path(ElgatoProduct::FacecamPro)
            .expect("sysfs lookup")
            .expect("Pro sysfs");
        assert!(path.join("authorized").exists());
    }
}
