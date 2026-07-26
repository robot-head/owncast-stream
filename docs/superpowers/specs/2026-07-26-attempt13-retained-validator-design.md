# Attempt 13 Retained Validator Binding

## Goal

Bind the independently reviewed retained-media validator to the sealed
Attempt 13 capture, then run exactly one complete retained-content evaluation.

## Chosen approach

Clone `/var/tmp/attempt7-validator-fix` into a new root-only candidate and
change only its frozen retained-manifest digest and evidence-binding
documentation. Keep the evidence path as the existing CLI argument.

This is narrower than adding a configurable digest and safer than weakening
the manifest check. The validator must continue rejecting every evidence root
whose manifest digest is not:

```text
bcc58ba6faefe553a985e0aa91726fda3090ba51f63358fa5cb4c3a254b2db62
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
2. after replacing only the frozen digest and binding documentation, the same
   command must accept Attempt 13 and produce the deterministic local adapter;
3. no decoder, retained evaluation, timeline diagnostic, source classifier,
   or subtitle classifier may run during preparation.

Run the full offline non-media test, formatting, Clippy, release-build, and
selfcheck gates. Freeze source, binary, documentation, RED/GREEN logs, and
checksums in a new root-only bundle.

## Independent review

Build a checksum-sealed review package containing the prior and candidate
source, focused diff, both manifests, preparation logs, and this design and
its implementation plan.

The review must prove:

- the only executable-code change is the retained-manifest digest;
- the Attempt 13 manifest is accepted and the Attempt 7 manifest is rejected;
- the 50 ms thresholds, transport-boundary fix, evidence parser, adapter,
  decoders, continuity checks, source windows, language classifier, and
  subtitle classifier are unchanged;
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

