//! Bridges GStreamer and the network. Encoders push packets/frames in here
//! (from GStreamer streaming threads); per-peer network tasks pull them out.
//! Incoming media is routed to `AppSrc`s registered by the receive pipelines.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use iroh::{EndpointId, endpoint::Connection};
use tokio::sync::mpsc;

use crate::proto::{Track, VIDEO_TRACKS};

#[derive(Clone)]
pub struct VideoFrame {
    pub data: Bytes,
    pub pts_us: u64,
    pub key: bool,
    pub captured: Instant,
}

/// Outgoing side of one peer.
pub struct PeerOut {
    pub conn: Connection,
    pub video: [mpsc::Sender<VideoFrame>; 2],
    /// Set when frames had to be dropped; the writer skips to the next keyframe.
    pub need_key: [Arc<AtomicBool>; 2],
    /// Tracks this peer asked us not to send (they stopped watching), indexed by `Track as usize`.
    pub paused: [AtomicBool; 4],
}

type KeyframeFn = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct Hub {
    peers: RwLock<HashMap<EndpointId, Arc<PeerOut>>>,
    sinks: RwLock<HashMap<(EndpointId, Track), gst_app::AppSrc>>,
    keyframe: Mutex<HashMap<Track, KeyframeFn>>,
    last_keyframe_req: Mutex<HashMap<Track, Instant>>,
    /// Generation counter per incoming (peer, video track): only the newest GOP stream may push.
    generations: Mutex<HashMap<(EndpointId, Track), u64>>,
    /// Last time we heard audio above threshold, per (peer, track); ms since `epoch`.
    pub speaking: Mutex<HashMap<(EndpointId, Track), Arc<AtomicU64>>>,
    pub epoch: once_epoch::Epoch,
    /// Outgoing video congestion per video track (camera, screen), read and reset by
    /// the encoder's bitrate controller.
    pub congestion: [VideoCongestion; 2],
    /// Mic test playback: our own audio, queued to come back to us after a delay.
    loopback: Mutex<Option<std::sync::mpsc::Sender<(Instant, Track, Vec<u8>)>>>,
}

/// How badly a video track's frames are backing up on their way out.
#[derive(Default)]
pub struct VideoCongestion {
    /// Worst time from encoding to sent, in ms, since the last read.
    pub delay_ms: std::sync::atomic::AtomicU32,
    /// Frames dropped (stale or queue full) since the last read.
    pub drops: std::sync::atomic::AtomicU32,
}

impl VideoCongestion {
    /// Returns (worst delay ms, drops) and resets both.
    pub fn take(&self) -> (u32, u32) {
        (self.delay_ms.swap(0, Ordering::Relaxed), self.drops.swap(0, Ordering::Relaxed))
    }
}

pub const VIDEO_QUEUE: usize = 6;

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // ----- outgoing -----

    pub fn add_peer(&self, id: EndpointId, out: Arc<PeerOut>) {
        self.peers.write().unwrap().insert(id, out);
        // Give the newcomer a picture right away.
        for t in VIDEO_TRACKS {
            self.request_keyframe_now(t);
        }
    }

    /// Removes the peer only if `conn` is still the registered connection
    /// (a reconnect may already have replaced it).
    pub fn remove_peer(&self, id: &EndpointId, conn: &Connection) {
        let mut peers = self.peers.write().unwrap();
        if peers.get(id).is_some_and(|p| p.conn.stable_id() == conn.stable_id()) {
            peers.remove(id);
        }
    }

    pub fn clear_peers(&self) {
        self.peers.write().unwrap().clear();
        self.sinks.write().unwrap().clear();
        self.generations.lock().unwrap().clear();
    }

    /// Called from the audio encoder's appsink for every RTP packet.
    /// Mic test: from now on, audio we send also comes back into this hub as if
    /// `as_peer` had sent it, `delay` later (register an audio sink for it).
    pub fn set_loopback(self: &Arc<Self>, as_peer: EndpointId, delay: std::time::Duration) {
        let (tx, rx) = std::sync::mpsc::channel::<(Instant, Track, Vec<u8>)>();
        *self.loopback.lock().unwrap() = Some(tx);
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("mic-loopback".into()).spawn(move || {
            while let Ok((sent, track, data)) = rx.recv() {
                let due = sent + delay;
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                let Some(hub) = weak.upgrade() else { return };
                hub.push(&as_peer, track, data);
            }
        });
    }

    pub fn send_audio(&self, track: Track, rtp: &[u8]) {
        if let Some(tx) = self.loopback.lock().unwrap().as_ref() {
            let _ = tx.send((Instant::now(), track, rtp.to_vec()));
        }
        let peers = self.peers.read().unwrap();
        if peers.is_empty() {
            return;
        }
        let mut pkt = Vec::with_capacity(rtp.len() + 1);
        pkt.push(track as u8);
        pkt.extend_from_slice(rtp);
        let pkt = Bytes::from(pkt);
        for p in peers.values() {
            if p.paused[track as usize].load(Ordering::Relaxed) {
                continue;
            }
            // Unreliable: if the connection is congested quinn drops old datagrams.
            let _ = p.conn.send_datagram(pkt.clone());
        }
    }

    /// Called from the video encoder's appsink for every access unit.
    pub fn send_video(&self, track: Track, frame: VideoFrame) {
        let idx = track.video_index();
        let mut congested = false;
        for p in self.peers.read().unwrap().values() {
            if p.paused[track as usize].load(Ordering::Relaxed) {
                continue;
            }
            if p.video[idx].try_send(frame.clone()).is_err() {
                p.need_key[idx].store(true, Ordering::Relaxed);
                congested = true;
            }
        }
        if congested {
            self.congestion[idx].drops.fetch_add(1, Ordering::Relaxed);
            self.request_keyframe(track);
        }
    }

    /// A frame of ours went out `delay` after it was encoded.
    pub fn report_video_sent(&self, track: Track, delay: std::time::Duration) {
        self.congestion[track.video_index()].delay_ms.fetch_max(delay.as_millis() as u32, Ordering::Relaxed);
    }

    /// A frame of ours was dropped for being too late.
    pub fn report_video_drop(&self, track: Track) {
        self.congestion[track.video_index()].drops.fetch_add(1, Ordering::Relaxed);
    }

    /// A peer stopped/started watching one of our tracks.
    pub fn set_paused(&self, peer: &EndpointId, track: Track, paused: bool) {
        if let Some(p) = self.peers.read().unwrap().get(peer) {
            p.paused[track as usize].store(paused, Ordering::Relaxed);
            if !paused && track.is_video() {
                // Restart their stream cleanly from a fresh keyframe.
                p.need_key[track.video_index()].store(true, Ordering::Relaxed);
            }
        }
        if !paused && track.is_video() {
            self.request_keyframe_now(track);
        }
    }

    pub fn set_keyframe_handler(&self, track: Track, f: Option<KeyframeFn>) {
        let mut map = self.keyframe.lock().unwrap();
        match f {
            Some(f) => {
                map.insert(track, f);
            }
            None => {
                map.remove(&track);
            }
        }
    }

    /// Asks the encoder for an IDR frame, at most twice a second.
    pub fn request_keyframe(&self, track: Track) {
        {
            let mut last = self.last_keyframe_req.lock().unwrap();
            let now = Instant::now();
            if let Some(t) = last.get(&track) {
                if now.duration_since(*t) < Duration::from_millis(500) {
                    return;
                }
            }
            last.insert(track, now);
        }
        self.request_keyframe_now(track);
    }

    fn request_keyframe_now(&self, track: Track) {
        if let Some(f) = self.keyframe.lock().unwrap().get(&track) {
            f();
        }
    }

    // ----- incoming -----

    pub fn register_sink(&self, peer: EndpointId, track: Track, src: gst_app::AppSrc) {
        self.sinks.write().unwrap().insert((peer, track), src);
    }

    pub fn unregister_sink(&self, peer: &EndpointId, track: Track) {
        self.sinks.write().unwrap().remove(&(*peer, track));
    }

    pub fn unregister_peer_sinks(&self, peer: &EndpointId) {
        self.sinks.write().unwrap().retain(|(p, _), _| p != peer);
        self.generations.lock().unwrap().retain(|(p, _), _| p != peer);
    }

    pub fn push(&self, peer: &EndpointId, track: Track, data: Vec<u8>) {
        let sinks = self.sinks.read().unwrap();
        if let Some(src) = sinks.get(&(*peer, track)) {
            let _ = src.push_buffer(gst::Buffer::from_mut_slice(data));
        }
    }

    pub fn new_generation(&self, peer: EndpointId, track: Track) -> u64 {
        let mut g = self.generations.lock().unwrap();
        let e = g.entry((peer, track)).or_insert(0);
        *e += 1;
        *e
    }

    pub fn is_current_generation(&self, peer: &EndpointId, track: Track, gen_: u64) -> bool {
        self.generations.lock().unwrap().get(&(*peer, track)) == Some(&gen_)
    }

    // ----- speaking indicators -----

    pub fn speaking_cell(&self, peer: EndpointId, track: Track) -> Arc<AtomicU64> {
        self.speaking
            .lock()
            .unwrap()
            .entry((peer, track))
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    pub fn is_speaking(&self, peer: &EndpointId) -> bool {
        let now = self.epoch.now_ms();
        self.speaking.lock().unwrap().iter().any(|((p, t), v)| {
            p == peer && *t == Track::Mic && now.saturating_sub(v.load(Ordering::Relaxed)) < 300
        })
    }
}

pub mod once_epoch {
    use std::time::Instant;

    pub struct Epoch(Instant);
    impl Default for Epoch {
        fn default() -> Self {
            Self(Instant::now())
        }
    }
    impl Epoch {
        pub fn now_ms(&self) -> u64 {
            self.0.elapsed().as_millis() as u64
        }
    }
}

/// RMS of interleaved S16LE samples, in dBFS.
pub fn rms_dbfs(samples: &[u8]) -> f64 {
    let n = samples.len() / 2;
    if n == 0 {
        return -100.0;
    }
    let mut acc = 0f64;
    for c in samples.chunks_exact(2) {
        let s = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0;
        acc += s * s;
    }
    let rms = (acc / n as f64).sqrt();
    if rms <= 1e-9 { -100.0 } else { 20.0 * rms.log10() }
}

pub const SPEAKING_THRESHOLD_DB: f64 = -45.0;
