//! Dialogs: name, add code, new group, rename, screen share options, settings.

use std::{cell::RefCell, rc::Rc};

use adw::prelude::*;
use gtk::glib;

use super::App;
use crate::{
    media::{appaudio::AudioShare, devices},
    store::{Group, Member, SCREEN_FPS, SCREEN_HEIGHTS, ShareCode, Suppression, VoiceMode, screen_kbps},
};

pub fn ask_name(app: &Rc<App>, first_run: bool) {
    let current = app.store.borrow().my_name.clone();
    let suggestion = if current.is_empty() {
        let real = glib::real_name().to_string_lossy().to_string();
        if real.is_empty() || real == "Unknown" { glib::user_name().to_string_lossy().to_string() } else { real }
    } else {
        current
    };
    let (heading, body) = if first_run {
        ("Welcome to Parley", "Private calls straight between you and your friends — no accounts, no servers to run.\n\nWhat should friends see as your name?")
    } else {
        ("Your name", "Friends who add you later will see this name.")
    };
    let d = adw::AlertDialog::new(Some(heading), Some(body));
    let entry = gtk::Entry::builder().text(&suggestion).activates_default(true).build();
    d.set_extra_child(Some(&entry));
    d.add_response("ok", "Continue");
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("ok"));
    if !first_run {
        d.add_response("cancel", "Cancel");
        d.set_close_response("cancel");
    } else {
        d.set_can_close(false);
    }
    let a = app.clone();
    d.connect_response(None, move |dlg, resp| {
        if resp != "ok" {
            return;
        }
        // The first-run dialog is non-dismissable (can-close = false), which also
        // stops it closing after a response, so close it explicitly.
        dlg.force_close();
        let name = entry.text().trim().to_string();
        let name = if name.is_empty() { "Friend".to_string() } else { name };
        a.store.borrow_mut().my_name = name;
        a.save();
        a.sidebar.refresh(&a);
        a.update_home();
    });
    d.present(Some(&app.window));
}

/// Paste a friend or group code.
pub fn add_code(app: &Rc<App>, prefill: Option<String>) {
    let d = adw::AlertDialog::new(
        Some("Add a friend or group"),
        Some("Paste the code someone sent you. It starts with “parley:”."),
    );
    let entry = gtk::Entry::builder().placeholder_text("parley:…").activates_default(true).build();
    let clip_text = prefill.clone();
    if let Some(t) = prefill {
        entry.set_text(&t);
    } else {
        // Paste from the clipboard automatically when it looks like a code.
        let e = entry.clone();
        app.window.clipboard().read_text_async(None::<&gtk::gio::Cancellable>, move |r| {
            if let Ok(Some(t)) = r {
                if t.contains("parley:") && e.text().is_empty() {
                    e.set_text(t.trim());
                }
            }
        });
    }
    let _ = clip_text;
    let name_row = gtk::Entry::builder().placeholder_text("Name (optional)").activates_default(true).build();
    let err = gtk::Label::new(None);
    err.add_css_class("error");
    err.set_wrap(true);
    err.set_visible(false);
    let scan = gtk::Button::builder()
        .label("Scan QR code with camera…")
        .css_classes(["pill"])
        .halign(gtk::Align::Center)
        .build();
    let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
    b.append(&entry);
    b.append(&name_row);
    b.append(&err);
    b.append(&scan);
    d.set_extra_child(Some(&b));
    let (a, e) = (app.clone(), entry.clone());
    scan.connect_clicked(move |_| {
        let e = e.clone();
        super::qr::scan(&a, move |text| e.set_text(text.trim()));
    });
    d.add_responses(&[("cancel", "Cancel"), ("add", "Add")]);
    d.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("add"));
    d.set_close_response("cancel");
    d.set_response_enabled("add", false);

    let update = {
        let (d, name_row, err) = (d.clone(), name_row.clone(), err.clone());
        move |text: &str| {
            if text.trim().is_empty() {
                d.set_response_enabled("add", false);
                err.set_visible(false);
                return;
            }
            match ShareCode::decode(text) {
                Ok(ShareCode::Contact { n, .. }) => {
                    if name_row.text().is_empty() {
                        name_row.set_text(&n);
                    }
                    err.set_visible(false);
                    d.set_response_enabled("add", true);
                }
                Ok(ShareCode::Group { n, .. }) => {
                    if name_row.text().is_empty() {
                        name_row.set_text(&n);
                    }
                    err.set_visible(false);
                    d.set_response_enabled("add", true);
                }
                Err(e) => {
                    err.set_label(&e.to_string());
                    err.set_visible(true);
                    d.set_response_enabled("add", false);
                }
            }
        }
    };
    update(&entry.text());
    let u2 = update.clone();
    entry.connect_changed(move |e| u2(&e.text()));

    let a = app.clone();
    d.connect_response(None, move |_, resp| {
        if resp != "add" {
            return;
        }
        let name = name_row.text().trim().to_string();
        match ShareCode::decode(&entry.text()) {
            Ok(ShareCode::Contact { n, k }) => {
                a.add_contact(k, if name.is_empty() { &n } else { &name });
            }
            Ok(ShareCode::Group { n, s, m, o, r }) => {
                let mut g = Group::learned(if name.is_empty() { n } else { name }, s, m, o, &r);
                let me = a.store.borrow().my_name.clone();
                g.add_member(a.me(), &me);
                let gname = g.name.clone();
                let room = g.room_id();
                a.add_group(g);
                let toast = adw::Toast::new(&glib::markup_escape_text(&format!("Joined group “{gname}”.")));
                toast.set_button_label(Some("Join call"));
                let a2 = a.clone();
                toast.connect_button_clicked(move |_| a2.join_group(room));
                a.toasts.add_toast(toast);
            }
            Err(_) => {}
        }
    });
    d.present(Some(&app.window));
}

pub fn new_group(app: &Rc<App>) {
    let d = adw::AlertDialog::new(
        Some("New group"),
        Some("Pick friends to include. They'll be able to join straight away; for anyone else, share the group's invite code."),
    );
    let name = gtk::Entry::builder().placeholder_text("Group name").activates_default(true).build();
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let contacts = app.store.borrow().contacts.clone();
    let checks: Rc<RefCell<Vec<(gtk::CheckButton, Member)>>> = Rc::default();
    for c in &contacts {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).build();
        let cb = gtk::CheckButton::new();
        row.add_prefix(&cb);
        row.set_activatable_widget(Some(&cb));
        list.append(&row);
        checks.borrow_mut().push((cb, Member { id: c.id, name: c.name.clone() }));
    }
    let b = gtk::Box::new(gtk::Orientation::Vertical, 10);
    b.append(&name);
    if contacts.is_empty() {
        let l = gtk::Label::new(Some("You have no friends added yet — you can still create the group and share its code."));
        l.set_wrap(true);
        l.add_css_class("dim-label");
        b.append(&l);
    } else {
        let sc = gtk::ScrolledWindow::builder()
            .child(&list)
            .max_content_height(260)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        b.append(&sc);
    }
    d.set_extra_child(Some(&b));
    d.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
    d.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("create"));
    d.set_close_response("cancel");
    let a = app.clone();
    d.connect_response(None, move |_, resp| {
        if resp != "create" {
            return;
        }
        let n = name.text().trim().to_string();
        let n = if n.is_empty() { "Group".to_string() } else { n };
        let my_name = a.store.borrow().my_name.clone();
        let mut members = vec![Member { id: a.me(), name: my_name }];
        for (cb, m) in checks.borrow().iter() {
            if cb.is_active() {
                members.push(m.clone());
            }
        }
        let g = Group::new(n.clone(), members);
        let code = ShareCode::for_group(&g).encode();
        let room = g.room_id();
        a.add_group(g);
        // Offer the code right away.
        let done = adw::AlertDialog::new(
            Some(&format!("“{n}” is ready")),
            Some("Press Join to call everyone in it. To invite someone who isn't in your contacts, send them the invite code."),
        );
        done.add_responses(&[("copy", "Copy invite code"), ("join", "Join now")]);
        done.set_response_appearance("join", adw::ResponseAppearance::Suggested);
        done.set_close_response("copy");
        let a2 = a.clone();
        done.connect_response(None, move |_, r| {
            if r == "join" {
                a2.join_group(room);
            } else {
                a2.copy(&code, "Group code");
            }
        });
        done.present(Some(&a.window));
    });
    d.present(Some(&app.window));
}

pub fn rename(app: &Rc<App>, current: &str, apply: impl Fn(&Rc<App>, String) + 'static) {
    let d = adw::AlertDialog::new(Some("Rename"), None);
    let entry = gtk::Entry::builder().text(current).activates_default(true).build();
    d.set_extra_child(Some(&entry));
    d.add_responses(&[("cancel", "Cancel"), ("ok", "Rename")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("ok"));
    d.set_close_response("cancel");
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        let new = entry.text().trim().to_string();
        if r == "ok" && !new.is_empty() {
            apply(&a, new);
            a.save();
            a.sidebar.refresh(&a);
        }
    });
    d.present(Some(&app.window));
}

/// Screen share options, then the system's screen/window picker.
pub fn screen_share(app: &Rc<App>) {
    let d = adw::Dialog::new();
    d.set_title("Share your screen");
    d.set_content_width(440);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    let page = adw::PreferencesPage::new();

    let q = adw::PreferencesGroup::new();
    q.set_title("Stream quality");
    let (cur_h, cur_fps) = {
        let st = &app.store.borrow().settings;
        (st.screen_height, st.screen_fps)
    };
    let res_labels: Vec<String> = SCREEN_HEIGHTS.iter().map(|h| format!("{h}p")).collect();
    let res_labels: Vec<&str> = res_labels.iter().map(String::as_str).collect();
    let qrow = adw::ComboRow::builder().title("Resolution").model(&gtk::StringList::new(&res_labels)).build();
    qrow.set_selected(SCREEN_HEIGHTS.iter().position(|h| *h == cur_h).unwrap_or(2) as u32);
    let mut fps_labels: Vec<String> = SCREEN_FPS.iter().map(|f| format!("{f} fps")).collect();
    fps_labels.push("Custom".into());
    let fps_labels: Vec<&str> = fps_labels.iter().map(String::as_str).collect();
    let frow = adw::ComboRow::builder().title("Frame rate").model(&gtk::StringList::new(&fps_labels)).build();
    frow.set_selected(SCREEN_FPS.iter().position(|f| *f == cur_fps).unwrap_or(SCREEN_FPS.len()) as u32);
    let custom = adw::SpinRow::with_range(5.0, 120.0, 1.0);
    custom.set_title("Custom frame rate");
    custom.set_value(cur_fps as f64);
    // The chosen frame rate: a preset, or the custom value.
    let fps_of = {
        let (frow, custom) = (frow.clone(), custom.clone());
        move || SCREEN_FPS.get(frow.selected() as usize).copied().unwrap_or(custom.value() as u32)
    };
    let rate = {
        let (qrow, frow, custom, fps_of) = (qrow.clone(), frow.clone(), custom.clone(), fps_of.clone());
        move || {
            custom.set_visible(frow.selected() as usize == SCREEN_FPS.len());
            let h = SCREEN_HEIGHTS[qrow.selected() as usize];
            frow.set_subtitle(&format!(
                "Up to {:.1} Mbps; lowered automatically if the connection can't keep up",
                screen_kbps(h, fps_of()) as f64 / 1000.0
            ));
        }
    };
    rate();
    {
        let r = rate.clone();
        qrow.connect_selected_notify(move |_| r());
        let r = rate.clone();
        frow.connect_selected_notify(move |_| r());
        let r = rate.clone();
        custom.connect_value_notify(move |_| r());
    }
    q.add(&qrow);
    q.add(&frow);
    q.add(&custom);
    page.add(&q);

    let g = adw::PreferencesGroup::new();
    g.set_title("Sound");
    g.set_description(Some("Parley's own audio (your friends' voices) is never included, so nobody hears an echo."));
    let mode = adw::ComboRow::builder().title("Share audio").build();
    mode.set_model(Some(&gtk::StringList::new(&["No audio", "All desktop audio", "Only selected apps"])));
    let apps = app.playing_apps.borrow().clone();
    let prev = app.share_audio.borrow().clone();
    mode.set_selected(match prev {
        AudioShare::Off => 0,
        AudioShare::Desktop => 1,
        AudioShare::Apps(_) => 2,
    });
    if matches!(prev, AudioShare::Off) {
        mode.set_selected(1);
    }
    g.add(&mode);
    let apps_group = adw::PreferencesGroup::new();
    apps_group.set_title("Apps playing sound");
    let switches: Rc<RefCell<Vec<(adw::SwitchRow, String)>>> = Rc::default();
    if apps.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No apps are playing sound right now")
            .subtitle("Start playback in the app first, then reopen this dialog.")
            .build();
        apps_group.add(&row);
    }
    let prev_apps = if let AudioShare::Apps(v) = &prev { v.clone() } else { vec![] };
    for a in &apps {
        let row = adw::SwitchRow::builder().title(glib::markup_escape_text(a)).active(prev_apps.contains(a)).build();
        apps_group.add(&row);
        switches.borrow_mut().push((row, a.clone()));
    }
    apps_group.set_visible(mode.selected() == 2);
    let ag = apps_group.clone();
    mode.connect_selected_notify(move |m| ag.set_visible(m.selected() == 2));
    page.add(&g);
    page.add(&apps_group);

    let sharing = app.state.get().screen;
    if sharing {
        d.set_title("Screen share");
        q.set_sensitive(false);
        q.set_description(Some("Stop and start sharing again to change quality."));
    }
    let go = gtk::Button::builder()
        .label(if sharing { "Update sound" } else { "Choose screen or window…" })
        .css_classes(["pill", "suggested-action"])
        .build();
    let stop = gtk::Button::builder().label("Stop sharing").css_classes(["pill", "destructive-action"]).build();
    stop.set_visible(sharing);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(6);
    buttons.set_margin_bottom(18);
    buttons.append(&stop);
    buttons.append(&go);
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bx.append(&page);
    bx.append(&buttons);
    let (a, d3) = (app.clone(), d.clone());
    stop.connect_clicked(move |_| {
        d3.close();
        a.stop_screen_share();
    });
    tv.set_content(Some(&bx));
    d.set_child(Some(&tv));

    let (a, d2) = (app.clone(), d.clone());
    go.connect_clicked(move |_| {
        {
            let st = &mut a.store.borrow_mut().settings;
            st.screen_height = SCREEN_HEIGHTS[qrow.selected() as usize];
            st.screen_fps = fps_of();
        }
        a.save();
        let audio = match mode.selected() {
            0 => AudioShare::Off,
            1 => AudioShare::Desktop,
            _ => AudioShare::Apps(
                switches.borrow().iter().filter(|(r, _)| r.is_active()).map(|(_, n)| n.clone()).collect(),
            ),
        };
        let audio = match audio {
            AudioShare::Apps(v) if v.is_empty() => AudioShare::Off,
            x => x,
        };
        d2.close();
        if a.state.get().screen {
            a.set_share_audio(audio);
        } else {
            a.start_screen_share(audio);
        }
    });
    d.present(Some(&app.window));
}

pub fn settings(app: &Rc<App>) {
    let d = adw::PreferencesDialog::new();
    d.set_search_enabled(false);
    let s = app.store.borrow().settings.clone();

    // ----- Voice & video -----
    let page = adw::PreferencesPage::new();
    page.set_title("Voice & Video");
    page.set_icon_name(Some("parley-audio-input-microphone-symbolic"));

    let dev = adw::PreferencesGroup::new();
    dev.set_title("Devices");
    let mics = devices::microphones();
    let spks = devices::speakers();
    let cams = devices::cameras();
    let mic_row = device_combo("Microphone", &mics, &s.mic);
    let spk_row = device_combo("Speakers / headphones", &spks, &s.speaker);
    let cam_row = device_combo("Camera", &cams, &s.camera);
    dev.add(&mic_row);
    dev.add(&spk_row);
    dev.add(&cam_row);
    let bt_mic = adw::SwitchRow::builder()
        .title("Use Bluetooth headset microphones")
        .subtitle("Off: with “System default”, earbuds stay in high-quality mode and another mic is used. On: their mic is used, which switches them to low-quality headset mode.")
        .active(s.allow_bluetooth_mic)
        .build();
    dev.add(&bt_mic);
    page.add(&dev);
    let a = app.clone();
    bt_mic.connect_active_notify(move |r| {
        a.store.borrow_mut().settings.allow_bluetooth_mic = r.is_active();
        a.save();
        a.restart_mic();
    });

    let proc_ = adw::PreferencesGroup::new();
    proc_.set_title("Voice processing");
    let modes = gtk::StringList::new(&["Voice Isolation", "Studio", "Custom"]);
    let mode_row = adw::ComboRow::builder().title("Input profile").model(&modes).build();
    mode_row.set_selected(match s.voice_mode {
        VoiceMode::Isolation => 0,
        VoiceMode::Studio => 1,
        VoiceMode::Custom => 2,
    });
    let mode_hint = |m: VoiceMode| match m {
        VoiceMode::Isolation => "Only your voice: removes keyboards, desk knocks, chewing and other noise, and evens out your volume",
        VoiceMode::Studio => "Your microphone exactly as it is, with no processing",
        VoiceMode::Custom => "Choose the suppression, sensitivity and gain yourself",
    };
    mode_row.set_subtitle(mode_hint(s.voice_mode));

    let supp = gtk::StringList::new(&["Enhanced", "Standard", "None"]);
    let supp_row = adw::ComboRow::builder()
        .title("Noise suppression")
        .subtitle("Enhanced removes non-speech sounds; Standard only steady background hiss")
        .model(&supp)
        .build();
    supp_row.set_selected(match s.suppression {
        Suppression::Enhanced => 0,
        Suppression::Standard => 1,
        Suppression::Off => 2,
    });
    let sens_auto = adw::SwitchRow::builder()
        .title("Automatically determine input sensitivity")
        .subtitle("Transmit only while you're speaking")
        .active(s.sensitivity_auto)
        .build();
    let sens_row = adw::ActionRow::builder().title("Input sensitivity").build();
    let sens = gtk::Scale::with_range(gtk::Orientation::Horizontal, -100.0, 0.0, 1.0);
    sens.set_value(s.sensitivity_db as f64);
    sens.set_hexpand(true);
    sens.set_width_request(200);
    sens.set_draw_value(true);
    sens.set_value_pos(gtk::PositionType::Right);
    sens.set_format_value_func(|_, v| format!("{v:.0} dB"));
    sens.set_valign(gtk::Align::Center);
    sens_row.add_suffix(&sens);
    let agc = adw::SwitchRow::builder()
        .title("Automatic gain control")
        .subtitle("Keeps your voice at a steady, comfortable level, even when you lean back")
        .active(s.auto_gain)
        .build();
    let echo = adw::SwitchRow::builder().title("Echo cancellation").subtitle("Needed when using speakers instead of headphones").active(s.echo_cancel).build();

    // Live level: green while your voice is being sent.
    let meter_row = adw::ActionRow::builder().title("Mic level").subtitle("Speak to test").build();
    let meter = gtk::LevelBar::for_interval(0.0, 1.0);
    meter.set_mode(gtk::LevelBarMode::Continuous);
    meter.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_LOW));
    meter.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
    meter.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_FULL));
    meter.set_hexpand(true);
    meter.set_width_request(200);
    meter.set_valign(gtk::Align::Center);
    meter.add_css_class("voice-meter");
    meter_row.add_suffix(&meter);

    let in_call = app.room().is_some();
    let play_row = adw::SwitchRow::builder()
        .title("Hear yourself")
        .subtitle(if in_call {
            "Available outside calls"
        } else {
            "Plays your mic back a second later, exactly as friends would hear it. Use headphones."
        })
        .sensitive(!in_call)
        .build();
    {
        let a = app.clone();
        play_row.connect_active_notify(move |r| a.set_mic_playback(r.is_active()));
    }
    for r in [mode_row.upcast_ref::<gtk::Widget>(), meter_row.upcast_ref(), play_row.upcast_ref(), supp_row.upcast_ref(), sens_auto.upcast_ref(), sens_row.upcast_ref(), agc.upcast_ref(), echo.upcast_ref()] {
        proc_.add(r);
    }
    page.add(&proc_);

    // Custom-only rows.
    let sync_rows = {
        let (supp_row, sens_auto, sens_row, agc, mode_row) = (supp_row.clone(), sens_auto.clone(), sens_row.clone(), agc.clone(), mode_row.clone());
        move || {
            let custom = mode_row.selected() == 2;
            supp_row.set_visible(custom);
            sens_auto.set_visible(custom);
            agc.set_visible(custom);
            sens_row.set_visible(custom && !sens_auto.is_active());
        }
    };
    sync_rows();

    // Meter: poll the shared level ~20 times a second while the dialog is open.
    app.start_mic_test();
    {
        let (a, weak) = (app.clone(), meter.downgrade());
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let Some(m) = weak.upgrade() else { return glib::ControlFlow::Break };
            let db = a.voice.level_db();
            m.set_value(((db + 100.0) / 100.0).clamp(0.0, 1.0) as f64);
            let open = a.voice.open.load(std::sync::atomic::Ordering::Relaxed);
            if open {
                m.add_css_class("open");
            } else {
                m.remove_css_class("open");
            }
            glib::ControlFlow::Continue
        });
        let a = app.clone();
        d.connect_closed(move |_| {
            a.mic_playback.set(false);
            a.stop_mic_test();
        });
    }

    {
        let (a, sync1, mode_row2) = (app.clone(), sync_rows.clone(), mode_row.clone());
        mode_row.connect_selected_notify(move |r| {
            let m = match r.selected() {
                0 => VoiceMode::Isolation,
                1 => VoiceMode::Studio,
                _ => VoiceMode::Custom,
            };
            mode_row2.set_subtitle(mode_hint(m));
            a.update_voice(|st| st.voice_mode = m);
            sync1();
        });
        let a = app.clone();
        supp_row.connect_selected_notify(move |r| {
            let v = match r.selected() {
                0 => Suppression::Enhanced,
                1 => Suppression::Standard,
                _ => Suppression::Off,
            };
            a.update_voice(|st| st.suppression = v);
        });
        let (a, sync_rows) = (app.clone(), sync_rows.clone());
        sens_auto.connect_active_notify(move |r| {
            let on = r.is_active();
            a.update_voice(|st| st.sensitivity_auto = on);
            sync_rows();
        });
        let a = app.clone();
        sens.connect_value_changed(move |r| {
            let v = r.value() as f32;
            a.update_voice(|st| st.sensitivity_db = v);
        });
        let a = app.clone();
        agc.connect_active_notify(move |r| {
            let on = r.is_active();
            a.update_voice(|st| st.auto_gain = on);
        });
        let a = app.clone();
        echo.connect_active_notify(move |r| {
            a.store.borrow_mut().settings.echo_cancel = r.is_active();
            a.save();
            a.restart_mic();
        });
    }

    let vid = adw::PreferencesGroup::new();
    vid.set_title("Video");
    let cam_kbps = adw::SpinRow::with_range(300.0, 6000.0, 100.0);
    cam_kbps.set_title("Camera bitrate (kbps)");
    cam_kbps.set_value(s.camera_kbps as f64);
    let hw = adw::SwitchRow::builder().title("Hardware video encoding").subtitle("Uses your GPU when available (VA-API / NVENC)").active(s.hw_encode).build();
    vid.add(&cam_kbps);
    vid.add(&hw);
    page.add(&vid);
    d.add(&page);

    // ----- General -----
    let gen_ = adw::PreferencesPage::new();
    gen_.set_title("General");
    gen_.set_icon_name(Some("parley-emblem-system-symbolic"));
    let prof = adw::PreferencesGroup::new();
    prof.set_title("Profile");
    let name = adw::EntryRow::builder().title("Your name").text(&app.store.borrow().my_name).show_apply_button(true).build();
    prof.add(&name);
    let code_row = adw::ActionRow::builder().title("Your code").subtitle(app.my_code().chars().take(32).collect::<String>() + "…").build();
    let copy = gtk::Button::builder().icon_name("parley-edit-copy-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).action_name("app.copy-my-code").build();
    code_row.add_suffix(&copy);
    prof.add(&code_row);
    let pic_row = adw::ActionRow::builder().title("Your picture").subtitle("Only on this computer").build();
    let my_name = app.store.borrow().my_name.clone();
    pic_row.add_prefix(&super::identity::avatar(32, &my_name, &app.me()));
    let pic_btn = gtk::Button::builder().label("Change…").valign(gtk::Align::Center).build();
    let (a, dd) = (app.clone(), d.clone());
    pic_btn.connect_clicked(move |_| {
        dd.close();
        let me = a.me();
        a.edit_picture(me);
    });
    pic_row.add_suffix(&pic_btn);
    prof.add(&pic_row);
    gen_.add(&prof);
    let look = adw::PreferencesGroup::new();
    look.set_title("Appearance");
    let style_row = adw::ComboRow::builder().title("Style").model(&gtk::StringList::new(&["Follow system", "Light", "Dark"])).build();
    style_row.set_selected(match s.appearance {
        crate::store::Appearance::System => 0,
        crate::store::Appearance::Light => 1,
        crate::store::Appearance::Dark => 2,
    });
    let tags = adw::SwitchRow::builder()
        .title("Show #tags after names")
        .subtitle("The end of each person's key, telling apart people with the same name. Strangers calling always show theirs.")
        .active(s.show_tags)
        .build();
    let pictures = adw::ComboRow::builder()
        .title("Pictures")
        .subtitle("For people you haven't chosen a picture for")
        .model(&gtk::StringList::new(&["Patterns from each key", "Initials (GNOME style)"]))
        .build();
    pictures.set_selected(if s.picture_style == crate::store::PictureStyle::Initials { 1 } else { 0 });
    let a = app.clone();
    pictures.connect_selected_notify(move |r| {
        a.set_picture_style(if r.selected() == 1 { crate::store::PictureStyle::Initials } else { crate::store::PictureStyle::Pattern });
    });
    look.add(&style_row);
    look.add(&pictures);
    look.add(&tags);
    gen_.add(&look);
    let a = app.clone();
    style_row.connect_selected_notify(move |r| {
        let v = match r.selected() {
            1 => crate::store::Appearance::Light,
            2 => crate::store::Appearance::Dark,
            _ => crate::store::Appearance::System,
        };
        a.store.borrow_mut().settings.appearance = v;
        a.store.borrow().save();
        super::apply_appearance(v);
    });
    let a = app.clone();
    tags.connect_active_notify(move |r| a.set_show_tags(r.is_active()));
    let beh = adw::PreferencesGroup::new();
    beh.set_title("Behavior");
    let bg = adw::SwitchRow::builder().title("Keep running when closed").subtitle("So friends can still call you").active(s.run_in_background).build();
    let unknown = adw::SwitchRow::builder()
        .title("Allow calls from people not in my contacts")
        .subtitle("They still need your code; you'll always be asked before answering")
        .active(s.allow_unknown_callers)
        .build();
    let autostart_on = if in_flatpak() { s.autostart } else { autostart_enabled() };
    let autostart = adw::SwitchRow::builder().title("Start when I log in").active(autostart_on).build();
    // Both need a way back to Parley (on Windows, its tray icon).
    bg.set_visible(app.can_run_in_background());
    autostart.set_visible(app.can_run_in_background());
    beh.add(&bg);
    beh.add(&autostart);
    beh.add(&unknown);
    gen_.add(&beh);
    let priv_ = adw::PreferencesGroup::new();
    priv_.set_title("Privacy");
    priv_.set_description(Some(
        "Calls are encrypted end to end with keys only you and your friends hold. Parley finds friends through free public relay and discovery servers run by n0 (iroh); when a direct connection isn't possible, encrypted packets pass through a relay, which cannot read or hear anything.",
    ));
    let replace = gtk::Button::builder().label("Replace…").valign(gtk::Align::Center).css_classes(["destructive-action"]).build();
    let replace_row = adw::ActionRow::builder()
        .title("Replace my identity")
        .subtitle("If your code got out: a new code for you, moved over to your friends and groups, and the old one stops working")
        .build();
    replace_row.add_suffix(&replace);
    priv_.add(&replace_row);
    let waiting = app.move_waiting();
    if !waiting.is_empty() {
        let w = adw::ActionRow::builder()
            .title("Still to get your new identity")
            .subtitle(format!("{} — each gets it the next time you're both online", waiting.join(", ")))
            .css_classes(["property"])
            .build();
        priv_.add(&w);
    }
    let (a, dd) = (app.clone(), d.clone());
    replace.connect_clicked(move |_| confirm_replace_identity(&a, &dd));
    gen_.add(&priv_);
    let diag = adw::PreferencesGroup::new();
    diag.set_title("Troubleshooting");
    let log = crate::store::log_path();
    let log_row = adw::ActionRow::builder().title("Diagnostics log").subtitle(log.display().to_string()).subtitle_selectable(true).build();
    let copy_log = gtk::Button::builder().icon_name("parley-edit-copy-symbolic").tooltip_text("Copy path").valign(gtk::Align::Center).css_classes(["flat"]).build();
    let (a, lp) = (app.clone(), log.display().to_string());
    copy_log.connect_clicked(move |_| a.copy(&lp, "Log path"));
    log_row.add_suffix(&copy_log);
    diag.add(&log_row);
    gen_.add(&diag);
    d.add(&gen_);

    // ----- Apply on change -----
    let names = |l: &[devices::Device]| -> Vec<String> { std::iter::once(String::new()).chain(l.iter().map(|d| d.name.clone())).collect() };
    let (mic_names, spk_names, cam_names) = (names(&mics), names(&spks), names(&cams));
    let a = app.clone();
    mic_row.connect_selected_notify(move |r| {
        a.store.borrow_mut().settings.mic = mic_names[r.selected() as usize].clone();
        a.save();
        a.restart_mic();
    });
    let a = app.clone();
    spk_row.connect_selected_notify(move |r| {
        a.store.borrow_mut().settings.speaker = spk_names[r.selected() as usize].clone();
        a.save();
        a.restart_audio_output();
    });
    let a = app.clone();
    cam_row.connect_selected_notify(move |r| {
        a.store.borrow_mut().settings.camera = cam_names[r.selected() as usize].clone();
        a.save();
        a.restart_camera();
    });
    let a = app.clone();
    cam_kbps.connect_value_notify(move |r| {
        a.store.borrow_mut().settings.camera_kbps = r.value() as u32;
        a.save();
    });
    let a = app.clone();
    hw.connect_active_notify(move |r| {
        a.store.borrow_mut().settings.hw_encode = r.is_active();
        a.save();
    });
    let a = app.clone();
    bg.connect_active_notify(move |r| {
        a.store.borrow_mut().settings.run_in_background = r.is_active();
        a.save();
    });
    let a = app.clone();
    unknown.connect_active_notify(move |r| {
        a.store.borrow_mut().settings.allow_unknown_callers = r.is_active();
        a.save();
    });
    let a = app.clone();
    autostart.connect_active_notify(move |r| {
        let on = r.is_active();
        if in_flatpak() {
            #[cfg(target_os = "linux")]
            {
                // Sandboxed apps ask the Background portal instead of writing autostart files.
                let a = a.clone();
                let rt = a.rt.clone();
                glib::spawn_future_local(async move {
                    let res = rt
                        .spawn(async move {
                            ashpd::desktop::background::Background::request()
                                .reason("Start Parley when you log in so friends can call you")
                                .auto_start(on)
                                .command(&["parley", "--background"])
                                .dbus_activatable(false)
                                .send()
                                .await?
                                .response()
                        })
                        .await;
                    match res {
                        Ok(Ok(r)) => {
                            a.store.borrow_mut().settings.autostart = r.auto_start();
                            a.save();
                        }
                        Ok(Err(e)) => a.toast(&format!("Couldn't change autostart: {e}")),
                        Err(_) => {}
                    }
                });
            }
        } else if let Err(e) = set_autostart(on) {
            a.toast(&format!("Couldn't change autostart: {e}"));
        } else {
            a.store.borrow_mut().settings.autostart = on;
            a.save();
        }
    });
    let a = app.clone();
    name.connect_apply(move |e| {
        let n = e.text().trim().to_string();
        if !n.is_empty() {
            a.store.borrow_mut().my_name = n;
            a.save();
            a.sidebar.refresh(&a);
            a.update_home();
        }
    });
    d.present(Some(&app.window));
}

fn confirm_replace_identity(app: &Rc<App>, settings: &adw::PreferencesDialog) {
    let d = adw::AlertDialog::new(Some("Replace your identity?"), Some("For when your code reached people you don't want hearing from you."));
    let details = gtk::Label::new(Some(
        "• You get a new code (and a new #tag). The old one is deleted: nobody can call, message or see you with it any more.\n\n\
         • Your friends and the members of your groups are moved over automatically, each the next time you're both online (they need Parley 0.7.0 or newer). Until then you'll look offline to each other. Chats are kept.\n\n\
         • People who only have your old code don't get the new one.\n\n\
         First remove any friend, and leave any group, you don't trust: they would get your new code too.",
    ));
    details.set_wrap(true);
    details.set_xalign(0.0);
    details.set_max_width_chars(46);
    d.set_extra_child(Some(&details));
    d.add_response("cancel", "Cancel");
    d.add_response("replace", "Replace identity");
    d.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    d.set_default_response(Some("cancel"));
    d.set_close_response("cancel");
    let (a, s) = (app.clone(), settings.clone());
    d.connect_response(None, move |_, r| {
        if r == "replace" {
            s.close();
            a.replace_identity();
        }
    });
    d.present(Some(settings));
}

/// Windows: asks which screen to share.
#[cfg(windows)]
pub async fn pick_monitor(app: &Rc<App>, monitors: &[devices::Device]) -> Option<devices::Device> {
    let d = adw::AlertDialog::new(Some("Share which screen?"), None);
    for (i, m) in monitors.iter().enumerate() {
        let size = devices::monitor_size(&m.device).map(|(w, h)| format!(" — {w}×{h}")).unwrap_or_default();
        d.add_response(&i.to_string(), &format!("Screen {}{size}", i + 1));
        d.set_response_appearance(&i.to_string(), adw::ResponseAppearance::Suggested);
    }
    d.add_response("cancel", "Cancel");
    d.set_close_response("cancel");
    let r = d.choose_future(Some(&app.window)).await;
    r.parse::<usize>().ok().and_then(|i| monitors.get(i).cloned())
}

fn device_combo(title: &str, list: &[devices::Device], current: &str) -> adw::ComboRow {
    let mut labels = vec!["System default".to_string()];
    labels.extend(list.iter().map(|d| d.name.clone()));
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let row = adw::ComboRow::builder().title(title).build();
    row.set_model(Some(&gtk::StringList::new(&refs)));
    let idx = list.iter().position(|d| d.name == current).map(|i| i + 1).unwrap_or(0);
    row.set_selected(idx as u32);
    row
}

fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

#[cfg(not(windows))]
fn autostart_enabled() -> bool {
    autostart_path().exists()
}

#[cfg(not(windows))]
fn autostart_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("autostart").join(format!("{}.desktop", super::APP_ID))
}

#[cfg(not(windows))]
fn set_autostart(on: bool) -> std::io::Result<()> {
    let p = autostart_path();
    if on {
        std::fs::create_dir_all(p.parent().unwrap())?;
        let exe = std::env::current_exe()?;
        std::fs::write(
            &p,
            format!(
                "[Desktop Entry]\nType=Application\nName=Parley\nExec={} --background\nIcon={}\nX-GNOME-Autostart-enabled=true\n",
                exe.display(),
                super::APP_ID
            ),
        )
    } else if p.exists() {
        std::fs::remove_file(p)
    } else {
        Ok(())
    }
}

/// Windows: a value under the user's Run key, starting Parley hidden in the tray.
#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// `reg.exe`, without the console window it would otherwise flash up.
#[cfg(windows)]
fn reg(args: &[&str]) -> std::io::Result<bool> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output()?;
    Ok(out.status.success())
}

#[cfg(windows)]
fn autostart_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "Parley"]).unwrap_or(false)
}

#[cfg(windows)]
fn set_autostart(on: bool) -> std::io::Result<()> {
    let ok = if on {
        let exe = std::env::current_exe()?;
        reg(&["add", RUN_KEY, "/v", "Parley", "/t", "REG_SZ", "/d", &format!("\"{}\" --background", exe.display()), "/f"])?
    } else {
        !autostart_enabled() || reg(&["delete", RUN_KEY, "/v", "Parley", "/f"])?
    };
    if ok { Ok(()) } else { Err(std::io::Error::other("the registry change didn't take")) }
}

/// A contact's details: picture, name#tag, full key and contact code, when they were
/// added, their status and Parley version.
pub fn user_info(app: &Rc<App>, id: iroh::EndpointId) {
    use super::identity;
    // Friends, and people we only share a group with.
    let c = {
        let st = app.store.borrow();
        st.contact(&id).cloned().or_else(|| {
            st.groups.iter().flat_map(|g| g.members.iter()).find(|m| m.id == id).map(|m| crate::store::Contact { id, name: m.name.clone(), added: None })
        })
    };
    let Some(c) = c else { return };
    let friend = app.store.borrow().contact(&id).is_some();
    let d = adw::Dialog::new();
    d.set_title("User info");
    d.set_content_width(440);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());

    let page = adw::PreferencesPage::new();
    let top = gtk::Box::new(gtk::Orientation::Vertical, 8);
    top.set_margin_bottom(12);
    top.append(&identity::avatar(96, &c.name, &id));
    let title = gtk::Label::new(None);
    title.set_markup(&identity::tagged_markup_always(&c.name, &id));
    title.add_css_class("title-2");
    top.append(&title);
    let pic = gtk::Button::builder().label("Change picture…").halign(gtk::Align::Center).css_classes(["flat"]).build();
    top.append(&pic);
    let status = gtk::Label::new(Some(match app.presence_of(&id) {
        crate::proto::Presence::Online => "Online",
        crate::proto::Presence::DoNotDisturb => "Do not disturb",
        crate::proto::Presence::Offline => "Offline",
    }));
    status.add_css_class("dim-label");
    top.append(&status);
    let head = adw::PreferencesGroup::new();
    head.add(&top);
    page.add(&head);

    let g = adw::PreferencesGroup::new();
    let key = id.to_string();
    let key_row = adw::ActionRow::builder()
        .title("Identity key")
        .subtitle(&key)
        .subtitle_selectable(true)
        .build();
    key_row.add_css_class("property");
    let copy_key = gtk::Button::builder().icon_name("parley-edit-copy-symbolic").tooltip_text("Copy key").valign(gtk::Align::Center).css_classes(["flat"]).build();
    let (a, k) = (app.clone(), key.clone());
    copy_key.connect_clicked(move |_| a.copy(&k, "Identity key"));
    key_row.add_suffix(&copy_key);
    g.add(&key_row);

    let code = ShareCode::Contact { n: c.name.clone(), k: id }.encode();
    let code_row = adw::ActionRow::builder().title("Contact code").subtitle(code.chars().take(32).collect::<String>() + "…").build();
    let copy_code = gtk::Button::builder().icon_name("parley-edit-copy-symbolic").tooltip_text("Copy contact code").valign(gtk::Align::Center).css_classes(["flat"]).build();
    let a = app.clone();
    copy_code.connect_clicked(move |_| a.copy(&code, "Contact code"));
    code_row.add_suffix(&copy_code);
    g.add(&code_row);

    let added = match c.added.and_then(|t| glib::DateTime::from_unix_local(t as i64).ok()) {
        Some(dt) => dt.format("%e %B %Y").map(|s| s.trim().to_string()).unwrap_or_default(),
        None => "Before Parley 0.4.3 (not recorded)".to_string(),
    };
    if friend {
        g.add(&adw::ActionRow::builder().title("Added").subtitle(&added).css_classes(["property"]).build());
    } else {
        let row = adw::ActionRow::builder().title("Not your friend").subtitle("You're in a group together").build();
        let add = gtk::Button::builder().label("Add friend").valign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).build();
        let (a, n, dd) = (app.clone(), c.name.clone(), d.clone());
        add.connect_clicked(move |_| {
            a.add_contact(id, &n);
            dd.close();
        });
        row.add_suffix(&add);
        g.add(&row);
    }
    let version = app.version_of(&id).map(|v| format!("Parley {v}")).unwrap_or_else(|| "Unknown (not seen yet, or older than 0.4.3)".into());
    g.add(&adw::ActionRow::builder().title("Version").subtitle(&version).css_classes(["property"]).build());
    page.add(&g);

    let (a, dd) = (app.clone(), d.clone());
    pic.connect_clicked(move |_| {
        dd.close();
        a.edit_picture(id);
    });
    tv.set_content(Some(&page));
    d.set_child(Some(&tv));
    d.present(Some(&app.window));
}
