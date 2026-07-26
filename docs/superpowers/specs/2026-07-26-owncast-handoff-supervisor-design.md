# Owncast Prompt-Handoff Supervisor

## Goal

Capture a full 93-second retained movie interval by pressing Enter
immediately after the existing lobby proof, without depending on an agent tool
round trip.

## Design

Create one root-only Bash supervisor under `/var/tmp`. It wraps the existing
installed `owncast-stream` process and leaves the reviewed HLS collector
unchanged.

The supervisor:

1. creates a private FIFO and opens it read/write before launching the
   streamer, avoiding FIFO startup deadlock;
2. starts the exact Passenger stream with stdin connected to the FIFO and
   line-buffered output captured to a private log;
3. polls the loopback Owncast status API at 100 ms intervals;
4. waits for both `Lobby is live` in the process log and API status proving
   `online=true`, title `Starting soon: Passenger`, and a nonempty
   `lastConnectTime`;
5. calculates a conservative upper-bound delay from `lastConnectTime` to the
   local proof timestamp and refuses the handoff if it exceeds five seconds;
6. writes exactly one newline to the FIFO, records the timestamp, and closes
   the write path against further Enter presses;
7. waits for `Movie is live`, title `Passenger`, unchanged
   `lastConnectTime`, and exactly one inbound connection;
8. polls the expected fresh evidence path for the collector's root-owned,
   regular `capture-status.txt` success marker;
9. forwards one SIGINT to the streamer and verifies clean exit.

The evidence path is an explicit supervisor argument and must not exist before
the collector starts. Any failed precondition, timeout, unsafe success marker,
title mismatch, reconnect, or duplicate handoff stops the streamer safely and
produces no successful capture claim.
The supervisor contains and prints no stream key, title token, password,
cookie, or Authorization value.

## Scope

- The installed streamer, production repository, validator, Owncast,
  collector, routes, and credentials are unchanged.
- The script is an Attempt 10 operational artifact, not product code.
- The retained collector still requires the exact live segment set `0..30`.
- No media validator runs until the new evidence is sealed and reviewed.

## Verification

A non-network self-test uses fake status responses and a fake child process to
prove:

- the FIFO launches without blocking;
- a valid lobby proof causes exactly one newline;
- a proof over five seconds causes no newline;
- a title or connection change fails;
- cleanup sends one SIGINT and removes the FIFO.

Before live execution, an independent review verifies the script, self-test
output, hashes, root-only modes, absence of credentials, and lack of changes
to the reviewed collector and production binary.

The live attempt passes only when Enter occurs within five seconds of the
recorded connection, the detected movie boundary is before 15.5 seconds in
the retained timeline, segment evidence `0..30` is complete, and all existing
single-connection, title, auth, health, cleanup, and integrity gates pass.
