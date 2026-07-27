# Gain Controls and VU Meter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Up/Down gain control and a stereo 3-second peak-hold VU meter in an amber projection-console TUI.

**Architecture:** The existing broadcast pipeline gains GStreamer's native `level` element immediately after `audio_gain`. `StreamSession` owns gain and the latest stereo meter values, while `src/ui.rs` maps keys and renders the approved responsive amber console using existing Ratatui primitives.

**Tech Stack:** Rust 1.92+, GStreamer 1.28, Ratatui 0.30, Crossterm

## Global Constraints

- Up and Down adjust gain by exactly 1 dB in every playback state.
- Gain starts at +3 dB and clamps to the inclusive range -12 dB through +12 dB.
- Meter the post-gain signal sent to the encoder.
- Post meter updates every 100 ms.
- Hold peaks for 3 seconds, then decay at 12 dB/sec.
- Render stereo L/R meters on a -60 dB through 0 dB scale.
- Use the approved amber monochrome projection-console layout.
- Add no crate, audio branch, thread, or custom sample analysis.
- Malformed level messages leave the previous reading unchanged.

---

### Task 1: Pipeline gain and stereo metering

**Files:**
- Modify: `src/pipeline.rs:17-320`
- Modify: `src/pipeline.rs:653-828`
- Test: `src/pipeline.rs:830-end`

**Interfaces:**
- Consumes: the existing `audio_gain` element and broadcast bus
- Produces: `AudioLevels`, `StreamSession::gain_db()`, `StreamSession::levels()`, and `StreamSession::adjust_gain(i8)`

- [ ] **Step 1: Add failing tests for gain conversion and clamping**

Add:

```rust
#[test]
fn gain_steps_and_clamps_in_db() {
    assert!((db_to_amplitude(0.0) - 1.0).abs() < 0.000001);
    assert!((db_to_amplitude(3.0) - 1.4125375).abs() < 0.000001);
    assert_eq!(adjusted_gain_db(3.0, 1), 4.0);
    assert_eq!(adjusted_gain_db(3.0, -1), 2.0);
    assert_eq!(adjusted_gain_db(12.0, 1), 12.0);
    assert_eq!(adjusted_gain_db(-12.0, -1), -12.0);
}
```

- [ ] **Step 2: Run the gain test and verify RED**

Run:

```bash
cargo test pipeline::tests::gain_steps_and_clamps_in_db --locked
```

Expected: compile failure because `db_to_amplitude` and `adjusted_gain_db` do not exist.

- [ ] **Step 3: Add the gain constants and helpers**

Near `INTER_CHANNEL`, add:

```rust
const DEFAULT_GAIN_DB: f64 = 3.0;
const MIN_GAIN_DB: f64 = -12.0;
const MAX_GAIN_DB: f64 = 12.0;

fn db_to_amplitude(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

fn adjusted_gain_db(current: f64, steps: i8) -> f64 {
    (current + f64::from(steps)).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
}
```

- [ ] **Step 4: Run the gain test and verify GREEN**

Run:

```bash
cargo test pipeline::tests::gain_steps_and_clamps_in_db --locked
```

Expected: PASS.

- [ ] **Step 5: Add a failing level-message parsing test**

Add the public meter value and this test:

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AudioLevels {
    pub(crate) peak: [f64; 2],
    pub(crate) decay: [f64; 2],
}

impl Default for AudioLevels {
    fn default() -> Self {
        Self {
            peak: [-60.0; 2],
            decay: [-60.0; 2],
        }
    }
}

#[test]
fn parses_stereo_peak_and_decay_from_level_message() {
    let _gst = gst_test();
    let structure = gst::Structure::builder("level")
        .field("peak", gst::glib::ValueArray::new([-4.2_f64, -6.1]))
        .field("decay", gst::glib::ValueArray::new([-2.8_f64, -3.4]))
        .build();
    let message = gst::message::Element::new(structure);

    assert_eq!(
        parse_audio_levels(&message),
        Some(AudioLevels {
            peak: [-4.2, -6.1],
            decay: [-2.8, -3.4],
        })
    );
}

#[test]
fn ignores_malformed_level_message() {
    let _gst = gst_test();
    let message = gst::message::Element::new(gst::Structure::new_empty("level"));

    assert_eq!(parse_audio_levels(&message), None);
}
```

- [ ] **Step 6: Run the parser tests and verify RED**

Run:

```bash
cargo test pipeline::tests::parses_stereo_peak_and_decay_from_level_message --locked
```

Expected: compile failure because `parse_audio_levels` does not exist.

- [ ] **Step 7: Implement level-message parsing**

Add:

```rust
fn stereo_values(structure: &gst::StructureRef, field: &str) -> Option<[f64; 2]> {
    let values = structure.get::<gst::glib::ValueArray>(field).ok()?;
    Some([
        values.first()?.get::<f64>().ok()?,
        values.get(1)?.get::<f64>().ok()?,
    ])
}

fn parse_audio_levels(message: &gst::MessageRef) -> Option<AudioLevels> {
    let structure = message.structure()?;
    if structure.name() != "level" {
        return None;
    }
    Some(AudioLevels {
        peak: stereo_values(structure, "peak")?,
        decay: stereo_values(structure, "decay")?,
    })
}
```

- [ ] **Step 8: Run the parser tests and verify GREEN**

Run:

```bash
cargo test pipeline::tests::parses_stereo_peak_and_decay_from_level_message --locked
cargo test pipeline::tests::ignores_malformed_level_message --locked
```

Expected: both PASS.

- [ ] **Step 9: Add a failing pipeline configuration test**

Extend `broadcast_and_playback_use_separate_pipelines`:

```rust
let meter = broadcast.pipeline.by_name("audio_meter").unwrap();
assert_eq!(meter.property::<u64>("interval"), 100_000_000);
assert_eq!(meter.property::<u64>("peak-ttl"), 3_000_000_000);
assert_eq!(meter.property::<f64>("peak-falloff"), 12.0);
assert!(meter.property::<bool>("post-messages"));
assert!((broadcast.audio_gain.property::<f64>("volume") - db_to_amplitude(3.0)).abs() < 0.000001);
```

- [ ] **Step 10: Run the pipeline test and verify RED**

Run:

```bash
cargo test pipeline::tests::broadcast_and_playback_use_separate_pipelines --locked
```

Expected: failure because `audio_meter` and `BroadcastPipeline::audio_gain` do not exist.

- [ ] **Step 11: Add native level metering and retain the gain element**

Add `"level"` to `REQUIRED_ELEMENTS`.

Change the audio graph to:

```text
! volume name=audio_gain
! level name=audio_meter interval=100000000
    peak-ttl=3000000000 peak-falloff=12 post-messages=true
! avenc_aac name=audio_encoder bitrate=192000
```

Add `audio_gain: gst::Element` to `BroadcastPipeline`. Retrieve it after parsing:

```rust
let audio_gain = pipeline
    .by_name("audio_gain")
    .ok_or_else(|| error("Audio gain is missing"))?;
audio_gain.set_property("volume", db_to_amplitude(DEFAULT_GAIN_DB));
```

Store `audio_gain` in the returned struct.

- [ ] **Step 12: Add session state and methods**

Add to `StreamSession`:

```rust
gain_db: f64,
levels: AudioLevels,
```

Initialize them with `DEFAULT_GAIN_DB` and `AudioLevels::default()`. Add:

```rust
pub(crate) fn gain_db(&self) -> f64 {
    self.gain_db
}

pub(crate) fn levels(&self) -> AudioLevels {
    self.levels
}

pub(crate) fn adjust_gain(&mut self, steps: i8) {
    self.gain_db = adjusted_gain_db(self.gain_db, steps);
    self.broadcast
        .audio_gain
        .set_property("volume", db_to_amplitude(self.gain_db));
}
```

Change `poll(&self)` to `poll(&mut self)`. In the broadcast bus loop, before
error handling, add:

```rust
if let Some(levels) = parse_audio_levels(&message) {
    self.levels = levels;
    continue;
}
```

- [ ] **Step 13: Run focused and full pipeline tests**

Run:

```bash
cargo test pipeline::tests --locked
```

Expected: all pipeline tests PASS.

- [ ] **Step 14: Commit pipeline behavior**

```bash
git add src/pipeline.rs
git commit -m "feat: add gain control and audio metering"
```

### Task 2: Amber projection-console TUI

**Files:**
- Modify: `src/ui.rs:1-201`
- Test: `src/ui.rs:125-end`

**Interfaces:**
- Consumes: `AudioLevels`, `gain_db()`, `levels()`, and `adjust_gain(i8)` from Task 1
- Produces: Up/Down commands and the responsive amber projection-console display

- [ ] **Step 1: Add failing Up/Down key mapping assertions**

Add to `maps_only_valid_keys_for_each_playback_state`:

```rust
for state in [
    PlaybackState::Lobby,
    PlaybackState::Playing,
    PlaybackState::Paused,
] {
    assert_eq!(command_for_key(state, KeyCode::Up), Some(Command::Gain(1)));
    assert_eq!(command_for_key(state, KeyCode::Down), Some(Command::Gain(-1)));
}
```

Add `Gain(i8)` to `Command`.

- [ ] **Step 2: Run the key test and verify RED**

Run:

```bash
cargo test ui::tests::maps_only_valid_keys_for_each_playback_state --locked
```

Expected: assertion failure because Up and Down map to `None`.

- [ ] **Step 3: Implement Up/Down commands**

Add these match arms before the state-specific controls:

```rust
(_, KeyCode::Up) => Some(Command::Gain(1)),
(_, KeyCode::Down) => Some(Command::Gain(-1)),
```

In `run`, handle:

```rust
Some(Command::Gain(steps)) => session.adjust_gain(steps),
```

- [ ] **Step 4: Run the key test and verify GREEN**

Run:

```bash
cargo test ui::tests::maps_only_valid_keys_for_each_playback_state --locked
```

Expected: PASS.

- [ ] **Step 5: Add failing meter-bar tests**

Add:

```rust
#[test]
fn meter_bar_clamps_signal_and_marks_decay_peak() {
    assert_eq!(meter_bar(10, -60.0, -60.0), "┃·········");
    assert_eq!(meter_bar(10, 0.0, 0.0), "█████████┃");
    let bar = meter_bar(10, -30.0, -6.0);
    assert_eq!(bar.chars().filter(|cell| *cell == '█').count(), 5);
    assert_eq!(bar.chars().position(|cell| cell == '┃'), Some(9));
}
```

- [ ] **Step 6: Run the meter test and verify RED**

Run:

```bash
cargo test ui::tests::meter_bar_clamps_signal_and_marks_decay_peak --locked
```

Expected: compile failure because `meter_bar` does not exist.

- [ ] **Step 7: Implement the meter string helper**

Add:

```rust
fn meter_position(db: f64, width: usize) -> usize {
    ((((db.clamp(-60.0, 0.0) + 60.0) / 60.0) * width as f64).round() as usize)
        .min(width)
}

fn meter_bar(width: usize, peak: f64, decay: f64) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec!['·'; width];
    for cell in cells.iter_mut().take(meter_position(peak, width)) {
        *cell = '█';
    }
    cells[meter_position(decay, width).min(width - 1)] = '┃';
    cells.into_iter().collect()
}
```

- [ ] **Step 8: Run the meter test and verify GREEN**

Run:

```bash
cargo test ui::tests::meter_bar_clamps_signal_and_marks_decay_peak --locked
```

Expected: PASS.

- [ ] **Step 9: Replace the compact render test with wide and narrow console tests**

Extend `Status`:

```rust
pub(crate) gain_db: f64,
pub(crate) levels: AudioLevels,
```

Add a helper in tests:

```rust
fn status() -> Status<'static> {
    Status {
        title: "Passenger",
        state: PlaybackState::Playing,
        position: gst::ClockTime::from_seconds(2_538),
        duration: gst::ClockTime::from_seconds(6_423),
        gain_db: 3.0,
        levels: AudioLevels {
            peak: [-4.2, -6.1],
            decay: [-2.8, -3.4],
        },
    }
}
```

Replace `renders_compact_status_and_help` with:

```rust
#[test]
fn renders_amber_projection_console() {
    let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
    terminal.draw(|frame| render(frame, &status())).unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

    assert!(text.contains("OWNCAST"));
    assert!(text.contains("SHOW LOCAL - AUTO MODE"));
    assert!(text.contains("SHOW INFORMATION"));
    assert!(text.contains("TITLE: PASSENGER"));
    assert!(text.contains("STATUS: PLAYING"));
    assert!(text.contains("00:42:18 / 01:47:03"));
    assert!(text.contains("GAIN +3 dB"));
    assert!(text.contains("PROGRAM AUDIO / 3 SEC PEAK HOLD"));
    assert!(text.contains("ENTER"));
    assert!(text.contains("↑ / ↓"));
    assert!(buffer.content().iter().any(|cell| cell.fg == AMBER));
}

#[test]
fn narrow_console_keeps_core_status_and_controls() {
    let mut terminal = Terminal::new(TestBackend::new(60, 34)).unwrap();
    terminal.draw(|frame| render(frame, &status())).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();

    for required in ["PASSENGER", "PLAYING", "00:42:18", "GAIN +3 dB", "L", "R", "Q"] {
        assert!(text.contains(required), "missing {required}");
    }
}
```

- [ ] **Step 10: Run render tests and verify RED**

Run:

```bash
cargo test ui::tests::renders_amber_projection_console --locked
cargo test ui::tests::narrow_console_keeps_core_status_and_controls --locked
```

Expected: failures because the current three-line Paragraph lacks the approved layout.

- [ ] **Step 11: Implement the amber responsive console**

Import:

```rust
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
```

Define:

```rust
const AMBER: Color = Color::Rgb(255, 201, 40);
const BLACK: Color = Color::Black;

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(AMBER))
        .style(Style::default().fg(AMBER).bg(BLACK))
}
```

Implement `render` with Ratatui `Layout`:

- Fill `frame.area()` black.
- Use a 3:1 horizontal split at widths of at least 90 columns; otherwise use a
  vertical split that places the status rail below the main console.
- Main console rows: 2-line header, 3-line title panel, 1-line inverted status
  strip, 1-line time/gain strip, 6-line meter panel, 3-line double-bordered
  state banner, and the remaining height for five key panels.
- Render title and labels uppercase.
- Render status strips with `Style::default().fg(BLACK).bg(AMBER)`.
- Render each meter with `meter_bar`, its channel label, and numeric peak.
- Render the state banner with `BorderType::Double` and centered bold text.
- Render the right rail as bordered SYSTEM, GAIN VALUE, and PEAK VALUES panels.
- Split key panels horizontally when wide and into wrapped rows when narrow.

Update the `Status` construction in `run`:

```rust
gain_db: session.gain_db(),
levels: session.levels(),
```

- [ ] **Step 12: Run all UI tests and verify GREEN**

Run:

```bash
cargo test ui::tests --locked
```

Expected: all UI tests PASS.

- [ ] **Step 13: Run formatting and Clippy**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
```

Expected: both exit 0.

- [ ] **Step 14: Commit the TUI**

```bash
git add src/ui.rs
git commit -m "feat: add amber VU console"
```

### Task 3: Documentation and final verification

**Files:**
- Modify: `README.md:6-17`
- Modify: `README.md:49-54`

**Interfaces:**
- Consumes: completed gain and meter behavior
- Produces: user-facing control and display documentation

- [ ] **Step 1: Document the new behavior**

Change the display feature to:

```markdown
- Amber projection-console TUI with playback status, gain, and stereo peak meters
```

Add under Controls:

```markdown
- Up and Down adjust gain by 1 dB from -12 dB to +12 dB.
```

Add after the controls:

```markdown
The stereo VU meter shows the post-gain signal from -60 dB to 0 dB. Peak
markers hold for 3 seconds, then decay at 12 dB per second.
```

- [ ] **Step 2: Run the complete fresh verification gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --all-targets --locked
cargo test --all-targets --locked
cargo build --release --locked
go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.12 .github/workflows/ci.yml .github/workflows/release.yml
git diff --check
```

Expected: every command exits 0; all existing and new tests pass.

- [ ] **Step 3: Commit documentation**

```bash
git add README.md
git commit -m "docs: describe gain and VU controls"
```
