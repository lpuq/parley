//! Screen-share audio routing (PipeWire on Linux; not on Windows yet).
//!
//! While sharing audio we create a private virtual sink and link the chosen
//! apps' output ports into it (they keep playing to the speakers too). The
//! screen-audio encoder records that sink's monitor. Streams owned by our own
//! process are never linked, so friends never hear themselves echoed back.

#[derive(Clone, Debug, PartialEq)]
pub enum AudioShare {
    Off,
    /// Everything playing on the desktop except Parley itself.
    Desktop,
    /// Only these applications (by `application.name`).
    Apps(Vec<String>),
}

pub enum PwEvent {
    /// Applications currently playing audio (excluding us).
    Apps(Vec<String>),
    /// The virtual sink exists and can be recorded from.
    SinkReady,
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::AppAudio;

/// Without PipeWire there's no app-audio routing (yet): sharing sends no sound.
#[cfg(not(target_os = "linux"))]
pub struct AppAudio {
    pub sink_name: String,
    pub default_source: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(not(target_os = "linux"))]
impl AppAudio {
    pub fn start(_events: async_channel::Sender<PwEvent>) -> Option<Self> {
        None
    }

    pub fn set(&self, _mode: AudioShare) {}
}
