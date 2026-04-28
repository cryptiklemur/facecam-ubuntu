#[test]
#[ignore = "requires Facecam Pro at /dev/video0"]
fn capture_session_streams_mjpg_1080p30() {
    use facecam_common::formats::{PixelFormat, VideoMode};
    use facecam_common::v4l2;
    use facecam_daemon::capture::CaptureSession;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    let f = v4l2::open_device_nonblocking("/dev/video0").expect("open");
    let fd = f.as_raw_fd();
    v4l2::set_format(fd, 1920, 1080, PixelFormat::Mjpeg.to_fourcc()).expect("S_FMT");

    let mode = VideoMode {
        format: PixelFormat::Mjpeg,
        width: 1920,
        height: 1080,
        fps_numerator: 1,
        fps_denominator: 30,
    };
    let mut session = CaptureSession::start(fd, mode).expect("start");
    let mut got_frame = false;
    for _ in 0..2 {
        match session.next_frame(Duration::from_millis(2000)) {
            Ok(frame) => {
                assert!(frame.bytes.len() > 1000, "MJPG frame too small");
                got_frame = true;
                break;
            }
            Err(_) => continue,
        }
    }
    session.stop().expect("stop");
    assert!(got_frame, "Did not get a frame after 2 attempts");
}
