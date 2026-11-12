//! Runs a raw recording (S16LE, 48 kHz, mono) through the voice processor.
//!
//!     cargo run --example voice_check -- in.raw out.raw isolation|studio|standard|threshold:-48

use std::sync::atomic::Ordering;

use parley::media::voice::{Gate, VoiceParams, VoiceProcessor};

fn main() {
    let mut args = std::env::args().skip(1);
    let (input, output) = (args.next().expect("input"), args.next().expect("output"));
    let mode = args.next().unwrap_or("isolation".into());
    let params = match mode.as_str() {
        "studio" => VoiceParams::new(false, Gate::Off, -100.0, false),
        "isolation" => VoiceParams::new(true, Gate::Auto, -50.0, true),
        m if m.starts_with("threshold:") => VoiceParams::new(false, Gate::Threshold, m[10..].parse().unwrap(), true),
        m => panic!("unknown mode {m}"),
    };
    let bytes = std::fs::read(&input).unwrap();
    let mut samples: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let mut v = VoiceProcessor::new(params.clone());
    // Feed 20 ms buffers like the microphone pipeline does.
    let mut open_frames = 0;
    for chunk in samples.chunks_mut(960) {
        v.process(chunk);
        open_frames += params.open.load(Ordering::Relaxed) as usize;
    }
    let out: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&output, out).unwrap();
    println!("{mode}: gate open {open_frames} of {} buffers", samples.len() / 960);
}
