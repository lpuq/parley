fn main() {
    gst::init().unwrap();
    use parley::media::devices;
    for (k, l) in [("mic", devices::microphones(true)), ("speaker", devices::speakers()), ("camera", devices::cameras())] {
        for d in l {
            println!("{k}: {}", d.name);
        }
    }
}
