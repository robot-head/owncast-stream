# Bitmap Subtitle Fallback Design

## Problem

`owncast-stream` selects every English embedded subtitle stream. This host has
no GStreamer PGS decoder, so selecting a Blu-ray PGS stream prevents the movie
pipeline from reaching handoff and leaves the lobby live.

## Design

The GStreamer stream-candidate boundary will ignore subtitle caps in the
`subpicture/*` family. Existing stream selection then uses an external subtitle
when supplied or streams without burned subtitles.

No decoder installation, retry, subtitle conversion, OCR, or new dependency is
needed.

## Error Handling

Unsupported embedded tracks are treated as unavailable rather than fatal. All
existing process and handoff errors remain unchanged.

## Testing

A unit regression test will supply a GStreamer text stream with
`subpicture/x-pgs` caps and verify the candidate is rejected. Existing unit and
integration tests will verify stream selection and continuous handoff.
