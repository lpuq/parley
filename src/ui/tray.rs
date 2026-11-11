//! System tray icon (StatusNotifierItem): shows Parley is running in the background
//! (so friends can call) and offers Open / Mute / Hang up / Quit.

use gtk::{gdk, glib, prelude::*};

pub enum TrayCmd {
    Show,
    ToggleMute,
    HangUp,
    Quit,
}

pub struct ParleyTray {
    pub tx: async_channel::Sender<TrayCmd>,
    pub in_call: bool,
    pub muted: bool,
    pub icons: Vec<ksni::Icon>,
}

impl ParleyTray {
    fn send(&self, c: TrayCmd) {
        let _ = self.tx.try_send(c);
    }
}

impl ksni::Tray for ParleyTray {
    fn id(&self) -> String {
        "parley".into()
    }
    fn title(&self) -> String {
        "Parley".into()
    }
    fn icon_name(&self) -> String {
        super::APP_ID.into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Parley".into(),
            description: if self.in_call { "In a call".into() } else { "Running — friends can call you".into() },
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCmd::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: "Open Parley".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::Show)),
                ..Default::default()
            }
            .into(),
        ];
        if self.in_call {
            items.push(
                StandardItem {
                    label: if self.muted { "Unmute".into() } else { "Mute".into() },
                    activate: Box::new(|t: &mut Self| t.send(TrayCmd::ToggleMute)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Hang up".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayCmd::HangUp)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(ksni::MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit Parley".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// The app icon as SNI pixmaps (ARGB32, network byte order). Panels can't always find
/// a Flatpak app's themed icon by name, so ship the pixels too.
pub fn icon_pixmaps() -> Vec<ksni::Icon> {
    let bytes = glib::Bytes::from_static(include_bytes!("../../data/icons/io.github.parley.Parley.svg"));
    let Ok(tex) = gdk::Texture::from_bytes(&bytes) else { return vec![] };
    let mut dl = gdk::TextureDownloader::new(&tex);
    dl.set_format(gdk::MemoryFormat::A8r8g8b8);
    let (data, stride) = dl.download_bytes();
    let (w, h) = (tex.width() as usize, tex.height() as usize);
    let mut px = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        px.extend_from_slice(&data[row * stride..row * stride + w * 4]);
    }
    vec![ksni::Icon { width: w as i32, height: h as i32, data: px }]
}
