//! Runs an image (e.g. a photo/screenshot of a Parley QR) through the scanner's decode path.
use gst::prelude::*;
fn main() {
    gst::init().unwrap();
    let path = std::env::args().nth(1).expect("image path");
    let p = gst::parse::launch(&format!(
        "filesrc location={path} ! decodebin ! videoconvert ! videoscale ! video/x-raw,format=GRAY8,width=960,height=540 ! appsink name=s"
    )).unwrap().downcast::<gst::Pipeline>().unwrap();
    let sink = p.by_name("s").unwrap().downcast::<gst_app::AppSink>().unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let sample = sink.pull_sample().expect("frame");
    let info = gst_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
    let map = sample.buffer().unwrap().map_readable().unwrap();
    let (w, h, stride) = (info.width() as usize, info.height() as usize, info.stride()[0] as usize);
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| map[y * stride + x]);
    for g in img.detect_grids() {
        match g.decode() {
            Ok((_, t)) => println!("decoded: {}… valid={}", &t[..t.len().min(24)], parley::store::ShareCode::decode(&t).is_ok()),
            Err(e) => println!("grid found, decode failed: {e:?}"),
        }
    }
    p.set_state(gst::State::Null).unwrap();
}
