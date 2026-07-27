# Rust-Controlled GStreamer Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace all spawned FFmpeg processes with one Rust-controlled GStreamer pipeline that keeps one RTMP connection while switching from the lobby to an English-audio movie with synchronized subtitles.

**Architecture:** Keep argument parsing, secret loading, and Owncast title HTTP calls in `main.rs`. Put the pure stream-choice policy in `media.rs`, and put GStreamer construction, dynamic stream linking, clocked selector switching, and bus handling in `pipeline.rs`. Build one pipeline containing both sources, two clock-synchronized input selectors, one encoder/mux/output chain, and no media subprocesses.

**Tech Stack:** Rust 2024, `gstreamer` 0.25, GStreamer 1.28 system libraries/plugins, `serde`, and `ureq`.

## Global Constraints

- The application must not spawn `ffmpeg`, `ffprobe`, `gst-launch`, or another media subprocess.
- Use one GStreamer pipeline, one common H.264/AAC encoder path, one FLV muxer, and one RTMP connection.
- Output is 1920x1080 at 30 fps, H.264 at 6 Mbps with a 60-frame GOP, and 48 kHz stereo AAC at 192 kbps.
- Prefer English audio, then the stream marked for default selection.
- Prefer non-SDH embedded English subtitles, then the supplied external SRT, then no subtitles.
- Retain the 80 Hz high-pass filter and dynamic compression; do not add loudness normalization.
- Support one lobby-to-movie switch and one movie per invocation.
- Do not add fades, seeking, playlists, recording, retries, hardware encoding, custom codecs, custom FLV muxing, or custom RTMP code.

---

## File Map

- `Cargo.toml`: replace process/probe dependencies with the official GStreamer binding.
- `src/media.rs`: pure stream metadata model and deterministic audio/subtitle selection.
- `src/pipeline.rs`: GStreamer preflight, pipeline construction, stream discovery/linking, handoff, EOS, and error handling.
- `src/main.rs`: retain CLI/secrets/title calls and delegate media work to `pipeline::run`.
- `tests/stream.rs`: retain CLI/title integration coverage and prove the binary never resolves an `ffmpeg` executable.
- `README.md`: document GStreamer runtime packages and the unchanged command line.
- `test-owncast-stream.sh`: add the smallest release-binary dependency check.

### Task 1: Add a pure stream-selection policy

**Files:**

- Create: `src/media.rs`
- Modify: `src/main.rs`

**Interfaces:**

- Produces: `StreamCandidate`, `StreamKind`, `Selection`, `SubtitleSource`, and `select_streams(&[StreamCandidate], Option<&Path>) -> Result<Selection, String>`.
- Consumed by Task 3 when a GStreamer `StreamCollection` arrives.

- [ ] **Step 1: Write failing selection tests**

Add `src/media.rs` with the public data shapes and tests first:

```rust
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StreamKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StreamCandidate {
    pub id: String,
    pub kind: StreamKind,
    pub language: Option<String>,
    pub is_default: bool,
    pub is_sdh: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SubtitleSource {
    Embedded(String),
    External(PathBuf),
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    pub video_id: String,
    pub audio_id: String,
    pub subtitle: SubtitleSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(id: &str, kind: StreamKind, language: Option<&str>, default: bool) -> StreamCandidate {
        StreamCandidate {
            id: id.into(),
            kind,
            language: language.map(str::to_owned),
            is_default: default,
            is_sdh: false,
        }
    }

    fn base() -> Vec<StreamCandidate> {
        vec![
            stream("video", StreamKind::Video, None, true),
            stream("ita", StreamKind::Audio, Some("ita"), true),
            stream("eng", StreamKind::Audio, Some("eng"), false),
        ]
    }

    #[test]
    fn prefers_english_audio_over_default() {
        assert_eq!(select_streams(&base(), None).unwrap().audio_id, "eng");
    }

    #[test]
    fn falls_back_to_default_audio() {
        let streams = vec![
            stream("video", StreamKind::Video, None, true),
            stream("ita", StreamKind::Audio, Some("ita"), true),
        ];
        assert_eq!(select_streams(&streams, None).unwrap().audio_id, "ita");
    }

    #[test]
    fn prefers_non_sdh_english_subtitles() {
        let mut streams = base();
        let mut sdh = stream("eng-sdh", StreamKind::Subtitle, Some("eng"), true);
        sdh.is_sdh = true;
        streams.push(sdh);
        streams.push(stream("eng-dialogue", StreamKind::Subtitle, Some("eng"), false));
        assert_eq!(
            select_streams(&streams, Some(Path::new("fallback.srt")))
                .unwrap()
                .subtitle,
            SubtitleSource::Embedded("eng-dialogue".into())
        );
    }

    #[test]
    fn falls_back_to_external_then_none() {
        let streams = base();
        assert_eq!(
            select_streams(&streams, Some(Path::new("fallback.srt")))
                .unwrap()
                .subtitle,
            SubtitleSource::External("fallback.srt".into())
        );
        assert_eq!(
            select_streams(&streams, None).unwrap().subtitle,
            SubtitleSource::None
        );
    }

    #[test]
    fn rejects_missing_video_or_supported_audio() {
        assert_eq!(
            select_streams(&[], None).unwrap_err(),
            "Movie has no video stream"
        );
        let video_only = vec![stream("video", StreamKind::Video, None, true)];
        assert_eq!(
            select_streams(&video_only, None).unwrap_err(),
            "Movie has no English or default audio stream"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test media::tests -- --nocapture
```

Expected: compilation fails because `select_streams` is not defined.

- [ ] **Step 3: Implement the minimal selection function**

Add:

```rust
pub(crate) fn select_streams(
    streams: &[StreamCandidate],
    external: Option<&Path>,
) -> Result<Selection, String> {
let video = streams
    .iter()
    .find(|stream| stream.kind == StreamKind::Video && stream.is_default)
    .or_else(|| streams.iter().find(|stream| stream.kind == StreamKind::Video))
    .ok_or_else(|| "Movie has no video stream".to_owned())?;
let audio = streams
    .iter()
    .find(|stream| {
        stream.kind == StreamKind::Audio
            && matches!(stream.language.as_deref(), Some("eng" | "en"))
    })
    .or_else(|| {
        streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Audio && stream.is_default)
    })
    .ok_or_else(|| "Movie has no English or default audio stream".to_owned())?;
let subtitle = streams
    .iter()
    .find(|stream| {
        stream.kind == StreamKind::Subtitle
            && matches!(stream.language.as_deref(), Some("eng" | "en"))
            && !stream.is_sdh
    })
    .map(|stream| SubtitleSource::Embedded(stream.id.clone()))
    .or_else(|| external.map(|path| SubtitleSource::External(path.to_owned())))
    .unwrap_or(SubtitleSource::None);

Ok(Selection {
    video_id: video.id.clone(),
    audio_id: audio.id.clone(),
    subtitle,
})
}
```

In `src/main.rs`, add `mod media;`. Keep the old process implementation and
its `ffprobe` and `libc` dependencies until the working pipeline replaces it
in Task 3.

- [ ] **Step 4: Run the focused and full tests**

Run:

```bash
cargo test media::tests -- --nocapture
cargo test
```

Expected: all five media tests and the existing process integration test pass.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/media.rs
git commit -m "refactor: add native stream selection"
```

### Task 2: Build and preflight the single pipeline

**Files:**

- Modify: `Cargo.toml`
- Create: `src/pipeline.rs`
- Modify: `src/main.rs`

**Interfaces:**

- Produces: `required_elements() -> &'static [&'static str]`, `missing_elements(&[&str]) -> Vec<String>`, and `PipelineParts::build(&Config, &str) -> Result<Self, Box<dyn Error>>`.
- Consumed by Task 3's runtime controller.

- [ ] **Step 1: Write the failing element-preflight test**

Add the binding to `Cargo.toml`:

```toml
gstreamer = "0.25"
```

Then start `src/pipeline.rs` with:

```rust
use gstreamer as gst;
use gst::prelude::*;
use std::error::Error;

use crate::{error, Config};

const REQUIRED_ELEMENTS: &[&str] = &[
    "uridecodebin3",
    "videotestsrc",
    "audiotestsrc",
    "textoverlay",
    "subtitleoverlay",
    "subparse",
    "queue",
    "input-selector",
    "videoconvert",
    "aspectratiocrop",
    "videoscale",
    "videorate",
    "audioconvert",
    "audioresample",
    "audiocheblimit",
    "audiodynamic",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "flvmux",
    "rtmpsink",
];

pub(crate) fn required_elements() -> &'static [&'static str] {
    REQUIRED_ELEMENTS
}

fn missing_elements(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .map(|name| (*name).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_lists_every_missing_element() {
        gst::init().unwrap();
        assert_eq!(
            missing_elements(&["fakesink", "owncast-element-that-does-not-exist"]),
            vec!["owncast-element-that-does-not-exist"]
        );
    }
}
```

Add `mod pipeline;` to `src/main.rs` so the module tests compile. The module is
not called by production until Task 3.

- [ ] **Step 2: Run the test to verify the host dependency is visible**

Run:

```bash
cargo test pipeline::tests::preflight_lists_every_missing_element -- --nocapture
```

Expected before host packages are installed: build failure from `gstreamer-1.0.pc` not being found. Install the approved system prerequisites:

```bash
sudo apt-get update
sudo apt-get install -y \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-tools \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav
```

Run the focused test again. Expected: PASS.

- [ ] **Step 3: Construct the static lobby, selector, encoder, mux, and sink graph**

Add `PipelineParts` and build the static graph with `gst::parse::launch`. The graph must be exactly one pipeline:

```rust
struct PipelineParts {
    pipeline: gst::Pipeline,
    movie: gst::Element,
    video_selector: gst::Element,
    audio_selector: gst::Element,
    movie_video_sink: gst::Pad,
    movie_audio_sink: gst::Pad,
    subtitle_overlay: gst::Element,
}
```

Use these production element settings:

```text
videotestsrc is-live=true pattern=black
  ! video/x-raw,width=1920,height=1080,framerate=30/1
  ! textoverlay text="PLEASE WAIT" font-desc="DejaVu Sans 96"
      valignment=center halignment=center ypad=80
  ! textoverlay text="The movie will begin shortly"
      font-desc="DejaVu Sans 42" color=0xb8c1d9ff
      valignment=center halignment=center ypad=70
  ! queue max-size-buffers=2 leaky=downstream
  ! video_selector.sink_0

audiotestsrc is-live=true wave=silence
  ! audio/x-raw,rate=48000,channels=2
  ! queue max-size-buffers=8 leaky=downstream
  ! audio_selector.sink_0

input-selector name=video_selector sync-streams=true
    sync-mode=clock cache-buffers=true drop-backwards=true
  ! videoconvert
  ! video/x-raw,format=I420,width=1920,height=1080,framerate=30/1
  ! x264enc bitrate=6000 key-int-max=60 bframes=0
      tune=zerolatency speed-preset=medium
  ! h264parse config-interval=1
  ! queue
  ! mux.

input-selector name=audio_selector sync-streams=true
    sync-mode=clock cache-buffers=true drop-backwards=true
  ! audioconvert
  ! audioresample
  ! audio/x-raw,format=F32LE,rate=48000,channels=2
  ! audiocheblimit mode=high-pass cutoff=80 poles=4
  ! audiodynamic mode=compressor characteristics=soft-knee
      threshold=0.125 ratio=2.0
  ! avenc_aac bitrate=192000
  ! aacparse
  ! queue
  ! mux.

flvmux name=mux streamable=true
  ! rtmpsink name=output
```

Create `uridecodebin3` programmatically so paths never need parse-string escaping:

```rust
let movie = gst::ElementFactory::make("uridecodebin3")
    .name("movie")
    .property("uri", gst::glib::filename_to_uri(&config.video, None)?)
    .build()?;
pipeline.add(&movie)?;
output.set_property("location", output_url);
```

Add and link the movie raw-video chain:

```text
queue max-size-buffers=2
  ! videoconvert
  ! aspectratiocrop aspect-ratio=16/9
  ! videoscale
  ! videorate
  ! video/x-raw,width=1920,height=1080,framerate=30/1
  ! subtitleoverlay name=movie_subtitles
  ! queue max-size-buffers=2
  ! video_selector.sink_1
```

Add and link the movie raw-audio chain:

```text
queue max-size-buffers=8
  ! audio_selector.sink_1
```

Set both selectors' `active-pad` to `sink_0`. Request and retain `sink_1` for dynamic movie linking. `Drop` for `PipelineParts` must call `pipeline.set_state(gst::State::Null)`.

- [ ] **Step 4: Add a structural pipeline test**

Add a test-only `build_with_sink(config, sink)` helper used by production with `rtmpsink` and by the test with `fakesink`. The test must assert:

```rust
assert_eq!(parts.pipeline.iterate_sinks().count(), 1);
assert_eq!(
    parts.video_selector.property::<gst::Pad>("active-pad").name(),
    "sink_0"
);
assert_eq!(
    parts.audio_selector.property::<gst::Pad>("active-pad").name(),
    "sink_0"
);
```

Run:

```bash
cargo test pipeline::tests -- --nocapture
```

Expected: preflight and structural tests pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/pipeline.rs
git commit -m "feat: build GStreamer stream pipeline"
```

### Task 3: Select streams and perform the clocked handoff

**Files:**

- Modify: `src/pipeline.rs`
- Modify: `src/main.rs`

**Interfaces:**

- Consumes: `media::select_streams`, `Selection`, and `SubtitleSource`.
- Produces: `pipeline::run(&Config) -> Result<(), Box<dyn Error>>`.

- [ ] **Step 1: Add failing timestamp and bus-error tests**

Add pure helpers and tests:

```rust
fn running_time_offset(boundary: gst::ClockTime, first_pts: gst::ClockTime) -> i64 {
    boundary.nseconds() as i64 - first_pts.nseconds() as i64
}

fn bus_error(message: &gst::MessageRef) -> Option<String> {
    match message.view() {
        gst::MessageView::Error(error) => Some(format!(
            "{}: {} ({})",
            error
                .src()
                .map(|source| source.path_string())
                .unwrap_or_else(|| "unknown".into()),
            error.error(),
            error.debug().unwrap_or_default()
        )),
        _ => None,
    }
}

#[test]
fn rebases_first_movie_pts_to_boundary() {
    assert_eq!(
        running_time_offset(
            gst::ClockTime::from_seconds(30),
            gst::ClockTime::from_mseconds(250)
        ),
        29_750_000_000
    );
}

#[test]
fn formats_originating_bus_element() {
    let source = gst::ElementFactory::make("fakesrc").name("broken-source").build().unwrap();
    let message = gst::message::Error::builder(gst::StreamError::Failed, "decode failed")
        .src(&source)
        .debug("fixture failure")
        .build();
    assert!(bus_error(&message).unwrap().contains("broken-source"));
}
```

- [ ] **Step 2: Run the focused tests to verify they fail**

Run:

```bash
cargo test pipeline::tests::rebases_first_movie_pts_to_boundary -- --nocapture
cargo test pipeline::tests::formats_originating_bus_element -- --nocapture
```

Expected: compilation fails because both helpers are missing.

- [ ] **Step 3: Map `StreamCollection` metadata and request only chosen streams**

When the movie element posts `MessageView::StreamCollection`, map each `gst::Stream` to `StreamCandidate`:

```rust
fn candidate(stream: &gst::Stream) -> Option<StreamCandidate> {
    let kind = if stream.stream_type().contains(gst::StreamType::VIDEO) {
        StreamKind::Video
    } else if stream.stream_type().contains(gst::StreamType::AUDIO) {
        StreamKind::Audio
    } else if stream.stream_type().contains(gst::StreamType::TEXT) {
        StreamKind::Subtitle
    } else {
        return None;
    };
    let tags = stream.tags();
    let language = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::LanguageCode>())
        .map(|tag| tag.get().to_ascii_lowercase());
    let title = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::Title>())
        .map(|tag| tag.get().to_ascii_lowercase())
        .unwrap_or_default();
    Some(StreamCandidate {
        id: stream.stream_id()?.to_string(),
        kind,
        language,
        is_default: stream.stream_flags().contains(gst::StreamFlags::SELECT),
        is_sdh: title.contains("sdh") || title.contains("hearing impaired"),
    })
}
```

Call `select_streams`, collect the selected video/audio/embedded-subtitle IDs, and send:

```rust
movie.send_event(gst::event::SelectStreams::new(selected_ids))
```

If selection fails, leave both selectors on the lobby pads and print the returned reason.

For `SubtitleSource::External(path)`, add `filesrc ! subparse ! queue` and link its source to `movie_subtitles.subtitle_sink`. For `SubtitleSource::None`, set `movie_subtitles.silent=true`.

- [ ] **Step 4: Link only selected decoded pads and capture their first timestamps**

In `movie.connect_pad_added`, read the sticky `StreamStart` event's stream ID. Link only the selected video, audio, and optional subtitle IDs. Install a `BLOCK_DOWNSTREAM | BUFFER` probe on each movie queue's source pad. Each probe records the first buffer PTS in shared state and remains installed until both audio and video are ready.

Use one `Mutex<ReadyState>` and one `Condvar`; do not add an async runtime:

```rust
#[derive(Default)]
struct ReadyState {
    video_pts: Option<gst::ClockTime>,
    audio_pts: Option<gst::ClockTime>,
    enter_pressed: bool,
    failure: Option<String>,
}
```

Spawn one standard-library thread that blocks on `stdin().read_line`. Its only action is setting `enter_pressed=true` and notifying the condition variable.

- [ ] **Step 5: Switch both selectors on one clock callback**

After Enter and both first timestamps are present:

1. Read `clock.time() - pipeline.base_time()` as current running time.
2. Set `boundary` to the next 30 fps frame boundary.
3. Set each movie selector pad offset with `running_time_offset(boundary, first_pts)`.
4. Schedule one single-shot pipeline-clock callback for `pipeline.base_time() + boundary`.
5. In that one callback, set both selectors' `active-pad` to their movie pads, remove both blocking probes, update the Owncast title to `config.title`, and print `Movie is live.`.

The next-frame calculation is:

```rust
fn next_frame_boundary(now: gst::ClockTime) -> gst::ClockTime {
    const FRAME_NS: u64 = 1_000_000_000 / 30;
    gst::ClockTime::from_nseconds(((now.nseconds() / FRAME_NS) + 1) * FRAME_NS)
}
```

This is a clock boundary, not a sleep or byte-count heuristic.

- [ ] **Step 6: Handle EOS, errors, and Ctrl-C in the bus loop**

Implement `pipeline::run(&Config)`:

- Initialize GStreamer and preflight all required elements before setting the first title.
- Set `Starting soon: TITLE`.
- Set the pipeline to `Playing` and print `Lobby is live. Press Enter to start "TITLE"...`.
- On a movie decode/preroll error before switching, retain the lobby and continue waiting for Ctrl-C.
- On encoder, muxer, or RTMP errors, return the formatted element/debug error.
- On movie EOS after switching, send EOS through the common encoder/mux path, wait for pipeline EOS, and return success.
- Register SIGINT without another crate:

```rust
gst::glib::unix_signal_add(gst::glib::Priority::DEFAULT, 2, move || {
    bus.post(&gst::message::Application::builder(
        gst::Structure::builder("owncast-interrupt").build(),
    ).build()).expect("pipeline bus accepts interrupt");
    gst::glib::ControlFlow::Break
});
```

- On SIGINT, post an application message that makes the bus loop exit; `PipelineParts::drop` sets the pipeline to `Null`.

Delete `Media`, `select_subtitle`, all `Command` builders, PID guards, relay
code, and their process/Unix imports. Remove `ffprobe` and `libc` from
`Cargo.toml`. Replace the old `Media::probe` and `run(config, media)` call in
`main` with:

```rust
let result = Config::parse(args.into_iter()).and_then(|config| pipeline::run(&config));
```

- [ ] **Step 7: Run all checks**

Run:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
rg -n 'Command::new|std::process::Command|ffprobe|ffmpeg|PREFIX_BYTES|io::copy' src Cargo.toml
```

Expected: format, tests, and Clippy pass; the search returns no matches.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/pipeline.rs
git commit -m "feat: switch lobby to movie in process"
```

### Task 4: Replace process-mocking coverage and document deployment

**Files:**

- Modify: `src/pipeline.rs`
- Modify: `tests/stream.rs`
- Modify: `README.md`
- Modify: `test-owncast-stream.sh`

**Interfaces:**

- Verifies the unchanged CLI/title boundary and the release deployment.
- No new production interface.

- [ ] **Step 1: Replace the FFmpeg mock integration test**

Keep `usage_errors_exit_with_status_two`. Delete
`keeps_one_publisher_and_prefers_embedded_subtitles` and its executable,
fixture-generation, socket-server, and temporary-directory helpers. Replace
it with a compile-time source guard:

```rust
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
```

The pipeline module tests cover construction, selection, timestamp rebasing,
and bus errors in process.

- [ ] **Step 2: Add the synthetic synchronization regression test**

Add an ignored in-process test in `src/pipeline.rs` named
`synthetic_handoff_stays_within_50ms`. It must create:

- 30 fps video frames whose pixels change at a known one-second boundary.
- A 48 kHz audio beep beginning at that same boundary.
- One subtitle cue beginning at that same boundary.
- The production selectors, encoders, muxer, and a temporary `filesink`.

Decode the resulting FLV in process with `uridecodebin`, record the first
changed video PTS, beep PTS, and subtitle-visible video PTS, and assert:

```rust
assert!(video_pts.nseconds().abs_diff(audio_pts.nseconds()) <= 50_000_000);
assert!(video_pts.nseconds().abs_diff(subtitle_pts.nseconds()) <= 50_000_000);
```

Run:

```bash
cargo test synthetic_handoff_stays_within_50ms -- --ignored --nocapture
```

Expected: PASS.

- [ ] **Step 3: Update requirements and behavior documentation**

Change the README opening to:

```markdown
A small Rust/GStreamer streamer for Owncast. It keeps one RTMP connection open
while switching from a generated lobby to one movie.
```

Replace the FFmpeg requirements with:

```markdown
- Rust 1.85 or newer
- GStreamer 1.28 development files
- GStreamer base, good, bad, ugly, and libav plugins
- Owncast reachable over RTMP and its integration API
```

Change the audio feature to “AAC audio with an 80 Hz high-pass filter and dynamic compression.” State that English audio is preferred and that external subtitles are used only when no non-SDH embedded English subtitle exists.

- [ ] **Step 4: Extend the installed-binary check**

Append to `test-owncast-stream.sh`:

```sh
if ldd "$project_dir/target/release/owncast-stream" | grep -q 'not found'; then
    echo "release binary has missing shared libraries" >&2
    exit 1
fi
```

- [ ] **Step 5: Run repository verification**

Run:

```bash
cargo fmt --check
cargo test
cargo test synthetic_handoff_stays_within_50ms -- --ignored --nocapture
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
./test-owncast-stream.sh
```

Expected: every command exits zero.

- [ ] **Step 6: Commit**

```bash
git add README.md src/pipeline.rs test-owncast-stream.sh tests/stream.rs
git commit -m "test: cover native media pipeline"
```

### Task 5: Deploy and validate the real Owncast boundary

**Files:**

- Modify: `/usr/local/bin/owncast-stream` by installation only
- No repository source changes

**Interfaces:**

- Consumes the release binary and the existing `/opt/owncast` credentials/routes.
- Produces live evidence for one RTMP connection, title updates, English audio, subtitle timing, SSO, and health.

- [ ] **Step 1: Install the verified release binary**

Run:

```bash
sudo install -m 0755 target/release/owncast-stream /usr/local/bin/owncast-stream
cmp --silent target/release/owncast-stream /usr/local/bin/owncast-stream
```

Expected: `cmp` exits zero.

- [ ] **Step 2: Re-run the committed synthetic clock test**

Run:

```bash
cargo test synthetic_handoff_stays_within_50ms -- --ignored --nocapture
```

Expected: PASS.

- [ ] **Step 3: Run the current multilingual movie through Owncast**

Start:

```bash
validation_since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
owncast-stream \
  "/opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.mkv" \
  "/opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.en.srt" \
  "Passenger"
```

After the lobby connects, capture:

```bash
lobby_connect_time=$(curl -fsS http://127.0.0.1:8081/api/status | jq -r .lastConnectTime)
```

Press Enter once. After `Movie is live.`, capture:

```bash
movie_connect_time=$(curl -fsS http://127.0.0.1:8081/api/status | jq -r .lastConnectTime)
test "$lobby_connect_time" = "$movie_connect_time"
test "$(docker logs --since "$validation_since" owncast 2>&1 |
  grep -c 'Inbound stream connected')" -eq 1
```

Expected: both checks pass and no second RTMP connection appears.

- [ ] **Step 4: Verify retained packet timing and selected content**

Read the retained Owncast HLS output with GStreamer's discover/decode APIs, not `ffprobe`. Record the first matching movie video frame, English dialogue audio buffer, and burned English subtitle frame. Expected:

- English audio is present; the Italian default track is absent.
- The burned subtitle matches the English dialogue.
- Audio/video and subtitle/video PTS deltas are each at most 50 ms.

- [ ] **Step 5: Verify title, auth, and health**

Capture `streamTitle` from `http://127.0.0.1:8081/api/status` during the lobby
and after the switch. Expected values are `Starting soon: Passenger` and
`Passenger`. Then run:

```bash
curl -fsS http://127.0.0.1:8081/api/status >/dev/null
redirect=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' \
  https://video.djspacecat.com/admin/)
case "$redirect" in
  "302 https://auth.djspacecat.com/"*) ;;
  *) echo "unexpected admin redirect: $redirect" >&2; exit 1 ;;
esac
```

Expected: the loopback status call succeeds and the protected admin route
redirects to `auth.djspacecat.com`.

- [ ] **Step 6: Final clean-tree verification**

Stop the test stream cleanly, then run:

```bash
git status --short
cargo test
cargo clippy --all-targets -- -D warnings
```

Expected: no uncommitted source changes and all checks pass.
