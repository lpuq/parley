//! Measures call audio quality end to end: node A sends a 440 Hz tone (PARLEY_TEST_TONE),
//! node B records what it would play (PARLEY_TEST_AUDIO_FILE). Combine with
//! PARLEY_TEST_JITTER_MS / PARLEY_TEST_LOSS_PCT to simulate a bad network.
//!   cargo run --example audio_path_check -- out.raw 20
use std::time::Duration;

use parley::{hub::Hub, media::{recv::AudioOut, voice::{Gate, VoiceParams},
        send::{MicOptions, MicSender}}, net::{self, NetCmd, UiEvent}, proto::Track, store::Contact};

fn main() {
    tracing_subscriber::fmt().with_env_filter("parley=info").init();
    gst::init().unwrap();
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("output file");
    let secs: u64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(20);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (ha, hb) = (Hub::new(), Hub::new());
    let (ta, ra) = async_channel::unbounded();
    let (tb, rb) = async_channel::unbounded();
    let (ka, kb) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
    let (ida, idb) = (ka.public(), kb.public());
    let (na, nb) = rt.block_on(async {
        (net::start(ka, ha.clone(), ta).await.unwrap(), net::start(kb, hb.clone(), tb).await.unwrap())
    });
    for (n, me, other) in [(&na, "A", idb), (&nb, "B", ida)] {
        n.send(NetCmd::Directory { my_name: me.into(), contacts: vec![Contact::new(other, "x")], groups: vec![], allow_unknown: true });
    }
    std::thread::sleep(Duration::from_secs(2));
    na.send(NetCmd::CallDirect(idb));
    let room = rt.block_on(async {
        loop {
            if let Ok(UiEvent::Incoming { room, .. }) = rb.recv().await { break room; }
        }
    });
    nb.send(NetCmd::Accept(room));
    rt.block_on(async { while !matches!(ra.recv().await, Ok(UiEvent::PeerJoined { .. })) {} });

    let ml = gtk::glib::MainLoop::new(None, false);
    unsafe { std::env::set_var("PARLEY_TEST_AUDIO_FILE", &out) };
    let mut audio = AudioOut::start(None, |e| eprintln!("OUT ERROR {e}")).unwrap();
    audio.add(&hb, ida, Track::Mic).unwrap();
    let opts = MicOptions { device: None, echo_cancel: false, webrtc_ns: false, voice: VoiceParams::new(false, Gate::Off, -100.0, false) };
    let mic = MicSender::start(ha.clone(), ida, opts, false, |e| eprintln!("MIC ERROR {e}")).unwrap();
    let ml2 = ml.clone();
    gtk::glib::timeout_add_seconds_local_once(secs as u32, move || ml2.quit());
    ml.run();
    drop(mic);
    drop(audio);
    std::thread::sleep(Duration::from_millis(500));
    println!("recorded {out}");
}
