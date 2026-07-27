# Bitmap Subtitle Fallback Design

## Problem

`owncast-stream` sends every embedded subtitle stream to FFmpeg's text-only
`subtitles` filter. Blu-ray PGS tracks therefore stop the movie encoder before
handoff and leave the lobby live.

## Design

Subtitle selection will ignore embedded bitmap codecs. It will retain the
existing preference order among supported embedded text tracks: English first,
then the first supported track. If none remain, the existing external subtitle
argument is used; without one, the movie streams without burned subtitles.

The codec check belongs in `select_subtitle`, the shared selection boundary.
No FFmpeg retry, subtitle conversion, OCR, or new dependency is needed.

## Error Handling

Unsupported embedded tracks are treated as unavailable rather than fatal. All
existing process and handoff errors remain unchanged.

## Testing

A unit regression test will supply an English `hdmv_pgs_subtitle` stream before
a supported text stream and verify the bitmap track is skipped. Existing unit
and integration tests will verify preference order and continuous handoff.
