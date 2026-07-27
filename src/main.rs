use serde::Serialize;
mod media;
mod pipeline;
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const STREAM_KEY_FILE: &str = "/opt/owncast/stream-key";
const TITLE_TOKEN_FILE: &str = "/opt/owncast/title-token";

#[derive(Debug)]
struct MessageError(String);

impl std::fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MessageError {}

fn error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(MessageError(message.into()))
}

fn resolve_media_path(cwd: &Path, value: &str, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let supplied = PathBuf::from(value);
    let path = if supplied.is_absolute() {
        supplied
    } else {
        cwd.join(supplied)
    };
    if !path.is_file() {
        return Err(error(format!("Cannot read {name}: {value}")));
    }
    path.canonicalize()
        .map_err(|_| error(format!("Cannot read {name}: {value}")))
}

struct Config {
    video: PathBuf,
    subtitles: Option<PathBuf>,
    title: String,
    stream_key: String,
    title_token: String,
}

impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let _program = args.next();
        let values: Vec<_> = args.collect();
        if values.is_empty() || values.len() > 3 {
            return Err(error("Usage: owncast-stream VIDEO [SUBTITLES] [TITLE]"));
        }

        let cwd = env::current_dir()?;
        let video = resolve_media_path(&cwd, &values[0], "video")?;
        let subtitles = values
            .get(1)
            .filter(|value| !value.is_empty())
            .map(|value| resolve_media_path(&cwd, value, "subtitles"))
            .transpose()?;
        let title = values.get(2).cloned().unwrap_or_else(|| {
            video
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });

        Ok(Self {
            video,
            subtitles,
            title,
            stream_key: read_secret(STREAM_KEY_FILE, "stream key")?,
            title_token: read_secret(TITLE_TOKEN_FILE, "title token")?,
        })
    }
}

fn read_secret(path: &str, name: &str) -> Result<String, Box<dyn Error>> {
    let value =
        fs::read_to_string(path).map_err(|_| error(format!("Cannot read {name}: {path}")))?;
    Ok(value.trim().to_owned())
}

struct Media {
    audio_channels: u32,
    embedded_subtitle: Option<usize>,
}

impl Media {
    fn probe(video: &PathBuf) -> Result<Self, Box<dyn Error>> {
        let probe = ffprobe::ffprobe(video)?;
        let audio_channels = probe
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("audio"))
            .and_then(|stream| stream.channels)
            .filter(|channels| *channels > 0)
            .ok_or_else(|| {
                error(format!(
                    "Cannot determine audio channels: {}",
                    video.display()
                ))
            })? as u32;
        let subtitles: Vec<_> = probe
            .streams
            .into_iter()
            .filter(|stream| stream.codec_type.as_deref() == Some("subtitle"))
            .collect();
        Ok(Self {
            audio_channels,
            embedded_subtitle: select_subtitle(&subtitles),
        })
    }
}

fn is_text_subtitle(stream: &Stream) -> bool {
    !matches!(
        stream.codec_name.as_deref(),
        Some("dvd_subtitle" | "dvb_subtitle" | "xsub" | "hdmv_pgs_subtitle")
    )
}

fn select_subtitle(streams: &[Stream]) -> Option<usize> {
    streams
        .iter()
        .position(|stream| {
            is_text_subtitle(stream)
                && stream
                    .tags
                    .as_ref()
                    .and_then(|tags| tags.language.as_deref())
                    == Some("eng")
        })
        .or_else(|| streams.iter().position(is_text_subtitle))
}

#[derive(Serialize)]
struct TitleBody<'a> {
    value: &'a str,
}

fn set_title(token: &str, title: &str) -> Result<(), Box<dyn Error>> {
    let url = env::var("OWNCAST_TITLE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081/api/integrations/streamtitle".into());
    ureq::post(url)
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(&TitleBody { value: title })?;
    Ok(())
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if !(2..=4).contains(&args.len()) {
        eprintln!("Usage: owncast-stream VIDEO [SUBTITLES] [TITLE]");
        std::process::exit(2);
    }
    let result = Config::parse(args.into_iter()).and_then(|config| pipeline::run(&config));
    if let Err(failure) = result {
        eprintln!("{failure}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;  
    use ffprobe::{Stream, StreamTags};    
    use std::path::Path;

    fn subtitle(codec: &str, language: &str) -> Stream {
        Stream {
            codec_name: Some(codec.into()),
            codec_type: Some("subtitle".into()),
            tags: Some(StreamTags {
                language: Some(language.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn prefers_english_embedded_subtitles() {
        assert_eq!(
            super::select_subtitle(&[subtitle("subrip", "spa"), subtitle("subrip", "eng")]),
            Some(1)
        );
    }

    #[test]
    fn falls_back_to_first_embedded_subtitle() {
        assert_eq!(
            super::select_subtitle(&[subtitle("subrip", "spa")]),
            Some(0)
        );
    }

    #[test]
    fn ignores_bitmap_embedded_subtitles() {
        assert_eq!(
            super::select_subtitle(&[subtitle("hdmv_pgs_subtitle", "eng")]),
            None
        );
    }

    #[test]
    fn reports_no_embedded_subtitles() {
        assert_eq!(super::select_subtitle(&[]), None);
    }

    #[test]
    fn resolves_relative_media_path_from_startup_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let resolved = resolve_media_path(root, "Cargo.toml", "video").unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, root.join("Cargo.toml").canonicalize().unwrap());
    }
}
