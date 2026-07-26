# Rust Port Final Validation

## Goal

Finish the Rust Owncast port by repairing the retained-media validator,
revoking the exposed Authelia sessions, and obtaining conclusive retained
audio, subtitle, continuity, and timing results.

## Scope

The production Rust pipeline and installed binary already match and remain
unchanged unless validation identifies a production defect. The known
`WasLinked` failure is in the external retained-media validator's
subtitle-enabled video decoder.

Keep the two previously omitted decoder-backed fixture tests omitted. Add no
new dependency, classifier, threshold, retry loop, or production feature.

## Session revocation

The Authelia deployment uses its dedicated `owncast-redis` container for
session storage. Before new validation, authenticate to that Redis instance
through its existing secret file and flush only the selected session
database. Do not alter Authelia users, groups, configuration, persistent
storage, or unrelated containers.

Verify Redis is healthy afterward and that protected routes require a fresh
login. Keep the sealed logs containing the old cookies root-only.

## Validator repair

Replace the validator's ambiguous automatic sink ghosting with explicit pad
ownership:

1. parse the video branch without automatic ghost pads;
2. name the input queue and add one explicit bin ghost pad targeting its sink;
3. continue linking the subtitle parser directly to
   `subtitleoverlay.subtitle_sink`.

This is the smallest deterministic repair and mirrors the explicit video-pad
pattern already used by the production Rust pipeline. It avoids depending on
which of two unlinked sink pads GStreamer chooses to ghost.

## Test-first gate

Before changing the validator, add one non-decoder wiring test that constructs
the subtitle-enabled branch and proves the subtitle sink is free while the
explicit video ghost pad exists. Require it to fail against the current
automatic-ghost implementation with `WasLinked`, then pass after the minimal
repair.

Run formatting, Clippy, release build, selfcheck, and all existing non-media
tests while continuing to skip exactly:

- `local_hls_with_large_transport_pts_decodes_at_zero_running_time`
- `local_lobby_movie_switch_fixture_calibrates_boundaries`

Freeze the repaired source, binary, test evidence, and checksums in a fresh
root-only candidate. Independently compare the change against the prior frozen
validator before any retained evaluation.

## Retained evaluation

The user's authorization starts a new evaluation cycle. Run the newly reviewed
validator against the immutable Attempt 13 capture, unchanged Passenger movie,
and unchanged English SRT.

Persist complete output and exit status. A result passes only when every
existing gate is conclusive:

- lobby-to-movie video and audio boundaries differ by at most 50 ms;
- decoded audio continuity and every adapter boundary stay within 50 ms;
- source calibration and controls pass;
- retained audio identifies English and rejects Italian;
- burned English subtitle content passes;
- subtitle/video timing differs by at most 50 ms;
- retained coverage reaches every frozen source window.

If the evaluator exposes another validator defect, preserve the failed
artifacts and return to a new test-first repair cycle. Do not weaken media
requirements or change production to satisfy a validator bug.

## Completion

After a conclusive retained pass:

1. run the complete production Rust verification suite;
2. verify the installed binary equals the worktree release binary;
3. verify Owncast is offline, healthy, and has no stale publisher;
4. commit the repair records and publish the feature branch.

Production installation is required only if the validated production binary
changes.
