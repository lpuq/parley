//! End-to-end check of screen-share audio: a separate "app" plays a 440 Hz tone,
//! node A routes desktop audio into its share sink and sends it, node B records what
//! it would play. Prints a per-second loudness timeline of the received audio.
//!   cargo run --example screen_audio_check -- out.raw 30
use std::{
    process::{Command, Stdio},
    time::Duration,
};

use parley::{
    hub::Hub,
    media::{
        appaudio::{AppAudio, AudioShare, PwEvent},
        recv::AudioOut,
        send::{MicOptions, MicSender, ScreenAudioSender},
    },
    net::{self, NetCmd, UiEvent},
    proto::Track,
    store::Contact,
};

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() {
    tracing_subscriber::fmt().with_env_filter(std::env::var("PARLEY_LOG").unwrap_or("parley=info".into())).init();
    gst::init().unwrap();
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("output file");
    let secs: u32 = args.next().map(|s| s.parse().unwrap()).unwrap_or(30);

    let _app = KillOnDrop(
        Command::new("gst-launch-1.0")
            .args(["-q", "audiotestsrc", "freq=440", "volume=0.1", "is-live=true", "!", "pulsesink", "client-name=ToneApp", "device=parley_test_tone"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

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
        n.send(NetCmd::Directory { my_name: me.into(), contacts: vec![Contact { id: other, name: "x".into() }], groups: vec![], allow_unknown: true });
    }
    std::thread::sleep(Duration::from_secs(2));
    na.send(NetCmd::CallDirect(idb));
    let room = rt.block_on(async { loop { if let Ok(UiEvent::Incoming { room, .. }) = rb.recv().await { break room; } } });
    nb.send(NetCmd::Accept(room));
    rt.block_on(async { while !matches!(ra.recv().await, Ok(UiEvent::PeerJoined { .. })) {} });

    // Sender: route desktop audio (the tone app) into the share sink and send it.
    let (pt, pr) = async_channel::unbounded();
    let aa = AppAudio::start(pt).expect("pipewire");
    aa.set(AudioShare::Desktop);
    rt.block_on(async { loop { if let Ok(PwEvent::SinkReady) = pr.recv().await { break; } } });
    std::thread::sleep(Duration::from_millis(300));

    let ml = gtk::glib::MainLoop::new(None, false);
    // "-" = play through a real sound card (PARLEY_TEST_SPEAKER = device name) instead of a file.
    let devs = parley::media::devices::speakers();
    let spk = std::env::var("PARLEY_TEST_SPEAKER").ok().and_then(|n| devs.iter().find(|d| d.name == n).map(|d| d.device.clone()));
    if out != "-" {
        unsafe { std::env::set_var("PARLEY_TEST_AUDIO_FILE", &out) };
    }
    let mut audio = AudioOut::start(spk.as_ref(), |e| eprintln!("OUT ERROR {e}")).unwrap();
    // Voice: a 1000 Hz tone (PARLEY_TEST_TONE + PARLEY_TEST_TONE_FREQ) when PARLEY_TEST_TONE is set.
    let _mic = std::env::var_os("PARLEY_TEST_TONE").map(|_| {
        let opts = MicOptions { device: None, echo_cancel: false, noise_suppression: false, auto_gain: false };
        MicSender::start(ha.clone(), ida, opts, false, |e| eprintln!("MIC ERROR {e}")).unwrap()
    });
    // Like a real call: both branches exist from call start; screen audio begins later.
    audio.add(&hb, ida, Track::Mic).unwrap();
    audio.add(&hb, ida, Track::ScreenAudio).unwrap();
    let delay: u32 = std::env::var("PARLEY_TEST_SHARE_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let sender: std::rc::Rc<std::cell::RefCell<Option<ScreenAudioSender>>> = Default::default();
    let (s2, ha2, sink) = (sender.clone(), ha.clone(), aa.sink_name.clone());
    gtk::glib::timeout_add_seconds_local_once(delay, move || {
        *s2.borrow_mut() = Some(ScreenAudioSender::start(ha2, &sink, |e| eprintln!("SCREEN AUDIO ERROR {e}")).unwrap());
        eprintln!("SHARE AUDIO STARTED");
    });
    let ml2 = ml.clone();
    gtk::glib::timeout_add_seconds_local_once(secs, move || ml2.quit());
    ml.run();
    sender.borrow_mut().take();
    drop(audio);
    aa.set(AudioShare::Off);
    std::thread::sleep(Duration::from_millis(500));
    println!("recorded {out}");
}
