//! Prints the default microphone as Parley sees it and whether it's Bluetooth.
fn main() {
    gst::init().unwrap();
    let (tx, _rx) = async_channel::unbounded();
    let aa = parley::media::appaudio::AppAudio::start(tx).expect("pipewire");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let def = aa.default_source.lock().unwrap().clone();
    println!("default microphone: {def:?}");
    for m in parley::media::devices::microphones(true) {
        println!("  {} node={:?} bluetooth={}", m.name, parley::media::devices::node_name(&m), parley::media::devices::is_bluetooth(&m));
    }
    std::process::exit(0);
}
