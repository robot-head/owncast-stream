# Playback Controls and Console Status

## Goal

Add keyboard pause, resume, and 30-second seek controls; keep Owncast live
with a frozen movie frame and silence while paused; show title, state, and time
in Ratatui; and derive the title from media metadata before parsing the
filename.

## Branch and dependencies

Implement on `feat/playback-controls`, created directly from merged
`origin/main` commit `209954480fb2d208d6cf165ba09cc134110c7ed7`.

Add only:

- `ratatui = "0.30.2"` for terminal rendering and input;
- `gstreamer-pbutils = "0.25"` for synchronous metadata and duration discovery;
- `torrent-name-parser = "0.12.1"` for filename fallback titles.

Use Ratatui's re-exported Crossterm API rather than adding Crossterm
separately.

## Media discovery and title

Discover the absolute movie URI once before streaming with a bounded
GStreamer `Discoverer`. Store its duration and global `Title` tag in a small
media-info value.

Resolve the displayed and Owncast title in this order:

1. non-empty explicit CLI `TITLE`;
2. non-empty GStreamer `Title` metadata;
3. `torrent_name_parser::Metadata::from(file_name).title()`;
4. raw file stem when the parser fails or returns an empty title.

Discovery failure is fatal because duration is required for bounded seeking
and the status display. Filename parsing failure is not fatal.

## Pipeline architecture

Keep one process but separate playback from broadcast output:

- the broadcast pipeline owns the lobby sources, input selectors, encoders,
  muxer, +3 dB audio gain, and the single RTMP sink;
- the playback pipeline owns movie decoding, selected English audio,
  subtitle overlay, and seeking;
- `intervideosink`/`intervideosrc` and
  `interaudiosink`/`interaudiosrc` carry raw media between the pipelines.

The broadcast pipeline remains `Playing` from lobby start through shutdown.
The playback pipeline prerolls in `Paused`, starts when Enter switches the
selectors to the movie, and can later pause or seek without stopping RTMP.

Set the inter-video source timeout so it continues repeating the latest movie
frame during a playback pause. On pause, switch the broadcast audio selector
to the existing lobby-silence pad before pausing playback. On resume, start
playback and return the audio selector to the movie pad. This produces a
frozen current frame and silence while retaining the publisher connection.

Seek the playback pipeline with `FLUSH | KEY_UNIT`. Left subtracts 30 seconds
and clamps at zero. Right adds 30 seconds and clamps at the discovered
duration. A seek preserves whether playback was playing or paused; when
paused, the newly sought frame becomes the frozen frame while audio remains
silent.

## Terminal application

Replace the blocking `stdin().read_line()` thread with one Ratatui-owned event
loop. Ratatui manages raw mode, alternate-screen setup, panic cleanup, and
terminal restoration.

Render a compact fullscreen status view every 100 milliseconds:

```text
Passenger  PLAYING  00:42:18 / 01:47:03

Enter Start · Space Pause/Resume · ←/→ ±30s · q Quit
```

States are `LOBBY`, `PLAYING`, and `PAUSED`. In the lobby, only Enter and `q`
act. After movie start:

- Space toggles pause/resume;
- Left seeks backward 30 seconds;
- Right seeks forward 30 seconds;
- `q` exits cleanly.

The UI reads movie position from the playback pipeline and uses the discovered
duration. Hours may exceed two digits; unknown position displays zero. Bus,
state-change, discovery, seek, drawing, and terminal errors end the command
with a visible error after terminal restoration.

## Internal boundaries

Keep terminal concerns outside `pipeline.rs`:

- `ui.rs` owns key mapping, state/time formatting, and Ratatui rendering;
- `media.rs` owns discovery and title precedence alongside stream selection;
- `pipeline.rs` owns both GStreamer pipelines and exposes start, pause,
  resume, seek, position, duration, and bus-processing operations to the
  event loop;
- `main.rs` parses arguments, resolves media info, and starts the terminal
  application.

Do not add a general command framework, async runtime, configuration layer, or
custom widget abstraction.

## Verification

Use test-first changes and no media decoder fixtures:

- title precedence covers explicit, embedded, parsed filename, and raw-stem
  fallback;
- key mapping covers Enter, Space, arrows, `q`, and ignored keys by state;
- time formatting covers zero, hours, and durations over 99 hours;
- seek target math covers both directions and duration clamping;
- a synthetic two-pipeline GStreamer test proves paused playback leaves the
  broadcast pipeline producing repeated video frames and silence;
- existing lobby-to-movie, timestamp, stream-selection, relative-path, audio
  gain, no-subprocess, and cleanup tests remain green;
- formatting, all non-decoder tests, all-target Clippy, and release build pass.

The existing `synthetic_handoff_stays_within_50ms` decoder test remains
ignored.
