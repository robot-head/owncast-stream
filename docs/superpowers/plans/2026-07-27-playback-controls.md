# Playback Controls and Console Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Ratatui keyboard controls and time status, preserve a live frozen-frame/silent broadcast while paused, and choose media titles from explicit input, embedded metadata, then parsed filenames.

**Architecture:** Split the current graph into an always-playing broadcast pipeline and a separately controlled playback pipeline joined by GStreamer's `inter` elements. Keep title discovery in `media.rs`, terminal state/rendering in a new `ui.rs`, and expose a small synchronous `StreamSession` control surface from `pipeline.rs`.

**Tech Stack:** Rust 2024, GStreamer/gstreamer-rs 0.25, GStreamer Pbutils Discoverer, Ratatui 0.30 with its Crossterm backend, torrent-name-parser 0.12.

## Global Constraints

- Preserve one RTMP publisher connection from lobby through movie playback and shutdown.
- Do not add an async runtime, standalone Crossterm dependency, command framework, configuration layer, or custom widget hierarchy.
- Use test-first RED/GREEN steps. Keep the existing decoder fixture test ignored.
- Keep the existing +3 dB (`1.4125375`) output gain, stream-selection rules, relative-path behavior, and visible fatal errors.
- Hold the current movie frame and select lobby silence while paused.
- Use `FLUSH | KEY_UNIT` seeks clamped to `0..=duration`.

---

## Task 1: Add the three focused dependencies

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] Add the dependencies without enabling Ratatui's unused default widgets:

```toml
gstreamer-pbutils = "0.25"
ratatui = { version = "0.30.2", default-features = false, features = ["crossterm"] }
torrent-name-parser = "0.12.1"
```

- [ ] Refresh the lockfile and prove the dependency set resolves:

Run: `cargo check`

Expected: exit 0; `Cargo.lock` contains `gstreamer-pbutils`, `ratatui`, and `torrent-name-parser`.

- [ ] Commit:

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add playback control dependencies"
```

## Task 2: Discover duration and resolve the media title

**Files:**

- Modify: `src/main.rs`
- Modify: `src/media.rs`

- [ ] Write failing unit tests in `src/media.rs` for the complete title precedence:

```rust
#[test]
fn explicit_title_wins() {
    assert_eq!(
        resolve_title(
            Some("Director's Cut"),
            Some("Embedded"),
            Path::new("Passenger.2024.1080p.BluRay.mkv"),
        ),
        "Director's Cut"
    );
}

#[test]
fn embedded_title_beats_filename() {
    assert_eq!(
        resolve_title(
            None,
            Some("Embedded"),
            Path::new("Passenger.2024.1080p.BluRay.mkv"),
        ),
        "Embedded"
    );
}

#[test]
fn torrent_filename_is_cleaned() {
    assert_eq!(
        resolve_title(None, None, Path::new("Passenger.2024.1080p.BluRay.mkv")),
        "Passenger"
    );
}

#[test]
fn unusable_parser_result_falls_back_to_stem() {
    assert_eq!(
        resolve_title(None, None, Path::new("...mkv")),
        ".."
    );
}
```

Also cover empty explicit and embedded strings as absent.

- [ ] Run the focused tests to confirm RED:

Run: `cargo test media::tests::explicit_title_wins`

Expected: compile failure because `resolve_title` does not exist.

- [ ] Change `Config.title` to `Option<String>` and make parsing preserve only a non-empty explicit CLI title:

```rust
struct Config {
    video: PathBuf,
    subtitles: Option<PathBuf>,
    title: Option<String>,
    stream_key: String,
    title_token: String,
}
```

- [ ] Add the minimal discovery value and title resolver to `media.rs`:

```rust
pub(crate) struct MediaInfo {
    pub(crate) title: String,
    pub(crate) duration: gst::ClockTime,
}

pub(crate) fn resolve_title(
    explicit: Option<&str>,
    embedded: Option<&str>,
    path: &Path,
) -> String
```

Trim each candidate. For the filename candidate, pass `path.file_name()` to
`torrent_name_parser::Metadata::from`, use its non-empty `title()`, then fall
back to `path.file_stem()`.

- [ ] Add synchronous discovery:

```rust
pub(crate) fn discover(
    path: &Path,
    explicit_title: Option<&str>,
) -> Result<MediaInfo, Box<dyn Error>>
```

Initialize GStreamer, convert the absolute path with
`gst::glib::filename_to_uri`, construct
`gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))`, and
call `discover_uri`. Reject an unsuccessful result, non-seekable media, or a
missing/zero duration. Read the first non-empty global/container
`gst::tags::Title` value and call `resolve_title`.

- [ ] Run the media tests:

Run: `cargo test media::tests`

Expected: all title-precedence and existing stream-selection tests pass.

- [ ] Commit:

```bash
git add src/main.rs src/media.rs
git commit -m "feat: discover media title and duration"
```

## Task 3: Add terminal-independent UI state, keys, and rendering

**Files:**

- Create: `src/ui.rs`
- Modify: `src/main.rs`

- [ ] Write failing tests in `src/ui.rs` for:

  - `Enter -> Start` and `q -> Quit` in `Lobby`;
  - Space/arrows ignored in `Lobby`;
  - Space toggles in `Playing` and `Paused`;
  - Left/Right map to `Seek(-30)`/`Seek(30)` after start;
  - unrelated keys are ignored;
  - `format_time` produces `00:00:00`, `01:02:03`, and `123:00:00`;
  - a `ratatui::backend::TestBackend` render contains the title, uppercase state,
    current/duration time, and help text.

Use these small types:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlaybackState { Lobby, Playing, Paused }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command { Start, TogglePause, Seek(i64), Quit }

struct Status<'a> {
    title: &'a str,
    state: PlaybackState,
    position: gst::ClockTime,
    duration: gst::ClockTime,
}
```

- [ ] Add `mod ui;` to `main.rs`, then run the new tests to confirm RED:

Run: `cargo test ui::tests`

Expected: compile failures for the unimplemented key mapping/formatter/renderer.

- [ ] Implement `command_for_key`, `format_time`, and one compact renderer using
Ratatui `Paragraph`/`Line`; do not introduce a custom widget abstraction.

- [ ] Run the UI tests:

Run: `cargo test ui::tests`

Expected: all pass without entering raw mode or touching the real terminal.

- [ ] Commit:

```bash
git add src/main.rs src/ui.rs
git commit -m "feat: add playback status UI"
```

## Task 4: Split playback from the always-live broadcast pipeline

**Files:**

- Modify: `src/pipeline.rs`

- [ ] Extend `REQUIRED_ELEMENTS` with:

```rust
"intervideosink",
"intervideosrc",
"interaudiosink",
"interaudiosrc",
```

- [ ] First write a failing synthetic test that creates:

  - a playback pipeline with live `videotestsrc`/`audiotestsrc` feeding
    `intervideosink`/`interaudiosink`;
  - a broadcast pipeline with the existing lobby sources and selectors,
    `intervideosrc`/`interaudiosrc` movie branches, and `fakesink` outputs;
  - pad probes counting output video buffers and non-silent movie audio.

Start both, select movie, pause only playback, select lobby-silence audio, wait
for several frame intervals, and assert video buffer count continues to grow
while no movie audio reaches the output.

- [ ] Run that single test to confirm RED:

Run: `cargo test pipeline::tests::paused_playback_keeps_video_live_and_audio_silent -- --exact`

Expected: failure because the split builders and inter branches do not exist.

- [ ] Replace `PipelineParts` with two narrowly scoped holders:

```rust
struct BroadcastPipeline {
    pipeline: gst::Pipeline,
    video_selector: gst::Element,
    audio_selector: gst::Element,
    video_lobby_pad: gst::Pad,
    audio_lobby_pad: gst::Pad,
    video_movie_pad: gst::Pad,
    audio_movie_pad: gst::Pad,
}

struct PlaybackPipeline {
    pipeline: gst::Pipeline,
    movie: gst::Element,
    movie_video_sink: gst::Pad,
    movie_audio_sink: gst::Pad,
    movie_subtitle_sink: gst::Pad,
    subtitle_overlay: gst::Element,
}
```

Both holders set their pipeline to `Null` in `Drop`.

- [ ] Move the lobby, selectors, gain, encoders, muxer, and output sink into
`BroadcastPipeline::build`. Replace its movie branches with:

```text
intervideosrc channel=owncast-movie timeout=18446744073709551615
  ! queue max-size-buffers=2 leaky=downstream
  ! video_selector.

interaudiosrc channel=owncast-movie
  ! queue max-size-buffers=8 leaky=downstream
  ! audio_selector.
```

Keep selector sync settings, output caps, encoders, one muxer, one sink, and
`audio_gain volume=1.4125375` unchanged.

- [ ] Move `uridecodebin3`, selected pad routing, subtitle overlay, video
conversion/cropping/scaling, and audio queue into
`PlaybackPipeline::build`. Terminate the selected outputs at:

```text
intervideosink channel=owncast-movie sync=true
interaudiosink channel=owncast-movie sync=true
```

- [ ] Remove the old same-pipeline timestamp-rebase and first-buffer blocking
machinery only after the synthetic inter-pipeline test covers its timing
boundary. The live inter sources now timestamp output on the broadcast
pipeline's clock; adapt the existing timestamp tests to assert that behavior,
and retain the stream-collection selection logic and fatal bus error behavior.

- [ ] Adapt the existing structural tests to inspect the correct holder and add
assertions that:

  - only the broadcast pipeline contains the RTMP/fake output;
  - it starts on both lobby pads;
  - the playback pipeline contains the decoder and inter sinks;
  - both pipelines cleanly return to `Null`.

- [ ] Run the pipeline tests:

Run: `cargo test pipeline::tests`

Expected: all non-ignored tests pass, including the new paused-output test.

- [ ] Commit:

```bash
git add src/pipeline.rs
git commit -m "refactor: separate playback from broadcast"
```

## Task 5: Expose synchronous playback controls and bus processing

**Files:**

- Modify: `src/pipeline.rs`

- [ ] Write failing pure tests for:

```rust
fn seek_target(
    position: gst::ClockTime,
    duration: gst::ClockTime,
    delta_seconds: i64,
) -> gst::ClockTime
```

Cover subtracting past zero, a normal backward seek, a normal forward seek,
and adding past duration.

- [ ] Write a failing synthetic control test that starts on the lobby, invokes
`start`, `pause`, a paused forward seek, and `resume`, asserting state changes,
selector choices, clamped position, and continued broadcast output.

- [ ] Run the focused tests to confirm RED:

Run: `cargo test pipeline::tests::seek_target_`

Then: `cargo test pipeline::tests::session_controls_synthetic_playback -- --exact`

Expected: compile failure for the missing helper/session API.

- [ ] Introduce the sole public control boundary:

```rust
pub(crate) struct StreamSession {
    broadcast: BroadcastPipeline,
    playback: PlaybackPipeline,
    state: PlaybackState,
    duration: gst::ClockTime,
    title: String,
}

impl StreamSession {
    pub(crate) fn new(
        config: &Config,
        media: &MediaInfo,
    ) -> Result<Self, Box<dyn Error>>;
    pub(crate) fn state(&self) -> PlaybackState;
    pub(crate) fn title(&self) -> &str;
    pub(crate) fn duration(&self) -> gst::ClockTime;
    pub(crate) fn position(&self) -> gst::ClockTime;
    pub(crate) fn start(&mut self) -> Result<(), Box<dyn Error>>;
    pub(crate) fn toggle_pause(&mut self) -> Result<(), Box<dyn Error>>;
    pub(crate) fn seek_by(&mut self, seconds: i64) -> Result<(), Box<dyn Error>>;
    pub(crate) fn poll(&mut self) -> Result<SessionEvent, Box<dyn Error>>;
}
```

`new` starts the broadcast pipeline in `Playing`, prerolls playback in
`Paused`, and sets the Owncast title to `Starting soon: {title}`. `start`
switches both selectors to movie, starts playback, updates the Owncast title,
and enters `Playing`.

- [ ] Implement pause/resume ordering:

  - pause: switch broadcast audio to the lobby pad, then set playback to
    `Paused`, then record `Paused`;
  - resume: set playback to `Playing`, switch audio to the movie pad, then
    record `Playing`;
  - video remains on the movie pad after first start.

- [ ] Implement `seek_by` with
`gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT`, using `seek_target`.
Preserve the playback state. When paused, wait for the paused state/preroll
after seeking so the inter-video sink receives the new frozen frame.

- [ ] Make `position` return zero when the playback position query fails.
Make `poll` non-blockingly drain both buses, dispatch stream collections, map
EOS to `SessionEvent::Finished`, and return fatal bus/application errors.

- [ ] Run focused and full pipeline tests:

Run: `cargo test pipeline::tests`

Expected: all non-ignored tests pass.

- [ ] Commit:

```bash
git add src/pipeline.rs
git commit -m "feat: add playback session controls"
```

## Task 6: Run the Ratatui application loop

**Files:**

- Modify: `src/main.rs`
- Modify: `src/ui.rs`
- Modify: `src/pipeline.rs`
- Modify: `tests/stream.rs`

- [ ] Add a failing source-boundary test in `tests/stream.rs` that requires
`main.rs` to invoke `ratatui::run`, requires `ui.rs` to poll/read Ratatui's
re-exported Crossterm events, and continues to reject a direct `crossterm`
dependency. Pure key dispatch is already behavior-tested in Task 3, while
session actions are behavior-tested in Task 5.

- [ ] Run the focused test to confirm RED:

Run: `cargo test terminal_event_loop_uses_ratatui`

Expected: assertion failure because the event loop is not wired.

- [ ] Implement:

```rust
pub(crate) fn run(
    terminal: &mut ratatui::DefaultTerminal,
    session: &mut StreamSession,
) -> Result<(), Box<dyn Error>>
```

Every iteration:

1. call `session.poll()` and exit on `Finished`;
2. draw title/state/position/duration;
3. poll Ratatui's re-exported Crossterm events for at most 100 ms;
4. ignore key releases and resize/mouse/paste events;
5. map a key and call `start`, `toggle_pause`, `seek_by`, or return on `q`.

- [ ] Replace the old `pipeline::run` entry point and stdin thread in `main`:

```rust
let result = Config::parse(args.into_iter()).and_then(|config| {
    let media = media::discover(&config.video, config.title.as_deref())?;
    let mut session = pipeline::StreamSession::new(&config, &media)?;
    ratatui::run(|terminal| ui::run(terminal, &mut session))
});
```

Allow errors to escape the Ratatui closure so terminal restoration occurs
before `main` prints the visible failure. Treat Ctrl-C key events in raw mode
as the same clean exit as `q`.

- [ ] Remove obsolete SIGINT/stdin handoff code and tests. Keep `q` as the
documented clean shutdown path and Ctrl-C as a conventional equivalent.

- [ ] Update `tests/stream.rs` source assertions so they require Ratatui and
pure-Rust metadata parsing while continuing to reject subprocess/FFmpeg
invocation.

- [ ] Run all tests:

Run: `cargo test`

Expected: all tests pass except the one retained ignored decoder test.

- [ ] Commit:

```bash
git add src/main.rs src/ui.rs src/pipeline.rs tests/stream.rs
git commit -m "feat: wire terminal playback controls"
```

## Task 7: Document behavior and correct the actual toolchain floor

**Files:**

- Modify: `README.md`

- [ ] Update the feature list and usage notes with:

  - Ratatui title/state/time display;
  - Enter start, Space pause/resume, Left/Right 30-second seek, `q` quit;
  - paused behavior is frozen movie video with silence while RTMP remains live;
  - title precedence: explicit CLI, embedded metadata, parsed filename, stem;
  - minimum Rust version 1.92 because `gstreamer` 0.25 already requires it.

- [ ] Verify the README contains the actual key contract:

Run: `rg -n "Rust 1\\.92|Enter|Space|Left|Right|30|metadata|filename|frozen|silence" README.md`

Expected: every behavior appears in the output.

- [ ] Commit:

```bash
git add README.md
git commit -m "docs: describe playback controls"
```

## Task 8: Retained regression and release verification

**Files:**

- Modify only if a verification failure exposes a defect in the planned scope.

- [ ] Format and ensure formatting is stable:

Run: `cargo fmt --all -- --check`

Expected: exit 0.

- [ ] Run all tests and verify the decoder fixture remains the only ignored test:

Run: `cargo test --all-targets`

Expected: all runnable unit and integration tests pass; only
`synthetic_handoff_stays_within_50ms` is ignored.

- [ ] Run strict linting:

Run: `cargo clippy --all-targets -- -D warnings`

Expected: exit 0 with no warnings.

- [ ] Build the production binary:

Run: `cargo build --release --locked`

Expected: exit 0.

- [ ] Inspect the final diff and history:

Run: `git diff --check && git status --short && git log --oneline origin/main..HEAD`

Expected: no whitespace errors, a clean worktree, and one intentional commit
per task.

- [ ] If verification exposes a scoped defect, return to its owning task,
add a regression test, make the smallest fix, rerun Task 8, and commit that
specific file set with `fix: preserve playback control invariants`.
