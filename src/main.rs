use serde::Serialize;
mod media;
mod pipeline;
mod ui;
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const STREAM_KEY_FILE: &str = "/opt/owncast/stream-key";
const TITLE_TOKEN_FILE: &str = "/opt/owncast/title-token";
const DEFAULT_RTMP_URL: &str = "rtmp://127.0.0.1/live";
const DEFAULT_TITLE_URL: &str = "http://127.0.0.1:8081/api/integrations/streamtitle";
const USAGE: &str = "Usage: owncast-stream [OPTIONS] VIDEO [SUBTITLES] [TITLE]\n\
Options:\n\
  --rtmp-url URL       RTMP publish URL without the stream key\n\
  --api-url URL        Owncast stream-title integration endpoint\n\
  --stream-key KEY     Stream key (defaults to /opt/owncast/stream-key)\n\
  --api-key KEY        Integration token (defaults to /opt/owncast/title-token)";

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
    Ok(path)
}

struct Config {
    video: PathBuf,
    subtitles: Option<PathBuf>,
    title: Option<String>,
    stream_key: String,
    title_token: String,
    rtmp_url: String,
    title_url: String,
}

impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let _program = args.next();
        let mut values = Vec::new();
        let mut stream_key = None;
        let mut title_token = None;
        let mut rtmp_url = None;
        let mut title_url = None;
        let mut args = args.peekable();
        while let Some(argument) = args.next() {
            let destination = match argument.as_str() {
                "--stream-key" => &mut stream_key,
                "--api-key" | "--title-token" => &mut title_token,
                "--rtmp-url" => &mut rtmp_url,
                "--api-url" | "--title-url" => &mut title_url,
                "--" => {
                    values.extend(args);
                    break;
                }
                value if value.starts_with('-') => {
                    return Err(error(format!("Unknown option: {value}\n{USAGE}")));
                }
                _ => {
                    values.push(argument);
                    continue;
                }
            };
            *destination = Some(
                args.next()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| error(format!("Missing value for {argument}\n{USAGE}")))?,
            );
        }
        if values.is_empty() || values.len() > 3 {
            return Err(error(USAGE));
        }

        let cwd = env::current_dir()?;
        let video = resolve_media_path(&cwd, &values[0], "video")?;
        let subtitles = values
            .get(1)
            .filter(|value| !value.is_empty())
            .map(|value| resolve_media_path(&cwd, value, "subtitles"))
            .transpose()?;
        let title = values
            .get(2)
            .map(|title| title.trim())
            .filter(|title| !title.is_empty())
            .map(str::to_owned);

        Ok(Self {
            video,
            subtitles,
            title,
            stream_key: match stream_key {
                Some(value) => value.trim().to_owned(),
                None => read_secret(STREAM_KEY_FILE, "stream key")?,
            },
            title_token: match title_token {
                Some(value) => value.trim().to_owned(),
                None => read_secret(TITLE_TOKEN_FILE, "title token")?,
            },
            rtmp_url: rtmp_url.unwrap_or_else(|| DEFAULT_RTMP_URL.to_owned()),
            title_url: title_url.unwrap_or_else(|| DEFAULT_TITLE_URL.to_owned()),
        })
    }
}

fn read_secret(path: &str, name: &str) -> Result<String, Box<dyn Error>> {
    let value =
        fs::read_to_string(path).map_err(|_| error(format!("Cannot read {name}: {path}")))?;
    Ok(value.trim().to_owned())
}

#[derive(Serialize)]
struct TitleBody<'a> {
    value: &'a str,
}

fn set_title(url: &str, token: &str, title: &str) -> Result<(), Box<dyn Error>> {
    ureq::post(url)
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(&TitleBody { value: title })?;
    Ok(())
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let result = Config::parse(args.into_iter()).and_then(|config| {
        let media = media::discover(&config.video, config.title.as_deref())?;
        let mut session = pipeline::StreamSession::new(&config, &media)?;
        ratatui::run(|terminal| ui::run(terminal, &mut session))
    });
    if let Err(failure) = result {
        eprintln!("{failure}");
        std::process::exit(if failure.to_string().contains("Usage: owncast-stream") {
            2
        } else {
            1
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn resolves_relative_media_path_from_startup_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let resolved = resolve_media_path(root, "Cargo.toml", "video").unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, root.join("Cargo.toml").canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_symlink_name_in_resolved_media_path() {
        use std::os::unix::fs::symlink;

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("owncast-path-{suffix}"));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("8f31.mkv"), []).unwrap();
        symlink("8f31.mkv", root.join("premiere.mkv")).unwrap();

        let resolved = resolve_media_path(&root, "premiere.mkv", "video").unwrap();

        assert_eq!(resolved, root.join("premiere.mkv"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_remote_owncast_options() {
        let video = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let config = Config::parse(
            [
                "owncast-stream",
                "--rtmp-url",
                "rtmp://owncast.example/live/",
                "--api-url",
                "https://owncast.example/api/integrations/streamtitle",
                "--stream-key",
                "remote-key",
                "--api-key",
                "remote-token",
                video.to_str().unwrap(),
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();

        assert_eq!(config.rtmp_url, "rtmp://owncast.example/live/");
        assert_eq!(
            config.title_url,
            "https://owncast.example/api/integrations/streamtitle"
        );
        assert_eq!(config.stream_key, "remote-key");
        assert_eq!(config.title_token, "remote-token");
    }
}
