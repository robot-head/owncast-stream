use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

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
fn keeps_one_publisher_and_prefers_embedded_subtitles() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("owncast-stream-test-{suffix}"));
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = root.join("calls");
    let subtitle = root.join("subtitle.srt");
    let video = root.join("movie.mkv");
    fs::write(&subtitle, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();

    let generated = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=s=64x64:r=30:d=2",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo:d=2",
            "-i",
        ])
        .arg(&subtitle)
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:s"])
        .args(["-metadata:s:s:0", "language=eng"])
        .args(["-c:v", "libx264", "-c:a", "aac", "-c:s", "srt", "-y"])
        .arg(&video)
        .status()
        .unwrap();
    assert!(generated.success());

    executable(
        &bin.join("ffmpeg"),
        r#"#!/usr/bin/env bash
set -euo pipefail
args=" $* "
printf '%s\n' "$*" >>"$OWNCAST_TEST_LOG"
if [[ $args == *' rtmp://'* ]]; then
  printf 'role publisher\n' >>"$OWNCAST_TEST_LOG"
  cat >/dev/null
elif [[ $args == *'color=c='* ]]; then
  printf 'role lobby\n' >>"$OWNCAST_TEST_LOG"
  trap 'exit 0' TERM INT
  while dd if=/dev/zero bs=188 count=100 status=none; do :; done
else
  printf 'role movie\n' >>"$OWNCAST_TEST_LOG"
  dd if=/dev/zero bs=188 count=11000 status=none
fi
"#,
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let titles = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&titles);
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = socket.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") && request.ends_with(b"}")
                {
                    break;
                }
            }
            let text = String::from_utf8(request).unwrap();
            received
                .lock()
                .unwrap()
                .push(text.split("\r\n\r\n").nth(1).unwrap().to_owned());
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        }
    });

    let existing_path = std::env::var("PATH").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_owncast-stream"))
        .arg(&video)
        .arg(&subtitle)
        .arg("Movie Night")
        .env("PATH", format!("{}:{existing_path}", bin.display()))
        .env("OWNCAST_TEST_LOG", &log)
        .env(
            "OWNCAST_TITLE_URL",
            format!("http://{address}/api/integrations/streamtitle"),
        )
        .env("OWNCAST_OUTPUT_URL", "rtmp://test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Lobby stopped; switching to movie."));
    assert!(stdout.contains("Movie is live."));
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("role publisher").count(), 1);
    assert_eq!(calls.matches("role lobby").count(), 1);
    assert_eq!(calls.matches("role movie").count(), 1);
    assert_eq!(calls.matches("-loglevel error").count(), 3);
    assert!(calls.contains("subtitles=filename=/dev/fd/3:si=0"));
    let titles = titles.lock().unwrap();
    assert_eq!(titles.len(), 2);
    assert!(titles[0].contains("Starting soon: Movie Night"));
    assert!(titles[1].contains("Movie Night"));
    fs::remove_dir_all(root).unwrap();
}
