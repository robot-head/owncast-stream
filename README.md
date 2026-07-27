# owncast-stream

A small Rust/GStreamer streamer for Owncast. It keeps one RTMP connection open
while switching from a generated lobby to one movie.

## Features

- 1920x1080 H.264 video at 30 fps
- AAC audio with an 80 Hz high-pass filter and dynamic compression
- Preferred English audio
- Embedded non-SDH English subtitles, with external subtitles used only as a fallback
- Continuous lobby-to-movie handoff without reconnecting viewers
- Owncast title updates through a native Rust HTTP client
- Clean lifecycle messages

## Requirements

- Rust 1.85 or newer
- GStreamer 1.28 development files
- GStreamer base, good, bad, ugly, and libav plugins
- Owncast reachable over RTMP and its integration API

The stream key and title integration token are read from:

```text
/opt/owncast/stream-key
/opt/owncast/title-token
```

## Build and install

```bash
cargo build --release --locked
sudo install -m 0755 target/release/owncast-stream /usr/local/bin/owncast-stream
```

## Usage

```bash
owncast-stream VIDEO [SUBTITLES] [TITLE]
```

The external subtitle file is used only when the video has no non-SDH embedded
English subtitle stream.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```
