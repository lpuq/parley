//! Capture → encode → [`Hub`] pipelines.

use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use gst::prelude::*;
use iroh::EndpointId;

use super::{
    ECHO_PROBE, Running, caps_filter, has, make, set_opt,
    voice::{VoiceParams, VoiceProcessor},
};
use crate::{
    hub::{Hub, SPEAKING_THRESHOLD_DB, VideoFrame, rms_dbfs},
    proto::Track,
};

pub const RTP_MTU: u32 = 1100;

// ---------------------------------------------------------------- audio

pub struct MicOptions {
    pub device: Option<gst::Device>,
    /// Requires the playback pipeline's echo probe to be running.
    pub echo_cancel: bool,
    /// WebRTC's (classic) noise suppression.
    pub webrtc_ns: bool,
    /// Neural suppression, voice gate and gain; adjustable while running.
    pub voice: Arc<VoiceParams>,
}

pub struct MicSender {
    _run: Running,
    valve: gst::Element,
}

impl MicSender {
    pub fn start(
        hub: Arc<Hub>,
        me: EndpointId,
        opts: MicOptions,
        muted: bool,
        on_error: impl Fn(String) + 'static,
    ) -> Result<Self> {
        let pipeline = gst::Pipeline::with_name("mic");
        let src = match &opts.device {
            // Test hook: a steady 440 Hz tone instead of the microphone.
            _ if std::env::var_os("PARLEY_TEST_TONE").is_some() => {
                let t = make("audiotestsrc")?;
                t.set_property("is-live", true);
                let f = std::env::var("PARLEY_TEST_TONE_FREQ").ok().and_then(|v| v.parse().ok()).unwrap_or(440.0f64);
                t.set_property("freq", f);
                t.set_property("volume", 0.3f64);
                t
            }
            Some(d) => d.create_element(Some("micsrc"))?,
            None if has("pulsesrc") => make("pulsesrc")?,
            None => make("autoaudiosrc")?,
        };
        // Small capture buffers keep mouth-to-ear latency down.
        set_opt(&src, "buffer-time", "40000");
        set_opt(&src, "latency-time", "10000");

        let conv = make("audioconvert")?;
        let res = make("audioresample")?;
        let caps = caps_filter("audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1")?;
        let mut chain: Vec<gst::Element> = vec![src, conv, res, caps];

        if (opts.echo_cancel || opts.webrtc_ns) && has("webrtcdsp") {
            let dsp = make("webrtcdsp")?;
            dsp.set_property("echo-cancel", opts.echo_cancel);
            dsp.set_property("noise-suppression", opts.webrtc_ns);
            set_opt(&dsp, "noise-suppression-level", "high");
            // Our own gain control (media/voice.rs) follows; two would fight.
            dsp.set_property("gain-control", false);
            set_opt(&dsp, "probe", ECHO_PROBE);
            set_opt(&dsp, "delay-agnostic", "true");
            set_opt(&dsp, "extended-filter", "true");
            set_opt(&dsp, "high-pass-filter", "true");
            chain.push(dsp);
            chain.push(make("audioconvert")?);
        }

        // Voice processing (see media/voice.rs) works on 48 kHz mono S16.
        let vcaps = caps_filter("audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1")?;
        let proc_ = std::sync::Mutex::new(VoiceProcessor::new(opts.voice.clone()));
        vcaps.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
                let b = buffer.make_mut();
                if let Ok(mut map) = b.map_writable() {
                    let bytes = map.as_mut_slice();
                    let (pre, samples, _) = unsafe { bytes.align_to_mut::<i16>() };
                    if pre.is_empty() {
                        proc_.lock().unwrap().process(samples);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        chain.push(vcaps);

        let valve = make("valve")?;
        valve.set_property("drop", muted);
        let enc = make("opusenc")?;
        enc.set_property("bitrate", 64000i32);
        set_opt(&enc, "audio-type", "voice");
        set_opt(&enc, "frame-size", "20");
        set_opt(&enc, "inband-fec", "true");
        set_opt(&enc, "packet-loss-percentage", "10");
        let pay = make("rtpopuspay")?;
        pay.set_property("mtu", RTP_MTU);
        set_opt(&pay, "pt", "96");
        let sink = rtp_appsink(hub.clone(), Track::Mic);

        chain.extend([valve.clone(), enc, pay, sink.upcast()]);
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;

        // Local "speaking" indicator (still shown while muted, like Discord's).
        let cell = hub.speaking_cell(me, Track::Mic);
        let hub2 = hub.clone();
        valve.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                if let Ok(map) = buf.map_readable() {
                    if rms_dbfs(&map) > SPEAKING_THRESHOLD_DB {
                        cell.store(hub2.epoch.now_ms(), Ordering::Relaxed);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        tracing::info!(
            "microphone: {} (echo cancel {}, webrtc suppression {}, neural suppression {}, gate {:?} at {} dB, auto gain {})",
            opts.device.as_ref().map(|d| d.display_name().to_string()).unwrap_or("system default".into()),
            opts.echo_cancel,
            opts.webrtc_ns,
            opts.voice.denoise.load(Ordering::Relaxed),
            opts.voice.gate(),
            opts.voice.threshold_db(),
            opts.voice.auto_gain.load(Ordering::Relaxed)
        );
        let run = Running::start(pipeline, "Microphone", on_error)?;
        Ok(Self { _run: run, valve })
    }

    pub fn set_muted(&self, muted: bool) {
        self.valve.set_property("drop", muted);
    }
}

/// Captures the monitor of our virtual "screen share audio" sink.
pub struct ScreenAudioSender {
    _run: Running,
}

impl ScreenAudioSender {
    pub fn start(hub: Arc<Hub>, sink_node_name: &str, on_error: impl Fn(String) + 'static) -> Result<Self> {
        let pipeline = gst::Pipeline::with_name("screen-audio");
        let src = if has("pulsesrc") {
            let s = make("pulsesrc")?;
            s.set_property("device", format!("{sink_node_name}.monitor"));
            s.set_property("client-name", "Parley screen audio");
            s
        } else {
            let s = make("pipewiresrc")?;
            s.set_property("target-object", sink_node_name);
            let props = gst::Structure::builder("props").field("stream.capture.sink", true).build();
            s.set_property("stream-properties", props);
            s
        };
        set_opt(&src, "buffer-time", "40000");
        set_opt(&src, "latency-time", "10000");
        let chain = [
            src,
            make("audioconvert")?,
            make("audioresample")?,
            caps_filter("audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2")?,
            {
                let enc = make("opusenc")?;
                enc.set_property("bitrate", 128000i32);
                set_opt(&enc, "audio-type", "generic");
                set_opt(&enc, "frame-size", "20");
                set_opt(&enc, "inband-fec", "true");
                enc
            },
            {
                let pay = make("rtpopuspay")?;
                pay.set_property("mtu", RTP_MTU);
                set_opt(&pay, "pt", "96");
                pay
            },
            rtp_appsink(hub, Track::ScreenAudio).upcast(),
        ];
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;
        // Diagnostics: how much sound we capture, every 10 s.
        tracing::info!("screen audio: capturing {sink_node_name}.monitor");
        let stats = Arc::new(std::sync::Mutex::new((0u64, 0f64, Instant::now())));
        chain[4].static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                if let Ok(map) = buf.map_readable() {
                    let mut g = stats.lock().unwrap();
                    g.0 += 1;
                    g.1 = g.1.max(rms_dbfs(&map));
                    if g.2.elapsed() >= std::time::Duration::from_secs(10) {
                        tracing::info!("screen audio: {} buffers captured in 10 s, loudest {:.0} dBFS", g.0, g.1);
                        *g = (0, -100.0, Instant::now());
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        Ok(Self { _run: Running::start(pipeline, "Screen audio", on_error)? })
    }
}

fn rtp_appsink(hub: Arc<Hub>, track: Track) -> gst_app::AppSink {
    let sink = gst_app::AppSink::builder().sync(false).max_buffers(8).drop(true).build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    if let Ok(map) = buf.map_readable() {
                        hub.send_audio(track, &map);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    sink
}

// ---------------------------------------------------------------- video

pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub hw: bool,
}

/// Hardware encoders can be listed yet fail on a given GPU/driver, so each is
/// test-encoded once (a few frames) before we trust it. Cached for the process.
fn hw_encoder_works(name: &str) -> bool {
    use std::{collections::HashMap, sync::Mutex};
    static CACHE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    if let Some(v) = CACHE.lock().unwrap().get_or_insert_default().get(name) {
        return *v;
    }
    let ok = (|| -> Option<bool> {
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers=10 ! video/x-raw,format=NV12,width=640,height=360,framerate=30/1 ! {name} ! h264parse ! fakesink"
        ))
        .ok()?;
        p.set_state(gst::State::Playing).ok()?;
        let bus = p.bus()?;
        let msg = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let _ = p.set_state(gst::State::Null);
        Some(matches!(msg.as_ref().map(|m| m.view()), Some(gst::MessageView::Eos(_))))
    })()
    .unwrap_or(false);
    tracing::info!("hardware encoder {name}: {}", if ok { "usable" } else { "not usable" });
    CACHE.lock().unwrap().get_or_insert_default().insert(name.to_string(), ok);
    ok
}

/// Probes hardware encoders in the background so the first camera/screen share doesn't wait.
pub fn warm_up_encoders() {
    std::thread::spawn(|| {
        for name in ["vah264lpenc", "vah264enc", "nvh264enc", "nvcudah264enc", "vaapih264enc"] {
            if has(name) {
                hw_encoder_works(name);
            }
        }
    });
}

/// Picks and configures an H.264 encoder tuned for low latency at a steady bitrate.
fn h264_encoder(p: &VideoParams) -> Result<(gst::Element, &'static str)> {
    let gop = (p.fps * 4).max(30);
    if p.hw {
        for name in ["vah264lpenc", "vah264enc"] {
            if has(name) && hw_encoder_works(name) {
                let e = make(name)?;
                set_opt(&e, "rate-control", "cbr");
                set_opt(&e, "bitrate", &p.kbps.to_string());
                set_opt(&e, "key-int-max", &gop.to_string());
                set_opt(&e, "b-frames", "0");
                set_opt(&e, "target-usage", "7");
                return Ok((e, "NV12"));
            }
        }
        for name in ["nvh264enc", "nvcudah264enc"] {
            if has(name) && hw_encoder_works(name) {
                let e = make(name)?;
                set_opt(&e, "rc-mode", "cbr");
                set_opt(&e, "bitrate", &p.kbps.to_string());
                set_opt(&e, "gop-size", &gop.to_string());
                set_opt(&e, "bframes", "0");
                set_opt(&e, "zerolatency", "true");
                set_opt(&e, "preset", "p1");
                set_opt(&e, "tune", "ultra-low-latency");
                return Ok((e, "NV12"));
            }
        }
        if has("vaapih264enc") && hw_encoder_works("vaapih264enc") {
            let e = make("vaapih264enc")?;
            set_opt(&e, "rate-control", "cbr");
            set_opt(&e, "bitrate", &p.kbps.to_string());
            set_opt(&e, "keyframe-period", &gop.to_string());
            set_opt(&e, "max-bframes", "0");
            return Ok((e, "NV12"));
        }
    }
    if has("x264enc") {
        let e = make("x264enc")?;
        e.set_property_from_str("tune", "zerolatency");
        e.set_property_from_str("speed-preset", if p.fps > 30 || p.height > 1080 { "superfast" } else { "veryfast" });
        e.set_property("bitrate", p.kbps);
        e.set_property("key-int-max", gop);
        e.set_property("bframes", 0u32);
        // ~1 frame of VBV keeps the rate steady and latency low.
        e.set_property("vbv-buf-capacity", (1000 / p.fps.max(1)).max(100) * 3);
        set_opt(&e, "b-adapt", "false");
        return Ok((e, "I420"));
    }
    if has("openh264enc") {
        let e = make("openh264enc")?;
        set_opt(&e, "bitrate", &(p.kbps * 1000).to_string());
        set_opt(&e, "rate-control", "bitrate");
        set_opt(&e, "gop-size", &gop.to_string());
        set_opt(&e, "usage-type", "screen");
        return Ok((e, "I420"));
    }
    Err(anyhow!("No H.264 encoder found. Install gstreamer1-plugins-ugly (x264) or openh264."))
}

/// Adaptive bitrate: smoothness first. When frames back up on the way out (the link
/// can't carry the bitrate), the encoder's bitrate drops at once, so the picture gets
/// softer instead of stuttering; once the link has been clear for a while it climbs
/// back towards the chosen quality. Runs until the encoder is gone.
fn adapt_bitrate(enc: &gst::Element, hub: Arc<Hub>, track: Track, max_kbps: u32) {
    // openh264enc counts bits per second; the others kbit/s.
    let scale = if enc.factory().is_some_and(|f| f.name() == "openh264enc") { 1000 } else { 1 };
    let weak = enc.downgrade();
    let _ = std::thread::Builder::new().name("bitrate".into()).spawn(move || {
        let mut ctl = BitrateControl::new(max_kbps);
        let idx = track.video_index();
        hub.congestion[idx].take();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let Some(enc) = weak.upgrade() else { return };
            let (delay, drops) = hub.congestion[idx].take();
            if let Some((from, to)) = ctl.step(delay, drops) {
                tracing::info!("{track:?}: bitrate {from} → {to} kbps (send delay {delay} ms, {drops} dropped)");
                set_opt(&enc, "bitrate", &(to * scale).to_string());
            }
        }
    });
}

/// The adaptive-bitrate decisions, fed every 500 ms with the worst send delay and the
/// number of dropped frames.
struct BitrateControl {
    max: f64,
    min: f64,
    cur: f64,
    applied: f64,
    calm: u32,
    /// Where congestion last struck: climb quickly up to just below it, then only
    /// creep, so we don't keep overshooting into dropped frames (a stutter each time).
    ceiling: f64,
}

impl BitrateControl {
    fn new(max_kbps: u32) -> Self {
        let max = max_kbps as f64;
        Self { max, min: (max * 0.15).max(300.0), cur: max, applied: max, calm: 0, ceiling: f64::MAX }
    }

    /// Returns (old, new) kbps when the encoder should change.
    fn step(&mut self, delay_ms: u32, drops: u32) -> Option<(u32, u32)> {
        if drops > 0 || delay_ms > 200 {
            self.ceiling = self.cur * 0.95;
            self.cur *= 0.6;
            self.calm = 0;
        } else if delay_ms > 80 {
            self.ceiling = self.cur * 0.95;
            self.cur *= 0.85;
            self.calm = 0;
        } else if delay_ms < 40 {
            self.calm += 1;
            if self.calm >= 4 {
                self.cur *= if self.cur < self.ceiling * 0.9 { 1.05 } else { 1.005 };
            }
            // After half a minute without trouble, the link may have improved.
            if self.calm % 60 == 0 && self.ceiling < f64::MAX {
                self.ceiling *= 1.05;
            }
        }
        self.cur = self.cur.clamp(self.min, self.max);
        if (self.cur - self.applied).abs() / self.applied > 0.03 {
            let from = self.applied as u32;
            self.applied = self.cur;
            return Some((from, self.cur as u32));
        }
        None
    }
}

#[cfg(test)]
mod bitrate_tests {
    use super::BitrateControl;

    /// A link carrying `cap` kbps: excess bitrate builds a queue (delay); past ~150 ms
    /// the send queue overflows and frames are dropped (a visible stutter).
    fn simulate(max: u32, cap: f64, secs: u32) -> (u32, f64) {
        let mut c = BitrateControl::new(max);
        let (mut backlog_ms, mut drops_total, mut applied) = (0.0f64, 0u32, max as f64);
        let mut tail = Vec::new();
        for tick in 0..secs * 2 {
            backlog_ms = (backlog_ms + (applied - cap) / cap * 500.0).max(0.0);
            let drops = if backlog_ms > 150.0 {
                backlog_ms = 0.0;
                1
            } else {
                0
            };
            drops_total += drops;
            if let Some((_, to)) = c.step(backlog_ms as u32, drops) {
                applied = to as f64;
            }
            if tick >= secs {
                tail.push(applied);
            }
        }
        (drops_total, tail.iter().sum::<f64>() / tail.len() as f64)
    }

    #[test]
    fn settles_near_link_capacity_without_repeated_stutters() {
        let (drops, avg) = simulate(6800, 2500.0, 300);
        eprintln!("2.5 Mbps link: {drops} stutters in 5 min, average {avg:.0} kbps");
        assert!(drops <= 6, "{drops} stutters in 5 minutes");
        assert!(avg > 0.75 * 2500.0 && avg < 2500.0, "average {avg} kbps on a 2500 kbps link");
    }

    #[test]
    fn full_quality_on_a_fast_link() {
        let (drops, avg) = simulate(6800, 50_000.0, 60);
        assert_eq!(drops, 0);
        assert_eq!(avg as u32, 6800);
    }
}

/// Builds `[raw video] → tee → (preview, encoder → appsink)` and returns the tee's input
/// element plus the preview paintable.
fn video_tail(
    pipeline: &gst::Pipeline,
    hub: Arc<Hub>,
    track: Track,
    p: &VideoParams,
) -> Result<(gst::Element, gtk::gdk::Paintable)> {
    let (enc, fmt) = h264_encoder(p)?;
    tracing::info!("{track:?}: encoder {} {}x{}@{} {}kbps", enc.factory().unwrap().name(), p.width, p.height, p.fps, p.kbps);
    adapt_bitrate(&enc, hub.clone(), track, p.kbps);

    let scale = make("videoconvertscale").or_else(|_| make("videoscale"))?;
    set_opt(&scale, "add-borders", "true");
    let conv = make("videoconvert")?;
    let caps = caps_filter(&format!(
        "video/x-raw,format={fmt},width={},height={},pixel-aspect-ratio=1/1",
        p.width, p.height
    ))?;
    // Constant output rate, applied last so repeated frames cost only a cheap encode.
    // Screencasts arrive at a variable rate (and not at all while the screen is still);
    // a fixed rate keeps the bitrate steady and gives late joiners a picture.
    let rate = make("videorate")?;
    let rate_caps = caps_filter(&format!("video/x-raw,framerate={}/1", p.fps))?;
    let tee = make("tee")?;

    // Preview branch: small, leaky, never blocks encoding.
    let q1 = leaky_queue(1)?;
    let pconv = make("videoconvertscale").or_else(|_| make("videoscale"))?;
    let pcaps = caps_filter(&format!("video/x-raw,width={},height={}", 640, (640 * p.height / p.width.max(1)) & !1))?;
    let pconv2 = make("videoconvert")?;
    let prgb = caps_filter("video/x-raw,format={BGRA,RGBA,BGRx,RGBx}")?;
    let psink = make("gtk4paintablesink")?;
    psink.set_property("sync", false);
    let paintable = psink.property::<gtk::gdk::Paintable>("paintable");

    // Encode branch.
    let q2 = leaky_queue(2)?;
    watch_luma(&q2, format!("{track:?} sent"));
    let parse = make("h264parse")?;
    parse.set_property("config-interval", -1i32);
    let ecaps = caps_filter("video/x-h264,stream-format=byte-stream,alignment=au")?;
    let sink = gst_app::AppSink::builder().sync(false).max_buffers(4).drop(false).build();
    let hub2 = hub.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    if let Ok(map) = buf.map_readable() {
                        let key = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
                        let pts_us = buf.pts().map(|t| t.useconds()).unwrap_or(0);
                        hub2.send_video(
                            track,
                            VideoFrame { data: Bytes::copy_from_slice(&map), pts_us, key, captured: Instant::now() },
                        );
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.add_many([
        &scale, &conv, &caps, &rate, &rate_caps, &tee, &q1, &pconv, &pcaps, &pconv2, &prgb, &psink, &q2, &enc, &parse,
        &ecaps, sink.upcast_ref(),
    ])?;
    gst::Element::link_many([&scale, &conv, &caps, &rate, &rate_caps, &tee])?;
    gst::Element::link_many([&tee, &q1, &pconv, &pcaps, &pconv2, &prgb, &psink])?;
    gst::Element::link_many([&tee, &q2, &enc, &parse, &ecaps, sink.upcast_ref()])?;

    // Keyframe requests (new peer, congestion recovery) → force an IDR upstream of the appsink.
    let weak = sink.downgrade();
    hub.set_keyframe_handler(
        track,
        Some(Box::new(move || {
            if let Some(s) = weak.upgrade() {
                let ev = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
                s.send_event(ev);
            }
        })),
    );
    Ok((scale, paintable))
}

/// Logs frame count and average brightness now and then: makes "black video"
/// reports diagnosable from the log (all-zero frames vs. no frames at all).
pub fn watch_luma(el: &gst::Element, label: String) {
    let pad = el.static_pad("src").unwrap();
    let count = std::sync::atomic::AtomicU64::new(0);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let n = count.fetch_add(1, Ordering::Relaxed) + 1;
        if matches!(n, 1 | 2 | 5 | 30) || n % 300 == 0 {
            if let Some(buf) = info.buffer() {
                if let Ok(map) = buf.map_readable() {
                    // First half of the buffer: all luma for I420/NV12, plain pixels for RGB.
                    let region = &map[..map.len() / 2];
                    let step = (region.len() / 8192).max(1);
                    let (mut sum, mut cnt, mut lit) = (0u64, 0u64, 0u64);
                    for b in region.iter().step_by(step) {
                        sum += *b as u64;
                        cnt += 1;
                        lit += (*b > 40) as u64;
                    }
                    tracing::info!(
                        "{label}: frame {n}, {} bytes, mean {}, {}% non-black",
                        map.len(),
                        sum / cnt.max(1),
                        lit * 100 / cnt.max(1)
                    );
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

fn leaky_queue(buffers: u32) -> Result<gst::Element> {
    let q = make("queue")?;
    q.set_property("max-size-buffers", buffers);
    q.set_property("max-size-bytes", 0u32);
    q.set_property("max-size-time", 0u64);
    q.set_property_from_str("leaky", "downstream");
    Ok(q)
}

/// Camera source producing raw video in its best mode (≤720p, ~30 fps).
/// Returns the not-yet-added elements and the mode's size and frame rate.
pub fn camera_source(device: &gst::Device) -> Result<(Vec<gst::Element>, u32, u32, u32)> {
    let src = device.create_element(Some("camsrc"))?;
    let mut chain = vec![src];
    let (mut w, mut h, mut fps) = (1280u32, 720u32, 30u32);
    match super::devices::best_camera_caps(device) {
        Some(c) => {
            let s = c.structure(0).unwrap();
            w = s.get::<i32>("width").unwrap_or(1280) as u32;
            h = s.get::<i32>("height").unwrap_or(720) as u32;
            if let Ok(f) = s.get::<gst::Fraction>("framerate") {
                fps = ((f.numer() as f64 / f.denom().max(1) as f64).round() as u32).clamp(5, 30);
            }
            let jpeg = s.name() == "image/jpeg";
            let cf = make("capsfilter")?;
            cf.set_property("caps", &c);
            chain.push(cf);
            if jpeg {
                chain.push(make("jpegdec")?);
            }
        }
        None => chain.push(make("videoconvert")?),
    }
    Ok((chain, w, h, fps))
}

pub struct VideoSender {
    _run: Running,
    hub: Arc<Hub>,
    track: Track,
    pub preview: gtk::gdk::Paintable,
    /// Stops the screen frame repeater thread.
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Drop for VideoSender {
    fn drop(&mut self) {
        self.hub.set_keyframe_handler(self.track, None);
        if let Some(s) = &self.stop {
            s.store(true, Ordering::Relaxed);
        }
    }
}

impl VideoSender {
    pub fn camera(
        hub: Arc<Hub>,
        device: &gst::Device,
        kbps: u32,
        hw: bool,
        on_error: impl Fn(String) + 'static,
    ) -> Result<Self> {
        let pipeline = gst::Pipeline::with_name("camera");
        let (mut chain, mut w, mut h, fps) = camera_source(device)?;
        // Encode at most 720p.
        if h > 720 {
            w = w * 720 / h;
            h = 720;
        }
        let p = VideoParams { width: w & !1, height: h & !1, fps, kbps, hw };
        let (head, preview) = video_tail(&pipeline, hub.clone(), Track::Camera, &p)?;
        chain.push(head);
        pipeline.add_many(&chain[..chain.len() - 1])?;
        gst::Element::link_many(&chain)?;
        let run = Running::start(pipeline, "Camera", on_error)?;
        Ok(Self { _run: run, hub, track: Track::Camera, preview, stop: None })
    }

    pub fn screen(
        hub: Arc<Hub>,
        fd: Option<OwnedFd>,
        node_id: u32,
        size: Option<(i32, i32)>,
        quality: (u32, u32, u32),
        hw: bool,
        on_error: impl Fn(String) + 'static,
    ) -> Result<Self> {
        let (max_h, fps, kbps) = quality;
        let pipeline = gst::Pipeline::with_name("screen");
        let src = make("pipewiresrc")?;
        if let Some(fd) = &fd {
            src.set_property("fd", fd.as_raw_fd());
        }
        src.set_property("path", node_id.to_string());
        src.set_property("do-timestamp", true);
        // Compositors (niri, GNOME, KDE…) usually hand out GPU (DMA-BUF) frames, often
        // tiled. Reading those directly with the CPU gives black/garbage, so import them
        // through GL and download a normal image. Shared-memory frames pass through too.
        let mut head_chain: Vec<gst::Element> = Vec::new();
        if has("glupload") && has("glcolorconvert") && has("gldownload") {
            head_chain.push(make("glupload")?);
            head_chain.push(make("glcolorconvert")?);
            head_chain.push(make("gldownload")?);
        }
        let sysmem = caps_filter("video/x-raw")?;
        sysmem.static_pad("src").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                if let gst::EventView::Caps(c) = ev.view() {
                    tracing::info!("screen frames after import: {}", c.caps());
                }
            }
            gst::PadProbeReturn::Ok
        });
        src.static_pad("src").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                if let gst::EventView::Caps(c) = ev.view() {
                    tracing::info!("screen source caps: {}", c.caps());
                }
            }
            gst::PadProbeReturn::Ok
        });
        head_chain.push(sysmem);
        // Some compositors report a bogus pixel aspect ratio (niri: 1/2147483647), which
        // makes the scaler squash the picture into black borders. Screens have square pixels.
        if has("capssetter") {
            let fix = make("capssetter")?;
            fix.set_property("caps", gst::Caps::builder("video/x-raw").field("pixel-aspect-ratio", gst::Fraction::new(1, 1)).build());
            head_chain.push(fix);
        }
        let (sw, sh) = size.map(|(w, h)| (w.max(2) as u32, h.max(2) as u32)).unwrap_or((1920, 1080));
        let (w, h) = if sh > max_h { (sw * max_h / sh, max_h) } else { (sw, sh) };
        let p = VideoParams { width: (w & !1).max(2), height: (h & !1).max(2), fps, kbps, hw };
        let (head, preview) = video_tail(&pipeline, hub.clone(), Track::Screen, &p)?;
        // Compositors only send frames when the screen changes, and pipewiresrc's
        // keepalive doesn't repeat GPU frames. So capture ends in an appsink holding the
        // newest frame, and a repeater thread feeds the encoder side through an appsrc:
        // new frames right away, the last one again every 200 ms while the screen is idle.
        let latest: Arc<std::sync::Mutex<(Option<gst::Sample>, u64)>> = Default::default();
        let grab = gst_app::AppSink::builder().sync(false).max_buffers(1).drop(true).build();
        // Must not wait for preroll: the encoder side only gets data once this sink does.
        grab.set_property("async", false);
        let (l2, l3) = (latest.clone(), latest.clone());
        grab.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_preroll(move |s| {
                    let sample = s.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                    let mut g = l3.lock().unwrap();
                    g.0 = Some(sample);
                    g.1 += 1;
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let mut g = l2.lock().unwrap();
                    g.0 = Some(sample);
                    g.1 += 1;
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let feed = gst_app::AppSrc::builder().is_live(true).do_timestamp(true).format(gst::Format::Time).build();
        set_opt(feed.upcast_ref(), "leaky-type", "downstream");
        set_opt(feed.upcast_ref(), "max-buffers", "2");

        let mut capture = vec![src];
        capture.extend(head_chain);
        capture.push(grab.clone().upcast());
        pipeline.add_many(&capture)?;
        gst::Element::link_many(&capture)?;
        pipeline.add(&feed)?;
        feed.link(&head)?;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (stop2, feed_weak) = (stop.clone(), feed.downgrade());
        let tick = std::time::Duration::from_millis((1000 / fps.max(1)) as u64);
        std::thread::Builder::new().name("screen-feed".into()).spawn(move || {
            let (mut last_seq, mut last_push) = (0u64, Instant::now());
            let mut caps: Option<gst::Caps> = None;
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(tick);
                let Some(feed) = feed_weak.upgrade() else { break };
                let (sample, seq) = {
                    let g = latest.lock().unwrap();
                    (g.0.clone(), g.1)
                };
                let Some(sample) = sample else {
                    if last_push.elapsed() > std::time::Duration::from_secs(3) {
                        tracing::warn!("screen feed: no frames captured yet");
                        last_push = Instant::now();
                    }
                    continue;
                };
                if seq == last_seq && last_push.elapsed() < std::time::Duration::from_millis(200) {
                    continue;
                }
                if caps.as_ref() != sample.caps_owned().as_ref() {
                    caps = sample.caps_owned();
                    feed.set_caps(caps.as_ref());
                }
                if let Some(buf) = sample.buffer() {
                    let mut b = buf.copy();
                    {
                        let b = b.make_mut();
                        b.set_pts(gst::ClockTime::NONE);
                        b.set_dts(gst::ClockTime::NONE);
                        b.set_duration(gst::ClockTime::NONE);
                    }
                    match feed.push_buffer(b) {
                        Ok(_) if last_seq == 0 => tracing::info!("screen feed: first frame pushed"),
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("screen feed stopped: {e:?}");
                            break;
                        }
                    }
                }
                last_seq = seq;
                last_push = Instant::now();
            }
        })?;

        let mut run = Running::start(pipeline, "Screen share", on_error)?;
        // The portal's PipeWire connection must outlive the pipeline.
        if let Some(fd) = fd {
            run.keep_alive(fd);
        }
        Ok(Self { _run: run, hub, track: Track::Screen, preview, stop: Some(stop) })
    }
}
