//! Contacts and groups list.

use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;

use super::{App, dialogs};
use super::identity;
use crate::{
    proto::{MediaState, Presence, RoomKey},
    store::{ShareCode, Status},
};

pub struct Sidebar {
    app: Weak<App>,
    root: adw::ToolbarView,
    friends: gtk::ListBox,
    groups: gtk::ListBox,
    friends_empty: gtk::Label,
    groups_empty: gtk::Label,
    me_row: adw::ActionRow,
    me_avatar: adw::Avatar,
    /// The open group's members who aren't your friends, at the bottom.
    others_title: gtk::Box,
    others_label: gtk::Label,
    others: gtk::ListBox,
    others_empty: gtk::Label,
    scroll: gtk::ScrolledWindow,
    status_dot: gtk::Box,
    status_label: gtk::Label,
}

impl Sidebar {
    pub fn new(app: Weak<App>) -> Self {
        let root = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        // The version, while Parley is young: makes "which version are you on?" easy.
        header.set_title_widget(Some(&adw::WindowTitle::new("Parley", &format!("v{}", crate::proto::APP_VERSION))));

        let menu = gtk::gio::Menu::new();
        menu.append(Some("Add friend or group from code…"), Some("app.add-contact"));
        menu.append(Some("New group…"), Some("app.new-group"));
        let sec = gtk::gio::Menu::new();
        sec.append(Some("Change my name…"), Some("app.rename-me"));
        sec.append(Some("Settings"), Some("app.settings"));
        sec.append(Some("Quit"), Some("app.quit"));
        menu.append_section(None, &sec);
        let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
        header.pack_end(&menu_btn);
        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add a friend's code")
            .action_name("app.add-contact")
            .build();
        header.pack_start(&add_btn);
        root.add_top_bar(&header);

        let me_row = adw::ActionRow::builder().title("You").subtitle("Share your code so friends can call you").build();
        let me_avatar = adw::Avatar::new(32, None, true);
        me_avatar.set_valign(gtk::Align::Center);
        me_row.add_prefix(&me_avatar);
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy my code")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .action_name("app.copy-my-code")
            .build();
        let qr = gtk::Button::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Show my QR code")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .action_name("app.show-my-qr")
            .build();
        me_row.add_suffix(&qr);
        me_row.add_suffix(&copy);
        // Your name is the way back to the home page (the buttons copy and show your code).
        me_row.set_activatable(true);
        me_row.set_tooltip_text(Some("Home"));
        me_row.set_action_name(Some("app.home"));
        let me_list = gtk::ListBox::new();
        me_list.add_css_class("boxed-list");
        me_list.set_selection_mode(gtk::SelectionMode::None);
        me_list.append(&me_row);

        // Status: Online / Do Not Disturb / Invisible.
        let status_menu = gtk::gio::Menu::new();
        status_menu.append(Some("Online"), Some("app.status::online"));
        status_menu.append(Some("Do Not Disturb — calls arrive silently"), Some("app.status::dnd"));
        status_menu.append(Some("Invisible — appear offline"), Some("app.status::invisible"));
        let status_dot = presence_dot();
        let status_label = gtk::Label::new(None);
        let status_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        status_box.append(&status_dot);
        status_box.append(&status_label);
        let status_btn = gtk::MenuButton::builder()
            .child(&status_box)
            .menu_model(&status_menu)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Your status")
            .build();
        let status_row = adw::ActionRow::builder().title("Status").build();
        status_row.add_suffix(&status_btn);
        status_row.set_activatable_widget(Some(&status_btn));
        me_list.append(&status_row);

        let friends = list();
        let groups = list();
        let friends_empty = hint("No friends yet. Press + and paste a code a friend sent you.");
        let groups_empty = hint("Groups let 3 people call together. Create one from the menu.");

        let others = list();
        let others_empty = hint("Everyone in this group is already one of your friends.");
        let others_title = section_title("", None);
        let others_label = others_title.first_child().and_downcast::<gtk::Label>().unwrap();
        others_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let new_group = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New group")
            .css_classes(["flat", "circular"])
            .action_name("app.new-group")
            .build();

        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.set_margin_start(12);
        col.set_margin_end(12);
        col.set_margin_top(6);
        col.set_margin_bottom(12);
        col.append(&me_list);
        col.append(&section_title("Friends", None));
        col.append(&friends);
        col.append(&friends_empty);
        col.append(&section_title("Groups", Some(&new_group)));
        col.append(&groups);
        col.append(&groups_empty);
        col.append(&others_title);
        col.append(&others);
        col.append(&others_empty);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&col)
            .build();
        root.set_content(Some(&scroll));

        Self {
            app,
            root,
            friends,
            groups,
            friends_empty,
            groups_empty,
            me_row,
            me_avatar,
            others_title,
            others_label,
            others,
            others_empty,
            scroll,
            status_dot,
            status_label,
        }
    }

    /// Highlights your own row while the home page is showing.
    pub fn set_home_shown(&self, on: bool) {
        if on {
            self.me_row.add_css_class("conv-open");
        } else {
            self.me_row.remove_css_class("conv-open");
        }
    }

    /// Scrolls down to the open group's members (they're at the bottom).
    pub fn scroll_to_members(&self) {
        let s = self.scroll.clone();
        glib::idle_add_local_once(move || {
            let adj = s.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });
    }

    pub fn widget(&self) -> &adw::ToolbarView {
        &self.root
    }

    pub fn refresh_soon(&self) {
        let w = self.app.clone();
        glib::idle_add_local_once(move || {
            if let Some(a) = w.upgrade() {
                a.sidebar.refresh(&a);
            }
        });
    }

    pub fn refresh(&self, app: &Rc<App>) {
        while let Some(c) = self.friends.first_child() {
            self.friends.remove(&c);
        }
        while let Some(c) = self.groups.first_child() {
            self.groups.remove(&c);
        }
        let st = app.store.borrow();
        let my_name = if st.my_name.is_empty() { "You" } else { &st.my_name };
        self.me_row.set_title(&identity::tagged_markup(my_name, &app.me));
        self.me_avatar.set_text(Some(my_name));
        self.me_avatar.set_custom_image(Some(&identity::identicon(&app.me)));
        let (cls, text) = match st.settings.status {
            Status::Online => ("online", "Online"),
            Status::DoNotDisturb => ("dnd", "Do Not Disturb"),
            Status::Invisible => ("offline", "Invisible"),
        };
        set_presence_class(&self.status_dot, cls);
        self.status_label.set_label(text);
        let room = app.room();

        let mut contacts = st.contacts.clone();
        contacts.sort_by_key(|c| c.name.to_lowercase());
        for c in contacts {
            let in_call = room == Some(RoomKey::Direct(c.id));
            let presence = app.presence_of(&c.id);
            let (cls, text) = match presence {
                Presence::Online => ("online", "Online"),
                Presence::DoNotDisturb => ("dnd", "Do not disturb"),
                Presence::Offline => ("offline", "Offline"),
            };
            let row = adw::ActionRow::builder()
                .title(identity::tagged_markup(&c.name, &c.id))
                .title_lines(1)
                .subtitle(if in_call { "In call" } else { text })
                .build();
            let avatar = gtk::Overlay::new();
            avatar.set_child(Some(&identity::avatar(32, &c.name, &c.id)));
            let dot = presence_dot();
            set_presence_class(&dot, if in_call { "online" } else { cls });
            dot.set_halign(gtk::Align::End);
            dot.set_valign(gtk::Align::End);
            avatar.add_overlay(&dot);
            avatar.set_valign(gtk::Align::Center);
            row.add_prefix(&avatar);
            let call = gtk::Button::builder()
                .icon_name("call-start-symbolic")
                .tooltip_text(format!("Call {}", c.name))
                .valign(gtk::Align::Center)
                .css_classes(["circular", "call-row-btn"])
                .sensitive(room.is_none())
                .build();
            let (a, id) = (self.app.clone(), c.id);
            call.connect_clicked(move |_| {
                if let Some(a) = a.upgrade() {
                    a.call_contact(id);
                }
            });
            let conv = crate::chat::Conv::Direct(c.id);
            // Unread count on the avatar's corner, so names keep their room.
            if let Some(b) = unread_badge(app.chat.borrow().unread(&conv)) {
                b.set_halign(gtk::Align::End);
                b.set_valign(gtk::Align::Start);
                avatar.add_overlay(&b);
            }
            row.add_suffix(&self.chat_button(conv, &format!("Message {}", c.name)));
            row.add_suffix(&call);
            row.add_suffix(&self.contact_menu(c.id, c.name.clone()));
            // Clicking the row opens the chat (the green button calls).
            row.set_activatable(true);
            let a = self.app.clone();
            row.connect_activated(move |_| {
                if let Some(a) = a.upgrade() {
                    a.open_chat(conv);
                }
            });
            if app.chat_open() == Some(conv) {
                row.add_css_class("conv-open");
            }
            self.friends.append(&row);
        }

        let mut groups = st.groups.clone();
        groups.sort_by_key(|g| g.name.to_lowercase());
        for g in groups {
            let rid = g.room_id();
            let in_call = room == Some(RoomKey::Group(rid));
            let others: Vec<String> = g
                .members
                .iter()
                .filter(|m| m.id != app.me)
                .map(|m| st.name_for(&m.id).unwrap_or_else(|| m.name.clone()))
                .collect();
            let in_room = app.group_call_members(&rid);
            let subtitle = if in_call {
                "In call".to_string()
            } else if !in_room.is_empty() {
                "Call in progress — press Join".to_string()
            } else if others.is_empty() {
                "Share the group code to invite people".to_string()
            } else {
                others.join(", ")
            };
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&g.name))
                .title_lines(1)
                .subtitle(glib::markup_escape_text(&subtitle))
                .build();
            let icon = gtk::Image::from_icon_name("system-users-symbolic");
            icon.set_pixel_size(20);
            icon.add_css_class("group-icon");
            let group_icon = gtk::Overlay::new();
            group_icon.set_child(Some(&icon));
            group_icon.set_valign(gtk::Align::Center);
            row.add_prefix(&group_icon);
            let join = gtk::Button::builder()
                .label(if in_call { "Joined" } else { "Join" })
                .valign(gtk::Align::Center)
                .css_classes(["pill", "join-btn"])
                .sensitive(room.is_none())
                .build();
            let a = self.app.clone();
            join.connect_clicked(move |_| {
                if let Some(a) = a.upgrade() {
                    a.join_group(rid);
                }
            });
            let conv = crate::chat::Conv::Group(rid);
            if let Some(b) = unread_badge(app.chat.borrow().unread(&conv)) {
                b.set_halign(gtk::Align::End);
                b.set_valign(gtk::Align::Start);
                group_icon.add_overlay(&b);
            }
            row.add_suffix(&self.chat_button(conv, &format!("Message {}", g.name)));
            row.add_suffix(&join);
            row.add_suffix(&self.group_menu(rid, g.name.clone(), ShareCode::for_group(&g).encode()));
            row.set_activatable(true);
            let a = self.app.clone();
            row.connect_activated(move |_| {
                if let Some(a) = a.upgrade() {
                    a.open_chat(conv);
                }
            });
            if app.chat_open() == Some(conv) {
                row.add_css_class("conv-open");
            }
            self.groups.append(&row);
            // Who's in the call (Discord-style): avatar, name, LIVE while sharing,
            // and mute/deafen icons.
            for (id, ms) in in_room {
                let name = if id == app.me {
                    if st.my_name.is_empty() { "You".to_string() } else { st.my_name.clone() }
                } else {
                    st.name_for(&id).unwrap_or_else(|| {
                        g.members.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_else(|| id.fmt_short().to_string())
                    })
                };
                self.groups.append(&call_member_row(&name, &id, &ms));
            }
        }
        // The open group chat's members who aren't your friends, so everyone in it can
        // be seen (with their status) somewhere.
        while let Some(c) = self.others.first_child() {
            self.others.remove(&c);
        }
        let open_group = match app.chat_open() {
            Some(crate::chat::Conv::Group(r)) => st.groups.iter().find(|g| g.room_id() == r).cloned(),
            _ => None,
        };
        let mut n_others = 0;
        if let Some(g) = &open_group {
            self.others_label.set_label(&format!("Others in {}", g.name));
            let in_room = app.group_call_members(&g.room_id());
            let mut members: Vec<_> = g.members.iter().filter(|m| m.id != app.me && st.contact(&m.id).is_none()).cloned().collect();
            // Online first, then by name.
            members.sort_by_key(|m| (app.presence_of(&m.id) == Presence::Offline, m.name.to_lowercase()));
            for m in members {
                n_others += 1;
                let in_call = in_room.iter().any(|(id, _)| *id == m.id);
                self.others.append(&self.other_member_row(&m.id, &m.name, app.presence_of(&m.id), in_call));
            }
        }
        self.others_title.set_visible(open_group.is_some());
        self.others.set_visible(n_others > 0);
        self.others_empty.set_visible(open_group.is_some() && n_others == 0);

        self.friends_empty.set_visible(st.contacts.is_empty());
        self.friends.set_visible(!st.contacts.is_empty());
        self.groups_empty.set_visible(st.groups.is_empty());
        self.groups.set_visible(!st.groups.is_empty());
    }

    /// Someone in the open group who isn't your friend: status, and a button to add them.
    fn other_member_row(&self, id: &iroh::EndpointId, name: &str, presence: Presence, in_call: bool) -> adw::ActionRow {
        let (cls, text) = match presence {
            _ if in_call => ("online", "In the group call"),
            Presence::Online => ("online", "Online"),
            Presence::DoNotDisturb => ("dnd", "Do not disturb"),
            Presence::Offline => ("offline", "Offline"),
        };
        let row = adw::ActionRow::builder()
            .title(identity::tagged_markup(name, id))
            .title_lines(1)
            .subtitle(text)
            .tooltip_text("User info")
            .build();
        let avatar = gtk::Overlay::new();
        avatar.set_child(Some(&identity::avatar(32, name, id)));
        let dot = presence_dot();
        set_presence_class(&dot, cls);
        dot.set_halign(gtk::Align::End);
        dot.set_valign(gtk::Align::End);
        avatar.add_overlay(&dot);
        avatar.set_valign(gtk::Align::Center);
        row.add_prefix(&avatar);
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(format!("Add {name} as a friend"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let (a, id, n) = (self.app.clone(), *id, name.to_string());
        add.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.add_contact(id, &n);
            }
        });
        row.add_suffix(&add);
        row.set_activatable(true);
        let a = self.app.clone();
        row.connect_activated(move |_| {
            if let Some(a) = a.upgrade() {
                dialogs::user_info(&a, id);
            }
        });
        row
    }

    /// Opens the conversation (no call needed).
    fn chat_button(&self, conv: crate::chat::Conv, tip: &str) -> gtk::Button {
        let b = gtk::Button::builder()
            .icon_name("chat-message-new-symbolic")
            .tooltip_text(tip)
            .valign(gtk::Align::Center)
            .css_classes(["circular", "chat-row-btn"])
            .build();
        let a = self.app.clone();
        b.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.open_chat(conv);
            }
        });
        b
    }

    fn contact_menu(&self, id: iroh::EndpointId, name: String) -> gtk::MenuButton {
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let info = flat_button("User info");
        let rename = flat_button("Rename…");
        let remove = flat_button("Remove");
        remove.add_css_class("error");
        b.append(&info);
        b.append(&rename);
        b.append(&remove);
        pop.set_child(Some(&b));
        let (a, p) = (self.app.clone(), pop.clone());
        info.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                dialogs::user_info(&a, id);
            }
        });
        let (a, n, p) = (self.app.clone(), name.clone(), pop.clone());
        rename.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                dialogs::rename(&a, &n, move |a, new| {
                    if let Some(c) = a.store.borrow_mut().contacts.iter_mut().find(|c| c.id == id) {
                        c.name = new;
                    }
                });
            }
        });
        let (a, p) = (self.app.clone(), pop.clone());
        remove.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                a.store.borrow_mut().contacts.retain(|c| c.id != id);
                a.save();
                a.sidebar.refresh(&a);
                a.update_home();
                a.toast(&format!("Removed {name}"));
            }
        });
        gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .popover(&pop)
            .build()
    }

    fn group_menu(&self, room: [u8; 32], name: String, code: String) -> gtk::MenuButton {
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let share = flat_button("Copy invite code");
        let share_qr = flat_button("Show invite QR code");
        let rename = flat_button("Rename…");
        let leave = flat_button("Leave group");
        leave.add_css_class("error");
        b.append(&share);
        b.append(&share_qr);
        b.append(&rename);
        b.append(&leave);
        pop.set_child(Some(&b));
        let (a, p, c) = (self.app.clone(), pop.clone(), code.clone());
        share.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                a.copy(&c, "Group code");
            }
        });
        let (a, p, n) = (self.app.clone(), pop.clone(), name.clone());
        share_qr.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                super::qr::show(&a, &format!("Join “{n}”"), "Scan with Parley (+ → Scan with camera) to join this group.", &code);
            }
        });
        let (a, n, p) = (self.app.clone(), name.clone(), pop.clone());
        rename.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                dialogs::rename(&a, &n, move |a, new| {
                    if let Some(g) = a.store.borrow_mut().group_by_room_mut(&room) {
                        g.name = new;
                    }
                });
            }
        });
        let (a, p) = (self.app.clone(), pop.clone());
        leave.connect_clicked(move |_| {
            p.popdown();
            if let Some(a) = a.upgrade() {
                a.store.borrow_mut().groups.retain(|g| g.room_id() != room);
                a.save();
                a.sidebar.refresh(&a);
                a.toast(&format!("Left {name}"));
            }
        });
        gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .popover(&pop)
            .build()
    }
}

fn list() -> gtk::ListBox {
    let l = gtk::ListBox::new();
    l.add_css_class("boxed-list");
    l.set_selection_mode(gtk::SelectionMode::None);
    l
}

fn hint(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.set_margin_start(6);
    l
}

fn section_title(text: &str, extra: Option<&gtk::Button>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.set_margin_top(14);
    let l = gtk::Label::new(Some(text));
    l.add_css_class("heading");
    l.set_xalign(0.0);
    l.set_hexpand(true);
    l.set_margin_start(6);
    b.append(&l);
    if let Some(e) = extra {
        b.append(e);
    }
    b
}

fn flat_button(label: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.add_css_class("flat");
    if let Some(l) = b.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    b
}

/// A small round status indicator (green / red / grey).
fn presence_dot() -> gtk::Box {
    let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    d.add_css_class("presence-dot");
    d.set_valign(gtk::Align::Center);
    d
}

fn set_presence_class(d: &gtk::Box, cls: &str) {
    for c in ["online", "dnd", "offline"] {
        d.remove_css_class(c);
    }
    d.add_css_class(cls);
}

/// One person in a group call, shown under the group.
fn call_member_row(name: &str, id: &iroh::EndpointId, ms: &MediaState) -> gtk::ListBoxRow {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.set_margin_start(36);
    b.set_margin_end(12);
    b.set_margin_top(4);
    b.set_margin_bottom(4);
    b.append(&identity::avatar(24, name, id));
    let l = gtk::Label::new(None);
    l.set_markup(&identity::tagged_markup(name, id));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    b.append(&l);
    if ms.screen {
        let live = gtk::Label::new(Some("LIVE"));
        live.add_css_class("live-badge");
        live.set_valign(gtk::Align::Center);
        b.append(&live);
    }
    if ms.mic_muted && !ms.deafened {
        let i = gtk::Image::from_icon_name("microphone-disabled-symbolic");
        i.add_css_class("call-member-off");
        i.set_tooltip_text(Some("Muted"));
        b.append(&i);
    }
    if ms.deafened {
        let i = gtk::Image::from_icon_name("audio-volume-muted-symbolic");
        i.add_css_class("call-member-off");
        i.set_tooltip_text(Some("Deafened"));
        b.append(&i);
    }
    let row = gtk::ListBoxRow::builder().child(&b).activatable(false).selectable(false).build();
    row.add_css_class("call-member");
    row
}

/// A red unread-count pill, if there's anything unread.
fn unread_badge(n: u32) -> Option<gtk::Label> {
    (n > 0).then(|| {
        let l = gtk::Label::new(Some(&if n > 99 { "99+".to_string() } else { n.to_string() }));
        l.add_css_class("unread-badge");
        l.set_valign(gtk::Align::Center);
        l
    })
}
