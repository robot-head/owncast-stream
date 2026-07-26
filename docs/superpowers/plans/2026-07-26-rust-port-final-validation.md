# Rust Port Final Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Revoke the exposed Authelia sessions, repair the retained-media validator's subtitle pad wiring, obtain conclusive retained-media results, and publish the verified Rust port.

**Architecture:** Keep the production pipeline unchanged unless retained validation identifies a production defect. Create a fresh validator candidate from the frozen Attempt 13 source, first extract its video-branch constructor without behavioral change, then use a non-decoder RED/GREEN pad-wiring test to replace ambiguous automatic ghosting with one explicit video input ghost pad. Run one retained evaluation per frozen candidate and preserve every result.

**Tech Stack:** Rust 2024, Cargo offline mode, GStreamer Rust bindings, Authelia 4.39, Redis 7, Docker Compose, SHA-256 manifests, Git.

## Global Constraints

- Keep both decoder-backed fixture tests omitted.
- Add no dependency, classifier, threshold, retry loop, or production feature.
- Do not weaken any retained audio, subtitle, coverage, continuity, source-control, or 50 ms timing gate.
- Do not print, copy, or persist an Authelia cookie or Redis password.
- Keep the existing sealed Attempt 13 inputs immutable.
- Preserve every evaluator result; never rerun the same frozen candidate.
- Install production only if the validated production binary changes.

---

### Task 1: Revoke Authelia sessions

**Files:**

- Read: `/opt/owncast/compose.yml`
- Read: `/opt/owncast/secrets/redis_password`
- Append: `.superpowers/sdd/2026-07-26-rust-port-final-validation/progress.md`

**Interfaces:**

- Consumes: dedicated `owncast-redis` session store and its mounted secret.
- Produces: empty active-session database and healthy Authelia/Redis services.

- [ ] **Step 1: Record safe prestate**

Run without exposing secret values:

```bash
sudo docker inspect owncast-redis --format '{{.State.Health.Status}}'
sudo docker inspect authelia --format '{{.State.Health.Status}}'
sudo docker exec owncast-redis sh -c \
  'REDISCLI_AUTH="$(cat /run/secrets/redis_password)" redis-cli DBSIZE'
```

Expected: both containers report `healthy`; record only the integer database
size.

- [ ] **Step 2: Flush only the selected Redis database**

```bash
sudo docker exec owncast-redis sh -c \
  'REDISCLI_AUTH="$(cat /run/secrets/redis_password)" redis-cli FLUSHDB'
```

Expected: exact output `OK`.

- [ ] **Step 3: Verify revocation and auth health**

```bash
sudo docker exec owncast-redis sh -c \
  'REDISCLI_AUTH="$(cat /run/secrets/redis_password)" redis-cli DBSIZE'
sudo docker inspect owncast-redis --format '{{.State.Health.Status}}'
sudo docker inspect authelia --format '{{.State.Health.Status}}'
curl -fsS -o /dev/null -w '%{http_code}\n' https://auth.djspacecat.com/api/health
curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' \
  https://git.djspacecat.com/
curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' \
  https://claude.djspacecat.com/
```

Expected: database size `0`, both containers `healthy`, Authelia health `200`,
and both protected applications redirect to Authelia.

---

### Task 2: Create a fresh validator candidate

**Files:**

- Copy: `/var/tmp/attempt13-validator/`
- Create: `/var/tmp/attempt14-validator/`
- Modify: `/var/tmp/attempt14-validator/src/main.rs`
- Create: `/var/tmp/attempt14-validator/BASELINE.sha256`

**Interfaces:**

- Consumes: frozen Attempt 13 validator source SHA-256
  `3dd8c01d08606c9603ff92c7f26946a572cfbda19803dd460a5c9e32eed70aba`.
- Produces: editable Attempt 14 candidate with a behavior-preserving
  `build_video_branch(request: &VideoRequest) -> Result<gst::Bin, Box<dyn Error>>`.

- [ ] **Step 1: Verify and copy the frozen candidate**

```bash
sudo sh -c 'cd /var/tmp/attempt13-validator && sha256sum -c CHECKSUMS.sha256'
test ! -e /var/tmp/attempt14-validator
sudo cp -a /var/tmp/attempt13-validator /var/tmp/attempt14-validator
sudo chown -R matt:matt /var/tmp/attempt14-validator
chmod -R u+rwX,go-rwx /var/tmp/attempt14-validator
sha256sum /var/tmp/attempt14-validator/src/main.rs
```

Require the exact source hash above. Record it in `BASELINE.sha256`.

- [ ] **Step 2: Extract the current video branch without changing behavior**

Move the current branch-description and parse call from `decode_video` into:

```rust
fn build_video_branch(
    request: &VideoRequest,
) -> Result<gst::Bin, Box<dyn Error>>
```

The helper must initially preserve `bin_from_description(&chain, true)` and
return the parsed bin. Keep collector lookup, pipeline membership, and
subtitle-source setup in `decode_video`. Replace only the corresponding
branch-construction code with one helper call.

- [ ] **Step 3: Prove the extraction is behavior-preserving**

```bash
cd /var/tmp/attempt14-validator
cargo fmt --check
cargo test --offline -- \
  --skip local_hls_with_large_transport_pts_decodes_at_zero_running_time \
  --skip local_lobby_movie_switch_fixture_calibrates_boundaries
cargo clippy --offline -- -D warnings
```

Expected: 44 passed, 0 failed, exactly 2 filtered out, and no warnings.
Capture complete commands, output, and statuses in `refactor-green.log`.

---

### Task 3: Repair subtitle pad wiring test-first

**Files:**

- Modify: `/var/tmp/attempt14-validator/src/main.rs`
- Create: `/var/tmp/attempt14-validator/pad-wiring-red.log`
- Create: `/var/tmp/attempt14-validator/pad-wiring-green.log`

**Interfaces:**

- Consumes: `build_video_branch(&VideoRequest)`.
- Produces: a video branch with explicit `video_sink` and `subtitle_sink`
  ghost pads.

- [ ] **Step 1: Write the non-decoder regression test**

Add this test to the existing test module:

```rust
#[test]
fn subtitle_video_branch_has_independent_inputs() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let request = VideoRequest {
        start_ns: 0,
        end_ns: 1,
        step_ns: 1,
        width: 320,
        height: 180,
        subtitle: Some(PathBuf::from("unused.srt")),
    };
    let branch = build_video_branch(&request).unwrap();
    let subtitle_source = gst::ElementFactory::make("fakesrc").build().unwrap();
    pipeline
        .add_many([branch.upcast_ref(), &subtitle_source])
        .unwrap();

    subtitle_source
        .static_pad("src")
        .unwrap()
        .link(&branch.static_pad("subtitle_sink").unwrap())
        .unwrap();
    assert_eq!(
        branch.static_pad("video_sink").unwrap().direction(),
        gst::PadDirection::Sink
    );
}
```

- [ ] **Step 2: Run the test and verify RED**

```bash
cd /var/tmp/attempt14-validator
cargo test --offline subtitle_video_branch_has_independent_inputs
```

Expected: fail because the current automatically ghosted branch does not
provide both named inputs. Preserve the initial direct-link RED containing
`WasLinked` and the revised named-input RED. Capture the complete results in
`pad-wiring-red.log` and `pad-wiring-red-two-pads.log`.

- [ ] **Step 3: Implement the minimal explicit-pad repair**

In `build_video_branch`:

1. name the first queue `video_input`;
2. change `bin_from_description(&chain, true)` to
   `bin_from_description(&chain, false)`;
3. create and add:

```rust
let video_sink = gst::GhostPad::builder_with_target(
    &branch
        .by_name("video_input")
        .ok_or("video input queue is missing")?
        .static_pad("sink")
        .ok_or("video input pad is missing")?,
)?
.name("video_sink")
.build();
branch.add_pad(&video_sink)?;
```

4. create a second ghost pad named `subtitle_sink` targeting
   `overlay.subtitle_sink` when subtitles are requested.

In `decode_video`, link the selected video pad to
`branch.static_pad("video_sink")` and the subtitle parser to
`branch.static_pad("subtitle_sink")`.

- [ ] **Step 4: Verify GREEN and all non-decoder gates**

```bash
cd /var/tmp/attempt14-validator
cargo test --offline subtitle_video_branch_has_independent_inputs
cargo fmt --check
cargo test --offline -- \
  --skip local_hls_with_large_transport_pts_decodes_at_zero_running_time \
  --skip local_lobby_movie_switch_fixture_calibrates_boundaries
cargo clippy --offline -- -D warnings
cargo build --offline --release
target/release/attempt4-validator --selfcheck
```

Expected: focused test passes; 45 passed, 0 failed, exactly 2 filtered out;
formatting, Clippy, release build, and selfcheck all pass. Capture complete
results in `pad-wiring-green.log`.

---

### Task 4: Freeze and independently inspect Attempt 14

**Files:**

- Create: `/var/tmp/attempt14-validator/CHECKSUMS.sha256`
- Create: `/var/tmp/attempt14-validator-review/`
- Create: `/var/tmp/attempt14-validator-review/PACKAGE-SHA256SUMS`

**Interfaces:**

- Consumes: repaired candidate and RED/GREEN evidence.
- Produces: immutable root-only validator and review package approved for one
  retained evaluation.

- [ ] **Step 1: Freeze candidate hashes**

Hash `Cargo.toml`, `Cargo.lock`, `src/main.rs`, release binary,
`BASELINE.sha256`, `refactor-green.log`, `pad-wiring-red.log`, and
`pad-wiring-green.log`. Verify every entry, then set candidate ownership
`root:root`, directory modes `0500`, executable mode `0500`, and other file
modes `0400`.

- [ ] **Step 2: Build a focused review package**

Include prior and repaired source, a unified diff, candidate checksums,
RED/GREEN logs, the approved design, and this implementation plan. Hash every
package file and verify the package manifest.

- [ ] **Step 3: Review exact scope**

Require the review to prove:

- evidence digests, thresholds, source windows, classifiers, adapter,
  continuity code, and `run_real` are unchanged;
- the behavioral refactor only extracts existing construction;
- the repair only names the input queue, disables automatic ghosting, adds
  explicit video/subtitle ghost pads, and selects those bin-boundary pads in
  `decode_video`;
- the new test performs no media decode;
- exactly the two approved decoder tests remain omitted;
- all candidate and package hashes verify.

Stop before retained media on any Important or Critical finding.

---

### Task 5: Run and seal one Attempt 14 retained evaluation

**Files:**

- Execute: `/var/tmp/attempt14-validator/target/release/attempt4-validator`
- Read-only: `/var/tmp/owncast-task5-attempt13-20260726T195605Z/`
- Create: `/var/tmp/Attempt14-evaluator-$eval_stamp/`
- Create: `/var/tmp/Attempt14-evaluator-run-$eval_stamp/`

**Interfaces:**

- Consumes: reviewed Attempt 14 binary and immutable Attempt 13 media.
- Produces: one sealed conclusive evaluator result for that binary.

- [ ] **Step 1: Run a credential-safe preflight**

Verify candidate, review package, all Attempt 13 input manifests, movie/SRT
hashes and readability, Owncast `online=false`, helper/validator count zero,
RTMP count zero, protected-route redirect, clean worktree, and committed plan.
Use `/proc/*/exe` for exact process matching; do not use overlength `pgrep -x`.
Persist status codes but no response headers.

- [ ] **Step 2: Execute the frozen candidate once**

Create names once:

```bash
eval_stamp=$(date -u +%Y%m%dT%H%M%SZ)
eval_output="/var/tmp/Attempt14-evaluator-$eval_stamp"
eval_run="/var/tmp/Attempt14-evaluator-run-$eval_stamp"
test ! -e "$eval_output"
test ! -e "$eval_run"
sudo install -d -m 0700 -o root -g root "$eval_run"
```

Run under a root shell with complete stdout/stderr redirected to a root-only
file. The shell must disable immediate-exit around the evaluator, save `$?`
immediately, then persist command, UTC times, monotonic wall time, and exact
exit status in `execution.tsv`. Invoke:

```bash
timeout 900s \
  /var/tmp/attempt14-validator/target/release/attempt4-validator \
  --real-retained \
  /var/tmp/owncast-task5-attempt13-20260726T195605Z \
  /opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.mkv \
  /opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.en.srt \
  "$eval_output"
```

Do not use `tee`; do not run the same frozen candidate again.

- [ ] **Step 3: Evaluate every gate**

Require conclusive PASS for transport boundary, A/V delta, all decoded
continuity and adapter-boundary deltas, source controls, English language,
Italian rejection, burned English subtitle content, subtitle/video timing,
and all three retained source windows.

- [ ] **Step 4: Seal safely**

Hash every output and execution artifact, reject symlinks, set ownership
`root:root`, directories `0500`, and files `0400`. Reverify manifests and
poststate without recording cookies or authorization headers.

If the result exposes another validator defect, record FAIL and begin a newly
frozen test-first candidate. Never tune or rerun this candidate.

---

### Task 6: Verify and publish the finished Rust port

**Files:**

- Verify: `src/`
- Verify: `tests/`
- Append: `.superpowers/sdd/2026-07-26-rust-port-final-validation/progress.md`
- Append: `.superpowers/sdd/2026-07-26-rust-port-final-validation/final-report.md`

**Interfaces:**

- Consumes: conclusive retained-media PASS.
- Produces: verified local branch and matching published remote branch.

- [ ] **Step 1: Run full production verification**

```bash
cargo fmt --check
cargo test --offline
cargo clippy --offline -- -D warnings
cargo build --offline --release
```

Require every command to exit zero with no warnings.

- [ ] **Step 2: Verify deployment boundary**

```bash
sha256sum target/release/owncast-stream /usr/local/bin/owncast-stream
curl -fsS http://127.0.0.1:8081/api/status
sudo docker inspect owncast --format '{{.State.Status}}'
```

Require matching binary hashes, Owncast HTTP 200 with `online=false`, running
container, zero helper/validator processes, and zero established RTMP
connections. Install only if the two binary hashes differ due to an approved
production change.

- [ ] **Step 3: Record and commit final evidence**

Append sanitized hashes, commands, statuses, retained metrics, and artifact
paths to the SDD ledger/report. Run `git diff --check` and `git status
--short`, then commit only tracked final-validation records.

- [ ] **Step 4: Publish and verify**

```bash
git push -u origin feat/gstreamer-pipeline
git ls-remote --heads origin feat/gstreamer-pipeline
git rev-parse HEAD
```

Require the remote branch hash to equal local `HEAD`.
