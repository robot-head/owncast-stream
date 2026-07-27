# Relative Paths and Audio Gain

## Goal

Accept video and subtitle paths relative to the caller's working directory and
raise movie audio by a fixed 3 dB without changing the command line.

## Path resolution

Resolve both media arguments once during `Config::parse`. A shared helper
receives the startup working directory and the supplied path:

- absolute paths remain absolute;
- relative paths are joined to the startup working directory;
- the resolved path must be a regular file;
- downstream media and subtitle code receives the resolved absolute path.

Keep the existing usage and readable-file error behavior. Do not change the
process working directory or add path configuration.

## Audio gain

Add GStreamer's existing `volume` element after `audiodynamic` and before AAC
encoding. Set its linear gain to `1.4125375`, equivalent to +3 dB. Name it
`audio_gain` so construction tests can inspect the configured value, and add
`volume` to the existing required-element preflight.

Keep the current high-pass filter, compressor, AAC settings, and lobby silence
unchanged. Add no gain option, normalization pass, limiter, or dependency.

## Verification

Use test-first changes:

1. a path test proves a relative repository file resolves to an absolute file;
2. a pipeline construction test proves `audio_gain` exists with +3 dB gain;
3. run formatting, all non-decoder tests, all-target Clippy, and release build.

The existing ignored synthetic decoder test remains omitted.
