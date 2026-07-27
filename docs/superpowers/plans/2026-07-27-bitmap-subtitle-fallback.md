# Bitmap Subtitle Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent bitmap-only embedded subtitles from stopping the movie encoder.

**Architecture:** Reject GStreamer subtitle streams whose caps are `subpicture/*` at the existing `candidate` boundary. The unchanged selector then falls back to an external subtitle or no subtitle.

**Tech Stack:** Rust 2024, GStreamer 1.x

## Global Constraints

- Add no dependency or new process.
- Preserve the continuous publisher and existing handoff behavior.
- Unsupported embedded bitmap subtitles are unavailable, not fatal.

---

### Task 1: Skip bitmap subtitle streams

**Files:**
- Modify: `src/pipeline.rs:49-77`
- Test: `src/pipeline.rs`

**Interfaces:**
- Consumes: `gst::Stream.caps`
- Produces: unchanged `candidate(stream: &gst::Stream) -> Option<StreamCandidate>`

- [ ] **Step 1: Write the failing regression test**

```rust
#[test]
fn ignores_bitmap_subtitle_streams() {
    let _gst = gst_test();
    let caps = gst::Caps::builder("subpicture/x-pgs").build();
    let stream = gst::Stream::new(
        Some("subtitle-1"),
        Some(&caps),
        gst::StreamType::TEXT,
        gst::StreamFlags::empty(),
    );

    assert_eq!(candidate(&stream), None);
}
```

- [ ] **Step 2: Run the regression test and verify RED**

Run: `cargo test --locked ignores_bitmap_subtitle_streams`

Expected: FAIL because `candidate` returns the PGS stream.

- [ ] **Step 3: Implement the minimal caps guard**

After identifying the stream kind in `candidate`, reject bitmap subtitle caps:

```rust
if kind == StreamKind::Subtitle
    && stream.caps().is_some_and(|caps| {
        caps.structure(0)
            .is_some_and(|structure| structure.name().as_str().starts_with("subpicture/"))
        })
{
    return None;
}
```

- [ ] **Step 4: Run focused and full verification**

Run:

```bash
cargo test --locked ignores_bitmap_subtitle_streams
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

Expected: all commands pass with no warnings.

- [ ] **Step 5: Build and verify the real MKV selection**

Run:

```bash
cargo build --release --locked
ffprobe -v error -select_streams s -show_entries stream=codec_name \
  -of default=nw=1:nk=1 \
  /opt/owncast/uploads/Vampire.Time.Travelers.1998.1080i.BluRay.HEVC.x265.DD.2.0-MAD.mkv
```

Expected: build succeeds; probe reports `hdmv_pgs_subtitle`, corresponding to
the rejected `subpicture/x-pgs` caps.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/pipeline.rs docs/superpowers
git commit -m "fix: skip unsupported bitmap subtitles"
```
