# owncast-stream

A small Rust orchestrator for streaming video files to Owncast through FFmpeg.
It keeps one RTMP publisher connected while showing a silent lobby, then
switches to the movie when Enter is pressed.

## Features

- 1920x1080 H.264 video at 30 fps
- AAC audio with voice compression and loudness normalization
- Embedded English subtitles, with embedded or external fallbacks
- Continuous lobby-to-movie handoff without reconnecting viewers
- Owncast title updates through a native Rust HTTP client
- Clean lifecycle messages instead of interleaved FFmpeg logs

## Requirements

- Rust 1.85 or newer
- `ffmpeg` and `ffprobe`
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

The external subtitle file is used only when the video has no embedded
subtitle streams.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```
