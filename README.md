# owncast-stream

A small Rust/GStreamer streamer for Owncast. It keeps one RTMP connection open
while switching from a generated lobby to one movie.

![TUI Demo](./docs/demo.gif)

## Features

- 1920x1080 H.264 video at 30 fps
- AAC audio with an 80 Hz high-pass filter and dynamic compression
- Preferred English audio
- Embedded non-SDH English subtitles, with external subtitles used only as a fallback
- Continuous lobby-to-movie handoff without reconnecting viewers
- Pause with a frozen movie frame and silence while the RTMP stream stays live
- 30-second forward and backward seeking
- Amber projection-console TUI with playback status, gain, and stereo peak meters
- Owncast title updates through a native Rust HTTP client
- Clean lifecycle messages

## Requirements

- Rust 1.92 or newer
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
owncast-stream [OPTIONS] VIDEO [SUBTITLES] [TITLE]
```

By default, the streamer publishes to the local Owncast instance and reads its
credentials from the files listed above. To publish to a remote instance,
provide its RTMP publish URL, stream-title integration endpoint, and credentials:

```bash
owncast-stream \
  --rtmp-url rtmp://owncast.example/live \
  --api-url https://owncast.example/api/integrations/streamtitle \
  --stream-key "$STREAM_KEY" \
  --api-key "$TITLE_TOKEN" \
  movie.mkv
```

The RTMP URL should omit the stream key; `owncast-stream` appends the value of
`--stream-key`. All four options may also be used independently, so a remote
server can still use credentials read from the default files.

The external subtitle file is used only when the video has no non-SDH embedded
English subtitle stream.

Controls:

- Enter starts the movie from the lobby.
- Space pauses or resumes playback.
- Left and Right seek backward or forward 30 seconds.
- Up and Down adjust gain by 1 dB from -12 dB to +12 dB.
- `q` or Ctrl-C quits.

The stereo VU meter shows the post-gain signal from -60 dB to 0 dB. Peak
markers hold for 3 seconds, then decay at 12 dB per second.

When `TITLE` is omitted, the title comes from embedded media metadata, then a
cleaned filename from `torrent-name-parser`, then the raw filename stem.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```
