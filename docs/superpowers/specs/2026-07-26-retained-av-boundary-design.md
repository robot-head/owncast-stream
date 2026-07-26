# Retained A/V Boundary Validation

## Problem

The retained-HLS validator treats the first three non-silent 10 ms audio
windows as the movie-audio handoff. Passenger is silent for 3.253 seconds
after the first movie frame, so that content-onset heuristic reports an A/V
failure even though the retained transport timeline is continuous.

The diagnostic evidence shows:

- movie video departure: `21,966,666,666 ns`;
- first sustained audible audio: `25,220,000,000 ns`;
- no mapped audio gap or overlap over 50 ms;
- every HLS segment-boundary delta is under 0.334 ms.

## Design

Keep the existing video boundary detector and 50 ms limit. Replace only the
audio-side handoff measurement with transport coverage:

1. Find the mapped retained-audio buffer interval containing the detected
   video boundary.
2. If no interval contains it, use the nearest mapped interval edge.
3. Fail unless that interval or edge covers the video boundary within 50 ms.
4. Continue requiring all mapped audio continuity and HLS segment-boundary
   deltas to remain within 50 ms.

The reported movie-audio boundary is the video-boundary timestamp when an
audio interval contains it, otherwise the nearest interval edge. The audible
onset remains diagnostic information only; silence is valid media content and
cannot identify a selector handoff.

Language correlation still proves that retained movie audio matches the
English source rather than the Italian source. Subtitle detection and
subtitle/video timing remain unchanged and continue to use the detected movie
video boundary.

## Alternatives Rejected

- Add an audible marker and recapture the stream: changes production output
  solely for validation.
- Infer the switch from low-level silence correlation: content-specific and
  unreliable when both sides are silent.
- Increase the 50 ms threshold: hides the validator defect and weakens the
  actual timing requirement.

## Failure Handling

Validation fails when audio timestamps are missing, no mapped audio interval
is close enough to the video boundary, any mapped continuity delta exceeds
50 ms, or existing language/subtitle/content gates fail. No threshold is
changed and no failure is retried automatically.

## Tests and Verification

Test-first coverage must prove:

- continuous audio containing the video boundary passes despite several
  seconds of following silence;
- the nearest interval edge is reported when the boundary falls between
  buffers;
- a gap over 50 ms fails;
- missing or empty timing data fails;
- existing language, subtitle, source-control, evidence-integrity, and
  adapter tests remain unchanged and pass.

After review, run the frozen validator once against the sealed Attempt 7
evidence. It must report transport A/V delta at most 50 ms, bounded continuity,
English-not-Italian audio, burned English subtitles, and subtitle/video delta
at most 50 ms.
