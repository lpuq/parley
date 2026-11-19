//! What both tray icons share (Linux's StatusNotifierItem, Windows' notification area):
//! the commands their menus send, and the red unread dot drawn on the icon.

pub enum TrayCmd {
    Show,
    ToggleMute,
    HangUp,
    Quit,
    /// Windows: the notification shown from the tray icon was clicked.
    #[cfg(windows)]
    NotificationClicked,
}

/// Paints a red dot (with a thin white ring, so it stands out on any panel) in the top
/// right corner of a `w` x `h` image whose pixels are 4 bytes, `order` saying which byte
/// is alpha, red, green and blue.
pub fn add_dot(data: &mut [u8], w: usize, h: usize, order: [usize; 4]) {
    let [ia, ir, ig, ib] = order;
    let (wf, hf) = (w as f32, h as f32);
    let r = wf.min(hf) * 0.24;
    let ring = (wf.min(hf) * 0.05).max(1.0);
    let (cx, cy) = (wf - r - ring, r + ring);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            // Edge coverage (1 px of anti-aliasing) for the dot and its ring.
            let cover = |radius: f32| (radius + 0.5 - d).clamp(0.0, 1.0);
            let (dot, outer) = (cover(r), cover(r + ring));
            if outer <= 0.0 {
                continue;
            }
            let p = (y * w + x) * 4;
            let mix = |old: u8, new: f32, a: f32| (old as f32 * (1.0 - a) + new * a).round() as u8;
            // White ring first, then the red dot over it.
            let ring_rgb = [mix(data[p + ir], 255.0, outer), mix(data[p + ig], 255.0, outer), mix(data[p + ib], 255.0, outer)];
            data[p + ia] = mix(data[p + ia], 255.0, outer);
            data[p + ir] = mix(ring_rgb[0], 224.0, dot);
            data[p + ig] = mix(ring_rgb[1], 27.0, dot);
            data[p + ib] = mix(ring_rgb[2], 36.0, dot);
        }
    }
}
