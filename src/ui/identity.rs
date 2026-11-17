//! Telling people apart: a short tag from each identity's key (`alice#f3a7`, like
//! Discord's discriminators) and an identicon (a symmetric pattern and colour derived
//! from the key), so two identities with the same name never look alike.

use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use gtk::{gdk, glib};
use iroh::EndpointId;
use sha2::{Digest, Sha256};

/// The last four hex digits of the identity key.
pub fn tag(id: &EndpointId) -> String {
    let hex = id.to_string();
    hex[hex.len().saturating_sub(4)..].to_string()
}

static SHOW_TAGS: AtomicBool = AtomicBool::new(true);

/// Settings → Appearance: whether names carry their #tag.
pub fn set_show_tags(on: bool) {
    SHOW_TAGS.store(on, Ordering::Relaxed);
}

/// `name#tag` (or just the name, if tags are hidden), as plain text.
pub fn tagged(name: &str, id: &EndpointId) -> String {
    if SHOW_TAGS.load(Ordering::Relaxed) { tagged_always(name, id) } else { name.to_string() }
}

/// `name#tag` whatever the setting: where telling identities apart is the point
/// (someone unknown calling, user info).
pub fn tagged_always(name: &str, id: &EndpointId) -> String {
    format!("{name}#{}", tag(id))
}

/// `name#tag` as Pango markup, with the tag dimmed (or just the name, if tags are
/// hidden). `name` is escaped here.
pub fn tagged_markup(name: &str, id: &EndpointId) -> String {
    if SHOW_TAGS.load(Ordering::Relaxed) { tagged_markup_always(name, id) } else { glib::markup_escape_text(name).to_string() }
}

pub fn tagged_markup_always(name: &str, id: &EndpointId) -> String {
    format!("{}<span alpha=\"55%\">#{}</span>", glib::markup_escape_text(name), tag(id))
}

/// A 5×5 mirrored pattern in a colour picked from the key, on a soft background.
pub fn identicon(id: &EndpointId) -> gdk::Texture {
    let h = Sha256::digest(id.as_bytes());
    let hue = u16::from_be_bytes([h[0], h[1]]) as f64 / 65535.0;
    let (fr, fg, fb) = hsl(hue, 0.62, 0.55);
    let (br, bg, bb) = hsl(hue, 0.35, 0.90);
    const CELLS: usize = 5;
    const PX: usize = 24; // per cell
    const PAD: usize = 12;
    let size = CELLS * PX + 2 * PAD;
    let mut on = [[false; CELLS]; CELLS];
    for (row, line) in on.iter_mut().enumerate() {
        for col in 0..3 {
            let bit = h[2 + row * 3 + col] & 1 == 1;
            line[col] = bit;
            line[CELLS - 1 - col] = bit;
        }
    }
    let mut px = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let cell = |v: usize| (v >= PAD && v < PAD + CELLS * PX).then(|| (v - PAD) / PX);
            let fill = matches!((cell(x), cell(y)), (Some(cx), Some(cy)) if on[cy][cx]);
            let (r, g, b) = if fill { (fr, fg, fb) } else { (br, bg, bb) };
            px.extend_from_slice(&[r, g, b, 255]);
        }
    }
    gdk::MemoryTexture::new(
        size as i32,
        size as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(px),
        size * 4,
    )
    .upcast()
}

/// An avatar showing the identicon (initials stay as the accessible text).
pub fn avatar(size: i32, name: &str, id: &EndpointId) -> adw::Avatar {
    let a = adw::Avatar::new(size, Some(name), true);
    a.set_custom_image(Some(&identicon(id)));
    a
}

fn hsl(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (f(r), f(g), f(b))
}

#[cfg(test)]
mod tests {
    #[test]
    fn tags_and_icons_differ_between_identities() {
        let a = iroh::SecretKey::generate().public();
        let b = iroh::SecretKey::generate().public();
        assert_eq!(super::tag(&a).len(), 4);
        assert!(super::tag(&a).chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(super::tagged("alice", &a), super::tagged("alice", &b));
        assert_eq!(super::tagged_markup("<b>", &a), format!("&lt;b&gt;<span alpha=\"55%\">#{}</span>", super::tag(&a)));
    }
}
