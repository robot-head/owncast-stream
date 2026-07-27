use gst::prelude::*;
use gstreamer as gst;
use std::{
    env,
    error::Error,
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
const DEFAULT_GAIN_DB: f64 = 3.0;
const MIN_GAIN_DB: f64 = -12.0;
const MAX_GAIN_DB: f64 = 12.0;

fn db_to_amplitude(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

fn adjusted_gain_db(current: f64, steps: i8) -> f64 {
    (current + f64::from(steps)).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AudioLevels {
    pub(crate) peak: [f64; 2],
    pub(crate) decay: [f64; 2],
}

impl Default for AudioLevels {
    fn default() -> Self {
        Self {
            peak: [-60.0; 2],
            decay: [-60.0; 2],
        }
    }
}

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
    "level",
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

fn stereo_values(structure: &gst::StructureRef, field: &str) -> Option<[f64; 2]> {
    let values = structure.get::<gst::glib::ValueArray>(field).ok()?;
    let values = [
        values.first()?.get::<f64>().ok()?,
        values.get(1)?.get::<f64>().ok()?,
    ];
    values
        .iter()
        .all(|value| value.is_finite() || *value == f64::NEG_INFINITY)
        .then(|| values.map(|value| value.clamp(-60.0, 0.0)))
}

fn parse_audio_levels(message: &gst::MessageRef) -> Option<AudioLevels> {
    let structure = message.structure()?;
    if structure.name() != "level" {
        return None;
    }
    Some(AudioLevels {
        peak: stereo_values(structure, "peak")?,
        decay: stereo_values(structure, "decay")?,
    })
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
    audio_gain: gst::Element,
    freeze_bin: gst::Bin,
    freeze_source: gst::Element,
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
              ! volume name=audio_gain
              ! level name=audio_meter interval=100000000
                  peak-ttl=3000000000 peak-falloff=12 post-messages=true
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
        let audio_gain = pipeline
            .by_name("audio_gain")
            .ok_or_else(|| error("Audio gain is missing"))?;
        audio_gain.set_property("volume", db_to_amplitude(DEFAULT_GAIN_DB));
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
        video_selector.set_property("active-pad", Some(&video_lobby_pad));
        audio_selector.set_property("active-pad", Some(&audio_lobby_pad));

        Ok(Self {
            pipeline,
            video_selector,
            audio_selector,
            audio_gain,
            freeze_bin,
            freeze_source,
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

    fn freeze(&self, mut frame: gst::Buffer) -> Result<(), Box<dyn Error>> {
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
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
}

struct CapturedFrame {
    generation: u64,
    buffer: gst::Buffer,
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
        let latest_frame = Self::capture_output(
            &video_bin
                .by_name("movie_video_output")
                .ok_or_else(|| error("Movie video output is missing"))?,
        )?;
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
            latest_frame,
        })
    }

    fn capture_output(
        movie_video_output: &gst::Element,
    ) -> Result<Arc<Mutex<Option<CapturedFrame>>>, Box<dyn Error>> {
        let latest_frame = Arc::new(Mutex::new(None::<CapturedFrame>));
        let captured_frame = latest_frame.clone();
        movie_video_output
            .static_pad("sink")
            .ok_or_else(|| error("Movie video output is missing its sink pad"))?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    let mut latest = captured_frame.lock().unwrap();
                    *latest = Some(CapturedFrame {
                        generation: latest
                            .as_ref()
                            .map_or(1, |frame| frame.generation.saturating_add(1)),
                        buffer: buffer.copy(),
                    });
                }
                gst::PadProbeReturn::Ok
            })
            .ok_or_else(|| error("Cannot capture movie frames"))?;
        Ok(latest_frame)
    }

    fn frame_generation(&self) -> u64 {
        self.latest_frame
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |frame| frame.generation)
    }

    fn latest_frame(&self) -> Result<gst::Buffer, Box<dyn Error>> {
        self.latest_frame
            .lock()
            .unwrap()
            .as_ref()
            .map(|frame| frame.buffer.copy())
            .ok_or_else(|| error("No movie frame is available to freeze"))
    }

    fn check_bus_errors(bus: &gst::Bus) -> Result<(), Box<dyn Error>> {
        while let Some(message) = bus.timed_pop(gst::ClockTime::ZERO) {
            if let Some(failure) = bus_error(&message) {
                return Err(error(failure));
            }
        }
        Ok(())
    }

    fn wait_for_frame_after(
        &self,
        generation: u64,
        timeout: Duration,
    ) -> Result<gst::Buffer, Box<dyn Error>> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| error("Playback bus is missing"))?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            while gst::glib::MainContext::default().pending() {
                gst::glib::MainContext::default().iteration(false);
            }
            Self::check_bus_errors(&bus)?;
            if let Some(frame) = self
                .latest_frame
                .lock()
                .unwrap()
                .as_ref()
                .filter(|frame| frame.generation > generation)
            {
                return Ok(frame.buffer.copy());
            }
            if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(10))
                && let Some(failure) = bus_error(&message)
            {
                return Err(error(failure));
            }
        }
        Err(error("Movie frame did not become available"))
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
    gain_db: f64,
    levels: AudioLevels,
    title: String,
    title_token: String,
    subtitles: Option<std::path::PathBuf>,
    duration: gst::ClockTime,
}

fn seek_target(
    position: gst::ClockTime,
    duration: gst::ClockTime,
    delta_seconds: i64,
) -> gst::ClockTime {
    if delta_seconds >= 0 {
        position
            .saturating_add(gst::ClockTime::from_seconds(delta_seconds as u64))
            .min(duration)
    } else {
        position.saturating_sub(gst::ClockTime::from_seconds(delta_seconds.unsigned_abs()))
    }
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
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
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
        self.start_transition(Duration::from_secs(1))?;
        set_title(&self.title_token, &self.title)?;
        Ok(())
    }

    fn start_transition(&mut self, timeout: Duration) -> Result<(), Box<dyn Error>> {
        self.playback.pipeline.set_state(gst::State::Playing)?;
        self.playback.wait_for_frame_after(0, timeout)?;
        self.broadcast.select_movie();
        self.state = PlaybackState::Playing;
        Ok(())
    }

    pub(crate) fn state(&self) -> PlaybackState {
        self.state
    }

    pub(crate) fn gain_db(&self) -> f64 {
        self.gain_db
    }

    pub(crate) fn levels(&self) -> AudioLevels {
        self.levels
    }

    pub(crate) fn adjust_gain(&mut self, steps: i8) {
        self.gain_db = adjusted_gain_db(self.gain_db, steps);
        self.broadcast
            .audio_gain
            .set_property("volume", db_to_amplitude(self.gain_db));
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn duration(&self) -> gst::ClockTime {
        self.duration
    }

    pub(crate) fn position(&self) -> gst::ClockTime {
        self.playback
            .pipeline
            .query_position()
            .unwrap_or(gst::ClockTime::ZERO)
    }

    pub(crate) fn toggle_pause(&mut self) -> Result<(), Box<dyn Error>> {
        match self.state {
            PlaybackState::Playing => {
                self.broadcast.freeze(self.playback.latest_frame()?)?;
                self.playback.pipeline.set_state(gst::State::Paused)?;
                self.state = PlaybackState::Paused;
            }
            PlaybackState::Paused => {
                self.playback.pipeline.set_state(gst::State::Playing)?;
                self.broadcast.select_movie();
                self.state = PlaybackState::Playing;
            }
            PlaybackState::Lobby => {}
        }
        Ok(())
    }

    pub(crate) fn seek_by(&mut self, seconds: i64) -> Result<(), Box<dyn Error>> {
        if self.state == PlaybackState::Lobby {
            return Ok(());
        }
        let target = seek_target(self.position(), self.duration, seconds);
        let keep_last_frame = self.state == PlaybackState::Paused && target == self.duration;
        let generation = self.playback.frame_generation();
        self.playback
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target)?;
        if self.state == PlaybackState::Paused {
            let state_change = self
                .playback
                .pipeline
                .state(gst::ClockTime::from_seconds(5))
                .0;
            PlaybackPipeline::check_bus_errors(
                &self
                    .playback
                    .pipeline
                    .bus()
                    .ok_or_else(|| error("Playback bus is missing"))?,
            )?;
            if state_change? == gst::StateChangeSuccess::Async {
                return Err(error("Playback pipeline did not settle within 5 seconds"));
            }
            if keep_last_frame {
                return Ok(());
            }
            let frame = self
                .playback
                .wait_for_frame_after(generation, Duration::from_secs(1))?;
            self.broadcast.freeze(frame)?;
        }
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> Result<SessionEvent, Box<dyn Error>> {
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
            if let Some(levels) = parse_audio_levels(&message) {
                self.levels = levels;
                continue;
            }
            if let Some(failure) = bus_error(&message) {
                return Err(error(failure));
            }
        }
        Ok(SessionEvent::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
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

    fn level_message(peak: [f64; 2], decay: [f64; 2]) -> gst::Message {
        let mut structure = gst::Structure::new_empty("level");
        // SAFETY: Both arrays contain only `f64`, which is `Send`.
        unsafe {
            structure.set_value(
                "peak",
                gst::glib::ValueArray::new(peak)
                    .to_value()
                    .into_send_value(),
            );
            structure.set_value(
                "decay",
                gst::glib::ValueArray::new(decay)
                    .to_value()
                    .into_send_value(),
            );
        }
        gst::message::Element::new(structure)
    }

    fn session_with_fakesink() -> StreamSession {
        let pipeline = gst::Pipeline::new();
        let movie = gst::ElementFactory::make("fakesrc").build().unwrap();
        let subtitle_overlay = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add_many([&movie, &subtitle_overlay]).unwrap();
        StreamSession {
            broadcast: BroadcastPipeline::build_with_sink("fakesink").unwrap(),
            playback: PlaybackPipeline {
                pipeline,
                movie,
                subtitle_overlay,
                sinks: SelectedSinks {
                    video: gst::Pad::new(gst::PadDirection::Sink),
                    audio: gst::Pad::new(gst::PadDirection::Sink),
                    subtitle: gst::Pad::new(gst::PadDirection::Sink),
                },
                selection: Arc::new(OnceLock::new()),
                setup: Arc::new(Mutex::new(SetupState::default())),
                latest_frame: Arc::new(Mutex::new(None)),
            },
            state: PlaybackState::Lobby,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title: "Passenger".into(),
            title_token: String::new(),
            subtitles: None,
            duration: gst::ClockTime::from_seconds(100),
        }
    }

    fn playback_capture_with_appsrc() -> (PlaybackPipeline, gst::Element) {
        let pipeline = gst::parse::launch(&format!(
            r#"
            appsrc name=source is-live=true format=time
                caps="video/x-raw,format=I420,width=16,height=16,framerate=30/1"
              ! intervideosink name=movie_video_output channel={INTER_CHANNEL} sync=false
            "#
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let source = pipeline.by_name("source").unwrap();
        let movie = gst::ElementFactory::make("fakesrc").build().unwrap();
        let subtitle_overlay = gst::ElementFactory::make("identity").build().unwrap();
        let latest_frame =
            PlaybackPipeline::capture_output(&pipeline.by_name("movie_video_output").unwrap())
                .unwrap();
        (
            PlaybackPipeline {
                pipeline,
                movie,
                subtitle_overlay,
                sinks: SelectedSinks {
                    video: gst::Pad::new(gst::PadDirection::Sink),
                    audio: gst::Pad::new(gst::PadDirection::Sink),
                    subtitle: gst::Pad::new(gst::PadDirection::Sink),
                },
                selection: Arc::new(OnceLock::new()),
                setup: Arc::new(Mutex::new(SetupState::default())),
                latest_frame,
            },
            source,
        )
    }

    fn push_test_frame(source: &gst::Element, pts: gst::ClockTime) {
        let mut frame = gst::Buffer::with_size(16 * 16 * 3 / 2).unwrap();
        frame.get_mut().unwrap().set_pts(pts);
        assert_eq!(
            source.emit_by_name::<gst::FlowReturn>("push-buffer", &[&frame]),
            gst::FlowReturn::Ok
        );
    }

    fn session_with_test_video(delay_microseconds: u64) -> StreamSession {
        let pipeline = gst::parse::launch(&format!(
            r#"
            videotestsrc num-buffers=900 pattern=ball
              ! identity sleep-time={delay_microseconds}
              ! videoconvert
              ! video/x-raw,format=I420,width=16,height=16,framerate=30/1
              ! identity name=movie_video_output
              ! fakesink sync=true
            "#
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let latest_frame =
            PlaybackPipeline::capture_output(&pipeline.by_name("movie_video_output").unwrap())
                .unwrap();
        let movie = gst::ElementFactory::make("fakesrc").build().unwrap();
        let subtitle_overlay = gst::ElementFactory::make("identity").build().unwrap();
        StreamSession {
            broadcast: BroadcastPipeline::build_with_sink("fakesink").unwrap(),
            playback: PlaybackPipeline {
                pipeline,
                movie,
                subtitle_overlay,
                sinks: SelectedSinks {
                    video: gst::Pad::new(gst::PadDirection::Sink),
                    audio: gst::Pad::new(gst::PadDirection::Sink),
                    subtitle: gst::Pad::new(gst::PadDirection::Sink),
                },
                selection: Arc::new(OnceLock::new()),
                setup: Arc::new(Mutex::new(SetupState::default())),
                latest_frame,
            },
            state: PlaybackState::Lobby,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title: "Passenger".into(),
            title_token: String::new(),
            subtitles: None,
            duration: gst::ClockTime::from_seconds(30),
        }
    }

    #[test]
    fn playback_capture_advances_only_for_source_frames() {
        let _gst = gst_test();
        let (playback, source) = playback_capture_with_appsrc();
        playback.pipeline.set_state(gst::State::Playing).unwrap();
        push_test_frame(&source, gst::ClockTime::from_seconds(1));
        playback
            .wait_for_frame_after(0, Duration::from_secs(1))
            .unwrap();
        let generation = playback.frame_generation();
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        broadcast.pipeline.set_state(gst::State::Playing).unwrap();
        broadcast.select_movie();

        std::thread::sleep(Duration::from_millis(150));

        assert_eq!(playback.frame_generation(), generation);
    }

    #[test]
    fn wait_for_frame_after_returns_the_new_source_buffer() {
        let _gst = gst_test();
        let (playback, source) = playback_capture_with_appsrc();
        playback.pipeline.set_state(gst::State::Playing).unwrap();
        push_test_frame(&source, gst::ClockTime::from_seconds(1));
        playback
            .wait_for_frame_after(0, Duration::from_secs(1))
            .unwrap();
        let generation = playback.frame_generation();

        push_test_frame(&source, gst::ClockTime::from_seconds(2));
        let frame = playback
            .wait_for_frame_after(generation, Duration::from_secs(1))
            .unwrap();

        assert_eq!(playback.frame_generation(), generation + 1);
        assert_eq!(frame.pts(), Some(gst::ClockTime::from_seconds(2)));
    }

    #[test]
    fn wait_for_frame_after_propagates_playback_error_before_a_frame() {
        let _gst = gst_test();
        let (playback, source) = playback_capture_with_appsrc();
        playback.pipeline.set_state(gst::State::Playing).unwrap();
        playback
            .pipeline
            .bus()
            .unwrap()
            .post(gst::message::Error::builder(gst::CoreError::Failed, "synthetic failure").build())
            .unwrap();
        push_test_frame(&source, gst::ClockTime::from_seconds(1));

        let failure = playback
            .wait_for_frame_after(0, Duration::from_secs(1))
            .unwrap_err();

        assert!(failure.to_string().contains("synthetic failure"));
    }

    #[test]
    fn start_waits_for_first_movie_frame_before_playing() {
        let _gst = gst_test();
        let mut session = session_with_test_video(150_000);
        session
            .broadcast
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        let started = Instant::now();

        session.start_transition(Duration::from_secs(1)).unwrap();

        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(session.state(), PlaybackState::Playing);
        session.toggle_pause().unwrap();
        assert_eq!(session.state(), PlaybackState::Paused);
    }

    #[test]
    fn paused_seek_freezes_a_post_seek_source_frame() {
        let _gst = gst_test();
        let mut session = session_with_test_video(0);
        session
            .broadcast
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session.start_transition(Duration::from_secs(1)).unwrap();
        let frozen = Arc::new(Mutex::new(None));
        let captured = frozen.clone();
        session
            .broadcast
            .freeze_source
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer()
                    && let Ok(map) = buffer.map_readable()
                {
                    *captured.lock().unwrap() = Some(map.as_slice().to_vec());
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        session.toggle_pause().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while frozen.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(frozen.lock().unwrap().take().is_some());
        let generation = session.playback.frame_generation();

        session.seek_by(3).unwrap();

        assert!(session.playback.frame_generation() > generation);
        let current = session.playback.latest_frame().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while frozen.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let frozen = frozen.lock().unwrap().clone();
        assert_eq!(
            frozen.as_deref(),
            Some(current.map_readable().unwrap().as_slice())
        );
        assert_eq!(session.state(), PlaybackState::Paused);
    }

    #[test]
    fn paused_seek_to_duration_keeps_last_frame() {
        let _gst = gst_test();
        let mut session = session_with_test_video(0);
        session
            .broadcast
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session.start_transition(Duration::from_secs(1)).unwrap();
        let freezes = Arc::new(AtomicU64::new(0));
        let counted_freezes = freezes.clone();
        session
            .broadcast
            .freeze_source
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counted_freezes.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        session.toggle_pause().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while freezes.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let freeze_count = freezes.load(Ordering::Relaxed);
        assert!(freeze_count > 0);
        let selected = session
            .broadcast
            .video_selector
            .property::<gst::Pad>("active-pad");
        let sought = Arc::new(AtomicBool::new(false));
        let observed_seek = sought.clone();
        session
            .playback
            .pipeline
            .by_name("movie_video_output")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                if matches!(
                    info.event().map(|event| event.view()),
                    Some(gst::EventView::Seek(_))
                ) {
                    observed_seek.store(true, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        session.seek_by(30).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        assert!(sought.load(Ordering::Relaxed));
        assert_eq!(freezes.load(Ordering::Relaxed), freeze_count);
        assert_eq!(session.state(), PlaybackState::Paused);
        assert_eq!(
            session
                .broadcast
                .video_selector
                .property::<gst::Pad>("active-pad"),
            selected
        );
    }

    #[test]
    fn paused_seek_to_duration_propagates_playback_error() {
        let _gst = gst_test();
        let mut session = session_with_test_video(0);
        session
            .broadcast
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session.start_transition(Duration::from_secs(1)).unwrap();
        let freezes = Arc::new(AtomicU64::new(0));
        let counted_freezes = freezes.clone();
        session
            .broadcast
            .freeze_source
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counted_freezes.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        session.toggle_pause().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while freezes.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let freeze_count = freezes.load(Ordering::Relaxed);
        assert!(freeze_count > 0);
        let bus = session.playback.pipeline.bus().unwrap();
        session
            .playback
            .pipeline
            .by_name("movie_video_output")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                if matches!(
                    info.event().map(|event| event.view()),
                    Some(gst::EventView::Seek(_))
                ) {
                    bus.post(
                        gst::message::Error::builder(
                            gst::CoreError::Failed,
                            "synthetic seek failure",
                        )
                        .build(),
                    )
                    .unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        let failure = session.seek_by(30).unwrap_err();
        std::thread::sleep(Duration::from_millis(100));

        assert!(failure.to_string().contains("synthetic seek failure"));
        assert_eq!(freezes.load(Ordering::Relaxed), freeze_count);
        assert_eq!(session.state(), PlaybackState::Paused);
    }

    #[test]
    fn converts_db_to_amplitude() {
        assert!((db_to_amplitude(0.0) - 1.0).abs() < 0.000001);
        assert!((db_to_amplitude(3.0) - 1.4125375).abs() < 0.000001);
    }

    #[test]
    fn gain_steps_one_db() {
        assert_eq!(adjusted_gain_db(3.0, 1), 4.0);
        assert_eq!(adjusted_gain_db(3.0, -1), 2.0);
    }

    #[test]
    fn gain_clamps_to_bounds() {
        assert_eq!(adjusted_gain_db(12.0, 1), 12.0);
        assert_eq!(adjusted_gain_db(-12.0, -1), -12.0);
    }

    #[test]
    fn session_gain_updates_volume_and_clamps_repeated_commands() {
        let _gst = gst_test();
        let mut session = session_with_fakesink();

        session.adjust_gain(1);
        assert_eq!(session.gain_db(), 4.0);
        assert!(
            (session.broadcast.audio_gain.property::<f64>("volume") - 1.584893192).abs() < 0.000001
        );

        for _ in 0..30 {
            session.adjust_gain(1);
        }
        assert_eq!(session.gain_db(), 12.0);
        assert!(
            (session.broadcast.audio_gain.property::<f64>("volume") - 3.981071706).abs() < 0.000001
        );

        for _ in 0..30 {
            session.adjust_gain(-1);
        }
        assert_eq!(session.gain_db(), -12.0);
        assert!(
            (session.broadcast.audio_gain.property::<f64>("volume") - 0.251188643).abs() < 0.000001
        );
    }

    #[test]
    fn parses_stereo_peak_and_decay_from_level_message() {
        let _gst = gst_test();
        let message = level_message([-4.2, -6.1], [-2.8, -3.4]);

        assert_eq!(
            parse_audio_levels(&message),
            Some(AudioLevels {
                peak: [-4.2, -6.1],
                decay: [-2.8, -3.4],
            })
        );
    }

    #[test]
    fn clamps_silent_audio_levels_to_floor() {
        let _gst = gst_test();
        let message = level_message([-90.0, -60.0], [-120.0, -61.0]);

        assert_eq!(
            parse_audio_levels(&message),
            Some(AudioLevels {
                peak: [-60.0, -60.0],
                decay: [-60.0, -60.0],
            })
        );
    }

    #[test]
    fn maps_negative_infinity_silence_to_floor() {
        let _gst = gst_test();
        let message = level_message([f64::NEG_INFINITY, -4.0], [-3.0, f64::NEG_INFINITY]);

        assert_eq!(
            parse_audio_levels(&message),
            Some(AudioLevels {
                peak: [-60.0, -4.0],
                decay: [-3.0, -60.0],
            })
        );
    }

    #[test]
    fn clamps_audio_levels_above_zero() {
        let _gst = gst_test();
        let message = level_message([1.0, 12.0], [0.1, 3.0]);

        assert_eq!(
            parse_audio_levels(&message),
            Some(AudioLevels {
                peak: [0.0, 0.0],
                decay: [0.0, 0.0],
            })
        );
    }

    #[test]
    fn rejects_non_finite_audio_levels() {
        let _gst = gst_test();
        for value in [f64::NAN, f64::INFINITY] {
            let message = level_message([value, -4.0], [-3.0, -5.0]);

            assert_eq!(parse_audio_levels(&message), None);
        }
    }

    #[test]
    fn ignores_malformed_level_message() {
        let _gst = gst_test();
        let message = gst::message::Element::new(gst::Structure::new_empty("level"));

        assert_eq!(parse_audio_levels(&message), None);
    }

    #[test]
    fn poll_preserves_levels_after_invalid_message() {
        let _gst = gst_test();
        let mut session = session_with_fakesink();
        let previous = AudioLevels {
            peak: [-4.0, -5.0],
            decay: [-2.0, -3.0],
        };
        session.levels = previous;
        session
            .broadcast
            .pipeline
            .bus()
            .unwrap()
            .post(level_message([f64::NAN, -4.0], [-3.0, -5.0]))
            .unwrap();

        session.poll().unwrap();

        assert_eq!(session.levels(), previous);
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
            broadcast
                .video_selector
                .property::<gst::Pad>("active-pad")
                .name(),
            "sink_0"
        );
        assert_eq!(
            broadcast.audio_selector.property::<gst::Pad>("active-pad"),
            broadcast.audio_lobby_pad
        );
        let meter = broadcast.pipeline.by_name("audio_meter").unwrap();
        assert_eq!(meter.property::<u64>("interval"), 100_000_000);
        assert_eq!(meter.property::<u64>("peak-ttl"), 3_000_000_000);
        assert_eq!(meter.property::<f64>("peak-falloff"), 12.0);
        assert!(meter.property::<bool>("post-messages"));
        assert!(
            (broadcast.audio_gain.property::<f64>("volume") - db_to_amplitude(3.0)).abs()
                < 0.000001
        );
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
        broadcast
            .freeze(gst::Buffer::with_size(1920 * 1080 * 3 / 2).unwrap())
            .unwrap();
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

    #[test]
    fn seek_target_clamps_to_media_bounds() {
        let position = gst::ClockTime::from_seconds(40);
        let duration = gst::ClockTime::from_seconds(100);

        assert_eq!(
            seek_target(position, duration, -30),
            gst::ClockTime::from_seconds(10)
        );
        assert_eq!(seek_target(position, duration, -90), gst::ClockTime::ZERO);
        assert_eq!(
            seek_target(position, duration, 30),
            gst::ClockTime::from_seconds(70)
        );
        assert_eq!(seek_target(position, duration, 90), duration);
    }

    #[test]
    fn session_pause_and_resume_switch_broadcast_sources() {
        let _gst = gst_test();
        let mut session = session_with_test_video(0);
        session
            .broadcast
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session.start_transition(Duration::from_secs(1)).unwrap();

        session.toggle_pause().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(session.state(), PlaybackState::Paused);
        assert_eq!(
            session
                .broadcast
                .video_selector
                .property::<gst::Pad>("active-pad"),
            session.broadcast.video_freeze_pad
        );
        assert_eq!(
            session
                .broadcast
                .audio_selector
                .property::<gst::Pad>("active-pad"),
            session.broadcast.audio_lobby_pad
        );

        session.toggle_pause().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(session.state(), PlaybackState::Playing);
        assert_eq!(
            session
                .broadcast
                .video_selector
                .property::<gst::Pad>("active-pad"),
            session.broadcast.video_movie_pad
        );
        assert_eq!(
            session
                .broadcast
                .audio_selector
                .property::<gst::Pad>("active-pad"),
            session.broadcast.audio_movie_pad
        );
    }

    #[test]
    fn session_seek_uses_clamped_time_and_preserves_state() {
        let _gst = gst_test();
        let playback_pipeline = gst::parse::launch(
            "videotestsrc num-buffers=900 ! video/x-raw,framerate=30/1 ! fakesink sync=true",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let movie = gst::ElementFactory::make("fakesrc").build().unwrap();
        let subtitle_overlay = gst::ElementFactory::make("identity").build().unwrap();
        let playback = PlaybackPipeline {
            pipeline: playback_pipeline,
            movie,
            subtitle_overlay,
            sinks: SelectedSinks {
                video: gst::Pad::new(gst::PadDirection::Sink),
                audio: gst::Pad::new(gst::PadDirection::Sink),
                subtitle: gst::Pad::new(gst::PadDirection::Sink),
            },
            selection: Arc::new(OnceLock::new()),
            setup: Arc::new(Mutex::new(SetupState::default())),
            latest_frame: Arc::new(Mutex::new(None)),
        };
        let mut session = StreamSession {
            broadcast: BroadcastPipeline::build_with_sink("fakesink").unwrap(),
            playback,
            state: PlaybackState::Playing,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title: "Passenger".into(),
            title_token: String::new(),
            subtitles: None,
            duration: gst::ClockTime::from_seconds(30),
        };
        session
            .playback
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session
            .playback
            .pipeline
            .state(gst::ClockTime::from_seconds(1))
            .0
            .unwrap();

        session.seek_by(3).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let position = session.position();
        assert!(
            position >= gst::ClockTime::from_mseconds(2_900),
            "position was {position}"
        );
        assert!(position <= gst::ClockTime::from_mseconds(3_500));
        assert_eq!(session.state(), PlaybackState::Playing);
    }
}
