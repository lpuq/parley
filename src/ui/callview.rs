//! The in-call screen: video/avatar tiles plus the control bar.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use adw::prelude::*;
use gtk::glib;

use super::identity;
use iroh::EndpointId;

use super::{App, dialogs};
use crate::{
    media::devices,
    proto::{RoomKey, Track},
};

/// Pointer rest before overlays (names, LIVE, controls) fade out.
const IDLE_MS: u64 = 4000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum TileKey {
    /// `None` = me.
    User(Option<EndpointId>),
    Screen(Option<EndpointId>),
}

impl TileKey {
    fn owner(self) -> Option<EndpointId> {
        match self {
            Self::User(o) | Self::Screen(o) => o,
        }
    }
}

struct Tile {
    frame: gtk::AspectFrame,
    overlay: gtk::Overlay,
    picture: gtk::Picture,
    avatar: adw::Avatar,
    name: gtk::Label,
    muted: gtk::Image,
    status: gtk::Label,
    link: gtk::Label,
    /// Remote screen tiles: shown instead of the video when we're not watching.
    not_watching: Option<gtk::Box>,
    /// Volume/mute menu button (remote voices and streams).
    menu: Option<gtk::MenuButton>,
}

pub struct CallView {
    app: Weak<App>,
    root: gtk::Box,
    head: gtk::Box,
    bar: gtk::Box,
    /// Holds the stage; in fill-window mode the controls float over it.
    stage_overlay: gtk::Overlay,
    /// Fill-window mode: controls, names and badges fade after a while without
    /// pointer movement.
    idle_timer: RefCell<Option<glib::SourceId>>,
    bar_hovered: Cell<bool>,
    /// Call shortcuts, for the window to install.
    pub keys: RefCell<Option<gtk::EventControllerKey>>,
    strip_scroll: gtk::ScrolledWindow,
    /// Full screen showing only the focused tile.
    immersive: Cell<bool>,
    /// Whether the sidebar was showing before going full screen.
    sidebar_before: Cell<bool>,
    title: gtk::Label,
    subtitle: gtk::Label,
    stage: gtk::Box,
    grid: gtk::Grid,
    focus_main: gtk::Box,
    strip: gtk::Box,
    focus_layout: gtk::Box,
    waiting: adw::StatusPage,
    tiles: RefCell<HashMap<TileKey, Tile>>,
    focus: Cell<Option<TileKey>>,
    order: RefCell<Vec<TileKey>>,
    mic_btn: adw::SplitButton,
    deaf_btn: gtk::Button,
    cam_btn: adw::SplitButton,
    screen_btn: gtk::Button,
    /// Controls hidden in narrow windows (Settings is also in the sidebar menu).
    pub wide_only: Vec<gtk::Widget>,
}

impl CallView {
    pub fn new(app: Weak<App>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("call-view");

        // Title area.
        let title = gtk::Label::new(None);
        title.add_css_class("title-3");
        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");
        let head = gtk::Box::new(gtk::Orientation::Vertical, 2);
        head.set_margin_top(8);
        head.set_margin_bottom(8);
        head.append(&title);
        head.append(&subtitle);
        root.append(&head);

        // Stage.
        let stage = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stage.set_vexpand(true);
        stage.set_hexpand(true);
        stage.set_margin_start(16);
        stage.set_margin_end(16);
        let grid = gtk::Grid::builder()
            .row_spacing(10)
            .column_spacing(10)
            .row_homogeneous(true)
            .column_homogeneous(true)
            .vexpand(true)
            .hexpand(true)
            .build();
        let focus_main = gtk::Box::new(gtk::Orientation::Vertical, 0);
        focus_main.set_vexpand(true);
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        strip.set_halign(gtk::Align::Center);
        strip.set_margin_top(10);
        let strip_scroll = gtk::ScrolledWindow::builder()
            .vscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(120)
            .child(&strip)
            .build();
        let focus_layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        focus_layout.set_vexpand(true);
        focus_layout.append(&focus_main);
        focus_layout.append(&strip_scroll);
        let waiting = adw::StatusPage::builder()
            .icon_name("parley-system-users-symbolic")
            .title("Waiting for others to join…")
            .description("Everyone in the group was called. Anyone can join later from the group's Join button.")
            .build();
        waiting.add_css_class("compact");
        stage.append(&grid);
        let stage_overlay = gtk::Overlay::new();
        stage_overlay.set_child(Some(&stage));
        root.append(&stage_overlay);

        // Controls.
        let mic_btn = adw::SplitButton::builder().icon_name("parley-audio-input-microphone-symbolic").tooltip_text("Mute (M)").build();
        mic_btn.add_css_class("call-btn");
        let deaf_btn = round_button("parley-audio-headphones-symbolic", "Deafen");
        let cam_btn = adw::SplitButton::builder().icon_name("parley-camera-web-symbolic").tooltip_text("Turn on camera").build();
        cam_btn.add_css_class("call-btn");
        let screen_btn = round_button("parley-video-display-symbolic", "Share your screen");
        let chat_btn = round_button("parley-chat-message-new-symbolic", "Chat");
        let settings_btn = round_button("parley-emblem-system-symbolic", "Settings");
        settings_btn.set_action_name(Some("app.settings"));
        let hang = round_button("parley-call-stop-symbolic", "Leave call");
        hang.add_css_class("hangup");

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        bar.add_css_class("call-controls");
        bar.set_halign(gtk::Align::Center);
        bar.set_margin_top(12);
        bar.set_margin_bottom(16);
        for w in [mic_btn.upcast_ref::<gtk::Widget>(), deaf_btn.upcast_ref(), cam_btn.upcast_ref(), screen_btn.upcast_ref(), chat_btn.upcast_ref(), settings_btn.upcast_ref()] {
            bar.append(w);
        }
        let sep = gtk::Separator::new(gtk::Orientation::Vertical);
        sep.set_margin_start(4);
        sep.set_margin_end(4);
        bar.append(&sep);
        bar.append(&hang);
        root.append(&bar);

        let this = Self {
            app: app.clone(),
            root,
            head,
            bar: bar.clone(),
            stage_overlay,
            idle_timer: RefCell::new(None),
            bar_hovered: Cell::new(false),
            keys: RefCell::new(None),
            strip_scroll: strip_scroll.clone(),
            immersive: Cell::new(false),
            sidebar_before: Cell::new(true),
            title,
            subtitle,
            stage,
            grid,
            focus_main,
            strip,
            focus_layout,
            waiting,
            tiles: RefCell::new(HashMap::new()),
            focus: Cell::new(None),
            order: RefCell::new(Vec::new()),
            mic_btn,
            deaf_btn,
            cam_btn,
            screen_btn,
            wide_only: vec![chat_btn.clone().upcast(), settings_btn.upcast(), sep.upcast()],
        };

        let a = app.clone();
        this.mic_btn.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.toggle_mute();
            }
        });
        let a = app.clone();
        this.deaf_btn.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.toggle_deafen();
            }
        });
        let a = app.clone();
        this.cam_btn.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.toggle_camera();
            }
        });
        let a = app.clone();
        this.screen_btn.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                dialogs::screen_share(&a);
            }
        });
        let a = app.clone();
        chat_btn.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.toggle_call_chat();
            }
        });
        let a = app.clone();
        hang.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.hang_up();
            }
        });
        this.mic_btn.set_popover(Some(&device_popover(app.clone(), DeviceKind::Mic)));
        this.cam_btn.set_popover(Some(&device_popover(app.clone(), DeviceKind::Camera)));

        // Keyboard: M mute, D deafen, F fill window. Listened for on the whole window
        // (focus is often still on the sidebar's Call button), but only while the call
        // is on screen; text fields keep the keys they type.
        let keys = gtk::EventControllerKey::new();
        let a = app.clone();
        keys.connect_key_pressed(move |_, key, _, mods| {
            if !mods.is_empty() {
                return glib::Propagation::Proceed;
            }
            let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
            let typing = gtk::prelude::GtkWindowExt::focus(&a.window).is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
            if typing || a.room().is_none() || a.content.visible_child_name().as_deref() != Some("call") {
                return glib::Propagation::Proceed;
            }
            match key {
                gtk::gdk::Key::m => a.toggle_mute(),
                gtk::gdk::Key::d => a.toggle_deafen(),
                gtk::gdk::Key::Escape if a.call_view.immersive.get() => a.call_view.set_immersive(&a, None),
                gtk::gdk::Key::Escape if a.window.is_fullscreen() => a.window.unfullscreen(),
                gtk::gdk::Key::f | gtk::gdk::Key::F => a.call_view.toggle_fill(&a),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        this.keys.replace(Some(keys));

        // Fill-window mode: any real pointer movement brings the controls back. (GTK
        // also reports "motion" when the layout changes under a resting pointer.)
        let motion = gtk::EventControllerMotion::new();
        let last = Rc::new(Cell::new((f64::NAN, f64::NAN)));
        let (a, l) = (app.clone(), last.clone());
        motion.connect_motion(move |_, x, y| {
            let (lx, ly) = l.get();
            // (The first position is NaN, which compares false: written so it counts.)
            if !((x - lx).abs() <= 2.0 && (y - ly).abs() <= 2.0) {
                l.set((x, y));
                if let Some(a) = a.upgrade() {
                    a.call_view.wake();
                }
            }
        });
        this.root.add_controller(motion);
        // Never fade the controls out from under the pointer.
        let hover = gtk::EventControllerMotion::new();
        let a = app.clone();
        hover.connect_enter(move |_, _, _| {
            if let Some(a) = a.upgrade() {
                a.call_view.bar_hovered.set(true);
            }
        });
        let a = app.clone();
        hover.connect_leave(move |_| {
            if let Some(a) = a.upgrade() {
                a.call_view.bar_hovered.set(false);
            }
        });
        this.bar.add_controller(hover);
        this
    }

    /// Shows the controls (fill-window mode), and fades them again after 4 s of rest.
    fn wake(&self) {
        if let Some(t) = self.idle_timer.borrow_mut().take() {
            t.remove();
        }
        self.root.remove_css_class("idle");
        self.root.set_cursor_from_name(None);
        self.bar.set_can_target(true);
        if !self.immersive.get() {
            return;
        }
        let a = self.app.clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(IDLE_MS), move || {
            let Some(a) = a.upgrade() else { return };
            a.call_view.idle_timer.borrow_mut().take();
            a.call_view.go_idle();
        });
        *self.idle_timer.borrow_mut() = Some(id);
    }

    fn go_idle(&self) {
        if !self.immersive.get() {
            return;
        }
        let menu_open = self.bar_hovered.get()
            || [&self.mic_btn, &self.cam_btn].iter().any(|b| b.popover().is_some_and(|p| p.is_visible()))
            || self.tiles.borrow().values().any(|t| t.menu.as_ref().is_some_and(|m| m.is_active()));
        if menu_open {
            self.wake();
            return;
        }
        self.root.add_css_class("idle");
        self.root.set_cursor_from_name(Some("none"));
        // Invisible buttons mustn't catch clicks.
        self.bar.set_can_target(false);
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// One tile (a stream, usually) fills the window; `None` goes back.
    pub fn set_immersive(&self, app: &Rc<App>, key: Option<TileKey>) {
        let on = key.is_some();
        if let Some(k) = key {
            self.focus.set(Some(k));
        }
        if self.immersive.replace(on) == on {
            self.relayout(app);
            return;
        }
        self.head.set_visible(!on);
        self.strip_scroll.set_visible(!on);
        // The controls float over the picture (and fade) instead of taking a strip.
        if on {
            self.root.remove(&self.bar);
            self.bar.set_valign(gtk::Align::End);
            self.stage_overlay.add_overlay(&self.bar);
        } else {
            self.stage_overlay.remove_overlay(&self.bar);
            self.bar.set_valign(gtk::Align::Fill);
            self.root.append(&self.bar);
        }
        app.content_page.set_reveal_top_bars(!on);
        toggle_class(self.root.upcast_ref(), "immersive", on);
        if on {
            self.sidebar_before.set(app.split.shows_sidebar());
            app.split.set_show_sidebar(false);
            app.brief_toast("Press F or Esc to exit · F11 for full screen", 2000);
        } else {
            app.split.set_show_sidebar(self.sidebar_before.get());
        }
        self.wake();
        self.relayout(app);
    }

    /// F: fill the window with the focused tile, or else the first shared screen.
    pub fn toggle_fill(&self, app: &Rc<App>) {
        if self.immersive.get() {
            self.set_immersive(app, None);
            return;
        }
        let order = self.order.borrow().clone();
        let key = self
            .focus
            .get()
            .or_else(|| order.iter().copied().find(|k| matches!(k, TileKey::Screen(Some(_)))))
            .or_else(|| order.iter().copied().find(|k| matches!(k, TileKey::Screen(_))));
        if let Some(k) = key {
            self.set_immersive(app, Some(k));
        }
    }

    pub fn is_immersive(&self) -> bool {
        self.immersive.get()
    }

    pub fn rebuild_empty(&self) {
        self.focus.set(None);
        for (_, t) in self.tiles.borrow_mut().drain() {
            detach(t.frame.upcast_ref());
        }
        detach(self.waiting.upcast_ref());
        self.order.borrow_mut().clear();
    }

    /// Reconciles tiles with the current participants and media state.
    pub fn rebuild(&self, app: &Rc<App>) {
        if app.room().is_none() {
            return;
        }
        let st = app.state.get();
        let my_name = {
            let n = app.store.borrow().my_name.clone();
            if n.is_empty() { "You".to_string() } else { n }
        };
        let parts = app.participants();

        let mut want: Vec<TileKey> = vec![TileKey::User(None)];
        if st.screen {
            want.push(TileKey::Screen(None));
        }
        for p in &parts {
            want.push(TileKey::User(Some(p.id)));
            if p.state.screen {
                want.push(TileKey::Screen(Some(p.id)));
            }
        }
        // Screens first, like Discord's stage: they're what people came to see.
        want.sort_by_key(|k| match k {
            TileKey::Screen(Some(_)) => 0,
            TileKey::User(Some(_)) => 1,
            TileKey::Screen(None) => 2,
            TileKey::User(None) => 3,
        });

        {
            let mut tiles = self.tiles.borrow_mut();
            tiles.retain(|k, t| {
                let keep = want.contains(k);
                if !keep {
                    detach(t.frame.upcast_ref());
                }
                keep
            });
            for k in &want {
                if !tiles.contains_key(k) {
                    tiles.insert(*k, self.make_tile(*k, app));
                }
                let t = &tiles[k];
                match *k {
                    TileKey::User(None) => {
                        set_tile(t, &app.me(), &my_name, &format!("{} (you)", identity::tagged_markup(&my_name, &app.me())), app.local_paintable(Track::Camera), st.mic_muted || st.deafened, false);
                        t.status.set_visible(false);
                        t.picture.set_content_fit(gtk::ContentFit::Cover);
                    }
                    TileKey::Screen(None) => {
                        set_tile(t, &app.me(), &my_name, "Your screen", app.local_paintable(Track::Screen), false, true);
                        t.status.set_visible(false);
                    }
                    TileKey::User(Some(id)) => {
                        let p = parts.iter().find(|p| p.id == id).unwrap();
                        let name = app.store.borrow().contact(&id).map(|c| c.name.clone()).unwrap_or(p.name.clone());
                        let video = if p.state.camera { app.remote_paintable(&id, Track::Camera) } else { None };
                        set_tile(t, &id, &name, &identity::tagged_markup(&name, &id), video, p.state.mic_muted || p.state.deafened, false);
                        t.name.set_tooltip_text(Some(&match app.version_of(&id) {
                            Some(v) => format!("Parley {v}"),
                            None => "Parley version unknown (older than 0.4.3)".to_string(),
                        }));
                        t.picture.set_content_fit(gtk::ContentFit::Cover);
                        t.status.set_visible(p.reconnecting);
                        t.status.set_label("Reconnecting…");
                        t.muted.set_icon_name(Some(if p.state.deafened {
                            "parley-audio-volume-muted-symbolic"
                        } else {
                            "parley-microphone-disabled-symbolic"
                        }));
                    }
                    TileKey::Screen(Some(id)) => {
                        let name = app.display_name(&id);
                        set_tile(t, &id, &name, &format!("{}'s screen", identity::tagged_markup(&name, &id)), app.remote_paintable(&id, Track::Screen), false, true);
                        t.status.set_visible(false);
                        if let Some(b) = &t.not_watching {
                            b.set_visible(!app.is_watching(&id));
                        }
                    }
                }
            }
        }
        if let Some(f) = self.focus.get() {
            if !want.contains(&f) {
                self.focus.set(None);
            }
        }
        // Auto-focus a newly started remote screen share if nothing is focused.
        let new_screen = want
            .iter()
            .find(|k| matches!(k, TileKey::Screen(Some(_))) && !self.order.borrow().contains(k))
            .copied();
        if self.focus.get().is_none() {
            if let Some(k) = new_screen {
                self.focus.set(Some(k));
            }
        }
        *self.order.borrow_mut() = want;
        self.relayout(app);
        self.update_controls(app);
        self.update_title(app);
    }

    /// Lays the tiles out again (also on window width changes: narrow windows get one
    /// column of smaller tiles so the call never needs more width than the window has).
    pub fn relayout(&self, app: &Rc<App>) {
        let tiles = self.tiles.borrow();
        let order = self.order.borrow();
        for t in tiles.values() {
            detach(t.frame.upcast_ref());
        }
        detach(self.waiting.upcast_ref());
        detach(self.grid.upcast_ref());
        detach(self.focus_layout.upcast_ref());

        let alone = app.participants().is_empty();
        if let Some(f) = self.focus.get().filter(|_| order.len() > 1) {
            self.stage.append(&self.focus_layout);
            let main = &tiles[&f];
            main.frame.set_size_request(-1, -1);
            self.focus_main.append(&main.frame);
            for k in order.iter().filter(|k| **k != f) {
                let t = &tiles[k];
                t.frame.set_size_request(200, 112);
                self.strip.append(&t.frame);
            }
            return;
        }
        self.focus.set(None);
        self.stage.append(&self.grid);
        let mut items: Vec<gtk::Widget> = order.iter().map(|k| tiles[k].frame.clone().upcast()).collect();
        if alone && matches!(app.room(), Some(RoomKey::Group(_))) {
            items.push(self.waiting.clone().upcast());
        }
        let n = items.len().max(1);
        let compact = app.window.has_css_class("compact");
        let cols = match n {
            _ if compact => 1,
            1 => 1,
            2 => 2,
            3 | 4 => 2,
            _ => 3,
        };
        let (w_min, h_min) = if compact { (160, 90) } else { (240, 135) };
        for (i, w) in items.iter().enumerate() {
            if let Some(f) = w.downcast_ref::<gtk::AspectFrame>() {
                f.set_size_request(w_min, h_min);
            }
            self.grid.attach(w, (i % cols) as i32, (i / cols) as i32, 1, 1);
        }
    }

    fn make_tile(&self, key: TileKey, app: &Rc<App>) -> Tile {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        let label_name = match key.owner() {
            None => app.store.borrow().my_name.clone(),
            Some(id) => app.display_name(&id),
        };
        let avatar = identity::avatar(88, &label_name, &key.owner().unwrap_or(app.me()));
        avatar.set_halign(gtk::Align::Center);
        avatar.set_valign(gtk::Align::Center);
        let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
        base.add_css_class("tile-bg");
        base.append(&picture);
        let overlay = gtk::Overlay::new();
        overlay.add_css_class("tile");
        overlay.set_overflow(gtk::Overflow::Hidden);
        overlay.set_child(Some(&base));
        overlay.add_overlay(&avatar);

        let name = gtk::Label::new(None);
        name.add_css_class("tile-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let muted = gtk::Image::from_icon_name("parley-microphone-disabled-symbolic");
        muted.add_css_class("tile-muted");
        let nb = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        nb.set_halign(gtk::Align::Start);
        nb.set_valign(gtk::Align::End);
        nb.set_margin_start(8);
        nb.set_margin_bottom(8);
        nb.append(&muted);
        nb.append(&name);
        nb.add_css_class("tile-chrome");
        overlay.add_overlay(&nb);

        let tr = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tr.set_halign(gtk::Align::End);
        tr.set_valign(gtk::Align::Start);
        tr.set_margin_end(8);
        tr.set_margin_top(8);
        let status = gtk::Label::new(None);
        status.add_css_class("tile-status");
        status.set_visible(false);
        let link = gtk::Label::new(None);
        link.add_css_class("tile-link");
        link.set_visible(false);
        tr.append(&status);
        tr.append(&link);
        if let TileKey::Screen(_) = key {
            let fs = gtk::Button::builder()
                .icon_name("parley-view-fullscreen-symbolic")
                .tooltip_text("Fill window (F) · F11 for full screen")
                .css_classes(["osd", "circular", "tile-menu"])
                .build();
            let a = self.app.clone();
            fs.connect_clicked(move |_| {
                if let Some(a) = a.upgrade() {
                    let cv = &a.call_view;
                    cv.set_immersive(&a, if cv.immersive.get() { None } else { Some(key) });
                }
            });
            tr.append(&fs);
            let live = gtk::Label::new(Some("LIVE"));
            live.add_css_class("live-badge");
            live.set_halign(gtk::Align::Start);
            live.set_valign(gtk::Align::Start);
            live.set_margin_start(8);
            live.set_margin_top(8);
            live.add_css_class("tile-chrome");
            overlay.add_overlay(&live);
        }
        let mut menu = None;
        if let TileKey::User(Some(id)) = key {
            let m = self.tile_menu(id, Track::Mic);
            tr.append(&m);
            menu = Some((m, id, Track::Mic));
        }
        let mut not_watching = None;
        if let TileKey::Screen(Some(id)) = key {
            let m = self.tile_menu(id, Track::ScreenAudio);
            tr.append(&m);
            menu = Some((m, id, Track::ScreenAudio));
            let b = gtk::Box::new(gtk::Orientation::Vertical, 10);
            b.set_halign(gtk::Align::Center);
            b.set_valign(gtk::Align::Center);
            let l = gtk::Label::new(Some(&format!("{label_name} is sharing their screen")));
            l.add_css_class("heading");
            let watch = gtk::Button::builder().label("Watch stream").css_classes(["pill", "suggested-action"]).build();
            let a = self.app.clone();
            watch.connect_clicked(move |_| {
                if let Some(a) = a.upgrade() {
                    a.set_watching(id, true);
                }
            });
            b.append(&l);
            b.append(&watch);
            b.set_visible(false);
            overlay.add_overlay(&b);
            not_watching = Some(b);
        }
        overlay.add_overlay(&tr);

        let frame = gtk::AspectFrame::new(0.5, 0.5, 16.0 / 9.0, false);
        frame.set_child(Some(&overlay));
        frame.set_hexpand(true);
        frame.set_vexpand(true);

        // Click to focus, double-click for fullscreen.
        let click = gtk::GestureClick::new();
        let a = self.app.clone();
        click.connect_pressed(move |g, n, x, y| {
            let Some(a) = a.upgrade() else { return };
            // Leave clicks on the tile's own buttons (volume, watch…) to them.
            if let Some(w) = g.widget() {
                let mut hit = w.pick(x, y, gtk::PickFlags::DEFAULT);
                while let Some(h) = hit {
                    if h == w {
                        break;
                    }
                    if h.is::<gtk::Button>() || h.is::<gtk::MenuButton>() {
                        return;
                    }
                    hit = h.parent();
                }
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            let cv = &a.call_view;
            if n == 2 {
                cv.set_immersive(&a, if cv.immersive.get() { None } else { Some(key) });
                return;
            } else if n == 1 && !cv.immersive.get() {
                cv.focus.set(if cv.focus.get() == Some(key) { None } else { Some(key) });
            }
            cv.relayout(&a);
        });
        overlay.add_controller(click);

        // Video-player-style overlay: on a tile showing video, the name, LIVE badge and
        // buttons (and the pointer) fade out after a few seconds without mouse movement.
        tr.add_css_class("tile-chrome");
        let menu_open = Rc::new(Cell::new(false));
        let idle_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        let arm_idle = {
            let (ov, tr, menu_open, idle_timer) = (overlay.clone(), tr.clone(), menu_open.clone(), idle_timer.clone());
            Rc::new(move |delay_ms: u64| {
                if let Some(t) = idle_timer.borrow_mut().take() {
                    t.remove();
                }
                let (ov, tr, menu_open, slot) = (ov.clone(), tr.clone(), menu_open.clone(), idle_timer.clone());
                let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay_ms), move || {
                    slot.borrow_mut().take();
                    let busy = menu_open.get() || {
                        let mut c = tr.first_child();
                        let mut open = false;
                        while let Some(w) = c {
                            open |= w.downcast_ref::<gtk::MenuButton>().is_some_and(|m| m.is_active());
                            c = w.next_sibling();
                        }
                        open
                    };
                    if ov.has_css_class("has-video") && !busy && !in_immersive(ov.upcast_ref()) {
                        ov.add_css_class("idle");
                        ov.set_cursor_from_name(Some("none"));
                    }
                });
                *idle_timer.borrow_mut() = Some(id);
            })
        };
        let wake = {
            let (ov, arm_idle) = (overlay.clone(), arm_idle.clone());
            move || {
                ov.remove_css_class("idle");
                ov.set_cursor_from_name(None);
                arm_idle(IDLE_MS);
            }
        };
        let motion = gtk::EventControllerMotion::new();
        {
            // GTK also reports "motion" when the layout changes under a resting pointer
            // (the call timer ticks every second), so only real movement counts.
            let last = Rc::new(Cell::new((f64::NAN, f64::NAN)));
            let (w, l) = (wake.clone(), last.clone());
            motion.connect_enter(move |_, x, y| {
                l.set((x, y));
                w();
            });
            let (w, l) = (wake.clone(), last.clone());
            motion.connect_motion(move |_, x, y| {
                let (lx, ly) = l.get();
                if (x - lx).abs() > 2.0 || (y - ly).abs() > 2.0 {
                    l.set((x, y));
                    w();
                }
            });
            let a = arm_idle.clone();
            motion.connect_leave(move |_| a(800));
        }
        overlay.add_controller(motion);
        arm_idle(IDLE_MS);

        // Right-click: the same menu as the tile's button, at the pointer.
        if let Some((_, id, track)) = &menu {
            let (id, track) = (*id, *track);
            let rclick = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
            let (w, ov, mo) = (self.app.clone(), overlay.clone(), menu_open.clone());
            rclick.connect_pressed(move |g, _, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                let Some(a) = w.upgrade() else { return };
                let pop = a.call_view.tile_popover(id, track);
                pop.set_parent(&ov);
                pop.set_has_arrow(false);
                pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                mo.set(true);
                let mo = mo.clone();
                pop.connect_closed(move |p| {
                    mo.set(false);
                    let p = p.clone();
                    glib::idle_add_local_once(move || p.unparent());
                });
                pop.popup();
            });
            overlay.add_controller(rclick);
        }
        let menu = menu.map(|(m, ..)| m);

        Tile { frame, overlay, picture, avatar, name, muted, status, link, not_watching, menu }
    }

    /// The menu for someone's voice (`Track::Mic`) or their stream (`Track::ScreenAudio`):
    /// volume, mute, and Stop/Watch stream or Add to contacts. Shown from the tile's
    /// button and on right-click.
    fn tile_popover(&self, id: EndpointId, track: Track) -> gtk::Popover {
        let stream = track == Track::ScreenAudio;
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.set_margin_start(6);
        b.set_margin_end(6);
        b.set_margin_top(6);
        b.set_margin_bottom(6);
        let lbl = gtk::Label::new(Some(if stream { "Stream volume" } else { "Voice volume" }));
        lbl.add_css_class("heading");
        lbl.set_xalign(0.0);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 200.0, 5.0);
        scale.set_width_request(220);
        scale.add_mark(100.0, gtk::PositionType::Bottom, None);
        scale.set_format_value_func(|_, v| format!("{v:.0}%"));
        scale.set_draw_value(true);
        let mute = gtk::CheckButton::with_label(if stream { "Mute stream" } else { "Mute" });
        let action = gtk::Button::new();
        action.add_css_class("flat");
        let full = gtk::Button::with_label("Fill window (F)");
        full.add_css_class("flat");
        b.append(&lbl);
        b.append(&scale);
        b.append(&mute);
        b.append(&full);
        b.append(&action);
        let (a, p) = (self.app.clone(), pop.clone());
        full.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                let key = if stream { TileKey::Screen(Some(id)) } else { TileKey::User(Some(id)) };
                a.call_view.set_immersive(&a, Some(key));
            }
        });
        pop.set_child(Some(&b));

        let (a, s2, m2, act2) = (self.app.clone(), scale.clone(), mute.clone(), action.clone());
        pop.connect_show(move |_| {
            let Some(a) = a.upgrade() else { return };
            s2.set_value(a.track_volume(&id, track) * 100.0);
            m2.set_active(a.track_muted(&id, track));
            if stream {
                act2.set_label(if a.is_watching(&id) { "Stop watching" } else { "Watch stream" });
                act2.set_visible(true);
            } else {
                act2.set_label("Add to contacts");
                act2.set_visible(a.store.borrow().contact(&id).is_none());
            }
        });
        let a = self.app.clone();
        scale.connect_value_changed(move |s| {
            if let Some(a) = a.upgrade() {
                a.set_track_volume(id, track, s.value() / 100.0);
            }
        });
        let a = self.app.clone();
        mute.connect_toggled(move |m| {
            if let Some(a) = a.upgrade() {
                a.set_track_muted(id, track, m.is_active());
                a.call_view.tick(&a);
            }
        });
        let (a, p) = (self.app.clone(), pop.clone());
        action.connect_clicked(move |_| {
            p.popdown();
            let Some(a) = a.upgrade() else { return };
            if stream {
                let watching = a.is_watching(&id);
                a.set_watching(id, !watching);
            } else {
                let name = a.participants().iter().find(|x| x.id == id).map(|x| x.name.clone()).unwrap_or_default();
                a.add_contact(id, &name);
            }
        });
        pop
    }

    fn tile_menu(&self, id: EndpointId, track: Track) -> gtk::MenuButton {
        gtk::MenuButton::builder()
            .icon_name("parley-audio-volume-high-symbolic")
            .tooltip_text(if track == Track::ScreenAudio { "Stream volume, mute, stop watching" } else { "Volume" })
            .css_classes(["osd", "circular", "tile-menu"])
            .popover(&self.tile_popover(id, track))
            .build()
    }

    fn update_controls(&self, app: &Rc<App>) {
        let st = app.state.get();
        let muted = st.mic_muted || st.deafened;
        self.mic_btn.set_icon_name(if muted { "parley-microphone-disabled-symbolic" } else { "parley-audio-input-microphone-symbolic" });
        self.mic_btn.set_tooltip_text(Some(if muted { "Unmute (M)" } else { "Mute (M)" }));
        toggle_class(self.mic_btn.upcast_ref(), "off", muted);
        self.deaf_btn.set_icon_name(if st.deafened { "parley-audio-volume-muted-symbolic" } else { "parley-audio-headphones-symbolic" });
        self.deaf_btn.set_tooltip_text(Some(if st.deafened { "Undeafen (D)" } else { "Deafen (D)" }));
        toggle_class(self.deaf_btn.upcast_ref(), "off", st.deafened);
        self.cam_btn.set_icon_name(if st.camera { "parley-camera-web-symbolic" } else { "parley-camera-disabled-symbolic" });
        self.cam_btn.set_tooltip_text(Some(if st.camera { "Turn off camera" } else { "Turn on camera" }));
        toggle_class(self.cam_btn.upcast_ref(), "on", st.camera);
        self.screen_btn.set_tooltip_text(Some(if st.screen { "Stop sharing" } else { "Share your screen" }));
        toggle_class(self.screen_btn.upcast_ref(), "on", st.screen);
    }

    fn update_title(&self, app: &Rc<App>) {
        self.title.set_label(&app.room_title());
    }

    /// Periodic: speaking rings, timer, link quality.
    pub fn tick(&self, app: &Rc<App>) {
        let st = app.state.get();
        let parts = app.participants();
        for (k, t) in self.tiles.borrow().iter() {
            if let Some(m) = &t.menu {
                let (id, track) = match k {
                    TileKey::User(Some(id)) => (*id, Track::Mic),
                    TileKey::Screen(Some(id)) => (*id, Track::ScreenAudio),
                    _ => continue,
                };
                m.set_icon_name(if app.track_muted(&id, track) { "parley-audio-volume-muted-symbolic" } else { "parley-audio-volume-high-symbolic" });
            }
        }
        for (k, t) in self.tiles.borrow().iter() {
            let TileKey::User(owner) = k else { continue };
            let (speaking, muted) = match owner {
                None => (app.hub.is_speaking(&app.me()), st.mic_muted || st.deafened),
                Some(id) => {
                    let p = parts.iter().find(|p| &p.id == id);
                    if let Some(p) = p {
                        match p.direct {
                            Some(direct) => {
                                t.link.set_visible(true);
                                t.link.set_label(&format!("{} ms", p.rtt_ms));
                                t.link.set_tooltip_text(Some(if direct {
                                    "Direct peer-to-peer connection (end-to-end encrypted)"
                                } else {
                                    "Relayed connection — still end-to-end encrypted; the relay only sees scrambled data"
                                }));
                                toggle_class(t.link.upcast_ref(), "relayed", !direct);
                            }
                            None => t.link.set_visible(false),
                        }
                    }
                    (app.hub.is_speaking(id), p.is_some_and(|p| p.state.mic_muted || p.state.deafened))
                }
            };
            toggle_class(t.overlay.upcast_ref(), "speaking", speaking && !muted);
        }
        let n = parts.len() + 1;
        let dur = app.call_started().map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let relayed = parts.iter().any(|p| p.direct == Some(false));
        self.subtitle.set_label(&format!(
            "{:02}:{:02} · {} {} · End-to-end encrypted{}",
            dur / 60,
            dur % 60,
            n,
            if n == 1 { "person" } else { "people" },
            if relayed { " · relayed" } else { "" }
        ));
        // Paintables can appear after the first frame decodes.
        for (k, t) in self.tiles.borrow().iter() {
            let want = match *k {
                TileKey::User(None) => app.local_paintable(Track::Camera),
                TileKey::Screen(None) => app.local_paintable(Track::Screen),
                TileKey::User(Some(id)) => app.remote_paintable(&id, Track::Camera),
                TileKey::Screen(Some(id)) => app.remote_paintable(&id, Track::Screen),
            };
            if t.picture.paintable() != want {
                t.picture.set_paintable(want.as_ref());
                t.avatar.set_visible(want.is_none());
                toggle_class(t.overlay.upcast_ref(), "has-video", want.is_some());
            }
        }
    }
}

/// `name` is the plain name (for the avatar), `label` the tile's caption as markup.
fn set_tile(t: &Tile, id: &EndpointId, name: &str, label: &str, video: Option<gtk::gdk::Paintable>, muted: bool, screen: bool) {
    t.name.set_markup(label);
    identity::apply(&t.avatar, name, id);
    t.muted.set_visible(muted);
    t.avatar.set_visible(video.is_none() && !screen);
    // Video gets black bars around it, not the tile's grey.
    toggle_class(t.overlay.upcast_ref(), "has-video", video.is_some() || screen);
    if t.picture.paintable() != video {
        t.picture.set_paintable(video.as_ref());
    }
}

/// Whether `w` is inside the call view in fill-window mode (where the call view's own
/// idle timer fades everything together).
fn in_immersive(w: &gtk::Widget) -> bool {
    let mut p = w.parent();
    while let Some(x) = p {
        if x.has_css_class("call-view") {
            return x.has_css_class("immersive");
        }
        p = x.parent();
    }
    false
}

fn detach(w: &gtk::Widget) {
    if let Some(p) = w.parent() {
        if let Some(g) = p.downcast_ref::<gtk::Grid>() {
            g.remove(w);
        } else if let Some(b) = p.downcast_ref::<gtk::Box>() {
            b.remove(w);
        }
    }
}

fn round_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("call-btn");
    b
}

fn toggle_class(w: &gtk::Widget, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

#[derive(Clone, Copy)]
enum DeviceKind {
    Mic,
    Camera,
}

/// Popover listing devices; selecting one switches immediately.
fn device_popover(app: Weak<App>, kind: DeviceKind) -> gtk::Popover {
    let pop = gtk::Popover::new();
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.set_margin_start(4);
    b.set_margin_end(4);
    b.set_margin_top(4);
    b.set_margin_bottom(4);
    pop.set_child(Some(&b));
    let p2 = pop.clone();
    pop.connect_show(move |_| {
        while let Some(c) = b.first_child() {
            b.remove(&c);
        }
        let Some(a) = app.upgrade() else { return };
        let (title, list, current) = match kind {
            DeviceKind::Mic => {
                let st = a.store.borrow();
                ("Input device", devices::microphones(st.settings.allow_desktop_mic), st.settings.mic.clone())
            }
            DeviceKind::Camera => ("Camera", devices::cameras(), a.store.borrow().settings.camera.clone()),
        };
        let h = gtk::Label::new(Some(title));
        h.add_css_class("heading");
        h.set_xalign(0.0);
        h.set_margin_bottom(4);
        b.append(&h);
        let mut names: Vec<String> = vec![String::new()];
        names.extend(list.into_iter().map(|d| d.name));
        let mut group: Option<gtk::CheckButton> = None;
        for name in names {
            let label = if name.is_empty() { "System default".to_string() } else { name.clone() };
            let cb = gtk::CheckButton::with_label(&label);
            if let Some(g) = &group {
                cb.set_group(Some(g));
            } else {
                group = Some(cb.clone());
            }
            cb.set_active(name == current);
            let (a, p) = (app.clone(), p2.clone());
            cb.connect_toggled(move |cb| {
                if !cb.is_active() {
                    return;
                }
                let Some(a) = a.upgrade() else { return };
                {
                    let mut st = a.store.borrow_mut();
                    match kind {
                        DeviceKind::Mic => st.settings.mic = name.clone(),
                        DeviceKind::Camera => st.settings.camera = name.clone(),
                    }
                }
                a.save();
                match kind {
                    DeviceKind::Mic => a.restart_mic(),
                    DeviceKind::Camera => a.restart_camera(),
                }
                p.popdown();
            });
            b.append(&cb);
        }
        if let DeviceKind::Mic = kind {
            let more = gtk::Button::with_label("Voice settings…");
            more.add_css_class("flat");
            more.set_action_name(Some("app.settings"));
            b.append(&more);
        }
    });
    pop
}
