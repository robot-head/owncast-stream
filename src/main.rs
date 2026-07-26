use ffprobe::Stream;
use serde::Serialize;
mod media;
mod pipeline;
use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
};

const STREAM_KEY_FILE: &str = "/opt/owncast/stream-key";
const TITLE_TOKEN_FILE: &str = "/opt/owncast/title-token";
const VIDEO_FILTER: &str =
    "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,setsar=1,fps=30";
const AUDIO_FILTER: &str = "highpass=f=80,acompressor=threshold=0.125:ratio=2:attack=20:release=250:makeup=1.4,loudnorm=I=-16:LRA=7:TP=-1.5";
const LOBBY_FILTER: &str = "drawtext=fontfile=/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf:text='PLEASE WAIT':fontcolor=white:fontsize=96:x=(w-text_w)/2:y=(h-text_h)/2-80,drawtext=fontfile=/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf:text='The movie will begin shortly':fontcolor=0xB8C1D9:fontsize=42:x=(w-text_w)/2:y=(h-text_h)/2+70";
const PREFIX_BYTES: usize = 1_880_000;

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

        let video = PathBuf::from(&values[0]);
        if !video.is_file() {
            return Err(error(format!("Cannot read video: {}", video.display())));
        }
        let subtitles = values
            .get(1)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if let Some(path) = &subtitles
            && !path.is_file()
        {
            return Err(error(format!("Cannot read subtitles: {}", path.display())));
        }
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

fn select_subtitle(streams: &[Stream]) -> Option<usize> {
    if streams.is_empty() {
        return None;
    }
    Some(
        streams
            .iter()
            .position(|stream| {
                stream
                    .tags
                    .as_ref()
                    .and_then(|tags| tags.language.as_deref())
                    == Some("eng")
            })
            .unwrap_or(0),
    )
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

fn add_video_encode_args(command: &mut Command) {
    command.args([
        "-c:v",
        "libx264",
        "-preset",
        "medium",
        "-b:v",
        "6000k",
        "-maxrate",
        "6000k",
        "-bufsize",
        "12000k",
        "-g",
        "60",
        "-keyint_min",
        "60",
        "-sc_threshold",
        "0",
        "-pix_fmt",
        "yuv420p",
        "-r",
        "30",
    ]);
}

fn add_audio_encode_args(command: &mut Command, channels: u32) {
    command.args([
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        "-ar",
        "48000",
        "-ac",
        &channels.to_string(),
    ]);
}

fn add_ts_output_args(command: &mut Command) {
    command.args([
        "-f",
        "mpegts",
        "-mpegts_flags",
        "+resend_headers+initial_discontinuity",
        "pipe:1",
    ]);
}

fn publisher_command(url: &str) -> Command {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-re",
        "-dts_delta_threshold",
        "1",
        "-i",
        "pipe:0",
        "-c",
        "copy",
        "-f",
        "flv",
        url,
    ]);
    command.stdin(Stdio::piped());
    command
}

fn lobby_command(channels: u32) -> Command {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "color=c=0x080B14:s=1920x1080:r=30",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
        "-vf",
        LOBBY_FILTER,
    ]);
    add_video_encode_args(&mut command);
    add_audio_encode_args(&mut command, channels);
    add_ts_output_args(&mut command);
    command.stdout(Stdio::piped());
    command
}

fn movie_command(
    config: &Config,
    media: &Media,
) -> Result<(Command, Option<File>), Box<dyn Error>> {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-stats_period",
        "5",
        "-stats",
        "-re",
        "-i",
    ]);
    command.arg(&config.video);

    let (filter, subtitle_file) = if let Some(index) = media.embedded_subtitle {
        (
            format!("{VIDEO_FILTER},subtitles=filename=/dev/fd/3:si={index}"),
            Some(File::open(&config.video)?),
        )
    } else if let Some(path) = &config.subtitles {
        (
            format!("{VIDEO_FILTER},subtitles=filename=/dev/fd/3"),
            Some(File::open(path)?),
        )
    } else {
        (VIDEO_FILTER.to_owned(), None)
    };
    command.args(["-vf", &filter]);
    add_video_encode_args(&mut command);
    command.args(["-af", AUDIO_FILTER]);
    add_audio_encode_args(&mut command, media.audio_channels);
    add_ts_output_args(&mut command);
    command.stdout(Stdio::piped());

    if let Some(file) = &subtitle_file {
        let source_fd = file.as_raw_fd();
        // SAFETY: dup2 is async-signal-safe and this closure performs no allocation.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(source_fd, 3) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    Ok((command, subtitle_file))
}

#[derive(Default)]
struct ProcessGuard(Vec<u32>);

impl ProcessGuard {
    fn track(&mut self, child: &Child) {
        self.0.push(child.id());
    }

    fn finished(&mut self, child: &Child) {
        self.0.retain(|pid| *pid != child.id());
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        for pid in &self.0 {
            // SAFETY: kill is called with an owned child PID and a valid signal.
            unsafe {
                libc::kill(*pid as i32, libc::SIGTERM);
            }
        }
    }
}

fn terminate(child: &Child) {
    // SAFETY: kill is called with an owned child PID and a valid signal.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
}

fn checked_wait(child: &mut Child, role: &str) -> Result<(), Box<dyn Error>> {
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(error(format!("{role} exited with {status}")))
    }
}

fn run(config: Config, media: Media) -> Result<(), Box<dyn Error>> {
    let output_url = env::var("OWNCAST_OUTPUT_URL")
        .unwrap_or_else(|_| format!("rtmp://127.0.0.1/live/{}", config.stream_key));
    let mut guard = ProcessGuard::default();

    set_title(
        &config.title_token,
        &format!("Starting soon: {}", config.title),
    )?;

    let mut publisher = publisher_command(&output_url).spawn()?;
    guard.track(&publisher);
    let mut publisher_input = publisher
        .stdin
        .take()
        .ok_or_else(|| error("Cannot open publisher input"))?;

    let mut lobby = lobby_command(media.audio_channels).spawn()?;
    guard.track(&lobby);
    let mut lobby_output = lobby
        .stdout
        .take()
        .ok_or_else(|| error("Cannot open lobby output"))?;
    let lobby_copy = thread::spawn(move || {
        let result = io::copy(&mut lobby_output, &mut publisher_input);
        (result, publisher_input)
    });

    println!(
        "Lobby is live. Press Enter to start \"{}\"...",
        config.title
    );
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let (mut movie_command, _subtitle_file) = movie_command(&config, &media)?;
    let mut movie = movie_command.spawn()?;
    guard.track(&movie);
    let mut movie_output = movie
        .stdout
        .take()
        .ok_or_else(|| error("Cannot open movie output"))?;
    let mut prefix = vec![0; PREFIX_BYTES];
    if let Err(read_error) = movie_output.read_exact(&mut prefix) {
        eprintln!("Movie encoder stopped before handoff; the lobby will remain live until Ctrl-C.");
        let _ = lobby.wait();
        return Err(Box::new(read_error));
    }

    terminate(&lobby);
    let _ = lobby.wait();
    guard.finished(&lobby);
    let (copy_result, mut publisher_input) = lobby_copy
        .join()
        .map_err(|_| error("Lobby relay thread failed"))?;
    copy_result?;

    println!("Lobby stopped; switching to movie.");
    publisher_input.write_all(&prefix)?;
    set_title(&config.title_token, &config.title)?;
    println!("Movie is live.");
    io::copy(&mut movie_output, &mut publisher_input)?;
    drop(publisher_input);

    checked_wait(&mut movie, "Movie encoder")?;
    guard.finished(&movie);
    checked_wait(&mut publisher, "Publisher")?;
    guard.finished(&publisher);
    Ok(())
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if !(2..=4).contains(&args.len()) {
        eprintln!("Usage: owncast-stream VIDEO [SUBTITLES] [TITLE]");
        std::process::exit(2);
    }
    let result = Config::parse(args.into_iter()).and_then(|config| {
        let media = Media::probe(&config.video)?;
        run(config, media)
    });
    if let Err(failure) = result {
        eprintln!("{failure}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use ffprobe::{Stream, StreamTags};

    fn subtitle(language: &str) -> Stream {
        Stream {
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
            super::select_subtitle(&[subtitle("spa"), subtitle("eng")]),
            Some(1)
        );
    }

    #[test]
    fn falls_back_to_first_embedded_subtitle() {
        assert_eq!(super::select_subtitle(&[subtitle("spa")]), Some(0));
    }

    #[test]
    fn reports_no_embedded_subtitles() {
        assert_eq!(super::select_subtitle(&[]), None);
    }
}
