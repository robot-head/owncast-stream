# Bitmap Subtitle Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent bitmap-only embedded subtitles from stopping the movie encoder.

**Architecture:** Filter FFmpeg's four bitmap subtitle codec names at the existing `select_subtitle` boundary. Preserve original stream indexes and the existing English-first preference so `movie_command` automatically falls back to an external subtitle or no subtitle.

**Tech Stack:** Rust 2024, `ffprobe` crate, installed FFmpeg

## Global Constraints

- Add no dependency or new process.
- Preserve the continuous publisher and existing handoff behavior.
- Unsupported embedded bitmap subtitles are unavailable, not fatal.

---

### Task 1: Skip bitmap subtitle streams

**Files:**
- Modify: `src/main.rs:122-138`
- Test: `src/main.rs:433-465`

**Interfaces:**
- Consumes: `ffprobe::Stream.codec_name`, `ffprobe::Stream.tags.language`
- Produces: unchanged `select_subtitle(streams: &[Stream]) -> Option<usize>`

- [ ] **Step 1: Write the failing regression test**

Change the test helper to accept a codec and add the PGS regression:

```rust
fn subtitle(codec: &str, language: &str) -> Stream {
    Stream {
        codec_name: Some(codec.into()),
        codec_type: Some("subtitle".into()),
        tags: Some(StreamTags {
            language: Some(language.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn ignores_bitmap_embedded_subtitles() {
    assert_eq!(
        super::select_subtitle(&[subtitle("hdmv_pgs_subtitle", "eng")]),
        None
    );
}
```

Update existing helper calls to pass `"subrip"`.

- [ ] **Step 2: Run the regression test and verify RED**

Run: `cargo test --locked ignores_bitmap_embedded_subtitles`

Expected: FAIL because `select_subtitle` returns `Some(0)`.

- [ ] **Step 3: Implement the minimal codec filter**

Add the FFmpeg bitmap codec predicate and apply it to both preference searches:

```rust
fn is_text_subtitle(stream: &Stream) -> bool {
    !matches!(
        stream.codec_name.as_deref(),
        Some("dvd_subtitle" | "dvb_subtitle" | "xsub" | "hdmv_pgs_subtitle")
    )
}

fn select_subtitle(streams: &[Stream]) -> Option<usize> {
    streams
        .iter()
        .position(|stream| {
            is_text_subtitle(stream)
                && stream
                    .tags
                    .as_ref()
                    .and_then(|tags| tags.language.as_deref())
                    == Some("eng")
        })
        .or_else(|| streams.iter().position(is_text_subtitle))
}
```

- [ ] **Step 4: Run focused and full verification**

Run:

```bash
cargo test --locked ignores_bitmap_embedded_subtitles
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

Expected: all commands pass with no warnings.

- [ ] **Step 5: Build and verify the real MKV selection**

Run:

```bash
cargo build --release --locked
ffprobe -v error -select_streams s \
  -show_entries stream=index,codec_name:stream_tags=language \
  -of compact=p=0:nk=1 \
  /opt/owncast/uploads/Vampire.Time.Travelers.1998.1080i.BluRay.HEVC.x265.DD.2.0-MAD.mkv
```

Expected: build succeeds; probe reports `hdmv_pgs_subtitle`, which the regression now rejects.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs docs/superpowers/plans/2026-07-27-bitmap-subtitle-fallback.md
git commit -m "fix: skip bitmap subtitle tracks"
```
