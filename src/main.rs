//! Parley — private, serverless voice / video / screen-share calls for Linux.

use parley::{hub, net, store, ui};

use std::{cell::RefCell, rc::Rc};

use adw::prelude::*;
use anyhow::Context;

thread_local! {
    static APP: RefCell<Option<Rc<ui::App>>> = const { RefCell::new(None) };
}

fn main() -> anyhow::Result<()> {
    // Log to stderr and to a file (apps started from the menu have no visible stderr).
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let log_path = parley::store::log_path();
    if let Some(dir) = log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::rename(&log_path, log_path.with_extension("log.old"));
    let file = std::fs::File::create(&log_path).ok();
    let filter = tracing_subscriber::EnvFilter::try_from_env("PARLEY_LOG")
        .unwrap_or_else(|_| "parley=info,iroh=warn".into());
    match file {
        Some(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr.and(std::sync::Mutex::new(f)))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
    tracing::info!("Parley {} starting; log at {}", env!("CARGO_PKG_VERSION"), log_path.display());
    gst::init().context("initialising GStreamer")?;
    gtk::glib::set_application_name("Parley");
    parley::media::send::warm_up_encoders();

    #[cfg(windows)]
    let Some(_running) = store::single_instance() else {
        tracing::info!("Parley is already running");
        return Ok(());
    };

    let background = std::env::args().any(|a| a == "--background");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("parley-net")
        .build()?;

    let secret = store::load_identity()?;
    let me = secret.public();
    let hub = hub::Hub::new();
    let (ui_tx, ui_rx) = async_channel::unbounded();
    let net = rt.block_on(net::start(secret, hub.clone(), ui_tx.clone()))?;

    // A separate profile (for running two copies side by side) needs its own app id.
    let app_id = match std::env::var("PARLEY_PROFILE") {
        Ok(p) if !p.is_empty() => format!("{}.{}", ui::APP_ID, p.replace(|c: char| !c.is_ascii_alphanumeric(), "")),
        _ => ui::APP_ID.to_string(),
    };
    let app = adw::Application::builder().application_id(app_id).build();
    let handle = rt.handle().clone();
    let first = std::cell::Cell::new(true);
    app.connect_activate(move |app| {
        APP.with(|cell| {
            if let Some(a) = cell.borrow().as_ref() {
                a.present();
                return;
            }
            let a = ui::App::new(app, net.clone(), hub.clone(), me, handle.clone(), ui_tx.clone(), ui_rx.clone());
            if first.replace(false) && background {
                a.window.set_visible(false);
                // Keep running with no window until a call comes in or the user opens us.
                std::mem::forget(app.hold());
            } else {
                a.present();
            }
            *cell.borrow_mut() = Some(a);
        });
    });
    // Our own flags (like --background) aren't GTK's business.
    let code = app.run_with_args::<&str>(&[]);
    APP.with(|c| c.borrow_mut().take());
    // Don't let lingering network/IO tasks keep the process alive after Quit.
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    std::process::exit(if code == gtk::glib::ExitCode::SUCCESS { 0 } else { 1 });
}
