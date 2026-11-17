//! Pictures you choose for people (or yourself), GNOME style: an image, or letters or
//! a symbol on a colour or a gradient. They stay on this computer; nobody else sees
//! them.

use std::{cell::RefCell, rc::Rc};

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk, pango};
use iroh::EndpointId;

use super::{App, identity};
use crate::store::{Picture, PictureKind, Store, picture_path};

/// Pictures are drawn (and image pictures kept) at this size.
const SIZE: f32 = 256.0;

/// Symbols to choose from (Adwaita's, built in).
const SYMBOLS: &[&str] = &[
    "parley-avatar-default-symbolic",
    "parley-face-smile-big-symbolic",
    "parley-face-laugh-symbolic",
    "parley-face-cool-symbolic",
    "parley-face-wink-symbolic",
    "parley-face-angel-symbolic",
    "parley-face-devilish-symbolic",
    "parley-face-monkey-symbolic",
    "parley-face-glasses-symbolic",
    "parley-face-surprise-symbolic",
    "parley-face-kiss-symbolic",
    "parley-face-raspberry-symbolic",
    "parley-emoji-nature-symbolic",
    "parley-emoji-food-symbolic",
    "parley-emoji-activities-symbolic",
    "parley-emoji-travel-symbolic",
    "parley-emoji-objects-symbolic",
    "parley-emoji-symbols-symbolic",
    "parley-emoji-flags-symbolic",
    "parley-starred-symbolic",
    "parley-weather-clear-symbolic",
    "parley-weather-clear-night-symbolic",
    "parley-applications-games-symbolic",
    "parley-input-gaming-symbolic",
    "parley-audio-x-generic-symbolic",
    "parley-camera-photo-symbolic",
    "parley-applications-science-symbolic",
    "parley-applications-engineering-symbolic",
    "parley-applications-graphics-symbolic",
    "parley-airplane-mode-symbolic",
];

/// Backgrounds to choose from: GNOME's palette, then gradients across it.
const BACKGROUNDS: &[(&str, Option<&str>)] = &[
    ("#3584e4", None),
    ("#2190a4", None),
    ("#3a944a", None),
    ("#c88800", None),
    ("#ed5b00", None),
    ("#e62d42", None),
    ("#d56199", None),
    ("#9141ac", None),
    ("#6f8396", None),
    ("#3584e4", Some("#9141ac")),
    ("#2190a4", Some("#3a944a")),
    ("#c88800", Some("#e62d42")),
    ("#ed5b00", Some("#d56199")),
    ("#d56199", Some("#9141ac")),
    ("#3584e4", Some("#2190a4")),
    ("#3a944a", Some("#c88800")),
    ("#9141ac", Some("#e62d42")),
    ("#6f8396", Some("#3584e4")),
];

/// Draws every saved picture, at startup.
pub fn load_all(st: &Store) {
    for p in &st.pictures {
        identity::set_picture(p.id, texture_for(p));
    }
}

fn texture_for(p: &Picture) -> Option<gdk::Texture> {
    match &p.kind {
        PictureKind::Image => gdk::Texture::from_filename(picture_path(&p.id)).ok(),
        PictureKind::Made { letters, icon, from, to } => Some(draw(letters, icon.as_deref(), &rgba(from), to.as_deref().map(rgba).as_ref())),
    }
}

fn rgba(hex: &str) -> gdk::RGBA {
    gdk::RGBA::parse(hex).unwrap_or(gdk::RGBA::new(0.4, 0.4, 0.4, 1.0))
}

fn hex(c: &gdk::RGBA) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c.red()), b(c.green()), b(c.blue()))
}

/// White on dark backgrounds, near-black on light ones.
fn foreground(from: &gdk::RGBA, to: Option<&gdk::RGBA>) -> gdk::RGBA {
    let lum = |c: &gdk::RGBA| 0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue();
    let l = to.map_or(lum(from), |t| (lum(from) + lum(t)) / 2.0);
    if l > 0.62 { gdk::RGBA::new(0.0, 0.0, 0.0, 0.8) } else { gdk::RGBA::WHITE }
}

/// Letters (or a symbol, if `icon`) on a colour, or a diagonal gradient.
fn draw(letters: &str, icon: Option<&str>, from: &gdk::RGBA, to: Option<&gdk::RGBA>) -> gdk::Texture {
    let snap = gtk::Snapshot::new();
    let all = graphene::Rect::new(0.0, 0.0, SIZE, SIZE);
    match to {
        Some(to) => snap.append_linear_gradient(
            &all,
            &graphene::Point::new(0.0, 0.0),
            &graphene::Point::new(SIZE, SIZE),
            &[gsk::ColorStop::new(0.0, *from), gsk::ColorStop::new(1.0, *to)],
        ),
        None => snap.append_color(from, &all),
    }
    let fg = foreground(from, to);
    if let Some(icon) = icon {
        let px = (SIZE * 0.5) as i32;
        let display = gdk::Display::default().expect("display");
        let paintable = gtk::IconTheme::for_display(&display).lookup_icon(
            icon,
            &[],
            px,
            1,
            gtk::TextDirection::None,
            gtk::IconLookupFlags::FORCE_SYMBOLIC,
        );
        let off = (SIZE - px as f32) / 2.0;
        snap.save();
        snap.translate(&graphene::Point::new(off, off));
        paintable.snapshot_symbolic(&snap, px as f64, px as f64, &[fg]);
        snap.restore();
    } else if !letters.is_empty() {
        let layout = pango::Layout::new(&gtk::Label::new(None).pango_context());
        let mut font = pango::FontDescription::from_string("Sans Bold");
        let scale = if letters.chars().count() > 2 { 0.32 } else { 0.42 };
        font.set_absolute_size(SIZE as f64 * scale * pango::SCALE as f64);
        layout.set_font_description(Some(&font));
        layout.set_text(letters);
        let (ink, _) = layout.pixel_extents();
        snap.save();
        snap.translate(&graphene::Point::new(
            (SIZE - ink.width() as f32) / 2.0 - ink.x() as f32,
            (SIZE - ink.height() as f32) / 2.0 - ink.y() as f32,
        ));
        snap.append_layout(&layout, &fg);
        snap.restore();
    }
    render(snap)
}

fn render(snap: gtk::Snapshot) -> gdk::Texture {
    let node = snap.to_node().expect("something drawn");
    let renderer = gsk::CairoRenderer::new();
    let _ = renderer.realize_for_display(&gdk::Display::default().expect("display"));
    let t = renderer.render_texture(&node, Some(&graphene::Rect::new(0.0, 0.0, SIZE, SIZE)));
    renderer.unrealize();
    t
}

/// An image file, cropped to its middle square and scaled to [`SIZE`].
fn square_image(file: &gio::File) -> Result<gdk::Texture, glib::Error> {
    let src = gdk::Texture::from_file(file)?;
    let (w, h) = (src.width() as f32, src.height() as f32);
    let k = SIZE / w.min(h);
    let snap = gtk::Snapshot::new();
    snap.push_clip(&graphene::Rect::new(0.0, 0.0, SIZE, SIZE));
    snap.append_scaled_texture(&src, gsk::ScalingFilter::Trilinear, &graphene::Rect::new((SIZE - w * k) / 2.0, (SIZE - h * k) / 2.0, w * k, h * k));
    snap.pop();
    Ok(render(snap))
}

/// The two letters a GNOME avatar would show: first letters of the first two words.
fn initials(name: &str) -> String {
    name.split_whitespace().filter_map(|w| w.chars().next()).take(2).flat_map(char::to_uppercase).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Letters,
    Symbol,
    Image,
}

struct Draft {
    mode: Mode,
    letters: String,
    icon: String,
    from: gdk::RGBA,
    to: Option<gdk::RGBA>,
    image: Option<gdk::Texture>,
}

impl Draft {
    fn texture(&self) -> Option<gdk::Texture> {
        match self.mode {
            Mode::Letters => Some(draw(&self.letters, None, &self.from, self.to.as_ref())),
            Mode::Symbol => Some(draw("", Some(&self.icon), &self.from, self.to.as_ref())),
            Mode::Image => self.image.clone(),
        }
    }
}

/// The picture editor for `id` (us, a friend, or anyone in a group).
pub fn edit(app: &Rc<App>, id: EndpointId, name: &str) {
    let me = id == app.me();
    let saved = app.store.borrow().picture(&id).cloned();
    let mut draft = Draft {
        mode: Mode::Letters,
        letters: initials(name),
        icon: SYMBOLS[0].to_string(),
        from: rgba(BACKGROUNDS[(id.as_bytes()[0] as usize) % 9].0),
        to: None,
        image: None,
    };
    match saved.as_ref().map(|p| &p.kind) {
        Some(PictureKind::Image) => {
            draft.mode = Mode::Image;
            draft.image = gdk::Texture::from_filename(picture_path(&id)).ok();
        }
        Some(PictureKind::Made { letters, icon, from, to }) => {
            draft.mode = if icon.is_some() { Mode::Symbol } else { Mode::Letters };
            draft.letters = letters.clone();
            if let Some(i) = icon {
                draft.icon = i.clone();
            }
            draft.from = rgba(from);
            draft.to = to.as_deref().map(rgba);
        }
        None => {}
    }
    let draft = Rc::new(RefCell::new(draft));

    let d = adw::Dialog::new();
    d.set_title(if me { "Your picture" } else { "Picture" });
    d.set_content_width(440);
    let tv = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    header.pack_end(&save);
    tv.add_top_bar(&header);

    let col = gtk::Box::new(gtk::Orientation::Vertical, 14);
    col.set_margin_start(18);
    col.set_margin_end(18);
    col.set_margin_top(6);
    col.set_margin_bottom(18);
    let preview = adw::Avatar::new(112, Some(name), true);
    preview.set_halign(gtk::Align::Center);
    col.append(&preview);
    let note = gtk::Label::new(Some(if me {
        "Only on this computer: friends see you as their Parley shows you."
    } else {
        "Only on this computer: nobody else sees it."
    }));
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    note.set_wrap(true);
    note.set_justify(gtk::Justification::Center);
    col.append(&note);

    // Letters / Symbol / Image.
    let modes = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    modes.add_css_class("linked");
    modes.set_halign(gtk::Align::Center);
    let b_letters = gtk::ToggleButton::with_label("Letters");
    let b_symbol = gtk::ToggleButton::with_label("Symbol");
    let b_image = gtk::ToggleButton::with_label("Image");
    b_symbol.set_group(Some(&b_letters));
    b_image.set_group(Some(&b_letters));
    for b in [&b_letters, &b_symbol, &b_image] {
        modes.append(b);
    }
    col.append(&modes);

    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.set_interpolate_size(true);
    let letters = gtk::Entry::builder().max_length(3).width_chars(6).xalign(0.5).halign(gtk::Align::Center).build();
    letters.set_text(&draft.borrow().letters);
    stack.add_named(&letters, Some("letters"));
    let symbols = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(10).min_children_per_line(6).homogeneous(true).build();
    stack.add_named(&symbols, Some("symbol"));
    let choose = gtk::Button::builder().label("Choose an image…").halign(gtk::Align::Center).css_classes(["pill"]).build();
    stack.add_named(&choose, Some("image"));
    col.append(&stack);

    // Backgrounds (letters and symbols).
    let bg_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let bg_label = gtk::Label::new(Some("Background"));
    bg_label.add_css_class("heading");
    bg_label.set_xalign(0.0);
    bg_box.append(&bg_label);
    let swatches = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(9).min_children_per_line(9).homogeneous(true).build();
    bg_box.append(&swatches);
    let custom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    custom.append(&gtk::Label::new(Some("Custom")));
    let from_btn = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
    let gradient = gtk::CheckButton::with_label("Gradient to");
    let to_btn = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
    custom.append(&from_btn);
    custom.append(&gradient);
    custom.append(&to_btn);
    bg_box.append(&custom);
    col.append(&bg_box);

    let reset = gtk::Button::builder().label("Use the default picture").halign(gtk::Align::Center).css_classes(["flat"]).build();
    reset.set_visible(saved.is_some());
    col.append(&reset);

    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).child(&col).build();
    tv.set_content(Some(&scroll));
    d.set_child(Some(&tv));

    // Keeps the preview, the controls and Save in step with the draft.
    let sync = {
        let (draft, preview, stack, bg_box, save, from_btn, to_btn, gradient) =
            (draft.clone(), preview.clone(), stack.clone(), bg_box.clone(), save.clone(), from_btn.clone(), to_btn.clone(), gradient.clone());
        Rc::new(move || {
            let dr = draft.borrow();
            stack.set_visible_child_name(match dr.mode {
                Mode::Letters => "letters",
                Mode::Symbol => "symbol",
                Mode::Image => "image",
            });
            bg_box.set_visible(dr.mode != Mode::Image);
            from_btn.set_rgba(&dr.from);
            gradient.set_active(dr.to.is_some());
            to_btn.set_sensitive(dr.to.is_some());
            if let Some(t) = dr.to {
                to_btn.set_rgba(&t);
            }
            let tex = dr.texture();
            save.set_sensitive(tex.is_some());
            preview.set_custom_image(tex.as_ref());
        })
    };
    // Setting the colour buttons from `sync` fires their handlers: ignore those.
    let syncing = Rc::new(std::cell::Cell::new(false));
    let update = {
        let (sync, syncing) = (sync.clone(), syncing.clone());
        Rc::new(move || {
            syncing.set(true);
            sync();
            syncing.set(false);
        })
    };

    match draft.borrow().mode {
        Mode::Letters => b_letters.set_active(true),
        Mode::Symbol => b_symbol.set_active(true),
        Mode::Image => b_image.set_active(true),
    }
    for (b, m) in [(&b_letters, Mode::Letters), (&b_symbol, Mode::Symbol), (&b_image, Mode::Image)] {
        let (draft, update) = (draft.clone(), update.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                draft.borrow_mut().mode = m;
                update();
            }
        });
    }
    {
        let (draft, update) = (draft.clone(), update.clone());
        letters.connect_changed(move |e| {
            draft.borrow_mut().letters = e.text().trim().to_string();
            update();
        });
    }
    for &icon in SYMBOLS {
        let b = gtk::Button::builder().icon_name(icon).css_classes(["flat", "circular"]).build();
        b.set_tooltip_text(Some(icon.trim_start_matches("parley-").trim_end_matches("-symbolic")));
        let (draft, update) = (draft.clone(), update.clone());
        b.connect_clicked(move |_| {
            draft.borrow_mut().icon = icon.to_string();
            update();
        });
        symbols.append(&b);
    }
    for &(from, to) in BACKGROUNDS {
        let sample = adw::Avatar::new(28, None, false);
        sample.set_custom_image(Some(&draw("", None, &rgba(from), to.map(rgba).as_ref())));
        let b = gtk::Button::builder().child(&sample).css_classes(["flat", "circular", "swatch"]).build();
        let (draft, update) = (draft.clone(), update.clone());
        b.connect_clicked(move |_| {
            let mut dr = draft.borrow_mut();
            dr.from = rgba(from);
            dr.to = to.map(rgba);
            drop(dr);
            update();
        });
        swatches.append(&b);
    }
    {
        let (dr, up, sy) = (draft.clone(), update.clone(), syncing.clone());
        from_btn.connect_rgba_notify(move |b| {
            if !sy.get() {
                dr.borrow_mut().from = b.rgba();
                up();
            }
        });
        let (dr, up, sy) = (draft.clone(), update.clone(), syncing.clone());
        to_btn.connect_rgba_notify(move |b| {
            if !sy.get() {
                dr.borrow_mut().to = Some(b.rgba());
                up();
            }
        });
        let (draft, update, syncing) = (draft.clone(), update.clone(), syncing.clone());
        gradient.connect_toggled(move |g| {
            if !syncing.get() {
                let mut dr = draft.borrow_mut();
                // Starts at a darker shade of the colour; the button beside it changes it.
                let f = dr.from;
                dr.to = g.is_active().then(|| gdk::RGBA::new(f.red() * 0.55, f.green() * 0.55, f.blue() * 0.55, 1.0));
                drop(dr);
                update();
            }
        });
    }
    {
        let (draft, update, d2) = (draft.clone(), update.clone(), d.clone());
        choose.connect_clicked(move |_| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Images"));
            filter.add_mime_type("image/*");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let fd = gtk::FileDialog::builder().title("Choose a picture").filters(&filters).modal(true).build();
            let (draft, update) = (draft.clone(), update.clone());
            let parent = d2.root().and_downcast::<gtk::Window>();
            fd.open(parent.as_ref(), gio::Cancellable::NONE, move |res| {
                let Ok(file) = res else { return };
                match square_image(&file) {
                    Ok(t) => {
                        draft.borrow_mut().image = Some(t);
                        update();
                    }
                    Err(e) => tracing::warn!("couldn't open that image: {e}"),
                }
            });
        });
    }
    {
        let (a, draft, d2) = (app.clone(), draft.clone(), d.clone());
        save.connect_clicked(move |_| {
            let dr = draft.borrow();
            let Some(tex) = dr.texture() else { return };
            let kind = match dr.mode {
                Mode::Image => {
                    let path = picture_path(&id);
                    let saved = path.parent().map(std::fs::create_dir_all).is_some() && tex.save_to_png(&path).is_ok();
                    if !saved {
                        a.toast("Couldn't save the picture.");
                        return;
                    }
                    PictureKind::Image
                }
                Mode::Letters | Mode::Symbol => PictureKind::Made {
                    letters: dr.letters.clone(),
                    icon: (dr.mode == Mode::Symbol).then(|| dr.icon.clone()),
                    from: hex(&dr.from),
                    to: dr.to.as_ref().map(hex),
                },
            };
            if !matches!(kind, PictureKind::Image) {
                let _ = std::fs::remove_file(picture_path(&id));
            }
            {
                let mut st = a.store.borrow_mut();
                st.pictures.retain(|p| p.id != id);
                st.pictures.push(Picture { id, kind });
                st.save();
            }
            identity::set_picture(id, Some(tex));
            a.refresh_pictures();
            d2.close();
        });
    }
    {
        let (a, d2) = (app.clone(), d.clone());
        reset.connect_clicked(move |_| {
            let _ = std::fs::remove_file(picture_path(&id));
            {
                let mut st = a.store.borrow_mut();
                st.pictures.retain(|p| p.id != id);
                st.save();
            }
            identity::set_picture(id, None);
            a.refresh_pictures();
            d2.close();
        });
    }
    update();
    d.present(Some(&app.window));
}

#[cfg(test)]
mod tests {
    #[test]
    fn initials_like_gnome() {
        assert_eq!(super::initials("ada lovelace"), "AL");
        assert_eq!(super::initials("bob"), "B");
        assert_eq!(super::initials("  "), "");
    }
}
