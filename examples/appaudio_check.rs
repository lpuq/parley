//! Checks screen-share audio routing against the live PipeWire graph:
//! another process's stream gets linked into our sink, our own never does.
use std::{process::{Command, Stdio}, thread::sleep, time::Duration};

use gst::prelude::*;
use parley::media::appaudio::{AppAudio, AudioShare, PwEvent};

fn links() -> String {
    String::from_utf8(Command::new("pw-link").arg("-l").output().unwrap().stdout).unwrap()
}

fn linked_into_share(l: &str, from: &str, sink: &str) -> bool {
    // pw-link -l prints "node:port\n  |-> other:port" blocks.
    let mut cur = "";
    for line in l.lines() {
        if !line.starts_with(' ') {
            cur = line;
        } else if cur.starts_with(from) && line.contains(&format!("|-> {sink}:")) {
            return true;
        }
    }
    false
}

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() {
    gst::init().unwrap();
    // A different process playing silence (an "app").
    let _other = KillOnDrop(Command::new("gst-launch-1.0")
        .args(["-q", "audiotestsrc", "volume=0", "is-live=true", "!", "pipewiresink",
               "stream-properties=props,application.name=OtherApp,node.name=otherapp"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    // Our own process playing (stands in for Parley's call audio).
    let own = gst::parse::launch(
        "audiotestsrc volume=0 is-live=true ! pipewiresink stream-properties=props,application.name=SelfTest,node.name=ownplayback",
    )
    .unwrap();
    own.set_state(gst::State::Playing).unwrap();

    let (tx, rx) = async_channel::unbounded();
    let aa = AppAudio::start(tx).expect("pipewire");
    let sink = aa.sink_name.clone();
    let mut apps = vec![];
    for _ in 0..60 {
        sleep(Duration::from_millis(250));
        while let Ok(ev) = rx.try_recv() {
            if let PwEvent::Apps(a) = ev {
                apps = a;
            }
        }
        if apps.iter().any(|a| a == "OtherApp") { break; }
    }
    println!("apps reported: {apps:?}");
    assert!(apps.iter().any(|a| a == "OtherApp"), "other app not listed");
    assert!(!apps.iter().any(|a| a == "SelfTest"), "our own stream must not be offered");
    assert!(!apps.iter().any(|a| a.contains("WirePlumber")), "system DSP nodes must not be offered");

    aa.set(AudioShare::Desktop);
    sleep(Duration::from_secs(2));
    let l = links();
    let ok_other = linked_into_share(&l, "otherapp:", &sink);
    let ok_own = linked_into_share(&l, "ownplayback:", &sink);
    println!("desktop mode: other linked={ok_other} own linked={ok_own}");
    assert!(ok_other && !ok_own);

    aa.set(AudioShare::Apps(vec!["SomethingElse".into()]));
    sleep(Duration::from_secs(1));
    let ok_other = linked_into_share(&links(), "otherapp:", &sink);
    println!("apps mode (other not selected): other linked={ok_other}");
    assert!(!ok_other);

    aa.set(AudioShare::Apps(vec!["OtherApp".into()]));
    sleep(Duration::from_secs(1));
    let ok_other = linked_into_share(&links(), "otherapp:", &sink);
    println!("apps mode (other selected): other linked={ok_other}");
    assert!(ok_other);

    aa.set(AudioShare::Off);
    sleep(Duration::from_secs(1));
    let gone = !links().contains(&sink);
    println!("off: sink removed={gone}");
    assert!(gone);
    println!("PASS");
    drop(_other);
    // Tearing down the in-process test stream can stall on some setups; nothing to clean.
    std::mem::forget(own);
    std::process::exit(0);
}
