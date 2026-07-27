# Relative Paths and Audio Gain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve relative media arguments from the caller's working directory and add fixed +3 dB makeup gain after compression.

**Architecture:** Normalize both input paths once in `Config::parse`, so every downstream consumer receives an absolute validated file path. Reuse GStreamer's installed `volume` element in the single shared audio output chain.

**Tech Stack:** Rust 2024, standard library paths and filesystem APIs, GStreamer 1.28 Rust bindings, Cargo.

## Global Constraints

- Keep `owncast-stream VIDEO [SUBTITLES] [TITLE]` unchanged.
- Keep absolute paths, readable-file errors, the high-pass filter, compressor, AAC settings, and lobby silence unchanged.
- Set audio gain to exactly `1.4125375` (+3 dB).
- Add no dependency, gain option, normalization pass, or limiter.
- Keep `synthetic_handoff_stays_within_50ms` omitted.

---

### Task 1: Resolve relative media paths

**Files:**

- Modify: `src/main.rs`

**Interfaces:**

- Consumes: startup directory from `env::current_dir()` and a supplied media argument.
- Produces: `resolve_media_path(cwd: &Path, value: &str, name: &str) -> Result<PathBuf, Box<dyn Error>>`.

- [ ] **Step 1: Write the failing path test**

Add a test module at the end of `src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolves_relative_media_path_from_startup_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let resolved = resolve_media_path(root, "Cargo.toml", "video").unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, root.join("Cargo.toml").canonicalize().unwrap());
    }
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test --offline resolves_relative_media_path_from_startup_directory
```

Expected: compilation fails because `resolve_media_path` does not exist.

- [ ] **Step 3: Implement the shared resolver**

Import `Path` alongside `PathBuf`, then add:

```rust
fn resolve_media_path(
    cwd: &Path,
    value: &str,
    name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let supplied = PathBuf::from(value);
    let path = if supplied.is_absolute() {
        supplied
    } else {
        cwd.join(supplied)
    };
    if !path.is_file() {
        return Err(error(format!("Cannot read {name}: {value}")));
    }
    path.canonicalize()
        .map_err(|_| error(format!("Cannot read {name}: {value}")))
}
```

In `Config::parse`, resolve the startup directory once and use the helper for
both arguments:

```rust
let cwd = env::current_dir()?;
let video = resolve_media_path(&cwd, &values[0], "video")?;
let subtitles = values
    .get(1)
    .filter(|value| !value.is_empty())
    .map(|value| resolve_media_path(&cwd, value, "subtitles"))
    .transpose()?;
```

Remove the replaced `PathBuf::from` and `is_file` validation blocks.

- [ ] **Step 4: Run focused and complete tests**

Run:

```bash
cargo test --offline resolves_relative_media_path_from_startup_directory
cargo test --offline
```

Expected: focused test passes; complete suite passes with the existing decoder
test ignored.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "fix: resolve relative media paths"
```

---

### Task 2: Add fixed audio makeup gain

**Files:**

- Modify: `src/pipeline.rs`

**Interfaces:**

- Consumes: compressed `F32LE` stereo audio in the shared output chain.
- Produces: named GStreamer element `audio_gain` with `volume=1.4125375`.

- [ ] **Step 1: Write the failing pipeline assertion**

In `pipeline_has_one_sink_and_starts_on_lobby`, immediately after building
`parts`, add:

```rust
let gain = parts
    .pipeline
    .by_name("audio_gain")
    .expect("Audio gain element is missing");
assert_eq!(gain.property::<f64>("volume"), 1.4125375);
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test --offline pipeline_has_one_sink_and_starts_on_lobby
```

Expected: failure with `Audio gain element is missing`.

- [ ] **Step 3: Add the existing GStreamer element**

Add `"volume"` to `REQUIRED_ELEMENTS`. In the shared audio chain, insert the
named element after `audiodynamic` and before `avenc_aac`:

```text
! audiodynamic mode=compressor characteristics=soft-knee
    threshold=0.125 ratio=2.0
! volume name=audio_gain volume=1.4125375
! avenc_aac name=audio_encoder bitrate=192000
```

- [ ] **Step 4: Run focused and complete tests**

Run:

```bash
cargo test --offline pipeline_has_one_sink_and_starts_on_lobby
cargo test --offline
```

Expected: focused test passes; complete suite passes with the existing decoder
test ignored.

- [ ] **Step 5: Commit**

```bash
git add src/pipeline.rs
git commit -m "fix: raise movie audio by 3 dB"
```

---

### Task 3: Run final verification

**Files:**

- Verify: `src/main.rs`
- Verify: `src/pipeline.rs`

**Interfaces:**

- Consumes: completed path and gain commits.
- Produces: clean, buildable branch ready to publish.

- [ ] **Step 1: Run every static and test gate**

```bash
cargo fmt --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo build --offline --release
git diff --check
git status --short
```

Expected: every command exits zero; 33 unit tests pass, the existing decoder
test is ignored, 2 integration tests pass, and the worktree is clean.

- [ ] **Step 2: Verify branch ancestry**

```bash
git merge-base --is-ancestor origin/main HEAD
git log --oneline origin/main..HEAD
```

Expected: ancestry check exits zero and the log contains only the design,
relative-path, and +3 dB commits.
