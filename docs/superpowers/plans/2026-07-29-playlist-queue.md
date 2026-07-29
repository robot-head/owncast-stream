# Playlist Queue and File Chooser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a mutable multi-video playlist, repeatable startup queue arguments, in-terminal file selection, automatic track advancement, and Previous/Next controls while preserving one Owncast publisher connection.

**Architecture:** Keep `BroadcastPipeline` alive for the process lifetime and replace only the track-specific `PlaybackPipeline` plus `AudioBridge`. Store discovered media entries and playlist indices in `StreamSession`; keep filesystem browsing and focus state in `ui.rs`.

**Tech Stack:** Rust 2024, standard library filesystem APIs, GStreamer/gstreamer-rs 0.25, GStreamer Pbutils Discoverer, Ratatui 0.30 with its Crossterm backend.

## Global Constraints

- Preserve `owncast-stream [OPTIONS] VIDEO [SUBTITLES] [TITLE]`.
- Add repeatable `--queue VIDEO`; queued and chooser-added entries use automatic title and embedded-subtitle discovery.
- Keep one RTMP publisher connection alive through every track transition and the final lobby.
- Keep the active playlist entry locked. Do not move another entry across it.
- Manual Previous/Next restarts the adjacent entry at zero and preserves Playing/Paused state.
- Natural end-of-stream starts the next entry Playing; final end-of-stream returns to the lobby.
- Retain Left/Right 30-second seeking and Up/Down 1 dB gain adjustment.
- The approved TUI is the persistent full-width lower playlist with compact key help.
- Implement the chooser with `std::fs::read_dir`; add no dependency.
- Runtime chooser discovery failures remain visible without stopping playback or changing the playlist.
- Do not add mouse support, search, persistence, multi-select, recursive folder addition, preloading, rollback machinery, an async runtime, or a command framework.
- Keep all existing stream selection, subtitle, pause, seek, audio bridge, no-subprocess, and cleanup behavior.

## File Map

- `src/main.rs`: parse `--queue`, resolve startup paths, discover every startup entry, and pass the startup directory to the TUI.
- `src/media.rs`: expand `MediaInfo` into the complete immutable playlist entry and keep discovery/path-specific errors in one place.
- `src/pipeline.rs`: own pure playlist mutation, the optional active playback runtime, track replacement, EOF advancement, and title updates.
- `src/ui.rs`: own playback/playlist/chooser focus, key mapping, chooser navigation, playlist rendering, and inline chooser errors.
- `tests/stream.rs`: keep the public usage text and no-subprocess contract current.
- `README.md`: document queue syntax, playlist behavior, chooser controls, and track controls.

---

### Task 1: Parse and discover the startup queue

**Files:**

- Modify: `src/main.rs:16-152`
- Modify: `src/media.rs:41-106`
- Modify: `tests/stream.rs:3-18`

**Interfaces:**

- Produces: `Config::queued_videos: Vec<PathBuf>` and `Config::startup_dir: PathBuf`.
- Produces: `media::MediaInfo { path, subtitles, title, duration }`.
- Produces: `media::discover(path, subtitles, explicit_title) -> Result<MediaInfo, Box<dyn Error>>`.
- Consumes later: `StreamSession::new(&Config, Vec<MediaInfo>)` and `ui::run(..., &Path)`.

- [ ] **Step 1: Add failing parser tests for repeatable queue entries**

Add to `src/main.rs` tests:

```rust
fn parse_with_media(arguments: &[&str]) -> Config {
    Config::parse(
        arguments
            .iter()
            .copied()
            .map(str::to_owned),
    )
    .unwrap()
}

#[test]
fn parses_repeatable_queue_in_argument_order() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let first = root.join("Cargo.toml");
    let second = root.join("README.md");
    let third = root.join("Cargo.lock");
    let config = parse_with_media(&[
        "owncast-stream",
        "--stream-key", "key",
        "--api-key", "token",
        "--queue", second.to_str().unwrap(),
        "--queue", third.to_str().unwrap(),
        first.to_str().unwrap(),
    ]);

    assert_eq!(config.video, first);
    assert_eq!(config.queued_videos, vec![second, third]);
}

#[test]
fn queue_requires_a_nonempty_value() {
    let failure = Config::parse(
        ["owncast-stream", "--queue"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap_err();

    assert!(failure.to_string().contains("Missing value for --queue"));
}
```

- [ ] **Step 2: Run the parser test to verify RED**

Run:

```bash
cargo test tests::parses_repeatable_queue_in_argument_order -- --exact
```

Expected: compile failure because `Config::queued_videos` does not exist.

- [ ] **Step 3: Extend the usage text and parser without changing positional semantics**

Change `USAGE` and `Config` in `src/main.rs` to include:

```rust
const USAGE: &str = "Usage: owncast-stream [OPTIONS] VIDEO [SUBTITLES] [TITLE]\n\
Options:\n\
  --queue VIDEO       Append a video to the startup playlist (repeatable)\n\
  --rtmp-url URL      RTMP publish URL without the stream key\n\
  --api-url URL       Owncast stream-title integration endpoint\n\
  --stream-key KEY    Stream key (defaults to /opt/owncast/stream-key)\n\
  --api-key KEY       Integration token (defaults to /opt/owncast/title-token)";

struct Config {
    startup_dir: PathBuf,
    video: PathBuf,
    subtitles: Option<PathBuf>,
    title: Option<String>,
    queued_videos: Vec<PathBuf>,
    stream_key: String,
    title_token: String,
    rtmp_url: String,
    title_url: String,
}
```

Collect `--queue` values in a `Vec<String>` during option parsing. After
`env::current_dir()`, resolve them with the existing `resolve_media_path`:

```rust
let queued_videos = queued_values
    .iter()
    .map(|value| resolve_media_path(&cwd, value, "queued video"))
    .collect::<Result<Vec<_>, _>>()?;
```

Always append queued values after the positional first entry, regardless of
where `--queue` appears among options.

- [ ] **Step 4: Expand the discovered media value and add a path-specific test**

Change `MediaInfo` and `discover` in `src/media.rs`:

```rust
#[derive(Clone, Debug)]
pub(crate) struct MediaInfo {
    pub(crate) path: PathBuf,
    pub(crate) subtitles: Option<PathBuf>,
    pub(crate) title: String,
    pub(crate) duration: gst::ClockTime,
}

pub(crate) fn discover(
    path: &Path,
    subtitles: Option<PathBuf>,
    explicit_title: Option<&str>,
) -> Result<MediaInfo, Box<dyn Error>>
```

Keep the existing duration/title discovery, clone `path` into the result, and
wrap discovery failures once:

```rust
let discovered = Discoverer::new(gst::ClockTime::from_seconds(10))?
    .discover_uri(&uri)
    .map_err(|failure| error(format!(
        "Cannot discover video {}: {failure}",
        path.display()
    )))?;
```

Add this pure constructor assertion to the existing discovery validation test
after extracting a private `media_info` helper if needed:

```rust
let entry = MediaInfo {
    path: PathBuf::from("/shows/Passenger.mkv"),
    subtitles: Some(PathBuf::from("/shows/Passenger.srt")),
    title: "Passenger".into(),
    duration: gst::ClockTime::from_seconds(60),
};
assert_eq!(entry.path, PathBuf::from("/shows/Passenger.mkv"));
assert_eq!(entry.subtitles, Some(PathBuf::from("/shows/Passenger.srt")));
```

- [ ] **Step 5: Discover every startup entry before constructing the session**

Replace the single discovery in `main` with:

```rust
let first = media::discover(
    &config.video,
    config.subtitles.clone(),
    config.title.as_deref(),
)?;
let mut entries = Vec::with_capacity(1 + config.queued_videos.len());
entries.push(first);
for path in &config.queued_videos {
    entries.push(media::discover(path, None, None)?);
}
let mut session = pipeline::StreamSession::new(&config, entries)?;
ratatui::run(|terminal| {
    ui::run(terminal, &mut session, &config.startup_dir)
})
```

Temporarily adapt `StreamSession::new` to accept `Vec<MediaInfo>` and use its
first entry so the branch compiles; Task 2 replaces that temporary indexing
with `Playlist`.

- [ ] **Step 6: Update the public usage assertion and run focused tests**

Add the exact `--queue` line to `tests/stream.rs`, then run:

```bash
cargo test tests::parses_repeatable_queue_in_argument_order
cargo test tests::queue_requires_a_nonempty_value
cargo test --test stream usage_errors_exit_with_status_two
cargo test media::tests
```

Expected: all commands exit 0.

- [ ] **Step 7: Commit startup queue parsing**

```bash
git add src/main.rs src/media.rs src/pipeline.rs src/ui.rs tests/stream.rs
git commit -m "feat: parse startup video queue"
```

### Task 2: Add the pure playlist model

**Files:**

- Modify: `src/pipeline.rs:1051-1086`

**Interfaces:**

- Consumes: `Vec<MediaInfo>` from Task 1.
- Produces: `Playlist::entries`, `active`, `selected`, and `next_index`.
- Produces: mutation methods `select_by`, `move_selected`, `remove_selected`, and `push`.
- Produces: navigation methods `start_target`, `previous_target`, `next_target`, `activate`, and `finish_target`.

- [ ] **Step 1: Write failing playlist mutation tests**

Add a media factory and tests inside `pipeline.rs`:

```rust
fn entry(title: &str) -> MediaInfo {
    MediaInfo {
        path: PathBuf::from(format!("/tmp/{title}.mkv")),
        subtitles: None,
        title: title.into(),
        duration: gst::ClockTime::from_seconds(60),
    }
}

#[test]
fn playlist_locks_active_row_and_both_crossing_moves() {
    let mut playlist = Playlist::new(vec![
        entry("one"),
        entry("two"),
        entry("three"),
        entry("four"),
    ]);
    playlist.activate(1);

    playlist.selected = 1;
    assert!(!playlist.remove_selected());
    assert!(!playlist.move_selected(1));

    playlist.selected = 0;
    assert!(!playlist.move_selected(1));
    playlist.selected = 2;
    assert!(!playlist.move_selected(-1));
    assert_eq!(
        playlist.entries.iter().map(|entry| entry.title.as_str()).collect::<Vec<_>>(),
        ["one", "two", "three", "four"]
    );
}

#[test]
fn playlist_reorders_and_removes_unlocked_rows() {
    let mut playlist = Playlist::new(vec![
        entry("one"),
        entry("two"),
        entry("three"),
        entry("four"),
    ]);
    playlist.activate(1);
    playlist.selected = 2;

    assert!(playlist.move_selected(1));
    assert_eq!(playlist.selected, 3);
    assert!(playlist.remove_selected());
    assert_eq!(
        playlist.entries.iter().map(|entry| entry.title.as_str()).collect::<Vec<_>>(),
        ["one", "two", "four"]
    );
    assert_eq!(playlist.active, Some(1));
}
```

- [ ] **Step 2: Write failing playhead tests**

```rust
#[test]
fn playlist_tracks_start_previous_next_and_final_lobby() {
    let mut playlist = Playlist::new(vec![entry("one"), entry("two")]);

    assert_eq!(playlist.start_target(), Some(0));
    playlist.activate(0);
    assert_eq!(playlist.previous_target(), None);
    assert_eq!(playlist.next_target(), Some(1));

    playlist.activate(1);
    assert_eq!(playlist.previous_target(), Some(0));
    assert_eq!(playlist.next_target(), None);
    assert_eq!(playlist.finish_target(), None);
    assert_eq!(playlist.active, None);
    assert_eq!(playlist.next_index, 2);

    playlist.push(entry("three"));
    assert_eq!(playlist.start_target(), Some(2));
    assert_eq!(playlist.previous_target(), Some(1));
}
```

- [ ] **Step 3: Run the playlist tests to verify RED**

Run:

```bash
cargo test pipeline::tests::playlist_
```

Expected: compile failure because `Playlist` does not exist.

- [ ] **Step 4: Implement the smallest in-module playlist value**

Add above `StreamSession`:

```rust
struct Playlist {
    entries: Vec<MediaInfo>,
    active: Option<usize>,
    selected: usize,
    next_index: usize,
}

impl Playlist {
    fn new(entries: Vec<MediaInfo>) -> Self {
        Self {
            entries,
            active: None,
            selected: 0,
            next_index: 0,
        }
    }

    fn start_target(&self) -> Option<usize> {
        (self.next_index < self.entries.len()).then_some(self.next_index)
    }

    fn previous_target(&self) -> Option<usize> {
        self.active
            .unwrap_or(self.next_index)
            .checked_sub(1)
    }

    fn next_target(&self) -> Option<usize> {
        let index = self.active.map_or(self.next_index, |index| index + 1);
        (index < self.entries.len()).then_some(index)
    }

    fn activate(&mut self, index: usize) {
        self.active = Some(index);
        self.selected = index;
        self.next_index = index + 1;
    }

    fn finish_target(&mut self) -> Option<usize> {
        self.active = None;
        self.start_target()
    }

    fn push(&mut self, entry: MediaInfo) {
        self.entries.push(entry);
        self.selected = self.entries.len() - 1;
    }
}
```

Implement `select_by`, `move_selected`, and `remove_selected` with checked
index arithmetic. `move_selected` returns `false` when the source or target is
the active index. `remove_selected` adjusts `active` and `next_index` when a
preceding row is removed, clamps `selected`, and returns whether it changed the
list. Selection and removal are no-ops for an empty playlist; removing the last
unlocked entry leaves `selected` and `next_index` at zero.

- [ ] **Step 5: Run the pure model tests**

Run:

```bash
cargo test pipeline::tests::playlist_
```

Expected: every playlist test passes without constructing GStreamer elements.

- [ ] **Step 6: Commit the playlist model**

```bash
git add src/pipeline.rs
git commit -m "feat: add mutable playlist state"
```

### Task 3: Replace playback while retaining broadcast

**Files:**

- Modify: `src/pipeline.rs:418-605`
- Modify: `src/pipeline.rs:710-1050`
- Modify: `src/pipeline.rs:1051-1302`

**Interfaces:**

- Consumes: `Playlist` and `MediaInfo` from Tasks 1-2.
- Produces: `ActivePlayback { audio_bridge, playback }`.
- Produces: `StreamSession::previous_track`, `next_track`, `add_entry`, and playlist getters.
- Changes: `StreamSession::poll() -> Result<(), Box<dyn Error>>`; EOF no longer exits the TUI.

- [ ] **Step 1: Add a failing broadcast-lobby selector test**

Store the existing `video_lobby_pad` in `BroadcastPipeline`, then add:

```rust
#[test]
fn broadcast_can_return_from_movie_to_lobby() {
    let _guard = gst_test();
    let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();

    broadcast.select_movie();
    broadcast.select_lobby();

    assert_eq!(
        broadcast.video_selector.property::<Option<gst::Pad>>("active-pad"),
        Some(broadcast.video_lobby_pad.clone())
    );
    assert_eq!(
        broadcast.audio_selector.property::<Option<gst::Pad>>("active-pad"),
        Some(broadcast.audio_lobby_pad.clone())
    );
}
```

- [ ] **Step 2: Add failing navigation/state tests around a fake session**

Adapt `session_with_fakesink` to accept multiple entries and add:

```rust
#[test]
fn manual_track_targets_preserve_playback_state() {
    let mut session = session_with_fakesink_entries(vec![
        entry("one"),
        entry("two"),
        entry("three"),
    ]);
    session.playlist.activate(1);
    session.state = PlaybackState::Paused;

    assert_eq!(session.playlist.previous_target(), Some(0));
    assert_eq!(session.playlist.next_target(), Some(2));
    assert_eq!(session.state, PlaybackState::Paused);
}

#[test]
fn final_eos_returns_playlist_to_lobby() {
    let mut playlist = Playlist::new(vec![entry("one")]);
    playlist.activate(0);

    assert_eq!(playlist.finish_target(), None);
    assert_eq!(playlist.active, None);
    assert_eq!(playlist.next_index, 1);
}

#[test]
fn polling_final_eos_selects_lobby_without_finishing_the_session() {
    let _guard = gst_test();
    let mut session = session_with_fakesink_entries(vec![entry("one")]);
    session.playlist.activate(0);
    session.state = PlaybackState::Playing;
    session
        .active
        .as_ref()
        .unwrap()
        .playback
        .pipeline
        .bus()
        .unwrap()
        .post(gst::message::Eos::builder().build())
        .unwrap();

    session.poll().unwrap();

    assert_eq!(session.state(), PlaybackState::Lobby);
    assert_eq!(session.active_index(), None);
    assert_eq!(
        session.broadcast.video_selector.property::<Option<gst::Pad>>("active-pad"),
        Some(session.broadcast.video_lobby_pad.clone())
    );
}
```

- [ ] **Step 3: Run the focused tests to verify RED**

Run:

```bash
cargo test pipeline::tests::broadcast_can_return_from_movie_to_lobby
cargo test pipeline::tests::manual_track_targets_preserve_playback_state
```

Expected: compile failures for the missing lobby pad/method and new session
shape.

- [ ] **Step 4: Make playback entry-specific**

Change:

```rust
fn PlaybackPipeline::build(entry: &MediaInfo) -> Result<Self, Box<dyn Error>>
```

Set `uridecodebin3` from `entry.path`, and replace every stored
`StreamSession.subtitles` use with `entry.subtitles.as_deref()`.

Group the drop-sensitive values:

```rust
struct ActivePlayback {
    // Drop the bridge first so its reader thread cannot outlive playback.
    audio_bridge: AudioBridge,
    playback: PlaybackPipeline,
}
```

- [ ] **Step 5: Retain both lobby selector pads**

Add `video_lobby_pad: gst::Pad` to `BroadcastPipeline` and:

```rust
fn select_lobby(&self) {
    self.video_selector
        .set_property("active-pad", Some(&self.video_lobby_pad));
    self.audio_selector
        .set_property("active-pad", Some(&self.audio_lobby_pad));
}
```

Keep `select_movie` and `freeze` unchanged.

- [ ] **Step 6: Change `StreamSession` to own playlist plus optional playback**

Use this field shape:

```rust
pub(crate) struct StreamSession {
    broadcast: BroadcastPipeline,
    active: Option<ActivePlayback>,
    playlist: Playlist,
    state: PlaybackState,
    gain_db: f64,
    levels: AudioLevels,
    title_token: String,
    title_url: String,
}
```

`StreamSession::new(&Config, Vec<MediaInfo>)` builds and starts only the
broadcast pipeline, stores `Playlist::new(entries)`, leaves `active` as `None`,
and sets the initial Owncast title to `Starting soon: <first title>`.

- [ ] **Step 7: Implement one shared track replacement path**

Add:

```rust
fn activate_track(
    &mut self,
    index: usize,
    target_state: PlaybackState,
) -> Result<(), Box<dyn Error>>
```

Its body must perform this order:

```rust
if let Some(active) = &self.active {
    active.audio_bridge.set_paused(true);
    self.broadcast.freeze(active.playback.latest_frame()?)?;
}
drop(self.active.take());

let entry = &self.playlist.entries[index];
let playback = PlaybackPipeline::build(entry)?;
playback.pipeline.set_state(gst::State::Paused)?;
playback.wait_ready(entry.subtitles.as_deref())?;
let audio_bridge = AudioBridge::start(
    playback.audio_output.clone(),
    self.broadcast.audio_source.clone(),
    self.broadcast.pipeline.clone(),
)?;
let active = ActivePlayback {
    audio_bridge,
    playback,
};
active.playback.pipeline.set_state(gst::State::Playing)?;
let frame = active
    .playback
    .wait_for_frame_after(0, Duration::from_secs(1))?;

set_title(&self.title_url, &self.title_token, &entry.title)?;
if target_state == PlaybackState::Paused {
    active.audio_bridge.set_paused(true);
    self.broadcast.freeze(frame)?;
    active.playback.pipeline.set_state(gst::State::Paused)?;
} else {
    self.broadcast.select_movie();
}
self.active = Some(active);
self.playlist.activate(index);
self.state = target_state;
```

Keep failure behavior fatal. Do not rebuild the previous track after an
activation error.

- [ ] **Step 8: Route start, Previous, Next, pause, seek, position, and duration through `active`**

Expose:

```rust
pub(crate) fn previous_track(&mut self) -> Result<(), Box<dyn Error>>;
pub(crate) fn next_track(&mut self) -> Result<(), Box<dyn Error>>;
pub(crate) fn add_entry(&mut self, entry: MediaInfo);
pub(crate) fn entries(&self) -> &[MediaInfo];
pub(crate) fn active_index(&self) -> Option<usize>;
pub(crate) fn selected_index(&self) -> usize;
pub(crate) fn next_index(&self) -> usize;
pub(crate) fn active_path(&self) -> Option<&Path>;
pub(crate) fn select_entry(&mut self, delta: i32);
pub(crate) fn move_selected(&mut self, delta: i32);
pub(crate) fn remove_selected(&mut self);
```

`start` activates `playlist.start_target()` Playing. Manual navigation gets
the adjacent target and calls `activate_track(target, self.state)`. Boundary
targets return `Ok(())`. Pause, seek, position, and duration operate on
`active`. In the initial lobby, `title()` and `duration()` describe the first
pending entry with a zero position. After the final entry, they return
`"No track queued"`, zero duration, and zero position until another entry is
appended.

- [ ] **Step 9: Auto-advance inside `poll`**

Change `poll` to return `Result<(), Box<dyn Error>>`. Drain the active playback
bus and remember whether EOS occurred. After releasing the bus borrow:

```rust
if eos {
    if let Some(next) = self.playlist.finish_target() {
        self.activate_track(next, PlaybackState::Playing)?;
    } else {
        self.broadcast.select_lobby();
        drop(self.active.take());
        self.state = PlaybackState::Lobby;
    }
}
```

Continue draining broadcast levels/errors exactly as before.

- [ ] **Step 10: Add a synthetic replacement test**

Build one `BroadcastPipeline::build_with_sink("fakesink")` and attach a buffer
counter before its video output. Sequentially start and stop two small
GStreamer pipelines whose `videotestsrc` feeds
`intervideosink channel=owncast-movie`. Select lobby silence while replacing
the source, then select movie again:

```rust
let broadcast_ptr = broadcast.pipeline.as_ptr();
first.set_state(gst::State::Playing).unwrap();
broadcast.select_movie();
std::thread::sleep(Duration::from_millis(150));
let after_first = output_buffers.load(Ordering::SeqCst);

broadcast.select_lobby();
first.set_state(gst::State::Null).unwrap();
second.set_state(gst::State::Playing).unwrap();
broadcast.select_movie();
std::thread::sleep(Duration::from_millis(150));

assert_eq!(broadcast.pipeline.as_ptr(), broadcast_ptr);
assert!(after_first > 0);
assert!(output_buffers.load(Ordering::SeqCst) > after_first);
```

Return both synthetic pipelines to `Null`. This proves source replacement
without requiring an RTMP server, movie decoder, or injectable pipeline
factory.

- [ ] **Step 11: Run pipeline verification**

Run:

```bash
cargo test pipeline::tests::playlist_
cargo test pipeline::tests::broadcast_can_return_from_movie_to_lobby
cargo test pipeline::tests::synthetic_track_replacement_keeps_broadcast_live
cargo test pipeline::tests
```

Expected: all non-ignored pipeline tests pass.

- [ ] **Step 12: Commit track replacement**

```bash
git add src/pipeline.rs
git commit -m "feat: advance playlist without reconnecting"
```

### Task 4: Render and edit the persistent playlist

**Files:**

- Modify: `src/ui.rs:1-318`
- Modify: `src/ui.rs:320-619`

**Interfaces:**

- Consumes: playlist getters and mutators from Task 3.
- Produces: `Focus::{Playback, Playlist}`.
- Produces: focus-aware `Command` values for navigation and editing.
- Produces: lower-panel playlist rendering at 80x24 and larger sizes.

- [ ] **Step 1: Write failing focus-aware key tests**

Replace `command_for_key(state, KeyCode)` tests with `KeyEvent` tests:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Playback,
    Playlist,
}

#[test]
fn playback_focus_maps_track_and_playlist_keys() {
    assert_eq!(
        command_for_key(
            Focus::Playback,
            PlaybackState::Playing,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        ),
        Some(Command::NextTrack)
    );
    assert_eq!(
        command_for_key(
            Focus::Playback,
            PlaybackState::Paused,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        ),
        Some(Command::PreviousTrack)
    );
    assert_eq!(
        command_for_key(
            Focus::Playback,
            PlaybackState::Playing,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        ),
        Some(Command::TogglePlaylistFocus)
    );
}

#[test]
fn playlist_focus_maps_selection_reorder_and_remove() {
    assert_eq!(
        command_for_key(
            Focus::Playlist,
            PlaybackState::Playing,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        ),
        Some(Command::Select(1))
    );
    assert_eq!(
        command_for_key(
            Focus::Playlist,
            PlaybackState::Playing,
            KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
        ),
        Some(Command::Move(-1))
    );
    assert_eq!(
        command_for_key(
            Focus::Playlist,
            PlaybackState::Playing,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        ),
        Some(Command::Remove)
    );
}
```

- [ ] **Step 2: Run UI keys to verify RED**

Run:

```bash
cargo test ui::tests::playback_focus_maps_track_and_playlist_keys
```

Expected: compile failure because `Focus` and the new commands do not exist.

- [ ] **Step 3: Add the exact command surface**

Use:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Start,
    TogglePause,
    Seek(i64),
    Gain(i8),
    PreviousTrack,
    NextTrack,
    OpenChooser,
    TogglePlaylistFocus,
    Select(i32),
    Move(i32),
    Remove,
    Quit,
}
```

Change `command_for_key` to accept the complete `KeyEvent`, preserving existing
playback mappings and adding the approved focus-specific mappings. Treat
Delete and `d` identically in playlist focus; Tab and Esc return to playback
focus.

- [ ] **Step 4: Add failing playlist render tests**

Extend `Status` with:

```rust
playlist: &'a [MediaInfo],
active_index: Option<usize>,
selected_index: usize,
next_index: usize,
focus: Focus,
```

Add:

```rust
#[test]
fn standard_console_shows_persistent_playlist_and_compact_help() {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let status = playlist_status();
    terminal.draw(|frame| render(frame, &status, None)).unwrap();
    let text = region_text(terminal.backend().buffer(), Rect::new(0, 0, 80, 24));

    for required in [
        "PLAYLIST",
        "✓ 1 Passenger",
        "▶ 2 Alien",
        "LOCKED",
        "· 3 Arrival",
        "P/N TRACK",
        "A ADD",
        "TAB EDIT",
    ] {
        assert!(text.contains(required), "missing {required}");
    }
}

#[test]
fn playlist_window_keeps_selected_row_visible() {
    assert_eq!(playlist_window(10, 3, 12), 8..11);
    assert_eq!(playlist_window(1, 3, 12), 0..3);
}

#[test]
fn playlist_title_truncation_is_character_safe() {
    assert_eq!(truncate_title("Arrival 🛸 Extended", 10), "Arrival 🛸…");
    assert_eq!(truncate_title("Alien", 10), "Alien");
}
```

- [ ] **Step 5: Replace the control tiles and rail with the approved lower panel**

Use one vertical layout:

```rust
let rows = Layout::vertical([
    Constraint::Length(1),
    Constraint::Length(3),
    Constraint::Length(1),
    Constraint::Length(1),
    Constraint::Length(5),
    Constraint::Min(6),
    Constraint::Length(3),
])
.split(frame.area());
```

Keep header, title, status/time, and stereo meter. Render playlist rows into
`rows[5]` and focus-specific compact key help into `rows[6]`. Delete
`render_controls` and `render_rail`.

Use these row markers:

```rust
let state = if Some(index) == status.active_index {
    "▶"
} else if index < status.next_index {
    "✓"
} else {
    "·"
};
let selected = if index == status.selected_index { ">" } else { " " };
let locked = if Some(index) == status.active_index { " LOCKED" } else { "" };
```

Compute a viewport with `playlist_window(selected, panel_height, len)`.
Implement `truncate_title(title: &str, max_chars: usize) -> String` with
`title.chars()`, reserving one character for `…` when truncation is required.
Use it before appending the right-aligned duration and `LOCKED`; never slice a
UTF-8 string by byte index.

- [ ] **Step 6: Wire UI commands to `StreamSession`**

Keep `let mut focus = Focus::Playback;` inside `run`. Map:

```rust
Command::PreviousTrack => session.previous_track()?,
Command::NextTrack => session.next_track()?,
Command::TogglePlaylistFocus => {
    focus = match focus {
        Focus::Playback => Focus::Playlist,
        Focus::Playlist => Focus::Playback,
    };
}
Command::Select(delta) => session.select_entry(delta),
Command::Move(delta) => session.move_selected(delta),
Command::Remove => session.remove_selected(),
```

Call `session.poll()?` every loop; do not exit on track EOF.

- [ ] **Step 7: Run UI verification**

Run:

```bash
cargo test ui::tests::playback_focus_maps_track_and_playlist_keys
cargo test ui::tests::playlist_focus_maps_selection_reorder_and_remove
cargo test ui::tests::standard_console_shows_persistent_playlist_and_compact_help
cargo test ui::tests
```

Expected: all UI tests pass, including the updated 110x28, 60x34, and 80x24
snapshots/assertions.

- [ ] **Step 8: Commit the playlist TUI**

```bash
git add src/ui.rs
git commit -m "feat: show and edit playlist in TUI"
```

### Task 5: Add the standard-library file chooser

**Files:**

- Modify: `src/ui.rs`
- Modify: `src/main.rs:147-153`

**Interfaces:**

- Consumes: `media::discover(path, None, None)` and `StreamSession::add_entry`.
- Consumes: active track parent from `StreamSession::active_path`.
- Produces: `FileChooser::open`, `move_selection`, `parent`, and `selected_path`.
- Changes: `ui::run(terminal, session, startup_dir)`.

- [ ] **Step 1: Write failing filesystem navigation tests**

Use a unique temporary directory and standard library file operations:

```rust
#[test]
fn chooser_lists_parent_directories_then_files_by_name() {
    let root = temp_directory("chooser-order");
    fs::create_dir(root.join("Zulu")).unwrap();
    fs::create_dir(root.join("alpha")).unwrap();
    fs::write(root.join("b.mkv"), []).unwrap();
    fs::write(root.join("A.mp4"), []).unwrap();

    let chooser = FileChooser::open(root.clone()).unwrap();
    let labels = chooser
        .entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["..", "alpha/", "Zulu/", "A.mp4", "b.mkv"]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn chooser_parent_navigation_reloads_entries() {
    let root = temp_directory("chooser-parent");
    let child = root.join("child");
    fs::create_dir_all(&child).unwrap();
    let mut chooser = FileChooser::open(child).unwrap();

    chooser.parent().unwrap();

    assert_eq!(chooser.directory, root);
    fs::remove_dir_all(&chooser.directory).unwrap();
}
```

- [ ] **Step 2: Run chooser tests to verify RED**

Run:

```bash
cargo test ui::tests::chooser_
```

Expected: compile failure because `FileChooser` does not exist.

- [ ] **Step 3: Implement the minimal chooser state**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileKind {
    Parent,
    Directory,
    File,
}

struct FileEntry {
    path: PathBuf,
    label: String,
    kind: FileKind,
}

struct FileChooser {
    directory: PathBuf,
    entries: Vec<FileEntry>,
    selected: usize,
    error: Option<String>,
}
```

`FileChooser::open` canonicalizes the directory and calls a private
`read_entries`. `read_entries`:

1. adds `..` when `directory.parent()` exists;
2. collects only child directories and regular files from `fs::read_dir`;
3. sorts directories and files separately by lowercase display label;
4. appends directories before files.

Use saturating selection movement. Enter on Parent/Directory replaces the
directory and refreshes. Enter on File returns its path without mutating the
chooser.

- [ ] **Step 4: Add chooser key mapping tests**

Extend `Command` with chooser-local actions:

```rust
ChooserMove(i32),
ChooserActivate,
ChooserParent,
CloseChooser,
```

Add assertions for Up/Down, Enter, Backspace, Esc, `q`, and Ctrl-C while the
chooser is open. Chooser mapping takes precedence over playback/playlist
mapping.

- [ ] **Step 5: Render the chooser overlay and inline error**

Change:

```rust
fn render(
    frame: &mut Frame<'_>,
    status: &Status<'_>,
    chooser: Option<&FileChooser>,
)
```

Render the normal TUI first. When a chooser exists, center a bordered rectangle,
clear it with `ratatui::widgets::Clear`, show the current directory, visible
entries, selection marker, `Enter Open/Add · Backspace Parent · Esc Cancel`,
and `chooser.error` in the bottom line.

Use Ratatui clipping and the same amber/black palette. Do not add mouse events
or a filename filter.

- [ ] **Step 6: Keep playback polling while validating chooser selections**

In `run`, keep:

```rust
let mut chooser: Option<FileChooser> = None;
```

Open from:

```rust
let directory = session
    .active_path()
    .and_then(Path::parent)
    .unwrap_or(startup_dir);
chooser = Some(FileChooser::open(directory.to_owned())?);
```

On a selected regular file:

```rust
match media::discover(&path, None, None) {
    Ok(entry) => {
        session.add_entry(entry);
        chooser = None;
    }
    Err(failure) => {
        if let Some(chooser) = &mut chooser {
            chooser.error = Some(failure.to_string());
        }
    }
}
```

The loop must call `session.poll()?` before rendering regardless of whether the
chooser is open.

- [ ] **Step 7: Test failed insertion without mutating a playlist**

Keep discovery itself outside the pure chooser and test the branch through a
small helper:

```rust
fn add_discovered(
    session: &mut StreamSession,
    chooser: &mut FileChooser,
    result: Result<MediaInfo, Box<dyn Error>>,
) -> bool
```

It returns `true` after adding and `false` after storing an error. Test:

```rust
let before = session.entries().len();
assert!(!add_discovered(
    &mut session,
    &mut chooser,
    Err(error("Cannot discover video broken.mkv")),
));
assert_eq!(session.entries().len(), before);
assert_eq!(
    chooser.error.as_deref(),
    Some("Cannot discover video broken.mkv")
);
```

- [ ] **Step 8: Run chooser and UI tests**

Run:

```bash
cargo test ui::tests::chooser_
cargo test ui::tests::standard_console_shows_persistent_playlist_and_compact_help
cargo test ui::tests
```

Expected: all tests pass without opening a real terminal.

- [ ] **Step 9: Commit the chooser**

```bash
git add src/main.rs src/ui.rs
git commit -m "feat: add videos from TUI chooser"
```

### Task 6: Verify title updates, documentation, and the complete feature

**Files:**

- Modify: `src/pipeline.rs`
- Modify: `README.md:1-87`
- Modify: `tests/stream.rs`

**Interfaces:**

- Verifies: manual and automatic activation call the existing `set_title`.
- Documents: repeatable queue syntax and every approved key.
- Produces: a fully formatted, lint-clean release build.

- [ ] **Step 1: Add a loopback title-update regression test**

Extract the title line inside `activate_track` into:

```rust
fn update_title(&self, index: usize) -> Result<(), Box<dyn Error>> {
    set_title(
        &self.title_url,
        &self.title_token,
        &self.playlist.entries[index].title,
    )
}
```

Use `std::net::TcpListener` bound to `127.0.0.1:0`, accept one request in a
thread, and capture the request bytes. Call `update_title(1)` on a fake session
and assert:

```rust
assert!(request.contains("POST / HTTP/1.1"));
assert!(request.contains("Authorization: Bearer token"));
assert!(request.contains(r#"{"value":"Alien"}"#));
```

Keep exactly one `self.update_title(index)?` call inside `activate_track`.
Manual and EOF navigation both use `activate_track`, so the loopback test plus
that shared call covers both without duplicating HTTP tests.

- [ ] **Step 2: Run the title test**

Run:

```bash
cargo test pipeline::tests::track_activation_updates_owncast_title
```

Expected: pass with a loopback server only.

- [ ] **Step 3: Update README usage and controls**

Document:

```bash
owncast-stream [OPTIONS] VIDEO [SUBTITLES] [TITLE]
owncast-stream movie-one.mkv --queue movie-two.mkv --queue movie-three.mkv
```

Update the feature list with the persistent playlist, automatic advancement,
runtime chooser, and previous/next controls. Replace the current Controls list
with:

```text
Enter starts the first pending track from the lobby.
Space pauses or resumes playback.
Left and Right seek backward or forward 30 seconds.
Up and Down adjust gain by 1 dB.
p and n restart the previous or next track.
a opens the file chooser.
Tab focuses playlist editing; Shift+Up/Down moves and Delete removes unlocked entries.
q or Ctrl-C quits.
```

State that the active row is locked and final EOF returns to the lobby.

- [ ] **Step 4: Run formatting and the full test suite**

Run:

```bash
cargo fmt --check
cargo test --all-targets
```

Expected: formatting exits 0; every non-ignored test passes with zero failures.

- [ ] **Step 5: Run static analysis and release build**

Run:

```bash
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
```

Expected: both commands exit 0 with no Clippy warnings.

- [ ] **Step 6: Confirm dependency and subprocess boundaries**

Run:

```bash
git diff --exit-code -- Cargo.toml Cargo.lock
cargo test --test stream media_path_has_no_subprocess_calls
git diff --check
```

Expected: no dependency changes, the no-subprocess test passes, and no
whitespace errors are reported.

- [ ] **Step 7: Commit documentation and final verification changes**

```bash
git add README.md src/pipeline.rs tests/stream.rs
git commit -m "docs: describe playlist controls"
```

- [ ] **Step 8: Inspect the final branch**

Run:

```bash
git status --short
git log --oneline --decorate -7
```

Expected: clean status and one intentional commit per task above the approved
design commit.
