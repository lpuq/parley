//! GTK4 / libadwaita front end.

mod callview;
mod chat;
mod dialogs;
pub mod identity;
mod picture;
mod qr;
mod sidebar;
#[cfg(target_os = "linux")]
mod tray;
mod tray_common;
#[cfg(windows)]
mod tray_win;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use adw::prelude::*;
use gtk::{gio, glib};
use iroh::EndpointId;

use crate::{
    hub::Hub,
    media::{
        appaudio::{AppAudio, AudioShare, PwEvent},
        devices,
        portal::ScreenPick,
        recv::{AudioOut, VideoReceiver},
        send::{MicOptions, MicSender, ScreenAudioSender, VideoSender},
        voice::{Gate, VoiceParams},
    },
    net::{NetCmd, NetHandle, UiEvent},
    proto::{CallPresence, MediaState, Presence, RoomKey, Track},
    store::{Contact, Group, ShareCode, Status, Store},
};

use callview::CallView;
use sidebar::Sidebar;

pub const APP_ID: &str = "io.github.parley.Parley";

#[derive(Clone, Debug, PartialEq)]
pub struct Participant {
    pub id: EndpointId,
    pub name: String,
    pub state: MediaState,
    pub reconnecting: bool,
    pub direct: Option<bool>,
    pub rtt_ms: u32,
}

#[derive(Default)]
struct Media {
    audio_out: Option<AudioOut>,
    mic: Option<MicSender>,
    camera: Option<VideoSender>,
    screen: Option<VideoSender>,
    screen_stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// What's being shared: PipeWire node, its size, and whether frames go through GL.
    screen_source: Option<(u32, Option<(i32, i32)>, bool)>,
    screen_audio: Option<ScreenAudioSender>,
    /// Waiting for PipeWire to announce the virtual sink before recording it.
    screen_audio_pending: bool,
    /// Windows: the screen being shared.
    #[cfg(windows)]
    screen_monitor: Option<gst::Device>,
    remote: HashMap<(EndpointId, Track), VideoReceiver>,
}

#[derive(Clone, Copy, PartialEq)]
enum UpdateStage {
    None,
    Available,
    Installing,
    Ready,
}

pub struct Incoming {
    room: RoomKey,
    from: EndpointId,
    name: String,
    dialog: adw::AlertDialog,
}

pub struct App {
    pub app: adw::Application,
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::OverlaySplitView,
    content: gtk::Stack,
    content_page: adw::ToolbarView,
    home: adw::StatusPage,
    outgoing: adw::StatusPage,
    sidebar: Sidebar,
    call_view: CallView,

    pub store: RefCell<Store>,
    pub net: NetHandle,
    pub hub: Arc<Hub>,
    /// Our identity (it changes if the user replaces it).
    me: Cell<EndpointId>,
    /// Where network engines send their events (a new identity starts a new engine).
    ui_tx: async_channel::Sender<UiEvent>,
    pub rt: tokio::runtime::Handle,

    /// Current call, if any.
    room: Cell<Option<RoomKey>>,
    call_started: Cell<Option<Instant>>,
    participants: RefCell<Vec<Participant>>,
    /// People whose screen share we chose not to watch.
    unwatched: RefCell<std::collections::HashSet<EndpointId>>,
    pub state: Cell<MediaState>,
    media: RefCell<Media>,
    incoming: RefCell<Option<Incoming>>,
    ringer: RefCell<Option<Ringer>>,

    app_audio: Option<AppAudio>,
    pub playing_apps: RefCell<Vec<String>>,
    /// When playback last had to be restarted after an error.
    audio_restarted: Cell<Option<Instant>>,
    /// The top bar's update button, and how far along an update is.
    update_btn: gtk::Button,
    update_stage: Cell<UpdateStage>,
    update_version: RefCell<Option<String>>,
    updater: RefCell<Option<crate::update::Updater>>,
    pub share_audio: RefCell<AudioShare>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    #[cfg(target_os = "linux")]
    tray: RefCell<Option<ksni::Handle<tray::ParleyTray>>>,
    #[cfg(windows)]
    tray: RefCell<Option<tray_win::Tray>>,
    /// Windows: the notification showing from the tray icon, and the action clicking it runs.
    #[cfg(windows)]
    shown_notification: RefCell<Option<(String, Option<(String, Option<glib::Variant>)>)>>,
    bt_mic_hint_shown: Cell<bool>,
    /// Live voice-processing settings shared with the microphone pipeline.
    pub voice: Arc<VoiceParams>,
    /// A microphone running outside calls, for the level meter in Settings.
    mic_test: RefCell<Option<MicSender>>,
    /// Mic test playback ("hear yourself"): the speaker side, when enabled.
    mic_test_out: RefCell<Option<AudioOut>>,
    pub mic_playback: Cell<bool>,
    /// Contacts' online status, as last heard.
    presence: RefCell<HashMap<EndpointId, Presence>>,
    /// Who's in which group call (as told by group members), with their mute/deafen/share.
    call_presence: RefCell<HashMap<EndpointId, CallPresence>>,
    /// Others' Parley versions, as last told (calls and status checks; 0.4.3+).
    versions: RefCell<HashMap<EndpointId, String>>,

    /// Text chat: history and outbox, the view, and where it can be shown.
    pub chat: RefCell<crate::chat::ChatStore>,
    chat_view: Rc<chat::ChatView>,
    chat_page: gtk::Box,
    chat_reveal: gtk::Revealer,
    chat_panel: gtk::Box,
    /// Peers we're delivering chat to right now (one delivery at a time each).
    chat_inflight: RefCell<std::collections::HashSet<EndpointId>>,
    /// Home page: "Return to call", shown while in a call.
    home_return: gtk::Button,
    /// Calls ringing or under way, by conversation, for their record in the chat.
    open_calls: RefCell<HashMap<crate::chat::Conv, OpenCall>>,
}

/// A call's record in its conversation ("Bob started a call"), until it's over.
struct OpenCall {
    id: String,
    started: Instant,
    connected: Option<Instant>,
    incoming: bool,
    /// A group call we've seen people in (it's over once they've all left).
    seen_active: bool,
}

fn conv_of(room: RoomKey) -> crate::chat::Conv {
    match room {
        RoomKey::Direct(id) => crate::chat::Conv::Direct(id),
        RoomKey::Group(r) => crate::chat::Conv::Group(r),
    }
}

impl App {
    pub fn new(
        app: &adw::Application,
        net: NetHandle,
        hub: Arc<Hub>,
        me: EndpointId,
        rt: tokio::runtime::Handle,
        ui_tx: async_channel::Sender<UiEvent>,
        ui_rx: async_channel::Receiver<UiEvent>,
    ) -> Rc<Self> {
        load_css();
        load_icons();

        let store = Store::load();
        apply_appearance(store.settings.appearance);
        identity::set_show_tags(store.settings.show_tags);
        identity::set_initials_style(store.settings.picture_style == crate::store::PictureStyle::Initials);
        picture::load_all(&store);
        let (pw_tx, pw_rx) = async_channel::unbounded();
        let app_audio = AppAudio::start(pw_tx);

        let toasts = adw::ToastOverlay::new();
        let split = adw::OverlaySplitView::new();
        split.set_sidebar_width_fraction(0.26);
        split.set_min_sidebar_width(290.0);
        split.set_max_sidebar_width(340.0);

        let content = gtk::Stack::new();
        content.set_transition_type(gtk::StackTransitionType::Crossfade);
        // Size to the visible page only: the call controls' width mustn't widen the home page.
        content.set_hhomogeneous(false);
        content.set_interpolate_size(false);
        let home = adw::StatusPage::builder()
            .icon_name("parley-call-start-symbolic")
            .title("Ready to call")
            .build();
        let outgoing = adw::StatusPage::builder().icon_name("parley-call-start-symbolic").build();
        outgoing.add_css_class("outgoing");

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Parley")
            .default_width(1180)
            .default_height(760)
            .build();

        let this = Rc::new_cyclic(|weak: &std::rc::Weak<App>| App {
            app: app.clone(),
            window: window.clone(),
            toasts: toasts.clone(),
            split: split.clone(),
            content: content.clone(),
            content_page: adw::ToolbarView::new(),
            home: home.clone(),
            outgoing: outgoing.clone(),
            sidebar: Sidebar::new(weak.clone()),
            call_view: CallView::new(weak.clone()),
            store: RefCell::new(store),
            net,
            hub,
            me: Cell::new(me),
            ui_tx,
            rt,
            room: Cell::new(None),
            call_started: Cell::new(None),
            participants: RefCell::new(Vec::new()),
            unwatched: RefCell::new(Default::default()),
            state: Cell::new(MediaState::default()),
            media: RefCell::new(Media::default()),
            incoming: RefCell::new(None),
            ringer: RefCell::new(None),
            app_audio,
            playing_apps: RefCell::new(Vec::new()),
            audio_restarted: Cell::new(None),
            update_btn: gtk::Button::builder().css_classes(["pill", "update-btn"]).valign(gtk::Align::Center).visible(false).build(),
            update_stage: Cell::new(UpdateStage::None),
            update_version: RefCell::new(None),
            updater: RefCell::new(None),
            share_audio: RefCell::new(AudioShare::Off),
            hold: RefCell::new(None),
            #[cfg(any(target_os = "linux", windows))]
            tray: RefCell::new(None),
            #[cfg(windows)]
            shown_notification: RefCell::new(None),
            bt_mic_hint_shown: Cell::new(false),
            voice: VoiceParams::new(true, Gate::Auto, -50.0, true),
            mic_test: RefCell::new(None),
            mic_test_out: RefCell::new(None),
            mic_playback: Cell::new(false),
            presence: RefCell::new(HashMap::new()),
            call_presence: RefCell::new(HashMap::new()),
            versions: RefCell::new(HashMap::new()),
            chat: RefCell::new(crate::chat::ChatStore::open()),
            chat_view: chat::ChatView::new(weak.clone()),
            chat_page: gtk::Box::new(gtk::Orientation::Vertical, 0),
            chat_reveal: gtk::Revealer::new(),
            chat_panel: gtk::Box::new(gtk::Orientation::Vertical, 0),
            chat_inflight: RefCell::new(Default::default()),
            home_return: gtk::Button::builder()
                .label("Return to call")
                .halign(gtk::Align::Center)
                .css_classes(["pill", "join-btn"])
                .visible(false)
                .build(),
            open_calls: RefCell::new(HashMap::new()),
        });

        // ----- layout -----
        let content_page = this.content_page.clone();
        let content_header = adw::HeaderBar::new();
        content_header.set_show_title(false);
        // The sidebar stays docked beside the content, like Discord's: it only goes
        // away when you hide it with this button (remembered across restarts).
        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("parley-sidebar-show-symbolic")
            .tooltip_text("Show or hide the sidebar")
            .build();
        split.set_show_sidebar(!this.store.borrow().settings.sidebar_hidden);
        split.bind_property("show-sidebar", &sidebar_toggle, "active").bidirectional().sync_create().build();
        {
            // Only real clicks count as a choice (fill-window mode hides it for a while too).
            let t = this.clone();
            sidebar_toggle.connect_clicked(move |b| {
                t.store.borrow_mut().settings.sidebar_hidden = !b.is_active();
                t.store.borrow().save();
            });
        }
        content_header.pack_start(&sidebar_toggle);
        {
            let t = this.clone();
            this.update_btn.connect_clicked(move |_| t.update_clicked());
        }
        content_header.pack_end(&this.update_btn);
        content_page.add_top_bar(&content_header);
        content_page.set_content(Some(&content));

        let (home_page, home_buttons) = this.home_page();
        content.add_named(&home_page, Some("home"));
        content.add_named(&outgoing, Some("outgoing"));
        // The call page: the call, and a side panel for its chat.
        let call_page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        this.call_view.widget().set_hexpand(true);
        call_page.append(this.call_view.widget());
        this.chat_panel.set_width_request(340);
        this.chat_panel.add_css_class("chat-panel");
        this.chat_reveal.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
        // Never stretch: otherwise the chat inside (which wants to expand) makes the
        // closed panel claim a share of spare width, leaving an empty grey strip.
        this.chat_reveal.set_hexpand(false);
        this.chat_reveal.set_child(Some(&this.chat_panel));
        call_page.append(&this.chat_reveal);
        content.add_named(&call_page, Some("call"));
        this.chat_page.set_vexpand(true);
        content.add_named(&this.chat_page, Some("chat"));
        content.set_visible_child_name("home");

        split.set_sidebar(Some(this.sidebar.widget()));
        split.set_content(Some(&content_page));
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));
        if let Some(k) = this.call_view.keys.take() {
            window.add_controller(k);
        }

        // Narrow windows: everything gets more compact so the window (and its close
        // button) never needs more width than it has. The sidebar stays docked; hiding
        // it (its button) is how to go narrower still.
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        bp.add_setter(&home_buttons, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
        bp.add_setter(&split, "max-sidebar-width", Some(&280.0f64.to_value()));
        bp.add_setter(&split, "min-sidebar-width", Some(&260.0f64.to_value()));
        for w in &this.call_view.wide_only {
            bp.add_setter(w, "visible", Some(&false.to_value()));
        }
        {
            let t = this.clone();
            bp.connect_apply(move |_| {
                t.window.add_css_class("compact");
                // No room for the chat beside a call: it stays on its own page instead.
                t.chat_reveal.set_reveal_child(false);
                t.call_view.relayout(&t);
            });
            let t = this.clone();
            bp.connect_unapply(move |_| {
                t.window.remove_css_class("compact");
                t.call_view.relayout(&t);
            });
        }
        window.add_breakpoint(bp);

        {
            // Your name in the sidebar is the way home: highlight it there.
            let t = this.clone();
            content.connect_visible_child_name_notify(move |c| {
                t.sidebar.set_home_shown(c.visible_child_name().as_deref() == Some("home"));
            });
            this.sidebar.set_home_shown(true);
            let t = this.clone();
            this.home_return.connect_clicked(move |_| t.show_call());
        }

        this.install_actions();
        // Chat: retry waiting messages now and then, and clear unread counts when the
        // window comes back to an open conversation.
        {
            let t = this.clone();
            glib::timeout_add_seconds_local(60, move || {
                t.deliver_all_chat();
                t.send_group_news(None);
                glib::ControlFlow::Continue
            });
            let t = this.clone();
            window.connect_is_active_notify(move |_| {
                if let Some(c) = t.chat_view.conv() {
                    if t.chat_visible(c) {
                        t.mark_read(c);
                    }
                }
            });
        }
        this.start_tray();
        this.sidebar.refresh(&this);
        this.update_home();
        this.push_directory();
        this.push_moves();

        {
            let t = this.clone();
            window.connect_close_request(move |w| {
                let bg = t.can_run_in_background() && t.store.borrow().settings.run_in_background;
                if t.room.get().is_some() && !bg {
                    t.hang_up();
                    t.end_call_media();
                }
                if bg {
                    // Still in a call: it carries on from the tray.
                    if t.room.get().is_some() {
                        t.notify(
                            "in-call",
                            "Still in your call",
                            "Parley keeps the call going in the background. Open it from the tray icon, or hang up there.",
                            Some(("app.show", None)),
                            |_| {},
                        );
                    }
                    // Stay reachable for incoming calls.
                    w.set_visible(false);
                    if t.hold.borrow().is_none() {
                        *t.hold.borrow_mut() = Some(t.app.hold());
                    }
                    if !t.store.borrow().settings.background_hint_shown {
                        t.store.borrow_mut().settings.background_hint_shown = true;
                        t.save();
                        t.notify(
                            "background",
                            "Parley is still running",
                            "So friends can call you. Quit from the Parley tray icon, or open Parley and use its menu → Quit. (Settings → \"Keep running when closed\" turns this off.)",
                            None,
                            |_| {},
                        );
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }

        // Network events.
        {
            let t = this.clone();
            glib::spawn_future_local(async move {
                while let Ok(ev) = ui_rx.recv().await {
                    t.on_net_event(ev);
                }
            });
        }
        // PipeWire app list / sink readiness.
        {
            let t = this.clone();
            glib::spawn_future_local(async move {
                while let Ok(ev) = pw_rx.recv().await {
                    match ev {
                        PwEvent::Apps(apps) => {
                            tracing::debug!("apps playing audio: {apps:?}");
                            *t.playing_apps.borrow_mut() = apps;
                        }
                        PwEvent::SinkReady => {
                            // Give pipewire-pulse a moment to publish the monitor source.
                            let t = t.clone();
                            glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                                t.start_screen_audio_if_pending()
                            });
                        }
                    }
                }
            });
        }
        // Speaking rings, link badges, call timer.
        {
            let t = this.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
                if t.room.get().is_some() {
                    t.call_view.tick(&t);
                    // The window got too narrow for the chat beside the call.
                    if t.chat_reveal.reveals_child() && !t.make_room_for_chat_panel() {
                        t.open_chat_page_in_call();
                    }
                }
                glib::ControlFlow::Continue
            });
            let t2 = this.clone();
            glib::timeout_add_seconds_local(10, move || {
                if let Some(o) = t2.media.borrow().audio_out.as_ref() {
                    o.log_stats();
                }
                glib::ControlFlow::Continue
            });
        }

        // Ctrl+C copies text selected across chat messages (labels don't take focus);
        // Ctrl+V pastes into the message box even when it isn't focused.
        {
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let w = Rc::downgrade(&this);
            keys.connect_key_pressed(move |_, key, _, mods| {
                let Some(t) = w.upgrade() else { return glib::Propagation::Proceed };
                if !mods.contains(gtk::gdk::ModifierType::CONTROL_MASK) || t.window.visible_dialog().is_some() {
                    return glib::Propagation::Proceed;
                }
                let handled = match key.to_lower() {
                    gtk::gdk::Key::c => t.chat_view.copy_selection(),
                    gtk::gdk::Key::v => t.chat_view.paste_to_composer(),
                    _ => false,
                };
                if handled { glib::Propagation::Stop } else { glib::Propagation::Proceed }
            });
            this.window.add_controller(keys);
        }

        if this.store.borrow().settings.check_updates {
            this.start_update_checks();
        }

        // Diagnostics: route desktop audio into the share sink without a call/portal.
        if std::env::var_os("PARLEY_TEST_SHARE_AUDIO").is_some() {
            if let Some(a) = &this.app_audio {
                a.set(AudioShare::Desktop);
            }
        }
        if this.store.borrow().my_name.trim().is_empty() {
            let t = this.clone();
            glib::idle_add_local_once(move || dialogs::ask_name(&t, true));
        }
        this
    }

    // ------------------------------------------------------------ updates

    pub fn start_update_checks(self: &Rc<Self>) {
        if self.updater.borrow().is_some() {
            return;
        }
        let (tx, rx) = async_channel::unbounded();
        let Some(u) = crate::update::Updater::start(tx) else { return };
        *self.updater.borrow_mut() = Some(u);
        let t = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                t.on_update_event(ev);
            }
        });
    }

    /// Turned off in Settings. (An update already installed still applies on restart.)
    pub fn stop_update_checks(&self) {
        if self.update_stage.get() != UpdateStage::Ready {
            self.updater.borrow_mut().take();
            self.set_update_stage(UpdateStage::None, "");
        }
    }

    fn set_update_stage(&self, stage: UpdateStage, label: &str) {
        self.update_stage.set(stage);
        self.update_btn.set_label(label);
        self.update_btn.set_visible(stage != UpdateStage::None);
        self.update_btn.set_sensitive(stage != UpdateStage::Installing);
    }

    fn on_update_event(self: &Rc<Self>, ev: crate::update::UpdateEvent) {
        use crate::update::UpdateEvent as E;
        match ev {
            E::Available(version) => {
                // (The portal repeats this; don't disturb an update under way.)
                if self.update_stage.get() == UpdateStage::None {
                    *self.update_version.borrow_mut() = version;
                    self.set_update_stage(UpdateStage::Available, "Update");
                    self.update_btn.set_tooltip_text(Some("A new version of Parley is available"));
                }
            }
            E::Progress(p) => self.set_update_stage(UpdateStage::Installing, &format!("Updating {p}%")),
            E::Ready => {
                self.set_update_stage(UpdateStage::Ready, "Restart to update");
                self.update_btn.set_tooltip_text(Some("The new version starts when Parley restarts"));
            }
            E::Failed(msg) => {
                tracing::warn!("update failed: {msg}");
                self.toast(&format!("Couldn't update Parley: {msg}"));
                self.set_update_stage(UpdateStage::Available, "Update");
            }
        }
    }

    fn update_clicked(self: &Rc<Self>) {
        match self.update_stage.get() {
            UpdateStage::Available => {
                let heading = match &*self.update_version.borrow() {
                    Some(v) => format!("Update to Parley {v}?"),
                    None => "Update Parley?".to_string(),
                };
                let d = adw::AlertDialog::new(Some(&heading), Some("It installs in the background, so you can keep using Parley. Your friends, chats and settings stay as they are."));
                d.add_response("later", "Not now");
                d.add_response("update", "Update");
                d.set_response_appearance("update", adw::ResponseAppearance::Suggested);
                d.set_default_response(Some("update"));
                d.set_close_response("later");
                let t = self.clone();
                d.connect_response(None, move |_, r| {
                    if r == "update" {
                        t.set_update_stage(UpdateStage::Installing, "Updating…");
                        if let Some(u) = t.updater.borrow().as_ref() {
                            u.install();
                        }
                    }
                });
                d.present(Some(&self.window));
            }
            UpdateStage::Ready if self.room.get().is_some() => {
                let d = adw::AlertDialog::new(Some("Restart Parley now?"), Some("This ends your call. You can also finish it first and restart afterwards."));
                d.add_response("later", "Not now");
                d.add_response("restart", "End call and restart");
                d.set_response_appearance("restart", adw::ResponseAppearance::Destructive);
                d.set_close_response("later");
                let t = self.clone();
                d.connect_response(None, move |_, r| {
                    if r == "restart" {
                        t.restart_into_update();
                    }
                });
                d.present(Some(&self.window));
            }
            UpdateStage::Ready => self.restart_into_update(),
            _ => {}
        }
    }

    fn restart_into_update(self: &Rc<Self>) {
        let res = self.updater.borrow().as_ref().map(|u| u.relaunch_after_quit());
        match res {
            Some(Ok(())) => self.quit(),
            Some(Err(e)) => self.toast(&format!("Couldn't restart Parley: {e:#}. Quit and open it again to finish updating.")),
            None => {}
        }
    }

    /// Leaves any call cleanly (so friends see "left", not a dropout), then exits.
    pub fn quit(self: &Rc<Self>) {
        let in_call = self.room.get().is_some();
        if in_call {
            self.hang_up();
        }
        self.end_call_media();
        self.hold.borrow_mut().take();
        #[cfg(target_os = "linux")]
        if let Some(h) = self.tray.borrow_mut().take() {
            let rt = self.rt.clone();
            rt.spawn(async move {
                h.shutdown().await;
            });
        }
        #[cfg(windows)]
        self.tray.borrow_mut().take();
        let app = self.app.clone();
        let delay = if in_call { 400 } else { 50 };
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || app.quit());
    }

    /// Whether closing the window may leave Parley running. On Linux it can be reopened
    /// from the app menu even without a tray; on Windows only from the tray icon.
    pub fn can_run_in_background(&self) -> bool {
        #[cfg(windows)]
        return self.tray.borrow().is_some();
        #[cfg(not(windows))]
        true
    }

    #[cfg(windows)]
    fn start_tray(self: &Rc<Self>) {
        let (tx, rx) = async_channel::unbounded();
        match tray_win::Tray::new(tx, self.any_unread()) {
            Ok(t) => *self.tray.borrow_mut() = Some(t),
            Err(e) => tracing::warn!("no tray icon ({e}); closing the window will quit Parley"),
        }
        self.handle_tray_commands(rx);
    }

    #[cfg(windows)]
    fn update_tray(&self) {
        let st = self.state.get();
        if let Some(t) = self.tray.borrow().as_ref() {
            t.update(self.room.get().is_some(), st.mic_muted || st.deafened, self.any_unread());
        }
    }

    fn handle_tray_commands(self: &Rc<Self>, rx: async_channel::Receiver<tray_common::TrayCmd>) {
        let t = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = rx.recv().await {
                match cmd {
                    tray_common::TrayCmd::Show => t.present(),
                    tray_common::TrayCmd::ToggleMute => t.toggle_mute(),
                    tray_common::TrayCmd::HangUp => t.hang_up(),
                    tray_common::TrayCmd::Quit => t.quit(),
                    #[cfg(windows)]
                    tray_common::TrayCmd::NotificationClicked => {
                        match t.shown_notification.borrow_mut().take().and_then(|(_, a)| a) {
                            Some((a, target)) => {
                                let name = a.strip_prefix("app.").unwrap_or(&a);
                                t.app.activate_action(name, target.as_ref());
                            }
                            None => t.present(),
                        }
                    }
                }
            }
        });
    }

    #[cfg(target_os = "linux")]
    fn start_tray(self: &Rc<Self>) {
        let (tx, rx) = async_channel::unbounded();
        let icons = tray::icon_pixmaps();
        let tray = tray::ParleyTray { tx, in_call: false, muted: false, unread: self.any_unread(), icons_unread: tray::with_dot(&icons), icons };
        let sandboxed = std::path::Path::new("/.flatpak-info").exists();
        let rt = self.rt.clone();
        let t = self.clone();
        glib::spawn_future_local(async move {
            use ksni::TrayMethods;
            match rt.spawn(async move { tray.disable_dbus_name(sandboxed).spawn().await }).await {
                Ok(Ok(handle)) => *t.tray.borrow_mut() = Some(handle),
                Ok(Err(e)) => tracing::info!("no system tray available ({e}); Parley still runs in the background"),
                Err(_) => {}
            }
        });
        self.handle_tray_commands(rx);
    }

    /// Mirror call state into the tray menu.
    #[cfg(target_os = "linux")]
    fn update_tray(&self) {
        let Some(h) = self.tray.borrow().clone() else { return };
        let (in_call, st, unread) = (self.room.get().is_some(), self.state.get(), self.any_unread());
        self.rt.spawn(async move {
            h.update(|t| {
                t.in_call = in_call;
                t.muted = st.mic_muted || st.deafened;
                t.unread = unread;
            })
            .await;
        });
    }

    fn any_unread(&self) -> bool {
        self.chat.borrow().unread.values().any(|n| *n > 0)
    }

    pub fn me(&self) -> EndpointId {
        self.me.get()
    }

    pub fn present(&self) {
        self.hold.borrow_mut().take();
        self.window.set_visible(true);
        self.window.present();
    }

    /// The home page (your name in the sidebar). A call carries on; the home page
    /// offers the way back to it.
    pub fn show_home(&self) {
        if self.content.visible_child_name().as_deref() == Some("chat") {
            self.chat_view.close();
        }
        self.update_home();
        self.content.set_visible_child_name("home");
        self.sidebar.refresh_soon();
    }

    /// Shows the sidebar (if hidden) and scrolls it to the open group's other members.
    pub fn show_group_members(&self) {
        if !self.split.shows_sidebar() {
            self.split.set_show_sidebar(true);
            self.store.borrow_mut().settings.sidebar_hidden = false;
            self.store.borrow().save();
        }
        self.sidebar.scroll_to_members();
    }

    /// The home page, and its row of buttons (stacked vertically in narrow windows).
    fn home_page(&self) -> (gtk::Widget, gtk::Box) {
        let copy = gtk::Button::builder()
            .label("Copy my code")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .action_name("app.copy-my-code")
            .build();
        let qr = gtk::Button::builder()
            .label("Show my QR code")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .action_name("app.show-my-qr")
            .build();
        let add = gtk::Button::builder()
            .label("Add a friend's code…")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .action_name("app.add-contact")
            .build();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
        b.set_halign(gtk::Align::Center);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        top.append(&copy);
        top.append(&qr);
        b.append(&top);
        b.append(&add);
        b.append(&self.home_return);
        self.home.set_child(Some(&b));
        (self.home.clone().upcast(), top)
    }

    pub fn update_home(&self) {
        let st = self.store.borrow();
        let desc = if st.contacts.is_empty() && st.groups.is_empty() {
            "Send your code to a friend (Signal, email, anything). When they add it, they can call you — and you'll be able to save them back.".to_string()
        } else {
            "Pick a friend or group on the left to call.\nCalls are end-to-end encrypted and go directly between you — no server, no account.".to_string()
        };
        self.home.set_description(Some(&desc));
        self.home_return.set_visible(self.room.get().is_some());
        self.home.set_title(&if st.my_name.is_empty() {
            "Ready to call".to_string()
        } else {
            format!("Hi, {}", st.my_name)
        });
    }

    pub fn toast(&self, msg: &str) {
        let t = adw::Toast::new(&glib::markup_escape_text(msg));
        t.set_timeout(5);
        self.toasts.add_toast(t);
    }

    /// A short hint that goes away on its own after `ms`, whatever the window is doing
    /// (libadwaita holds a toast while the pointer rests on it).
    pub fn brief_toast(&self, msg: &str, ms: u64) {
        let t = adw::Toast::new(&glib::markup_escape_text(msg));
        t.set_timeout(0);
        self.toasts.add_toast(t.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || t.dismiss());
    }

    pub fn my_code(&self) -> String {
        ShareCode::Contact { n: self.store.borrow().my_name.clone(), k: self.me() }.encode()
    }

    pub fn copy(&self, text: &str, what: &str) {
        self.window.clipboard().set_text(text);
        self.toast(&format!("{what} copied — paste it to your friend in any chat app."));
    }

    pub fn save(&self) {
        self.store.borrow().save();
        self.push_directory();
    }

    pub fn push_directory(&self) {
        let st = self.store.borrow();
        // Status first, so the first checks of our contacts already carry it.
        self.net.send(NetCmd::SetStatus(st.settings.status));
        self.net.send(NetCmd::Directory {
            my_name: st.my_name.clone(),
            contacts: st.contacts.clone(),
            groups: st.groups.clone(),
            allow_unknown: st.settings.allow_unknown_callers,
        });
    }

    pub fn display_name(&self, id: &EndpointId) -> String {
        self.store.borrow().name_for(id).unwrap_or_else(|| id.fmt_short().to_string())
    }

    pub fn room(&self) -> Option<RoomKey> {
        self.room.get()
    }

    pub fn room_title(&self) -> String {
        match self.room.get() {
            Some(RoomKey::Direct(p)) => self.display_name(&p),
            Some(RoomKey::Group(r)) => self
                .store
                .borrow()
                .group_by_room(&r)
                .map(|g| g.name.clone())
                .unwrap_or_else(|| "Group call".into()),
            None => String::new(),
        }
    }

    pub fn participants(&self) -> Vec<Participant> {
        self.participants.borrow().clone()
    }

    pub fn call_started(&self) -> Option<Instant> {
        self.call_started.get()
    }

    fn install_actions(self: &Rc<Self>) {
        let add = |name: &str, f: Box<dyn Fn(&Rc<App>)>| {
            let a = gio::SimpleAction::new(name, None);
            let t = self.clone();
            a.connect_activate(move |_, _| f(&t));
            self.app.add_action(&a);
        };
        add("copy-my-code", Box::new(|t| t.copy(&t.my_code(), "Your code")));
        add("show-my-qr", Box::new(|t| {
            qr::show(
                t,
                "Your code",
                "Your friend opens Parley → + → Scan with camera. A phone camera reads it as text you can forward.",
                &t.my_code(),
            )
        }));
        add("add-contact", Box::new(|t| dialogs::add_code(t, None)));
        add("new-group", Box::new(|t| dialogs::new_group(t)));
        add("settings", Box::new(|t| dialogs::settings(t)));
        add("rename-me", Box::new(|t| dialogs::ask_name(t, false)));
        add("edit-my-picture", Box::new(|t| {
            let name = t.store.borrow().my_name.clone();
            picture::edit(t, t.me(), if name.is_empty() { "You" } else { &name })
        }));
        add("accept-call", Box::new(|t| t.answer(true)));
        add("decline-call", Box::new(|t| t.answer(false)));
        add("show", Box::new(|t| t.present()));
        add("home", Box::new(|t| t.show_home()));
        add("quit", Box::new(|t| t.quit()));
        {
            let a = gio::SimpleAction::new("open-chat", Some(glib::VariantTy::STRING));
            let t = self.clone();
            a.connect_activate(move |_, v| {
                let Some(conv) = v.and_then(|v| v.str()).and_then(crate::chat::Conv::from_key) else { return };
                t.present();
                t.open_chat(conv);
            });
            self.app.add_action(&a);
        }
        add("fullscreen", Box::new(|t| {
            if t.window.is_fullscreen() {
                t.window.unfullscreen();
            } else {
                t.window.fullscreen();
            }
        }));
        {
            let cur = match self.store.borrow().settings.status {
                Status::Online => "online",
                Status::DoNotDisturb => "dnd",
                Status::Invisible => "invisible",
            };
            let a = gio::SimpleAction::new_stateful("status", Some(glib::VariantTy::STRING), &cur.to_variant());
            let t = self.clone();
            a.connect_activate(move |a, v| {
                let Some(v) = v.and_then(|v| v.str().map(str::to_owned)) else { return };
                a.set_state(&v.to_variant());
                t.set_status(match v.as_str() {
                    "dnd" => Status::DoNotDisturb,
                    "invisible" => Status::Invisible,
                    _ => Status::Online,
                });
            });
            self.app.add_action(&a);
        }
        self.app.set_accels_for_action("app.quit", &["<Ctrl>q"]);
        self.app.set_accels_for_action("app.fullscreen", &["F11"]);
    }

    // ------------------------------------------------------------ calling

    pub fn call_contact(self: &Rc<Self>, id: EndpointId) {
        if self.room.get().is_some() {
            self.toast("Hang up first to start another call.");
            return;
        }
        let name = self.display_name(&id);
        self.outgoing.set_title(&format!("Calling {name}…"));
        self.outgoing.set_description(Some("Connecting…"));
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "destructive-action"])
            .build();
        let t = self.clone();
        cancel.connect_clicked(move |_| {
            t.net.send(NetCmd::Hangup);
        });
        self.outgoing.set_child(Some(&cancel));
        self.content.set_visible_child_name("outgoing");
        self.net.send(NetCmd::CallDirect(id));
        let me = self.me();
        self.call_record_start(crate::chat::Conv::Direct(id), me, false);
        self.set_ringer(Some(RingKind::Outgoing));
    }

    pub fn join_group(self: &Rc<Self>, room: [u8; 32]) {
        if self.room.get() == Some(RoomKey::Group(room)) {
            return;
        }
        if self.room.get().is_some() {
            self.toast("Hang up first to join another call.");
            return;
        }
        self.net.send(NetCmd::JoinGroup(room));
    }

    pub fn hang_up(&self) {
        self.net.send(NetCmd::Hangup);
    }

    fn answer(self: &Rc<Self>, accept: bool) {
        let inc = self.incoming.borrow_mut().take();
        let Some(inc) = inc else { return };
        inc.dialog.force_close();
        self.set_ringer(None);
        if accept {
            if self.room.get().is_some() {
                self.net.send(NetCmd::Hangup);
            }
            self.net.send(NetCmd::Accept(inc.room));
            self.present();
            // Answering someone means you know them: save them so either of you can
            // call back, without a second code exchange.
            let unknown = self.store.borrow().contact(&inc.from).is_none() && inc.from != self.me();
            if unknown {
                self.store.borrow_mut().contacts.push(Contact::new(inc.from, inc.name.clone()));
                self.save();
                self.sidebar.refresh(self);
                self.update_home();
                let toast = adw::Toast::new(&glib::markup_escape_text(&format!("Added {} to your contacts", inc.name)));
                toast.set_button_label(Some("Undo"));
                let (t, id) = (self.clone(), inc.from);
                toast.connect_button_clicked(move |_| {
                    t.store.borrow_mut().contacts.retain(|c| c.id != id);
                    t.save();
                    t.sidebar.refresh(&t);
                    t.update_home();
                });
                self.toasts.add_toast(toast);
            }
        } else {
            self.net.send(NetCmd::Decline(inc.room));
            // A declined direct call is over; a group call goes on without us.
            if let RoomKey::Direct(_) = inc.room {
                self.call_record_end(conv_of(inc.room));
            }
        }
    }

    pub fn add_contact(self: &Rc<Self>, id: EndpointId, name: &str) {
        if id == self.me() {
            self.toast("That's your own code.");
            return;
        }
        {
            let mut st = self.store.borrow_mut();
            if let Some(c) = st.contacts.iter_mut().find(|c| c.id == id) {
                c.name = name.to_string();
            } else {
                st.contacts.push(Contact::new(id, name));
            }
        }
        self.save();
        self.sidebar.refresh(self);
        self.update_home();
        self.toast(&format!("{name} added. You can call each other now."));
    }

    pub fn add_group(self: &Rc<Self>, g: Group) {
        {
            let mut st = self.store.borrow_mut();
            let room = g.room_id();
            let mut newcomers = Vec::new();
            if let Some(existing) = st.group_by_room_mut(&room) {
                if existing.owner.is_none() {
                    existing.owner = g.owner;
                }
                existing.merge_removals(&g.removed);
                for m in &g.members {
                    if existing.add_member(m.id, &m.name) {
                        newcomers.push(m.id);
                    }
                }
                // Someone new in the group: tell them who the owner removed.
                if existing.removed.is_empty() {
                    newcomers.clear();
                }
            } else {
                st.groups.push(g);
            }
            let me = self.me();
            for to in newcomers.into_iter().filter(|id| *id != me) {
                queue_news(&mut st, room, to);
            }
        }
        self.save();
        self.send_group_news(None);
        self.sidebar.refresh(self);
        self.update_home();
    }

    // ------------------------------------------------------------ network events

    fn on_net_event(self: &Rc<Self>, ev: UiEvent) {
        match ev {
            UiEvent::Ready { .. } => {}
            UiEvent::Incoming { room, from, name, group_name } => self.show_incoming(room, from, name, group_name),
            UiEvent::IncomingGone { room } => {
                let mine = self.incoming.borrow().as_ref().is_some_and(|i| i.room == room);
                if mine {
                    let inc = self.incoming.borrow_mut().take();
                    if let Some(i) = inc {
                        i.dialog.force_close();
                    }
                    self.set_ringer(None);
                    self.withdraw_notification();
                }
                // They gave up ringing: a missed call (group calls end when they empty).
                if let RoomKey::Direct(_) = room {
                    self.call_record_end(conv_of(room));
                }
            }
            UiEvent::OutgoingRinging => {
                self.outgoing.set_description(Some("Ringing…"));
            }
            UiEvent::CallActive { room } => self.on_call_active(room),
            UiEvent::CallEnded { reason } => {
                self.play_cue(Cue::Leave);
                if self.call_view.is_immersive() {
                    self.call_view.set_immersive(self, None);
                }
                self.end_call_media();
                if !reason.is_empty() {
                    self.toast(&reason);
                }
            }
            UiEvent::PeerJoined { id, name, state, app } => {
                if !app.is_empty() {
                    self.versions.borrow_mut().insert(id, app);
                }
                {
                    let mut ps = self.participants.borrow_mut();
                    if let Some(p) = ps.iter_mut().find(|p| p.id == id) {
                        p.name = name;
                        p.state = state;
                        p.reconnecting = false;
                    } else {
                        ps.push(Participant { id, name, state, reconnecting: false, direct: None, rtt_ms: 0 });
                        drop(ps);
                        self.play_cue(Cue::Join);
                    }
                }
                if self.unwatched.borrow().contains(&id) {
                    self.net.send(NetCmd::WatchScreen(id, false));
                }
                self.sync_remote_media(id);
                self.set_ringer(None);
                self.call_view.rebuild(self);
                self.sidebar.refresh_soon();
            }
            UiEvent::PeerLeft { id } => {
                self.play_cue(Cue::Leave);
                self.participants.borrow_mut().retain(|p| p.id != id);
                {
                    let mut m = self.media.borrow_mut();
                    m.remote.retain(|(p, _), _| *p != id);
                    if let Some(out) = m.audio_out.as_mut() {
                        out.remove_peer(&id);
                    }
                }
                self.call_view.rebuild(self);
                self.sidebar.refresh_soon();
            }
            UiEvent::PeerReconnecting { id } => {
                if let Some(p) = self.participants.borrow_mut().iter_mut().find(|p| p.id == id) {
                    p.reconnecting = true;
                }
                self.call_view.rebuild(self);
            }
            UiEvent::PeerState { id, state } => {
                let changed = {
                    let mut ps = self.participants.borrow_mut();
                    match ps.iter_mut().find(|p| p.id == id) {
                        Some(p) if p.state != state => {
                            p.state = state;
                            true
                        }
                        _ => false,
                    }
                };
                if changed {
                    if !state.screen {
                        self.unwatched.borrow_mut().remove(&id);
                    }
                    self.sync_remote_media(id);
                    self.call_view.rebuild(self);
                    self.sidebar.refresh_soon();
                }
            }
            UiEvent::PeerLink { id, direct, rtt_ms } => {
                if let Some(p) = self.participants.borrow_mut().iter_mut().find(|p| p.id == id) {
                    p.direct = Some(direct);
                    p.rtt_ms = rtt_ms;
                }
            }
            UiEvent::GroupUpdate { group } => {
                self.add_group(group);
            }
            UiEvent::MissedCall { from, name } => {
                let _ = from;
                self.toast(&format!("Missed call from {name}"));
                if !self.window.is_active() && !self.dnd() {
                    self.notify("missed", "Missed call", &format!("{name} tried to call you."), None, |_| {});
                }
            }
            UiEvent::Notice(msg) => self.toast(&msg),
            UiEvent::ChatReceived { from, name, group, msgs } => self.on_chat_received(from, name, group, msgs),
            UiEvent::IdentityMoved { old, new } => self.on_identity_moved(old, new),
            UiEvent::MoveDelivered { to, known } => self.on_move_delivered(to, known),
            UiEvent::GroupNews { from, news } => self.on_group_news(from, news),
            UiEvent::GroupNewsDelivered { to, room, ok } => {
                if ok {
                    let mut st = self.store.borrow_mut();
                    st.group_news_pending.retain(|p| !(p.to == to && p.room == room));
                    st.save();
                }
            }
            UiEvent::ChatDelivered { to, ids, refused } => self.on_chat_delivered(to, ids, refused),
            UiEvent::Presence { id, presence, call, app } => {
                if let Some(v) = app {
                    self.versions.borrow_mut().insert(id, v);
                }
                // They're reachable: hand over anything waiting for them.
                if presence != Presence::Offline {
                    self.deliver_chat(id);
                    self.send_group_news(Some(id));
                }
                if self.chat_view.conv() == Some(crate::chat::Conv::Direct(id)) {
                    self.chat_view.refresh_header(self);
                }
                self.presence.borrow_mut().insert(id, presence);
                match call {
                    Some(c) => self.call_presence.borrow_mut().insert(id, c),
                    None => self.call_presence.borrow_mut().remove(&id),
                };
                self.check_group_call_records();
                self.sidebar.refresh_soon();
            }
        }
    }

    pub fn dnd(&self) -> bool {
        self.store.borrow().settings.status == Status::DoNotDisturb
    }

    /// Who's in a group's call, with their media state: the live participants if we're
    /// in it, otherwise what members told us.
    pub fn group_call_members(&self, room: &crate::store::RoomId) -> Vec<(EndpointId, MediaState)> {
        if self.room.get() == Some(RoomKey::Group(*room)) {
            let mut v = vec![(self.me(), self.state.get())];
            v.extend(self.participants().iter().map(|p| (p.id, p.state)));
            return v;
        }
        let mut v: Vec<(EndpointId, MediaState)> =
            self.call_presence.borrow().iter().filter(|(_, c)| &c.room == room).map(|(id, c)| (*id, c.state)).collect();
        v.sort_by_key(|(id, _)| self.display_name(id).to_lowercase());
        v
    }

    // ------------------------------------------------------------ chat

    /// The page to show when not in a call: the open chat, or the welcome page.
    fn show_idle(&self) {
        if self.chat_view.conv().is_some() {
            self.put_chat(false);
            self.content.set_visible_child_name("chat");
        } else {
            self.content.set_visible_child_name("home");
        }
    }

    /// The conversation shown, if the chat is on screen.
    pub fn chat_open(&self) -> Option<crate::chat::Conv> {
        let shown = match self.content.visible_child_name().as_deref() {
            Some("chat") => true,
            Some("call") => self.chat_reveal.reveals_child(),
            _ => false,
        };
        self.chat_view.conv().filter(|_| shown)
    }

    pub fn show_call(&self) {
        if self.room.get().is_some() {
            self.content.set_visible_child_name("call");
        }
    }

    /// Moves the chat view into the call's side panel or onto its own page.
    fn put_chat(&self, panel: bool) {
        let root = &self.chat_view.root;
        let target = if panel { &self.chat_panel } else { &self.chat_page };
        if root.parent().as_ref() != Some(target.upcast_ref()) {
            if let Some(p) = root.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) {
                p.remove(root);
            }
            target.append(root);
        }
        self.chat_view.set_panel(panel);
        if !panel {
            self.chat_reveal.set_reveal_child(false);
        }
    }

    /// The conversation of the call we're in.
    fn call_conv(&self) -> Option<crate::chat::Conv> {
        match self.room.get()? {
            RoomKey::Direct(id) => Some(crate::chat::Conv::Direct(id)),
            RoomKey::Group(r) => Some(crate::chat::Conv::Group(r)),
        }
    }

    /// Opens a conversation: beside the call if it's the call's own (and there's
    /// room), otherwise as a page.
    /// Whether the chat panel fits beside the call. (Without this check a too-narrow
    /// window would cut off its own edge.) The sidebar is never hidden for it: only
    /// the user hides the sidebar; the chat gets its own page instead.
    fn make_room_for_chat_panel(&self) -> bool {
        const PANEL: i32 = 340;
        let call_min = self.call_view.widget().measure(gtk::Orientation::Horizontal, -1).0;
        let side = if self.split.shows_sidebar() { self.sidebar.widget().width() } else { 0 };
        self.window.width() >= call_min + PANEL + side
    }

    /// In a call but no room beside it: the chat as a page, with "Return to call".
    fn open_chat_page_in_call(&self) {
        self.put_chat(false);
        self.content.set_visible_child_name("chat");
    }

    pub fn open_chat(self: &Rc<Self>, conv: crate::chat::Conv) {
        let wide = !self.window.has_css_class("compact");
        let beside_call = wide && self.call_conv() == Some(conv) && self.room.get().is_some() && self.make_room_for_chat_panel();
        self.put_chat(beside_call);
        self.chat_view.show(self, conv);
        if beside_call {
            self.chat_reveal.set_reveal_child(true);
            self.content.set_visible_child_name("call");
        } else {
            self.content.set_visible_child_name("chat");
        }
        self.mark_read(conv);
        self.sidebar.refresh_soon();
    }

    /// The call's Chat button: toggles the side panel (or opens the chat page in
    /// narrow windows).
    pub fn toggle_call_chat(self: &Rc<Self>) {
        let Some(conv) = self.call_conv() else { return };
        if self.chat_reveal.reveals_child() && self.chat_view.conv() == Some(conv) {
            self.chat_reveal.set_reveal_child(false);
            self.sidebar.refresh_soon();
        } else {
            self.open_chat(conv);
        }
    }

    fn chat_visible(&self, conv: crate::chat::Conv) -> bool {
        if self.chat_view.conv() != Some(conv) || !self.window.is_active() {
            return false;
        }
        match self.content.visible_child_name().as_deref() {
            Some("chat") => true,
            Some("call") => self.chat_reveal.reveals_child(),
            _ => false,
        }
    }

    fn mark_read(&self, conv: crate::chat::Conv) {
        if self.chat.borrow().unread(&conv) > 0 {
            self.chat.borrow_mut().set_unread(&conv, 0);
            self.update_tray();
            self.sidebar.refresh_soon();
        }
    }

    /// Someone's name as shown in a conversation: contact name, else the name the
    /// group knows them by, else their short ID.
    pub fn name_in(&self, conv: &crate::chat::Conv, id: &EndpointId) -> String {
        if *id == self.me() {
            let n = self.store.borrow().my_name.clone();
            return if n.is_empty() { "You".into() } else { n };
        }
        if let Some(c) = self.store.borrow().contact(id) {
            return c.name.clone();
        }
        if let crate::chat::Conv::Group(r) = conv {
            if let Some(m) = self.store.borrow().groups.iter().find(|g| g.room_id() == *r).and_then(|g| g.members.iter().find(|m| m.id == *id)) {
                return m.name.clone();
            }
        }
        id.fmt_short().to_string()
    }

    /// A group's name and a "with …" line for the chat header.
    pub fn group_summary(&self, room: &crate::store::RoomId) -> (String, String) {
        let st = self.store.borrow();
        let Some(g) = st.groups.iter().find(|g| g.room_id() == *room) else { return ("Group".into(), String::new()) };
        let others: Vec<String> = g.members.iter().filter(|m| m.id != self.me()).map(|m| st.name_for(&m.id).unwrap_or_else(|| m.name.clone())).collect();
        (g.name.clone(), others.join(", "))
    }

    /// Who should receive a message in `conv` (everyone but us).
    fn chat_recipients(&self, conv: &crate::chat::Conv) -> Vec<EndpointId> {
        match conv {
            crate::chat::Conv::Direct(id) => vec![*id],
            crate::chat::Conv::Group(r) => self
                .store
                .borrow()
                .groups
                .iter()
                .find(|g| g.room_id() == *r)
                .map(|g| g.members.iter().map(|m| m.id).filter(|id| *id != self.me()).collect())
                .unwrap_or_default(),
        }
    }

    pub fn send_chat(self: &Rc<Self>, conv: crate::chat::Conv, text: String) {
        let wire = crate::proto::WireMsg { id: crate::chat::new_id(), ts: crate::chat::now_ms(), text };
        let msg = crate::chat::Message { id: wire.id.clone(), from: self.me(), ts: wire.ts, text: wire.text.clone(), call: None };
        let to = self.chat_recipients(&conv);
        {
            let mut c = self.chat.borrow_mut();
            c.add(&conv, msg.clone());
            c.queue(&conv, wire, to.clone());
        }
        if self.chat_view.conv() == Some(conv) {
            self.chat_view.append(self, &msg, (!to.is_empty()).then_some(false));
            // If it's still undelivered in a few seconds, explain why.
            let w = Rc::downgrade(self);
            glib::timeout_add_local_once(std::time::Duration::from_millis(chat::WAIT_NOTICE_MS + 500), move || {
                if let Some(a) = w.upgrade() {
                    a.chat_view.refresh_pending(&a);
                }
            });
        }
        for peer in to {
            self.deliver_chat(peer);
        }
    }

    /// Sends `peer` everything we owe them (one delivery at a time per peer).
    fn deliver_chat(self: &Rc<Self>, peer: EndpointId) {
        if self.chat_inflight.borrow().contains(&peer) {
            return;
        }
        let owed = self.chat.borrow().owed_to(&peer);
        let Some((group, msgs)) = owed.into_iter().next() else { return };
        self.chat_inflight.borrow_mut().insert(peer);
        let msgs = msgs.into_iter().take(crate::proto::MAX_CHAT_BATCH).collect();
        let name = self.store.borrow().my_name.clone();
        self.net.send(NetCmd::SendChat { to: peer, group, name, msgs });
    }

    /// Retries everything still owed to people who look reachable.
    fn deliver_all_chat(self: &Rc<Self>) {
        let peers: std::collections::HashSet<EndpointId> = self.chat.borrow().outbox.iter().flat_map(|p| p.to.clone()).collect();
        for p in peers {
            if self.presence_of(&p) != Presence::Offline || self.participants().iter().any(|x| x.id == p) {
                self.deliver_chat(p);
            }
        }
    }

    fn on_chat_delivered(self: &Rc<Self>, to: EndpointId, ids: Vec<String>, refused: bool) {
        self.chat_inflight.borrow_mut().remove(&to);
        if refused {
            self.chat.borrow_mut().refused(&to);
        }
        if !ids.is_empty() {
            self.chat.borrow_mut().delivered(&to, &ids);
            // More may be waiting (another conversation, or a bigger backlog).
            self.deliver_chat(to);
        }
        self.chat_view.refresh_pending(self);
    }

    fn on_chat_received(self: &Rc<Self>, from: EndpointId, name: String, group: Option<crate::store::RoomId>, msgs: Vec<crate::proto::WireMsg>) {
        let conv = match group {
            Some(r) => crate::chat::Conv::Group(r),
            None => crate::chat::Conv::Direct(from),
        };
        // Someone with our code messaged us first: add them, like answering a call does.
        if group.is_none() && self.store.borrow().contact(&from).is_none() && from != self.me() {
            let shown = if name.is_empty() { from.fmt_short().to_string() } else { name };
            self.store.borrow_mut().contacts.push(Contact::new(from, shown.clone()));
            self.save();
            self.push_directory();
            self.toast(&glib::markup_escape_text(&format!("{} messaged you and was added to your contacts", identity::tagged(&shown, &from))));
        }
        let mut fresh = Vec::new();
        for m in msgs {
            let msg = crate::chat::Message { id: m.id, from, ts: m.ts, text: m.text, call: None };
            if self.chat.borrow_mut().add(&conv, msg.clone()) {
                fresh.push(msg);
            }
        }
        if fresh.is_empty() {
            return;
        }
        if self.chat_view.conv() == Some(conv) {
            for m in &fresh {
                self.chat_view.append(self, m, None);
            }
        }
        if self.chat_visible(conv) {
            return;
        }
        let unread = self.chat.borrow().unread(&conv) + fresh.len() as u32;
        self.chat.borrow_mut().set_unread(&conv, unread);
        self.update_tray();
        self.sidebar.refresh_soon();
        if self.dnd() {
            return;
        }
        self.play_cue(Cue::Message);
        let who = identity::tagged(&self.name_in(&conv, &from), &from);
        let title = match conv {
            crate::chat::Conv::Group(r) => format!("{who} in {}", self.group_summary(&r).0),
            crate::chat::Conv::Direct(_) => who,
        };
        let last = &fresh[fresh.len() - 1].text;
        let body: String = last.chars().take(140).collect();
        self.notify(
            &format!("chat-{}", conv.key()),
            &title,
            &body,
            Some(("app.open-chat", Some(conv.key().to_variant()))),
            |_| {},
        );
    }

    /// Someone's Parley version, if they've told us (0.4.3 and later do).
    pub fn version_of(&self, id: &EndpointId) -> Option<String> {
        self.versions.borrow().get(id).cloned()
    }

    /// A contact's last known status (offline until we hear otherwise).
    pub fn presence_of(&self, id: &EndpointId) -> Presence {
        self.presence.borrow().get(id).copied().unwrap_or(Presence::Offline)
    }

    /// Redraws everyone's pictures (one was chosen or cleared, or the style changed).
    pub fn refresh_pictures(self: &Rc<Self>) {
        self.sidebar.refresh(self);
        if let Some(c) = self.chat_view.conv() {
            self.chat_view.show(self, c);
        }
        self.call_view.rebuild(self);
    }

    pub fn edit_picture(self: &Rc<Self>, id: EndpointId) {
        let name = if id == self.me() { self.store.borrow().my_name.clone() } else { self.known_name(&id) };
        picture::edit(self, id, &name);
    }

    /// Settings → Appearance → Pictures.
    pub fn set_picture_style(self: &Rc<Self>, style: crate::store::PictureStyle) {
        self.store.borrow_mut().settings.picture_style = style;
        self.store.borrow().save();
        identity::set_initials_style(style == crate::store::PictureStyle::Initials);
        self.refresh_pictures();
    }

    /// Someone's name: as a friend, else as their group knows them, else their short ID.
    pub fn known_name(&self, id: &EndpointId) -> String {
        let st = self.store.borrow();
        st.name_for(id)
            .or_else(|| st.groups.iter().flat_map(|g| g.members.iter()).find(|m| m.id == *id).map(|m| m.name.clone()))
            .unwrap_or_else(|| id.fmt_short().to_string())
    }

    /// Shows or hides #tags everywhere at once.
    pub fn set_show_tags(self: &Rc<Self>, on: bool) {
        self.store.borrow_mut().settings.show_tags = on;
        self.store.borrow().save();
        identity::set_show_tags(on);
        self.sidebar.refresh(self);
        if let Some(c) = self.chat_view.conv() {
            self.chat_view.show(self, c);
        }
        self.call_view.rebuild(self);
    }

    // ------------------------------------------------------------ call records

    /// "X started a call" in the conversation (kept locally, never sent).
    fn call_record_start(self: &Rc<Self>, conv: crate::chat::Conv, from: EndpointId, incoming: bool) {
        if self.open_calls.borrow().contains_key(&conv) {
            return;
        }
        let call = Some(crate::chat::CallRecord::default());
        let msg = crate::chat::Message { id: crate::chat::new_id(), from, ts: crate::chat::now_ms(), text: String::new(), call };
        self.chat.borrow_mut().add(&conv, msg.clone());
        self.open_calls
            .borrow_mut()
            .insert(conv, OpenCall { id: msg.id.clone(), started: Instant::now(), connected: None, incoming, seen_active: false });
        if self.chat_view.conv() == Some(conv) {
            self.chat_view.append(self, &msg, None);
        }
    }

    /// The call is over: how long it lasted, or that we missed it.
    fn call_record_end(self: &Rc<Self>, conv: crate::chat::Conv) {
        let Some(o) = self.open_calls.borrow_mut().remove(&conv) else { return };
        let record = match (conv, o.connected) {
            // A group call's length is the whole call's, from when it started.
            (crate::chat::Conv::Group(_), _) => crate::chat::CallRecord { lasted_s: Some(o.started.elapsed().as_secs()), missed: false },
            (_, Some(c)) => crate::chat::CallRecord { lasted_s: Some(c.elapsed().as_secs()), missed: false },
            (_, None) => crate::chat::CallRecord { lasted_s: None, missed: o.incoming },
        };
        let missed = record.missed;
        self.chat.borrow_mut().finish_call(&conv, &o.id, record);
        if missed && !self.chat_visible(conv) {
            let n = self.chat.borrow().unread(&conv) + 1;
            self.chat.borrow_mut().set_unread(&conv, n);
            self.update_tray();
            self.sidebar.refresh_soon();
        }
        if self.chat_view.conv() == Some(conv) {
            self.chat_view.show(self, conv);
        }
    }

    /// Group calls we're not in are over once nobody's left in them.
    fn check_group_call_records(self: &Rc<Self>) {
        let mine = self.room.get();
        let groups: Vec<crate::store::RoomId> = self
            .open_calls
            .borrow()
            .keys()
            .filter_map(|c| match c {
                crate::chat::Conv::Group(r) if mine != Some(RoomKey::Group(*r)) => Some(*r),
                _ => None,
            })
            .collect();
        for r in groups {
            let active = !self.group_call_members(&r).is_empty();
            let conv = crate::chat::Conv::Group(r);
            let over = {
                let mut open = self.open_calls.borrow_mut();
                let Some(o) = open.get_mut(&conv) else { continue };
                o.seen_active |= active;
                // (Right after the ring, the caller's status may not have arrived yet.)
                !active && (o.seen_active || o.started.elapsed() > std::time::Duration::from_secs(60))
            };
            if over {
                self.call_record_end(conv);
            }
        }
    }

    /// Whether this call record is for a call still going on.
    pub fn call_ongoing(&self, id: &str) -> bool {
        self.open_calls.borrow().values().any(|o| o.id == id)
    }

    // ------------------------------------------------------------ group owners

    /// The owner takes `id` out of the group: signed with our key, applied here, and
    /// passed to everyone in the group (them included, so their Parley lets go of it).
    pub fn remove_from_group(self: &Rc<Self>, room: crate::store::RoomId, id: EndpointId) {
        let me = self.me();
        let key = match crate::store::load_identity() {
            Ok(k) if k.public() == me => k,
            _ => {
                self.toast("Couldn't read your identity; nobody was removed.");
                return;
            }
        };
        let name = self.display_name(&id);
        let gname = {
            let mut st = self.store.borrow_mut();
            let Some(g) = st.group_by_room_mut(&room) else { return };
            if g.owner != Some(me) || id == me {
                return;
            }
            let r = crate::store::Removal::sign(&key, &room, id, crate::chat::now_ms() / 1000);
            g.merge_removals(&[r]);
            let to: Vec<EndpointId> = g.members.iter().map(|m| m.id).chain(std::iter::once(id)).filter(|x| *x != me).collect();
            let gname = g.name.clone();
            for t in to {
                queue_news(&mut st, room, t);
            }
            gname
        };
        tracing::info!("removed {} from a group", id.fmt_short());
        self.save();
        self.send_group_news(None);
        self.sidebar.refresh(self);
        if self.chat_view.conv() == Some(crate::chat::Conv::Group(room)) {
            self.chat_view.refresh_header(self);
        }
        self.toast(&format!("Removed {name} from {gname}."));
    }

    /// Sends what's waiting for group members (all of them, or just `only`).
    fn send_group_news(&self, only: Option<EndpointId>) {
        let st = self.store.borrow();
        for p in st.group_news_pending.iter().filter(|p| only.is_none_or(|o| o == p.to)) {
            if let Some(g) = st.group_by_room(&p.room) {
                let news = crate::proto::GroupNews { room: p.room, removed: g.removed.clone() };
                self.net.send(NetCmd::GroupNews { to: p.to, news });
            }
        }
    }

    /// A member passed on the owner's removals (checked against the owner's key).
    fn on_group_news(self: &Rc<Self>, from: EndpointId, news: crate::proto::GroupNews) {
        let me = self.me();
        let room = news.room;
        let (gname, owner, we_were_removed, newly) = {
            let mut st = self.store.borrow_mut();
            let Some(g) = st.group_by_room_mut(&room) else { return };
            let before: Vec<EndpointId> = g.removed.iter().map(|r| r.id).collect();
            if !g.merge_removals(&news.removed) {
                return;
            }
            let newly: Vec<EndpointId> = g.removed.iter().map(|r| r.id).filter(|id| !before.contains(id)).collect();
            let info = (g.name.clone(), g.owner, g.is_removed(&me), newly);
            if info.2 {
                st.groups.retain(|g| g.room_id() != room);
                st.group_news_pending.retain(|p| p.room != room);
            } else {
                // Pass it on, so it reaches everyone even if the owner goes offline.
                let g = st.group_by_room(&room).unwrap();
                let to: Vec<EndpointId> = g.members.iter().map(|m| m.id).chain(g.removed.iter().map(|r| r.id)).filter(|x| *x != me && *x != from).collect();
                for t in to {
                    queue_news(&mut st, room, t);
                }
            }
            info
        };
        self.save();
        if we_were_removed {
            let by = owner.map(|o| self.display_name(&o)).unwrap_or_else(|| "The owner".into());
            if self.chat_view.conv() == Some(crate::chat::Conv::Group(room)) {
                self.chat_view.close();
                if self.content.visible_child_name().as_deref() == Some("chat") {
                    self.show_home();
                }
            }
            self.toast(&format!("{by} removed you from {gname}."));
        } else {
            self.send_group_news(None);
            let names: Vec<String> = newly.iter().map(|id| self.display_name(id)).collect();
            if !names.is_empty() {
                self.toast(&format!("{} removed from {gname} by its owner.", names.join(", ")));
            }
            if let Some(c) = self.chat_view.conv() {
                if self.chat_open() == Some(c) {
                    self.chat_view.refresh_header(self);
                }
            }
        }
        self.sidebar.refresh(self);
    }

    // ------------------------------------------------------------ identity

    /// Tells the network engine about our identity moves still to be delivered.
    fn push_moves(&self) {
        let st = self.store.borrow();
        if !st.moves.is_empty() && !st.move_pending.is_empty() {
            self.net.send(NetCmd::Moves { chain: st.moves.clone(), pending: st.move_pending.clone() });
        }
    }

    /// Replaces our identity (for when the code got out): a new key, signed over by the
    /// old one, which is then deleted, so the old code reaches nobody from now on. Friends
    /// and group members are moved over as they come online; nobody else learns the new
    /// code.
    pub fn replace_identity(self: &Rc<Self>) {
        if self.room.get().is_some() {
            self.toast("Hang up first to replace your identity.");
            return;
        }
        let old = match crate::store::load_identity() {
            Ok(k) if k.public() == self.me() => k,
            _ => {
                self.toast("Couldn't read your current identity; nothing was changed.");
                return;
            }
        };
        let new = iroh::SecretKey::generate();
        let (old_id, new_id) = (old.public(), new.public());
        let ts = crate::chat::now_ms() / 1000;
        let notice = crate::proto::MoveNotice::sign(&old, new_id, ts);
        drop(old);
        if let Err(e) = crate::store::stage_identity(&new) {
            tracing::error!("staging new identity: {e:#}");
            self.toast("Couldn't save a new identity; nothing was changed.");
            return;
        }
        // Our own messages and group memberships follow us; the move is recorded
        // before the new key takes over (see store::load_identity).
        self.chat.borrow_mut().rename_peer(old_id, new_id);
        {
            let mut st = self.store.borrow_mut();
            st.replace_id(&old_id, &new_id);
            // Removals in groups we own are re-signed by the new identity (so people who
            // join later can check them) and passed on again.
            let mut resend = Vec::new();
            for g in st.groups.iter_mut().filter(|g| g.owner == Some(new_id) && !g.removed.is_empty()) {
                let room = g.room_id();
                g.removed = g.removed.iter().map(|r| crate::store::Removal::sign(&new, &room, r.id, r.ts)).collect();
                resend.extend(g.members.iter().map(|m| (room, m.id)).filter(|(_, id)| *id != new_id));
            }
            for (room, to) in resend {
                queue_news(&mut st, room, to);
            }
            st.moves.push(notice);
            st.move_pending = st.known_peers(&new_id);
            st.save();
            picture::load_all(&st);
        }
        if let Err(e) = crate::store::commit_identity() {
            tracing::error!("switching identity: {e:#}");
            self.toast("Couldn't switch to the new identity. Restart Parley to finish.");
            return;
        }
        tracing::info!("identity replaced: {} is now {}", old_id.fmt_short(), new_id.fmt_short());

        // A new endpoint for the new key; the old one shuts down (its code goes dark).
        let (t, hub, tx, rt) = (self.clone(), self.hub.clone(), self.ui_tx.clone(), self.rt.clone());
        glib::spawn_future_local(async move {
            match rt.spawn(crate::net::start(new, hub, tx)).await {
                Ok(Ok(handle)) => {
                    t.net.replace(handle);
                    t.me.set(new_id);
                    t.presence.borrow_mut().clear();
                    t.call_presence.borrow_mut().clear();
                    t.push_directory();
                    t.push_moves();
                    t.sidebar.refresh(&t);
                    t.update_home();
                    if let Some(c) = t.chat_view.conv() {
                        if t.chat_open() == Some(c) {
                            t.chat_view.show(&t, c);
                        }
                    }
                    t.toast(&format!(
                        "You're now {}. Your friends and groups get your new code as they come online.",
                        identity::tagged(&t.store.borrow().my_name, &new_id)
                    ));
                }
                Ok(Err(e)) => t.network_restart_failed(&format!("{e:#}")),
                Err(e) => t.network_restart_failed(&e.to_string()),
            }
        });
    }

    fn network_restart_failed(&self, why: &str) {
        tracing::error!("starting the network as the new identity failed: {why}");
        self.toast("Your new identity is saved, but the network didn't restart. Restart Parley.");
    }

    /// Who hasn't got our new identity yet (for Settings).
    pub fn move_waiting(&self) -> Vec<String> {
        let ids = self.store.borrow().move_pending.clone();
        ids.iter().map(|id| identity::tagged(&self.display_name(id), id)).collect()
    }

    fn on_move_delivered(self: &Rc<Self>, to: EndpointId, known: bool) {
        let done = {
            let mut st = self.store.borrow_mut();
            st.move_pending.retain(|id| *id != to);
            st.save();
            st.move_pending.is_empty()
        };
        tracing::info!("told {} about our new identity ({})", to.fmt_short(), if known { "followed" } else { "they don't know us" });
        if known {
            self.deliver_chat(to);
        }
        if done {
            self.toast("Everyone you know now has your new identity.");
        }
    }

    /// A friend or group member replaced their identity (proven by their old key):
    /// `olds` is every identity they went by since the one we knew.
    fn on_identity_moved(self: &Rc<Self>, olds: Vec<EndpointId>, new: EndpointId) {
        let Some(&first) = olds.first() else { return };
        let name = self.display_name(&first);
        let open = self.chat_open();
        let mut changed = false;
        for old in &olds {
            changed |= self.store.borrow_mut().replace_id(old, &new);
            self.chat.borrow_mut().rename_peer(*old, new);
            self.presence.borrow_mut().remove(old);
            self.call_presence.borrow_mut().remove(old);
            let version = self.versions.borrow_mut().remove(old);
            if let Some(v) = version {
                self.versions.borrow_mut().insert(new, v);
            }
        }
        if changed {
            self.save();
            picture::load_all(&self.store.borrow());
        }
        // An open conversation shows them under their new identity.
        match self.chat_view.conv() {
            Some(crate::chat::Conv::Direct(id)) if olds.contains(&id) => {
                if open.is_some() {
                    self.open_chat(crate::chat::Conv::Direct(new));
                } else {
                    self.chat_view.close();
                }
            }
            Some(c) if open == Some(c) => self.chat_view.show(self, c),
            _ => {}
        }
        self.sidebar.refresh(self);
        self.deliver_chat(new);
        self.toast(&format!(
            "{name} replaced their identity code (now {}). Updated in your friends and groups.",
            identity::tagged(&name, &new)
        ));
    }

    pub fn set_status(self: &Rc<Self>, st: Status) {
        self.store.borrow_mut().settings.status = st;
        self.save();
        self.net.send(NetCmd::SetStatus(st));
        self.sidebar.refresh(self);
    }

    fn show_incoming(self: &Rc<Self>, room: RoomKey, from: EndpointId, name: String, group_name: Option<String>) {
        self.call_record_start(conv_of(room), from, true);
        // Take it out first: closing fires the response handler, which reads `incoming`.
        let old = self.incoming.borrow_mut().take();
        if let Some(old) = old {
            old.dialog.force_close();
        }
        let known = self.store.borrow().contact(&from).is_some();
        // Names carry their key's tag (and the picture is drawn from the key), so a
        // stranger calling as "Mom" can't pass as your contact Mom.
        let shown = match self.store.borrow().contact(&from) {
            Some(c) => identity::tagged(&c.name, &from),
            None if name.is_empty() => from.fmt_short().to_string(),
            // A stranger always shows their tag, so they can't pass as a contact.
            None => identity::tagged_always(&name, &from),
        };
        let (heading, mut body) = match &group_name {
            Some(g) => (format!("{shown} is calling “{g}”"), "Group call".to_string()),
            None => (format!("{shown} is calling"), "Voice and video call".to_string()),
        };
        if !known {
            body = format!(
                "{body}\n\n{shown} isn't in your contacts yet. Answering adds them, so only answer if you know them."
            );
        }
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        if group_name.is_some() {
            dialog.add_responses(&[("decline", "Not now"), ("accept", "Join")]);
        } else {
            dialog.add_responses(&[("decline", "Decline"), ("accept", "Answer")]);
        }
        dialog.set_response_appearance("decline", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("accept"));
        dialog.set_close_response("decline");
        let avatar = identity::avatar(80, &shown, &from);
        avatar.set_margin_top(6);
        dialog.set_extra_child(Some(&avatar));
        let t = self.clone();
        dialog.connect_response(None, move |_, resp| {
            let still = t.incoming.borrow().as_ref().is_some_and(|i| i.room == room);
            if still {
                t.answer(resp == "accept");
            }
        });
        // Saved as the contact's name if answering adds them.
        let plain = match self.store.borrow().contact(&from) {
            Some(c) => c.name.clone(),
            None if name.is_empty() => from.fmt_short().to_string(),
            None => name,
        };
        *self.incoming.borrow_mut() = Some(Incoming { room, from, name: plain, dialog: dialog.clone() });
        // Shown inside Parley; only a direct call brings the window forward. A group call
        // just rings (whoever starts one rings everyone, and joining is optional).
        dialog.present(Some(&self.window));
        // Do Not Disturb: no sound, nothing pops up; the call is there in Parley (and in
        // the chat, as a missed call).
        if self.dnd() {
            return;
        }
        let group = group_name.is_some();
        if !group {
            self.present();
        }
        self.set_ringer(Some(RingKind::Incoming));

        if !self.window.is_active() {
            let body = if group { "Group call — join if you like" } else { "Parley call" };
            self.notify("incoming", &heading, body, Some(("app.show", None)), |n| {
                n.set_priority(if group { gio::NotificationPriority::Normal } else { gio::NotificationPriority::Urgent });
                n.add_button(if group { "Not now" } else { "Decline" }, "app.decline-call");
                n.add_button(if group { "Join" } else { "Answer" }, "app.accept-call");
            });
        }
    }

    /// Shows a desktop notification, replacing any earlier one with the same `id`.
    /// Clicking it runs `action` (an app action and its target), or just opens Parley.
    /// `extras` adds what only Linux shows (buttons, priority). On Windows it comes from
    /// the tray icon, as long as there is one.
    fn notify(
        &self,
        id: &str,
        title: &str,
        body: &str,
        action: Option<(&str, Option<glib::Variant>)>,
        extras: impl FnOnce(&gio::Notification),
    ) {
        #[cfg(windows)]
        if let Some(t) = self.tray.borrow().as_ref() {
            if t.notify(title, body) {
                let action = action.map(|(a, target)| (a.to_string(), target));
                *self.shown_notification.borrow_mut() = Some((id.to_string(), action));
                return;
            }
        }
        let n = gio::Notification::new(title);
        n.set_body(Some(body));
        if let Some((a, target)) = action {
            n.set_default_action_and_target_value(a, target.as_ref());
        }
        extras(&n);
        self.app.send_notification(Some(id), &n);
    }

    fn withdraw_notification(&self) {
        #[cfg(windows)]
        {
            let mut shown = self.shown_notification.borrow_mut();
            if shown.as_ref().is_some_and(|(id, _)| id == "incoming") {
                *shown = None;
                if let Some(t) = self.tray.borrow().as_ref() {
                    t.withdraw();
                }
            }
        }
        self.app.withdraw_notification("incoming");
    }

    fn on_call_active(self: &Rc<Self>, room: RoomKey) {
        self.withdraw_notification();
        if let RoomKey::Group(_) = room {
            self.set_ringer(None);
        }
        if self.room.get() == Some(room) {
            return;
        }
        self.room.set(Some(room));
        // A record if there isn't one yet: we started this group call, or joined one
        // without being rung (then whoever's in it started it, as far as we know).
        let conv = conv_of(room);
        if !self.open_calls.borrow().contains_key(&conv) {
            let me = self.me();
            let from = match room {
                RoomKey::Group(r) => self.group_call_members(&r).into_iter().map(|(id, _)| id).find(|id| *id != me).unwrap_or(me),
                RoomKey::Direct(_) => me,
            };
            self.call_record_start(conv, from, false);
        }
        if let Some(o) = self.open_calls.borrow_mut().get_mut(&conv) {
            o.connected.get_or_insert_with(Instant::now);
            o.seen_active = true;
        }
        self.call_started.set(Some(Instant::now()));
        self.participants.borrow_mut().clear();
        let mut st = self.state.get();
        st.camera = false;
        st.screen = false;
        st.screen_audio = false;
        self.state.set(st);
        self.start_audio();
        self.content.set_visible_child_name("call");
        self.update_home();
        self.sidebar.refresh(self);
        self.call_view.rebuild(self);
        self.net.send(NetCmd::MediaState(self.state.get()));
        if matches!(room, RoomKey::Direct(_)) {
            self.set_ringer(None);
        }
        self.update_tray();
    }

    fn end_call_media(self: &Rc<Self>) {
        // Our call's record: a direct call is over; a group call is over once empty.
        let ended = self.room.get();
        let me = self.me();
        let open: Vec<crate::chat::Conv> = self.open_calls.borrow().keys().copied().collect();
        for conv in open {
            match conv {
                crate::chat::Conv::Direct(_) => self.call_record_end(conv),
                crate::chat::Conv::Group(r) if ended == Some(RoomKey::Group(r)) => {
                    if !self.group_call_members(&r).iter().any(|(id, _)| *id != me) {
                        self.call_record_end(conv);
                    }
                }
                _ => {}
            }
        }
        self.set_ringer(None);
        self.unwatched.borrow_mut().clear();
        self.room.set(None);
        self.call_started.set(None);
        self.participants.borrow_mut().clear();
        {
            let mut m = self.media.borrow_mut();
            m.remote.clear();
            m.mic = None;
            m.camera = None;
            m.screen = None;
            m.screen_audio = None;
            m.screen_audio_pending = false;
            if let Some(stop) = m.screen_stop.take() {
                let _ = stop.send(());
            }
            m.audio_out = None;
        }
        if let Some(a) = &self.app_audio {
            a.set(AudioShare::Off);
        }
        self.hub.clear_peers();
        let mut st = self.state.get();
        st.camera = false;
        st.screen = false;
        st.screen_audio = false;
        self.state.set(st);
        self.chat_reveal.set_reveal_child(false);
        // Leave the home page or a chat where it is; only the call's own page goes.
        if matches!(self.content.visible_child_name().as_deref(), Some("call" | "outgoing")) {
            self.show_idle();
        }
        self.update_home();
        self.call_view.rebuild_empty();
        self.update_tray();
        self.sidebar.refresh_soon();
    }

    // ------------------------------------------------------------ local media

    fn on_media_error(self: &Rc<Self>) -> impl Fn(String) + 'static {
        let weak = Rc::downgrade(self);
        move |msg| {
            if let Some(t) = weak.upgrade() {
                t.toast(&msg);
            }
        }
    }

    fn start_audio(self: &Rc<Self>) {
        let settings = self.store.borrow().settings.clone();
        let speakers = devices::speakers();
        let spk = devices::find(&speakers, &settings.speaker).map(|d| d.device.clone());
        // Any error stops the playback pipeline, and with it everyone's sound: bring it
        // back rather than leaving people to rejoin the call.
        let weak = Rc::downgrade(self);
        let on_error = move |msg: String| {
            let Some(t) = weak.upgrade() else { return };
            let recently = t.audio_restarted.get().is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5));
            if recently {
                t.toast(&msg);
                return;
            }
            tracing::warn!("{msg}: restarting audio output");
            t.audio_restarted.set(Some(Instant::now()));
            glib::idle_add_local_once(move || t.restart_audio_output());
        };
        let ready = match AudioOut::start(spk.as_ref(), on_error) {
            Ok(out) => {
                out.set_deafened(self.state.get().deafened);
                let r = out.ready.clone();
                self.media.borrow_mut().audio_out = Some(out);
                Some(r)
            }
            Err(e) => {
                self.toast(&format!("Audio output failed: {e:#}"));
                None
            }
        };
        // The mic's echo canceller needs playback running (it listens to what we play).
        // If output doesn't come up, start the mic anyway so we can still be heard.
        let t = self.clone();
        let started = Instant::now();
        let room = self.room.get();
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            if t.room.get() != room {
                return glib::ControlFlow::Break;
            }
            let ok = ready.as_ref().is_some_and(|r| r.load(std::sync::atomic::Ordering::Relaxed));
            if ok {
                t.restart_mic_with(true);
                return glib::ControlFlow::Break;
            }
            if started.elapsed() > std::time::Duration::from_secs(3) {
                t.toast("Speakers aren't responding — check the output device in Settings. Your mic still works.");
                t.restart_mic_with(false);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        // Re-add remote audio branches (e.g. after a speaker change).
        let ids: Vec<EndpointId> = self.participants.borrow().iter().map(|p| p.id).collect();
        for id in ids {
            self.sync_remote_media(id);
        }
    }

    pub fn restart_audio_output(self: &Rc<Self>) {
        if self.room.get().is_none() {
            return;
        }
        {
            let mut m = self.media.borrow_mut();
            m.mic = None; // webrtcdsp holds the echo probe
            m.audio_out = None;
        }
        self.start_audio();
    }

    /// Starts capturing the picked screen. With `gpu_import`, GPU (DMA-BUF) frames are
    /// imported through GL; if that fails, capture restarts once on shared-memory
    /// frames, which every compositor supports. Other failures end the share rather
    /// than leaving friends on a black tile.
    fn start_screen_capture(
        self: &Rc<Self>,
        fd: Option<crate::media::ScreenFd>,
        node_id: u32,
        size: Option<(i32, i32)>,
        gpu_import: bool,
    ) -> anyhow::Result<()> {
        let settings = self.store.borrow().settings.clone();
        let on_error = self.screen_error_handler(node_id, size, gpu_import);
        // The old capture and encoder go first: the new encoder takes over keyframe
        // requests, which the old one would clear when dropped.
        self.media.borrow_mut().screen = None;
        #[cfg(windows)]
        let s = {
            let _ = (fd, node_id, size);
            let monitor = self.media.borrow().screen_monitor.clone().ok_or_else(|| anyhow::anyhow!("no screen chosen"))?;
            VideoSender::monitor(self.hub.clone(), &monitor, settings.screen_params(), settings.hw_encode, on_error)?
        };
        #[cfg(not(windows))]
        let s = VideoSender::screen(self.hub.clone(), fd, node_id, size, settings.screen_params(), settings.hw_encode, gpu_import, on_error)?;
        let mut m = self.media.borrow_mut();
        m.screen = Some(s);
        m.screen_source = Some((node_id, size, gpu_import));
        Ok(())
    }

    fn screen_error_handler(self: &Rc<Self>, node_id: u32, size: Option<(i32, i32)>, gpu_import: bool) -> impl Fn(String) + 'static {
        let weak = Rc::downgrade(self);
        move |msg: String| {
            let Some(t) = weak.upgrade() else { return };
            // (Ended, or already moved on to shared memory.)
            if !t.state.get().screen || t.media.borrow().screen_source.is_some_and(|s| s.2 != gpu_import) {
                return;
            }
            if gpu_import {
                tracing::warn!("screen: GPU frame import failed ({msg}); capturing through shared memory instead");
                let fd = t.media.borrow().screen.as_ref().and_then(|s| s.capture_fd());
                t.media.borrow_mut().screen_source = Some((node_id, size, false));
                // Replace the failed capture after this error callback returns.
                glib::idle_add_local_once(move || {
                    if let Err(e) = t.start_screen_capture(fd, node_id, size, false) {
                        t.toast(&format!("Screen sharing stopped — {e:#}"));
                        t.stop_screen_share();
                    }
                });
                return;
            }
            t.toast(&format!("Screen sharing stopped — {msg}"));
            t.stop_screen_share();
        }
    }

    /// New resolution or frame rate for the running share. The capture and the call
    /// state carry on, so nobody's view of the stream changes but its quality.
    pub fn apply_stream_quality(self: &Rc<Self>) {
        let Some((node_id, size, gpu_import)) = self.media.borrow().screen_source else { return };
        let Some(old) = self.media.borrow_mut().screen.take() else { return };
        let settings = self.store.borrow().settings.clone();
        let on_error = self.screen_error_handler(node_id, size, gpu_import);
        match old.with_quality(settings.screen_params(), settings.hw_encode, on_error) {
            Ok(s) => {
                self.media.borrow_mut().screen = Some(s);
                self.call_view.rebuild(self);
            }
            Err(e) => {
                self.toast(&format!("Screen sharing stopped — {e:#}"));
                self.stop_screen_share();
            }
        }
    }

    /// Windows: which screen to share (asked only when there's more than one).
    #[cfg(windows)]
    async fn pick_monitor(self: &Rc<Self>) -> anyhow::Result<ScreenPick> {
        let monitors = crate::media::devices::monitors();
        let chosen = match monitors.len() {
            0 => anyhow::bail!("Windows didn't report any screen to capture."),
            1 => monitors[0].clone(),
            _ => dialogs::pick_monitor(self, &monitors).await.ok_or_else(|| anyhow::anyhow!("Cancelled"))?,
        };
        let size = crate::media::devices::monitor_size(&chosen.device);
        self.media.borrow_mut().screen_monitor = Some(chosen.device);
        // Nothing like a portal session to end afterwards.
        let (stop, _) = tokio::sync::oneshot::channel();
        Ok(ScreenPick { fd: None, node_id: 0, size, stop })
    }

    /// Pushes the voice settings to the running microphone (no restart needed).
    pub fn apply_voice(&self) {
        let c = self.store.borrow().settings.voice_config();
        self.voice.denoise.store(c.denoise, std::sync::atomic::Ordering::Relaxed);
        self.voice.auto_gain.store(c.auto_gain, std::sync::atomic::Ordering::Relaxed);
        match c.gate {
            None => self.voice.set_gate(Gate::Off),
            Some(None) => self.voice.set_gate(Gate::Auto),
            Some(Some(db)) => {
                self.voice.set_gate(Gate::Threshold);
                self.voice.set_threshold_db(db);
            }
        }
    }

    /// Changes voice settings: saves them, applies them live, and restarts the mic
    /// only when the WebRTC suppressor has to be switched on or off.
    pub fn update_voice(self: &Rc<Self>, f: impl FnOnce(&mut crate::store::Settings)) {
        let before = self.store.borrow().settings.voice_config().webrtc_ns;
        f(&mut self.store.borrow_mut().settings);
        self.save();
        self.apply_voice();
        if self.store.borrow().settings.voice_config().webrtc_ns != before {
            self.restart_any_mic();
        }
    }

    /// Runs the microphone outside a call so Settings can show its level.
    pub fn start_mic_test(self: &Rc<Self>) {
        if self.room.get().is_some() || self.mic_test.borrow().is_some() {
            return;
        }
        self.apply_voice();
        let settings = self.store.borrow().settings.clone();
        let mics = devices::microphones();
        let opts = MicOptions {
            device: devices::find(&mics, &settings.mic).map(|d| d.device.clone()),
            echo_cancel: false,
            webrtc_ns: settings.voice_config().webrtc_ns,
            voice: self.voice.clone(),
        };
        // Its own hub with nobody in it: the audio goes nowhere, unless "hear yourself"
        // is on, in which case it comes back a second later through the same Opus
        // decoding and playback path a friend's voice takes.
        let hub = Hub::new();
        if self.mic_playback.get() {
            hub.set_loopback(self.me(), std::time::Duration::from_millis(1000));
            let spk = devices::find(&devices::speakers(), &settings.speaker).map(|d| d.device.clone());
            match AudioOut::start(spk.as_ref(), |e| tracing::warn!("mic playback: {e}")) {
                Ok(mut out) => {
                    let _ = out.add(&hub, self.me(), Track::Mic);
                    *self.mic_test_out.borrow_mut() = Some(out);
                }
                Err(e) => tracing::warn!("mic playback failed: {e:#}"),
            }
        }
        match MicSender::start(hub, self.me(), opts, false, |_| {}) {
            Ok(m) => *self.mic_test.borrow_mut() = Some(m),
            Err(e) => tracing::warn!("mic test failed: {e:#}"),
        }
    }

    pub fn stop_mic_test(&self) {
        self.mic_test.borrow_mut().take();
        self.mic_test_out.borrow_mut().take();
    }

    /// Turns "hear yourself" on or off (mic test only, outside calls).
    pub fn set_mic_playback(self: &Rc<Self>, on: bool) {
        self.mic_playback.set(on);
        if self.room.get().is_none() {
            self.stop_mic_test();
            self.start_mic_test();
        }
    }

    /// Restarts whichever microphone is running (call or test) with new settings.
    pub fn restart_any_mic(self: &Rc<Self>) {
        if self.mic_test.borrow_mut().take().is_some() {
            self.start_mic_test();
        }
        self.restart_mic();
    }

    pub fn restart_mic(self: &Rc<Self>) {
        let out_ok = self
            .media
            .borrow()
            .audio_out
            .as_ref()
            .is_some_and(|o| o.ready.load(std::sync::atomic::Ordering::Relaxed));
        self.restart_mic_with(out_ok);
    }

    fn restart_mic_with(self: &Rc<Self>, echo_probe_ready: bool) {
        if self.room.get().is_none() {
            return;
        }
        self.stop_mic_test();
        self.media.borrow_mut().mic = None;
        let settings = self.store.borrow().settings.clone();
        let mics = devices::microphones();
        let mut dev = devices::find(&mics, &settings.mic).map(|d| d.device.clone());
        // "System default" pointing at Bluetooth earbuds would flip them into headset mode
        // (telephone-quality and, on many adapters, silent). Use another mic instead unless
        // the user picked the headset mic explicitly or allowed it.
        if dev.is_none() && !settings.allow_bluetooth_mic {
            let default = self.app_audio.as_ref().and_then(|a| a.default_source.lock().unwrap().clone());
            let default_is_bt = default
                .as_ref()
                .is_some_and(|n| n.starts_with("bluez_") || mics.iter().any(|m| devices::node_name(m).as_ref() == Some(n) && devices::is_bluetooth(m)));
            if default_is_bt {
                if let Some(alt) = mics.iter().find(|m| !devices::is_bluetooth(m)) {
                    tracing::info!("microphone: default {default:?} is a Bluetooth headset; using {} so earbuds stay in high-quality mode", alt.name);
                    if !self.bt_mic_hint_shown.replace(true) {
                        self.toast(&format!(
                            "Using {} as the mic so your Bluetooth earbuds keep high-quality sound. Change in Settings → Microphone.",
                            alt.name
                        ));
                    }
                    dev = Some(alt.device.clone());
                }
            }
        }
        let st = self.state.get();
        self.apply_voice();
        let opts = MicOptions {
            device: dev,
            echo_cancel: settings.echo_cancel && echo_probe_ready,
            webrtc_ns: settings.voice_config().webrtc_ns,
            voice: self.voice.clone(),
        };
        match MicSender::start(self.hub.clone(), self.me(), opts, st.mic_muted || st.deafened, self.on_media_error()) {
            Ok(m) => self.media.borrow_mut().mic = Some(m),
            Err(e) => self.toast(&format!("Microphone failed: {e:#}")),
        }
    }

    fn sync_remote_media(self: &Rc<Self>, id: EndpointId) {
        let Some(p) = self.participants.borrow().iter().find(|p| p.id == id).cloned() else { return };
        let mut m = self.media.borrow_mut();
        if let Some(out) = m.audio_out.as_mut() {
            for t in [Track::Mic, Track::ScreenAudio] {
                if let Err(e) = out.add(&self.hub, id, t) {
                    tracing::error!("audio branch: {e:#}");
                }
            }
        }
        let watching = !self.unwatched.borrow().contains(&id);
        for (track, on) in [(Track::Camera, p.state.camera), (Track::Screen, p.state.screen && watching)] {
            let has = m.remote.contains_key(&(id, track));
            if on && !has {
                match VideoReceiver::start(&self.hub, id, track, self.on_media_error()) {
                    Ok(r) => {
                        m.remote.insert((id, track), r);
                        self.net.send(NetCmd::RequestKeyframe(id, track));
                    }
                    Err(e) => {
                        drop(m);
                        self.toast(&format!("Can't show video: {e:#}"));
                        m = self.media.borrow_mut();
                    }
                }
            } else if !on && has {
                m.remote.remove(&(id, track));
            }
        }
    }

    pub fn remote_paintable(&self, id: &EndpointId, track: Track) -> Option<gtk::gdk::Paintable> {
        self.media.borrow().remote.get(&(*id, track)).map(|r| r.paintable.clone())
    }

    pub fn local_paintable(&self, track: Track) -> Option<gtk::gdk::Paintable> {
        let m = self.media.borrow();
        match track {
            Track::Camera => m.camera.as_ref().map(|c| c.preview.clone()),
            Track::Screen => m.screen.as_ref().map(|c| c.preview.clone()),
            _ => None,
        }
    }

    pub fn is_watching(&self, id: &EndpointId) -> bool {
        !self.unwatched.borrow().contains(id)
    }

    /// Stop/start watching someone's screen share. The sharer stops sending it to us
    /// (video and its audio), so this also saves bandwidth.
    pub fn set_watching(self: &Rc<Self>, id: EndpointId, on: bool) {
        if on {
            self.unwatched.borrow_mut().remove(&id);
        } else {
            self.unwatched.borrow_mut().insert(id);
        }
        self.net.send(NetCmd::WatchScreen(id, on));
        self.sync_remote_media(id);
        self.call_view.rebuild(self);
    }

    pub fn track_volume(&self, id: &EndpointId, track: Track) -> f64 {
        self.media.borrow().audio_out.as_ref().map(|o| o.volume(id, track)).unwrap_or(1.0)
    }

    pub fn set_track_volume(&self, id: EndpointId, track: Track, v: f64) {
        if let Some(o) = self.media.borrow_mut().audio_out.as_mut() {
            o.set_volume(id, track, v);
        }
    }

    pub fn track_muted(&self, id: &EndpointId, track: Track) -> bool {
        self.media.borrow().audio_out.as_ref().is_some_and(|o| o.is_muted(id, track))
    }

    pub fn set_track_muted(&self, id: EndpointId, track: Track, muted: bool) {
        if let Some(o) = self.media.borrow_mut().audio_out.as_mut() {
            o.set_muted(id, track, muted);
        }
    }

    fn publish_state(&self, st: MediaState) {
        self.state.set(st);
        self.sidebar.refresh_soon();
        self.net.send(NetCmd::MediaState(st));
    }

    pub fn toggle_mute(self: &Rc<Self>) {
        let mut st = self.state.get();
        if st.deafened {
            // Unmuting while deafened undeafens too, like Discord.
            st.deafened = false;
            st.mic_muted = false;
        } else {
            st.mic_muted = !st.mic_muted;
        }
        self.apply_audio_state(st);
    }

    pub fn toggle_deafen(self: &Rc<Self>) {
        let mut st = self.state.get();
        st.deafened = !st.deafened;
        self.apply_audio_state(st);
    }

    fn apply_audio_state(self: &Rc<Self>, st: MediaState) {
        {
            let m = self.media.borrow();
            if let Some(mic) = &m.mic {
                mic.set_muted(st.mic_muted || st.deafened);
            }
            if let Some(out) = &m.audio_out {
                out.set_deafened(st.deafened);
            }
        }
        self.publish_state(st);
        self.call_view.rebuild(self);
        self.update_tray();
    }

    pub fn toggle_camera(self: &Rc<Self>) {
        let mut st = self.state.get();
        if st.camera {
            self.media.borrow_mut().camera = None;
            st.camera = false;
        } else {
            let settings = self.store.borrow().settings.clone();
            let cams = devices::cameras();
            let dev = devices::find(&cams, &settings.camera).or(cams.first()).map(|d| d.device.clone());
            let Some(dev) = dev else {
                self.toast("No camera found.");
                return;
            };
            match VideoSender::camera(self.hub.clone(), &dev, settings.camera_kbps, settings.hw_encode, self.on_media_error()) {
                Ok(c) => {
                    self.media.borrow_mut().camera = Some(c);
                    st.camera = true;
                }
                Err(e) => {
                    self.toast(&format!("Camera failed: {e:#}"));
                    return;
                }
            }
        }
        self.publish_state(st);
        self.call_view.rebuild(self);
    }

    pub fn restart_camera(self: &Rc<Self>) {
        if self.state.get().camera {
            self.toggle_camera();
            self.toggle_camera();
        }
    }

    pub fn stop_screen_share(self: &Rc<Self>) {
        {
            let mut m = self.media.borrow_mut();
            m.screen = None;
            m.screen_audio = None;
            m.screen_audio_pending = false;
            if let Some(stop) = m.screen_stop.take() {
                let _ = stop.send(());
            }
        }
        if let Some(a) = &self.app_audio {
            a.set(AudioShare::Off);
        }
        let mut st = self.state.get();
        st.screen = false;
        st.screen_audio = false;
        self.publish_state(st);
        self.call_view.rebuild(self);
    }

    /// Opens the portal picker and starts sharing.
    pub fn start_screen_share(self: &Rc<Self>, audio: AudioShare) {
        let t = self.clone();
        let rt = self.rt.clone();
        glib::spawn_future_local(async move {
            // Test hook: share an existing PipeWire screencast node instead of asking the portal.
            let test_node = std::env::var("PARLEY_TEST_SCREEN_NODE").ok().and_then(|v| v.parse::<u32>().ok());
            let pick: Result<anyhow::Result<ScreenPick>, tokio::task::JoinError> = match test_node {
                Some(node_id) => {
                    let (stop, _) = tokio::sync::oneshot::channel();
                    Ok(Ok(ScreenPick { fd: None, node_id, size: None, stop }))
                }
                #[cfg(windows)]
                None => {
                    let _ = &rt;
                    Ok(t.pick_monitor().await)
                }
                #[cfg(not(windows))]
                None => rt.spawn(crate::media::portal::pick_screen()).await,
            };
            let pick: ScreenPick = match pick {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => {
                    let msg = format!("{e:#}");
                    if !msg.contains("Cancelled") && !msg.contains("cancelled") {
                        t.toast(&format!("Screen sharing unavailable: {msg}"));
                    }
                    return;
                }
                Err(_) => return,
            };
            if t.room.get().is_none() {
                let _ = pick.stop.send(());
                return;
            }
            // Already sharing: this swaps what's shared. Friends' view of the stream carries on.
            let was_sharing = t.state.get().screen;
            let old_stop = t.media.borrow_mut().screen_stop.take();
            // (No GPU import to fall back from on Windows.)
            let started = t.start_screen_capture(pick.fd, pick.node_id, pick.size, cfg!(not(windows)));
            if let Some(stop) = old_stop {
                let _ = stop.send(());
            }
            if let Err(e) = started {
                let _ = pick.stop.send(());
                t.toast(&format!("Screen share failed: {e:#}"));
                if was_sharing {
                    t.stop_screen_share();
                }
                return;
            }
            t.media.borrow_mut().screen_stop = Some(pick.stop);
            if was_sharing {
                t.set_share_audio(audio);
                t.call_view.rebuild(&t);
                return;
            }
            let mut st = t.state.get();
            st.screen = true;
            *t.share_audio.borrow_mut() = audio.clone();
            st.screen_audio = audio != AudioShare::Off && t.begin_screen_audio(&audio);
            t.publish_state(st);
            t.call_view.rebuild(&t);
        });
    }

    pub fn set_share_audio(self: &Rc<Self>, audio: AudioShare) {
        *self.share_audio.borrow_mut() = audio.clone();
        if !self.state.get().screen {
            return;
        }
        let mut st = self.state.get();
        if audio == AudioShare::Off {
            self.media.borrow_mut().screen_audio = None;
            st.screen_audio = false;
            if let Some(a) = &self.app_audio {
                a.set(AudioShare::Off);
            }
        } else if self.media.borrow().screen_audio.is_none() {
            st.screen_audio = self.begin_screen_audio(&audio);
        } else if let Some(a) = &self.app_audio {
            // Already recording: just changes which apps play into the sink.
            a.set(audio.clone());
        }
        self.publish_state(st);
    }

    /// Starts recording the sound shared with the screen; returns whether it's on. On
    /// Linux recording begins once PipeWire reports the virtual sink (SinkReady); on
    /// Windows the desktop's sound (all but Parley's own) is recorded straight away.
    fn begin_screen_audio(self: &Rc<Self>, mode: &AudioShare) -> bool {
        #[cfg(windows)]
        {
            let _ = mode;
            match ScreenAudioSender::desktop(self.hub.clone(), self.on_media_error()) {
                Ok(s) => {
                    self.media.borrow_mut().screen_audio = Some(s);
                    true
                }
                Err(e) => {
                    self.toast(&format!("Screen audio failed: {e:#}"));
                    false
                }
            }
        }
        #[cfg(not(windows))]
        {
            let Some(a) = &self.app_audio else {
                self.toast("Can't capture app audio: PipeWire isn't available.");
                return false;
            };
            self.media.borrow_mut().screen_audio_pending = true;
            a.set(mode.clone());
            true
        }
    }

    fn start_screen_audio_if_pending(self: &Rc<Self>) {
        let Some(a) = &self.app_audio else { return };
        let mut m = self.media.borrow_mut();
        if !m.screen_audio_pending || m.screen_audio.is_some() || m.screen.is_none() {
            return;
        }
        let sink = a.sink_name.clone();
        m.screen_audio_pending = false;
        drop(m);
        match ScreenAudioSender::start(self.hub.clone(), &sink, self.on_media_error()) {
            Ok(s) => self.media.borrow_mut().screen_audio = Some(s),
            Err(e) => self.toast(&format!("Screen audio failed: {e:#}")),
        }
    }

    // ------------------------------------------------------------ ringing

    fn set_ringer(&self, kind: Option<RingKind>) {
        let mut r = self.ringer.borrow_mut();
        match kind {
            None => *r = None,
            Some(k) => {
                if r.as_ref().is_some_and(|x| x.kind == k) {
                    return;
                }
                *r = self.sound_sink().and_then(|sink| Ringer::start(k, sink));
            }
        }
    }
}

/// Remembers that `to` should hear the group's removals (until they confirm).
fn queue_news(st: &mut Store, room: crate::store::RoomId, to: EndpointId) {
    let p = crate::store::PendingNews { room, to };
    if !st.group_news_pending.contains(&p) {
        st.group_news_pending.push(p);
    }
}

fn load_css() {
    let display = gtk::gdk::Display::default().expect("display");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    // Light appearance: Parley's own (Discord-like) colours, swapped in while light.
    let light = gtk::CssProvider::new();
    light.load_from_string(include_str!("style-light.css"));
    let apply = move |sm: &adw::StyleManager| {
        gtk::style_context_remove_provider_for_display(&display, &light);
        if !sm.is_dark() {
            gtk::style_context_add_provider_for_display(&display, &light, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        }
    };
    let sm = adw::StyleManager::default();
    apply(&sm);
    sm.connect_dark_notify(apply);
}

/// Parley's icons are built in (Adwaita's, renamed), so a desktop's own icon theme
/// (KDE's Breeze, say) can't swap them; GTK's own icons (menus, window buttons) are
/// taken from Adwaita too.
fn load_icons() {
    gio::resources_register_include!("parley.gresource").expect("built-in icons");
    let display = gtk::gdk::Display::default().expect("display");
    gtk::IconTheme::for_display(&display).add_resource_path("/io/github/parley/Parley/icons");
    if let Some(s) = gtk::Settings::default() {
        s.set_gtk_icon_theme_name(Some("Adwaita"));
    }
}

/// Settings → Appearance. "Follow system" is dark unless the system asks for light.
pub fn apply_appearance(a: crate::store::Appearance) {
    use crate::store::Appearance;
    adw::StyleManager::default().set_color_scheme(match a {
        Appearance::System => adw::ColorScheme::PreferDark,
        Appearance::Light => adw::ColorScheme::ForceLight,
        Appearance::Dark => adw::ColorScheme::ForceDark,
    });
}

// ------------------------------------------------------------ ringtone

#[derive(Clone, Copy, PartialEq, Eq)]
enum RingKind {
    Incoming,
    Outgoing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cue {
    Join,
    Leave,
    /// A new chat message (a single soft note).
    Message,
}

thread_local! {
    static LAST_CUE: Cell<Option<Instant>> = const { Cell::new(None) };
}

impl App {
    /// A short two-note chime: rising when someone joins, falling when someone leaves.
    /// Where chimes and ringing go: the chosen speaker, else the default one.
    fn sound_sink(&self) -> Option<gst::Element> {
        use gst::prelude::*;
        let name = self.store.borrow().settings.speaker.clone();
        let speaker = devices::find(&devices::speakers(), &name).and_then(|d| d.device.create_element(None).ok());
        speaker.map(Ok).unwrap_or_else(|| gst::ElementFactory::make("autoaudiosink").build()).ok()
    }

    fn play_cue(&self, cue: Cue) {
        use gst::prelude::*;
        // Several people arriving at once (joining a group) get one chime.
        let now = Instant::now();
        if LAST_CUE.with(|c| c.get().is_some_and(|t| now.duration_since(t).as_millis() < 400)) {
            return;
        }
        LAST_CUE.with(|c| c.set(Some(now)));
        let (a, b) = match cue {
            Cue::Join => (523.25, 783.99),
            Cue::Leave => (783.99, 523.25),
            Cue::Message => (1046.5, 0.0),
        };
        const SR: f64 = 48_000.0;
        let note = |f: f64, n: usize| {
            (0..n).map(move |i| {
                let t = i as f64 / SR;
                let env = (1.0 - (-t * 400.0).exp()) * (-t * 18.0).exp();
                (env * 0.22 * ((2.0 * std::f64::consts::PI * f * t).sin() + 0.25 * (4.0 * std::f64::consts::PI * f * t).sin()) * 32767.0)
                    as i16
            })
        };
        let pcm: Vec<u8> = if b == 0.0 {
            note(a, 9_000).map(|s| s / 2).flat_map(|s| s.to_le_bytes()).collect()
        } else {
            note(a, 6_000).chain(note(b, 12_000)).flat_map(|s| s.to_le_bytes()).collect()
        };

        let Some(sink) = self.sound_sink() else { return };
        let src = gst_app::AppSrc::builder()
            .caps(&gst::Caps::builder("audio/x-raw").field("format", "S16LE").field("layout", "interleaved").field("rate", 48_000i32).field("channels", 1i32).build())
            .format(gst::Format::Time)
            .build();
        let pipeline = gst::Pipeline::new();
        let Ok(conv) = gst::ElementFactory::make("audioconvert").build() else { return };
        if pipeline.add_many([src.upcast_ref(), &conv, &sink]).is_err() || gst::Element::link_many([src.upcast_ref(), &conv, &sink]).is_err() {
            return;
        }
        let mut buf = gst::Buffer::from_mut_slice(pcm);
        buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        let _ = src.push_buffer(buf);
        let _ = src.end_of_stream();
        // Tear down once played (or after a few seconds whatever happens).
        let p = pipeline.clone();
        pipeline.call_async(|p| {
            let _ = p.set_state(gst::State::Playing);
        });
        glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
            p.call_async(|p| {
                let _ = p.set_state(gst::State::Null);
            });
        });
    }
}

struct Ringer {
    kind: RingKind,
    pipeline: gst::Pipeline,
}

const SR: usize = 48_000;

/// Adds a soft struck-bell note (marimba-like: quick attack, smooth decay, gentle
/// overtones that fade first) to `out` at `at` seconds.
fn bell(out: &mut [f32], at: f64, freq: f64, gain: f64, decay: f64) {
    use std::f64::consts::TAU;
    let start = (at * SR as f64) as usize;
    for (i, o) in out.iter_mut().skip(start).enumerate() {
        let t = i as f64 / SR as f64;
        let env = (1.0 - (-t * 500.0).exp()) * (-t * decay).exp();
        if t > 0.05 && env < 1e-4 {
            break;
        }
        let tone = (TAU * freq * t).sin() + 0.18 * (-t * decay * 2.0).exp() * (TAU * 2.0 * freq * t).sin() + 0.06 * (-t * decay * 4.0).exp() * (TAU * 3.0 * freq * t).sin();
        *o += (gain * env * tone) as f32;
    }
}

/// One period of a ring, as 48 kHz mono S16LE.
fn ring_pcm(kind: RingKind) -> Vec<u8> {
    const C5: f64 = 523.25;
    const E5: f64 = 659.25;
    const G5: f64 = 783.99;
    const C6: f64 = 1046.5;
    let (period, notes, gain, decay): (f64, &[(f64, f64)], f64, f64) = match kind {
        // A rising chime, then a softer answer, then quiet.
        RingKind::Incoming => (3.2, &[(0.0, C5), (0.16, E5), (0.32, G5), (0.48, C6), (1.15, E5), (1.31, G5), (1.47, C6)], 0.16, 4.5),
        // A quiet double tap while it rings at the other end.
        RingKind::Outgoing => (3.5, &[(0.0, E5), (0.32, E5)], 0.07, 5.5),
    };
    let mut out = vec![0f32; (period * SR as f64) as usize];
    for &(at, f) in notes {
        bell(&mut out, at, f, gain, decay);
    }
    out.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect()
}

/// One period of the incoming (or outgoing) ring, 48 kHz mono S16LE, for listening
/// to it outside a call (`cargo run --example ring_preview`).
pub fn ring_preview(incoming: bool) -> Vec<u8> {
    ring_pcm(if incoming { RingKind::Incoming } else { RingKind::Outgoing })
}

impl Ringer {
    fn start(kind: RingKind, sink: gst::Element) -> Option<Self> {
        use gst::prelude::*;
        let period = std::sync::Arc::new(ring_pcm(kind));
        let dur = gst::ClockTime::from_nseconds((period.len() / 2) as u64 * 1_000_000_000 / SR as u64);
        let src = gst_app::AppSrc::builder()
            .caps(&gst::Caps::builder("audio/x-raw").field("format", "S16LE").field("layout", "interleaved").field("rate", SR as i32).field("channels", 1i32).build())
            .format(gst::Format::Time)
            .build();
        // Loops the period for as long as it rings.
        let n = std::sync::atomic::AtomicU64::new(0);
        src.set_callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |src, _| {
                    let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut buf = gst::Buffer::from_slice(period.as_ref().clone());
                    if let Some(b) = buf.get_mut() {
                        b.set_pts(dur * i);
                        b.set_duration(dur);
                    }
                    let _ = src.push_buffer(buf);
                })
                .build(),
        );
        let pipeline = gst::Pipeline::new();
        let conv = gst::ElementFactory::make("audioconvert").build().ok()?;
        let resample = gst::ElementFactory::make("audioresample").build().ok()?;
        pipeline.add_many([src.upcast_ref(), &conv, &resample, &sink]).ok()?;
        gst::Element::link_many([src.upcast_ref(), &conv, &resample, &sink]).ok()?;
        pipeline.call_async(|p| {
            let _ = p.set_state(gst::State::Playing);
        });
        Some(Self { kind, pipeline })
    }
}

impl Drop for Ringer {
    fn drop(&mut self) {
        use gst::prelude::*;
        self.pipeline.call_async(|p| {
            let _ = p.set_state(gst::State::Null);
        });
    }
}
