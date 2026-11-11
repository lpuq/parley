//! Runs Parley's screen-share pipeline on a PipeWire screencast node and reports
//! negotiated caps and frame brightness in the log. Needs a GTK display (e.g. broadway).
//!   cargo run --example screen_check -- <pipewire-node-id> [width height]
fn main() {
    tracing_subscriber::fmt().with_env_filter("parley=info").init();
    gst::init().unwrap();
    gtk::init().unwrap();
    let mut a = std::env::args().skip(1);
    let node: u32 = a.next().expect("node id").parse().unwrap();
    let w: i32 = a.next().map(|v| v.parse().unwrap()).unwrap_or(1920);
    let h: i32 = a.next().map(|v| v.parse().unwrap()).unwrap_or(1080);
    let hub = parley::hub::Hub::new();
    let s = parley::media::send::VideoSender::screen(hub, None, node, Some((w, h)), (1080, 30, 4500), true, |e| {
        eprintln!("PIPELINE ERROR: {e}")
    })
    .unwrap();
    let ml = gtk::glib::MainLoop::new(None, false);
    let ml2 = ml.clone();
    gtk::glib::timeout_add_seconds_local_once(12, move || ml2.quit());
    ml.run();
    drop(s);
    println!("done");
}
