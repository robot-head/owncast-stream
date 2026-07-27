use std::process::Command;

#[test]
fn usage_errors_exit_with_status_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_owncast-stream"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Usage: owncast-stream VIDEO [SUBTITLES] [TITLE]\n"
    );
}

#[test]
fn media_path_has_no_subprocess_calls() {
    let sources = concat!(
        include_str!("../src/main.rs"),
        include_str!("../src/pipeline.rs"),
        include_str!("../src/media.rs"),
    );
    for forbidden in ["Command::new", "std::process::Command", "ffprobe", "ffmpeg"] {
        assert!(
            !sources.contains(forbidden),
            "media source contains forbidden subprocess call: {forbidden}"
        );
    }
}
