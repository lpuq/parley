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
/// How long a message must wait undelivered before we explain why.
pub const WAIT_NOTICE_MS: u64 = 5000;

/// A place in the conversation's text: which message, and how many characters in.
type Spot = (usize, i32);

/// The word (or run of spaces or punctuation) around a character offset, as offsets.
fn word_at(text: &str, at: i32) -> (i32, i32) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let at = (at.max(0) as usize).min(chars.len() - 1);
    let kind = |c: char| if c.is_alphanumeric() || c == '_' { 0 } else if c.is_whitespace() { 1 } else { 2 };
    let k = kind(chars[at]);
    let start = chars[..at].iter().rposition(|c| kind(*c) != k).map_or(0, |p| p + 1);
    let end = chars[at..].iter().position(|c| kind(*c) != k).map_or(chars.len(), |p| at + p);
    (start as i32, end as i32)
}

/// The part of message `i` a selection from `from` to `to` covers, as character offsets.
fn range_in(labels: &[gtk::Label], i: usize, from: Spot, to: Spot) -> Option<(i32, i32)> {
    if i < from.0 || i > to.0 {
        return None;
    }
    let start = if i == from.0 { from.1 } else { 0 };
    let end = if i == to.0 { to.1 } else { labels[i].text().chars().count() as i32 };
    (start < end).then_some((start, end))
}

/// Highlights part of a message in the standard selection blue. (GTK lets only one label
/// at a time hold a real selection, so a selection across messages is drawn this way.)
fn highlight(label: &gtk::Label, range: Option<(i32, i32)>) {
    let Some((start, end)) = range else {
        label.set_attributes(None);
        return;
    };
    let text = label.text();
    let byte = |chars: i32| text.char_indices().nth(chars as usize).map_or(text.len(), |(b, _)| b) as u32;
    let (from, to) = (byte(start), byte(end));
    let attrs = gtk::pango::AttrList::new();
    let mut bg = gtk::pango::AttrColor::new_background(0x3535, 0x8484, 0xe4e4);
    bg.set_start_index(from);
    bg.set_end_index(to);
    attrs.insert(bg);
    let mut fg = gtk::pango::AttrColor::new_foreground(0xffff, 0xffff, 0xffff);
    fg.set_start_index(from);
    fg.set_end_index(to);
    attrs.insert(fg);
    label.set_attributes(Some(&attrs));
}

pub struct ChatView {
    app: Weak<App>,
    pub root: gtk::Box,
    header: gtk::Box,
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
    /// The message labels in order, for selecting across them.
    texts: RefCell<Vec<gtk::Label>>,
    /// The current selection across messages, as dragged (start, end).
    selection: Cell<Option<(Spot, Spot)>>,
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
            header: header.clone(),
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
            texts: RefCell::default(),
            selection: Cell::new(None),
        });
        Self::add_selection_drag(&this);

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
        // A group's header shows who's in it: its members are listed at the bottom of
        // the sidebar.
        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(&this);
        click.connect_released(move |g, _, x, y| {
            let Some(v) = w.upgrade() else { return };
            if !matches!(v.conv.get(), Some(Conv::Group(_))) {
                return;
            }
            // Not when it's one of the header's buttons being pressed.
            let mut hit = v.header.pick(x, y, gtk::PickFlags::DEFAULT);
            while let Some(h) = hit {
                if h.is::<gtk::Button>() {
                    return;
                }
                hit = h.parent();
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(a) = v.app.upgrade() {
                a.show_group_members();
            }
        });
        this.header.add_controller(click);
        let w = Rc::downgrade(&this);
        this.back_to_call.connect_clicked(move |_| {
            if let Some(a) = w.upgrade().and_then(|v| v.app.upgrade()) {
                a.show_call();
            }
        });
        this
    }

    /// Left-button selection for the whole conversation (each message is its own label,
    /// and a label can only select within itself): drag across messages, double-click
    /// for a word, triple-click for a message. Links, buttons and the right-click menu
    /// are left alone.
    fn add_selection_drag(this: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_PRIMARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let drag = gtk::GestureDrag::new();
        drag.set_button(gtk::gdk::BUTTON_PRIMARY);
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        drag.group_with(&click);
        // Where the selection began, and the pointer now (in the scroller, which stays
        // put while the list scrolls under it).
        let anchor: Rc<Cell<Option<Spot>>> = Rc::default();
        let pointer: Rc<Cell<(f64, f64)>> = Rc::default();
        let dragging: Rc<Cell<bool>> = Rc::default();

        let (w, a) = (Rc::downgrade(this), anchor.clone());
        click.connect_pressed(move |g, n_press, x, y| {
            let Some(v) = w.upgrade() else { return };
            if !v.owns_press(g, x, y) {
                a.set(None);
                g.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            v.clear_selection();
            let spot = v.spot_at(x, y);
            a.set(spot);
            let Some((i, at)) = spot else { return };
            let text = v.texts.borrow()[i].text();
            let (start, end) = match n_press {
                1 => return,
                2 => word_at(&text, at),
                _ => (0, text.chars().count() as i32),
            };
            a.set(Some((i, start)));
            v.select((i, start), (i, end));
        });

        let (w, a, p, d) = (Rc::downgrade(this), anchor.clone(), pointer.clone(), dragging.clone());
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(v) = w.upgrade() else { return };
            let Some((x, y)) = g.start_point() else { return };
            p.set((x + dx, y + dy));
            if !d.get() {
                if dx.hypot(dy) < 3.0 || a.get().is_none() {
                    return;
                }
                d.set(true);
                // Past the top or bottom edge, the list scrolls along.
                let (w, a, p, d) = (w.clone(), a.clone(), p.clone(), d.clone());
                glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
                    let Some(v) = w.upgrade().filter(|_| d.get()) else { return glib::ControlFlow::Break };
                    let (x, y) = p.get();
                    let h = v.scroller.height() as f64;
                    let step = if y < 24.0 { (y - 24.0).max(-60.0) } else if y > h - 24.0 { (y - h + 24.0).min(60.0) } else { 0.0 };
                    if step != 0.0 {
                        let adj = v.scroller.vadjustment();
                        adj.set_value(adj.value() + step);
                        if let (Some(from), Some(to)) = (a.get(), v.spot_at(x, y)) {
                            v.select(from, to);
                        }
                    }
                    glib::ControlFlow::Continue
                });
            }
            if let (Some(from), Some(to)) = (a.get(), v.spot_at(x + dx, y + dy)) {
                v.select(from, to);
            }
        });
        // As with any selected text on Linux, it can be middle-click pasted.
        let (d, w) = (dragging, Rc::downgrade(this));
        drag.connect_drag_end(move |_, _, _| {
            d.set(false);
            if let Some(v) = w.upgrade() {
                if let Some(text) = v.selected_text() {
                    v.root.primary_clipboard().set_text(&text);
                }
            }
        });
        this.scroller.add_controller(click);
        this.scroller.add_controller(drag);

        // Right-click on a selection: Copy. (Otherwise the labels' own menu, for links.)
        let menu = gtk::Popover::new();
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_parent(&this.scroller);
        let copy = gtk::Button::builder().label("Copy").css_classes(["flat"]).build();
        menu.set_child(Some(&copy));
        let (w, m) = (Rc::downgrade(this), menu.clone());
        copy.connect_clicked(move |_| {
            m.popdown();
            if let Some(v) = w.upgrade() {
                v.copy_selection();
            }
        });
        let right = gtk::GestureClick::new();
        right.set_button(gtk::gdk::BUTTON_SECONDARY);
        right.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(this);
        right.connect_pressed(move |g, _, x, y| {
            let Some(v) = w.upgrade() else { return };
            if v.selected_text().is_none() {
                return;
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            menu.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            menu.popup();
        });
        this.scroller.add_controller(right);
    }

    /// Whether a left press in the scroller is ours to select with: not on a scrollbar,
    /// a button or a link, and not a finger (that scrolls).
    fn owns_press(&self, g: &gtk::GestureClick, x: f64, y: f64) -> bool {
        if g.current_event_device().is_some_and(|d| d.source() == gtk::gdk::InputSource::Touchscreen) {
            return false;
        }
        let mut hit = self.scroller.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(h) = hit {
            if h == *self.scroller.upcast_ref::<gtk::Widget>() {
                break;
            }
            if h.is::<gtk::Scrollbar>() || h.is::<gtk::Button>() {
                return false;
            }
            if h.downcast_ref::<gtk::Label>().is_some_and(|l| l.current_uri().is_some()) {
                return false;
            }
            hit = h.parent();
        }
        true
    }

    /// The text position under a point in the scroller: the nearest message above or
    /// below when it's between messages.
    fn spot_at(&self, x: f64, y: f64) -> Option<Spot> {
        let labels = self.texts.borrow();
        let point = self.scroller.compute_point(&self.list, &gtk::graphene::Point::new(x as f32, y as f32))?;
        for (i, l) in labels.iter().enumerate() {
            let b = l.compute_bounds(&self.list)?;
            if point.y() > b.y() + b.height() {
                continue;
            }
            if point.y() < b.y() {
                return Some((i, 0));
            }
            let q = self.list.compute_point(l, &point)?;
            let (ox, oy) = l.layout_offsets();
            let layout = l.layout();
            let scale = gtk::pango::SCALE;
            let (_, index, trailing) = layout.xy_to_index((q.x() as i32 - ox) * scale, (q.y() as i32 - oy) * scale);
            let text = layout.text();
            let at = (index.max(0) as usize).min(text.len());
            return Some((i, text[..at].chars().count() as i32 + trailing));
        }
        let last = labels.len().checked_sub(1)?;
        Some((last, labels[last].text().chars().count() as i32))
    }

    fn select(&self, a: Spot, b: Spot) {
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        let labels = self.texts.borrow();
        let old = self.selection.get();
        // Only the messages in the old or new selection can change.
        let (first, last) = old.map_or((from.0, to.0), |(f, t)| (f.0.min(from.0), t.0.max(to.0)));
        for i in first..=last.min(labels.len().saturating_sub(1)) {
            let was = old.and_then(|(f, t)| range_in(&labels, i, f, t));
            let now = range_in(&labels, i, from, to);
            if was != now {
                highlight(&labels[i], now);
            }
        }
        self.selection.set(Some((from, to)));
    }

    fn clear_selection(&self) {
        if let Some((from, to)) = self.selection.take() {
            let labels = self.texts.borrow();
            for l in labels.iter().take(to.0 + 1).skip(from.0) {
                highlight(l, None);
            }
        }
    }

    /// What's selected, one message per line.
    fn selected_text(&self) -> Option<String> {
        let (from, to) = self.selection.get()?;
        let labels = self.texts.borrow();
        let parts: Vec<String> = (from.0..=to.0.min(labels.len().checked_sub(1)?))
            .filter_map(|i| {
                let (start, end) = range_in(&labels, i, from, to)?;
                Some(labels[i].text().chars().skip(start as usize).take((end - start) as usize).collect())
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// Ctrl+C: copies what's selected in the conversation. False if that's nothing, or
    /// the text being typed has a selection of its own.
    pub fn copy_selection(&self) -> bool {
        if !self.root.is_mapped() || (self.input.has_focus() && self.input.buffer().has_selection()) {
            return false;
        }
        let Some(text) = self.selected_text() else { return false };
        self.root.clipboard().set_text(&text);
        true
    }

    /// Ctrl+V with no text field focused: pastes into the message box.
    pub fn paste_to_composer(&self) -> bool {
        if !self.input.is_mapped() || self.conv.get().is_none() {
            return false;
        }
        let typing = self.root.root().and_then(|r| r.focus()).is_some_and(|f| f.is::<gtk::TextView>() || f.is::<gtk::Editable>());
        if typing {
            return false;
        }
        self.input.grab_focus();
        self.input.emit_paste_clipboard();
        true
    }

    pub fn conv(&self) -> Option<Conv> {
        self.conv.get()
    }

    /// No conversation open (going home).
    pub fn close(&self) {
        self.conv.set(None);
        self.input.buffer().set_text("");
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.labels.borrow_mut().clear();
        self.texts.borrow_mut().clear();
        self.selection.set(None);
        self.last.set(None);
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
        self.texts.borrow_mut().clear();
        self.selection.set(None);
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
        if at_end || m.from == app.me() {
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
        let group = matches!(conv, Conv::Group(_));
        set_class(self.header.upcast_ref(), "clickable", group);
        self.header.set_cursor_from_name(group.then_some("pointer"));
        self.header.set_tooltip_text(group.then_some("Show the group's members"));
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
                self.call_btn.set_icon_name("parley-call-start-symbolic");
                self.call_btn.set_tooltip_text(Some("Call"));
                self.placeholder.set_label(&format!("Message @{name}"));
            }
            Conv::Group(r) => {
                let icon = gtk::Image::from_icon_name("parley-system-users-symbolic");
                icon.set_pixel_size(24);
                icon.add_css_class("group-icon");
                self.avatar_slot.append(&icon);
                let (name, members) = app.group_summary(&r);
                self.title.set_markup(&glib::markup_escape_text(&name));
                self.subtitle.set_label(&members);
                self.call_btn.set_icon_name("parley-call-start-symbolic");
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
        // Only once something has really been waiting: a note that pops up for the
        // split second a normal delivery takes would make the chat jump.
        let waiting = app.chat.borrow().oldest_waiting(&conv).is_some_and(|ts| crate::chat::now_ms().saturating_sub(ts) > WAIT_NOTICE_MS);
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
        if let Some(call) = &m.call {
            // Messages after it start a fresh group.
            self.last.set(Some((m.from, 0, day)));
            self.list.append(&call_row(app, &conv, m, call, when.as_ref()));
            return;
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
        self.texts.borrow_mut().push(text.clone());

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

/// "Bob started a call that lasted 5 minutes.", like Discord.
fn call_row(app: &Rc<App>, conv: &Conv, m: &Message, call: &crate::chat::CallRecord, when: Option<&glib::DateTime>) -> gtk::Box {
    let who = if m.from == app.me() {
        "You".to_string()
    } else {
        identity::tagged_markup(&app.name_in(conv, &m.from), &m.from)
    };
    let soft = |t: &str| format!("<span alpha=\"70%\">{}</span>", glib::markup_escape_text(t));
    let (text, state) = if call.missed {
        (format!("{}<b>{who}</b>{}", soft("You missed a call from "), soft(".")), "missed")
    } else if let Some(s) = call.lasted_s {
        (format!("<b>{who}</b>{}", soft(&format!(" started a call that lasted {}.", lasted(s)))), "ended")
    } else if app.call_ongoing(&m.id) {
        (format!("<b>{who}</b>{}", soft(" started a call.")), "ongoing")
    } else {
        (format!("<b>{who}</b>{}", soft(" started a call.")), "ended")
    };
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("chat-msg");
    row.add_css_class("first");
    row.add_css_class("chat-call");
    let icon = gtk::Image::from_icon_name("parley-call-start-symbolic");
    icon.add_css_class(state);
    icon.set_margin_start(12);
    icon.set_margin_end(8);
    let label = gtk::Label::new(None);
    label.set_markup(&text);
    label.set_wrap(true);
    label.set_xalign(0.0);
    let ts = gtk::Label::new(Some(&when.map(stamp).unwrap_or_default()));
    ts.add_css_class("caption");
    ts.add_css_class("dim-label");
    row.append(&icon);
    row.append(&label);
    row.append(&ts);
    if let Some(d) = when {
        row.set_tooltip_text(Some(&d.format("%A, %B %e, %Y %H:%M").map(|s| s.to_string()).unwrap_or_default()));
    }
    row
}

/// Discord's wording: "a few seconds", "a minute", "12 minutes", "an hour", "3 hours".
fn lasted(s: u64) -> String {
    match s {
        0..=44 => "a few seconds".into(),
        45..=89 => "a minute".into(),
        90..=3599 => format!("{} minutes", (s + 30) / 60),
        3600..=5399 => "an hour".into(),
        _ => format!("{} hours", (s + 1800) / 3600),
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
    fn call_lengths_read_like_discord() {
        let l = super::lasted;
        assert_eq!([l(5), l(60), l(150), l(59 * 60), l(3700), l(3 * 3600)], ["a few seconds", "a minute", "3 minutes", "59 minutes", "an hour", "3 hours"]);
    }

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
