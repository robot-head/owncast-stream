# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/robot-head/owncast-stream/releases/tag/v0.1.0) - 2026-07-28

### Added

- add amber audio console
- add gain control and audio metering
- wire terminal playback controls
- add playback session controls
- add playback status UI
- discover media title and duration
- add continuous Owncast streamer

### Fixed

- stop audio dropouts at the pipeline hand-off ([#10](https://github.com/robot-head/owncast-stream/pull/10))
- propagate paused seek errors
- seek to paused media end
- freeze playback-side frames
- handle silence and compact terminals
- bound audio meter levels
- preserve symlink media names
- skip bitmap subtitle tracks

### Other

- *(deps)* update actions/checkout action to v7 ([#12](https://github.com/robot-head/owncast-stream/pull/12))
- *(deps)* update release-plz/action action to v0.5 ([#11](https://github.com/robot-head/owncast-stream/pull/11))
- Add initial Renovate configuration file
- Add remote Owncast connection options ([#8](https://github.com/robot-head/owncast-stream/pull/8))
- Add TUI demo image to README
- Demo gif
- assert EOF freeze is preserved
- plan paused frame fixes
- define paused frame fixes
- describe gain and VU controls
- bind console labels to panels
- strengthen audio console coverage
- tighten gain meter test plan
- plan gain controls and VU meter
- adopt amber console layout
- define gain controls and VU meter
- automate Linux releases
- add Rust checks
- plan CI and release automation
- define CI and release automation
- describe playback controls
- separate playback from broadcast
- add playback dependencies
- plan playback controls
- define playback controls
- Fix regressions in audio track selection and compilation
- Merge branch 'main' into codex/skip-bitmap-subtitles
- plan bitmap subtitle fallback
- design bitmap subtitle fallback
- retain Rust build ignore
- ignore local worktrees
- plan GStreamer pipeline migration
- design GStreamer media pipeline
