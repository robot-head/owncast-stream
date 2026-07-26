use std::path::{Path, PathBuf};

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
        .ok_or_else(|| "Movie has no English or default audio stream".to_owned())?;
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
            "Movie has no English or default audio stream"
        );
    }
}
