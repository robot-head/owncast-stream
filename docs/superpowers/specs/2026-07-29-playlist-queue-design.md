# Playlist Queue and File Chooser

## Goal

Allow `owncast-stream` to queue multiple videos, display and edit the queue in
the TUI, add videos through an in-terminal file chooser at any time, and move
to the previous or next track without reconnecting the Owncast broadcast.

## Command-line interface

Preserve the existing invocation:

```text
owncast-stream [OPTIONS] VIDEO [SUBTITLES] [TITLE]
```

Add a repeatable option:

```text
--queue VIDEO        Append a video to the startup playlist
```

The positional video remains the first playlist entry. Its optional subtitle
and explicit title arguments continue to apply only to that entry. Each
`--queue` entry uses automatic title and embedded-subtitle discovery.

Resolve every supplied path relative to the startup working directory and
discover every startup entry before opening the RTMP connection. Reject the
whole command when an entry is unreadable, unseekable, has no duration, or
cannot be discovered; identify the failing path in the error.

## Playlist model

Store playlist entries in a `Vec` in playback order. Each entry contains:

- the absolute video path;
- an optional external subtitle path;
- its resolved display and Owncast title;
- its discovered duration.

The playlist separately tracks the active entry and the selected row used for
editing. Entries remain in the list after playback, so Previous can restart a
played track from the beginning.

While a track is active:

- its row is locked and cannot be moved or removed;
- entries before it may be reordered among themselves;
- entries after it may be reordered among themselves;
- no entry may be moved across the active row.

Deleting or moving a non-active entry updates the active and selected indices
as necessary. When no track is active, no row is locked.

Previous and Next restart the adjacent entry at position zero. At either
boundary the command is a no-op. Manual Previous and Next preserve whether
playback was Playing or Paused.

Natural end-of-stream automatically starts the next entry in Playing state.
After the final entry, release playback, select the broadcast lobby, and keep
the RTMP connection and TUI running. A video appended afterward becomes
pending and Enter starts the first pending entry. Previous from the final
lobby restarts the last entry.

## Pipeline lifecycle

Keep one `BroadcastPipeline` alive from startup through shutdown. It continues
to own the lobby, video/audio selectors, gain, meters, encoders, muxer, and
RTMP sink.

Group the track-specific `PlaybackPipeline` and `AudioBridge` into one
disposable active-playback value. `StreamSession` owns the playlist and an
optional active-playback value.

To change tracks:

1. retain the current Playing or Paused state for a manual change;
2. freeze the last available movie frame and select lobby silence;
3. stop and drop the old audio bridge and playback pipeline;
4. construct the selected entry's playback pipeline;
5. wait for stream selection and the first video frame;
6. update the Owncast title;
7. select the movie output and either play it or keep its first frame frozen.

Automatic end-of-stream follows the same path but always starts the next entry
Playing. After final end-of-stream, select the existing lobby video and silence
instead of building another playback pipeline.

Gain belongs to the broadcast pipeline and therefore survives track changes.
The RTMP pipeline never leaves Playing during track changes or the final lobby.

If a discovered track later fails to initialize or decode, restore the terminal
and exit through the existing fatal pipeline-error path. Do not add rollback or
preloading machinery.

## TUI layout

Use the approved persistent lower-panel layout:

1. show information and current playback status;
2. program audio meter;
3. full-width playlist;
4. compact key-help footer.

Remove the large per-control tiles. Retain the current amber-on-black console
style and responsive behavior. At 80 columns by 24 rows, the current title,
state, time, gain, stereo meter, playlist, and key help must remain visible.

Each playlist row shows:

- one-based order;
- a state marker for played, active, or pending;
- title;
- duration;
- `LOCKED` on the active row.

Long titles truncate to the available width. When the list exceeds the panel,
scroll its viewport to keep the active row or edit selection visible.

## TUI controls

Playback focus keeps the existing controls:

| Key | Action |
| --- | --- |
| Enter | Start the first pending entry from the lobby |
| Space | Pause or resume |
| Left / Right | Seek backward or forward 30 seconds |
| Up / Down | Adjust gain by 1 dB |
| `p` / `n` | Previous or next track |
| `a` | Open the file chooser |
| Tab | Focus playlist editing |
| `q` or Ctrl-C | Quit |

Playlist focus uses:

| Key | Action |
| --- | --- |
| Up / Down | Select a row |
| Shift+Up / Shift+Down | Move the selected unlocked row |
| Delete or `d` | Remove the selected unlocked row |
| `a` | Open the file chooser |
| Tab or Esc | Return to playback focus |
| `p` / `n` | Previous or next track |
| `q` or Ctrl-C | Quit |

Ignored operations leave the playlist unchanged. The footer changes with the
active focus so the available keys remain visible.

## File chooser

Implement the chooser with `std::fs::read_dir` and existing Ratatui widgets.
Do not add a dependency.

Open it at the active video's parent directory, or the startup working
directory when no track is active. Show the parent entry, child directories,
and regular files sorted by name. Do not filter by extension because GStreamer
support is capability-based.

Chooser controls are:

| Key | Action |
| --- | --- |
| Up / Down | Select an entry |
| Enter | Enter a directory or add a regular file |
| Backspace | Move to the parent directory |
| Esc | Close without adding |

Playback and broadcast polling continue while the chooser is open. Discover a
selected file before appending it. Discovery failure keeps the chooser open,
does not change the playlist, and displays the path-specific error inline.
Successful insertion closes the chooser and selects the new playlist row.

Do not add mouse support, search, persistence, multi-select, recursive folder
addition, or playlist serialization.

## Internal boundaries

Follow the existing modules:

- `main.rs` parses repeated `--queue` options and constructs startup entries;
- `media.rs` owns entry discovery and title resolution;
- `pipeline.rs` owns playlist playback state and track replacement while
  retaining the single broadcast pipeline;
- `ui.rs` owns playlist/chooser focus, key mapping, filesystem navigation, and
  rendering.

Do not add a general command framework, async runtime, separate playlist
service, preloader, or new dependency.

## Errors and status

Startup path and discovery errors remain fatal and print after terminal
restoration. Runtime chooser errors appear inside the chooser and leave the
current stream untouched. Locked or boundary playlist operations are harmless
no-ops.

Existing broadcast, playback bus, title update, draw, and terminal errors
remain fatal. Error messages for queued entries include the relevant path.

## Verification

Use test-first changes. Cover:

- repeatable `--queue` parsing, ordering, relative paths, missing values, and
  legacy positional compatibility;
- playlist insertion, selection, current-row locking, deletion, reordering on
  either side of the current row, and rejection of moves across it;
- Previous, Next, boundary no-ops, state preservation, automatic end-of-stream,
  final-lobby behavior, and starting an entry appended after final EOF;
- chooser directory sorting, parent navigation, file insertion, cancellation,
  and inline discovery failure without playlist mutation;
- key mapping in playback, playlist, and chooser focus;
- playlist rendering, scrolling, truncation, locked/current markers, and the
  complete approved layout at 80x24;
- Owncast title changes for manual and automatic track changes;
- a synthetic GStreamer transition proving the broadcast pipeline continues
  producing output while playback is replaced and after final EOF;
- all existing path, title, stream-selection, pause, seek, gain, audio bridge,
  handoff, cleanup, and no-subprocess tests.

Run:

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
```
