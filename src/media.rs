use gstreamer as gst;
use gstreamer_pbutils::{Discoverer, DiscovererResult, prelude::DiscovererStreamInfoExt};
use std::{
    error::Error,
    path::{Path, PathBuf},
};
use torrent_name_parser::Metadata;

use crate::error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StreamKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StreamCandidate {
    pub id: String,
    pub kind: StreamKind,
    pub language: Option<String>,
    pub is_default: bool,
    pub is_sdh: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SubtitleSource {
    Embedded(String),
    External(PathBuf),
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    pub video_id: String,
    pub audio_id: String,
    pub subtitle: SubtitleSource,
}

#[derive(Clone, Debug)]
pub(crate) struct MediaInfo {
    pub(crate) path: PathBuf,
    pub(crate) subtitles: Option<PathBuf>,
    pub(crate) title: String,
    pub(crate) duration: gst::ClockTime,
}

pub(crate) fn resolve_title(explicit: Option<&str>, embedded: Option<&str>, path: &Path) -> String {
    if let Some(title) = explicit
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or_else(|| embedded.map(str::trim).filter(|title| !title.is_empty()))
    {
        return title.to_owned();
    }
    if let Some(title) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| Metadata::from(name).ok())
        .map(|metadata| metadata.title().trim().to_owned())
        .filter(|title| !title.is_empty())
    {
        return title;
    }
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn validated_duration(
    result: DiscovererResult,
    seekable: bool,
    duration: Option<gst::ClockTime>,
) -> Result<gst::ClockTime, String> {
    if result != DiscovererResult::Ok {
        return Err(format!("Media discovery failed: {result:?}"));
    }
    if !seekable {
        return Err("Media is not seekable".into());
    }
    duration
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "Media duration is unavailable".into())
}

pub(crate) fn discover(
    path: &Path,
    subtitles: Option<PathBuf>,
    explicit_title: Option<&str>,
) -> Result<MediaInfo, Box<dyn Error>> {
    let discover = || -> Result<MediaInfo, Box<dyn Error>> {
        gst::init()?;
        let uri = gst::glib::filename_to_uri(path, None)?;
        let info = Discoverer::new(gst::ClockTime::from_seconds(10))?.discover_uri(&uri)?;
        let duration = validated_duration(info.result(), info.is_seekable(), info.duration())
            .map_err(error)?;
        let embedded = info
            .stream_info()
            .and_then(|stream| stream.tags())
            .and_then(|tags| {
                tags.get::<gst::tags::Title>()
                    .map(|title| title.get().to_owned())
            });

        Ok(MediaInfo {
            path: path.to_owned(),
            subtitles,
            title: resolve_title(explicit_title, embedded.as_deref(), path),
            duration,
        })
    };

    discover().map_err(|failure| {
        error(format!(
            "Cannot discover video {}: {failure}",
            path.display()
        ))
    })
}

pub(crate) fn select_streams(
    streams: &[StreamCandidate],
    external: Option<&Path>,
) -> Result<Selection, String> {
    let video = streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video && stream.is_default)
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.kind == StreamKind::Video)
        })
        .ok_or_else(|| "Movie has no video stream".to_owned())?;
    let audio = streams
        .iter()
        .find(|stream| {
            stream.kind == StreamKind::Audio
                && matches!(stream.language.as_deref(), Some("eng" | "en"))
        })
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.kind == StreamKind::Audio && stream.is_default)
        })
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream.kind == StreamKind::Audio)
        })
        .ok_or_else(|| "Movie has no audio stream".to_owned())?;
    let subtitle = external
        .map(|path| SubtitleSource::External(path.to_owned()))
        .or_else(|| {
            streams
                .iter()
                .find(|stream| {
                    stream.kind == StreamKind::Subtitle
                        && matches!(stream.language.as_deref(), Some("eng" | "en"))
                        && !stream.is_sdh
                })
                .map(|stream| SubtitleSource::Embedded(stream.id.clone()))
        })
        .unwrap_or(SubtitleSource::None);

    Ok(Selection {
        video_id: video.id.clone(),
        audio_id: audio.id.clone(),
        subtitle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(
        id: &str,
        kind: StreamKind,
        language: Option<&str>,
        default: bool,
    ) -> StreamCandidate {
        StreamCandidate {
            id: id.into(),
            kind,
            language: language.map(str::to_owned),
            is_default: default,
            is_sdh: false,
        }
    }

    fn base() -> Vec<StreamCandidate> {
        vec![
            stream("video", StreamKind::Video, None, true),
            stream("ita", StreamKind::Audio, Some("ita"), true),
            stream("eng", StreamKind::Audio, Some("eng"), false),
        ]
    }

    #[test]
    fn prefers_english_audio_over_default() {
        assert_eq!(select_streams(&base(), None).unwrap().audio_id, "eng");
    }

    #[test]
    fn falls_back_to_default_audio() {
        let streams = vec![
            stream("video", StreamKind::Video, None, true),
            stream("ita", StreamKind::Audio, Some("ita"), true),
        ];
        assert_eq!(select_streams(&streams, None).unwrap().audio_id, "ita");
    }

    #[test]
    fn falls_back_to_first_audio_when_metadata_has_no_preference() {
        let streams = vec![
            stream("video", StreamKind::Video, None, true),
            stream("audio", StreamKind::Audio, None, false),
        ];
        assert_eq!(select_streams(&streams, None).unwrap().audio_id, "audio");
    }

    #[test]
    fn explicit_subtitles_override_embedded_english() {
        let mut streams = base();
        let mut sdh = stream("eng-sdh", StreamKind::Subtitle, Some("eng"), true);
        sdh.is_sdh = true;
        streams.push(sdh);
        streams.push(stream(
            "eng-dialogue",
            StreamKind::Subtitle,
            Some("eng"),
            false,
        ));
        assert_eq!(
            select_streams(&streams, Some(Path::new("fallback.srt")))
                .unwrap()
                .subtitle,
            SubtitleSource::External("fallback.srt".into())
        );
        assert_eq!(
            select_streams(&streams, None).unwrap().subtitle,
            SubtitleSource::Embedded("eng-dialogue".into())
        );
    }

    #[test]
    fn falls_back_to_external_then_none() {
        let streams = base();
        assert_eq!(
            select_streams(&streams, Some(Path::new("fallback.srt")))
                .unwrap()
                .subtitle,
            SubtitleSource::External("fallback.srt".into())
        );
        assert_eq!(
            select_streams(&streams, None).unwrap().subtitle,
            SubtitleSource::None
        );
    }

    #[test]
    fn rejects_missing_video_or_supported_audio() {
        assert_eq!(
            select_streams(&[], None).unwrap_err(),
            "Movie has no video stream"
        );
        let video_only = vec![stream("video", StreamKind::Video, None, true)];
        assert_eq!(
            select_streams(&video_only, None).unwrap_err(),
            "Movie has no audio stream"
        );
    }

    #[test]
    fn title_precedence_prefers_explicit_then_embedded() {
        let path = Path::new("Passenger.2024.1080p.BluRay.mkv");

        assert_eq!(
            resolve_title(Some(" Director's Cut "), Some("Embedded"), path),
            "Director's Cut"
        );
        assert_eq!(
            resolve_title(Some(""), Some(" Embedded "), path),
            "Embedded"
        );
    }

    #[test]
    fn title_falls_back_to_parsed_filename_then_raw_stem() {
        assert_eq!(
            resolve_title(None, None, Path::new("Passenger.2024.1080p.BluRay.mkv")),
            "Passenger"
        );
        assert_eq!(resolve_title(None, None, Path::new("1080p.mkv")), "1080p");
    }

    #[test]
    fn discovery_requires_ok_seekable_nonzero_duration() {
        use gstreamer_pbutils::DiscovererResult;

        let minute = gst::ClockTime::from_seconds(60);
        assert_eq!(
            validated_duration(DiscovererResult::Ok, true, Some(minute)).unwrap(),
            minute
        );
        assert!(validated_duration(DiscovererResult::Error, true, Some(minute)).is_err());
        assert!(validated_duration(DiscovererResult::Ok, false, Some(minute)).is_err());
        assert!(
            validated_duration(DiscovererResult::Ok, true, Some(gst::ClockTime::ZERO)).is_err()
        );
        assert!(validated_duration(DiscovererResult::Ok, true, None).is_err());
    }

    #[test]
    fn discovery_error_names_the_video_path() {
        let path = Path::new("/definitely/missing/playlist-entry.mkv");

        let failure = discover(path, None, None).err().unwrap();

        assert!(failure.to_string().contains(&path.display().to_string()));
    }
}
