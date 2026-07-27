use gst::prelude::*;
use gstreamer as gst;
use std::{
    env,
    error::Error,
    io,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::{
    Config, error,
    media::{MediaInfo, Selection, StreamCandidate, StreamKind, SubtitleSource, select_streams},
    set_title,
    ui::PlaybackState,
};

const INTER_CHANNEL: &str = "owncast-movie";
const REQUIRED_ELEMENTS: &[&str] = &[
    "uridecodebin3",
    "videotestsrc",
    "audiotestsrc",
    "textoverlay",
    "subtitleoverlay",
    "subparse",
    "filesrc",
    "queue",
    "input-selector",
    "intervideosink",
    "intervideosrc",
    "interaudiosink",
    "interaudiosrc",
    "appsrc",
    "imagefreeze",
    "videoconvert",
    "aspectratiocrop",
    "videoscale",
    "videorate",
    "audioconvert",
    "audioresample",
    "audiocheblimit",
    "audiodynamic",
    "volume",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "flvmux",
    "rtmpsink",
];

pub(crate) fn required_elements() -> &'static [&'static str] {
    REQUIRED_ELEMENTS
}

fn missing_elements(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .map(|name| (*name).to_owned())
        .collect()
}

fn preflight() -> Result<(), Box<dyn Error>> {
    gst::init()?;
    let missing = missing_elements(required_elements());
    if missing.is_empty() {
        Ok(())
    } else {
        Err(error(format!(
            "Missing GStreamer elements: {}",
            missing.join(", ")
        )))
    }
}

fn bus_error(message: &gst::MessageRef) -> Option<String> {
    match message.view() {
        gst::MessageView::Error(failure) => Some(format!(
            "{}: {} ({})",
            failure
                .src()
                .map(|source| source.path_string())
                .unwrap_or_else(|| "unknown".into()),
            failure.error(),
            failure.debug().unwrap_or_default()
        )),
        _ => None,
    }
}

fn candidate(stream: &gst::Stream) -> Option<StreamCandidate> {
    let kind = if stream.stream_type().contains(gst::StreamType::VIDEO) {
        StreamKind::Video
    } else if stream.stream_type().contains(gst::StreamType::AUDIO) {
        StreamKind::Audio
    } else if stream.stream_type().contains(gst::StreamType::TEXT) {
        StreamKind::Subtitle
    } else {
        return None;
    };
    if kind == StreamKind::Subtitle
        && stream.caps().is_some_and(|caps| {
            caps.structure(0)
                .is_some_and(|structure| structure.name().as_str().starts_with("subpicture/"))
        })
    {
        return None;
    }
    let tags = stream.tags();
    let language = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::LanguageCode>())
        .map(|tag| tag.get().to_ascii_lowercase());
    let title = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::Title>())
        .map(|tag| tag.get().to_ascii_lowercase())
        .unwrap_or_default();
    Some(StreamCandidate {
        id: stream.stream_id()?.to_string(),
        kind,
        language,
        is_default: stream.stream_flags().contains(gst::StreamFlags::SELECT),
        is_sdh: title.contains("sdh") || title.contains("hearing impaired"),
    })
}

struct BroadcastPipeline {
    pipeline: gst::Pipeline,
    video_selector: gst::Element,
    audio_selector: gst::Element,
    freeze_bin: gst::Bin,
    freeze_source: gst::Element,
    latest_frame: Arc<Mutex<Option<gst::Buffer>>>,
    video_lobby_pad: gst::Pad,
    audio_lobby_pad: gst::Pad,
    video_movie_pad: gst::Pad,
    audio_movie_pad: gst::Pad,
    video_freeze_pad: gst::Pad,
}

impl BroadcastPipeline {
    fn build(output_url: &str) -> Result<Self, Box<dyn Error>> {
        let parts = Self::build_with_sink("rtmpsink")?;
        parts
            .pipeline
            .by_name("output")
            .ok_or_else(|| error("Pipeline output is missing"))?
            .set_property("location", output_url);
        Ok(parts)
    }

    fn build_with_sink(sink: &str) -> Result<Self, Box<dyn Error>> {
        preflight()?;
        let pipeline = gst::parse::launch(&format!(
            r#"
            videotestsrc is-live=true pattern=black
              ! video/x-raw,width=1920,height=1080,framerate=30/1
              ! textoverlay text="PLEASE WAIT" font-desc="DejaVu Sans 96"
                  valignment=center halignment=center ypad=80
              ! textoverlay text="The movie will begin shortly"
                  font-desc="DejaVu Sans 42" color=0xb8c1d9ff
                  valignment=center halignment=center ypad=70
              ! queue max-size-buffers=2 leaky=downstream
              ! video_selector.sink_0

            intervideosrc name=movie_video_source channel={INTER_CHANNEL}
                timeout=18446744073709551615
              ! queue max-size-buffers=2 leaky=downstream
              ! video_selector.sink_1

            input-selector name=video_selector sync-streams=true
                sync-mode=clock cache-buffers=true drop-backwards=true
              ! videoconvert
              ! video/x-raw,format=I420,width=1920,height=1080,framerate=30/1
              ! x264enc name=video_encoder bitrate=6000 key-int-max=60 bframes=0
                  tune=zerolatency speed-preset=medium
              ! h264parse name=video_parser config-interval=1
              ! queue name=video_output_queue
              ! mux.

            audiotestsrc is-live=true wave=silence
              ! audio/x-raw,rate=48000,channels=2
              ! queue max-size-buffers=8 leaky=downstream
              ! audio_selector.sink_0

            interaudiosrc name=movie_audio_source channel={INTER_CHANNEL}
              ! queue max-size-buffers=8 leaky=downstream
              ! audio_selector.sink_1

            input-selector name=audio_selector sync-streams=true
                sync-mode=clock cache-buffers=true drop-backwards=true
              ! audioconvert
              ! audioresample
              ! audio/x-raw,format=F32LE,rate=48000,channels=2
              ! audiocheblimit mode=high-pass cutoff=80 poles=4
              ! audiodynamic mode=compressor characteristics=soft-knee
                  threshold=0.125 ratio=2.0
              ! volume name=audio_gain volume=1.4125375
              ! avenc_aac name=audio_encoder bitrate=192000
              ! aacparse name=audio_parser
              ! queue name=audio_output_queue
              ! mux.

            flvmux name=mux streamable=true
              ! {sink} name=output
            "#
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| error("Parsed broadcast graph is not a pipeline"))?;
        let video_selector = pipeline
            .by_name("video_selector")
            .ok_or_else(|| error("Video selector is missing"))?;
        let audio_selector = pipeline
            .by_name("audio_selector")
            .ok_or_else(|| error("Audio selector is missing"))?;
        let video_lobby_pad = video_selector
            .static_pad("sink_0")
            .ok_or_else(|| error("Video lobby pad is missing"))?;
        let audio_lobby_pad = audio_selector
            .static_pad("sink_0")
            .ok_or_else(|| error("Audio lobby pad is missing"))?;
        let video_movie_pad = video_selector
            .static_pad("sink_1")
            .ok_or_else(|| error("Movie video pad is missing"))?;
        let audio_movie_pad = audio_selector
            .static_pad("sink_1")
            .ok_or_else(|| error("Movie audio pad is missing"))?;
        let freeze_bin = gst::parse::bin_from_description_with_name(
            r#"
            appsrc name=freeze_source is-live=true do-timestamp=true format=time
                caps="video/x-raw,format=I420,width=1920,height=1080,framerate=30/1"
              ! imagefreeze is-live=true
              ! queue max-size-buffers=2 leaky=downstream
            "#,
            true,
            "freeze_video",
        )?;
        let freeze_source = freeze_bin
            .by_name("freeze_source")
            .ok_or_else(|| error("Freeze source is missing"))?;
        pipeline.add(&freeze_bin)?;
        let video_freeze_pad = video_selector
            .request_pad_simple("sink_%u")
            .ok_or_else(|| error("Cannot request frozen video pad"))?;
        freeze_bin
            .static_pad("src")
            .ok_or_else(|| error("Freeze output is missing"))?
            .link(&video_freeze_pad)?;
        let latest_frame = Arc::new(Mutex::new(None));
        let captured_frame = latest_frame.clone();
        video_movie_pad
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    *captured_frame.lock().unwrap() = Some(buffer.copy());
                }
                gst::PadProbeReturn::Ok
            })
            .ok_or_else(|| error("Cannot capture movie frames"))?;
        video_selector.set_property("active-pad", Some(&video_lobby_pad));
        audio_selector.set_property("active-pad", Some(&audio_lobby_pad));

        Ok(Self {
            pipeline,
            video_selector,
            audio_selector,
            freeze_bin,
            freeze_source,
            latest_frame,
            video_lobby_pad,
            audio_lobby_pad,
            video_movie_pad,
            audio_movie_pad,
            video_freeze_pad,
        })
    }

    fn select_movie(&self) {
        self.video_selector
            .set_property("active-pad", Some(&self.video_movie_pad));
        self.audio_selector
            .set_property("active-pad", Some(&self.audio_movie_pad));
    }

    fn freeze(&self) -> Result<(), Box<dyn Error>> {
        let mut frame = self
            .latest_frame
            .lock()
            .unwrap()
            .as_ref()
            .map(|buffer| buffer.copy())
            .ok_or_else(|| error("No movie frame is available to freeze"))?;
        {
            let frame = frame
                .get_mut()
                .ok_or_else(|| error("Cannot prepare frozen movie frame"))?;
            frame.set_pts(None);
            frame.set_dts(None);
            frame.set_duration(None);
        }
        self.freeze_bin.set_state(gst::State::Null)?;
        self.freeze_bin.sync_state_with_parent()?;
        if self
            .freeze_source
            .emit_by_name::<gst::FlowReturn>("push-buffer", &[&frame])
            != gst::FlowReturn::Ok
        {
            return Err(error("Cannot start frozen movie frame"));
        }
        self.video_selector
            .set_property("active-pad", Some(&self.video_freeze_pad));
        self.audio_selector
            .set_property("active-pad", Some(&self.audio_lobby_pad));
        Ok(())
    }
}

impl Drop for BroadcastPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[derive(Clone)]
struct SelectedSinks {
    video: gst::Pad,
    audio: gst::Pad,
    subtitle: gst::Pad,
}

struct PendingPad {
    pad: gst::Pad,
    stream_id: Option<String>,
}

#[derive(Default)]
struct SetupState {
    pending: Vec<PendingPad>,
    failure: Option<String>,
}

fn setup_failure(setup: &Arc<Mutex<SetupState>>, reason: impl Into<String>) {
    setup.lock().unwrap().failure = Some(reason.into());
}

fn resolve_pending_pads(
    setup: &Arc<Mutex<SetupState>>,
    selection: &OnceLock<Selection>,
    sinks: &SelectedSinks,
) {
    let Some(selection) = selection.get() else {
        return;
    };
    let pending = std::mem::take(&mut setup.lock().unwrap().pending);
    let mut unresolved = Vec::new();
    for pending in pending {
        let Some(stream_id) = pending.stream_id.as_deref() else {
            unresolved.push(pending);
            continue;
        };
        let sink = if stream_id == selection.video_id {
            Some(&sinks.video)
        } else if stream_id == selection.audio_id {
            Some(&sinks.audio)
        } else if matches!(
            &selection.subtitle,
            SubtitleSource::Embedded(id) if stream_id == id
        ) {
            Some(&sinks.subtitle)
        } else {
            None
        };
        if let Some(sink) = sink
            && pending.pad.peer().as_ref() != Some(sink)
            && let Err(failure) = pending.pad.link(sink)
        {
            setup_failure(
                setup,
                format!("Cannot link selected stream {stream_id}: {failure}"),
            );
        }
    }
    setup.lock().unwrap().pending.extend(unresolved);
}

fn watch_movie_pad(
    pad: &gst::Pad,
    setup: &Arc<Mutex<SetupState>>,
    selection: &Arc<OnceLock<Selection>>,
    sinks: &SelectedSinks,
) {
    setup.lock().unwrap().pending.push(PendingPad {
        pad: pad.clone(),
        stream_id: None,
    });
    let probe_setup = setup.clone();
    let probe_selection = selection.clone();
    let probe_sinks = sinks.clone();
    if pad
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            let Some(gst::EventView::StreamStart(event)) = info.event().map(|event| event.view())
            else {
                return gst::PadProbeReturn::Ok;
            };
            if let Some(pending) = probe_setup
                .lock()
                .unwrap()
                .pending
                .iter_mut()
                .find(|pending| pending.pad == *pad)
            {
                pending.stream_id = Some(event.stream_id().to_owned());
            }
            resolve_pending_pads(&probe_setup, &probe_selection, &probe_sinks);
            gst::PadProbeReturn::Remove
        })
        .is_none()
    {
        setup_failure(setup, format!("Cannot inspect movie pad {}", pad.name()));
    }
}

struct PlaybackPipeline {
    pipeline: gst::Pipeline,
    movie: gst::Element,
    subtitle_overlay: gst::Element,
    sinks: SelectedSinks,
    selection: Arc<OnceLock<Selection>>,
    setup: Arc<Mutex<SetupState>>,
}

impl PlaybackPipeline {
    fn build(config: &Config) -> Result<Self, Box<dyn Error>> {
        preflight()?;
        let pipeline = gst::Pipeline::new();
        let movie = gst::ElementFactory::make("uridecodebin3")
            .name("movie")
            .property("uri", gst::glib::filename_to_uri(&config.video, None)?)
            .build()?;
        let video_bin = gst::parse::bin_from_description_with_name(
            &format!(
                r#"
                queue name=movie_video_input max-size-buffers=2
                  ! videoconvert
                  ! aspectratiocrop aspect-ratio=16/9
                  ! videoscale
                  ! videorate
                  ! video/x-raw,width=1920,height=1080,framerate=30/1
                  ! subtitleoverlay name=movie_subtitles
                  ! videoconvert
                  ! video/x-raw,format=I420,width=1920,height=1080,framerate=30/1
                  ! queue max-size-buffers=2
                  ! intervideosink name=movie_video_output channel={INTER_CHANNEL} sync=true
                "#
            ),
            false,
            "movie_video",
        )?;
        let audio_bin = gst::parse::bin_from_description_with_name(
            &format!(
                "queue name=movie_audio_input max-size-buffers=8 \
                 ! interaudiosink name=movie_audio_output channel={INTER_CHANNEL} sync=true"
            ),
            false,
            "movie_audio",
        )?;
        let subtitle_overlay = video_bin
            .by_name("movie_subtitles")
            .ok_or_else(|| error("Subtitle overlay is missing"))?;
        let video_sink = gst::GhostPad::builder_with_target(
            &video_bin
                .by_name("movie_video_input")
                .ok_or_else(|| error("Movie video input queue is missing"))?
                .static_pad("sink")
                .ok_or_else(|| error("Movie video input is missing"))?,
        )?
        .name("video_sink")
        .build();
        video_bin.add_pad(&video_sink)?;
        let subtitle_sink = gst::GhostPad::builder_with_target(
            &subtitle_overlay
                .static_pad("subtitle_sink")
                .ok_or_else(|| error("Movie subtitle input is missing"))?,
        )?
        .name("subtitle_sink")
        .build();
        video_bin.add_pad(&subtitle_sink)?;
        let audio_sink = gst::GhostPad::builder_with_target(
            &audio_bin
                .by_name("movie_audio_input")
                .ok_or_else(|| error("Movie audio input queue is missing"))?
                .static_pad("sink")
                .ok_or_else(|| error("Movie audio input is missing"))?,
        )?
        .name("audio_sink")
        .build();
        audio_bin.add_pad(&audio_sink)?;
        pipeline.add_many([&movie, video_bin.upcast_ref(), audio_bin.upcast_ref()])?;

        let sinks = SelectedSinks {
            video: video_sink.upcast(),
            audio: audio_sink.upcast(),
            subtitle: subtitle_sink.upcast(),
        };
        let selection = Arc::new(OnceLock::new());
        let setup = Arc::new(Mutex::new(SetupState::default()));
        let pad_setup = setup.clone();
        let pad_selection = selection.clone();
        let pad_sinks = sinks.clone();
        movie.connect_pad_added(move |_, pad| {
            watch_movie_pad(pad, &pad_setup, &pad_selection, &pad_sinks);
        });

        Ok(Self {
            pipeline,
            movie,
            subtitle_overlay,
            sinks,
            selection,
            setup,
        })
    }

    fn add_external_subtitles(&self, path: &std::path::Path) -> Result<(), Box<dyn Error>> {
        let subtitles = gst::parse::bin_from_description_with_name(
            "filesrc name=source ! subparse ! queue",
            true,
            "movie_external_subtitles",
        )?;
        subtitles
            .by_name("source")
            .ok_or_else(|| error("External subtitle source is missing"))?
            .set_property("location", path);
        self.pipeline.add(&subtitles)?;
        subtitles
            .static_pad("src")
            .ok_or_else(|| error("External subtitle output is missing"))?
            .link(&self.sinks.subtitle)?;
        subtitles.sync_state_with_parent()?;
        Ok(())
    }

    fn handle_stream_collection(
        &self,
        message: &gst::MessageRef,
        external: Option<&std::path::Path>,
    ) -> bool {
        let gst::MessageView::StreamCollection(collection) = message.view() else {
            return false;
        };
        if self.selection.get().is_some()
            || !message.src().is_some_and(|source| {
                source == self.movie.upcast_ref::<gst::Object>()
                    || source.has_as_ancestor(&self.movie)
            })
        {
            return false;
        }
        let candidates: Vec<_> = collection
            .stream_collection()
            .iter()
            .filter_map(|stream| candidate(&stream))
            .collect();
        let chosen = match select_streams(&candidates, external) {
            Ok(chosen) => chosen,
            Err(failure) => {
                setup_failure(&self.setup, failure);
                return true;
            }
        };
        if self.selection.set(chosen).is_err() {
            setup_failure(&self.setup, "Movie streams were selected twice");
            return true;
        }
        let chosen = self.selection.get().unwrap();
        let ids = [
            Some(chosen.video_id.as_str()),
            Some(chosen.audio_id.as_str()),
            match &chosen.subtitle {
                SubtitleSource::Embedded(id) => Some(id.as_str()),
                _ => None,
            },
        ]
        .into_iter()
        .flatten();
        if !self.movie.send_event(gst::event::SelectStreams::new(ids)) {
            setup_failure(&self.setup, "Movie rejected selected streams");
            return true;
        }
        resolve_pending_pads(&self.setup, &self.selection, &self.sinks);
        match &chosen.subtitle {
            SubtitleSource::External(path) => {
                if let Err(failure) = self.add_external_subtitles(path) {
                    setup_failure(
                        &self.setup,
                        format!("Cannot prepare external subtitles: {failure}"),
                    );
                }
            }
            SubtitleSource::None => self.subtitle_overlay.set_property("silent", true),
            SubtitleSource::Embedded(_) => {}
        }
        true
    }

    fn ready(&self) -> bool {
        let Some(selection) = self.selection.get() else {
            return false;
        };
        self.sinks.video.is_linked()
            && self.sinks.audio.is_linked()
            && (!matches!(selection.subtitle, SubtitleSource::Embedded(_))
                || self.sinks.subtitle.is_linked())
    }

    fn failure(&self) -> Option<String> {
        self.setup.lock().unwrap().failure.clone()
    }

    fn wait_ready(&self, external: Option<&std::path::Path>) -> Result<(), Box<dyn Error>> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| error("Playback bus is missing"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            while gst::glib::MainContext::default().pending() {
                gst::glib::MainContext::default().iteration(false);
            }
            if let Some(failure) = self.failure() {
                return Err(error(failure));
            }
            if self.ready() {
                return Ok(());
            }
            if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                if self.handle_stream_collection(&message, external) {
                    continue;
                }
                if let Some(failure) = bus_error(&message) {
                    return Err(error(failure));
                }
            }
        }
        Err(error("Movie did not become ready within 10 seconds"))
    }
}

impl Drop for PlaybackPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub(crate) enum SessionEvent {
    Running,
    Finished,
}

pub(crate) struct StreamSession {
    broadcast: BroadcastPipeline,
    playback: PlaybackPipeline,
    state: PlaybackState,
    title: String,
    title_token: String,
    subtitles: Option<std::path::PathBuf>,
    duration: gst::ClockTime,
}

impl StreamSession {
    pub(crate) fn new(config: &Config, media: &MediaInfo) -> Result<Self, Box<dyn Error>> {
        let output_url = env::var("OWNCAST_OUTPUT_URL")
            .unwrap_or_else(|_| format!("rtmp://127.0.0.1/live/{}", config.stream_key));
        let broadcast = BroadcastPipeline::build(&output_url)?;
        let playback = PlaybackPipeline::build(config)?;
        set_title(
            &config.title_token,
            &format!("Starting soon: {}", media.title),
        )?;
        broadcast.pipeline.set_state(gst::State::Playing)?;
        playback.pipeline.set_state(gst::State::Paused)?;
        Ok(Self {
            broadcast,
            playback,
            state: PlaybackState::Lobby,
            title: media.title.clone(),
            title_token: config.title_token.clone(),
            subtitles: config.subtitles.clone(),
            duration: media.duration,
        })
    }

    pub(crate) fn start(&mut self) -> Result<(), Box<dyn Error>> {
        if self.state != PlaybackState::Lobby {
            return Ok(());
        }
        self.playback.wait_ready(self.subtitles.as_deref())?;
        self.broadcast.select_movie();
        self.playback.pipeline.set_state(gst::State::Playing)?;
        set_title(&self.title_token, &self.title)?;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    pub(crate) fn poll(&self) -> Result<SessionEvent, Box<dyn Error>> {
        while gst::glib::MainContext::default().pending() {
            gst::glib::MainContext::default().iteration(false);
        }
        let playback_bus = self
            .playback
            .pipeline
            .bus()
            .ok_or_else(|| error("Playback bus is missing"))?;
        while let Some(message) = playback_bus.timed_pop(gst::ClockTime::ZERO) {
            if self
                .playback
                .handle_stream_collection(&message, self.subtitles.as_deref())
            {
                continue;
            }
            if let Some(failure) = bus_error(&message) {
                return Err(error(failure));
            }
            if matches!(message.view(), gst::MessageView::Eos(_)) {
                return Ok(SessionEvent::Finished);
            }
        }
        let broadcast_bus = self
            .broadcast
            .pipeline
            .bus()
            .ok_or_else(|| error("Broadcast bus is missing"))?;
        while let Some(message) = broadcast_bus.timed_pop(gst::ClockTime::ZERO) {
            if let Some(failure) = bus_error(&message) {
                return Err(error(failure));
            }
        }
        Ok(SessionEvent::Running)
    }
}

pub(crate) fn run(config: &Config, media: &MediaInfo) -> Result<(), Box<dyn Error>> {
    let mut session = StreamSession::new(config, media)?;
    println!("Lobby is live. Press Enter to start \"{}\"...", media.title);
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    session.start()?;
    println!("Movie is live.");
    while matches!(session.poll()?, SessionEvent::Running) {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static GST_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn gst_test() -> std::sync::MutexGuard<'static, ()> {
        let guard = GST_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gst::init().unwrap();
        guard
    }

    fn config() -> Config {
        Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: None,
            title: None,
            stream_key: String::new(),
            title_token: String::new(),
        }
    }

    #[test]
    fn ignores_bitmap_subtitle_streams() {
        let _gst = gst_test();
        let caps = gst::Caps::builder("subpicture/x-pgs").build();
        let stream = gst::Stream::new(
            Some("subtitle-1"),
            Some(&caps),
            gst::StreamType::TEXT,
            gst::StreamFlags::empty(),
        );

        assert_eq!(candidate(&stream), None);
    }

    #[test]
    fn preflight_lists_every_missing_element() {
        let _gst = gst_test();
        assert_eq!(
            missing_elements(&["fakesink", "owncast-element-that-does-not-exist"]),
            vec!["owncast-element-that-does-not-exist"]
        );
    }

    #[test]
    fn broadcast_and_playback_use_separate_pipelines() {
        let _gst = gst_test();
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        let playback = PlaybackPipeline::build(&config()).unwrap();
        broadcast.pipeline.set_state(gst::State::Playing).unwrap();
        broadcast
            .pipeline
            .state(gst::ClockTime::from_seconds(1))
            .0
            .unwrap();

        assert!(broadcast.pipeline.by_name("movie_video_source").is_some());
        assert!(broadcast.pipeline.by_name("movie_audio_source").is_some());
        assert!(broadcast.pipeline.by_name("movie").is_none());
        assert!(playback.pipeline.by_name("movie").is_some());
        assert!(playback.pipeline.by_name("movie_video_output").is_some());
        assert!(playback.pipeline.by_name("movie_audio_output").is_some());
        assert!(playback.pipeline.by_name("output").is_none());
        assert_eq!(
            broadcast.video_selector.property::<gst::Pad>("active-pad"),
            broadcast.video_lobby_pad
        );
        assert_eq!(
            broadcast.audio_selector.property::<gst::Pad>("active-pad"),
            broadcast.audio_lobby_pad
        );
        let gain = broadcast.pipeline.by_name("audio_gain").unwrap();
        assert!((gain.property::<f64>("volume") - 1.4125375).abs() < 0.000001);
    }

    #[test]
    fn paused_playback_keeps_broadcast_video_live_and_selects_silence() {
        let _gst = gst_test();
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        let playback = gst::parse::launch(&format!(
            r#"
            videotestsrc is-live=true pattern=white
              ! videoconvert
              ! video/x-raw,format=I420,width=1920,height=1080,framerate=30/1
              ! intervideosink channel={INTER_CHANNEL} sync=true
            audiotestsrc is-live=true wave=sine
              ! interaudiosink channel={INTER_CHANNEL} sync=true
            "#
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let frames = Arc::new(AtomicU64::new(0));
        let counted = frames.clone();
        broadcast
            .video_selector
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counted.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        broadcast.pipeline.set_state(gst::State::Playing).unwrap();
        playback.set_state(gst::State::Playing).unwrap();
        broadcast.select_movie();
        std::thread::sleep(Duration::from_millis(300));
        broadcast.freeze().unwrap();
        playback.set_state(gst::State::Paused).unwrap();
        let before = frames.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(250));
        let after = frames.load(Ordering::Relaxed);

        assert!(after > before, "video stopped while playback was paused");
        assert_eq!(
            broadcast.audio_selector.property::<gst::Pad>("active-pad"),
            broadcast.audio_lobby_pad
        );
        playback.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn imagefreeze_repeats_one_appsrc_frame() {
        let _gst = gst_test();
        let pipeline = gst::parse::launch(
            r#"
            appsrc name=source is-live=true format=time
                caps="video/x-raw,format=I420,width=16,height=16,framerate=30/1"
              ! imagefreeze is-live=true
              ! fakesink name=sink sync=true
            "#,
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let frames = Arc::new(AtomicU64::new(0));
        let counted = frames.clone();
        pipeline
            .by_name("sink")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counted.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut frame = gst::Buffer::with_size(16 * 16 * 3 / 2).unwrap();
        frame.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        assert_eq!(
            pipeline
                .by_name("source")
                .unwrap()
                .emit_by_name::<gst::FlowReturn>("push-buffer", &[&frame]),
            gst::FlowReturn::Ok
        );

        std::thread::sleep(Duration::from_millis(250));

        assert!(frames.load(Ordering::Relaxed) >= 3);
        pipeline.set_state(gst::State::Null).unwrap();
    }
}
