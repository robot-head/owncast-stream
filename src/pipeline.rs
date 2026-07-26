use gst::glib::translate::IntoGlib;
use gst::prelude::*;
use gstreamer as gst;
use std::{
    env,
    error::Error,
    ffi::c_void,
    io,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use crate::{
    Config, error,
    media::{Selection, StreamCandidate, StreamKind, SubtitleSource, select_streams},
    set_title,
};

fn running_time_offset(boundary: gst::ClockTime, first_pts: gst::ClockTime) -> i64 {
    boundary.nseconds() as i64 - first_pts.nseconds() as i64
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
}

unsafe extern "C" fn post_interrupt(data: *mut c_void) -> i32 {
    // SAFETY: GLib calls this with the boxed bus passed to g_unix_signal_add_full.
    let bus = unsafe { &*(data.cast::<gst::Bus>()) };
    bus.post(
        gst::message::Application::builder(gst::Structure::builder("owncast-interrupt").build())
            .build(),
    )
    .expect("pipeline bus accepts interrupt");
    0
}

unsafe extern "C" fn drop_signal_bus(data: *mut c_void) {
    // SAFETY: GLib invokes destroy notify exactly once for this Box::into_raw pointer.
    drop(unsafe { Box::from_raw(data.cast::<gst::Bus>()) });
}

fn register_sigint(bus: &gst::Bus) {
    let data = Box::into_raw(Box::new(bus.clone())).cast();
    // SAFETY: callbacks use the boxed GstBus until GLib calls the destroy notifier.
    unsafe {
        g_unix_signal_add_full(
            gst::glib::Priority::DEFAULT.into_glib(),
            2,
            post_interrupt,
            data,
            drop_signal_bus,
        );
    }
}

const REQUIRED_ELEMENTS: &[&str] = &[
    "uridecodebin3",
    "videotestsrc",
    "audiotestsrc",
    "textoverlay",
    "subtitleoverlay",
    "subparse",
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
              ! x264enc bitrate=6000 key-int-max=60 bframes=0
                  tune=zerolatency speed-preset=medium
              ! h264parse config-interval=1
              ! queue
              ! mux.

            input-selector name=audio_selector sync-streams=true
                sync-mode=clock cache-buffers=true drop-backwards=true
              ! audioconvert
              ! audioresample
              ! audio/x-raw,format=F32LE,rate=48000,channels=2
              ! audiocheblimit mode=high-pass cutoff=80 poles=4
              ! audiodynamic mode=compressor characteristics=soft-knee
                  threshold=0.125 ratio=2.0
              ! avenc_aac bitrate=192000
              ! aacparse
              ! queue
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

        let video_bin = gst::parse::bin_from_description(
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
        let audio_bin = gst::parse::bin_from_description("queue max-size-buffers=8", true)?;
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
            movie_video_src: video_bin
                .static_pad("src")
                .ok_or_else(|| error("Movie video output is missing"))?,
            movie_audio_src: audio_bin
                .static_pad("src")
                .ok_or_else(|| error("Movie audio output is missing"))?,
            subtitle_overlay: video_bin
                .by_name("movie_subtitles")
                .ok_or_else(|| error("Subtitle overlay is missing"))?,
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
    enter_pressed: bool,
    failure: Option<String>,
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

fn add_external_subtitles(
    pipeline: &gst::Pipeline,
    overlay: &gst::Element,
    path: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    let subtitles =
        gst::parse::bin_from_description("filesrc name=source ! subparse ! queue", true)?;
    subtitles
        .by_name("source")
        .ok_or_else(|| error("External subtitle source is missing"))?
        .set_property("location", path);
    pipeline.add(&subtitles)?;
    subtitles
        .static_pad("src")
        .ok_or_else(|| error("External subtitle output is missing"))?
        .link(
            &overlay
                .static_pad("subtitle_sink")
                .ok_or_else(|| error("Subtitle overlay input is missing"))?,
        )?;
    subtitles.sync_state_with_parent()?;
    Ok(())
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

    let video_ready = ready.clone();
    let video_probe = parts
        .movie_video_src
        .add_probe(
            gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::BUFFER,
            move |_, info| {
                record_first_pts(&video_ready, true, info);
                gst::PadProbeReturn::Ok
            },
        )
        .ok_or_else(|| error("Cannot block movie video"))?;
    let audio_ready = ready.clone();
    let audio_probe = parts
        .movie_audio_src
        .add_probe(
            gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::BUFFER,
            move |_, info| {
                record_first_pts(&audio_ready, false, info);
                gst::PadProbeReturn::Ok
            },
        )
        .ok_or_else(|| error("Cannot block movie audio"))?;

    let selected = selection.clone();
    let video_sink = parts.movie_video_sink.clone();
    let audio_sink = parts.movie_audio_sink.clone();
    let subtitle_sink = parts
        .subtitle_overlay
        .static_pad("subtitle_sink")
        .ok_or_else(|| error("Subtitle overlay input is missing"))?;
    parts.movie.connect_pad_added(move |_, pad| {
        let Some(stream_id) = pad
            .sticky_event::<gst::event::StreamStart>(0)
            .map(|event| event.stream_id().to_owned())
        else {
            return;
        };
        let Some(selected) = selected.get() else {
            return;
        };
        let sink = if stream_id == selected.video_id {
            &video_sink
        } else if stream_id == selected.audio_id {
            &audio_sink
        } else if matches!(
            &selected.subtitle,
            SubtitleSource::Embedded(id) if stream_id == id.as_str()
        ) {
            &subtitle_sink
        } else {
            return;
        };
        if let Err(failure) = pad.link(sink) {
            eprintln!("Cannot link selected stream {stream_id}: {failure}");
        }
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
    let movie_video_src = parts.movie_video_src.clone();
    let movie_audio_src = parts.movie_audio_src.clone();
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
            return;
        }
        let video_pts = state.video_pts.unwrap();
        let audio_pts = state.audio_pts.unwrap();
        drop(state);

        let Some(clock) = switch_pipeline.clock() else {
            return;
        };
        let Some(base_time) = switch_pipeline.base_time() else {
            return;
        };
        let boundary = next_frame_boundary(clock.time().saturating_sub(base_time));
        movie_video_pad.set_offset(running_time_offset(boundary, video_pts));
        movie_audio_pad.set_offset(running_time_offset(boundary, audio_pts));
        let clock_id = clock.new_single_shot_id(base_time + boundary);
        let _ = clock_id.wait_async(move |_, _, _| {
            video_selector.set_property("active-pad", Some(&movie_video_pad));
            audio_selector.set_property("active-pad", Some(&movie_audio_pad));
            movie_video_src.remove_probe(video_probe);
            movie_audio_src.remove_probe(audio_probe);
            switch_flag.store(true, Ordering::Release);
            if let Err(failure) = set_title(&title_token, &title) {
                let structure = gst::Structure::builder("owncast-title-error")
                    .field("error", failure.to_string())
                    .build();
                let _ = switch_bus.post(gst::message::Application::builder(structure).build());
                return;
            }
            println!("Movie is live.");
        });
    });

    register_sigint(&bus);

    let context = gst::glib::MainContext::default();
    loop {
        while context.pending() {
            context.iteration(false);
        }
        let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match message.view() {
            gst::MessageView::StreamCollection(collection)
                if selection.get().is_none()
                    && message
                        .src()
                        .is_some_and(|source| source.has_as_ancestor(&parts.movie)) =>
            {
                let candidates: Vec<_> = collection
                    .stream_collection()
                    .iter()
                    .filter_map(|stream| candidate(&stream))
                    .collect();
                match select_streams(&candidates, config.subtitles.as_deref()) {
                    Ok(chosen) => {
                        match &chosen.subtitle {
                            SubtitleSource::External(path) => {
                                add_external_subtitles(
                                    &parts.pipeline,
                                    &parts.subtitle_overlay,
                                    path,
                                )?;
                            }
                            SubtitleSource::None => {
                                parts.subtitle_overlay.set_property("silent", true);
                            }
                            SubtitleSource::Embedded(_) => {}
                        }
                        selection
                            .set(chosen)
                            .map_err(|_| error("Movie streams were selected twice"))?;
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
                        if !parts.movie.send_event(gst::event::SelectStreams::new(ids)) {
                            return Err(error("Movie rejected selected streams"));
                        }
                    }
                    Err(reason) => {
                        eprintln!("{reason}; the lobby will remain live until Ctrl-C.");
                        ready.0.lock().unwrap().failure = Some(reason);
                        ready.1.notify_all();
                    }
                }
            }
            gst::MessageView::Error(_) => {
                let failure = bus_error(&message).unwrap();
                let from_movie = message
                    .src()
                    .is_some_and(|source| source.has_as_ancestor(&parts.movie));
                if from_movie && !switched.load(Ordering::Acquire) {
                    eprintln!(
                        "Movie decoder stopped before handoff; the lobby will remain live until Ctrl-C.\n{failure}"
                    );
                    ready.0.lock().unwrap().failure = Some(failure);
                    ready.1.notify_all();
                } else {
                    return Err(error(failure));
                }
            }
            gst::MessageView::Eos(_) => return Ok(()),
            gst::MessageView::Application(application) => {
                let structure = application
                    .structure()
                    .ok_or_else(|| error("Application message has no structure"))?;
                match structure.name().as_str() {
                    "owncast-interrupt" => return Ok(()),
                    "owncast-title-error" => {
                        return Err(error(
                            structure
                                .get::<String>("error")
                                .unwrap_or_else(|_| "Cannot update Owncast title".into()),
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rebases_first_movie_pts_to_boundary() {
        assert_eq!(
            running_time_offset(
                gst::ClockTime::from_seconds(30),
                gst::ClockTime::from_mseconds(250)
            ),
            29_750_000_000
        );
    }

    #[test]
    fn formats_originating_bus_element() {
        gst::init().unwrap();
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
        gst::init().unwrap();
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
    fn preflight_lists_every_missing_element() {
        gst::init().unwrap();
        assert_eq!(
            missing_elements(&["fakesink", "owncast-element-that-does-not-exist"]),
            vec!["owncast-element-that-does-not-exist"]
        );
    }

    #[test]
    fn pipeline_has_one_sink_and_starts_on_lobby() {
        gst::init().unwrap();
        let config = Config {
            video: PathBuf::from("/tmp/movie.mkv"),
            subtitles: None,
            title: String::new(),
            stream_key: String::new(),
            title_token: String::new(),
        };

        let parts = PipelineParts::build_with_sink(&config, "fakesink").unwrap();
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
    }
}
