# Paused Frame Review Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make start, pause, and paused seeks freeze the correct playback-side frame without failing at startup or media end.

**Architecture:** Move frame capture from the repeating broadcast input to the normalized playback output. Track each captured buffer with a generation number so waits are content-driven; keep the current frozen frame when a paused seek reaches exact EOS.

**Tech Stack:** Rust, GStreamer 1.x Rust bindings, existing synthetic GStreamer/fakesink tests.

## Global Constraints

- Do not launch the streaming binary.
- Do not contact the host Owncast endpoint or read `/opt/owncast` secrets.
- Use only synthetic GStreamer sources and fakesinks in tests.
- Do not add dependencies, threads, or another media transport.
- Keep all waits bounded and preserve existing pipeline-error propagation.
- Keep decoder fixture tests omitted as requested.

---

### Task 1: Capture and wait for playback-side frames

**Files:**
- Modify: `src/pipeline.rs:180-870`
- Test: `src/pipeline.rs:1190-1535`

**Interfaces:**
- Consumes: the normalized I420 buffer entering `movie_video_output`
- Produces: `CapturedFrame { generation: u64, buffer: gst::Buffer }`, `PlaybackPipeline::frame_generation() -> u64`, and `PlaybackPipeline::wait_for_frame_after(u64, Duration) -> Result<gst::Buffer, Box<dyn Error>>`

- [ ] **Step 1: Write failing source-generation tests**

Add focused synthetic tests that:

```rust
#[test]
fn playback_capture_advances_only_for_source_frames() {
    // Push one playback-side buffer, record its generation, and prove the
    // repeating broadcast path cannot advance that generation.
}

#[test]
fn wait_for_frame_after_returns_the_new_source_buffer() {
    // Record generation N, perform a synthetic seek/frame push, then assert
    // the returned buffer belongs to generation N + 1.
}
```

The production change that must make these fail is moving capture ownership to
`PlaybackPipeline` and adding generation-aware waiting.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test pipeline::tests::playback_capture_advances_only_for_source_frames --locked
cargo test pipeline::tests::wait_for_frame_after_returns_the_new_source_buffer --locked
```

Expected: compilation failure because playback-side capture and wait APIs do
not exist.

- [ ] **Step 3: Implement playback-side capture**

Add the minimum shared state:

```rust
struct CapturedFrame {
    generation: u64,
    buffer: gst::Buffer,
}

struct PlaybackPipeline {
    // existing fields
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
}
```

Install one buffer probe on `movie_video_output`'s sink pad. Each playback
buffer replaces `latest_frame` and increments the previous generation.

Remove `BroadcastPipeline::latest_frame` and its probe. Change
`BroadcastPipeline::freeze` to accept the captured `gst::Buffer` directly.

Add:

```rust
fn frame_generation(&self) -> u64;
fn wait_for_frame_after(
    &self,
    generation: u64,
    timeout: Duration,
) -> Result<gst::Buffer, Box<dyn Error>>;
```

The bounded wait must iterate the GLib main context, return only when the
stored generation is newer, and fail immediately on playback bus errors.

- [ ] **Step 4: Run the source-generation tests and verify GREEN**

Run the two focused commands from Step 2.

Expected: both pass with no network access.

- [ ] **Step 5: Write failing session behavior tests**

Add or tighten synthetic tests:

```rust
#[test]
fn start_waits_for_first_movie_frame_before_playing() {
    // Delay the first synthetic video buffer and assert the session cannot
    // expose Playing before that buffer is captured.
}

#[test]
fn paused_seek_freezes_a_post_seek_source_frame() {
    // Pause, record generation N, seek, and assert the frozen buffer came
    // from a generation greater than N.
}

#[test]
fn paused_seek_to_duration_keeps_last_frame() {
    // Pause near the end, seek forward to exact duration, assert Ok, Paused,
    // and the existing frozen frame remains selected.
}
```

The first test should exercise a private start-transition helper so it does not
call the Owncast title API.

- [ ] **Step 6: Run session behavior tests and verify RED**

Run:

```bash
cargo test pipeline::tests::start_waits_for_first_movie_frame_before_playing --locked
cargo test pipeline::tests::paused_seek_freezes_a_post_seek_source_frame --locked
cargo test pipeline::tests::paused_seek_to_duration_keeps_last_frame --locked
```

Expected: failures showing the state changes too early, the repeated
broadcast-side frame satisfies the seek, and exact-duration seek times out.

- [ ] **Step 7: Wire start, pause, and seek to captured generations**

Use the shared playback capture in all three paths:

```rust
// start transition
let frame = self.playback.wait_for_frame_after(0, bounded_timeout)?;
self.broadcast.select_movie();
// only then expose PlaybackState::Playing

// pause
let frame = self.playback.latest_frame()?;
self.broadcast.freeze(frame)?;

// paused seek below duration
let generation = self.playback.frame_generation();
self.playback.pipeline.seek_simple(flags, target)?;
let frame = self.playback.wait_for_frame_after(generation, bounded_timeout)?;
self.broadcast.freeze(frame)?;

// paused seek at exact duration
// keep the currently selected frozen frame and return Ok(())
```

Keep public hotkeys and state names unchanged. Do not suppress unrelated
errors.

- [ ] **Step 8: Run focused and full verification**

Run:

```bash
cargo test pipeline::tests::start_waits_for_first_movie_frame_before_playing --locked
cargo test pipeline::tests::paused_seek_freezes_a_post_seek_source_frame --locked
cargo test pipeline::tests::paused_seek_to_duration_keeps_last_frame --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --all-targets --locked
cargo test --all-targets --locked
cargo build --release --locked
git diff --check
```

Expected: every command exits zero; no ignored decoder fixture is restored.

- [ ] **Step 9: Commit**

```bash
git add src/pipeline.rs
git commit -m "fix: freeze playback-side frames"
```

- [ ] **Step 10: Push and answer the three review threads**

Push `feat/playback-controls`, then reply in each inline thread:

- source-frame thread: state that frame generation now originates before
  `intervideosink`;
- end-seek thread: state that exact-duration paused seeks retain the last
  frozen frame;
- startup thread: state that `Playing` is exposed only after the first captured
  movie frame.

Include the focused test names and resolve each thread after the reply.
