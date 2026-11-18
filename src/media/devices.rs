//! Microphone, speaker, camera (and, on Windows, screen) enumeration.

use gst::prelude::*;

#[derive(Clone)]
pub struct Device {
    pub name: String,
    pub device: gst::Device,
}

fn provider_devices(provider: &str, class: &str) -> Option<Vec<gst::Device>> {
    let f = gst::DeviceProviderFactory::find(provider)?;
    let p = f.get()?;
    let devs: Vec<gst::Device> = p.devices().into_iter().filter(|d| d.has_classes(class)).collect();
    Some(devs)
}

fn prop_str(d: &gst::Device, key: &str) -> Option<String> {
    d.properties().and_then(|s| s.get::<String>(key).ok())
}

/// Prefer the PulseAudio API (served by pipewire-pulse): it's the most robust
/// path for live audio in GStreamer. Fall back to native PipeWire; on Windows, WASAPI.
fn audio_devices(class: &str) -> Vec<Device> {
    let devs = ["pulsedeviceprovider", "pipewiredeviceprovider", "wasapi2deviceprovider"]
        .iter()
        .find_map(|p| provider_devices(p, class).filter(|v| !v.is_empty()))
        .unwrap_or_default();
    devs.into_iter()
        .filter(|d| prop_str(d, "device.class").as_deref() != Some("monitor"))
        .filter(|d| !d.display_name().contains("Parley"))
        .map(|d| Device { name: d.display_name().to_string(), device: d })
        .collect()
}

pub fn microphones() -> Vec<Device> {
    audio_devices("Audio/Source")
}

pub fn speakers() -> Vec<Device> {
    audio_devices("Audio/Sink")
}

pub fn cameras() -> Vec<Device> {
    // Linux: V4L2 (or PipeWire); Windows: Media Foundation (or kernel streaming).
    let devs = ["v4l2deviceprovider", "pipewiredeviceprovider", "mfdeviceprovider", "ksdeviceprovider"]
        .iter()
        .find_map(|p| provider_devices(p, "Video/Source").filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let mut out: Vec<Device> = Vec::new();
    for d in devs {
        // Skip metadata/ISP nodes that expose no usable video caps.
        let usable = d.caps().is_some_and(|c| {
            c.iter().any(|s| s.name() == "video/x-raw" || s.name() == "image/jpeg")
        });
        if !usable {
            continue;
        }
        let name = d.display_name().to_string();
        if out.iter().any(|o| o.name == name) {
            continue;
        }
        out.push(Device { name, device: d });
    }
    out
}

/// PipeWire/pulse node name of a device, if known.
pub fn node_name(d: &Device) -> Option<String> {
    prop_str(&d.device, "node.name")
}

/// Bluetooth headsets: using their microphone switches them to low-quality headset mode.
pub fn is_bluetooth(d: &Device) -> bool {
    prop_str(&d.device, "device.api").as_deref() == Some("bluez5")
        || node_name(d).is_some_and(|n| n.starts_with("bluez_"))
}

pub fn find<'a>(list: &'a [Device], name: &str) -> Option<&'a Device> {
    if name.is_empty() {
        return None;
    }
    list.iter().find(|d| d.name == name)
}

/// Picks the best camera mode: up to 1280x720 at >= 30 fps, preferring raw
/// formats unless only MJPEG reaches the frame rate.
pub fn best_camera_caps(dev: &gst::Device) -> Option<gst::Caps> {
    let caps = dev.caps()?;
    let mut best: Option<(i64, gst::Structure)> = None;
    for (s, features) in caps.iter_with_features() {
        let name = s.name().as_str();
        if name != "video/x-raw" && name != "image/jpeg" {
            continue;
        }
        // Only plain system-memory modes; DMABuf/DMA_DRM needs a GL/VA path we don't build.
        let sysmem = features.is_empty() || features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);
        if !sysmem || s.get::<&str>("format").is_ok_and(|f| f == "DMA_DRM") {
            continue;
        }
        let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else { continue };
        let fps = max_fps(s);
        if fps < 10.0 {
            continue;
        }
        let hs = h.min(720) as i64;
        let over = if h > 720 { (h - 720) as i64 } else { 0 };
        let score = hs * 1000 + (fps.min(30.0) as i64) * 20 - over
            + if name == "video/x-raw" { 5 } else { 0 };
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            let mut st = gst::Structure::builder(name).field("width", w).field("height", h).build();
            if let Some(f) = pick_fps(s, 30.0) {
                st.set("framerate", f);
            }
            if name == "video/x-raw" {
                if let Ok(fmt) = s.value("format") {
                    st.set_value("format", fmt.clone());
                }
            }
            best = Some((score, st));
        }
    }
    best.map(|(_, s)| gst::Caps::builder_full().structure(s).build())
}

fn fractions(s: &gst::StructureRef) -> Vec<gst::Fraction> {
    let Ok(v) = s.value("framerate") else { return vec![] };
    if let Ok(f) = v.get::<gst::Fraction>() {
        return vec![f];
    }
    if let Ok(l) = v.get::<gst::List>() {
        return l.iter().filter_map(|x| x.get::<gst::Fraction>().ok()).collect();
    }
    if let Ok(r) = v.get::<gst::FractionRange>() {
        return vec![r.max(), r.min()];
    }
    vec![]
}

fn fps_of(f: &gst::Fraction) -> f64 {
    if f.denom() == 0 { 0.0 } else { f.numer() as f64 / f.denom() as f64 }
}

fn max_fps(s: &gst::StructureRef) -> f64 {
    fractions(s).iter().map(fps_of).fold(0.0, f64::max)
}

fn pick_fps(s: &gst::StructureRef, target: f64) -> Option<gst::Fraction> {
    let fr = fractions(s);
    // Closest to target from above, else the fastest available.
    fr.iter()
        .filter(|f| fps_of(f) >= target - 0.5)
        .min_by(|a, b| fps_of(a).partial_cmp(&fps_of(b)).unwrap())
        .or_else(|| fr.iter().max_by(|a, b| fps_of(a).partial_cmp(&fps_of(b)).unwrap()))
        .copied()
}

/// Windows: the screens GStreamer can capture (Direct3D 11).
#[cfg(windows)]
pub fn monitors() -> Vec<Device> {
    provider_devices("d3d11screencapturedeviceprovider", "Source/Monitor")
        .unwrap_or_default()
        .into_iter()
        .map(|d| Device { name: d.display_name().to_string(), device: d })
        .collect()
}

/// A screen's size in pixels, from the capture device's caps.
pub fn monitor_size(d: &gst::Device) -> Option<(i32, i32)> {
    let caps = d.caps()?;
    let s = caps.structure(0)?;
    Some((s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?))
}
