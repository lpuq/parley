//! GStreamer pipelines for capture/encode (send) and decode/playback (recv),
//! plus PipeWire app-audio routing and the screencast portal.

pub mod appaudio;
pub mod devices;
pub mod portal;
pub mod recv;
pub mod send;

use anyhow::{Result, anyhow};
use gst::prelude::*;

/// Name shared by the echo-cancel probe (on playback) and webrtcdsp (on the mic).
pub const ECHO_PROBE: &str = "parley-echo-probe";

pub fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| anyhow!("GStreamer element \"{factory}\" is missing (install the matching gstreamer1-plugins package)"))
}

pub fn has(factory: &str) -> bool {
    gst::ElementFactory::find(factory).is_some()
}

pub fn caps_filter(caps: &str) -> Result<gst::Element> {
    let caps: gst::Caps = caps.parse().map_err(|_| anyhow!("bad caps {caps}"))?;
    Ok(gst::ElementFactory::make("capsfilter").property("caps", caps).build()?)
}

/// Sets a property only if the element has it; enums/flags may be given by nick.
pub fn set_opt(el: &gst::Element, name: &str, value: &str) {
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, value);
    }
}

/// Keeps a pipeline's bus watch alive and stops the pipeline on drop.
///
/// State changes run on GStreamer's thread pool: PulseAudio elements block
/// while connecting, and that must never freeze the UI.
pub struct Running {
    pub pipeline: gst::Pipeline,
    _watch: gst::bus::BusWatchGuard,
    /// Resources (e.g. the portal's PipeWire fd) released only after the pipeline stops.
    keep: Vec<Box<dyn std::any::Any + Send>>,
}

impl Running {
    /// Starts the pipeline, reporting errors through `on_error` (called on the GTK thread).
    pub fn start(pipeline: gst::Pipeline, label: &str, on_error: impl Fn(String) + 'static) -> Result<Self> {
        let bus = pipeline.bus().expect("pipeline bus");
        let label = label.to_string();
        let weak = pipeline.downgrade();
        let watch = bus.add_watch_local(move |_, msg| {
            // An element's latency changed (e.g. the adaptive jitter buffer): redistribute
            // it, or sinks keep the old budget and treat delayed audio as late.
            if let gst::MessageView::Latency(_) = msg.view() {
                if let Some(p) = weak.upgrade() {
                    p.call_async(|p| {
                        let _ = p.recalculate_latency();
                    });
                }
            }
            if let gst::MessageView::Error(e) = msg.view() {
                let src = msg.src().map(|s| s.path_string().to_string()).unwrap_or_default();
                tracing::error!("{label}: {} ({src}) {:?}", e.error(), e.debug());
                if let Some(p) = weak.upgrade() {
                    p.call_async(|p| {
                        let _ = p.set_state(gst::State::Null);
                    });
                }
                on_error(format!("{label}: {}", e.error()));
            }
            gst::glib::ControlFlow::Continue
        })?;
        pipeline.call_async(|p| {
            if p.set_state(gst::State::Playing).is_err() {
                // Elements post the specific error on the bus.
                tracing::warn!("{} failed to start", p.name());
            }
        });
        Ok(Self { pipeline, _watch: watch, keep: Vec::new() })
    }

    pub fn keep_alive(&mut self, v: impl std::any::Any + Send) {
        self.keep.push(Box::new(v));
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let keep = std::mem::take(&mut self.keep);
        self.pipeline.call_async(move |p| {
            let _ = p.set_state(gst::State::Null);
            drop(keep);
        });
    }
}
