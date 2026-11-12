//! Microphone voice processing, applied after echo cancellation: neural noise
//! suppression (RNNoise, via `nnnoiseless`), a voice gate ("input sensitivity"),
//! and automatic gain that keeps speech at a steady, Discord-like loudness.
//!
//! Runs on 10 ms frames (480 samples at 48 kHz mono), adding 10 ms of latency.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering},
    },
};

use nnnoiseless::DenoiseState;

const FRAME: usize = DenoiseState::FRAME_SIZE;
/// Speech loudness the automatic gain aims for (RMS of voiced 10 ms frames).
const TARGET_DB: f32 = -20.0;
const MAX_GAIN_DB: f32 = 24.0;
const MIN_GAIN_DB: f32 = -12.0;
/// Peaks are held below this (a hair under full scale).
const CEILING: f32 = 0.89 * 32768.0;

/// When the voice gate passes sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Always open.
    Off = 0,
    /// Open while the neural voice detector hears speech.
    Auto = 1,
    /// Open while the input is louder than the threshold.
    Threshold = 2,
}

/// Settings shared with the UI; changes apply immediately, without restarting the mic.
pub struct VoiceParams {
    pub denoise: AtomicBool,
    gate: AtomicU8,
    /// Gate threshold in tenths of a dB (-1000..0).
    threshold: AtomicI32,
    pub auto_gain: AtomicBool,
    /// Input level of the latest frame in tenths of a dB (for the sensitivity meter).
    level: AtomicI32,
    /// Whether the gate is currently letting sound through.
    pub open: AtomicBool,
}

impl VoiceParams {
    pub fn new(denoise: bool, gate: Gate, threshold_db: f32, auto_gain: bool) -> Arc<Self> {
        let p = Arc::new(Self {
            denoise: AtomicBool::new(denoise),
            gate: AtomicU8::new(gate as u8),
            threshold: AtomicI32::new(0),
            auto_gain: AtomicBool::new(auto_gain),
            level: AtomicI32::new(-1000),
            open: AtomicBool::new(false),
        });
        p.set_threshold_db(threshold_db);
        p
    }

    pub fn gate(&self) -> Gate {
        match self.gate.load(Ordering::Relaxed) {
            1 => Gate::Auto,
            2 => Gate::Threshold,
            _ => Gate::Off,
        }
    }

    pub fn set_gate(&self, g: Gate) {
        self.gate.store(g as u8, Ordering::Relaxed);
    }

    pub fn threshold_db(&self) -> f32 {
        self.threshold.load(Ordering::Relaxed) as f32 / 10.0
    }

    pub fn set_threshold_db(&self, db: f32) {
        self.threshold.store((db.clamp(-100.0, 0.0) * 10.0) as i32, Ordering::Relaxed);
    }

    /// Current input level in dBFS (about -100 when silent).
    pub fn level_db(&self) -> f32 {
        self.level.load(Ordering::Relaxed) as f32 / 10.0
    }
}

pub struct VoiceProcessor {
    params: Arc<VoiceParams>,
    rnn: Box<DenoiseState<'static>>,
    input: VecDeque<f32>,
    output: VecDeque<f32>,
    /// Gate gain (0..1), ramped per sample to avoid clicks.
    gate_gain: f32,
    hold: u32,
    /// Smoothed speech level (dBFS) and the gain applied for it.
    speech_db: f32,
    speech_frames: u32,
    gain_db: f32,
    limiter: f32,
}

impl VoiceProcessor {
    pub fn new(params: Arc<VoiceParams>) -> Self {
        let open = params.gate() == Gate::Off;
        Self {
            params,
            rnn: DenoiseState::new(),
            input: VecDeque::with_capacity(4 * FRAME),
            // One frame of delay, so every output sample is already processed.
            output: std::iter::repeat_n(0.0, FRAME).collect(),
            gate_gain: if open { 1.0 } else { 0.0 },
            hold: 0,
            speech_db: TARGET_DB,
            speech_frames: 0,
            gain_db: 0.0,
            limiter: 1.0,
        }
    }

    /// Processes interleaved mono S16 samples in place.
    pub fn process(&mut self, samples: &mut [i16]) {
        self.input.extend(samples.iter().map(|&s| s as f32));
        while self.input.len() >= FRAME {
            let mut frame = [0f32; FRAME];
            for (d, s) in frame.iter_mut().zip(self.input.drain(..FRAME)) {
                *d = s;
            }
            self.frame(&mut frame);
            self.output.extend(frame);
        }
        for s in samples.iter_mut() {
            *s = self.output.pop_front().unwrap_or(0.0).clamp(-32768.0, 32767.0) as i16;
        }
    }

    fn frame(&mut self, x: &mut [f32; FRAME]) {
        let p = &self.params;
        let gate = p.gate();
        let denoise = p.denoise.load(Ordering::Relaxed);

        // RNNoise also yields a voice probability, which the auto gate and the
        // gain control use even when its denoised output isn't wanted.
        let mut voice = 0.0;
        if denoise || gate == Gate::Auto || p.auto_gain.load(Ordering::Relaxed) {
            let mut out = [0f32; FRAME];
            voice = self.rnn.process_frame(&mut out, x);
            if denoise {
                *x = out;
            }
        }

        let rms = (x.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt() / 32768.0;
        let db = 20.0 * rms.max(1e-5).log10();
        p.level.store((db * 10.0) as i32, Ordering::Relaxed);

        // Voice gate with a hold time, so word endings and short pauses aren't chopped.
        let (want, hold_frames) = match gate {
            Gate::Off => (true, 0),
            Gate::Auto => (voice > 0.5 && db > -70.0, 40),
            Gate::Threshold => (db > p.threshold_db(), 30),
        };
        if want {
            self.hold = hold_frames;
        } else {
            self.hold = self.hold.saturating_sub(1);
        }
        let open = want || self.hold > 0;
        p.open.store(open, Ordering::Relaxed);

        // Automatic gain: learn the speaker's level only from voiced frames, then
        // steer towards the target. Rises slowly (no pumping on pauses), falls fast.
        if p.auto_gain.load(Ordering::Relaxed) {
            if voice > 0.7 && db > -65.0 {
                let alpha = if self.speech_frames < 150 { 0.1 } else { 0.03 };
                self.speech_frames += 1;
                self.speech_db += (db - self.speech_db) * alpha;
            }
            let target = (TARGET_DB - self.speech_db).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
            let step = if target > self.gain_db { 0.08 } else { 0.6 };
            self.gain_db += (target - self.gain_db).clamp(-step, step);
        } else {
            self.gain_db = 0.0;
        }
        let gain = 10f32.powf(self.gain_db / 20.0);

        // Limiter: instant attack per frame, ~100 ms release.
        let peak = x.iter().fold(0f32, |m, s| m.max(s.abs())) * gain;
        let want_lim = if peak > CEILING { CEILING / peak } else { 1.0 };
        let lim_from = self.limiter;
        self.limiter = if want_lim < self.limiter { want_lim } else { (self.limiter + 0.1).min(want_lim) };

        // Apply, ramping the gate (5 ms open, 60 ms close) and the limiter smoothly.
        let target_gate = if open { 1.0 } else { 0.0 };
        let (up, down) = (1.0 / 240.0, 1.0 / 2880.0);
        for (i, s) in x.iter_mut().enumerate() {
            if self.gate_gain < target_gate {
                self.gate_gain = (self.gate_gain + up).min(1.0);
            } else if self.gate_gain > target_gate {
                self.gate_gain = (self.gate_gain - down).max(0.0);
            }
            let t = i as f32 / FRAME as f32;
            let lim = if self.limiter < lim_from { self.limiter } else { lim_from + (self.limiter - lim_from) * t };
            *s *= self.gate_gain * gain * lim;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: f32, n: usize) -> Vec<i16> {
        (0..n).map(|i| (amp * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / 48_000.0).sin()) as i16).collect()
    }

    #[test]
    fn threshold_gate_blocks_quiet_and_passes_loud() {
        let p = VoiceParams::new(false, Gate::Threshold, -30.0, false);
        let mut v = VoiceProcessor::new(p.clone());
        let mut quiet = tone(100.0, 48_000); // about -50 dBFS
        v.process(&mut quiet);
        assert!(quiet[24_000..].iter().all(|s| s.abs() < 2), "quiet input leaked through the gate");
        let mut loud = tone(8000.0, 48_000); // about -15 dBFS
        v.process(&mut loud);
        assert!(loud[24_000..].iter().any(|s| s.abs() > 6000), "loud input was gated");
        assert!(p.open.load(Ordering::Relaxed));
    }

    #[test]
    fn studio_mode_is_transparent_apart_from_delay() {
        let p = VoiceParams::new(false, Gate::Off, -100.0, false);
        let mut v = VoiceProcessor::new(p);
        let input = tone(5000.0, 9600);
        let mut out = input.clone();
        v.process(&mut out);
        assert_eq!(&out[FRAME..], &input[..input.len() - FRAME]);
    }
}
