# Gain Controls and VU Meter Design

## Goal

Add live keyboard gain control and a stereo terminal VU meter without changing
the stream connection or adding a metering dependency.

## Gain control

Up raises gain by 1 dB and Down lowers gain by 1 dB in the lobby, while playing,
and while paused. Gain starts at +3 dB and is clamped to the inclusive range
from -12 dB to +12 dB.

`StreamSession` owns the current gain in dB. Adjusting it updates the existing
GStreamer `volume` element using the amplitude conversion `10^(dB / 20)`. The
TUI displays the current signed gain value.

## Audio metering

Add GStreamer's native `level` element immediately after `audio_gain`, so the
meter reflects the post-gain signal sent to the encoder. Configure it to post
messages every 100 ms with:

- `peak-ttl=3000000000` for a 3-second peak hold
- `peak-falloff=12` for 12 dB per second decay
- `post-messages=true`

The existing broadcast bus polling reads `level` element messages and retains
the first two channel values from both the `peak` and `decay` arrays. Missing,
malformed, or short messages do not stop streaming; they leave the last meter
reading unchanged. Normal GStreamer error messages remain fatal.

The meter starts at -60 dB for both channels. Values below -60 dB, including
negative infinity for silence, render at the floor; values above 0 dB render at
the ceiling.

## Terminal layout

Restyle the full-screen TUI after the supplied IMAX projection-console
reference:

- amber monochrome foreground and borders on black
- dense square-cornered control panels
- inverted amber status strips
- double-bordered, centered playback-state banner
- uppercase monospaced labels and values

The wide layout has a main console and a narrower right status rail. The main
console contains:

1. `OWNCAST` and `SHOW LOCAL - AUTO MODE` header
2. bordered `SHOW INFORMATION` title panel
3. inverted playback/stream status strip
4. elapsed time, duration, and signed gain row
5. bordered `PROGRAM AUDIO / 3 SEC PEAK HOLD` panel with separate `L` and `R`
   horizontal meters scaled from -60 to 0 dB
6. double-bordered `LOBBY`, `PLAYING`, or `PAUSED` banner
7. five bordered key panels for Enter, Space, Left/Right, Up/Down, and Q

Each meter shows the current peak as its filled bar, the held/decaying peak as a
marker, and the current numeric dB value. The right rail shows compact stream
state indicators, the current gain, and held left/right peak values.

When the terminal is too narrow for two columns, stack the status rail below the
main console. The central title, state, time, gain, meters, and key help must
remain visible; decorative labels may shorten before data is clipped.

## Code boundaries

- `src/pipeline.rs` configures `level`, parses its messages, stores readings,
  and adjusts the existing `volume` element.
- `src/ui.rs` maps Up and Down, draws gain and meters, and forwards gain
  commands to `StreamSession`.
- `README.md` documents the new controls and display.

No new crate, audio branch, thread, or custom sample analysis is added.

## Verification

Tests cover:

- Up and Down key mapping in every playback state
- 1 dB steps and -12/+12 dB clamping
- dB-to-amplitude conversion applied to `audio_gain`
- parsing stereo `peak` and `decay` values from a `level` message
- configured 100 ms interval, 3-second hold, and 12 dB/sec falloff
- TUI gain text, left/right bars, decay markers, and help

Run formatting, Clippy, all targets, all tests, the release build, and workflow
lint before publishing.
