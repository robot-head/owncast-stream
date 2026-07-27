# Rust-Controlled GStreamer Pipeline Design

## Goal

Replace the three external FFmpeg processes in `owncast-stream` with one
in-process, Rust-controlled GStreamer pipeline. Preserve the continuous
lobby-to-movie handoff, Owncast title updates, subtitle burn-in, and RTMP
publishing while fixing implicit media-track selection and timestamp handoff
fragility.

Native media libraries are allowed. The application must not spawn `ffmpeg`,
`ffprobe`, `gst-launch`, or another media subprocess.

## Chosen Approach

Use the official GStreamer Rust bindings. GStreamer supplies mature demuxers,
decoders, converters, H.264 and AAC encoders, subtitle rendering, FLV muxing,
RTMP output, clocks, and dynamic-pipeline support. Rust owns pipeline
construction, stream selection, state changes, title API calls, validation,
and error reporting.

Rejected alternatives:

- Embedded FFmpeg libraries retain FFmpeg filter-graph complexity and put
  unsafe native failures inside the process.
- Individually composing demuxer, decoder, scaler, x264, AAC, FLV, and RTMP
  crates would create a custom media framework for one source switch.
- A strictly Rust-only H.264/AAC pipeline is not mature enough for this
  production Owncast path.

References:

- [GStreamer Rust bindings](https://gstreamer.freedesktop.org/documentation/rust/stable/latest/docs/gstreamer/index.html)
- [GStreamer dynamic pipeline design](https://gstreamer.freedesktop.org/documentation/additional/design/dynamic.html)

## Architecture

Run one pipeline and one RTMP connection for the complete session:

```text
Lobby video + silence ─┐
                       ├─ clock-synced selectors ─ H.264/AAC ─ FLV ─ Owncast
Movie A/V/subtitles ───┘
```

The lobby branch starts immediately. Blocking pad probes hold the selected
movie audio and video immediately before the selectors after each has produced
its first buffer; bounded queues prevent the file source from running ahead.
When the operator presses Enter, Rust rebases the held movie branch to the
pipeline's current running time, schedules the audio and video selector changes
at one clock boundary, and releases both probes.

The common encoder, FLV muxer, RTMP sink, and pipeline clock never restart
during the handoff. This removes the current MPEG-TS byte prefix, relay thread,
second realtime throttle, and timestamp discontinuity.

## Components

### Rust control plane

- Parse `VIDEO [SUBTITLES] [TITLE]`.
- Validate files and protected Owncast credentials.
- Initialize GStreamer and verify every required element exists.
- Select the media streams by language and role.
- Build and control the pipeline.
- Handle Enter, bus errors, EOS, and shutdown.
- Keep the existing native Rust HTTP title update:
  - `Starting soon: TITLE` before lobby playback.
  - `TITLE` at the movie cut.

### Lobby source

Use a live black video source, two native text overlays, and a live silence
source. Produce the same 1920x1080 title card and audio caps as the common
pipeline. No custom frame renderer is required.

### Movie source

Use a dynamic decode source and link only the selected streams:

1. Prefer English audio.
2. If English is unavailable, fall back to the default audio stream.
3. Prefer non-SDH embedded English subtitles.
4. If embedded English is unavailable, use the supplied external SRT.
5. If neither is available, continue without subtitles.

The current source file places Italian audio first and marks it default. The
new explicit selection prevents an Italian dub from being paired accidentally
with English subtitles.

### Common media path

Video:

- Convert, scale, crop, and rate-convert to 1920x1080 at 30 fps.
- Burn subtitles before selection.
- Encode H.264 at 6 Mbps.
- Use a two-second GOP, closed keyframes, and a live-streaming latency preset.

Audio:

- Convert and resample to 48 kHz stereo.
- Retain high-pass filtering and dynamic compression.
- Omit dynamic loudness normalization in the first implementation because it
  complicates a clocked live pipeline and is not required for the handoff.
- Encode AAC at 192 kbps.

Output:

- Parse encoded H.264 and AAC.
- Mux streamable FLV.
- Publish through one RTMP sink to the configured Owncast URL.

## Handoff

The movie branch is ready only when both selected streams have reached their
blocking probes. Enter before that point records the request, but the switch
waits for both streams or reports the movie's concrete error. The control plane
then calculates one future pipeline running-time boundary and schedules both
selector changes there.

Movie buffers receive a new time segment whose zero point maps to that
boundary. Audio and video therefore begin together without inheriting file
timestamps or resetting the output timeline. The lobby branch is released
only after both selectors report the movie pads active.

No fixed byte threshold, sleep, or timing heuristic participates in the cut.

## Failure Handling

- Missing plugins: fail before streaming and list every missing element.
- Movie probe/decode/preroll failure: keep the lobby live and print the
  concrete movie error.
- RTMP, muxer, or encoder failure: print the GStreamer element and debug
  details, set the pipeline to null, and exit nonzero.
- Movie EOS: send EOS through the common output, wait for the muxer and sink,
  set the pipeline to null, and exit zero.
- Ctrl-C or Rust unwinding: set the pipeline to null so native resources and
  the RTMP connection close.

Do not add an automatic retry loop. A restart policy belongs to the process
supervisor, not the media pipeline.

## Dependencies

Rust:

- `gstreamer` 0.25
- Supporting official GStreamer Rust crates only where their typed APIs are
  required
- Keep `serde` and `ureq`
- Remove `ffprobe` and `libc` after the old process pipeline is deleted

System:

- GStreamer development files
- Base, good, bad, ugly, and libav plugin sets
- H.264 encoder, AAC encoder, subtitle overlay, FLV muxer, and RTMP sink
  elements

The program performs an element preflight rather than assuming package names
or plugin availability.

## Validation

### Automated

- Unit-test English audio selection, default-audio fallback, embedded English
  subtitle selection, external-SRT fallback, and no-subtitle behavior.
- Generate a short synthetic file containing visible timecodes, an audio beep,
  and subtitle events around the handoff.
- Assert that the pipeline constructs only when all required elements exist.
- Assert bus errors return a nonzero result with the originating element.

### Live Owncast

- Start the lobby and confirm exactly one inbound Owncast connection.
- Switch to the synthetic movie and confirm the connection does not restart.
- Inspect retained HLS packets and rendered frames.
- Require audio, video, and subtitle events to align within 50 ms.
- Run a short segment of the current multilingual movie and confirm English
  audio is selected.
- Verify the two Owncast title updates, existing SSO redirect, and Owncast
  health.

## Deliberate Limits

- Support only lobby-to-movie switching, not a general compositor or audio
  mixer.
- Support one movie per process invocation.
- Keep one fixed output quality; Owncast continues producing viewer variants.
- Do not add fades, seeking, playlists, recording, reconnection, hardware
  encoding, or runtime pipeline reconfiguration.
- Do not write custom codecs, FLV muxing, or RTMP protocol code.
