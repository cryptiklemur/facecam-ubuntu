use facecam_common::device::{ElgatoProduct, FirmwareVersion};
use facecam_common::formats::PixelFormat;
use facecam_common::quirks::{applicable_quirks, is_format_known_broken, quirk_registry};

const FW_FACECAM_409: FirmwareVersion = FirmwareVersion { major: 4, minor: 9 };
const FW_FACECAM_300: FirmwareVersion = FirmwareVersion { major: 3, minor: 0 };
const FW_PRO_006: FirmwareVersion = FirmwareVersion { major: 0, minor: 6 };

#[test]
fn every_quirk_has_at_least_one_product() {
    for q in quirk_registry() {
        assert!(
            !q.products.is_empty(),
            "Quirk {} has empty products list — quirks must cite which products they were observed on",
            q.id
        );
    }
}

#[test]
fn no_contradictory_firmware_ranges() {
    for q in quirk_registry() {
        if let (Some(min), Some(max)) = (q.firmware_min, q.firmware_max) {
            assert!(
                min < max,
                "Quirk {} has firmware_min ({}) >= firmware_max ({})",
                q.id,
                min,
                max
            );
        }
    }
}

#[test]
fn quirk_ids_are_unique() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for q in quirk_registry() {
        assert!(seen.insert(q.id), "Duplicate quirk id: {}", q.id);
    }
}

#[test]
fn quirk_descriptions_are_non_empty() {
    for q in quirk_registry() {
        assert!(!q.summary.is_empty(), "Quirk {} has empty summary", q.id);
        assert!(
            !q.description.is_empty(),
            "Quirk {} has empty description",
            q.id
        );
    }
}

#[test]
fn nv12_known_broken_on_original_facecam() {
    assert!(is_format_known_broken(
        ElgatoProduct::Facecam,
        FW_FACECAM_409,
        PixelFormat::Nv12
    ));
}

#[test]
fn nv12_not_known_broken_on_pro() {
    // The Pro was originally suspected of BOGUS_NV12 but on 2026-04-28 NV12
    // was observed to deliver valid frames after a single STREAM_START_RACE
    // recovery cycle. The 0-byte symptom was the race, not a format quirk.
    assert!(!is_format_known_broken(
        ElgatoProduct::FacecamPro,
        FW_PRO_006,
        PixelFormat::Nv12
    ));
}

#[test]
fn h264_not_known_broken_on_pro() {
    // Empirically streams cleanly. We do not yet have a positive 'reliable' label
    // either — that requires its own probe — but it MUST NOT be marked broken.
    assert!(!is_format_known_broken(
        ElgatoProduct::FacecamPro,
        FW_PRO_006,
        PixelFormat::H264
    ));
}

#[test]
fn yu12_not_marked_broken_on_pro() {
    // Pro does not advertise YU12; we have no observation; do not assume.
    assert!(!is_format_known_broken(
        ElgatoProduct::FacecamPro,
        FW_PRO_006,
        PixelFormat::Yu12
    ));
}

#[test]
fn yu12_known_broken_on_original_facecam() {
    assert!(is_format_known_broken(
        ElgatoProduct::Facecam,
        FW_FACECAM_409,
        PixelFormat::Yu12
    ));
}

#[test]
fn no_mjpeg_quirk_only_below_fw_4_0_on_original_facecam() {
    let q300 = applicable_quirks(ElgatoProduct::Facecam, FW_FACECAM_300);
    assert!(q300.iter().any(|q| q.id == "NO_MJPEG_OLD_FW"));

    let q409 = applicable_quirks(ElgatoProduct::Facecam, FW_FACECAM_409);
    assert!(!q409.iter().any(|q| q.id == "NO_MJPEG_OLD_FW"));
}

#[test]
fn no_mjpeg_quirk_does_not_apply_to_pro() {
    let q = applicable_quirks(ElgatoProduct::FacecamPro, FW_PRO_006);
    assert!(!q.iter().any(|q| q.id == "NO_MJPEG_OLD_FW"));
}

#[test]
fn open_close_lockup_is_facecam_only() {
    let q_facecam = applicable_quirks(ElgatoProduct::Facecam, FW_FACECAM_409);
    assert!(q_facecam.iter().any(|q| q.id == "OPEN_CLOSE_LOCKUP"));

    let q_pro = applicable_quirks(ElgatoProduct::FacecamPro, FW_PRO_006);
    assert!(!q_pro.iter().any(|q| q.id == "OPEN_CLOSE_LOCKUP"));
}

#[test]
fn unknown_pid_gets_no_quirks() {
    let q = applicable_quirks(ElgatoProduct::Unknown(0x9999), FW_FACECAM_409);
    assert!(q.is_empty());
}

#[test]
fn pro_has_stream_start_race_quirk() {
    let q = applicable_quirks(ElgatoProduct::FacecamPro, FW_PRO_006);
    let entry = q
        .iter()
        .find(|q| q.id == "PRO_STREAM_START_RACE")
        .expect("PRO_STREAM_START_RACE must apply to FacecamPro");
    assert!(matches!(
        entry.mitigation,
        facecam_common::quirks::QuirkMitigation::RetryStreamOn
    ));
}

#[test]
fn pro_has_usb_reset_destructive_quirk() {
    let q = applicable_quirks(ElgatoProduct::FacecamPro, FW_PRO_006);
    let entry = q
        .iter()
        .find(|q| q.id == "PRO_USB_RESET_DESTRUCTIVE")
        .expect("PRO_USB_RESET_DESTRUCTIVE must apply to FacecamPro");
    assert!(matches!(
        entry.mitigation,
        facecam_common::quirks::QuirkMitigation::AvoidUnlessLastResort
    ));
}

#[test]
fn pro_quirks_do_not_leak_to_facecam() {
    let q = applicable_quirks(ElgatoProduct::Facecam, FW_FACECAM_409);
    assert!(!q.iter().any(|q| q.id == "PRO_STREAM_START_RACE"));
    assert!(!q.iter().any(|q| q.id == "PRO_USB_RESET_DESTRUCTIVE"));
}
