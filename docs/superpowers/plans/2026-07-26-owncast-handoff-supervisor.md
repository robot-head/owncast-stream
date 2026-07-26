# Owncast Prompt-Handoff Supervisor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the delayed manual Enter step with a reviewed local supervisor that proves the lobby and sends exactly one newline within five seconds of the Owncast connection.

**Architecture:** A root-only Bash script launches the existing streamer through a private FIFO, polls the loopback status API, and sends one newline when the lobby proof is complete. The unchanged hardened collector runs beside it; a root-owned success marker tells the supervisor when to stop the streamer.

**Tech Stack:** Bash, FIFO, curl, jq, date, Docker CLI, existing Rust streamer, existing Python HLS collector.

## Global Constraints

- Do not modify the production repository source, installed streamer, collector, validator, Owncast, routes, or credentials.
- Do not print or persist a stream key, title token, password, cookie, or Authorization value.
- Send Enter exactly once and only after `Lobby is live`, `online=true`, title `Starting soon: Passenger`, and a nonempty `lastConnectTime`.
- Refuse Enter when the conservative proof delay exceeds 5,000 ms.
- Preserve one RTMP connection and unchanged `lastConnectTime`.
- Require the exact retained live segment set `0..30`.
- Do not run a media validator during this plan.
- Stop safely without retry on any live failure.

---

### Task 1: Build and self-test the root-only supervisor

**Files:**

- Create: `/var/tmp/attempt10-supervisor/supervise.sh`
- Create: `/var/tmp/attempt10-supervisor/README.md`
- Create: `/var/tmp/attempt10-supervisor/CHECKSUMS.sha256`

**Interfaces:**

- Consumes: `supervise.sh EVIDENCE_PATH RUN_LOG_DIR`.
- Reads: loopback `http://127.0.0.1:8081/api/status`, child process output, and `EVIDENCE_PATH/capture-status.txt`.
- Produces: one streamer process, one Enter, `streamer.log`, `supervisor.tsv`, and a clean SIGINT shutdown.

- [ ] **Step 1: Write failing self-tests first**

Implement `--selftest` before the production path exists. It must assert:

```bash
test "$(delay_ms 2026-07-26T15:00:00Z 1753542004500)" -eq 4500
handoff_allowed true "Starting soon: Passenger" value 4500
! handoff_allowed true "Starting soon: Passenger" value 5001
! handoff_allowed true Passenger value 100
```

It must also create a private FIFO, start a one-byte reader, write one newline,
close the writer, and assert the reader captured exactly one byte with hex
value `0a`. A fake child with an `INT` trap must record exactly one forwarded
SIGINT during cleanup.

- [ ] **Step 2: Run self-test and verify RED**

```bash
sudo timeout 30s /var/tmp/attempt10-supervisor/supervise.sh --selftest
```

Expected: nonzero because the production helper functions do not exist.
Capture complete output and exit status in `selftest-red.log`.

- [ ] **Step 3: Implement the minimal supervisor**

Use `set -euo pipefail`, `umask 077`, require root, require an absolute
root-owned mode-0700 evidence directory with no existing
`capture-status.txt`, and require an existing root-owned mode-0700 run-log
directory, then:

```bash
run_dir=$(mktemp -d /var/tmp/attempt10-supervisor-run.XXXXXX)
chmod 700 "$run_dir"
fifo="$run_dir/stdin.fifo"
mkfifo -m 600 "$fifo"
exec 3<> "$fifo"

stdbuf -oL -eL /usr/local/bin/owncast-stream \
  "/opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.mkv" \
  "/opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.en.srt" \
  "Passenger" <"$fifo" >"$log_dir/streamer.log" 2>&1 &
streamer_pid=$!
```

`delay_ms` parses `lastConnectTime` with `date -u -d` and subtracts it from
the millisecond proof timestamp using base-10 integer arithmetic.

`handoff_allowed` returns success only for:

```text
online=true
title=Starting soon: Passenger
lastConnectTime=nonempty
delay_ms<=5000
```

Poll every 100 ms for at most 30 seconds. Require the literal
`Lobby is live. Press Enter to start "Passenger"...` in `streamer.log` plus
the API proof. Record proof fields in `supervisor.tsv`, write one newline to
FD 3, close FD 3, set `enter_sent=1`, and make any second send an error.

Poll for at most 30 seconds for:

```text
Movie is live.
online=true
streamTitle=Passenger
lastConnectTime=<original value>
docker inbound count since validation start=1
```

Then poll for at most 190 seconds for a non-symlink, root-owned regular
`EVIDENCE_PATH/capture-status.txt` containing `capture=PASS`. Forward one
SIGINT to the child, wait for it, and require zero exit.

The EXIT/INT/TERM trap sends SIGINT only if the child is still alive, waits
for it, closes FD 3 if open, and removes only the exact private `run_dir`.

- [ ] **Step 4: Run self-test and verify GREEN**

```bash
sudo timeout 30s /var/tmp/attempt10-supervisor/supervise.sh --selftest
sudo bash -n /var/tmp/attempt10-supervisor/supervise.sh
```

Expected: `selftest=PASS fifo_newlines=1 delayed_handoff=BLOCKED cleanup_sigint=1`
and Bash syntax success. Capture complete output as `selftest-green.log`.

- [ ] **Step 5: Freeze the bundle**

Write a concise README with exact self-test and live invocation. Generate
`CHECKSUMS.sha256` for the script, README, and RED/GREEN logs; verify it.
Set the bundle directory to `0700` and regular files to `0600`, keeping the
script executable `0700`. Record `stat`, `sha256sum`, `bash -n`, self-test,
credential-pattern scan, and absence of live processes in the task report.

Do not start Owncast, the collector, or the streamer.

---

### Task 2: Independently review the supervisor

**Files:**

- Read: `/var/tmp/attempt10-supervisor/`
- Create: `/var/tmp/attempt10-supervisor-review/`

**Interfaces:**

- Consumes: Task 1 frozen script, self-test evidence, and checksums.
- Produces: a verified root-only package and clean/changes-required verdict.

- [ ] **Step 1: Build the review package**

Include the script, README, checksum manifest, RED/GREEN logs, design, plan,
and implementation report. Generate and verify `PACKAGE-SHA256SUMS`.

- [ ] **Step 2: Review behavior and safety**

Verify:

- FIFO open order cannot deadlock;
- only one newline path exists and it is guarded;
- the five-second comparison is inclusive and base-10 safe;
- exact lobby/movie titles and unchanged connection time are required;
- inbound count must equal one;
- marker path rejects symlinks, nonregular files, non-root ownership, and
  non-PASS content;
- traps signal only the exact child PID and remove only the `mktemp` directory;
- no credential is read, printed, or embedded;
- self-tests prove newline, delay rejection, title rejection, and cleanup;
- no live process or media command ran.

- [ ] **Step 3: Resolve findings before live execution**

For each finding, add one failing self-test, capture RED, make the smallest
script correction, capture GREEN, rerun syntax/self-test/scans, refresh
checksums, and request scoped re-review. No live command runs until review is
clean.

---

### Task 3: Capture Attempt 10 with prompt handoff

**Files:**

- Execute unchanged: `/var/tmp/attempt6-collector/collector.py`
- Execute reviewed: `/var/tmp/attempt10-supervisor/supervise.sh`
- Create: `/var/tmp/owncast-task5-attempt10-<UTC>/`
- Create: root-only run-log directory under `/var/tmp`
- Append ignored reports under `.superpowers/sdd/`

**Interfaces:**

- Consumes: reviewed supervisor and collector.
- Produces: sealed exact HLS evidence with movie boundary expected before
  15.5 seconds.

- [ ] **Step 1: Run one-shot preflight**

Require:

```bash
cargo test --locked
cargo test --locked pipeline::tests::synthetic_handoff_stays_within_50ms \
  -- --ignored --nocapture
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
cmp --silent target/release/owncast-stream /usr/local/bin/owncast-stream
sudo sha256sum -c /var/tmp/attempt6-collector/SHA256SUMS
sudo sha256sum -c /var/tmp/attempt10-supervisor/CHECKSUMS.sha256
```

Also require Owncast offline, helper count zero, established RTMP count zero,
anonymous admin redirect to `auth.djspacecat.com`, and clean Git status.
Stop without live work on failure.

- [ ] **Step 2: Start collector and marker writer**

Choose fresh absolute evidence and run-log paths. Start the unchanged collector
under its 190-second outer bound. In the same root-owned background coordinator,
wait for collector exit zero, then atomically create a root-owned regular
`capture-status.txt` containing:

```text
capture=PASS
```

On collector failure, do not create the marker.
Before launching the supervisor, wait only until the collector has created the
evidence directory and verify it is root-owned mode `0700` with no marker.

- [ ] **Step 3: Start the reviewed supervisor once**

Invoke:

```bash
sudo timeout 230s /var/tmp/attempt10-supervisor/supervise.sh \
  "$evidence_path" "$run_log_dir"
```

Do not manually write to the FIFO or send Enter. Stop without retry on any
supervisor or collector failure.

- [ ] **Step 4: Verify operational and retained evidence**

Require:

- proof-to-Enter upper bound at most 5,000 ms;
- exactly one Enter and one inbound connection;
- lobby/movie titles correct and `lastConnectTime` unchanged;
- collector prefix unique, exact segments `0..30`, 31 files, complete
  playlist snapshots/revision commits, no offline retained entry;
- retained durations total 93 seconds;
- segment 0 PDT to recorded Enter comfortably below 15.5 seconds;
- playlist metadata, hashes, sizes, history, revisions, and associations match;
- auth and loopback health pass;
- final online false, helper zero, collector zero, established RTMP zero.

- [ ] **Step 5: Seal and record**

Create the evidence manifest, verify every entry, set files to `0444` and
directories to `0555`, and reject symlinks. Seal the supervisor/collector logs
separately. Recheck binary comparison, package manifests, repository tests,
Clippy, Git cleanliness, and offline host state. Append exact Attempt 10
commands and sanitized outputs to the existing Task 5 report and both ledgers.
Do not run a media validator.

- [ ] **Step 6: Commit only this plan**

```bash
git add docs/superpowers/plans/2026-07-26-owncast-handoff-supervisor.md
git commit -m "docs: plan prompt-handoff supervisor"
```
