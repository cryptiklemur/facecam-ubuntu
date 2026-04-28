use facecam_common::profiles::{Profile, ProfileVideoMode};
use std::collections::HashMap;

#[test]
fn profile_roundtrips_through_toml() {
    let p = Profile {
        name: "test".into(),
        description: "roundtrip".into(),
        video_mode: Some(ProfileVideoMode {
            width: 1920,
            height: 1080,
            fps: 30,
            format: "MJPG".into(),
        }),
        controls: HashMap::from([("brightness".into(), 0)]),
    };
    let s = toml::to_string_pretty(&p).expect("encode");
    let back: Profile = toml::from_str(&s).expect("decode");
    assert_eq!(back.name, p.name);
    assert_eq!(back.controls.get("brightness"), Some(&0));
}
