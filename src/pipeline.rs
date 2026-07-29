use gst::prelude::*;
use gstreamer as gst;
use std::{
    collections::VecDeque,
    error::Error,
    sync::{Arc, Mutex, OnceLock},
    thread::{self, JoinHandle},
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

const AUDIO_RATE: u64 = 48_000;
const AUDIO_BYTES_PER_FRAME: u64 = 8; // F32LE, two channels
const AUDIO_CAPS: &str = "audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved";

/// One tick of the bridge. Small enough that a starved tick is a short gap,
/// large enough that the reader thread is not woken excessively.
const BRIDGE_PERIOD: Duration = Duration::from_millis(20);
/// How much decoded audio the bridge holds back before it starts emitting. The
/// playback and broadcast pipelines run on independent schedules, so the reader
/// needs slack to absorb jitter instead of fabricating silence on every tick.
const BRIDGE_TARGET_FILL: Duration = Duration::from_millis(200);
/// Upper bound on buffered audio. Reaching it stops draining the sink, which
/// back-pressures the decoder rather than growing latency without limit.
const BRIDGE_MAX_FILL: Duration = Duration::from_millis(500);
/// How far the backlog may exceed [`BRIDGE_TARGET_FILL`] before the bridge
/// drops the oldest audio. Every inserted silence delays everything behind it,
/// so without this the media timeline slips further from video on each gap.
/// Well above a single period, so ordinary jitter never trips it.
const BRIDGE_MAX_DRIFT: Duration = Duration::from_millis(100);
/// A tick this far behind the clock means the thread lost the CPU for long
/// enough that catching up sample-by-sample would be worse than resyncing.
const BRIDGE_RESYNC_SLIP: Duration = Duration::from_secs(1);

fn audio_bytes(span: Duration) -> usize {
    let frames = span.as_nanos() as u64 * AUDIO_RATE / 1_000_000_000;
    (frames * AUDIO_BYTES_PER_FRAME) as usize
}

#[derive(Default)]
struct BridgeState {
    buffered: VecDeque<u8>,
    /// Withhold decoded audio until [`BRIDGE_TARGET_FILL`] is buffered.
    priming: bool,
    /// Playback is paused. The broadcast is on the lobby pad, so anything the
    /// bridge emits is discarded; consuming the backlog would silently skip
    /// that audio and leave the movie out of sync once playback resumes.
    paused: bool,
    flush: bool,
    stopped: bool,
}

struct BridgeLimits {
    period_bytes: usize,
    target_bytes: usize,
    max_bytes: usize,
    drift_bytes: usize,
}

impl BridgeState {
    /// Apply a pending flush, dropping audio a seek has made stale.
    fn take_flush(&mut self) {
        if std::mem::take(&mut self.flush) {
            self.buffered.clear();
            self.priming = true;
        }
    }
}

/// Decide what the next tick emits. While priming, the bridge holds back audio
/// until `target_bytes` is buffered; once running, a tick that cannot be filled
/// emits silence and returns to priming, so one hiccup costs a single gap
/// rather than a run of them.
fn next_payload(state: &mut BridgeState, limits: &BridgeLimits) -> Vec<u8> {
    let silence = || vec![0; limits.period_bytes];
    // Paused playback broadcasts lobby silence, so hold the backlog untouched
    // for the resume rather than draining it into a discarded pad.
    if state.paused {
        return silence();
    }
    if state.priming {
        if state.buffered.len() < limits.target_bytes {
            return silence();
        }
        state.priming = false;
    }
    if state.buffered.len() < limits.period_bytes {
        return silence();
    }
    // Drop audio that inserted silence has pushed too far behind the media
    // timeline instead of carrying the delay forward for the rest of the movie.
    if state.buffered.len() > limits.target_bytes + limits.drift_bytes {
        let excess = state.buffered.len() - limits.target_bytes;
        state.buffered.drain(..excess);
    }
    state.buffered.drain(..limits.period_bytes).collect()
}

/// Carries decoded movie audio from the playback pipeline's `appsink` to the
/// broadcast pipeline's `appsrc`, pacing it against the broadcast clock.
///
/// The two pipelines are independent, so this replaces the `interaudiosink` /
/// `interaudiosrc` pair, whose fixed read period underran constantly and padded
/// each shortfall with silence.
struct AudioBridge {
    state: Arc<Mutex<BridgeState>>,
    reader: Option<JoinHandle<()>>,
}

impl AudioBridge {
    fn start(
        sink: gst::Element,
        source: gst::Element,
        broadcast: gst::Pipeline,
    ) -> Result<Self, Box<dyn Error>> {
        let state = Arc::new(Mutex::new(BridgeState {
            priming: true,
            ..BridgeState::default()
        }));
        let reader_state = state.clone();
        // Without this thread the movie audio pad has no producer at all, so a
        // failed spawn has to fail the session rather than stream silence.
        let reader = thread::Builder::new()
            .name("audio-bridge".into())
            .spawn(move || run_audio_bridge(&sink, &source, &broadcast, &reader_state))
            .map_err(|failure| error(format!("Cannot start the audio bridge: {failure}")))?;
        Ok(Self {
            state,
            reader: Some(reader),
        })
    }

    /// Drop buffered audio that a flushing seek has made stale.
    fn flush(&self) {
        self.state.lock().unwrap().flush = true;
    }

    /// Hold or release the backlog across a playback pause.
    fn set_paused(&self, paused: bool) {
        self.state.lock().unwrap().paused = paused;
    }

    /// A bridge with no reader thread, for tests that assemble a session by
    /// hand and never move audio through it.
    #[cfg(test)]
    fn idle() -> Self {
        Self {
            state: Arc::new(Mutex::new(BridgeState::default())),
            reader: None,
        }
    }
}

impl Drop for AudioBridge {
    fn drop(&mut self) {
        self.state.lock().unwrap().stopped = true;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Move every sample the sink has ready into the jitter buffer. Stopping at
/// [`BRIDGE_MAX_FILL`] leaves the rest queued in the sink, which back-pressures
/// the decoder.
fn drain_audio_sink(sink: &gst::Element, buffered: &mut VecDeque<u8>, max_bytes: usize) {
    while buffered.len() < max_bytes {
        let Some(sample) =
            sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&gst::ClockTime::ZERO])
        else {
            return;
        };
        let Some(buffer) = sample.buffer() else {
            continue;
        };
        if let Ok(map) = buffer.map_readable() {
            buffered.extend(map.as_slice());
        }
    }
}

fn run_audio_bridge(
    sink: &gst::Element,
    source: &gst::Element,
    broadcast: &gst::Pipeline,
    state: &Arc<Mutex<BridgeState>>,
) {
    let limits = BridgeLimits {
        period_bytes: audio_bytes(BRIDGE_PERIOD),
        target_bytes: audio_bytes(BRIDGE_TARGET_FILL),
        max_bytes: audio_bytes(BRIDGE_MAX_FILL),
        drift_bytes: audio_bytes(BRIDGE_MAX_DRIFT),
    };
    let period = gst::ClockTime::from_nseconds(BRIDGE_PERIOD.as_nanos() as u64);
    let slip = gst::ClockTime::from_nseconds(BRIDGE_RESYNC_SLIP.as_nanos() as u64);
    let mut next: Option<gst::ClockTime> = None;

    loop {
        if state.lock().unwrap().stopped {
            return;
        }
        // The broadcast pipeline owns the timeline: buffers are stamped with,
        // and released at, its running time, exactly as a live source would.
        let (Some(clock), Some(base)) = (broadcast.clock(), broadcast.base_time()) else {
            next = None;
            thread::sleep(BRIDGE_PERIOD);
            continue;
        };
        let Some(now) = clock.time().checked_sub(base) else {
            next = None;
            thread::sleep(BRIDGE_PERIOD);
            continue;
        };
        let due = match next {
            Some(due) if due + slip > now => due,
            // First tick, or the thread slipped far enough that replaying the
            // backlog would only push us further behind.
            _ => now,
        };
        if let Some(id) = due
            .checked_add(base)
            .map(|deadline| clock.new_single_shot_id(deadline))
        {
            let _ = id.wait();
        }
        if state.lock().unwrap().stopped {
            return;
        }

        let payload = {
            let mut state = state.lock().unwrap();
            state.take_flush();
            drain_audio_sink(sink, &mut state.buffered, limits.max_bytes);
            next_payload(&mut state, &limits)
        };

        let mut buffer = gst::Buffer::from_mut_slice(payload);
        {
            let buffer = buffer.get_mut().expect("fresh buffer is writable");
            buffer.set_pts(due);
            buffer.set_duration(period);
        }
        // A rejected push means the broadcast pipeline is flushing or shutting
        // down. Keep to the schedule and retry rather than killing audio for
        // the rest of the session; the loop only exits when it is stopped.
        let _ = source.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]);
        next = due.checked_add(period);
    }
}

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
    "appsrc",
    "appsink",
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
    audio_source: gst::Element,
    freeze_bin: gst::Bin,
    freeze_source: gst::Element,
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

            appsrc name=movie_audio_source is-live=true format=time
                do-timestamp=false block=false caps="{AUDIO_CAPS}"
              ! queue max-size-buffers=8
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
        let audio_source = pipeline
            .by_name("movie_audio_source")
            .ok_or_else(|| error("Movie audio source is missing"))?;
        audio_source.set_property(
            "min-latency",
            i64::try_from(BRIDGE_PERIOD.as_nanos()).unwrap_or(i64::MAX),
        );
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
            audio_source,
            freeze_bin,
            freeze_source,
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

    fn select_lobby(&self) {
        self.video_selector
            .set_property("active-pad", Some(&self.video_lobby_pad));
        self.audio_selector
            .set_property("active-pad", Some(&self.audio_lobby_pad));
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
    audio_output: gst::Element,
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
    fn build(entry: &MediaInfo) -> Result<Self, Box<dyn Error>> {
        preflight()?;
        let pipeline = gst::Pipeline::new();
        let movie = gst::ElementFactory::make("uridecodebin3")
            .name("movie")
            .property("uri", gst::glib::filename_to_uri(&entry.path, None)?)
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
                 ! audioconvert ! audioresample ! {AUDIO_CAPS} \
                 ! appsink name=movie_audio_output sync=true max-buffers=64 drop=false"
            ),
            false,
            "movie_audio",
        )?;
        let audio_output = audio_bin
            .by_name("movie_audio_output")
            .ok_or_else(|| error("Movie audio output is missing"))?;
        // Releasing on the media timeline is what keeps audio aligned with
        // video: a track that starts late stays late, and a gap stays a gap.
        // The offset hands each buffer over exactly one target fill early, so
        // the bridge gets its slack without the timeline drifting.
        audio_output.set_property(
            "ts-offset",
            -i64::try_from(BRIDGE_TARGET_FILL.as_nanos()).unwrap_or(i64::MAX),
        );
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
            audio_output,
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

struct Playlist {
    entries: Vec<MediaInfo>,
    active: Option<usize>,
    selected: usize,
    next_index: usize,
}

impl Playlist {
    fn new(entries: Vec<MediaInfo>) -> Self {
        Self {
            entries,
            active: None,
            selected: 0,
            next_index: 0,
        }
    }

    fn start_target(&self) -> Option<usize> {
        (self.next_index < self.entries.len()).then_some(self.next_index)
    }

    fn previous_target(&self) -> Option<usize> {
        self.active.unwrap_or(self.next_index).checked_sub(1)
    }

    fn next_target(&self) -> Option<usize> {
        let index = self.active.map_or(self.next_index, |index| index + 1);
        (index < self.entries.len()).then_some(index)
    }

    fn activate(&mut self, index: usize) {
        self.active = Some(index);
        self.selected = index;
        self.next_index = index + 1;
    }

    fn finish_target(&mut self) -> Option<usize> {
        self.active = None;
        self.start_target()
    }

    fn select_by(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected
                .saturating_add(delta as usize)
                .min(self.entries.len() - 1)
        };
    }

    fn move_selected(&mut self, delta: i32) -> bool {
        let target = if delta < 0 {
            self.selected.checked_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected.checked_add(delta as usize)
        };
        let Some(target) = target.filter(|target| *target < self.entries.len()) else {
            return false;
        };
        let crosses = |boundary: usize| {
            (self.selected < boundary && target >= boundary)
                || (target < boundary && self.selected >= boundary)
        };
        if let Some(active) = self.active {
            if self.selected == active || target == active || crosses(active) {
                return false;
            }
        } else if crosses(self.next_index) {
            return false;
        }
        self.entries.swap(self.selected, target);
        self.selected = target;
        true
    }

    fn remove_selected(&mut self) -> bool {
        if self.entries.is_empty() || self.active == Some(self.selected) {
            return false;
        }
        let removed = self.selected;
        self.entries.remove(removed);
        if self.active.is_some_and(|active| removed < active) {
            self.active = self.active.map(|active| active - 1);
        }
        if removed < self.next_index {
            self.next_index -= 1;
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        true
    }

    fn push(&mut self, entry: MediaInfo) {
        self.entries.push(entry);
        self.selected = self.entries.len() - 1;
    }
}

struct ActivePlayback {
    // Declared first so its reader thread is joined before playback is torn
    // down.
    audio_bridge: AudioBridge,
    playback: PlaybackPipeline,
}

pub(crate) struct StreamSession {
    broadcast: BroadcastPipeline,
    active: Option<ActivePlayback>,
    playlist: Playlist,
    state: PlaybackState,
    gain_db: f64,
    levels: AudioLevels,
    title_token: String,
    title_url: String,
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
    pub(crate) fn new(config: &Config, entries: Vec<MediaInfo>) -> Result<Self, Box<dyn Error>> {
        let first = entries
            .first()
            .ok_or_else(|| error("Playlist requires at least one video"))?;
        let output_url = format!(
            "{}/{}",
            config.rtmp_url.trim_end_matches('/'),
            config.stream_key
        );
        let broadcast = BroadcastPipeline::build(&output_url)?;
        set_title(
            &config.title_url,
            &config.title_token,
            &format!("Starting soon: {}", first.title),
        )?;
        broadcast.pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            broadcast,
            active: None,
            playlist: Playlist::new(entries),
            state: PlaybackState::Lobby,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title_token: config.title_token.clone(),
            title_url: config.title_url.clone(),
        })
    }

    pub(crate) fn start(&mut self) -> Result<(), Box<dyn Error>> {
        if self.state != PlaybackState::Lobby {
            return Ok(());
        }
        if let Some(index) = self.playlist.start_target() {
            self.activate_track(index, PlaybackState::Playing)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn start_transition(&mut self, timeout: Duration) -> Result<(), Box<dyn Error>> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| error("Playback is not active"))?;
        active.playback.pipeline.set_state(gst::State::Playing)?;
        active.playback.wait_for_frame_after(0, timeout)?;
        self.broadcast.select_movie();
        self.state = PlaybackState::Playing;
        Ok(())
    }

    fn activate_track(
        &mut self,
        index: usize,
        target_state: PlaybackState,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(active) = &self.active {
            active.audio_bridge.set_paused(true);
            self.broadcast.freeze(active.playback.latest_frame()?)?;
        }
        drop(self.active.take());

        let entry = self
            .playlist
            .entries
            .get(index)
            .cloned()
            .ok_or_else(|| error("Playlist entry is missing"))?;
        let playback = PlaybackPipeline::build(&entry)?;
        playback.pipeline.set_state(gst::State::Paused)?;
        playback.wait_ready(entry.subtitles.as_deref())?;
        let audio_bridge = AudioBridge::start(
            playback.audio_output.clone(),
            self.broadcast.audio_source.clone(),
            self.broadcast.pipeline.clone(),
        )?;
        let active = ActivePlayback {
            audio_bridge,
            playback,
        };
        active.playback.pipeline.set_state(gst::State::Playing)?;
        let frame = active
            .playback
            .wait_for_frame_after(0, Duration::from_secs(1))?;

        self.update_title(index)?;
        if target_state == PlaybackState::Paused {
            active.audio_bridge.set_paused(true);
            self.broadcast.freeze(frame)?;
            active.playback.pipeline.set_state(gst::State::Paused)?;
        } else {
            self.broadcast.select_movie();
        }
        self.active = Some(active);
        self.playlist.activate(index);
        self.state = target_state;
        Ok(())
    }

    fn update_title(&self, index: usize) -> Result<(), Box<dyn Error>> {
        set_title(
            &self.title_url,
            &self.title_token,
            &self.playlist.entries[index].title,
        )
    }

    pub(crate) fn previous_track(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(index) = self.playlist.previous_target() {
            let state = if self.state == PlaybackState::Paused {
                PlaybackState::Paused
            } else {
                PlaybackState::Playing
            };
            self.activate_track(index, state)?;
        }
        Ok(())
    }

    pub(crate) fn next_track(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(index) = self.playlist.next_target() {
            let state = if self.state == PlaybackState::Paused {
                PlaybackState::Paused
            } else {
                PlaybackState::Playing
            };
            self.activate_track(index, state)?;
        }
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

    pub(crate) fn add_entry(&mut self, entry: MediaInfo) {
        self.playlist.push(entry);
    }

    pub(crate) fn entries(&self) -> &[MediaInfo] {
        &self.playlist.entries
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.playlist.active
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.playlist.selected
    }

    pub(crate) fn next_index(&self) -> usize {
        self.playlist.next_index
    }

    pub(crate) fn active_path(&self) -> Option<&std::path::Path> {
        self.playlist
            .active
            .and_then(|index| self.playlist.entries.get(index))
            .map(|entry| entry.path.as_path())
    }

    pub(crate) fn select_entry(&mut self, delta: i32) {
        self.playlist.select_by(delta);
    }

    pub(crate) fn move_selected(&mut self, delta: i32) {
        self.playlist.move_selected(delta);
    }

    pub(crate) fn remove_selected(&mut self) {
        self.playlist.remove_selected();
    }

    pub(crate) fn adjust_gain(&mut self, steps: i8) {
        self.gain_db = adjusted_gain_db(self.gain_db, steps);
        self.broadcast
            .audio_gain
            .set_property("volume", db_to_amplitude(self.gain_db));
    }

    pub(crate) fn title(&self) -> &str {
        self.display_entry()
            .map_or("No track queued", |entry| entry.title.as_str())
    }

    pub(crate) fn duration(&self) -> gst::ClockTime {
        self.display_entry()
            .map_or(gst::ClockTime::ZERO, |entry| entry.duration)
    }

    pub(crate) fn position(&self) -> gst::ClockTime {
        self.active
            .as_ref()
            .and_then(|active| active.playback.pipeline.query_position())
            .unwrap_or(gst::ClockTime::ZERO)
    }

    fn display_entry(&self) -> Option<&MediaInfo> {
        self.playlist
            .active
            .or_else(|| self.playlist.start_target())
            .and_then(|index| self.playlist.entries.get(index))
    }

    fn pause_playback(&self) -> Result<(), Box<dyn Error>> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| error("Playback is not active"))?;
        self.broadcast.freeze(active.playback.latest_frame()?)?;
        active.playback.pipeline.set_state(gst::State::Paused)?;
        Ok(())
    }

    pub(crate) fn toggle_pause(&mut self) -> Result<(), Box<dyn Error>> {
        match self.state {
            PlaybackState::Playing => {
                // Hold the backlog before the broadcast leaves the movie pad,
                // so no buffered audio is consumed into the discarded output.
                let active = self
                    .active
                    .as_ref()
                    .ok_or_else(|| error("Playback is not active"))?;
                active.audio_bridge.set_paused(true);
                if let Err(failure) = self.pause_playback() {
                    active.audio_bridge.set_paused(false);
                    return Err(failure);
                }
                self.state = PlaybackState::Paused;
            }
            PlaybackState::Paused => {
                let active = self
                    .active
                    .as_ref()
                    .ok_or_else(|| error("Playback is not active"))?;
                active.playback.pipeline.set_state(gst::State::Playing)?;
                active.audio_bridge.set_paused(false);
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
        let duration = self.duration();
        let target = seek_target(self.position(), duration, seconds);
        let keep_last_frame = self.state == PlaybackState::Paused && target == duration;
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| error("Playback is not active"))?;
        let generation = active.playback.frame_generation();
        // Audio buffered from before the seek belongs to the old position.
        // Flush ahead of the seek so none of it is broadcast while the playback
        // pipeline settles, and again afterwards to drop whatever the sink
        // handed over mid-seek.
        active.audio_bridge.flush();
        let seek = active
            .playback
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target);
        active.audio_bridge.flush();
        seek?;
        if self.state == PlaybackState::Paused {
            let state_change = active
                .playback
                .pipeline
                .state(gst::ClockTime::from_seconds(5))
                .0;
            PlaybackPipeline::check_bus_errors(
                &active
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
            let frame = active
                .playback
                .wait_for_frame_after(generation, Duration::from_secs(1))?;
            self.broadcast.freeze(frame)?;
        }
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> Result<(), Box<dyn Error>> {
        while gst::glib::MainContext::default().pending() {
            gst::glib::MainContext::default().iteration(false);
        }
        let mut eos = false;
        if let Some(active) = &self.active {
            let playback_bus = active
                .playback
                .pipeline
                .bus()
                .ok_or_else(|| error("Playback bus is missing"))?;
            let subtitles = self
                .playlist
                .active
                .and_then(|index| self.playlist.entries.get(index))
                .and_then(|entry| entry.subtitles.as_deref());
            while let Some(message) = playback_bus.timed_pop(gst::ClockTime::ZERO) {
                if active
                    .playback
                    .handle_stream_collection(&message, subtitles)
                {
                    continue;
                }
                if let Some(failure) = bus_error(&message) {
                    return Err(error(failure));
                }
                if matches!(message.view(), gst::MessageView::Eos(_)) {
                    eos = true;
                }
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
        if eos {
            if let Some(next) = self.playlist.finish_target() {
                self.activate_track(next, PlaybackState::Playing)?;
            } else {
                self.broadcast.select_lobby();
                drop(self.active.take());
                self.state = PlaybackState::Lobby;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
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

    fn entry(title: &str) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from(format!("/tmp/{title}.mkv")),
            subtitles: None,
            title: title.into(),
            duration: gst::ClockTime::from_seconds(60),
        }
    }

    #[test]
    fn playlist_locks_active_row_and_both_crossing_moves() {
        let mut playlist = Playlist::new(vec![
            entry("one"),
            entry("two"),
            entry("three"),
            entry("four"),
        ]);
        playlist.activate(1);

        playlist.selected = 1;
        assert!(!playlist.remove_selected());
        assert!(!playlist.move_selected(1));

        playlist.selected = 0;
        assert!(!playlist.move_selected(1));
        playlist.selected = 2;
        assert!(!playlist.move_selected(-1));
        assert_eq!(
            playlist
                .entries
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three", "four"]
        );
    }

    #[test]
    fn playlist_reorders_and_removes_unlocked_rows() {
        let mut playlist = Playlist::new(vec![
            entry("one"),
            entry("two"),
            entry("three"),
            entry("four"),
        ]);
        playlist.activate(1);
        playlist.selected = 2;

        assert!(playlist.move_selected(1));
        assert_eq!(playlist.selected, 3);
        assert!(playlist.remove_selected());
        assert_eq!(
            playlist
                .entries
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "four"]
        );
        assert_eq!(playlist.active, Some(1));
    }

    #[test]
    fn playlist_tracks_start_previous_next_and_final_lobby() {
        let mut playlist = Playlist::new(vec![entry("one"), entry("two")]);

        assert_eq!(playlist.start_target(), Some(0));
        playlist.activate(0);
        assert_eq!(playlist.previous_target(), None);
        assert_eq!(playlist.next_target(), Some(1));

        playlist.activate(1);
        assert_eq!(playlist.previous_target(), Some(0));
        assert_eq!(playlist.next_target(), None);
        assert_eq!(playlist.finish_target(), None);
        assert_eq!(playlist.active, None);
        assert_eq!(playlist.next_index, 2);

        playlist.push(entry("three"));
        assert_eq!(playlist.start_target(), Some(2));
        assert_eq!(playlist.previous_target(), Some(1));
    }

    #[test]
    fn playlist_does_not_move_pending_entries_into_played_final_lobby_rows() {
        let mut playlist = Playlist::new(vec![entry("one"), entry("two")]);
        playlist.activate(1);
        assert_eq!(playlist.finish_target(), None);
        playlist.push(entry("three"));

        assert!(!playlist.move_selected(-1));
        assert_eq!(
            playlist
                .entries
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert_eq!(playlist.start_target(), Some(2));
    }

    #[test]
    fn session_exposes_playlist_edits() {
        let _gst = gst_test();
        let mut session = session_with_fakesink();

        session.add_entry(entry("Alien"));
        assert_eq!(
            session
                .entries()
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            ["Passenger", "Alien"]
        );
        assert_eq!(session.active_index(), Some(0));

        session.select_entry(1);
        session.move_selected(-1);
        assert_eq!(
            session
                .entries()
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            ["Passenger", "Alien"]
        );
        session.remove_selected();
        assert_eq!(session.entries().len(), 1);
    }

    #[test]
    fn session_track_navigation_is_a_noop_at_playlist_boundaries() {
        let _gst = gst_test();
        let mut session = session_with_fakesink();

        session.previous_track().unwrap();
        session.next_track().unwrap();

        assert_eq!(session.active_index(), Some(0));
        assert_eq!(session.title(), "Passenger");
    }

    #[test]
    fn polling_final_eos_returns_to_lobby_without_finishing_session() {
        let _gst = gst_test();
        let mut session = session_with_fakesink();
        session.state = PlaybackState::Playing;
        session
            .active
            .as_ref()
            .unwrap()
            .playback
            .pipeline
            .bus()
            .unwrap()
            .post(gst::message::Eos::builder().build())
            .unwrap();

        session.poll().unwrap();

        assert_eq!(session.state(), PlaybackState::Lobby);
        assert_eq!(session.active_index(), None);
        assert_eq!(session.title(), "No track queued");
    }

    #[test]
    fn track_activation_updates_owncast_title() {
        let _gst = gst_test();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        let mut session = session_with_fakesink();
        session.add_entry(entry("Alien"));
        session.title_token = "token".into();
        session.title_url = format!("http://{address}/");

        session.update_title(1).unwrap();
        let request = server.join().unwrap();

        assert!(request.contains("POST / HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer token")
        );
        let compact = request
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(compact.contains(r#"{"value":"Alien"}"#));
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
        let mut media = entry("Passenger");
        media.duration = gst::ClockTime::from_seconds(100);
        let mut playlist = Playlist::new(vec![media]);
        playlist.activate(0);
        StreamSession {
            broadcast: BroadcastPipeline::build_with_sink("fakesink").unwrap(),
            active: Some(ActivePlayback {
                audio_bridge: AudioBridge::idle(),
                playback: PlaybackPipeline {
                    pipeline,
                    movie,
                    audio_output: gst::ElementFactory::make("appsink").build().unwrap(),
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
            }),
            playlist,
            state: PlaybackState::Lobby,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title_token: String::new(),
            title_url: String::new(),
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
                audio_output: gst::ElementFactory::make("appsink").build().unwrap(),
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
        let mut media = entry("Passenger");
        media.duration = gst::ClockTime::from_seconds(30);
        let mut playlist = Playlist::new(vec![media]);
        playlist.activate(0);
        StreamSession {
            broadcast: BroadcastPipeline::build_with_sink("fakesink").unwrap(),
            active: Some(ActivePlayback {
                audio_bridge: AudioBridge::idle(),
                playback: PlaybackPipeline {
                    pipeline,
                    movie,
                    audio_output: gst::ElementFactory::make("appsink").build().unwrap(),
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
            }),
            playlist,
            state: PlaybackState::Lobby,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title_token: String::new(),
            title_url: String::new(),
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
        let generation = session.active.as_ref().unwrap().playback.frame_generation();

        session.seek_by(3).unwrap();

        assert!(session.active.as_ref().unwrap().playback.frame_generation() > generation);
        let current = session
            .active
            .as_ref()
            .unwrap()
            .playback
            .latest_frame()
            .unwrap();
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
            .active
            .as_ref()
            .unwrap()
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
        let bus = session
            .active
            .as_ref()
            .unwrap()
            .playback
            .pipeline
            .bus()
            .unwrap();
        session
            .active
            .as_ref()
            .unwrap()
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
        let playback = PlaybackPipeline::build(&entry("movie")).unwrap();
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
    fn broadcast_can_return_from_movie_to_lobby() {
        let _gst = gst_test();
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        broadcast.pipeline.set_state(gst::State::Playing).unwrap();
        broadcast
            .pipeline
            .state(gst::ClockTime::from_seconds(1))
            .0
            .unwrap();

        broadcast.select_movie();
        broadcast.select_lobby();

        assert_eq!(
            broadcast.video_selector.property::<gst::Pad>("active-pad"),
            broadcast.video_lobby_pad
        );
        assert_eq!(
            broadcast.audio_selector.property::<gst::Pad>("active-pad"),
            broadcast.audio_lobby_pad
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
    fn synthetic_track_replacement_keeps_broadcast_live() {
        let _gst = gst_test();
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        let source = || {
            gst::parse::launch(&format!(
                "videotestsrc is-live=true ! videoconvert \
                 ! video/x-raw,format=I420,width=1920,height=1080,framerate=30/1 \
                 ! intervideosink channel={INTER_CHANNEL} sync=true"
            ))
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap()
        };
        let first = source();
        let second = source();
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
        let broadcast_ptr = broadcast.pipeline.as_ptr();

        first.set_state(gst::State::Playing).unwrap();
        broadcast.select_movie();
        std::thread::sleep(Duration::from_millis(150));
        let after_first = frames.load(Ordering::Relaxed);

        broadcast.select_lobby();
        first.set_state(gst::State::Null).unwrap();
        second.set_state(gst::State::Playing).unwrap();
        broadcast.select_movie();
        std::thread::sleep(Duration::from_millis(150));

        assert_eq!(broadcast.pipeline.as_ptr(), broadcast_ptr);
        assert!(after_first > 0);
        assert!(frames.load(Ordering::Relaxed) > after_first);

        second.set_state(gst::State::Null).unwrap();
        broadcast.pipeline.set_state(gst::State::Null).unwrap();
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

    fn bridge_state(buffered: usize, priming: bool) -> BridgeState {
        BridgeState {
            buffered: std::iter::repeat_n(0x7f, buffered).collect(),
            priming,
            ..BridgeState::default()
        }
    }

    fn bridge_limits() -> BridgeLimits {
        BridgeLimits {
            period_bytes: 40,
            target_bytes: 160,
            max_bytes: 400,
            drift_bytes: 80,
        }
    }

    #[test]
    fn priming_bridge_emits_silence_until_the_target_fill_is_buffered() {
        let mut state = bridge_state(80, true);

        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0; 40]);
        assert!(state.priming, "bridge left priming below the target fill");
        assert_eq!(state.buffered.len(), 80, "silence consumed buffered audio");
    }

    #[test]
    fn primed_bridge_emits_buffered_audio_once_the_target_fill_is_reached() {
        let mut state = bridge_state(160, true);

        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0x7f; 40]);
        assert!(!state.priming);
        assert_eq!(state.buffered.len(), 120);
        // Still running, so a period is now enough to keep emitting audio.
        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0x7f; 40]);
        assert!(!state.priming);
    }

    #[test]
    fn starved_bridge_emits_silence_without_consuming_the_backlog() {
        let mut state = bridge_state(20, false);

        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0; 40]);
        assert_eq!(state.buffered.len(), 20, "silence consumed buffered audio");

        // The sink refills on the media timeline, so a period is enough to
        // resume; priming is only for startup and flushes.
        state.buffered.extend([0x7f; 40]);
        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0x7f; 40]);
    }

    #[test]
    fn paused_bridge_holds_the_backlog_for_the_resume() {
        let mut state = bridge_state(200, false);
        state.paused = true;

        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0; 40]);
        assert_eq!(
            state.buffered.len(),
            200,
            "paused bridge consumed audio into the discarded pad"
        );

        state.paused = false;
        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0x7f; 40]);
        assert!(!state.priming, "resume should not re-prime a full backlog");
    }

    #[test]
    fn backlog_beyond_the_drift_allowance_is_dropped_to_the_target() {
        // Repeated silence has pushed the backlog past target + drift.
        let mut state = bridge_state(300, false);

        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0x7f; 40]);

        // Trimmed to the target, then a period taken from it.
        assert_eq!(state.buffered.len(), 120);
    }

    #[test]
    fn backlog_within_the_drift_allowance_is_left_alone() {
        let mut state = bridge_state(240, false);

        assert_eq!(next_payload(&mut state, &bridge_limits()), vec![0x7f; 40]);

        assert_eq!(state.buffered.len(), 200, "ordinary jitter was trimmed");
    }

    #[test]
    fn flush_drops_buffered_audio_and_reprimes() {
        let mut state = bridge_state(200, false);
        state.flush = true;

        state.take_flush();

        assert!(state.buffered.is_empty(), "stale audio survived the flush");
        assert!(state.priming, "flushed bridge did not return to priming");
        assert!(!state.flush, "flush request was not consumed");
    }

    #[test]
    fn pausing_holds_buffered_audio_and_resuming_releases_it() {
        let _gst = gst_test();
        let mut session = session_with_test_video(0);
        session
            .broadcast
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session.start_transition(Duration::from_secs(1)).unwrap();

        session.toggle_pause().unwrap();

        assert_eq!(session.state(), PlaybackState::Paused);
        assert!(
            session
                .active
                .as_ref()
                .unwrap()
                .audio_bridge
                .state
                .lock()
                .unwrap()
                .paused,
            "paused playback kept draining the audio backlog"
        );

        session.toggle_pause().unwrap();

        assert_eq!(session.state(), PlaybackState::Playing);
        assert!(
            !session
                .active
                .as_ref()
                .unwrap()
                .audio_bridge
                .state
                .lock()
                .unwrap()
                .paused
        );
    }

    #[test]
    fn seek_flushes_audio_buffered_before_the_new_position() {
        let _gst = gst_test();
        let mut session = session_with_fakesink();
        session.state = PlaybackState::Playing;
        session
            .active
            .as_ref()
            .unwrap()
            .audio_bridge
            .state
            .lock()
            .unwrap()
            .buffered
            .extend([0x7f; 64]);

        session.seek_by(-5).unwrap();

        assert!(
            session
                .active
                .as_ref()
                .unwrap()
                .audio_bridge
                .state
                .lock()
                .unwrap()
                .flush
        );
    }

    #[test]
    fn audio_bridge_delivers_decoded_audio_after_priming() {
        let _gst = gst_test();
        let broadcast = gst::parse::launch(&format!(
            r#"
            appsrc name=movie_audio_source is-live=true format=time
                do-timestamp=false block=false caps="{AUDIO_CAPS}"
              ! fakesink name=sink sync=true
            "#
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let playback = gst::parse::launch(&format!(
            r#"
            audiotestsrc wave=sine freq=1000 volume=0.8
              ! audioconvert ! audioresample ! {AUDIO_CAPS}
              ! appsink name=movie_audio_output sync=false max-buffers=64 drop=false
            "#
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let loud = Arc::new(AtomicU64::new(0));
        let counted = loud.clone();
        broadcast
            .by_name("sink")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer()
                    && let Ok(map) = buffer.map_readable()
                    && map.chunks_exact(4).any(|sample| {
                        f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]).abs() > 0.1
                    })
                {
                    counted.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        broadcast.set_state(gst::State::Playing).unwrap();
        playback.set_state(gst::State::Playing).unwrap();
        let bridge = AudioBridge::start(
            playback.by_name("movie_audio_output").unwrap(),
            broadcast.by_name("movie_audio_source").unwrap(),
            broadcast.clone(),
        );
        std::thread::sleep(Duration::from_millis(900));
        let delivered = loud.load(Ordering::Relaxed);
        drop(bridge);
        playback.set_state(gst::State::Null).unwrap();
        broadcast.set_state(gst::State::Null).unwrap();

        assert!(
            delivered >= 10,
            "bridge delivered only {delivered} audio buffers"
        );
    }

    /// Records the broadcast's own timeline for both streams: one CSV row per
    /// buffer leaving the video selector and the audio gain, tagged with the
    /// running time it carries. A source whose video flashes and audio beeps
    /// share an edge then shows any A/V offset as the gap between those edges.
    #[test]
    #[ignore = "manual probe: PROBE_VIDEO=flashes.mkv PROBE_OUT=out.csv cargo test -- --ignored"]
    fn av_sync_probe() {
        let _gst = gst_test();
        let config = Config {
            startup_dir: PathBuf::from("/tmp"),
            video: std::env::var("PROBE_VIDEO").unwrap().into(),
            subtitles: None,
            title: None,
            queued_videos: Vec::new(),
            stream_key: String::new(),
            title_token: String::new(),
            rtmp_url: String::new(),
            title_url: String::new(),
        };
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        let playback = PlaybackPipeline::build(&MediaInfo {
            path: config.video,
            subtitles: None,
            title: "Probe".into(),
            duration: gst::ClockTime::from_seconds(1),
        })
        .unwrap();
        let log = Arc::new(Mutex::new(
            std::fs::File::create(std::env::var("PROBE_OUT").unwrap()).unwrap(),
        ));

        let video_log = log.clone();
        broadcast
            .video_selector
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                use std::io::Write;
                if let Some(buffer) = info.buffer()
                    && let Some(pts) = buffer.pts()
                    && let Ok(map) = buffer.map_readable()
                {
                    // Whole frames are flat black or white, so a corner of the
                    // luma plane is enough to classify one.
                    let corner = &map.as_slice()[..4096.min(map.size())];
                    let luma = corner.iter().map(|&value| u32::from(value)).sum::<u32>()
                        / corner.len() as u32;
                    writeln!(video_log.lock().unwrap(), "v,{},{luma}", pts.nseconds()).unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        let audio_log = log.clone();
        broadcast
            .audio_gain
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                use std::io::Write;
                if let Some(buffer) = info.buffer()
                    && let Some(pts) = buffer.pts()
                    && let Ok(map) = buffer.map_readable()
                {
                    let peak = map
                        .as_slice()
                        .chunks_exact(4)
                        .map(|sample| {
                            f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]).abs()
                        })
                        .fold(0.0_f32, f32::max);
                    writeln!(audio_log.lock().unwrap(), "a,{},{peak:.4}", pts.nseconds()).unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        broadcast.pipeline.set_state(gst::State::Playing).unwrap();
        playback.pipeline.set_state(gst::State::Paused).unwrap();
        let bridge = AudioBridge::start(
            playback.audio_output.clone(),
            broadcast.audio_source.clone(),
            broadcast.pipeline.clone(),
        )
        .unwrap();
        playback.wait_ready(None).unwrap();
        playback.pipeline.set_state(gst::State::Playing).unwrap();
        playback
            .wait_for_frame_after(0, Duration::from_secs(5))
            .unwrap();
        broadcast.select_movie();

        let seconds = std::env::var("PROBE_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(30);
        // Optionally exercise a pause partway through, which is where holding
        // the backlog matters.
        if let Ok(at) = std::env::var("PROBE_PAUSE_AT")
            && let Ok(at) = at.parse::<u64>()
        {
            std::thread::sleep(Duration::from_secs(at));
            bridge.set_paused(true);
            broadcast.freeze(playback.latest_frame().unwrap()).unwrap();
            playback.pipeline.set_state(gst::State::Paused).unwrap();
            std::thread::sleep(Duration::from_secs(2));
            playback.pipeline.set_state(gst::State::Playing).unwrap();
            bridge.set_paused(false);
            broadcast.select_movie();
            std::thread::sleep(Duration::from_secs(seconds - at));
        } else {
            std::thread::sleep(Duration::from_secs(seconds));
        }
        drop(bridge);
    }

    #[test]
    #[ignore = "manual probe: PROBE_VIDEO=in.mkv PROBE_OUT=out.f32 cargo test -- --ignored"]
    fn audio_bridge_continuity_probe() {
        let _gst = gst_test();
        let config = Config {
            startup_dir: PathBuf::from("/tmp"),
            video: std::env::var("PROBE_VIDEO").unwrap().into(),
            subtitles: None,
            title: None,
            queued_videos: Vec::new(),
            stream_key: String::new(),
            title_token: String::new(),
            rtmp_url: String::new(),
            title_url: String::new(),
        };
        let broadcast = BroadcastPipeline::build_with_sink("fakesink").unwrap();
        let playback = PlaybackPipeline::build(&MediaInfo {
            path: config.video,
            subtitles: None,
            title: "Probe".into(),
            duration: gst::ClockTime::from_seconds(1),
        })
        .unwrap();
        let capture = Arc::new(Mutex::new(
            std::fs::File::create(std::env::var("PROBE_OUT").unwrap()).unwrap(),
        ));
        broadcast
            .audio_gain
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                use std::io::Write;
                if let Some(buffer) = info.buffer()
                    && let Ok(map) = buffer.map_readable()
                {
                    capture.lock().unwrap().write_all(map.as_slice()).unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        broadcast.pipeline.set_state(gst::State::Playing).unwrap();
        playback.pipeline.set_state(gst::State::Paused).unwrap();
        let bridge = AudioBridge::start(
            playback.audio_output.clone(),
            broadcast.audio_source.clone(),
            broadcast.pipeline.clone(),
        );
        playback.wait_ready(None).unwrap();
        playback.pipeline.set_state(gst::State::Playing).unwrap();
        playback
            .wait_for_frame_after(0, Duration::from_secs(5))
            .unwrap();
        broadcast.select_movie();
        let seconds = std::env::var("PROBE_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(25);
        std::thread::sleep(Duration::from_secs(seconds));
        drop(bridge);
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
            audio_output: gst::ElementFactory::make("appsink").build().unwrap(),
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
        let mut media = entry("Passenger");
        media.duration = gst::ClockTime::from_seconds(30);
        let mut playlist = Playlist::new(vec![media]);
        playlist.activate(0);
        let mut session = StreamSession {
            broadcast: BroadcastPipeline::build_with_sink("fakesink").unwrap(),
            active: Some(ActivePlayback {
                audio_bridge: AudioBridge::idle(),
                playback,
            }),
            playlist,
            state: PlaybackState::Playing,
            gain_db: DEFAULT_GAIN_DB,
            levels: AudioLevels::default(),
            title_token: String::new(),
            title_url: String::new(),
        };
        session
            .active
            .as_ref()
            .unwrap()
            .playback
            .pipeline
            .set_state(gst::State::Playing)
            .unwrap();
        session
            .active
            .as_ref()
            .unwrap()
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
