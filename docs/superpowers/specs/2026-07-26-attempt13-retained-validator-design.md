# Attempt 13 Retained Validator Binding

## Goal

Bind the independently reviewed retained-media validator to the sealed
Attempt 13 capture, then run exactly one complete retained-content evaluation.

## Chosen approach

Clone `/var/tmp/attempt7-validator-fix` into a new root-only candidate and
change only its three frozen evidence-binding digests and documentation:
the retained root manifest, final live playlist, and deterministic local
adapter. Also rename the static preparation status fields from `segments` to
`segment_count` and from `duration_ns` to `total_duration_ns`. Keep the
evidence path as the existing CLI argument.

This is narrower than adding a configurable digest and safer than weakening
the manifest check. The validator must continue rejecting every evidence root
whose manifest digest is not:

```text
bcc58ba6faefe553a985e0aa91726fda3090ba51f63358fa5cb4c3a254b2db62
```

Its final-live playlist digest must be:

```text
a336d16633268688345bd7b742df7a5530ce99425ac3f66a906661a53cc03bdc
```

The sealed input is:

```text
/var/tmp/owncast-task5-attempt13-20260726T195605Z
```

## Preparation

Before changing the candidate, verify the Attempt 13 evidence, run-log,
preflight, and provenance-supplement manifests. Copy the reviewed validator
without modifying the original candidate or its review package.

Use static adapter preparation as the behavioral TDD gate:

1. the copied validator must reject Attempt 13 because it is still bound to
   the Attempt 7 manifest;
2. after replacing the manifest and playlist digests, the same command must
   reach the old local-adapter digest check and reject it;
3. hash that generated adapter, freeze its digest, rebuild, and require a
   fresh static adapter preparation to pass;
4. require the real static CLI output to fail an assertion for the two new
   status-field names, rename only those fields, and require the same assertion
   to pass;
5. no decoder, retained evaluation, timeline diagnostic, source classifier,
   or subtitle classifier may run during preparation.

Run the offline test suite while skipping exactly
`local_hls_with_large_transport_pts_decodes_at_zero_running_time` and
`local_lobby_movie_switch_fixture_calibrates_boundaries`, the two tests that
invoke media decoders. Require the remaining 44 tests, formatting, Clippy,
release build, and selfcheck to pass. Freeze source, binary, documentation,
RED/GREEN logs, and checksums in a new root-only bundle.

## Independent review

Build a checksum-sealed review package containing the prior and candidate
source, focused diff, both manifests, preparation logs, and this design and
its implementation plan.

The review must prove:

- the only executable-code changes are the three evidence-binding digest
  constants and two static status-field labels;
- the Attempt 13 manifest is accepted and the Attempt 7 manifest is rejected;
- the 50 ms thresholds, transport-boundary fix, evidence parser, adapter,
  decoders, continuity checks, source windows, language classifier, and
  subtitle classifier are unchanged;
- the two decoder-backed fixture tests were explicitly skipped and the other
  44 offline tests passed;
- no retained decode or evaluation ran during preparation.

No real evaluation may run until the scoped review passes.

## One-shot evaluation

After review, verify safe offline host state and all immutable input hashes.
Run the reviewed release binary once with `--real-retained`, the sealed
Attempt 13 evidence, the unchanged Passenger movie and English SRT, and a
fresh output directory.

Do not retry or tune code, thresholds, evidence, or classifiers after seeing
the result. Capture exit status, complete stdout and stderr, UTC start/end,
and wall time. Seal the evaluation and execution log with a sibling SHA-256
manifest, files mode `0444`, directories mode `0555`, and no symlinks.

The result passes only if every existing content, language, burned-subtitle,
coverage, continuity, source-control, and 50 ms A/V timing gate reports a
conclusive pass. The two Owncast DTS corrections from the handoff remain an
explicit review item and are resolved only by these decoded timing results.
