# Retained A/V Boundary Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the offline retained-HLS validator measure audio transport coverage at the movie video boundary instead of mistaking later audible content for the audio handoff.

**Architecture:** Clone the frozen Attempt 7 validator into a separate candidate and change only its boundary helper and reporting. Reuse the existing mapped `BufferTiming` intervals and unchanged 50 ms constant; later language and subtitle classifiers continue to use the resulting boundary. Freeze and review the candidate before one sealed evaluation of the existing evidence.

**Tech Stack:** Rust, GStreamer Rust bindings, Cargo offline builds, SHA-256 evidence manifests.

## Global Constraints

- Do not modify the production `owncast-stream` repository source or installed binary.
- Preserve the sealed Attempt 7 evidence and frozen failed evaluator byte-for-byte.
- Keep the 50 ms timing threshold unchanged.
- Keep evidence parsing, adapter generation, decoding, language classification, subtitle classification, source calibration, and `run_real` behavior unchanged except for the boundary call and its status text.
- Do not add dependencies, markers, network access, retries, or threshold tuning.
- Use test-first RED/GREEN evidence for every behavior change.
- Run the final retained evaluation exactly once after independent review.

---

### Task 1: Use mapped audio transport at the movie boundary

**Files:**

- Create by copying frozen candidate: `/var/tmp/attempt7-validator-fix/`
- Modify: `/var/tmp/attempt7-validator-fix/src/main.rs:2482-2500`
- Test: `/var/tmp/attempt7-validator-fix/src/main.rs:4890-4950`
- Create: `/var/tmp/attempt7-validator-fix/RETAINED_AV_BOUNDARY_DESIGN.md`
- Create: `/var/tmp/attempt7-validator-fix/RETAINED_AV_BOUNDARY_PLAN.md`

**Interfaces:**

- Consumes: `BufferTiming { pts_ns: u64, end_ns: u64 }`, `AudioCapture.buffer_timing`, and `MAX_SEGMENT_BOUNDARY_DELTA_NS`.
- Produces: `audio_transport_boundary(timing: &[BufferTiming], video_ns: u64) -> Result<u64, Box<dyn Error>>`.
- Preserves: `detect_movie_boundaries(...) -> Result<(u64, u64), Box<dyn Error>>`.

- [ ] **Step 1: Copy and verify the frozen starting point**

```bash
sudo cp -a /var/tmp/attempt7-validator /var/tmp/attempt7-validator-fix
sudo sha256sum -c /var/tmp/attempt7-validator-fix/CHECKSUMS.sha256
sudo sha256sum \
  /var/tmp/attempt7-validator/src/main.rs \
  /var/tmp/attempt7-validator-fix/src/main.rs
```

Expected: the copied manifest passes and both source hashes are
`3475be910a390812fc3c225f5bfcac6a086a126d581d8fc0d1f4523d7d57ab28`.

- [ ] **Step 2: Replace the existing onset-based boundary test with failing transport tests**

Add tests that call the wished-for helper:

```rust
#[test]
fn audio_transport_boundary_uses_interval_containing_video_even_when_audio_is_silent() {
    let timing = vec![
        BufferTiming { pts_ns: 80_000_000, end_ns: 110_000_000 },
        BufferTiming { pts_ns: 110_000_000, end_ns: 140_000_000 },
    ];
    assert_eq!(
        audio_transport_boundary(&timing, 100_000_000).unwrap(),
        100_000_000
    );
}

#[test]
fn audio_transport_boundary_uses_nearest_bounded_edge() {
    let timing = vec![
        BufferTiming { pts_ns: 0, end_ns: 90_000_000 },
        BufferTiming { pts_ns: 110_000_000, end_ns: 130_000_000 },
    ];
    assert_eq!(
        audio_transport_boundary(&timing, 100_000_000).unwrap(),
        90_000_000
    );
}

#[test]
fn audio_transport_boundary_rejects_missing_inverted_and_distant_timing() {
    assert!(audio_transport_boundary(&[], 100_000_000).is_err());
    assert!(
        audio_transport_boundary(
            &[BufferTiming { pts_ns: 110_000_000, end_ns: 100_000_000 }],
            105_000_000,
        )
        .is_err()
    );
    assert!(
        audio_transport_boundary(
            &[BufferTiming { pts_ns: 0, end_ns: 40_000_000 }],
            100_000_001,
        )
        .is_err()
    );
}
```

Update the combined boundary test so its `AudioCapture` has mapped timing
covering the selected `100_000_000 ns` video boundary while its samples remain
silent for several seconds. Expect `(100_000_000, 100_000_000)`.

- [ ] **Step 3: Run focused tests and verify RED**

```bash
cd /var/tmp/attempt7-validator-fix
timeout 120s cargo test --offline audio_transport_boundary -- --nocapture
```

Expected: compilation fails because `audio_transport_boundary` does not exist.
Capture the complete output as `tdd-red.log`.

- [ ] **Step 4: Implement the minimal transport helper**

Add immediately before `detect_movie_boundaries`:

```rust
fn audio_transport_boundary(
    timing: &[BufferTiming],
    video_ns: u64,
) -> Result<u64, Box<dyn Error>> {
    if timing.is_empty() || timing.iter().any(|buffer| buffer.end_ns < buffer.pts_ns) {
        return Err("decoded audio timing cannot locate the movie boundary".into());
    }
    if timing
        .iter()
        .any(|buffer| buffer.pts_ns <= video_ns && video_ns <= buffer.end_ns)
    {
        return Ok(video_ns);
    }
    let nearest = timing
        .iter()
        .flat_map(|buffer| [buffer.pts_ns, buffer.end_ns])
        .min_by_key(|edge| edge.abs_diff(video_ns))
        .ok_or("decoded audio timing cannot locate the movie boundary")?;
    if nearest.abs_diff(video_ns) > MAX_SEGMENT_BOUNDARY_DELTA_NS as u64 {
        return Err("A/V timing exceeds 50 ms".into());
    }
    Ok(nearest)
}
```

Change only the audio side of `detect_movie_boundaries`:

```rust
let movie_audio = audio_transport_boundary(&audio.buffer_timing, movie_video)?;
```

Do not remove `first_sustained_non_silent_pts`; the reviewed diagnostic still
uses it to explain content onset.

- [ ] **Step 5: Run focused tests and verify GREEN**

```bash
timeout 120s cargo test --offline audio_transport_boundary -- --nocapture
timeout 120s cargo test --offline movie_boundaries_are_derived -- --nocapture
```

Expected: all focused tests pass. Capture complete output as `tdd-green.log`.

- [ ] **Step 6: Make reporting describe transport rather than audibility**

Replace the misleading `minimum_audio_rms` and `sustained_audio_windows`
fields in the `movie_boundary` status line with:

```text
audio_boundary=transport_coverage
```

Continue printing exact `movie_video_ns`, `movie_audio_ns`, and `av_delta_ns`.
Do not change later uses of `movie_audio`.

- [ ] **Step 7: Run the complete non-media gate**

```bash
timeout 120s cargo fmt --check
timeout 120s cargo test --offline
timeout 120s cargo clippy --offline -- -D warnings
timeout 120s cargo build --offline --release
timeout 120s target/release/attempt4-validator --selfcheck
sha256sum Cargo.toml Cargo.lock src/main.rs target/release/attempt4-validator
```

Expected: formatting passes, all inherited and new tests pass, Clippy reports
no warnings, release build and selfcheck pass. Capture commands, output, exit
codes, UTC start/end, user, and working directory in `nonmedia-gates.log`.

- [ ] **Step 8: Freeze the candidate**

Create `CHECKSUMS.sha256` for `Cargo.toml`, `Cargo.lock`, `src/main.rs`, and the
release binary. Record hashes of the frozen failed evaluator and prove they
remain:

```text
source 3475be910a390812fc3c225f5bfcac6a086a126d581d8fc0d1f4523d7d57ab28
binary 1307d76b7c980801b6975140ae3f681e6dc24bbfd133302b8a17230a9f98cbfe
```

Do not run any evidence, adapter, timeline, or retained-content command.

---

### Task 2: Independently review the frozen validator fix

**Files:**

- Create: `/var/tmp/attempt7-validator-fix-review/`
- Read: `/var/tmp/attempt7-validator-fix/src/main.rs`
- Compare: `/var/tmp/attempt7-validator/src/main.rs`

**Interfaces:**

- Consumes: Task 1 candidate, RED/GREEN logs, gate log, and hashes.
- Produces: a checksum-sealed review package with a clean or actionable finding.

- [ ] **Step 1: Build the review package**

Include prior and candidate source, focused diff, both checksum manifests,
design, plan, TDD logs, and authenticated non-media gate log. Generate
`PACKAGE-SHA256SUMS` from the package root and verify it.

- [ ] **Step 2: Perform an independent scoped review**

Verify:

- the containing-interval and nearest-edge rules match the approved spec;
- exact 50 ms passes and 50 ms plus 1 ns fails;
- inverted and empty timing fail;
- the long-silence regression fails before and passes after the change;
- only boundary selection and status text changed;
- the 50 ms constant, evidence parser, adapter, decoders, continuity gate,
  language/subtitle classifiers, source calibration, and source windows are
  byte-identical;
- no retained media command ran during preparation.

Expected: no unresolved correctness, security, or evidence findings.

- [ ] **Step 3: Address findings test-first**

For any finding, add one focused failing unit test, capture RED, make the
smallest correction, capture GREEN, rerun the full non-media gate, refresh
hashes, and request another scoped review. Do not execute retained media until
the review is clean.

---

### Task 3: Run one sealed retained-content evaluation

**Files:**

- Read-only: `/var/tmp/owncast-task5-attempt7-20260726T135920Z/`
- Execute: `/var/tmp/attempt7-validator-fix/target/release/attempt4-validator`
- Create: fresh `/var/tmp/Attempt7-evaluator-fixed-<UTC>/`
- Append ignored report: `.superpowers/sdd/2026-07-26-gstreamer-pipeline/task-5-report.md`
- Append ignored ledger: `.superpowers/sdd/2026-07-26-gstreamer-pipeline/progress.md`

**Interfaces:**

- Consumes: independently reviewed Task 2 binary and sealed Attempt 7 evidence.
- Produces: one immutable evaluation result proving or rejecting all Task 5 content and timing gates.

- [ ] **Step 1: Verify immutable inputs and safe host state**

Verify the candidate package manifest, candidate checksums, evidence manifest
SHA `5be196214daf97111d9159d7f5a671e8177f5ee1ad3dde46f754a08f2919c1cd`,
read-only evidence modes, no evidence symlinks, Owncast offline, helper count
zero, established RTMP count zero, and clean repository.

- [ ] **Step 2: Execute exactly one frozen evaluation**

```bash
timeout 900s /var/tmp/attempt7-validator-fix/target/release/attempt4-validator \
  --real-retained \
  /var/tmp/owncast-task5-attempt7-20260726T135920Z \
  /opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.mkv \
  /opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.en.srt \
  /var/tmp/Attempt7-evaluator-fixed-<UTC>
```

Run once only. Capture complete stdout/stderr, exit code, UTC start/end, and
wall time. Do not retry or change code, thresholds, or evidence after seeing
the result.

- [ ] **Step 3: Require the complete result**

Expected:

- `audio_boundary=transport_coverage`;
- A/V boundary delta at most 50 ms;
- every mapped continuity and adapter-boundary delta at most 50 ms;
- source controls pass;
- retained audio classifies as English and rejects Italian;
- burned English subtitle content passes;
- subtitle/video delta is at most 50 ms;
- retained coverage reaches every required source window.

Any missing or inconclusive metric is a failed Task 5 gate.

- [ ] **Step 4: Seal and record evidence**

Hash every output and the execution log, write a sibling SHA-256 manifest,
set files to `0444` and directories to `0555`, and verify all hashes. Recheck
candidate/evidence immutability, helper count zero, RTMP count zero, Owncast
offline, and repository cleanliness. Append the exact result to the ignored
Task 5 report and progress ledger.

- [ ] **Step 5: Commit only the implementation plan**

The validator remains an external review artifact. Commit this repository
plan document without adding `/var/tmp` artifacts:

```bash
git add docs/superpowers/plans/2026-07-26-retained-av-boundary.md
git commit -m "docs: plan retained A/V boundary fix"
```
