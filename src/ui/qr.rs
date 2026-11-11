//! QR codes: show a share code, or read one with the webcam.

use std::rc::Rc;

use adw::prelude::*;
use gst::prelude::*;
use gtk::{gdk, glib};
use qrcode::{Color, EcLevel, QrCode};

use super::App;
use crate::media::{Running, caps_filter, devices, make, send::camera_source};

/// Renders `text` as a crisp QR texture (dark modules on white, 4-module quiet zone).
pub fn texture(text: &str, px_per_module: usize) -> Option<gdk::Texture> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L).ok()?;
    let n = code.width();
    let colors = code.to_colors();
    let quiet = 4;
    let side = (n + 2 * quiet) * px_per_module;
    let mut rgb = vec![255u8; side * side * 3];
    for my in 0..n {
        for mx in 0..n {
            if colors[my * n + mx] != Color::Dark {
                continue;
            }
            for dy in 0..px_per_module {
                let y = (my + quiet) * px_per_module + dy;
                for dx in 0..px_per_module {
                    let x = (mx + quiet) * px_per_module + dx;
                    let i = (y * side + x) * 3;
                    rgb[i..i + 3].copy_from_slice(&[0x10, 0x12, 0x14]);
                }
            }
        }
    }
    let bytes = glib::Bytes::from_owned(rgb);
    Some(
        gdk::MemoryTexture::new(side as i32, side as i32, gdk::MemoryFormat::R8g8b8, &bytes, side * 3)
            .upcast(),
    )
}

/// Dialog showing a code as a QR plus a copy button.
pub fn show(app: &Rc<App>, title: &str, hint: &str, code: &str) {
    let d = adw::Dialog::new();
    d.set_title(title);
    d.set_content_width(420);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    let b = gtk::Box::new(gtk::Orientation::Vertical, 14);
    b.set_margin_start(24);
    b.set_margin_end(24);
    b.set_margin_bottom(24);
    b.set_margin_top(6);
    if let Some(tex) = texture(code, 8) {
        let pic = gtk::Picture::for_paintable(&tex);
        pic.set_can_shrink(true);
        pic.set_size_request(320, 320);
        pic.add_css_class("qr");
        b.append(&pic);
    }
    let l = gtk::Label::new(Some(hint));
    l.set_wrap(true);
    l.set_justify(gtk::Justification::Center);
    l.add_css_class("dim-label");
    b.append(&l);
    let copy = gtk::Button::builder()
        .label("Copy as text")
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .build();
    let (a, c) = (app.clone(), code.to_string());
    copy.connect_clicked(move |_| a.copy(&c, "Code"));
    b.append(&copy);
    tv.set_content(Some(&b));
    d.set_child(Some(&tv));
    d.present(Some(&app.window));
}

/// Opens the camera and calls `found` with the first Parley code it reads.
pub fn scan(app: &Rc<App>, found: impl Fn(String) + 'static) {
    let cams = devices::cameras();
    let want = app.store.borrow().settings.camera.clone();
    let Some(dev) = devices::find(&cams, &want).or(cams.first()).map(|d| d.device.clone()) else {
        app.toast("No camera found.");
        return;
    };
    if app.state.get().camera {
        app.toast("Turn off your camera in the call first.");
        return;
    }

    let (tx, rx) = async_channel::bounded::<String>(1);
    let built = (|| -> anyhow::Result<(Running, gdk::Paintable)> {
        let pipeline = gst::Pipeline::with_name("qr-scan");
        let (head, _, _, _) = camera_source(&dev)?;
        let conv = make("videoconvert")?;
        let tee = make("tee")?;
        // Preview.
        let q1 = make("queue")?;
        q1.set_property_from_str("leaky", "downstream");
        q1.set_property("max-size-buffers", 1u32);
        let pscale = make("videoscale")?;
        let pconv = make("videoconvert")?;
        let pcaps = caps_filter("video/x-raw,format={BGRA,RGBA,BGRx,RGBx},width=640,height=360")?;
        let psink = make("gtk4paintablesink")?;
        psink.set_property("sync", false);
        let paintable = psink.property::<gdk::Paintable>("paintable");
        // Detection: small greyscale frames, newest only.
        let q2 = make("queue")?;
        q2.set_property_from_str("leaky", "downstream");
        q2.set_property("max-size-buffers", 1u32);
        let dscale = make("videoscale")?;
        let dconv = make("videoconvert")?;
        let dcaps = caps_filter("video/x-raw,format=GRAY8,width=960,height=540")?;
        let sink = gst_app::AppSink::builder().sync(false).max_buffers(1).drop(true).build();
        let tx2 = tx.clone();
        let mut n = 0u32;
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    n += 1;
                    if n % 3 != 0 || tx2.is_full() {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let (Some(buf), Some(caps)) = (sample.buffer(), sample.caps()) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let Ok(info) = gst_video::VideoInfo::from_caps(caps) else { return Ok(gst::FlowSuccess::Ok) };
                    let Ok(map) = buf.map_readable() else { return Ok(gst::FlowSuccess::Ok) };
                    let (w, h, stride) = (info.width() as usize, info.height() as usize, info.stride()[0] as usize);
                    let data: &[u8] = &map;
                    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| data[y * stride + x]);
                    for grid in img.detect_grids() {
                        if let Ok((_, text)) = grid.decode() {
                            if text.contains("parley:") {
                                let _ = tx2.try_send(text);
                            }
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let mut all = head.clone();
        all.extend([conv.clone(), tee.clone(), q1.clone(), pscale.clone(), pconv.clone(), pcaps.clone(), psink.clone()]);
        all.extend([q2.clone(), dscale.clone(), dconv.clone(), dcaps.clone(), sink.clone().upcast()]);
        pipeline.add_many(&all)?;
        let mut first = head;
        first.extend([conv, tee.clone()]);
        gst::Element::link_many(&first)?;
        gst::Element::link_many([&tee, &q1, &pscale, &pconv, &pcaps, &psink])?;
        gst::Element::link_many([&tee, &q2, &dscale, &dconv, &dcaps, sink.upcast_ref()])?;
        let weak = Rc::downgrade(app);
        let run = Running::start(pipeline, "Camera", move |e| {
            if let Some(a) = weak.upgrade() {
                a.toast(&e);
            }
        })?;
        Ok((run, paintable))
    })();
    let (run, paintable) = match built {
        Ok(x) => x,
        Err(e) => {
            app.toast(&format!("Can't open the camera: {e:#}"));
            return;
        }
    };

    let d = adw::Dialog::new();
    d.set_title("Scan a code");
    d.set_content_width(520);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    b.set_margin_start(18);
    b.set_margin_end(18);
    b.set_margin_bottom(18);
    let pic = gtk::Picture::for_paintable(&paintable);
    pic.set_size_request(480, 270);
    pic.add_css_class("tile");
    pic.set_overflow(gtk::Overflow::Hidden);
    b.append(&pic);
    let l = gtk::Label::new(Some("Hold the QR code from your friend's Parley in front of the camera."));
    l.set_wrap(true);
    l.add_css_class("dim-label");
    b.append(&l);
    tv.set_content(Some(&b));
    d.set_child(Some(&tv));

    // The pipeline lives as long as the dialog.
    let run = Rc::new(std::cell::RefCell::new(Some(run)));
    let r2 = run.clone();
    d.connect_closed(move |_| {
        r2.borrow_mut().take();
    });
    let d2 = d.clone();
    glib::spawn_future_local(async move {
        if let Ok(text) = rx.recv().await {
            run.borrow_mut().take();
            d2.close();
            found(text);
        }
    });
    d.present(Some(&app.window));
}

#[cfg(test)]
mod tests {
    #[test]
    fn qr_roundtrip() {
        // Encode a realistic group code and read it back with the scanner's decoder.
        let code = format!("parley:{}", "A".repeat(540));
        let qr = qrcode::QrCode::with_error_correction_level(code.as_bytes(), qrcode::EcLevel::L).unwrap();
        let n = qr.width();
        let colors = qr.to_colors();
        let (scale, quiet) = (4, 4);
        let side = (n + 2 * quiet) * scale;
        let mut img = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| {
            let (mx, my) = ((x / scale) as isize - quiet as isize, (y / scale) as isize - quiet as isize);
            if mx < 0 || my < 0 || mx >= n as isize || my >= n as isize {
                return 255;
            }
            if colors[my as usize * n + mx as usize] == qrcode::Color::Dark { 0 } else { 255 }
        });
        let grids = img.detect_grids();
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].decode().unwrap().1, code);
    }
}
