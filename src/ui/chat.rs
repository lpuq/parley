//! The chat view (Discord-style): a header, the message list with messages grouped
//! under the sender's picture and name, and the composer. One instance is moved
//! between the main page and the side panel of a call.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use adw::prelude::*;
use gtk::glib;
use iroh::EndpointId;

use super::{App, identity};
use crate::{
    chat::{Conv, Message},
    proto::{MAX_CHAT_CHARS, Presence},
};

/// Messages from the same person within this many ms share one header.
const GROUP_MS: u64 = 7 * 60 * 1000;

pub struct ChatView {
    app: Weak<App>,
    pub root: gtk::Box,
    avatar_slot: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    call_btn: gtk::Button,
    back_to_call: gtk::Button,
    scroller: gtk::ScrolledWindow,
    list: gtk::Box,
    input: gtk::TextView,
    placeholder: gtk::Label,
    notice: gtk::Label,
    conv: Cell<Option<Conv>>,
    /// Shown in a call's side panel (no header buttons) rather than as a page.
    panel: Cell<bool>,
    /// For grouping: the last message's sender, time and day.
    last: Cell<Option<(EndpointId, u64, i32)>>,
    /// Message id → its text label (to un-grey once delivered).
    labels: RefCell<HashMap<String, gtk::Label>>,
}

impl ChatView {
    pub fn new(app: Weak<App>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("chat-view");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.add_css_class("chat-header");
        let avatar_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        avatar_slot.set_valign(gtk::Align::Center);
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
        titles.set_valign(gtk::Align::Center);
        titles.set_hexpand(true);
        let title = gtk::Label::new(None);
        title.add_css_class("heading");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        titles.append(&title);
        titles.append(&subtitle);
        let back_to_call = gtk::Button::builder().label("Return to call").css_classes(["pill", "join-btn"]).valign(gtk::Align::Center).build();
        let call_btn = gtk::Button::builder().css_classes(["circular", "call-row-btn"]).valign(gtk::Align::Center).build();
        header.append(&avatar_slot);
        header.append(&titles);
        header.append(&back_to_call);
        header.append(&call_btn);
        root.append(&header);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("chat-list");
        list.set_valign(gtk::Align::End);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        root.append(&scroller);

        let notice = gtk::Label::new(None);
        notice.add_css_class("chat-notice");
        notice.set_wrap(true);
        notice.set_xalign(0.0);
        notice.set_visible(false);
        root.append(&notice);

        let composer = gtk::Overlay::new();
        composer.add_css_class("chat-composer");
        let input = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(10)
            .bottom_margin(10)
            .left_margin(14)
            .right_margin(14)
            .build();
        input.add_css_class("chat-input");
        let placeholder = gtk::Label::new(None);
        placeholder.add_css_class("dim-label");
        placeholder.set_halign(gtk::Align::Start);
        placeholder.set_valign(gtk::Align::Center);
        placeholder.set_margin_start(16);
        placeholder.set_can_target(false);
        composer.set_child(Some(&input));
        composer.add_overlay(&placeholder);
        root.append(&composer);

        let this = Rc::new(Self {
            app,
            root,
            avatar_slot,
            title,
            subtitle,
            call_btn,
            back_to_call,
            scroller,
            list,
            input,
            placeholder,
            notice,
            conv: Cell::new(None),
            panel: Cell::new(false),
            last: Cell::new(None),
            labels: RefCell::new(HashMap::new()),
        });

        // Enter sends; Shift+Enter is a new line.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
            if !enter || mods.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                return glib::Propagation::Proceed;
            }
            if let Some(v) = w.upgrade() {
                v.send();
            }
            glib::Propagation::Stop
        });
        this.input.add_controller(keys);
        let p = this.placeholder.clone();
        this.input.buffer().connect_changed(move |b| p.set_visible(b.char_count() == 0));

        let w = Rc::downgrade(&this);
        this.call_btn.connect_clicked(move |_| {
            let Some(v) = w.upgrade() else { return };
            let (Some(a), Some(conv)) = (v.app.upgrade(), v.conv.get()) else { return };
            match conv {
                Conv::Direct(id) => a.call_contact(id),
                Conv::Group(r) => a.join_group(r),
            }
        });
        let w = Rc::downgrade(&this);
        this.back_to_call.connect_clicked(move |_| {
            if let Some(a) = w.upgrade().and_then(|v| v.app.upgrade()) {
                a.show_call();
            }
        });
        this
    }

    pub fn conv(&self) -> Option<Conv> {
        self.conv.get()
    }

    pub fn set_panel(&self, panel: bool) {
        self.panel.set(panel);
        if let Some(a) = self.app.upgrade() {
            self.refresh_header(&a);
        }
    }

    /// Opens a conversation: header, full history, and focus in the composer.
    pub fn show(&self, app: &Rc<App>, conv: Conv) {
        let changed = self.conv.replace(Some(conv)) != Some(conv);
        if changed {
            self.input.buffer().set_text("");
        }
        self.refresh_header(app);
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.labels.borrow_mut().clear();
        self.last.set(None);
        let msgs = app.chat.borrow_mut().messages(&conv).clone();
        let pending = app.chat.borrow().pending_in(&conv);
        if msgs.is_empty() {
            let hint = gtk::Label::new(Some(match conv {
                Conv::Direct(_) => "This is the start of your conversation. Messages go straight to them, encrypted; if they're offline, Parley delivers them when you're both online.",
                Conv::Group(_) => "This is the start of the group's chat. Messages go to every member, encrypted; anyone offline gets them when you're both online.",
            }));
            hint.add_css_class("dim-label");
            hint.set_wrap(true);
            hint.set_margin_start(16);
            hint.set_margin_end(16);
            hint.set_margin_bottom(12);
            hint.set_xalign(0.0);
            self.list.append(&hint);
        }
        for m in &msgs {
            self.add_row(app, m, pending.get(&m.id).copied());
        }
        self.update_notice(app);
        self.scroll_to_end();
        self.input.grab_focus();
    }

    /// Adds a newly sent or received message to the open conversation.
    pub fn append(&self, app: &Rc<App>, m: &Message, pending: Option<bool>) {
        let adj = self.scroller.vadjustment();
        let at_end = adj.value() + adj.page_size() >= adj.upper() - 40.0;
        self.add_row(app, m, pending);
        if at_end || m.from == app.me {
            self.scroll_to_end();
        }
    }

    /// Un-greys messages that have been delivered, and explains refusals.
    pub fn refresh_pending(&self, app: &Rc<App>) {
        let Some(conv) = self.conv.get() else { return };
        let pending = app.chat.borrow().pending_in(&conv);
        for (id, l) in self.labels.borrow().iter() {
            let p = pending.get(id);
            set_class(l.upcast_ref(), "pending", p.is_some());
            set_class(l.upcast_ref(), "refused", p == Some(&true));
        }
        self.update_notice(app);
    }

    pub fn refresh_header(&self, app: &Rc<App>) {
        let Some(conv) = self.conv.get() else { return };
        while let Some(c) = self.avatar_slot.first_child() {
            self.avatar_slot.remove(&c);
        }
        let in_call = app.room().is_some();
        match conv {
            Conv::Direct(id) => {
                let name = app.display_name(&id);
                self.avatar_slot.append(&identity::avatar(32, &name, &id));
                self.title.set_markup(&identity::tagged_markup(&name, &id));
                self.subtitle.set_label(match app.presence_of(&id) {
                    Presence::Online => "Online",
                    Presence::DoNotDisturb => "Do not disturb",
                    Presence::Offline => "Offline",
                });
                self.call_btn.set_icon_name("call-start-symbolic");
                self.call_btn.set_tooltip_text(Some("Call"));
                self.placeholder.set_label(&format!("Message @{name}"));
            }
            Conv::Group(r) => {
                let icon = gtk::Image::from_icon_name("system-users-symbolic");
                icon.set_pixel_size(24);
                icon.add_css_class("group-icon");
                self.avatar_slot.append(&icon);
                let (name, members) = app.group_summary(&r);
                self.title.set_markup(&glib::markup_escape_text(&name));
                self.subtitle.set_label(&members);
                self.call_btn.set_icon_name("call-start-symbolic");
                self.call_btn.set_tooltip_text(Some("Join the group call"));
                self.placeholder.set_label(&format!("Message {name}"));
            }
        }
        let panel = self.panel.get();
        self.call_btn.set_visible(!panel && !in_call);
        self.back_to_call.set_visible(!panel && in_call);
    }

    fn update_notice(&self, app: &Rc<App>) {
        let Some(conv) = self.conv.get() else { return };
        let pending = app.chat.borrow().pending_in(&conv);
        let refused = pending.values().any(|r| *r);
        let waiting = !pending.is_empty();
        let text = match conv {
            Conv::Direct(id) if refused => {
                Some(format!("{} hasn't added you as a contact, so they can't receive your messages yet. They'll be delivered once they do.", app.display_name(&id)))
            }
            _ if refused => Some("Some members haven't added you yet, so they can't receive your messages.".to_string()),
            _ if waiting => Some("Greyed-out messages are waiting to be delivered: they're sent when you and the recipient are both online.".to_string()),
            _ => None,
        };
        self.notice.set_visible(text.is_some());
        self.notice.set_label(&text.unwrap_or_default());
    }

    fn send(&self) {
        let (Some(a), Some(conv)) = (self.app.upgrade(), self.conv.get()) else { return };
        let buf = self.input.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.chars().count() > MAX_CHAT_CHARS {
            a.toast(&format!("Messages can be up to {MAX_CHAT_CHARS} characters"));
            return;
        }
        buf.set_text("");
        a.send_chat(conv, text);
    }

    fn scroll_to_end(&self) {
        let s = self.scroller.clone();
        glib::idle_add_local_once(move || {
            let adj = s.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });
    }

    fn add_row(&self, app: &Rc<App>, m: &Message, pending: Option<bool>) {
        let Some(conv) = self.conv.get() else { return };
        let when = glib::DateTime::from_unix_local((m.ts / 1000) as i64).ok();
        let day = when.as_ref().map(|d| d.year() * 400 + d.day_of_year()).unwrap_or(0);
        let last = self.last.get();
        if last.is_none_or(|(_, _, d)| d != day) {
            if let Some(d) = &when {
                let sep = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                sep.add_css_class("chat-day");
                let l1 = gtk::Separator::new(gtk::Orientation::Horizontal);
                l1.set_hexpand(true);
                l1.set_valign(gtk::Align::Center);
                let l2 = gtk::Separator::new(gtk::Orientation::Horizontal);
                l2.set_hexpand(true);
                l2.set_valign(gtk::Align::Center);
                let t = gtk::Label::new(Some(d.format("%B %e, %Y").map(|s| s.to_string()).unwrap_or_default().replace("  ", " ").as_str()));
                t.add_css_class("caption");
                t.add_css_class("dim-label");
                sep.append(&l1);
                sep.append(&t);
                sep.append(&l2);
                self.list.append(&sep);
            }
        }
        let continues = last.is_some_and(|(from, ts, d)| from == m.from && d == day && m.ts.saturating_sub(ts) < GROUP_MS);
        self.last.set(Some((m.from, m.ts, day)));

        let text = gtk::Label::new(None);
        text.set_markup(&linkify(&m.text));
        text.set_wrap(true);
        text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        text.set_xalign(0.0);
        text.set_selectable(true);
        text.set_can_focus(false);
        text.add_css_class("chat-text");
        if let Some(refused) = pending {
            text.add_css_class("pending");
            set_class(text.upcast_ref(), "refused", refused);
        }
        self.labels.borrow_mut().insert(m.id.clone(), text.clone());

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        row.add_css_class("chat-msg");
        if continues {
            text.set_margin_start(56);
            row.append(&text);
        } else {
            row.add_css_class("first");
            let name = app.name_in(&conv, &m.from);
            let av = identity::avatar(40, &name, &m.from);
            av.set_valign(gtk::Align::Start);
            row.append(&av);
            let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
            col.set_hexpand(true);
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let who = gtk::Label::new(None);
            who.set_markup(&format!("<b>{}</b>", identity::tagged_markup(&name, &m.from)));
            who.add_css_class("chat-author");
            let ts = gtk::Label::new(Some(&when.as_ref().map(stamp).unwrap_or_default()));
            ts.add_css_class("caption");
            ts.add_css_class("dim-label");
            ts.set_valign(gtk::Align::Baseline);
            head.append(&who);
            head.append(&ts);
            col.append(&head);
            col.append(&text);
            row.append(&col);
        }
        if let Some(d) = &when {
            row.set_tooltip_text(Some(&d.format("%A, %B %e, %Y %H:%M").map(|s| s.to_string()).unwrap_or_default()));
        }
        self.list.append(&row);
    }
}

/// "Today at 14:03", "Yesterday at 09:12", or "14/11/2026 14:03".
fn stamp(d: &glib::DateTime) -> String {
    let now = glib::DateTime::now_local().ok();
    let day = |x: &glib::DateTime| (x.year(), x.day_of_year());
    let time = d.format("%H:%M").map(|s| s.to_string()).unwrap_or_default();
    match now {
        Some(n) if day(&n) == day(d) => format!("Today at {time}"),
        Some(n) if n.add_days(-1).is_ok_and(|y| day(&y) == day(d)) => format!("Yesterday at {time}"),
        _ => d.format("%d/%m/%Y %H:%M").map(|s| s.to_string()).unwrap_or_default(),
    }
}

/// Escapes text for Pango and turns http(s) links into clickable ones.
fn linkify(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        out.push_str(&glib::markup_escape_text(&rest[..i]));
        let tail = &rest[i..];
        let mut end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        while end > 0 && tail[..end].ends_with(['.', ',', '!', '?', ')', ']', '>', '"', '\'']) {
            end -= 1;
        }
        let url = &tail[..end];
        let esc = glib::markup_escape_text(url);
        out.push_str(&format!("<a href=\"{esc}\">{esc}</a>"));
        rest = &tail[end..];
    }
    out.push_str(&glib::markup_escape_text(rest));
    out
}

fn set_class(w: &gtk::Widget, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_are_escaped_and_clickable() {
        assert_eq!(super::linkify("a <b> & c"), "a &lt;b&gt; &amp; c");
        assert_eq!(
            super::linkify("see https://example.com/x?a=1&b=2."),
            "see <a href=\"https://example.com/x?a=1&amp;b=2\">https://example.com/x?a=1&amp;b=2</a>."
        );
        assert_eq!(super::linkify("http://a and https://b"), "<a href=\"http://a\">http://a</a> and <a href=\"https://b\">https://b</a>");
    }
}
