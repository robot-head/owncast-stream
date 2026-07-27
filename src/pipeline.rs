use gst::glib::translate::IntoGlib;
use gst::prelude::*;
use gstreamer as gst;
use std::{
    env,
    error::Error,
    ffi::c_void,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use crate::{
    Config, error,
    media::{Selection, StreamCandidate, StreamKind, SubtitleSource, select_streams},
    set_title,
};

fn rebase_timestamp(timestamp: u64, boundary: u64, first_pts: u64) -> Result<u64, String> {
    let rebased = i128::from(timestamp) + i128::from(boundary) - i128::from(first_pts);
    u64::try_from(rebased).map_err(|_| "Movie timestamp rebase is out of range".to_owned())
}

fn bus_error(message: &gst::MessageRef) -> Option<String> {
    match message.view() {
        gst::MessageView::Error(error) => Some(format!(
            "{}: {} ({})",
            error
                .src()
                .map(|source| source.path_string())
                .unwrap_or_else(|| "unknown".into()),
            error.error(),
            error.debug().unwrap_or_default()
        )),
        _ => None,
    }
}

fn next_frame_boundary(now: gst::ClockTime) -> gst::ClockTime {
    const FRAME_NS: u64 = 1_000_000_000 / 30;
    gst::ClockTime::from_nseconds(((now.nseconds() / FRAME_NS) + 1) * FRAME_NS)
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

#[link(name = "glib-2.0")]
unsafe extern "C" {
    fn g_unix_signal_add_full(
        priority: i32,
        signum: i32,
        handler: unsafe extern "C" fn(*mut c_void) -> i32,
        data: *mut c_void,
        notify: unsafe extern "C" fn(*mut c_void),
    ) -> u32;
    fn g_source_remove(tag: u32) -> i32;
}

struct SignalData {
    bus: gst::Bus,
    removed: Arc<AtomicBool>,
}

unsafe extern "C" fn post_interrupt(data: *mut c_void) -> i32 {
    // SAFETY: GLib calls this with the boxed data passed to g_unix_signal_add_full.
    let data = unsafe { &*(data.cast::<SignalData>()) };
    let _ = data.bus.post(
        gst::message::Application::builder(gst::Structure::builder("owncast-interrupt").build())
            .build(),
    );
    data.removed.store(true, Ordering::Release);
    0
}

unsafe extern "C" fn drop_signal_bus(data: *mut c_void) {
    // SAFETY: GLib invokes destroy notify exactly once for this Box::into_raw pointer.
    drop(unsafe { Box::from_raw(data.cast::<SignalData>()) });
}

struct SignalGuard {
    id: u32,
    removed: Arc<AtomicBool>,
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if !self.removed.swap(true, Ordering::AcqRel) {
            // SAFETY: id is a live GLib source owned by this guard.
            unsafe {
                g_source_remove(self.id);
            }
        }
    }
}

type SignalRegistrar = unsafe extern "C" fn(
    i32,
    i32,
    unsafe extern "C" fn(*mut c_void) -> i32,
    *mut c_void,
    unsafe extern "C" fn(*mut c_void),
) -> u32;

fn register_sigint_with(
    bus: &gst::Bus,
    registrar: SignalRegistrar,
) -> Result<SignalGuard, Box<dyn Error>> {
    let removed = Arc::new(AtomicBool::new(false));
    let data = Box::into_raw(Box::new(SignalData {
        bus: bus.clone(),
        removed: removed.clone(),
    }))
    .cast();
    // SAFETY: callbacks use the boxed GstBus until GLib calls the destroy notifier.
    let id = unsafe {
        registrar(
            gst::glib::Priority::DEFAULT.into_glib(),
            2,
            post_interrupt,
            data,
            drop_signal_bus,
        )
    };
    if id == 0 {
        // SAFETY: a failed registration did not transfer ownership to GLib.
        drop(unsafe { Box::from_raw(data.cast::<SignalData>()) });
        return Err(error("Cannot register SIGINT handler"));
    }
    Ok(SignalGuard { id, removed })
}

fn register_sigint(bus: &gst::Bus) -> Result<SignalGuard, Box<dyn Error>> {
    register_sigint_with(bus, g_unix_signal_add_full)
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

pub(crate) struct PipelineParts {
    pub(crate) pipeline: gst::Pipeline,
    pub(crate) movie: gst::Element,
    pub(crate) video_selector: gst::Element,
    pub(crate) audio_selector: gst::Element,
    pub(crate) movie_video_pad: gst::Pad,
    pub(crate) movie_audio_pad: gst::Pad,
    pub(crate) movie_video_sink: gst::Pad,
    pub(crate) movie_audio_sink: gst::Pad,
    pub(crate) movie_subtitle_sink: gst::Pad,
    pub(crate) movie_video_src: gst::Pad,
    pub(crate) movie_audio_src: gst::Pad,
    pub(crate) subtitle_overlay: gst::Element,
}

impl PipelineParts {
    pub(crate) fn build(config: &Config, output_url: &str) -> Result<Self, Box<dyn Error>> {
        let parts = Self::build_with_sink(config, "rtmpsink")?;
        parts
            .pipeline
            .by_name("output")
            .ok_or_else(|| error("Pipeline output is missing"))?
            .set_property("location", output_url);
        Ok(parts)
    }

    fn build_with_sink(config: &Config, sink: &str) -> Result<Self, Box<dyn Error>> {
        gst::init()?;
        let missing = missing_elements(required_elements());
        if !missing.is_empty() {
            return Err(error(format!(
                "Missing GStreamer elements: {}",
                missing.join(", ")
            )));
        }

        let description = format!(
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

            audiotestsrc is-live=true wave=silence
              ! audio/x-raw,rate=48000,channels=2
              ! queue max-size-buffers=8 leaky=downstream
              ! audio_selector.sink_0

            input-selector name=video_selector sync-streams=true
                sync-mode=clock cache-buffers=true drop-backwards=true
              ! videoconvert
              ! video/x-raw,format=I420,width=1920,height=1080,framerate=30/1
              ! x264enc name=video_encoder bitrate=6000 key-int-max=60 bframes=0
                  tune=zerolatency speed-preset=medium
              ! h264parse name=video_parser config-interval=1
              ! queue name=video_output_queue
              ! mux.

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
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| error("Parsed graph is not a pipeline"))?;
        let movie = gst::ElementFactory::make("uridecodebin3")
            .name("movie")
            .property("uri", gst::glib::filename_to_uri(&config.video, None)?)
            .build()?;
        pipeline.add(&movie)?;

        let video_bin = gst::parse::bin_from_description_with_name(
            r#"
            queue name=movie_video_input max-size-buffers=2
              ! videoconvert
              ! aspectratiocrop aspect-ratio=16/9
              ! videoscale
              ! videorate
              ! video/x-raw,width=1920,height=1080,framerate=30/1
              ! subtitleoverlay name=movie_subtitles
              ! queue max-size-buffers=2
            "#,
            true,
            "movie_video",
        )?;
        let movie_video_sink = gst::GhostPad::builder_with_target(
            &video_bin
                .by_name("movie_video_input")
                .ok_or_else(|| error("Movie video input queue is missing"))?
                .static_pad("sink")
                .ok_or_else(|| error("Movie video input is missing"))?,
        )?
        .name("video_sink")
        .build();
        video_bin.add_pad(&movie_video_sink)?;
        let audio_bin = gst::parse::bin_from_description_with_name(
            "queue max-size-buffers=8",
            true,
            "movie_audio",
        )?;
        pipeline.add(&video_bin)?;
        pipeline.add(&audio_bin)?;

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
            .request_pad_simple("sink_%u")
            .ok_or_else(|| error("Cannot request movie video pad"))?;
        let audio_movie_pad = audio_selector
            .request_pad_simple("sink_%u")
            .ok_or_else(|| error("Cannot request movie audio pad"))?;
        video_bin
            .static_pad("src")
            .ok_or_else(|| error("Movie video output is missing"))?
            .link(&video_movie_pad)?;
        audio_bin
            .static_pad("src")
            .ok_or_else(|| error("Movie audio output is missing"))?
            .link(&audio_movie_pad)?;
        video_selector.set_property("active-pad", Some(&video_lobby_pad));
        audio_selector.set_property("active-pad", Some(&audio_lobby_pad));
        let subtitle_overlay = video_bin
            .by_name("movie_subtitles")
            .ok_or_else(|| error("Subtitle overlay is missing"))?;

        Ok(Self {
            pipeline,
            movie,
            video_selector,
            audio_selector,
            movie_video_pad: video_movie_pad,
            movie_audio_pad: audio_movie_pad,
            movie_video_sink: movie_video_sink.upcast(),
            movie_audio_sink: audio_bin
                .static_pad("sink")
                .ok_or_else(|| error("Movie audio input is missing"))?,
            movie_subtitle_sink: video_bin
                .static_pad("sink")
                .ok_or_else(|| error("Movie subtitle input is missing"))?,
            movie_video_src: video_bin
                .static_pad("src")
                .ok_or_else(|| error("Movie video output is missing"))?,
            movie_audio_src: audio_bin
                .static_pad("src")
                .ok_or_else(|| error("Movie audio output is missing"))?,
            subtitle_overlay,
        })
    }
}

impl Drop for PipelineParts {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[derive(Default)]
struct ReadyState {
    video_pts: Option<gst::ClockTime>,
    audio_pts: Option<gst::ClockTime>,
    video_rebase: Option<TimestampRebase>,
    audio_rebase: Option<TimestampRebase>,
    enter_pressed: bool,
    failure: Option<String>,
    pending_pads: Vec<PendingPad>,
    blocking_probes: Option<BlockingProbes>,
    adjusted_subtitles: Option<AdjustedSubtitles>,
}

#[derive(Clone, Copy)]
struct TimestampRebase {
    boundary: u64,
    first_pts: u64,
}

struct PendingPad {
    pad: gst::Pad,
    stream_id: Option<String>,
}

#[derive(Clone)]
struct SelectedSinks {
    video: gst::Pad,
    audio: gst::Pad,
    subtitle: gst::Pad,
}

fn retain_lobby(ready: &Arc<(Mutex<ReadyState>, Condvar)>, reason: impl Into<String>) {
    let reason = reason.into();
    eprintln!("{reason}; the lobby will remain live until Ctrl-C.");
    ready.0.lock().unwrap().failure = Some(reason);
    ready.1.notify_all();
}

fn resolve_pending_pads(
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    selection: &OnceLock<Selection>,
    sinks: &SelectedSinks,
) {
    let Some(selection) = selection.get() else {
        return;
    };
    let pending = std::mem::take(&mut ready.0.lock().unwrap().pending_pads);
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
            retain_lobby(
                ready,
                format!("Cannot link selected stream {stream_id}: {failure}"),
            );
        }
    }
    ready.0.lock().unwrap().pending_pads.extend(unresolved);
}

fn watch_movie_pad(
    pad: &gst::Pad,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    selection: &Arc<OnceLock<Selection>>,
    sinks: &SelectedSinks,
) {
    ready.0.lock().unwrap().pending_pads.push(PendingPad {
        pad: pad.clone(),
        stream_id: None,
    });
    let seen = Arc::new(AtomicBool::new(false));
    let probe_seen = seen.clone();
    let probe_ready = ready.clone();
    let probe_selection = selection.clone();
    let probe_sinks = sinks.clone();
    let probe_id = pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::EventView::StreamStart(event)) = info.event().map(|event| event.view())
        else {
            return gst::PadProbeReturn::Ok;
        };
        if !probe_seen.swap(true, Ordering::AcqRel) {
            if let Some(pending) = probe_ready
                .0
                .lock()
                .unwrap()
                .pending_pads
                .iter_mut()
                .find(|pending| pending.pad == *pad)
            {
                pending.stream_id = Some(event.stream_id().to_owned());
            }
            resolve_pending_pads(&probe_ready, &probe_selection, &probe_sinks);
        }
        gst::PadProbeReturn::Remove
    });
    if let Some(event) = pad.sticky_event::<gst::event::StreamStart>(0)
        && !seen.swap(true, Ordering::AcqRel)
    {
        if let Some(pending) = ready
            .0
            .lock()
            .unwrap()
            .pending_pads
            .iter_mut()
            .find(|pending| pending.pad == *pad)
        {
            pending.stream_id = Some(event.stream_id().to_owned());
        }
        resolve_pending_pads(ready, selection, sinks);
        if let Some(probe_id) = probe_id {
            pad.remove_probe(probe_id);
        }
    } else if probe_id.is_none() {
        retain_lobby(ready, format!("Cannot inspect movie pad {}", pad.name()));
    }
}

fn fatal_pipeline_error(
    message: &gst::MessageRef,
    pipeline: &gst::Pipeline,
    switched: bool,
) -> bool {
    switched
        || !message.src().is_some_and(|source| {
            [
                "movie",
                "movie_video",
                "movie_audio",
                "movie_external_subtitles",
            ]
            .into_iter()
            .filter_map(|name| pipeline.by_name(name))
            .any(|element| {
                source == element.upcast_ref::<gst::Object>() || source.has_as_ancestor(&element)
            })
        })
}

fn record_first_pts(
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    video: bool,
    info: &gst::PadProbeInfo<'_>,
) {
    let Some(pts) = info.buffer().and_then(|buffer| buffer.pts()) else {
        return;
    };
    let mut state = ready.0.lock().unwrap();
    let slot = if video {
        &mut state.video_pts
    } else {
        &mut state.audio_pts
    };
    if slot.is_some() {
        return;
    }
    *slot = Some(pts);
    drop(state);
    ready.1.notify_all();
}

fn block_first_buffer(
    pad: &gst::Pad,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    video: bool,
) -> Option<gst::PadProbeId> {
    let ready = ready.clone();
    pad.add_probe(
        gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
        move |_, info| {
            record_first_pts(&ready, video, info);
            gst::PadProbeReturn::Ok
        },
    )
}

fn configure_timestamp_rebase(
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    boundary: gst::ClockTime,
) -> Result<(), String> {
    let mut state = ready.0.lock().unwrap();
    let video_pts = state
        .video_pts
        .ok_or_else(|| "Movie video timestamp is missing".to_owned())?;
    let audio_pts = state
        .audio_pts
        .ok_or_else(|| "Movie audio timestamp is missing".to_owned())?;
    state.video_rebase = Some(TimestampRebase {
        boundary: boundary.nseconds(),
        first_pts: video_pts.nseconds(),
    });
    state.audio_rebase = Some(TimestampRebase {
        boundary: boundary.nseconds(),
        first_pts: audio_pts.nseconds(),
    });
    Ok(())
}

fn add_timestamp_rebase_probe(
    pad: &gst::Pad,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    bus: &gst::Bus,
    video: bool,
) -> Option<gst::PadProbeId> {
    let ready = ready.clone();
    let bus = bus.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let timing = {
            let state = ready.0.lock().unwrap();
            if video {
                state.video_rebase
            } else {
                state.audio_rebase
            }
        };
        let result = (|| {
            let timing =
                timing.ok_or_else(|| "Movie timestamp rebase is not configured".to_owned())?;
            let buffer = info
                .buffer_mut()
                .ok_or_else(|| "Movie timestamp probe received no buffer".to_owned())?;
            let pts = buffer
                .pts()
                .map(|pts| {
                    rebase_timestamp(pts.nseconds(), timing.boundary, timing.first_pts)
                        .map(gst::ClockTime::from_nseconds)
                })
                .transpose()?;
            let dts = buffer
                .dts()
                .map(|dts| {
                    rebase_timestamp(dts.nseconds(), timing.boundary, timing.first_pts)
                        .map(gst::ClockTime::from_nseconds)
                })
                .transpose()?;
            let buffer = buffer.make_mut();
            buffer.set_pts(pts);
            buffer.set_dts(dts);
            Ok::<(), String>(())
        })();
        match result {
            Ok(()) => gst::PadProbeReturn::Ok,
            Err(failure) => {
                post_application_error(&bus, "owncast-switch-error", failure);
                gst::PadProbeReturn::Drop
            }
        }
    })
}

struct BlockingProbes {
    video: Option<(gst::Pad, gst::PadProbeId)>,
    audio: Option<(gst::Pad, gst::PadProbeId)>,
}

impl BlockingProbes {
    fn release(&mut self) {
        if let Some((pad, id)) = self.video.take() {
            pad.remove_probe(id);
        }
        if let Some((pad, id)) = self.audio.take() {
            pad.remove_probe(id);
        }
    }
}

impl Drop for BlockingProbes {
    fn drop(&mut self) {
        self.release();
    }
}

fn release_blocking_probes(ready: &Arc<(Mutex<ReadyState>, Condvar)>) {
    let probes = ready.0.lock().unwrap().blocking_probes.take();
    if let Some(mut probes) = probes {
        probes.release();
    }
}

fn post_application_error(bus: &gst::Bus, name: &str, failure: impl ToString) {
    let structure = gst::Structure::builder(name)
        .field("error", failure.to_string())
        .build();
    let _ = bus.post(gst::message::Application::builder(structure).build());
}

fn application_failure(message: &gst::MessageRef) -> Option<String> {
    let gst::MessageView::Application(application) = message.view() else {
        return None;
    };
    let structure = application.structure()?;
    let fallback = match structure.name().as_str() {
        "owncast-title-error" => "Cannot update Owncast title",
        "owncast-switch-error" => "Cannot schedule movie handoff",
        _ => return None,
    };
    Some(
        structure
            .get::<String>("error")
            .unwrap_or_else(|_| fallback.into()),
    )
}

fn require_timing(
    clock: Option<gst::Clock>,
    base_time: Option<gst::ClockTime>,
) -> Result<(gst::Clock, gst::ClockTime), String> {
    Ok((
        clock.ok_or_else(|| "Pipeline clock is missing".to_owned())?,
        base_time.ok_or_else(|| "Pipeline base time is missing".to_owned())?,
    ))
}

fn pipeline_timing(pipeline: &gst::Pipeline) -> Result<(gst::Clock, gst::ClockTime), String> {
    require_timing(pipeline.clock(), pipeline.base_time())
}

fn schedule_clock_callback<F, G>(
    clock_id: &gst::SingleShotClockId,
    bus: &gst::Bus,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    switched: &Arc<AtomicBool>,
    callback: F,
    after_release: G,
) -> bool
where
    F: FnOnce(&gst::Clock, Option<gst::ClockTime>, &gst::ClockId) + Send + 'static,
    G: FnOnce() + Send + 'static,
{
    let callback_ready = ready.clone();
    let callback_switched = switched.clone();
    match clock_id.wait_async(move |clock, jitter, id| {
        callback(clock, jitter, id);
        callback_switched.store(true, Ordering::Release);
        release_blocking_probes(&callback_ready);
        after_release();
    }) {
        Ok(_) => true,
        Err(failure) => {
            release_blocking_probes(ready);
            post_application_error(
                bus,
                "owncast-switch-error",
                format!("Cannot schedule movie handoff: {failure}"),
            );
            false
        }
    }
}

fn add_external_subtitles(
    pipeline: &gst::Pipeline,
    sink: &gst::Pad,
    path: &std::path::Path,
) -> Result<AdjustedSubtitles, Box<dyn Error>> {
    let contents = std::fs::read_to_string(path)?;
    let adjusted = write_adjusted_subtitles(&contents)?;
    let subtitles = gst::parse::bin_from_description_with_name(
        "filesrc name=source ! subparse ! queue",
        true,
        "movie_external_subtitles",
    )?;
    subtitles
        .by_name("source")
        .ok_or_else(|| error("External subtitle source is missing"))?
        .set_property("location", &adjusted.0);
    pipeline.add(&subtitles)?;
    let output = subtitles
        .static_pad("src")
        .ok_or_else(|| error("External subtitle output is missing"))?;
    output.link(sink)?;
    subtitles.sync_state_with_parent()?;
    Ok(adjusted)
}

static SUBTITLE_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct AdjustedSubtitles(PathBuf);

impl Drop for AdjustedSubtitles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_adjusted_subtitles(contents: &str) -> Result<AdjustedSubtitles, Box<dyn Error>> {
    let sequence = SUBTITLE_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "owncast-stream-subtitles-{}-{sequence}.srt",
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(contents.as_bytes())?;
    Ok(AdjustedSubtitles(path))
}

#[allow(clippy::too_many_arguments)]
fn configure_movie_streams(
    candidates: &[StreamCandidate],
    external: Option<&std::path::Path>,
    movie: &gst::Element,
    pipeline: &gst::Pipeline,
    overlay: &gst::Element,
    selection: &Arc<OnceLock<Selection>>,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    sinks: &SelectedSinks,
) {
    let chosen = match select_streams(candidates, external) {
        Ok(chosen) => chosen,
        Err(reason) => {
            retain_lobby(ready, reason);
            return;
        }
    };
    if selection.set(chosen).is_err() {
        retain_lobby(ready, "Movie streams were selected twice");
        return;
    }
    let chosen = selection.get().unwrap();
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
    if !movie.send_event(gst::event::SelectStreams::new(ids)) {
        retain_lobby(ready, "Movie rejected selected streams");
        return;
    }
    resolve_pending_pads(ready, selection, sinks);
    match &chosen.subtitle {
        SubtitleSource::External(path) => {
            match add_external_subtitles(pipeline, &sinks.subtitle, path) {
                Ok(adjusted) => ready.0.lock().unwrap().adjusted_subtitles = Some(adjusted),
                Err(failure) => retain_lobby(
                    ready,
                    format!("Cannot prepare external subtitles: {failure}"),
                ),
            }
        }
        SubtitleSource::None => overlay.set_property("silent", true),
        SubtitleSource::Embedded(_) => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_stream_collection_message(
    message: &gst::MessageRef,
    external: Option<&std::path::Path>,
    movie: &gst::Element,
    pipeline: &gst::Pipeline,
    overlay: &gst::Element,
    selection: &Arc<OnceLock<Selection>>,
    ready: &Arc<(Mutex<ReadyState>, Condvar)>,
    sinks: &SelectedSinks,
) -> bool {
    let gst::MessageView::StreamCollection(collection) = message.view() else {
        return false;
    };
    if selection.get().is_some()
        || !message.src().is_some_and(|source| {
            source == movie.upcast_ref::<gst::Object>() || source.has_as_ancestor(movie)
        })
    {
        return false;
    }
    let candidates: Vec<_> = collection
        .stream_collection()
        .iter()
        .filter_map(|stream| candidate(&stream))
        .collect();
    configure_movie_streams(
        &candidates,
        external,
        movie,
        pipeline,
        overlay,
        selection,
        ready,
        sinks,
    );
    true
}

pub(crate) fn run(config: &Config) -> Result<(), Box<dyn Error>> {
    let output_url = env::var("OWNCAST_OUTPUT_URL")
        .unwrap_or_else(|_| format!("rtmp://127.0.0.1/live/{}", config.stream_key));
    let parts = PipelineParts::build(config, &output_url)?;
    let bus = parts
        .pipeline
        .bus()
        .ok_or_else(|| error("Pipeline bus is missing"))?;
    let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
    let selection = Arc::new(OnceLock::<Selection>::new());
    let switched = Arc::new(AtomicBool::new(false));

    let video_probe = block_first_buffer(&parts.movie_video_src, &ready, true)
        .ok_or_else(|| error("Cannot block movie video"))?;
    let audio_probe = block_first_buffer(&parts.movie_audio_src, &ready, false)
        .ok_or_else(|| error("Cannot block movie audio"))?;
    let _video_rebase = add_timestamp_rebase_probe(&parts.movie_video_src, &ready, &bus, true)
        .ok_or_else(|| error("Cannot rebase movie video timestamps"))?;
    let _audio_rebase = add_timestamp_rebase_probe(&parts.movie_audio_src, &ready, &bus, false)
        .ok_or_else(|| error("Cannot rebase movie audio timestamps"))?;
    ready.0.lock().unwrap().blocking_probes = Some(BlockingProbes {
        video: Some((parts.movie_video_src.clone(), video_probe)),
        audio: Some((parts.movie_audio_src.clone(), audio_probe)),
    });

    let sinks = SelectedSinks {
        video: parts.movie_video_sink.clone(),
        audio: parts.movie_audio_sink.clone(),
        subtitle: parts.movie_subtitle_sink.clone(),
    };
    let selected = selection.clone();
    let pad_ready = ready.clone();
    let pad_sinks = sinks.clone();
    parts.movie.connect_pad_added(move |_, pad| {
        watch_movie_pad(pad, &pad_ready, &selected, &pad_sinks);
    });

    set_title(
        &config.title_token,
        &format!("Starting soon: {}", config.title),
    )?;
    parts.pipeline.set_state(gst::State::Playing)?;
    println!(
        "Lobby is live. Press Enter to start \"{}\"...",
        config.title
    );

    let input_ready = ready.clone();
    thread::spawn(move || {
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input_ready.0.lock().unwrap().enter_pressed = true;
        input_ready.1.notify_all();
    });

    let switch_ready = ready.clone();
    let switch_pipeline = parts.pipeline.clone();
    let video_selector = parts.video_selector.clone();
    let audio_selector = parts.audio_selector.clone();
    let movie_video_pad = parts.movie_video_pad.clone();
    let movie_audio_pad = parts.movie_audio_pad.clone();
    let switch_bus = bus.clone();
    let title = config.title.clone();
    let title_token = config.title_token.clone();
    let switch_flag = switched.clone();
    thread::spawn(move || {
        let mut state = switch_ready.0.lock().unwrap();
        while (state.video_pts.is_none() || state.audio_pts.is_none() || !state.enter_pressed)
            && state.failure.is_none()
        {
            state = switch_ready.1.wait(state).unwrap();
        }
        if state.failure.is_some() {
            drop(state);
            release_blocking_probes(&switch_ready);
            return;
        }
        drop(state);

        let (clock, base_time) = match pipeline_timing(&switch_pipeline) {
            Ok(timing) => timing,
            Err(failure) => {
                release_blocking_probes(&switch_ready);
                post_application_error(&switch_bus, "owncast-switch-error", failure);
                return;
            }
        };
        let boundary = next_frame_boundary(clock.time().saturating_sub(base_time));
        if let Err(failure) = configure_timestamp_rebase(&switch_ready, boundary) {
            release_blocking_probes(&switch_ready);
            post_application_error(&switch_bus, "owncast-switch-error", failure);
            return;
        }
        let clock_id = clock.new_single_shot_id(base_time + boundary);
        let callback_bus = switch_bus.clone();
        schedule_clock_callback(
            &clock_id,
            &switch_bus,
            &switch_ready,
            &switch_flag,
            move |_, _, _| {
                video_selector.set_property("active-pad", Some(&movie_video_pad));
                audio_selector.set_property("active-pad", Some(&movie_audio_pad));
            },
            move || {
                if let Err(failure) = set_title(&title_token, &title) {
                    post_application_error(&callback_bus, "owncast-title-error", failure);
                    return;
                }
                println!("Movie is live.");
            },
        );
    });

    let _signal = register_sigint(&bus)?;

    let context = gst::glib::MainContext::default();
    loop {
        while context.pending() {
            context.iteration(false);
        }
        let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        if handle_stream_collection_message(
            &message,
            config.subtitles.as_deref(),
            &parts.movie,
            &parts.pipeline,
            &parts.subtitle_overlay,
            &selection,
            &ready,
            &sinks,
        ) {
            continue;
        }
        match message.view() {
            gst::MessageView::Error(_) => {
                let failure = bus_error(&message).unwrap();
                if fatal_pipeline_error(&message, &parts.pipeline, switched.load(Ordering::Acquire))
                {
                    return Err(error(failure));
                }
                retain_lobby(&ready, failure);
            }
            gst::MessageView::Eos(_) => return Ok(()),
            gst::MessageView::Application(application) => {
                if let Some(failure) = application_failure(&message) {
                    return Err(error(failure));
                }
                let structure = application
                    .structure()
                    .ok_or_else(|| error("Application message has no structure"))?;
                if structure.name() == "owncast-interrupt" {
                    return Ok(());
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::glib::translate::FromGlib;
    use std::{
        fs,
        path::PathBuf,
        sync::mpsc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    static GST_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn gst_test() -> std::sync::MutexGuard<'static, ()> {
        let guard = GST_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gst::init().unwrap();
        guard
    }

    struct TestPipelineGuard(gst::Pipeline);

    impl Drop for TestPipelineGuard {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }

    fn linked_pads() -> (gst::Pad, gst::Pad) {
        let source = gst::Pad::new(gst::PadDirection::Src);
        let sink = gst::Pad::builder(gst::PadDirection::Sink)
            .chain_function(|_, _, _| Ok(gst::FlowSuccess::Ok))
            .build();
        source.set_active(true).unwrap();
        sink.set_active(true).unwrap();
        source.link(&sink).unwrap();
        source.push_event(gst::event::StreamStart::new("test"));
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        source.push_event(gst::event::Segment::new(segment.as_ref()));
        (source, sink)
    }

    #[test]
    fn first_buffer_probe_passes_startup_events_before_blocking_media() {
        let _gst = gst_test();
        let source = gst::Pad::new(gst::PadDirection::Src);
        let sink = gst::Pad::builder(gst::PadDirection::Sink)
            .chain_function(|_, _, _| Ok(gst::FlowSuccess::Ok))
            .build();
        source.set_active(true).unwrap();
        sink.set_active(true).unwrap();
        source.link(&sink).unwrap();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        let probe = block_first_buffer(&source, &ready, true).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if matches!(
                info.event().map(|event| event.view()),
                Some(gst::EventView::StreamStart(_) | gst::EventView::Segment(_))
            ) {
                event_tx.send(()).unwrap();
            }
            gst::PadProbeReturn::Ok
        })
        .unwrap();
        let (flow_tx, flow_rx) = mpsc::channel();
        let push_source = source.clone();
        std::thread::spawn(move || {
            assert!(push_source.push_event(gst::event::StreamStart::new("test")));
            let segment = gst::FormattedSegment::<gst::ClockTime>::new();
            assert!(push_source.push_event(gst::event::Segment::new(segment.as_ref())));
            let mut buffer = gst::Buffer::new();
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_seconds(7));
            flow_tx.send(push_source.push(buffer)).unwrap();
        });

        event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let state = ready
            .1
            .wait_timeout_while(ready.0.lock().unwrap(), Duration::from_secs(1), |state| {
                state.video_pts.is_none()
            })
            .unwrap()
            .0;
        assert_eq!(state.video_pts, Some(gst::ClockTime::from_seconds(7)));
        drop(state);
        assert!(matches!(
            flow_rx.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        source.remove_probe(probe);
        assert!(
            flow_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn rebase_probe_rewrites_every_movie_pts_and_dts_after_block_release() {
        let _gst = gst_test();
        let pipeline = gst::Pipeline::new();
        let bus = pipeline.bus().unwrap();
        let source = gst::Pad::new(gst::PadDirection::Src);
        let (buffer_tx, buffer_rx) = mpsc::channel();
        let sink = gst::Pad::builder(gst::PadDirection::Sink)
            .chain_function(move |_, _, buffer| {
                buffer_tx
                    .send((buffer.pts(), buffer.dts(), buffer.duration()))
                    .unwrap();
                Ok(gst::FlowSuccess::Ok)
            })
            .build();
        source.set_active(true).unwrap();
        sink.set_active(true).unwrap();
        source.link(&sink).unwrap();
        source.push_event(gst::event::StreamStart::new("movie"));
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        source.push_event(gst::event::Segment::new(segment.as_ref()));

        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        let blocker = block_first_buffer(&source, &ready, true).unwrap();
        let _rebase = add_timestamp_rebase_probe(&source, &ready, &bus, true).unwrap();
        let push_source = source.clone();
        std::thread::spawn(move || {
            for second in [0, 1] {
                let mut buffer = gst::Buffer::new();
                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_pts(gst::ClockTime::from_seconds(second));
                    buffer.set_dts(gst::ClockTime::from_mseconds(second * 1_000 + 900));
                    buffer.set_duration(gst::ClockTime::from_mseconds(20));
                }
                assert!(push_source.push(buffer).is_ok());
            }
        });

        let mut state = ready
            .1
            .wait_timeout_while(ready.0.lock().unwrap(), Duration::from_secs(1), |state| {
                state.video_pts.is_none()
            })
            .unwrap()
            .0;
        assert_eq!(state.video_pts, Some(gst::ClockTime::ZERO));
        state.audio_pts = Some(gst::ClockTime::ZERO);
        drop(state);
        configure_timestamp_rebase(&ready, gst::ClockTime::from_seconds(30)).unwrap();
        source.remove_probe(blocker);

        assert_eq!(
            buffer_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (
                Some(gst::ClockTime::from_seconds(30)),
                Some(gst::ClockTime::from_mseconds(30_900)),
                Some(gst::ClockTime::from_mseconds(20)),
            )
        );
        assert_eq!(
            buffer_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (
                Some(gst::ClockTime::from_seconds(31)),
                Some(gst::ClockTime::from_mseconds(31_900)),
                Some(gst::ClockTime::from_mseconds(20)),
            )
        );
    }

    #[test]
    fn timestamp_rebase_rejects_underflow_and_overflow() {
        assert!(rebase_timestamp(0, 0, 1).is_err());
        assert!(rebase_timestamp(u64::MAX, 1, 0).is_err());
    }

    #[test]
    fn timestamp_rebase_requires_both_first_timestamps() {
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        assert_eq!(
            configure_timestamp_rebase(&ready, gst::ClockTime::ZERO).unwrap_err(),
            "Movie video timestamp is missing"
        );
        ready.0.lock().unwrap().video_pts = Some(gst::ClockTime::ZERO);
        assert_eq!(
            configure_timestamp_rebase(&ready, gst::ClockTime::ZERO).unwrap_err(),
            "Movie audio timestamp is missing"
        );
    }

    #[test]
    fn audio_encoder_and_parser_stay_monotonic_across_handoff() {
        const TEST_TIMEOUT: Duration = Duration::from_secs(10);
        const FRAMES: u64 = 1024;

        fn audio_buffer(block: u64) -> gst::Buffer {
            let mut buffer =
                gst::Buffer::with_size(FRAMES as usize * 2 * size_of::<f32>()).unwrap();
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(gst::ClockTime::from_nseconds(
                    block * FRAMES * 1_000_000_000 / 48_000,
                ));
                buffer.set_dts(buffer.pts());
                buffer.set_duration(gst::ClockTime::from_nseconds(
                    FRAMES * 1_000_000_000 / 48_000,
                ));
            }
            buffer
        }

        let _gst = gst_test();
        let pipeline = gst::parse::launch(
            r#"
            appsrc name=source is-live=false block=false max-bytes=2097152 format=time
                caps=audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2
              ! queue name=source_branch
              ! audioconvert
              ! audioresample
              ! audio/x-raw,format=F32LE,rate=48000,channels=2
              ! audiocheblimit mode=high-pass cutoff=80 poles=4
              ! audiodynamic mode=compressor characteristics=soft-knee
                  threshold=0.125 ratio=2.0
              ! avenc_aac name=audio_encoder bitrate=192000
              ! aacparse name=audio_parser
              ! fakesink sync=false async=false
            "#,
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let (raw_tx, raw_rx) = mpsc::channel();
        pipeline
            .by_name("audio_encoder")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(pts) = info.buffer().and_then(|buffer| buffer.pts()) {
                    raw_tx.send(pts).unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        let (encoded_tx, encoded_rx) = mpsc::channel();
        pipeline
            .by_name("audio_parser")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(pts) = info.buffer().and_then(|buffer| buffer.pts()) {
                    encoded_tx.send(pts).unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        let _pipeline_guard = TestPipelineGuard(pipeline.clone());
        pipeline.set_state(gst::State::Playing).unwrap();
        pipeline
            .state(gst::ClockTime::from_seconds(TEST_TIMEOUT.as_secs()))
            .0
            .expect("audio handoff pipeline did not reach Playing within 10s");
        let source = pipeline.by_name("source").unwrap();
        let boundary = next_frame_boundary(
            audio_buffer(15).pts().unwrap()
                + gst::ClockTime::from_nseconds(FRAMES * 1_000_000_000 / 48_000),
        );
        for block in 0..16 {
            push_buffer(&source, audio_buffer(block));
        }
        for block in 0..64 {
            let mut buffer = audio_buffer(block);
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(gst::ClockTime::from_nseconds(
                    rebase_timestamp(buffer.pts().unwrap().nseconds(), boundary.nseconds(), 0)
                        .unwrap(),
                ));
                buffer.set_dts(buffer.pts());
            }
            push_buffer(&source, buffer);
        }
        end_stream(&source);
        let handoff_deadline = Instant::now() + TEST_TIMEOUT;
        let mut raw = Vec::new();
        while raw.len() < 80 {
            raw.push(
                raw_rx
                    .recv_timeout(handoff_deadline.saturating_duration_since(Instant::now()))
                    .expect("lobby and movie buffers did not reach the encoder within 10s"),
            );
        }
        let mut encoded = Vec::new();
        while encoded.iter().filter(|pts| **pts >= boundary).count() < 4 {
            encoded.push(
                encoded_rx
                    .recv_timeout(handoff_deadline.saturating_duration_since(Instant::now()))
                    .expect("four rebased movie buffers did not reach the parser within 10s"),
            );
        }
        pipeline.set_state(gst::State::Null).unwrap();
        pipeline
            .state(gst::ClockTime::from_seconds(TEST_TIMEOUT.as_secs()))
            .0
            .expect("audio handoff pipeline did not reach Null within 10s");

        assert!(
            encoded.iter().any(|pts| *pts < boundary),
            "parser emitted no lobby buffer before {boundary:?}: {encoded:?}"
        );
        assert!(raw.windows(2).all(|pair| pair[0] < pair[1]), "{raw:?}");
        assert!(
            encoded.windows(2).all(|pair| pair[0] < pair[1]),
            "{encoded:?}"
        );
        let raw_first = raw.into_iter().find(|pts| *pts >= boundary).unwrap();
        let encoded_first = encoded.into_iter().find(|pts| *pts >= boundary).unwrap();
        assert!(raw_first.nseconds().abs_diff(encoded_first.nseconds()) <= 50_000_000);
    }

    fn stream_collection_message(movie: &gst::Element) -> gst::Message {
        let collection = gst::StreamCollection::builder(None)
            .streams([
                gst::Stream::new(
                    Some("video"),
                    None,
                    gst::StreamType::VIDEO,
                    gst::StreamFlags::SELECT,
                ),
                gst::Stream::new(
                    Some("audio"),
                    None,
                    gst::StreamType::AUDIO,
                    gst::StreamFlags::SELECT,
                ),
            ])
            .build();
        gst::message::StreamCollection::builder(&collection)
            .src(movie)
            .build()
    }

    #[test]
    fn rebases_timestamp_to_boundary() {
        assert_eq!(
            rebase_timestamp(250_000_000, 30_000_000_000, 250_000_000).unwrap(),
            30_000_000_000
        );
    }

    #[test]
    fn formats_originating_bus_element() {
        let _gst = gst_test();
        let source = gst::ElementFactory::make("fakesrc")
            .name("broken-source")
            .build()
            .unwrap();
        let message = gst::message::Error::builder(gst::StreamError::Failed, "decode failed")
            .src(&source)
            .debug("fixture failure")
            .build();
        assert!(bus_error(&message).unwrap().contains("broken-source"));
    }

    #[test]
    fn rounds_handoff_up_to_next_frame() {
        assert_eq!(
            next_frame_boundary(gst::ClockTime::from_nseconds(33_333_333)),
            gst::ClockTime::from_nseconds(66_666_666)
        );
    }

    #[test]
    fn maps_stream_identity_type_and_default_flag() {
        let _gst = gst_test();
        let stream = gst::Stream::new(
            Some("video-1"),
            None,
            gst::StreamType::VIDEO,
            gst::StreamFlags::SELECT,
        );

        assert_eq!(
            candidate(&stream),
            Some(crate::media::StreamCandidate {
                id: "video-1".into(),
                kind: crate::media::StreamKind::Video,
                language: None,
                is_default: true,
                is_sdh: false,
            })
        );
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
    fn links_stream_start_that_precedes_selection() {
        let _gst = gst_test();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        let selection = Arc::new(OnceLock::new());
        let video_src = gst::Pad::builder(gst::PadDirection::Src)
            .name("video-src")
            .build();
        let video_sink = gst::Pad::builder(gst::PadDirection::Sink)
            .name("video-sink")
            .build();
        let audio_sink = gst::Pad::builder(gst::PadDirection::Sink)
            .name("audio-sink")
            .build();
        let subtitle_sink = gst::Pad::builder(gst::PadDirection::Sink)
            .name("subtitle-sink")
            .build();
        let sinks = SelectedSinks {
            video: video_sink.clone(),
            audio: audio_sink,
            subtitle: subtitle_sink,
        };

        watch_movie_pad(&video_src, &ready, &selection, &sinks);
        video_src.set_active(true).unwrap();
        video_src.push_event(gst::event::StreamStart::new("video"));
        selection
            .set(Selection {
                video_id: "video".into(),
                audio_id: "audio".into(),
                subtitle: SubtitleSource::None,
            })
            .unwrap();
        resolve_pending_pads(&ready, &selection, &sinks);

        assert_eq!(video_src.peer(), Some(video_sink));
        assert!(ready.0.lock().unwrap().failure.is_none());
    }

    #[test]
    fn embedded_subtitle_links_through_movie_bin() {
        let _gst = gst_test();
        let config = Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: None,
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };
        let parts = PipelineParts::build_with_sink(&config, "fakesink").unwrap();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        let selection = OnceLock::new();
        selection
            .set(Selection {
                video_id: "video".into(),
                audio_id: "audio".into(),
                subtitle: SubtitleSource::Embedded("subtitle".into()),
            })
            .unwrap();
        let subtitle_src = gst::Pad::new(gst::PadDirection::Src);
        let sinks = SelectedSinks {
            video: parts.movie_video_sink.clone(),
            audio: parts.movie_audio_sink.clone(),
            subtitle: parts.movie_subtitle_sink.clone(),
        };
        ready.0.lock().unwrap().pending_pads.push(PendingPad {
            pad: subtitle_src.clone(),
            stream_id: Some("subtitle".into()),
        });

        resolve_pending_pads(&ready, &selection, &sinks);

        assert_eq!(subtitle_src.peer(), Some(parts.movie_subtitle_sink.clone()));
        assert!(ready.0.lock().unwrap().failure.is_none());
    }

    #[test]
    fn external_subtitle_links_through_movie_bin() {
        let _gst = gst_test();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let subtitle = std::env::temp_dir().join(format!("owncast-subtitle-{suffix}.srt"));
        fs::write(&subtitle, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        let config = Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: Some(subtitle.clone()),
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };
        let parts = PipelineParts::build_with_sink(&config, "fakesink").unwrap();

        assert!(parts.movie_subtitle_sink.peer().is_none());
        add_external_subtitles(&parts.pipeline, &parts.movie_subtitle_sink, &subtitle).unwrap();

        assert!(parts.movie_subtitle_sink.peer().is_some());
        assert!(
            parts
                .pipeline
                .by_name("movie_external_subtitles")
                .unwrap()
                .downcast::<gst::Bin>()
                .unwrap()
                .by_name("source")
                .unwrap()
                .factory()
                .is_some_and(|factory| factory.name() == "filesrc")
        );
        fs::remove_file(subtitle).unwrap();
    }

    #[test]
    fn keeps_lobby_for_pre_handoff_branch_error() {
        let _gst = gst_test();
        let pipeline = gst::Pipeline::new();
        let movie_branch = gst::ElementFactory::make("fakesrc")
            .name("movie_video")
            .build()
            .unwrap();
        let output = gst::ElementFactory::make("fakesink")
            .name("output")
            .build()
            .unwrap();
        pipeline.add_many([&movie_branch, &output]).unwrap();
        let branch_error = gst::message::Error::builder(gst::StreamError::Failed, "preroll failed")
            .src(&movie_branch)
            .build();
        let output_error = gst::message::Error::builder(gst::StreamError::Failed, "publish failed")
            .src(&output)
            .build();

        assert!(!fatal_pipeline_error(&branch_error, &pipeline, false));
        assert!(fatal_pipeline_error(&output_error, &pipeline, false));
        assert!(fatal_pipeline_error(&branch_error, &pipeline, true));
    }

    #[test]
    fn parsers_and_post_encoder_queues_are_fatal_before_handoff() {
        let _gst = gst_test();
        let config = Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: None,
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };
        let parts = PipelineParts::build_with_sink(&config, "fakesink").unwrap();

        for name in [
            "video_parser",
            "video_output_queue",
            "audio_parser",
            "audio_output_queue",
        ] {
            let source = parts.pipeline.by_name(name).unwrap();
            let message = gst::message::Error::builder(gst::StreamError::Failed, "common failed")
                .src(&source)
                .build();
            assert!(
                fatal_pipeline_error(&message, &parts.pipeline, false),
                "{name} must be fatal"
            );
        }
    }

    #[test]
    fn selected_pad_linked_to_different_sink_keeps_lobby() {
        let _gst = gst_test();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        let selection = OnceLock::new();
        selection
            .set(Selection {
                video_id: "video".into(),
                audio_id: "audio".into(),
                subtitle: SubtitleSource::None,
            })
            .unwrap();
        let video_src = gst::Pad::new(gst::PadDirection::Src);
        let occupied_sink = gst::Pad::new(gst::PadDirection::Sink);
        video_src.link(&occupied_sink).unwrap();
        let sinks = SelectedSinks {
            video: gst::Pad::new(gst::PadDirection::Sink),
            audio: gst::Pad::new(gst::PadDirection::Sink),
            subtitle: gst::Pad::new(gst::PadDirection::Sink),
        };
        ready.0.lock().unwrap().pending_pads.push(PendingPad {
            pad: video_src,
            stream_id: Some("video".into()),
        });

        resolve_pending_pads(&ready, &selection, &sinks);

        assert!(
            ready
                .0
                .lock()
                .unwrap()
                .failure
                .as_deref()
                .is_some_and(|failure| failure.contains("Cannot link selected stream video"))
        );
    }

    #[test]
    fn records_recoverable_handoff_failure() {
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));

        retain_lobby(&ready, "subtitle failed");

        assert_eq!(
            ready.0.lock().unwrap().failure.as_deref(),
            Some("subtitle failed")
        );
    }

    #[test]
    fn rejects_failed_sigint_registration() {
        unsafe extern "C" fn reject_registration(
            _: i32,
            _: i32,
            _: unsafe extern "C" fn(*mut c_void) -> i32,
            _: *mut c_void,
            _: unsafe extern "C" fn(*mut c_void),
        ) -> u32 {
            0
        }

        let _gst = gst_test();
        assert!(register_sigint_with(&gst::Bus::new(), reject_registration).is_err());
    }

    #[test]
    fn signal_guard_removes_registered_source() {
        let _gst = gst_test();
        let context = gst::glib::MainContext::default();
        let guard = register_sigint(&gst::Bus::new()).unwrap();
        let source_id = unsafe { gst::glib::SourceId::from_glib(guard.id) };
        assert!(context.find_source_by_id(&source_id).is_some());

        drop(guard);

        assert!(context.find_source_by_id(&source_id).is_none());
    }

    #[test]
    fn sigint_callback_does_not_panic_when_bus_rejects_message() {
        let _gst = gst_test();
        let removed = Arc::new(AtomicBool::new(false));
        let bus = gst::Bus::new();
        bus.set_flushing(true);
        let data = Box::into_raw(Box::new(SignalData {
            bus,
            removed: removed.clone(),
        }))
        .cast();

        assert_eq!(unsafe { post_interrupt(data) }, 0);
        assert!(removed.load(Ordering::Acquire));
        unsafe { drop_signal_bus(data) };
    }

    #[test]
    fn surfaces_title_and_clock_application_failures() {
        let _gst = gst_test();
        for (name, failure) in [
            ("owncast-title-error", "title failed"),
            ("owncast-switch-error", "clock failed"),
        ] {
            let message = gst::message::Application::builder(
                gst::Structure::builder(name)
                    .field("error", failure)
                    .build(),
            )
            .build();
            assert_eq!(application_failure(&message).as_deref(), Some(failure));
        }
    }

    #[test]
    fn missing_pipeline_clock_and_base_time_are_reported() {
        let _gst = gst_test();
        let pipeline = gst::Pipeline::new();
        assert_eq!(
            pipeline_timing(&pipeline).unwrap_err(),
            "Pipeline clock is missing"
        );
        assert_eq!(
            require_timing(Some(gst::SystemClock::obtain().upcast()), None).unwrap_err(),
            "Pipeline base time is missing"
        );
    }

    #[test]
    fn rejected_clock_callback_releases_real_blocking_probes() {
        let _gst = gst_test();
        let (video_src, _video_sink) = linked_pads();
        let (audio_src, _audio_sink) = linked_pads();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        ready.0.lock().unwrap().blocking_probes = Some(BlockingProbes {
            video: Some((
                video_src.clone(),
                video_src
                    .add_probe(
                        gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::BUFFER,
                        |_, _| gst::PadProbeReturn::Ok,
                    )
                    .unwrap(),
            )),
            audio: Some((
                audio_src.clone(),
                audio_src
                    .add_probe(
                        gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::BUFFER,
                        |_, _| gst::PadProbeReturn::Ok,
                    )
                    .unwrap(),
            )),
        });
        let clock = gst::SystemClock::obtain();
        let clock_id = clock.new_single_shot_id(clock.time() + gst::ClockTime::from_seconds(5));
        clock_id.unschedule();
        let bus = gst::Bus::new();
        let switched = Arc::new(AtomicBool::new(false));

        assert!(!schedule_clock_callback(
            &clock_id,
            &bus,
            &ready,
            &switched,
            |_, _, _| {},
            || {},
        ));
        assert!(!switched.load(Ordering::Acquire));
        let failure = bus.timed_pop(gst::ClockTime::ZERO).unwrap();
        assert!(
            application_failure(&failure)
                .unwrap()
                .contains("Cannot schedule movie handoff")
        );
        let (sent, received) = mpsc::channel();
        std::thread::spawn(move || {
            sent.send((
                video_src.push(gst::Buffer::new()),
                audio_src.push(gst::Buffer::new()),
            ))
            .unwrap();
        });
        let (video, audio) = received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(video.is_ok());
        assert!(audio.is_ok());
    }

    #[test]
    fn successful_clock_callback_switches_releases_probes_then_runs_title_work() {
        let _gst = gst_test();
        let config = Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: None,
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };
        let parts = PipelineParts::build_with_sink(&config, "fakesink").unwrap();
        let (video_src, _video_sink) = linked_pads();
        let (audio_src, _audio_sink) = linked_pads();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let video_blocked = blocked_tx.clone();
        let video_probe = video_src
            .add_probe(
                gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::BUFFER,
                move |_, _| {
                    video_blocked.send(()).unwrap();
                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();
        let audio_probe = audio_src
            .add_probe(
                gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::BUFFER,
                move |_, _| {
                    blocked_tx.send(()).unwrap();
                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        ready.0.lock().unwrap().blocking_probes = Some(BlockingProbes {
            video: Some((video_src.clone(), video_probe)),
            audio: Some((audio_src.clone(), audio_probe)),
        });
        let (flow_tx, flow_rx) = mpsc::channel();
        for source in [video_src, audio_src] {
            let flow_tx = flow_tx.clone();
            std::thread::spawn(move || {
                flow_tx.send(source.push(gst::Buffer::new())).unwrap();
            });
        }
        blocked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        blocked_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let clock = gst::SystemClock::obtain();
        let clock_id = clock.new_single_shot_id(clock.time());
        let callback_ready = ready.clone();
        let video_selector = parts.video_selector.clone();
        let audio_selector = parts.audio_selector.clone();
        let movie_video_pad = parts.movie_video_pad.clone();
        let movie_audio_pad = parts.movie_audio_pad.clone();
        let (order_tx, order_rx) = mpsc::channel();
        let title_ready = ready.clone();
        let switched = Arc::new(AtomicBool::new(false));
        let title_switched = switched.clone();
        let (title_tx, title_rx) = mpsc::channel();
        assert!(schedule_clock_callback(
            &clock_id,
            &gst::Bus::new(),
            &ready,
            &switched,
            move |_, _, _| {
                let video_was_blocked = callback_ready.0.lock().unwrap().blocking_probes.is_some();
                video_selector.set_property("active-pad", Some(&movie_video_pad));
                let audio_was_blocked = callback_ready.0.lock().unwrap().blocking_probes.is_some();
                audio_selector.set_property("active-pad", Some(&movie_audio_pad));
                let both_were_blocked = callback_ready.0.lock().unwrap().blocking_probes.is_some();
                order_tx
                    .send((video_was_blocked, audio_was_blocked, both_were_blocked))
                    .unwrap();
            },
            move || {
                title_tx
                    .send((
                        title_ready.0.lock().unwrap().blocking_probes.is_some(),
                        title_switched.load(Ordering::Acquire),
                    ))
                    .unwrap();
            },
        ));

        let (video_was_blocked, audio_was_blocked, both_were_blocked) =
            order_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            video_was_blocked && audio_was_blocked && both_were_blocked,
            "both selector assignments must precede probe release"
        );
        let (title_was_blocked, switched_before_title) =
            title_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            switched_before_title,
            "shared switched state must precede probe release"
        );
        assert!(!title_was_blocked, "probe release must precede title work");
        assert!(
            flow_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        assert!(
            flow_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn stream_setup_rejections_retain_lobby() {
        let _gst = gst_test();
        let pipeline = gst::Pipeline::new();
        let movie = gst::ElementFactory::make("fakesrc").build().unwrap();
        let overlay = gst::ElementFactory::make("subtitleoverlay")
            .build()
            .unwrap();
        pipeline.add_many([&movie, &overlay]).unwrap();
        let ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        let selection = Arc::new(OnceLock::new());
        let sinks = SelectedSinks {
            video: gst::Pad::new(gst::PadDirection::Sink),
            audio: gst::Pad::new(gst::PadDirection::Sink),
            subtitle: overlay.static_pad("subtitle_sink").unwrap(),
        };
        pipeline
            .bus()
            .unwrap()
            .post(stream_collection_message(&movie))
            .unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop(gst::ClockTime::ZERO)
            .unwrap();

        handle_stream_collection_message(
            &message, None, &movie, &pipeline, &overlay, &selection, &ready, &sinks,
        );

        assert_eq!(
            ready.0.lock().unwrap().failure.as_deref(),
            Some("Movie rejected selected streams")
        );
    }

    #[test]
    fn external_subtitle_link_failure_is_reported() {
        let _gst = gst_test();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let subtitle = std::env::temp_dir().join(format!("owncast-subtitle-{suffix}.srt"));
        fs::write(&subtitle, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        let pipeline = gst::Pipeline::new();
        let overlay = gst::ElementFactory::make("subtitleoverlay")
            .build()
            .unwrap();
        let occupied = gst::ElementFactory::make("fakesrc").build().unwrap();
        pipeline.add_many([&overlay, &occupied]).unwrap();
        occupied
            .static_pad("src")
            .unwrap()
            .link(&overlay.static_pad("subtitle_sink").unwrap())
            .unwrap();

        assert!(
            add_external_subtitles(
                &pipeline,
                &overlay.static_pad("subtitle_sink").unwrap(),
                &subtitle
            )
            .is_err()
        );
        fs::remove_file(subtitle).unwrap();
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
    fn pipeline_has_one_sink_and_starts_on_lobby() {
        let _gst = gst_test();
        let config = Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: None,
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };

        let parts = PipelineParts::build_with_sink(&config, "fakesink").unwrap();
        let gain = parts
            .pipeline
            .by_name("audio_gain")
            .expect("Audio gain element is missing");
        assert!((gain.property::<f64>("volume") - 1.4125375).abs() < 0.000001);
        parts.pipeline.remove(&parts.movie).unwrap();
        let movie_video =
            gst::parse::bin_from_description("videotestsrc is-live=true pattern=white", true)
                .unwrap();
        let movie_audio =
            gst::parse::bin_from_description("audiotestsrc is-live=true wave=ticks", true).unwrap();
        parts.pipeline.add(&movie_video).unwrap();
        parts.pipeline.add(&movie_audio).unwrap();
        movie_video
            .static_pad("src")
            .unwrap()
            .link(&parts.movie_video_sink)
            .unwrap();
        movie_audio
            .static_pad("src")
            .unwrap()
            .link(&parts.movie_audio_sink)
            .unwrap();
        parts.pipeline.set_state(gst::State::Playing).unwrap();
        parts
            .pipeline
            .state(gst::ClockTime::from_seconds(1))
            .0
            .unwrap();

        assert_eq!(parts.pipeline.iterate_sinks().into_iter().count(), 1);
        assert_eq!(
            parts
                .video_selector
                .property::<gst::Pad>("active-pad")
                .name(),
            "sink_0"
        );
        assert_eq!(
            parts
                .audio_selector
                .property::<gst::Pad>("active-pad")
                .name(),
            "sink_0"
        );
        parts.pipeline.set_state(gst::State::Null).unwrap();
        parts
            .pipeline
            .state(gst::ClockTime::from_seconds(10))
            .0
            .unwrap();
    }

    fn appsrc(name: &str, caps: gst::Caps) -> gst::Element {
        gst::ElementFactory::make("appsrc")
            .name(name)
            .property("caps", caps)
            .property("format", gst::Format::Time)
            .property("block", true)
            .build()
            .unwrap()
    }

    fn push_buffer(source: &gst::Element, mut buffer: gst::Buffer) {
        assert_eq!(
            source.emit_by_name::<gst::FlowReturn>("push-buffer", &[&mut buffer]),
            gst::FlowReturn::Ok
        );
    }

    fn end_stream(source: &gst::Element) {
        assert_eq!(
            source.emit_by_name::<gst::FlowReturn>("end-of-stream", &[]),
            gst::FlowReturn::Ok
        );
    }

    fn write_synthetic_flv(path: &std::path::Path) {
        const WIDTH: usize = 320;
        const HEIGHT: usize = 180;
        const FRAME_NS: u64 = 1_000_000_000 / 30;
        const BEEP_START_NS: u64 = 1_000_000_000;

        let config = Config {
            video: PathBuf::from("/tmp/synthetic-movie.mkv"),
            subtitles: None,
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };
        let parts = PipelineParts::build_with_sink(&config, "filesink").unwrap();
        parts.pipeline.remove(&parts.movie).unwrap();
        parts
            .pipeline
            .by_name("output")
            .unwrap()
            .set_property("location", path);
        parts
            .pipeline
            .by_name("mux")
            .unwrap()
            .set_property("streamable", false);
        for element in parts.pipeline.iterate_elements() {
            let element = element.unwrap();
            if element
                .factory()
                .is_some_and(|factory| factory.name() == "textoverlay")
            {
                element.set_property("text", "");
            }
        }

        let video = appsrc(
            "synthetic_video",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", WIDTH as i32)
                .field("height", HEIGHT as i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        );
        let audio = appsrc(
            "synthetic_audio",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48_000_i32)
                .field("channels", 2_i32)
                .build(),
        );
        let subtitles = appsrc(
            "synthetic_subtitles",
            gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build(),
        );
        parts
            .pipeline
            .add_many([&video, &audio, &subtitles])
            .unwrap();
        video
            .static_pad("src")
            .unwrap()
            .link(&parts.movie_video_sink)
            .unwrap();
        audio
            .static_pad("src")
            .unwrap()
            .link(&parts.movie_audio_sink)
            .unwrap();
        subtitles
            .link(
                &parts
                    .subtitle_overlay
                    .parent()
                    .unwrap()
                    .downcast::<gst::Element>()
                    .unwrap(),
            )
            .unwrap();
        let (lobby_tx, lobby_rx) = mpsc::channel();
        let (audio_eos_tx, audio_eos_rx) = mpsc::channel();
        for queue in ["video_output_queue", "audio_output_queue"] {
            let lobby_tx = lobby_tx.clone();
            let seen = Arc::new(AtomicBool::new(false));
            parts
                .pipeline
                .by_name(queue)
                .unwrap()
                .static_pad("src")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if !seen.swap(true, Ordering::Relaxed) && info.buffer().is_some() {
                        lobby_tx.send(()).unwrap();
                    }
                    gst::PadProbeReturn::Ok
                })
                .unwrap();
        }
        parts
            .pipeline
            .by_name("audio_output_queue")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if matches!(
                    info.event().map(|event| event.view()),
                    Some(gst::EventView::Eos(_))
                ) {
                    audio_eos_tx.send(()).unwrap();
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        let rebase_ready = Arc::new((Mutex::new(ReadyState::default()), Condvar::new()));
        {
            let mut state = rebase_ready.0.lock().unwrap();
            state.video_pts = Some(gst::ClockTime::ZERO);
            state.audio_pts = Some(gst::ClockTime::ZERO);
        }
        let bus = parts.pipeline.bus().unwrap();
        let _video_rebase =
            add_timestamp_rebase_probe(&parts.movie_video_src, &rebase_ready, &bus, true).unwrap();
        let _audio_rebase =
            add_timestamp_rebase_probe(&parts.movie_audio_src, &rebase_ready, &bus, false).unwrap();

        parts.pipeline.set_state(gst::State::Playing).unwrap();
        for _ in 0..2 {
            lobby_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        }
        let (clock, base_time) = pipeline_timing(&parts.pipeline).unwrap();
        let boundary = next_frame_boundary(clock.time() - base_time);
        configure_timestamp_rebase(&rebase_ready, boundary).unwrap();
        parts
            .video_selector
            .set_property("active-pad", Some(&parts.movie_video_pad));
        parts
            .audio_selector
            .set_property("active-pad", Some(&parts.movie_audio_pad));

        let mut subtitle = gst::Buffer::from_mut_slice(b"SYNC".to_vec());
        {
            let subtitle = subtitle.get_mut().unwrap();
            subtitle.set_pts(gst::ClockTime::from_seconds(1));
            subtitle.set_duration(gst::ClockTime::from_mseconds(700));
        }
        push_buffer(&subtitles, subtitle);
        end_stream(&subtitles);

        thread::scope(|scope| {
            scope.spawn(|| {
                for frame in 0..60 {
                    let y = if frame < 30 { 16 } else { 80 };
                    let mut buffer = gst::Buffer::with_size(WIDTH * HEIGHT * 3 / 2).unwrap();
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(gst::ClockTime::from_nseconds(frame * FRAME_NS));
                        buffer.set_duration(gst::ClockTime::from_nseconds(FRAME_NS));
                        let mut map = buffer.map_writable().unwrap();
                        let pixels = map.as_mut_slice();
                        pixels[..WIDTH * HEIGHT].fill(y);
                        pixels[WIDTH * HEIGHT..].fill(128);
                    }
                    push_buffer(&video, buffer);
                }
            });
            scope.spawn(|| {
                const AUDIO_FRAMES: usize = 96_000;
                const AUDIO_BLOCK: usize = 1024;
                for start in (0..AUDIO_FRAMES).step_by(AUDIO_BLOCK) {
                    let frames = AUDIO_BLOCK.min(AUDIO_FRAMES - start);
                    let mut buffer = gst::Buffer::with_size(frames * 2 * size_of::<f32>()).unwrap();
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(gst::ClockTime::from_nseconds(
                            start as u64 * 1_000_000_000 / 48_000,
                        ));
                        buffer.set_duration(gst::ClockTime::from_nseconds(
                            frames as u64 * 1_000_000_000 / 48_000,
                        ));
                        let mut map = buffer.map_writable().unwrap();
                        for (sample, bytes) in map.as_mut_slice().chunks_exact_mut(4).enumerate() {
                            let frame = start + sample / 2;
                            let value = if frame as u64 * 1_000_000_000 / 48_000 >= BEEP_START_NS {
                                (2.0 * std::f32::consts::PI * 1_000.0 * frame as f32 / 48_000.0)
                                    .sin()
                                    * 0.5
                            } else {
                                0.0
                            };
                            bytes.copy_from_slice(&value.to_le_bytes());
                        }
                    }
                    push_buffer(&audio, buffer);
                }
            });
        });
        end_stream(&audio);
        audio_eos_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        end_stream(&video);

        let bus = parts.pipeline.bus().unwrap();
        loop {
            let message = bus.timed_pop(gst::ClockTime::from_seconds(30)).unwrap();
            match message.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(error) => panic!(
                    "synthetic encode failed: {} ({})",
                    error.error(),
                    error.debug().unwrap_or_default()
                ),
                _ => {}
            }
        }
    }

    #[derive(Default)]
    struct SyncPoints {
        video: Option<gst::ClockTime>,
        audio: Option<gst::ClockTime>,
        subtitle: Option<gst::ClockTime>,
    }

    fn decode_sync_points(path: &std::path::Path) -> SyncPoints {
        let pipeline = gst::parse::launch(&format!(
            r#"
            uridecodebin uri="{}" name=decode
            decode. ! queue ! videoconvert ! videoscale
              ! video/x-raw,format=GRAY8,width=320,height=180
              ! fakesink name=video_sink sync=false
            decode. ! queue ! audioconvert ! audioresample
              ! audio/x-raw,format=F32LE,rate=48000,channels=1
              ! fakesink name=audio_sink sync=false
            "#,
            gst::glib::filename_to_uri(path, None).unwrap()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let points = Arc::new(Mutex::new(SyncPoints::default()));
        let video_points = points.clone();
        pipeline
            .by_name("video_sink")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(buffer) = info.buffer() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(pts) = buffer.pts() else {
                    return gst::PadProbeReturn::Ok;
                };
                let map = buffer.map_readable().unwrap();
                let pixels = map.as_slice();
                let mut points = video_points.lock().unwrap();
                if points.video.is_none() && pixels.first().is_some_and(|pixel| *pixel > 40) {
                    points.video = Some(pts);
                }
                if points.subtitle.is_none() && pixels.iter().any(|pixel| *pixel > 180) {
                    points.subtitle = Some(pts);
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        let audio_points = points.clone();
        pipeline
            .by_name("audio_sink")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(buffer) = info.buffer() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(pts) = buffer.pts() else {
                    return gst::PadProbeReturn::Ok;
                };
                let map = buffer.map_readable().unwrap();
                if let Some(sample) = map
                    .as_slice()
                    .chunks_exact(4)
                    .position(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()).abs() > 0.05)
                {
                    let mut points = audio_points.lock().unwrap();
                    points.audio.get_or_insert(
                        pts + gst::ClockTime::from_nseconds(sample as u64 * 1_000_000_000 / 48_000),
                    );
                }
                gst::PadProbeReturn::Ok
            })
            .unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        loop {
            let message = bus.timed_pop(gst::ClockTime::from_seconds(30)).unwrap();
            match message.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(error) => panic!(
                    "synthetic decode failed: {} ({})",
                    error.error(),
                    error.debug().unwrap_or_default()
                ),
                _ => {}
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        drop(pipeline);
        Arc::into_inner(points).unwrap().into_inner().unwrap()
    }

    #[test]
    #[ignore = "encodes and decodes a synthetic 1080p FLV"]
    fn synthetic_handoff_stays_within_50ms() {
        let _gst = gst_test();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = std::env::temp_dir().join(format!("owncast-sync-{suffix}.flv"));
        write_synthetic_flv(&output);
        let points = decode_sync_points(&output);
        let video_pts = points.video.expect("changed video frame");
        let audio_pts = points.audio.expect("audio beep");
        let subtitle_pts = points.subtitle.expect("subtitle overlay");

        assert!(video_pts.nseconds().abs_diff(audio_pts.nseconds()) <= 50_000_000);
        assert!(video_pts.nseconds().abs_diff(subtitle_pts.nseconds()) <= 50_000_000);
        fs::remove_file(output).unwrap();
    }
}
