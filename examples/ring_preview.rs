//! Writes Parley's ringtones as WAV files (three rings each), to listen to them.
//!
//!     cargo run --example ring_preview -- out-dir

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    for (name, incoming) in [("ring-incoming.wav", true), ("ring-outgoing.wav", false)] {
        let pcm = parley::ui::ring_preview(incoming).repeat(3);
        let mut wav = Vec::with_capacity(44 + pcm.len());
        let n = pcm.len() as u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + n).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&n.to_le_bytes());
        wav.extend_from_slice(&pcm);
        let path = dir.join(name);
        std::fs::write(&path, wav).unwrap();
        println!("{}", path.display());
    }
}
