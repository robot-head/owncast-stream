# Python Owncast Prompt-Handoff Supervisor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Send one prompt handoff within five seconds of the Owncast lobby connection using a deterministic Python child-process supervisor.

**Architecture:** A root-only Python standard-library script launches the unchanged streamer with `stdin=PIPE`, polls loopback status, writes one newline, and closes stdin. It waits for the unchanged collector's success marker, then sends one SIGINT to the exact child PID and reaps it.

**Tech Stack:** Python 3 standard library, `subprocess`, `urllib.request`, `datetime`, `unittest`, existing Rust streamer and Python HLS collector.

## Global Constraints

- Do not modify the production repository source, installed streamer, collector, validator, Owncast, routes, or credentials.
- Do not print or persist a stream key, title token, password, cookie, or Authorization value.
- Send Enter exactly once and only while the child is alive and status proves
  `online=true`, title `Starting soon: Passenger`, and nonempty
  `lastConnectTime`.
- Refuse Enter when the conservative proof delay exceeds 5,000 ms.
- Require one inbound connection, title `Passenger`, and unchanged `lastConnectTime` after handoff.
- Require exact retained live segments `0..30`.
- Do not run a media validator during this plan.
- Stop safely without retry on any live failure.

---

### Task 1: Build and self-test the Python supervisor

**Files:**

- Create: `/var/tmp/attempt10-python-supervisor/supervise.py`
- Create: `/var/tmp/attempt10-python-supervisor/test_supervise.py`
- Create: `/var/tmp/attempt10-python-supervisor/README.md`
- Create: `/var/tmp/attempt10-python-supervisor/CHECKSUMS.sha256`

**Interfaces:**

- CLI: `supervise.py EVIDENCE_PATH RUN_LOG_DIR`
- Pure helper: `proof_delay_ms(last_connect: str, proof: datetime) -> int`
- Pure helper: `handoff_allowed(status: dict, proof: datetime) -> tuple[str, int]`
- Side effect: `send_enter(process: subprocess.Popen, already_sent: bool) -> bool`
- Cleanup: `stop_child(process: subprocess.Popen) -> None`
- Marker: `valid_capture_marker(evidence: Path) -> bool`

- [ ] **Step 1: Write tests before production functions**

`test_supervise.py` must assert:

```python
def test_proof_delay_and_lobby_gate():
    proof = datetime.datetime.fromisoformat("2026-07-26T15:00:04.500+00:00")
    assert supervise.proof_delay_ms("2026-07-26T15:00:00Z", proof) == 4500
    status = {
        "online": True,
        "streamTitle": "Starting soon: Passenger",
        "lastConnectTime": "2026-07-26T15:00:00Z",
    }
    assert supervise.handoff_allowed(status, proof) == (
        "2026-07-26T15:00:00Z",
        4500,
    )
```

Also require:

- exactly 5,000 ms passes and 5,001 ms fails;
- wrong title, offline, or empty connection time fails;
- `send_enter` writes exactly `"\n"`, flushes, closes stdin, and rejects a
  second call;
- `valid_capture_marker` accepts only a non-symlink, root-owned regular file
  with mode `0600` and exact bytes `capture=PASS\n`;
- `stop_child` waits for a harmless child to print `READY`, sends one SIGINT,
  and reaps exit zero without an orphan.

- [ ] **Step 2: Run tests and verify RED**

```bash
cd /var/tmp/attempt10-python-supervisor
sudo timeout 30s python3 -m unittest -v test_supervise.py
```

Expected: import or missing-function failures. Capture complete output and exit
status in `tdd-red.log`.

- [ ] **Step 3: Implement the minimal helpers**

Use only the standard library. Parse `Z` as `+00:00`, require timezone-aware
connection times, calculate `delta = proof - connection`, and return:

```python
delta.days * 86_400_000 + delta.seconds * 1_000 + delta.microseconds // 1_000
```

`handoff_allowed` rejects negative delay, delay over `5000`, offline state,
wrong title, or missing connection.

`send_enter` requires `process.stdin`, writes one newline, flushes, closes it,
and returns `True`; an already-sent call raises `RuntimeError`.

`stop_child` does nothing when already exited. Otherwise it sends
`signal.SIGINT`, waits up to 20 seconds, and raises if the child does not exit
zero. Test it with a real Python child that prints `READY` only after its
SIGINT handler is installed.

`valid_capture_marker` opens once with `O_NOFOLLOW|O_NONBLOCK`, validates the
same descriptor with `fstat`, requires a regular UID-0 mode-0600 file, and
performs one bounded exact marker read.

- [ ] **Step 4: Implement the live orchestration**

Require effective UID zero, absolute evidence/run-log paths, existing
root-owned mode-0700 directories, and no existing marker. Open
`streamer.log` mode `0600`, then:

```python
process = subprocess.Popen(
    [
        "/usr/local/bin/owncast-stream",
        "/opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.mkv",
        "/opt/owncast/uploads/Passenger.2026.1080p.ITA-ENG.MULTI.WEBRip.x265.AAC-V3SP4EV3R.en.srt",
        "Passenger",
    ],
    stdin=subprocess.PIPE,
    stdout=stream_log,
    stderr=subprocess.STDOUT,
    text=True,
)
```

Poll every 100 ms for at most 30 seconds while the child remains alive until
the fixed no-proxy/no-redirect opener for
`http://127.0.0.1:8081/api/status` returns the required lobby JSON. Record UTC
proof time, connection time, delay, and `enter_count=1` to mode-0600
`supervisor.tsv`, then call `send_enter`.

Poll at most 30 seconds while the child remains alive for online title
`Passenger`, unchanged connection time, and exactly one
`Inbound stream connected` line from:

```python
subprocess.run(
    ["docker", "logs", "--since", validation_since, "owncast"],
    check=True,
    capture_output=True,
    text=True,
    timeout=10,
)
```

Keep `streamer.log` only as diagnostic output. Do not gate either phase on
Rust stdout lines because redirected Rust stdout may remain buffered until
shutdown.

Poll at most 190 seconds for `valid_capture_marker(evidence)`, then call
`stop_child`. In `finally`, if the exact child still runs, send it one SIGINT,
wait 20 seconds, then kill only that child as last-resort cleanup.

- [ ] **Step 5: Run tests and verify GREEN**

```bash
sudo timeout 30s python3 -m unittest -v test_supervise.py
sudo python3 -m py_compile supervise.py test_supervise.py
```

Expected: all tests pass and bytecode compilation succeeds. Capture complete
output in `tdd-green.log`.

- [ ] **Step 6: Freeze the root-only bundle**

Write the exact self-test and live commands in README. Generate and verify
`CHECKSUMS.sha256` for source, tests, README, and RED/GREEN logs. Require
directory `0700`, script `0700`, and other files `0600`. Record full tests,
py_compile, checksum verification, `stat`, credential-pattern scan, process
count zero, and offline host state in the task report. Do not invoke the live
CLI, network, streamer, collector, or validator.

---

### Task 2: Independently review the Python supervisor

**Files:**

- Read: `/var/tmp/attempt10-python-supervisor/`
- Create: `/var/tmp/attempt10-python-supervisor-review/`

**Interfaces:**

- Consumes: Task 1 frozen bundle and TDD evidence.
- Produces: verified review package and clean/changes-required verdict.

- [ ] **Step 1: Package the frozen candidate**

Include source, tests, README, checksums, RED/GREEN logs, design, plan, and
Task 1 report. Generate and verify `PACKAGE-SHA256SUMS`.

- [ ] **Step 2: Review correctness and safety**

Verify:

- direct stdin pipe writes and closes exactly once;
- proof delay accepts exactly 5,000 ms and rejects 5,001 ms;
- lobby/movie titles and connection identity are exact;
- status access is loopback only;
- inbound count must equal one;
- marker validation rejects unsafe file types, ownership, modes, and content;
- SIGINT targets and reaps only the exact child;
- timeouts cannot leave an orphan;
- no credentials/dependencies/live execution are present;
- tests use a ready handshake and cannot race signal installation.

- [ ] **Step 3: Resolve findings test-first**

Add one failing test per finding, capture RED, make the smallest correction,
capture GREEN, rerun all non-live gates, refresh hashes, and request scoped
re-review. No live command runs until review is clean.

---

### Task 3: Capture Attempt 10

**Files:**

- Execute unchanged: `/var/tmp/attempt6-collector/collector.py`
- Execute reviewed: `/var/tmp/attempt10-python-supervisor/supervise.py`
- Create: `/var/tmp/owncast-task5-attempt10-<UTC>/`
- Create: root-only run-log directory
- Append: existing ignored Task 5 reports and ledgers

**Interfaces:**

- Consumes: reviewed supervisor and collector.
- Produces: sealed exact HLS evidence with handoff before retained timeline
  `15.5 s`.

- [ ] **Step 1: Run one-shot preflight**

Require repository tests, ignored synthetic handoff test with the exact unit
selector, Clippy, locked release build, release/install comparison, collector
and supervisor manifests, Owncast offline, helper/collector/RTMP zero, auth
redirect, and clean Git status. Stop before live work on failure.

- [ ] **Step 2: Start unchanged collector and marker coordinator**

Choose fresh nonexisting evidence and run-log paths. Start the collector under
its 190-second bound. Wait until it creates the root-owned mode-0700 evidence
directory. In the coordinator, create `capture-status.txt` atomically with
mode `0600` and exact `capture=PASS\n` only after collector exit zero.

- [ ] **Step 3: Run the supervisor once**

```bash
sudo python3 \
  /var/tmp/attempt10-python-supervisor/supervise.py \
  "$evidence_path" "$run_log_dir"
```

Rely on the supervisor's reviewed internal phase deadlines and bounded cleanup;
an outer timeout must not preempt its `finally` block. Do not manually send
Enter. Do not retry any live failure.

- [ ] **Step 4: Verify, seal, and record**

Require proof delay at most 5,000 ms, `enter_count=1`, one inbound connection,
correct titles, unchanged connection time, exact segments `0..30`, 31 files,
93 seconds, complete revision/history/hash/size associations, no retained
offline entry, auth/health pass, and final offline/helper/collector/RTMP zero.

Compute segment-0 PDT to Enter upper bound and require it below 15.5 seconds.
Create and verify the full evidence manifest, seal files `0444` and directories
`0555`, reject symlinks, seal logs, rerun integrity/repository/host checks, and
append sanitized Attempt 10 evidence to the reports and ledgers. Do not run a
media validator.

- [ ] **Step 5: Commit only this replacement plan**

```bash
git add docs/superpowers/plans/2026-07-26-owncast-handoff-supervisor-python.md
git commit -m "docs: plan Python handoff supervisor"
```
