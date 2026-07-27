# Paused Frame Review Fix

## Goal

Address the three unresolved PR review threads without changing the public
controls or contacting Owncast:

- pausing immediately after start must not fail before a movie frame exists;
- a paused seek must freeze the frame produced by that seek;
- a paused seek clamped to the media end must succeed and retain the last frame.

## Design

Capture the latest normalized movie frame at the playback pipeline output,
before `intervideosink`, instead of at the repeating broadcast
`intervideosrc`. Store the buffer with a monotonically increasing generation.

Starting playback waits for the first captured generation before selecting
movie output and exposing `Playing`. Pausing freezes the current playback-side
buffer. A paused seek below the duration remembers the current generation,
performs the seek, and waits for a newer generation before freezing it. A seek
to exactly the duration keeps the already frozen last valid frame and returns
success because EOS may not produce another frame.

All waits remain bounded and propagate pipeline errors. The existing broadcast
pipeline, input selectors, freeze bin, and keyboard behavior remain otherwise
unchanged.

## Tests

Synthetic GStreamer pipelines and fakesinks will verify:

- start does not report `Playing` before the first captured frame;
- immediate pause succeeds after start;
- paused seek waits for a playback-side generation change;
- paused seek to the exact duration succeeds without requiring a new frame.

No streaming binary, host Owncast endpoint, or `/opt/owncast` secret is used.
