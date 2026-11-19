//! Windows notification-area (tray) icon: Parley carries on there when its window is
//! closed. Click it to open Parley; its menu has Open, Mute/Unmute and Hang up (in a
//! call), and Quit. A red dot while something's unread.
//!
//! Parley's notifications come from this icon too. (GLib's own Windows notifications
//! would add a second Parley icon to the tray, and ignore clicks on them.)
//!
//! Its hidden window gets its messages through GTK's own Windows message loop, on the
//! main thread; clicks reach the app through `tx`.

use std::{cell::Cell, sync::OnceLock};

use gtk::{gdk, glib, prelude::*};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::Shell::{
        DefSubclassProc, NIF_INFO, NIIF_NONE, NIIF_RESPECT_QUIET_TIME, NIM_MODIFY, NIN_BALLOONUSERCLICK,
        NOTIFYICONDATAW, SetWindowSubclass, Shell_NotifyIconW,
    },
};

use super::tray_common::{TrayCmd, add_dot};

pub struct Tray {
    icon: TrayIcon,
    plain: Icon,
    dotted: Icon,
    /// What's shown now: in a call, muted, unread.
    shown: Cell<Option<(bool, bool, bool)>>,
    /// The id Windows knows the icon by, once found (see `notify`).
    uid: Cell<Option<u32>>,
}

/// tray-icon's window message for its icon's events (its WM_USER_TRAYICON). The event
/// is in the low word of `lparam`.
const TRAY_MESSAGE: u32 = 6002;
/// Where clicks on notifications go (the subclass procedure has no other way in).
static CLICKS: OnceLock<async_channel::Sender<TrayCmd>> = OnceLock::new();

impl Tray {
    pub fn new(tx: async_channel::Sender<TrayCmd>, unread: bool) -> anyhow::Result<Self> {
        let (rgba, size) = icon_rgba();
        let plain = Icon::from_rgba(rgba.clone(), size, size)?;
        let mut with_dot = rgba;
        add_dot(&mut with_dot, size as usize, size as usize, [3, 0, 1, 2]);
        let dotted = Icon::from_rgba(with_dot, size, size)?;

        let _ = CLICKS.set(tx.clone());
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
        // Notification clicks come to the icon's window, which tray-icon doesn't report.
        unsafe { SetWindowSubclass(icon.window_handle(), Some(notification_clicks), SUBCLASS_ID, 0) };
        let tray = Self { icon, plain, dotted, shown: Default::default(), uid: Default::default() };
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

    /// Shows a notification from the icon; Windows presents it like any app's (and holds
    /// it back during its own Do Not Disturb). A newer one replaces it. Clicking it sends
    /// `TrayCmd::NotificationClicked`. False if it couldn't be shown.
    pub fn notify(&self, title: &str, body: &str) -> bool {
        let mut nid = self.info();
        nid.dwInfoFlags = NIIF_NONE | NIIF_RESPECT_QUIET_TIME;
        copy_wide(&mut nid.szInfoTitle, title);
        // (An empty text would remove the notification instead.)
        copy_wide(&mut nid.szInfo, if body.is_empty() { " " } else { body });
        // tray-icon doesn't say which id it registered its icon under, but its window has
        // only that one icon: the first id Windows accepts is it.
        let ids = match self.uid.get() {
            Some(id) => id..id + 1,
            None => 0..64,
        };
        for id in ids {
            nid.uID = id;
            if unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) } != 0 {
                self.uid.set(Some(id));
                return true;
            }
        }
        false
    }

    /// Takes the notification away, if it's still showing.
    pub fn withdraw(&self) {
        if let Some(id) = self.uid.get() {
            let mut nid = self.info();
            nid.uID = id;
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        }
    }

    fn info(&self) -> NOTIFYICONDATAW {
        // Safety: a plain C struct, for which all zeroes is valid (and empty).
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.icon.window_handle();
        nid.uFlags = NIF_INFO;
        nid
    }
}

/// Ours among the icon window's subclasses (muda has one for the menu).
const SUBCLASS_ID: usize = 0x5041_524c;

unsafe extern "system" fn notification_clicks(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == TRAY_MESSAGE && (lparam as u32 & 0xffff) == NIN_BALLOONUSERCLICK {
        if let Some(tx) = CLICKS.get() {
            let _ = tx.try_send(TrayCmd::NotificationClicked);
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Copies `s` into a fixed-size UTF-16 field, cut short if need be, NUL-terminated.
fn copy_wide(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for c in s.chars() {
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        if n + units.len() >= dst.len() {
            break;
        }
        dst[n..n + units.len()].copy_from_slice(units);
        n += units.len();
    }
    dst[n] = 0;
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
