# Attempt 13 Retained Validator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind the reviewed retained-media validator to sealed Attempt 13 evidence and execute exactly one complete retained-content evaluation.

**Architecture:** Copy the reviewed Attempt 7 validator into a new root-only candidate and replace only its manifest, final-playlist, and deterministic-adapter digest constants. Prove the binding through static adapter preparation, independently review the frozen package, then run one persisted-status `--real-retained` evaluation.

**Tech Stack:** Rust 2024, Cargo offline mode, GStreamer Rust bindings, SHA-256 manifests, existing sealed HLS evidence.

## Global Constraints

- Do not modify the production repository source, installed streamer, Owncast, collector, sealed evidence, prior validator candidate, or prior review package.
- Add no dependency, CLI option, network request, retry, marker, threshold change, classifier change, decoder change, or timing change.
- Change executable code only at `RETAINED_MANIFEST_SHA256`, `RETAINED_PLAYLIST_SHA256`, and `RETAINED_ADAPTER_SHA256`.
- Bind the root manifest to `bcc58ba6faefe553a985e0aa91726fda3090ba51f63358fa5cb4c3a254b2db62`.
- Bind the final live playlist to `a336d16633268688345bd7b742df7a5530ce99425ac3f66a906661a53cc03bdc`.
- Derive and freeze the adapter digest through static preparation before any retained decode.
- Keep every existing 50 ms threshold, content window, transport-boundary rule, continuity check, source control, language classifier, and subtitle classifier unchanged.
- Run `--real-retained` exactly once, only after independent review passes.
- Do not retry or modify code, thresholds, evidence, or inputs after the real evaluation starts.

---

### Task 1: Rebind and freeze the validator

**Files:**

- Copy unchanged: `/var/tmp/attempt7-validator-fix/`
- Create: `/var/tmp/attempt13-validator/`
- Modify: `/var/tmp/attempt13-validator/src/main.rs:45-52`
- Create: `/var/tmp/attempt13-validator/ATTEMPT13_EVIDENCE_BINDING_DESIGN.md`
- Create: `/var/tmp/attempt13-validator/ATTEMPT13_EVIDENCE_BINDING_PLAN.md`
- Create: `/var/tmp/attempt13-validator/CHECKSUMS.sha256`

**Interfaces:**

- Consumes: sealed evidence `/var/tmp/owncast-task5-attempt13-20260726T195605Z`.
- Static CLI: `attempt4-validator --prepare-retained-adapter EVIDENCE ADAPTER_DIR`.
- Produces: root-only frozen validator bound to Attempt 13.

- [ ] **Step 1: Verify and copy immutable inputs**

Verify:

```bash
sudo sh -c 'cd /var/tmp/attempt7-validator-fix && sha256sum -c CHECKSUMS.sha256'
sudo sh -c 'cd /var/tmp/attempt7-validator-fix-review && sha256sum -c PACKAGE-SHA256SUMS'
sudo sh -c 'cd /var/tmp/owncast-task5-attempt13-20260726T195605Z && sha256sum -c manifest.sha256'
sudo sh -c 'cd /var/tmp/attempt13-run-20260726T195605Z && sha256sum -c manifest.sha256'
sudo sh -c 'cd /var/tmp/attempt13-preflight-20260726T195544Z && sha256sum -c manifest.sha256'
sudo sh -c 'cd /var/tmp/attempt13-provenance-supplement-20260726T200935Z && sha256sum -c manifest.sha256'
```

Require these verified prior hashes before copying:

```text
source=e49a2382367f31c76b362b8615e3039dc5f905753c16c4b4599eaa4fba272b31
binary=dcc2b8f96ba8e9e95e6451adc648264848154139316e2cef5bd47f3afa5c1520
candidate_manifest=cc7ded51852a34687127a7a88214e1c6bfa7a30b08a9438e246a96809a314281
review_package=8783849465e0f9693ee058fc0e44f5f3653f29804062f234a2ef9148e16b8659
```

Require `/var/tmp/attempt13-validator` not to exist, then copy with
`sudo cp -a`. Do not invoke any validator command yet.

- [ ] **Step 2: Run the behavioral RED against the unchanged copy**

Use a fresh nonexistent adapter path and capture stdout, stderr, exit status,
UTC start/end, user, command, and working directory in
`tdd-manifest-red.log`:

```bash
sudo timeout 120s \
  /var/tmp/attempt13-validator/target/release/attempt4-validator \
  --prepare-retained-adapter \
  /var/tmp/owncast-task5-attempt13-20260726T195605Z \
  /var/tmp/attempt13-adapter-red
```

Expected: nonzero with exact error `retained manifest digest mismatch`.
Require that no media decoder, timeline diagnostic, or retained evaluation
ran and that no adapter directory was accepted.

- [ ] **Step 3: Bind the manifest and final playlist**

Replace only:

```rust
const RETAINED_MANIFEST_SHA256: &str =
    "bcc58ba6faefe553a985e0aa91726fda3090ba51f63358fa5cb4c3a254b2db62";
const RETAINED_PLAYLIST_SHA256: &str =
    "a336d16633268688345bd7b742df7a5530ce99425ac3f66a906661a53cc03bdc";
```

Rebuild offline in release mode. Run static adapter preparation once with a
fresh `/var/tmp/attempt13-adapter-discovery` path and capture
`tdd-adapter-red.log`.

Expected: nonzero with exact error `derived local playlist digest mismatch`.
The command must create
`/var/tmp/attempt13-adapter-discovery/retained-local-vod.m3u8`. Hash that file
directly and record the lowercase SHA-256 as the derived adapter digest.

- [ ] **Step 4: Freeze the derived adapter digest and verify GREEN**

Replace only `RETAINED_ADAPTER_SHA256` with the digest derived in Step 3.
Rebuild offline in release mode. Use a fresh nonexistent
`/var/tmp/attempt13-adapter-green` path:

```bash
sudo timeout 120s \
  /var/tmp/attempt13-validator/target/release/attempt4-validator \
  --prepare-retained-adapter \
  /var/tmp/owncast-task5-attempt13-20260726T195605Z \
  /var/tmp/attempt13-adapter-green
```

Expected: exit zero and a status line containing the exact derived adapter
digest, `segment_count=31`, and `total_duration_ns=93000000000`. Capture the
complete result as `tdd-adapter-green.log`.

Run the same rebuilt candidate once against sealed Attempt 7 evidence with a
fresh adapter path. Expected: nonzero with
`retained manifest digest mismatch`. Capture `old-evidence-rejection.log`.

- [ ] **Step 5: Run complete non-media gates**

From `/var/tmp/attempt13-validator`, capture complete output and statuses in
`nonmedia-gates.log`:

```bash
sudo timeout 120s cargo fmt --check
sudo timeout 120s cargo test --offline
sudo timeout 120s cargo clippy --offline -- -D warnings
sudo timeout 120s cargo build --offline --release
sudo timeout 120s target/release/attempt4-validator --selfcheck
```

Require every command to exit zero with pristine output. Do not invoke
`--real-retained`, any diagnostic, or any decoder.

- [ ] **Step 6: Freeze the candidate**

Copy the approved design and this plan into the two candidate documentation
files. Generate `CHECKSUMS.sha256` over `Cargo.toml`, `Cargo.lock`,
`src/main.rs`, release binary, both documentation files, RED/GREEN logs,
old-evidence rejection, and the full gate log.

Require candidate directory mode `0700`, executable mode `0700`, and all
other frozen files mode `0600`; zero symlinks outside inherited Cargo target
internals; no credential-like content; no helper, validator, or RTMP process;
Owncast offline; and every checksum entry `OK`.

---

### Task 2: Independently review the frozen binding

**Files:**

- Read: `/var/tmp/attempt13-validator/`
- Compare: `/var/tmp/attempt7-validator-fix/`
- Create: `/var/tmp/attempt13-validator-review/`

**Interfaces:**

- Consumes: frozen Task 1 candidate and TDD/static evidence.
- Produces: checksum-sealed review package and explicit spec/quality verdicts.

- [ ] **Step 1: Build and verify the review package**

Include prior and candidate source, prior and candidate manifests, focused
source diff, design, plan, all RED/GREEN/static logs, non-media gate log, and
Task 1 report. Generate `PACKAGE-SHA256SUMS` from the package root and verify
every entry.

- [ ] **Step 2: Review exact scope and evidence**

Require:

- source diff changes only the three digest string literals;
- manifest RED proves the unchanged validator rejects Attempt 13;
- adapter RED proves the manifest and playlist pass before the old adapter
  digest rejects;
- adapter GREEN proves 31 segments and exactly 93 seconds;
- sealed Attempt 7 evidence is rejected by the rebound validator;
- candidate manifest, binary, documentation, and all logs verify;
- the 50 ms constants, boundary helper, parser, adapter serializer, decoders,
  content windows, continuity checks, source controls, language classifier,
  subtitle classifier, and `run_real` are byte-identical;
- preparation executed no retained decode, diagnostic, or real evaluation.

- [ ] **Step 3: Resolve findings before media execution**

For any Critical or Important finding, return the candidate to Task 1,
reproduce the defect with a focused static failing check, apply the smallest
correction, rerun all affected non-media gates, refresh both manifests, and
request scoped re-review. Do not run retained media until both spec and
quality verdicts pass.

---

### Task 3: Execute and seal one retained evaluation

**Files:**

- Execute: `/var/tmp/attempt13-validator/target/release/attempt4-validator`
- Read-only: `/var/tmp/owncast-task5-attempt13-20260726T195605Z/`
- Create: fresh `/var/tmp/Attempt13-evaluator-<UTC>/`
- Create: fresh `/var/tmp/Attempt13-evaluator-run-<UTC>/`
- Append: `.superpowers/sdd/2026-07-26-gstreamer-pipeline/task-5-report.md`
- Append: `.superpowers/sdd/2026-07-26-gstreamer-pipeline/progress.md`

**Interfaces:**

- Consumes: independently reviewed Task 2 binary and immutable Attempt 13 evidence.
- Produces: one immutable evaluation result for every retained content and timing gate.

- [ ] **Step 1: Run the final preflight once**

Verify candidate and review-package manifests; all four Attempt 13 structural
and provenance manifests; root-only/read-only modes; zero symlinks; exact
movie and SRT readability; Owncast API HTTP 200 with `online=false`;
Owncast container running; helper/validator count zero; established RTMP zero;
auth redirect HTTP 302; and clean repository.

Persist all commands, raw output, statuses, exact input hashes, and UTC time in
a root-only preflight log. Stop before media execution on any failure.

- [ ] **Step 2: Run exactly one persisted-status evaluation**

Choose fresh nonexistent output and run-log directories. Create the run-log
directory root-owned mode `0700`. Execute once under a root shell with
`umask 077`, redirect complete stdout and stderr to `evaluation.log`, and
persist command, UTC start/end, monotonic wall time, and exact exit status in
`execution.tsv` even when the validator fails:

```bash
timeout 900s \
  /var/tmp/attempt13-validator/target/release/attempt4-validator \
  --real-retained \
  /var/tmp/owncast-task5-attempt13-20260726T195605Z \
  /opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.mkv \
  /opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.en.srt \
  /var/tmp/Attempt13-evaluator-<UTC>
```

Do not use `tee`. Do not invoke this command a second time for any exit status
or result.

- [ ] **Step 3: Evaluate the complete result**

Require a conclusive result for all existing gates:

- `audio_boundary=transport_coverage`;
- movie A/V boundary delta at most 50 ms;
- every mapped continuity and adapter-boundary delta at most 50 ms;
- source calibration and controls pass;
- retained audio is English and rejects Italian;
- burned English subtitle content passes;
- subtitle/video delta is at most 50 ms;
- retained coverage reaches all three frozen source windows;
- the two Owncast DTS corrections do not produce any decoded discontinuity or
  timing failure.

Any nonzero exit, missing metric, inconclusive result, or failed gate is the
final result. Do not retry or tune.

- [ ] **Step 4: Seal and review the evaluation**

Hash every output and execution artifact in a sibling `manifest.sha256`;
record candidate, review-package, input evidence, movie, and SRT hashes; set
files to `0444`, directories to `0555`, and reject symlinks. Verify every
hash after sealing.

Recheck Owncast offline/HTTP 200, container running, helper/validator/RTMP
zero, auth redirect, repository cleanliness, candidate immutability, and all
Attempt 13 input manifests. Append the exact sanitized result and DTS
disposition to the existing Task 5 report and ledger. Submit the sealed
evaluation for an independent read-only task review.

- [ ] **Step 5: Verify the implementation plan is committed**

```bash
git status --short
git log --oneline -- docs/superpowers/plans/2026-07-26-attempt13-retained-validator.md
```

Require a clean repository and committed history for this plan. Do not create
an empty or duplicate commit.
