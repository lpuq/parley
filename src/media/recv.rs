//! [`Hub`] → decode → speakers / screen.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
};

use anyhow::Result;
use gst::prelude::*;
use iroh::EndpointId;

use super::{ECHO_PROBE, Running, caps_filter, has, make, set_opt};
use crate::{
    hub::{Hub, SPEAKING_THRESHOLD_DB, rms_dbfs},
    proto::Track,
};

// Measured with a 440 Hz tone over a simulated 80 ms-jitter link: 50 ms gave audible
// speed-ups (pitch to 520 Hz in 19% of 50 ms windows), 120 ms gave none.
const JITTER_MS: u32 = 120;
const OPUS_RTP_CAPS: &str =
    "application/x-rtp,media=audio,clock-rate=48000,encoding-name=OPUS,payload=96,encoding-params=(string)2";

const MIN_JITTER_MS: u32 = 80;
const MAX_JITTER_MS: u32 = 400;

/// Playout schedule for one received track. Each packet's playback time comes from
/// its RTP timestamp, anchored to the fastest packets seen ("floor"), not from when it
/// happened to arrive: on bursty Wi-Fi, arrival-based timestamps make the timeline
/// jump back and forth and the output stutters. The jitter buffer then holds packets
/// for `latency_ms` past that schedule, adapted to how late packets actually run.
#[derive(Default)]
struct Playout {
    last_ts: Option<u32>,
    /// Extended RTP timestamp (samples since the first packet), immune to wraparound.
    ext: i64,
    /// min(arrival − media time) in ns; may creep up slowly to follow clock drift.
    floor: Option<i64>,
    floor_at: u64,
    /// (arrival ns, ms behind schedule) over the last few seconds.
    delays: std::collections::VecDeque<(u64, u32)>,
    latency_ms: u32,
    adjusted_at: u64,
    /// Packets later than the buffer allowed, since the last stats line.
    late: u32,
}

impl Playout {
    /// Returns the packet's scheduled time (ns, running time) and a new jitter-buffer
    /// latency when it should change.
    fn schedule(&mut self, ts: u32, arrival: u64, initial_ms: u32) -> (u64, Option<u32>) {
        if self.latency_ms == 0 {
            self.latency_ms = initial_ms;
        }
        match self.last_ts {
            Some(l) => self.ext += ts.wrapping_sub(l) as i32 as i64,
            None => self.ext = 0,
        }
        self.last_ts = Some(ts);
        let media = self.ext * 1_000_000_000 / 48_000;
        let off = arrival as i64 - media;
        let floor = match self.floor {
            // Allow 0.5 ms/s of upward creep (sender clock slower than ours). A jump
            // of seconds means the sender restarted its stream: start over.
            Some(fl) => {
                let crept = fl + arrival.saturating_sub(self.floor_at) as i64 / 2000;
                if off > crept + 2_000_000_000 { off } else { crept.min(off) }
            }
            None => off,
        };
        self.floor = Some(floor);
        self.floor_at = arrival;
        let delay_ms = ((off - floor) / 1_000_000).max(0) as u32;
        if delay_ms > self.latency_ms {
            self.late += 1;
        }
        self.delays.push_back((arrival, delay_ms));
        while self.delays.front().is_some_and(|&(at, _)| arrival.saturating_sub(at) > 8_000_000_000) {
            self.delays.pop_front();
        }
        // Once a second: grow at once to cover 97% of packets (+30 ms), shrink slowly.
        let mut change = None;
        if arrival.saturating_sub(self.adjusted_at) > 1_000_000_000 && self.delays.len() >= 50 {
            self.adjusted_at = arrival;
            let mut v: Vec<u32> = self.delays.iter().map(|d| d.1).collect();
            v.sort_unstable();
            let p97 = v[(v.len() * 97 / 100).min(v.len() - 1)];
            let target = (p97 + 30).clamp(MIN_JITTER_MS, MAX_JITTER_MS).div_ceil(10) * 10;
            let next = if target > self.latency_ms {
                target
            } else if target + 40 < self.latency_ms {
                self.latency_ms - 10
            } else {
                self.latency_ms
            };
            if next != self.latency_ms {
                self.latency_ms = next;
                change = Some(next);
            }
        }
        ((floor + media).max(0) as u64, change)
    }
}

/// Timing diagnostics for one received track, logged (and reset) with the playback stats.
#[derive(Default)]
struct Flow {
    play: Playout,
    /// Arrival side: last (seq, rtp ts, arrival ns).
    last_in: Option<(u16, u32, u64)>,
    /// Consecutive packets whose RTP timestamps are not 20 ms apart (sender clock jumps).
    ts_jumps: u32,
    max_ts_jump_ms: i64,
    /// Arrival gaps longer than 80 ms, and the longest.
    stalls: u32,
    max_gap_ms: u64,
    /// Jitter-buffer output: last (rtp ts, pts). Gaps/overlaps = PTS steps that differ
    /// from the RTP step by more than 5 ms, i.e. holes or overlaps in the playback timeline.
    last_out: Option<(u32, u64)>,
    out_gaps: u32,
    out_overlaps: u32,
    max_skew_ms: i64,
    /// How far behind its timestamp audio reaches the mixer (running time − pts).
    late_min_ms: i64,
    late_max_ms: i64,
    late_n: u32,
}

struct Branch {
    elements: Vec<gst::Element>,
    flow: Arc<std::sync::Mutex<Flow>>,
    jitter: gst::Element,
    mixer_pad: Arc<std::sync::Mutex<Option<gst::Pad>>>,
    volume: gst::Element,
}

/// Runs pipeline surgery off the UI thread, in order. Linking into a running
/// mixer queries caps all the way down to the audio sink, which can block for
/// a long time if the sound server is stuck.
struct Worker(std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>);

impl Worker {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("audio-graph".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            })
            .expect("spawn audio worker");
        Self(tx)
    }
    fn run(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.0.send(Box::new(f));
    }
}

/// One playback pipeline mixing every remote audio track. It also hosts the
/// echo probe so the mic's echo canceller hears exactly what we play.
pub struct AudioOut {
    run: Running,
    sink: gst::Element,
    last_device: std::cell::RefCell<String>,
    /// Set once audio reaches the echo probe, i.e. playback really runs.
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    mixer: gst::Element,
    master: gst::Element,
    branches: HashMap<(EndpointId, Track), Branch>,
    /// Per-track volume (0.0–2.0): a person's voice and their stream audio separately.
    volumes: HashMap<(EndpointId, Track), f64>,
    worker: Worker,
}

impl AudioOut {
    pub fn start(device: Option<&gst::Device>, on_error: impl Fn(String) + 'static) -> Result<Self> {
        let pipeline = gst::Pipeline::with_name("playback");
        // A silent live source keeps the mixer clocked even with nobody talking.
        let silence = make("audiotestsrc")?;
        silence.set_property_from_str("wave", "silence");
        silence.set_property("is-live", true);
        silence.set_property("samplesperbuffer", 480i32);
        let scaps = caps_filter("audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2")?;
        let mixer = make("audiomixer")?;
        set_opt(&mixer, "latency", &(20 * gst::ClockTime::MSECOND.nseconds()).to_string());
        set_opt(&mixer, "ignore-inactive-pads", "true");
        let conv = make("audioconvert")?;
        let ocaps = caps_filter("audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2")?;
        let master = make("volume")?;
        let test_file = std::env::var("PARLEY_TEST_AUDIO_FILE").ok();
        let sink = match (&test_file, device) {
            // Test hook: record what we would play (raw S16LE 48 kHz stereo).
            (Some(path), _) => {
                let f = make("filesink")?;
                f.set_property("location", path);
                f
            }
            (None, Some(d)) => d.create_element(Some("speaker"))?,
            (None, None) if has("pulsesink") => make("pulsesink")?,
            (None, None) => make("autoaudiosink")?,
        };
        // Big enough for Bluetooth headsets and busy systems: an underrun means a gap
        // followed by the sink skipping ahead, which is far worse than ~0.1 s of latency.
        set_opt(&sink, "buffer-time", "100000");
        set_opt(&sink, "latency-time", "20000");
        set_opt(&sink, "client-name", "Parley");

        let mut chain = vec![silence, scaps, mixer.clone(), conv, ocaps];
        if has("webrtcechoprobe") {
            chain.push(gst::ElementFactory::make("webrtcechoprobe").name(ECHO_PROBE).build()?);
        }
        chain.extend([master.clone(), sink.clone()]);
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r2 = ready.clone();
        sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            r2.store(true, Ordering::Relaxed);
            gst::PadProbeReturn::Remove
        });
        tracing::info!(
            "audio output: {} ({})",
            device.map(|d| d.display_name().to_string()).unwrap_or("system default".into()),
            sink.factory().map(|f| f.name().to_string()).unwrap_or_default()
        );
        let run = Running::start(pipeline, "Audio output", on_error)?;
        Ok(Self { run, sink: sink.clone(), last_device: Default::default(), ready, mixer, master, branches: HashMap::new(), volumes: HashMap::new(), worker: Worker::new() })
    }

    pub fn set_deafened(&self, deaf: bool) {
        self.master.set_property("mute", deaf);
    }

    pub fn set_volume(&mut self, peer: EndpointId, track: Track, vol: f64) {
        self.volumes.insert((peer, track), vol);
        if let Some(b) = self.branches.get(&(peer, track)) {
            b.volume.set_property("volume", vol);
        }
    }

    pub fn volume(&self, peer: &EndpointId, track: Track) -> f64 {
        self.volumes.get(&(*peer, track)).copied().unwrap_or(1.0)
    }

    pub fn add(&mut self, hub: &Arc<Hub>, peer: EndpointId, track: Track) -> Result<()> {
        if self.branches.contains_key(&(peer, track)) {
            return Ok(());
        }
        let pipeline = &self.run.pipeline;
        let src = gst_app::AppSrc::builder()
            .caps(&OPUS_RTP_CAPS.parse::<gst::Caps>().unwrap())
            .is_live(true)
            .do_timestamp(true)
            .format(gst::Format::Time)
            .max_bytes(64 * 1024)
            .build();
        // If playback stalls, drop old packets rather than queueing without bound.
        set_opt(src.upcast_ref(), "leaky-type", "downstream");
        let jb = make("rtpjitterbuffer")?;
        let env = |k: &str| std::env::var(k).ok();
        jb.set_property("latency", env("PARLEY_TEST_JB_MS").and_then(|v| v.parse().ok()).unwrap_or(JITTER_MS));
        if let Some(m) = env("PARLEY_TEST_JB_MODE") {
            jb.set_property_from_str("mode", &m);
        }
        // No drop-on-latency: audio over the call link arrives in bursts (it shares the
        // connection with screen video), and dropping on every burst chops the sound up.
        if env("PARLEY_TEST_JB_DROP").is_some() {
            set_opt(&jb, "drop-on-latency", "true");
        }
        set_opt(&jb, "do-lost", "true");
        let depay = make("rtpopusdepay")?;
        let dec = make("opusdec")?;
        set_opt(&dec, "plc", "true");
        set_opt(&dec, "use-inband-fec", "true");
        let conv = make("audioconvert")?;
        let res = make("audioresample")?;
        let caps = caps_filter("audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2")?;
        let vol = make("volume")?;
        vol.set_property("volume", self.volume(&peer, track));

        let elements: Vec<gst::Element> =
            vec![src.clone().upcast(), jb, depay, dec, conv, res, caps, vol.clone()];

        let cell = hub.speaking_cell(peer, track);
        let hub2 = hub.clone();
        vol.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                if let Ok(map) = buf.map_readable() {
                    if rms_dbfs(&map) > SPEAKING_THRESHOLD_DB {
                        cell.store(hub2.epoch.now_ms(), Ordering::Relaxed);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        let flow = Arc::new(std::sync::Mutex::new(Flow::default()));
        let (f1, jbw) = (flow.clone(), elements[1].downgrade());
        let initial_ms = elements[1].property::<u32>("latency");
        let label = format!("{track:?} from {}", peer.fmt_short());
        elements[0].static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(t) = buffer.dts().or(buffer.pts()) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some((seq, ts)) = buffer.map_readable().ok().filter(|m| m.len() >= 8).map(|m| {
                (u16::from_be_bytes([m[2], m[3]]), u32::from_be_bytes([m[4], m[5], m[6], m[7]]))
            }) else {
                return gst::PadProbeReturn::Ok;
            };
            let t = t.nseconds();
            let mut f = f1.lock().unwrap();
            if let Some((ls, lts, lt)) = f.last_in {
                if seq == ls.wrapping_add(1) {
                    let d = ts.wrapping_sub(lts) as i32 as i64 / 48;
                    if d != 20 {
                        f.ts_jumps += 1;
                        if d.abs() > f.max_ts_jump_ms.abs() {
                            f.max_ts_jump_ms = d;
                        }
                    }
                }
                let gap = t.saturating_sub(lt) / 1_000_000;
                if gap > 80 {
                    f.stalls += 1;
                }
                f.max_gap_ms = f.max_gap_ms.max(gap);
            }
            f.last_in = Some((seq, ts, t));
            let (at, change) = f.play.schedule(ts, t, initial_ms);
            drop(f);
            let b = buffer.make_mut();
            b.set_dts(gst::ClockTime::from_nseconds(at));
            b.set_pts(gst::ClockTime::from_nseconds(at));
            if let (Some(ms), Some(jb)) = (change, jbw.upgrade()) {
                tracing::info!("playback {label}: jitter buffer now {ms} ms");
                jb.set_property("latency", ms);
            }
            gst::PadProbeReturn::Ok
        });
        let f2 = flow.clone();
        elements[1].static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                if let (Some(pts), Ok(m)) = (buf.pts(), buf.map_readable()) {
                    if m.len() >= 8 {
                        let ts = u32::from_be_bytes([m[4], m[5], m[6], m[7]]);
                        let pts = pts.nseconds();
                        let mut f = f2.lock().unwrap();
                        if let Some((lts, lpts)) = f.last_out {
                            let drtp = ts.wrapping_sub(lts) as i32 as i64 * 1_000_000_000 / 48_000;
                            let skew = (pts as i64 - lpts as i64 - drtp) / 1_000_000;
                            if skew > 5 {
                                f.out_gaps += 1;
                            } else if skew < -5 {
                                f.out_overlaps += 1;
                            }
                            if skew.abs() > f.max_skew_ms.abs() {
                                f.max_skew_ms = skew;
                            }
                        }
                        f.last_out = Some((ts, pts));
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        let (f3, vw) = (flow.clone(), vol.downgrade());
        vol.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let (Some(buf), Some(v)) = (info.buffer(), vw.upgrade()) {
                if let (Some(pts), Some(now)) = (buf.pts(), v.current_running_time()) {
                    let late = (now.nseconds() as i64 - pts.nseconds() as i64) / 1_000_000;
                    let mut f = f3.lock().unwrap();
                    if f.late_n == 0 || late < f.late_min_ms {
                        f.late_min_ms = late;
                    }
                    if f.late_n == 0 || late > f.late_max_ms {
                        f.late_max_ms = late;
                    }
                    f.late_n += 1;
                }
            }
            gst::PadProbeReturn::Ok
        });

        let mixer_pad = Arc::new(std::sync::Mutex::new(None));
        let (pipeline, mixer, els, vol2, pad_slot) =
            (pipeline.clone(), self.mixer.clone(), elements.clone(), vol.clone(), mixer_pad.clone());
        self.worker.run(move || {
            // The output pipeline starts asynchronously; elements added mid-transition can
            // get stuck short of PLAYING (silent until the device is changed). Wait it out.
            let _ = pipeline.state(gst::ClockTime::from_seconds(5));
            let res = (|| -> Result<()> {
                pipeline.add_many(&els)?;
                gst::Element::link_many(&els)?;
                let pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| anyhow::anyhow!("mixer pad"))?;
                vol2.static_pad("src").unwrap().link(&pad)?;
                *pad_slot.lock().unwrap() = Some(pad);
                for e in els.iter().rev() {
                    e.sync_state_with_parent()?;
                }
                Ok(())
            })();
            if let Err(e) = res {
                tracing::error!("adding audio branch failed: {e:#}");
            }
        });
        hub.register_sink(peer, track, src);
        self.branches.insert((peer, track), Branch { jitter: elements[1].clone(), elements, flow, mixer_pad, volume: vol });
        Ok(())
    }

    /// Logs each stream's jitter-buffer counters (lost/late packets reveal timing trouble).
    pub fn log_stats(&self) {
        // Which device the sound server actually plays our stream on (it can move
        // underneath us, e.g. Bluetooth earbuds switching to headset mode).
        if self.sink.find_property("current-device").is_some() {
            let dev = self.sink.property::<Option<String>>("current-device").unwrap_or_default();
            if *self.last_device.borrow() != dev {
                tracing::info!("audio output now playing on: {dev}");
                *self.last_device.borrow_mut() = dev;
            }
        }
        let mut q = gst::query::Latency::new();
        if self.run.pipeline.query(&mut q) {
            let (live, min, max) = q.result();
            tracing::debug!("playback pipeline latency: live {live}, min {min}, max {}", max.map(|m| m.to_string()).unwrap_or("none".into()));
        }
        for ((peer, track), b) in &self.branches {
            {
                let mut f = b.flow.lock().unwrap();
                tracing::info!(
                    "timing {:?} from {}: buffer {} ms, late {}, rtp ts jumps {} (max {} ms), arrival stalls>80ms {} (max gap {} ms), timeline gaps {} overlaps {} (max {} ms), reach mixer {}..{} ms after pts ({} bufs)",
                    track,
                    peer.fmt_short(),
                    f.play.latency_ms,
                    f.play.late,
                    f.ts_jumps,
                    f.max_ts_jump_ms,
                    f.stalls,
                    f.max_gap_ms,
                    f.out_gaps,
                    f.out_overlaps,
                    f.max_skew_ms,
                    f.late_min_ms,
                    f.late_max_ms,
                    f.late_n
                );
                let (li, lo, mut play) = (f.last_in, f.last_out, std::mem::take(&mut f.play));
                play.late = 0;
                *f = Flow { play, last_in: li, last_out: lo, ..Default::default() };
            }
            let st = b.jitter.property::<gst::Structure>("stats");
            let g = |k: &str| st.get::<u64>(k).unwrap_or(0);
            if g("num-pushed") == 0 && g("num-lost") == 0 {
                continue;
            }
            let src = &b.elements[0];
            let sp = |k: &str| src.find_property(k).map(|_| src.property::<u64>(k)).unwrap_or(0);
            tracing::info!(
                "playback {:?} from {}: pushed {}, lost {}, late {}, dup {}, jitter {} µs; queue in {} out {} dropped {} level {} B",
                track,
                peer.fmt_short(),
                g("num-pushed"),
                g("num-lost"),
                g("num-late"),
                g("num-duplicates"),
                st.get::<u64>("avg-jitter").unwrap_or(0) / 1000,
                sp("in"),
                sp("out"),
                sp("dropped"),
                sp("current-level-bytes")
            );
        }
    }

    pub fn remove_peer(&mut self, peer: &EndpointId) {
        let keys: Vec<_> = self.branches.keys().filter(|(p, _)| p == peer).copied().collect();
        for k in keys {
            if let Some(b) = self.branches.remove(&k) {
                let (pipeline, mixer) = (self.run.pipeline.clone(), self.mixer.clone());
                self.worker.run(move || {
                    for e in &b.elements {
                        let _ = e.set_state(gst::State::Null);
                    }
                    let _ = pipeline.remove_many(&b.elements);
                    if let Some(pad) = b.mixer_pad.lock().unwrap().take() {
                        mixer.release_request_pad(&pad);
                    }
                });
            }
        }
    }
}

/// Decodes one remote video track into a GTK paintable.
pub struct VideoReceiver {
    _run: Running,
    pub paintable: gtk::gdk::Paintable,
    hub: Arc<Hub>,
    peer: EndpointId,
    track: Track,
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.hub.unregister_sink(&self.peer, self.track);
    }
}

impl VideoReceiver {
    pub fn start(hub: &Arc<Hub>, peer: EndpointId, track: Track, on_error: impl Fn(String) + 'static) -> Result<Self> {
        let pipeline = gst::Pipeline::with_name(&format!("recv-{track:?}"));
        let src = gst_app::AppSrc::builder()
            .caps(&"video/x-h264,stream-format=byte-stream,alignment=au".parse::<gst::Caps>().unwrap())
            .is_live(true)
            .do_timestamp(true)
            .format(gst::Format::Time)
            .max_bytes(32 * 1024 * 1024)
            .build();
        let parse = make("h264parse")?;
        let dec = h264_decoder()?;
        let conv = make("videoconvert")?;
        // RGB into GTK: YUV textures render wrong on some GTK renderers.
        let rgb = caps_filter("video/x-raw,format={BGRA,RGBA,BGRx,RGBx}")?;
        super::send::watch_luma(&rgb, format!("{track:?} received from {}", peer.fmt_short()));
        let sink = make("gtk4paintablesink")?;
        sink.set_property("sync", false);
        let paintable = sink.property::<gtk::gdk::Paintable>("paintable");
        let chain = [src.clone().upcast(), parse, dec, conv, rgb, sink];
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;
        let run = Running::start(pipeline, "Video decoder", on_error)?;
        hub.register_sink(peer, track, src);
        Ok(Self { _run: run, paintable, hub: hub.clone(), peer, track })
    }
}

fn h264_decoder() -> Result<gst::Element> {
    // Software decoding is cheap at these resolutions and behaves the same on
    // every machine; hardware decoders are only a fallback.
    if has("avdec_h264") {
        let d = make("avdec_h264")?;
        // Slice threading adds no frame delay, unlike frame threading.
        set_opt(&d, "thread-type", "slice");
        set_opt(&d, "max-errors", "-1");
        return Ok(d);
    }
    for name in ["openh264dec", "vah264dec", "nvh264dec"] {
        if has(name) {
            return make(name);
        }
    }
    Err(anyhow::anyhow!("No H.264 decoder found. Install gstreamer1-plugin-libav (or the Flatpak codecs extension)."))
}
