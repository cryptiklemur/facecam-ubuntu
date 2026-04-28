<p align="center">
  <h1 align="center">facecam-ubuntu</h1>
  <p align="center">
    Linux-native runtime for the Elgato Facecam on Ubuntu
    <br />
    <a href="https://copyleftdev.github.io/facecam-ubuntu/"><strong>Documentation</strong></a>
    &middot;
    <a href="https://github.com/copyleftdev/facecam-ubuntu/releases">Releases</a>
    &middot;
    <a href="https://github.com/copyleftdev/facecam-ubuntu/issues">Issues</a>
  </p>
</p>

<p align="center">
  <a href="https://github.com/copyleftdev/facecam-ubuntu/actions/workflows/ci.yml"><img src="https://github.com/copyleftdev/facecam-ubuntu/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/copyleftdev/facecam-ubuntu/actions/workflows/docs.yml"><img src="https://github.com/copyleftdev/facecam-ubuntu/actions/workflows/docs.yml/badge.svg" alt="Docs" /></a>
  <a href="https://github.com/copyleftdev/facecam-ubuntu/releases"><img src="https://img.shields.io/github/v/release/copyleftdev/facecam-ubuntu?include_prereleases&label=release" alt="Release" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue" alt="License" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.75%2B-orange" alt="Rust" /></a>
</p>

---

No official Linux driver exists for the Elgato Facecam. This project provides the complete user-space control plane: device probing, a normalization daemon, profile-based control persistence, automated compatibility testing, and a broadcast-grade visual diagnostic tool.

## The Problem

The Elgato Facecam (USB `0fd9:0078`) enumerates as a standard UVC device on Linux but has critical quirks that break real-world workflows:

| Quirk | Impact |
|-------|--------|
| Bogus NV12/YU12 format advertisement | Green/garbage frames if wrong format selected |
| Open/close cycle lockup | Device bricks after first app closes it |
| ~50% startup failure rate | Camera randomly fails to initialize |
| No MJPEG on old firmware | Chromium/Electron apps can't negotiate |
| USB 2.0 fallback mode | PID changes to `0x0077`, no video at all |

Also supports the Elgato Facecam Pro (USB `0fd9:0079`). The Pro has its own quirks, empirically observed on firmware 0.06:

| Quirk | Impact | Mitigation |
|-------|--------|------------|
| `PRO_STREAM_START_RACE` | First STREAMON after open intermittently produces no frames | auto-handled by daemon recovery ladder |
| `PRO_USB_RESET_DESTRUCTIVE` | Issuing a USB reset mid-stream tears down the pipeline and forces full re-enumeration | auto-handled by daemon recovery ladder |

Every workaround in this project traces to a specific observed device behavior. No speculative mitigations.

## The Solution

```
Physical Facecam (/dev/video0)
        |
   facecam-daemon          # Single long-lived process
   (MMAP capture + USB     # Owns device, prevents lockup
    reset recovery)        # Auto-recovers from failures
        |
   v4l2loopback            # /dev/video10 "Facecam Normalized"
   (exclusive_caps=1)      # Chrome/Electron compatible
        |
   Any application         # OBS, Chrome, Zoom, Teams, etc.
```

## Recovery ladder

When the Pro misbehaves mid-stream, the daemon escalates through three rungs, stopping at the first one that restores frames:

1. **In-place STREAMOFF/STREAMON** - toggle streaming on the existing fd. Cheapest and fixes most stream-start races.
2. **Close + reopen pipeline** - tear down the v4l2 fd, reopen the device node, renegotiate format, resume capture.
3. **USB reset via sysfs `authorized` flag** - last resort. Writes `0` then `1` to the device's `authorized` attribute, forcing a full re-enumeration. Destructive on the Pro (see `PRO_USB_RESET_DESTRUCTIVE`), so the daemon only reaches this rung after the first two fail.

Every Pro mitigation is documented with the experiment that produced it - run `cargo doc --open` for full quirk descriptions.

## Install

### From .deb (Ubuntu 24.04+)

```bash
# Download from Releases page or CI artifacts
sudo dpkg -i facecam-ubuntu_*.deb
sudo apt-get install -f
```

### From Source

```bash
sudo apt-get install -y v4l2loopback-dkms v4l-utils libusb-1.0-0-dev pkg-config
git clone https://github.com/copyleftdev/facecam-ubuntu
cd facecam-ubuntu
cargo build --workspace --release
sudo ./install.sh
```

## Quick Start

```bash
# 1. Detect camera (USB 3.0 port required)
facecam-probe detect

# 2. See it live with diagnostic overlays
facecam-visual --resolution 720

# 3. Start the normalization daemon
sudo modprobe v4l2loopback video_nr=10 card_label="Facecam Normalized" exclusive_caps=1
sudo systemctl start facecam-daemon

# 4. Use /dev/video10 ("Facecam Normalized") in any app
```

## Tools

### facecam-probe

Detect, fingerprint, and enumerate the camera.

```
$ facecam-probe detect

Device:     Elgato Facecam (PID 0x0078)
Firmware:   4.09
Serial:     FW06M1A07449
USB:        bus 10 addr 2 (SuperSpeed 5 Gbps)
V4L2:       /dev/video0
```

```bash
facecam-probe formats     # Pixel formats, resolutions, bandwidth
facecam-probe controls    # All V4L2 controls with ranges
facecam-probe quirks      # Applicable device quirks
facecam-probe diagnostics # Full system diagnostic bundle (JSON)
```

### facecam-visual

Live camera viewer with broadcast-grade analysis overlays.

```
+---------------------------------------+
|          LIVE CAMERA FEED             |
|    [30 FPS]              [ZEBRA]     |
|                          [FOCUS]     |
|                          [ A/B ]     |
+=======================================+
| WAVEFORM MONITOR  | RGB HISTOGRAM    |
+---------------------------------------+
| 30.0fps 33.3ms  Brt:128  Con:5 ...  |
+---------------------------------------+
| Frame Timing Waterfall               |
+---------------------------------------+
```

| Key | Feature |
|-----|---------|
| <kbd>W</kbd> | Zebra stripes (overexposure > 235 IRE) |
| <kbd>E</kbd> | Focus peaking (Sobel edge detection) |
| <kbd>A</kbd> / <kbd>D</kbd> | A/B comparison capture / clear |
| <kbd>+</kbd> <kbd>-</kbd> | Brightness |
| <kbd>[</kbd> <kbd>]</kbd> | Contrast |
| <kbd>Z</kbd> <kbd>X</kbd> | Zoom |
| <kbd>S</kbd> | Snapshot with overlays |

### facecam-ctl

Control the daemon at runtime.

```bash
facecam-ctl status                        # Pipeline state, FPS, frame counts
facecam-ctl control set brightness 150    # Adjust controls
facecam-ctl profile apply streaming       # Switch profiles
facecam-ctl diagnostics                   # Export support bundle
facecam-ctl reset                         # Force USB recovery
```

### facecam-harness

Automated compatibility test suite.

```bash
$ facecam-harness full

  [PASS] device_detection (45ms)
  [PASS] format_enumeration (12ms)
  [PASS] control_enumeration (8ms)
  [PASS] format_negotiation (23ms)
  [PASS] open_close_cycles (1204ms)
  [PASS] control_roundtrip (15ms)
  [PASS] usb_topology (3ms)
  [PASS] kernel_modules (1ms)

  8/8 passed, 0 failed
```

## Profiles

```bash
facecam-ctl profile list
```

| Profile | Mode | Description |
|---------|------|-------------|
| `default` | 1080p30 UYVY | Factory defaults, auto exposure |
| `streaming` | 1080p60 MJPEG | Optimized for live streaming |
| `lowlight` | 720p30 UYVY | Higher brightness, manual exposure |
| `meeting` | 720p30 MJPEG | Bandwidth-friendly for calls |

Custom profiles are TOML files in `~/.config/facecam/profiles/`.

## Device Reference

Empirically verified on firmware 4.09:

| Control | Range | Default |
|---------|-------|---------|
| Brightness | 0-255 | 128 |
| Contrast | 0-10 | 3 |
| Saturation | 0-63 | 35 |
| Sharpness | 0-4 | 2 |
| White Balance | 2800-12500K | 5000K |
| Exposure Time | 1-2500 (100us units) | 156 |
| Zoom | 1-31 | 1 |

Extension Unit GUID `{a8e5782b-36e6-4fa1-87f8-83e32b323124}` with 9 proprietary controls (noise reduction, metering, save-to-flash) — documented but not yet reverse-engineered.

## Security

All input parsing surfaces are fuzz-tested with AFL++:

| Target | Executions | Crashes |
|--------|-----------|---------|
| V4L2 ioctl responses | 87K | 0 |
| IPC JSON commands | 310K | 0 |
| Profile TOML parsing | 64K | 0 |
| Format/fourcc parsing | 87K | 0 |
| Firmware BCD parsing | 87K | 0 |

## Architecture

```
facecam-ubuntu/
  crates/
    facecam-common/     # Shared: V4L2, USB, quirks, profiles, IPC
    facecam-probe/      # Device detection CLI
    facecam-daemon/     # Normalization daemon (capture -> v4l2loopback)
    facecam-ctl/        # Daemon control CLI
    facecam-harness/    # Automated test suite
    facecam-visual/     # Visual diagnostic tool
  config/               # udev, systemd, modprobe configs
  fuzz/                 # AFL++ harnesses and corpus
  docs/                 # mdbook documentation
```

## Requirements

- Ubuntu 24.04+ (tested on 25.10, kernel 6.8+)
- USB 3.0 port (mandatory for Facecam)
- Rust 1.75+ (build from source)
- `v4l2loopback-dkms`, `v4l-utils`, `libusb-1.0-0`

## Contributing

```bash
cargo fmt --all -- --check    # Formatting
cargo clippy -- -D warnings   # Lints
cargo test --workspace        # Tests
```

See the [Contributing Guide](https://copyleftdev.github.io/facecam-ubuntu/development/contributing.html) for details.

## License

This project is licensed under the [GNU General Public License v3.0 or later](LICENSE).

The GPL-3.0 is chosen to align with the Linux kernel ecosystem — the Facecam runtime interacts with GPL-licensed kernel modules (`uvcvideo`, `v4l2loopback`) and benefits from staying in the same license family.
