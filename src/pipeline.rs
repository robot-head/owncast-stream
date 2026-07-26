use gst::prelude::*;
use gstreamer as gst;
use std::error::Error;

use crate::{Config, error};

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
    pub(crate) movie_video_sink: gst::Pad,
    pub(crate) movie_audio_sink: gst::Pad,
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
            movie_video_sink: movie_video_sink.upcast(),
            movie_audio_sink: audio_bin
                .static_pad("sink")
                .ok_or_else(|| error("Movie audio input is missing"))?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
