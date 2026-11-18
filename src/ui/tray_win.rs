//! Windows notification-area (tray) icon: Parley carries on there when its window is
//! closed. Click it to open Parley; its menu has Open, Mute/Unmute and Hang up (in a
//! call), and Quit. A red dot while something's unread.
//!
//! Its hidden window gets its messages through GTK's own Windows message loop, on the
//! main thread; clicks reach the app through `tx`.

use gtk::{gdk, glib, prelude::*};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use super::tray_common::{TrayCmd, add_dot};

pub struct Tray {
    icon: TrayIcon,
    plain: Icon,
    dotted: Icon,
    /// What's shown now: in a call, muted, unread.
    shown: std::cell::Cell<Option<(bool, bool, bool)>>,
}

impl Tray {
    pub fn new(tx: async_channel::Sender<TrayCmd>, unread: bool) -> anyhow::Result<Self> {
        let (rgba, size) = icon_rgba();
        let plain = Icon::from_rgba(rgba.clone(), size, size)?;
        let mut with_dot = rgba;
        add_dot(&mut with_dot, size as usize, size as usize, [3, 0, 1, 2]);
        let dotted = Icon::from_rgba(with_dot, size, size)?;

        let t = tx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let cmd = match e.id().as_ref() {
                "open" => TrayCmd::Show,
                "mute" => TrayCmd::ToggleMute,
                "hangup" => TrayCmd::HangUp,
                "quit" => TrayCmd::Quit,
                _ => return,
            };
            let _ = t.try_send(cmd);
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let open = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if open {
                let _ = tx.try_send(TrayCmd::Show);
            }
        }));

        let icon = TrayIconBuilder::new()
            .with_icon(if unread { dotted.clone() } else { plain.clone() })
            .with_tooltip("Parley")
            .with_menu(Box::new(menu(false, false)?))
            .with_menu_on_left_click(false)
            .build()?;
        let tray = Self { icon, plain, dotted, shown: Default::default() };
        tray.update(false, false, unread);
        Ok(tray)
    }

    pub fn update(&self, in_call: bool, muted: bool, unread: bool) {
        if self.shown.replace(Some((in_call, muted, unread))) == Some((in_call, muted, unread)) {
            return;
        }
        let _ = self.icon.set_icon(Some(if unread { self.dotted.clone() } else { self.plain.clone() }));
        let tip = match (in_call, unread) {
            (true, _) => "Parley — in a call",
            (false, true) => "Parley — new messages",
            (false, false) => "Parley — friends can call you",
        };
        let _ = self.icon.set_tooltip(Some(tip));
        if let Ok(m) = menu(in_call, muted) {
            self.icon.set_menu(Some(Box::new(m)));
        }
    }
}

fn menu(in_call: bool, muted: bool) -> tray_icon::menu::Result<Menu> {
    let m = Menu::new();
    m.append(&MenuItem::with_id("open", "Open Parley", true, None))?;
    if in_call {
        m.append(&MenuItem::with_id("mute", if muted { "Unmute" } else { "Mute" }, true, None))?;
        m.append(&MenuItem::with_id("hangup", "Hang up", true, None))?;
    }
    m.append(&PredefinedMenuItem::separator())?;
    m.append(&MenuItem::with_id("quit", "Quit Parley", true, None))?;
    Ok(m)
}

/// The app icon as RGBA pixels, and its size.
fn icon_rgba() -> (Vec<u8>, u32) {
    let bytes = glib::Bytes::from_static(include_bytes!("../../data/icons/io.github.parley.Parley.svg"));
    let Ok(tex) = gdk::Texture::from_bytes(&bytes) else { return (vec![0; 4], 1) };
    let mut dl = gdk::TextureDownloader::new(&tex);
    dl.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (data, stride) = dl.download_bytes();
    let (w, h) = (tex.width() as usize, tex.height() as usize);
    let size = w.min(h);
    let mut px = Vec::with_capacity(size * size * 4);
    for row in 0..size {
        px.extend_from_slice(&data[row * stride..row * stride + size * 4]);
    }
    (px, size as u32)
}
