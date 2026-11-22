//! In-app updates. When a new version is out a small button appears in the top bar;
//! nothing is installed until the user agrees.
//!
//! - **Flatpak** (from a release bundle, which follows Parley's hosted repo): Flatpak's own update portal does
//!   the work, checks the repo's GPG signature, and needs no extra sandbox permission.
//!   It only looks for updates every 30 minutes, so release builds also read the signed
//!   manifest when Parley opens.
//! - **Windows**: a manifest signed with the release key names the new zip and its
//!   checksum. The zip is unpacked beside the install and swapped in after Parley exits.
//! - **Built from source**: updates the way it was built (`git pull && ./install.sh`).

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// Public half of the key that signs `latest.json` (Windows updates). A test build can
/// point at its own server and key at compile time; a normal build can't be redirected.
pub const MANIFEST_KEY: &str = match option_env!("PARLEY_UPDATE_KEY") {
    Some(k) => k,
    None => "788c14c04659e10d73dd847f58df7477635d99dc128906598cbdcf339e15c0fb",
};
#[cfg(any(windows, target_os = "linux"))]
const MANIFEST_URL: &str = match option_env!("PARLEY_UPDATE_URL") {
    Some(u) => u,
    None => "https://github.com/lpuq/parley/releases/latest/download/latest.json",
};
/// Set by the release build scripts (and checked by `flatpak/publish-repo.sh`). A build
/// from source can't take updates from the hosted repo, so it isn't offered them.
#[cfg(target_os = "linux")]
const RELEASE_BUILD: Option<&str> = option_env!("PARLEY_RELEASE_BUILD");

/// What a release offers, signed by the release key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub windows_x64: WindowsAsset,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowsAsset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// Parses a manifest, but only if `sig_hex` is the key's signature over exactly these bytes.
pub fn verified_manifest(json: &[u8], sig_hex: &str, key: &str) -> Result<Manifest> {
    let key: iroh::PublicKey = key.parse().map_err(|_| anyhow!("bad update key"))?;
    let sig: [u8; 64] = hex::decode(sig_hex.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("malformed update signature"))?;
    key.verify(json, &iroh::Signature::from_bytes(&sig)).map_err(|_| anyhow!("update signature doesn't match"))?;
    serde_json::from_slice(json).context("reading the update manifest")
}

/// Whether version `new` is later than `current` (numeric `major.minor.patch`).
pub fn is_newer(new: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let mut p = v.trim().trim_start_matches('v').split('.').map(|n| n.parse::<u32>().ok());
        Some((p.next()??, p.next()??, p.next()??))
    };
    matches!((parse(new), parse(current)), (Some(n), Some(c)) if n > c)
}

/// The curl that ships with the system (Windows) or the Flatpak runtime.
#[cfg(any(windows, target_os = "linux"))]
fn curl() -> std::path::PathBuf {
    #[cfg(windows)]
    return windows::system_tool("curl.exe");
    #[cfg(not(windows))]
    std::path::PathBuf::from("/usr/bin/curl")
}

#[cfg(any(windows, target_os = "linux"))]
fn fetch(url: &str) -> Result<Vec<u8>> {
    let mut command = std::process::Command::new(curl());
    command.args(["-fsSL", "--max-time", "30", url]);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, windows::CREATE_NO_WINDOW);
    let out = command.output().context("running curl")?;
    if !out.status.success() {
        anyhow::bail!("couldn't reach the update server");
    }
    Ok(out.stdout)
}

/// The signed manifest, if it offers a newer version than this one.
#[cfg(any(windows, target_os = "linux"))]
fn check() -> Result<Option<Manifest>> {
    let json = fetch(MANIFEST_URL)?;
    let sig = fetch(&format!("{MANIFEST_URL}.sig"))?;
    let m = verified_manifest(&json, &String::from_utf8_lossy(&sig), MANIFEST_KEY)?;
    Ok(is_newer(&m.version, env!("CARGO_PKG_VERSION")).then_some(m))
}

pub enum UpdateEvent {
    /// A new version is out (its number, when known).
    Available(Option<String>),
    /// Downloading and installing, in percent.
    Progress(u32),
    /// Installed: takes effect when Parley restarts.
    Ready,
    Failed(String),
}

/// Whether this install can update itself.
pub fn supported() -> bool {
    #[cfg(target_os = "linux")]
    return flatpak::in_flatpak();
    #[cfg(windows)]
    return windows::install_dir().is_some();
    #[cfg(not(any(target_os = "linux", windows)))]
    false
}

pub struct Updater {
    #[cfg(target_os = "linux")]
    portal: std::rc::Rc<flatpak::Portal>,
    #[cfg(windows)]
    win: windows::Win,
}

impl Updater {
    /// Starts watching for new versions; `None` if this install can't update itself.
    pub fn start(tx: async_channel::Sender<UpdateEvent>) -> Option<Self> {
        if !supported() {
            return None;
        }
        #[cfg(target_os = "linux")]
        return Some(Self { portal: flatpak::Portal::start(tx) });
        #[cfg(windows)]
        return Some(Self { win: windows::Win::start(tx) });
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = tx;
            None
        }
    }

    pub fn install(&self) {
        #[cfg(target_os = "linux")]
        self.portal.install();
        #[cfg(windows)]
        self.win.install();
    }

    /// Arranges for the new version to start once Parley has quit.
    pub fn relaunch_after_quit(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        return self.portal.relaunch();
        #[cfg(windows)]
        return self.win.relaunch();
        #[cfg(not(any(target_os = "linux", windows)))]
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod flatpak {
    use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::mpsc, time::Duration};

    use anyhow::{Result, anyhow};
    use gtk::{gio, glib, prelude::*};

    use super::{RELEASE_BUILD, UpdateEvent, check};

    const BUS: &str = "org.freedesktop.portal.Flatpak";
    const PATH: &str = "/org/freedesktop/portal/Flatpak";
    const MONITOR: &str = "org.freedesktop.portal.Flatpak.UpdateMonitor";
    /// Start the newest installed version, not the one this process runs from.
    const SPAWN_LATEST_VERSION: &str = "uint32 2";
    /// At login the network may not be up yet.
    const STARTUP_TRIES: u32 = 4;
    const STARTUP_RETRY: Duration = Duration::from_secs(45);

    /// The sandbox can't see which remote it came from, and needn't: the portal watches
    /// that remote. A build from `./install.sh` has a local one, so it's never offered
    /// anything until it's built again.
    pub fn in_flatpak() -> bool {
        std::path::Path::new("/.flatpak-info").exists()
    }

    pub struct Portal {
        tx: async_channel::Sender<UpdateEvent>,
        conn: RefCell<Option<gio::DBusConnection>>,
        monitor: RefCell<Option<String>>,
        subscription: RefCell<Option<gio::SignalSubscription>>,
        /// Dropping this ends the check at startup (Settings → Check for updates).
        _stop: mpsc::Sender<()>,
    }

    impl Portal {
        pub fn start(tx: async_channel::Sender<UpdateEvent>) -> Rc<Self> {
            let (stop, stopped) = mpsc::channel();
            let this = Rc::new(Self { tx, conn: Default::default(), monitor: Default::default(), subscription: Default::default(), _stop: stop });
            let t = this.clone();
            glib::spawn_future_local(async move {
                // (Only offer an update once the portal is there to install it.)
                match t.connect().await {
                    Ok(()) => check_at_startup(t.tx.clone(), stopped),
                    Err(e) => tracing::warn!("update checks unavailable: {e:#}"),
                }
            });
            this
        }

        async fn connect(&self) -> Result<()> {
            let conn = gio::bus_get_future(gio::BusType::Session).await?;
            let options: HashMap<String, glib::Variant> = HashMap::new();
            let reply = conn
                .call_future(
                    Some(BUS),
                    PATH,
                    BUS,
                    "CreateUpdateMonitor",
                    Some(&(options,).to_variant()),
                    Some(glib::VariantTy::new("(o)").unwrap()),
                    gio::DBusCallFlags::NONE,
                    30_000,
                )
                .await?;
            let path = reply.child_value(0).str().ok_or_else(|| anyhow!("no monitor path"))?.to_string();
            let tx = self.tx.clone();
            let sub = conn.subscribe_to_signal(
                Some(BUS),
                Some(MONITOR),
                None,
                Some(&path),
                None,
                gio::DBusSignalFlags::NONE,
                move |s| on_signal(&tx, s.signal_name, s.parameters),
            );
            *self.subscription.borrow_mut() = Some(sub);
            *self.monitor.borrow_mut() = Some(path);
            *self.conn.borrow_mut() = Some(conn);
            tracing::info!("watching for updates through the Flatpak portal");
            Ok(())
        }

        pub fn install(&self) {
            let (Some(conn), Some(path)) = (self.conn.borrow().clone(), self.monitor.borrow().clone()) else { return };
            let options: HashMap<String, glib::Variant> = HashMap::new();
            let tx = self.tx.clone();
            conn.call(
                Some(BUS),
                &path,
                MONITOR,
                "Update",
                Some(&("", options).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
                move |r| {
                    if let Err(e) = r {
                        let _ = tx.try_send(UpdateEvent::Failed(e.message().to_string()));
                    }
                },
            );
        }

        pub fn relaunch(&self) -> Result<()> {
            let conn = self.conn.borrow().clone().ok_or_else(|| anyhow!("not connected"))?;
            // In a fresh sandbox of the new version, once this one has gone (a second
            // copy would just hand over to this one and exit).
            let args = format!(
                "(b'/', [b'sh', b'-c', b'sleep 1; exec parley'], @a{{uh}} {{}}, @a{{ss}} {{}}, {SPAWN_LATEST_VERSION}, @a{{sv}} {{}})"
            );
            let args = glib::Variant::parse(None, &args)?;
            conn.call_sync(Some(BUS), PATH, BUS, "Spawn", Some(&args), None, gio::DBusCallFlags::NONE, 10_000, gio::Cancellable::NONE)?;
            Ok(())
        }
    }

    /// Release builds don't wait for the portal's first look, 30 minutes after opening.
    fn check_at_startup(tx: async_channel::Sender<UpdateEvent>, stopped: mpsc::Receiver<()>) {
        let Some(build) = RELEASE_BUILD else { return };
        let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            tracing::info!("checking for updates ({build})");
            for _ in 0..STARTUP_TRIES {
                match check() {
                    Ok(found) => {
                        if let (Some(m), Err(mpsc::TryRecvError::Empty)) = (found, stopped.try_recv()) {
                            let _ = tx.send_blocking(UpdateEvent::Available(Some(m.version)));
                        }
                        return;
                    }
                    Err(e) => tracing::info!("update check: {e:#}"),
                }
                if !matches!(stopped.recv_timeout(STARTUP_RETRY), Err(mpsc::RecvTimeoutError::Timeout)) {
                    return;
                }
            }
        });
    }

    impl Drop for Portal {
        fn drop(&mut self) {
            if let (Some(conn), Some(path)) = (self.conn.get_mut().take(), self.monitor.get_mut().take()) {
                conn.call(Some(BUS), &path, MONITOR, "Close", None, None, gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, |_| {});
            }
        }
    }

    fn on_signal(tx: &async_channel::Sender<UpdateEvent>, name: &str, params: &glib::Variant) {
        let info = glib::VariantDict::new(Some(&params.child_value(0)));
        let ev = match name {
            "UpdateAvailable" => UpdateEvent::Available(None),
            "Progress" => match info.lookup::<u32>("status").ok().flatten() {
                Some(0) => UpdateEvent::Progress(info.lookup::<u32>("progress").ok().flatten().unwrap_or(0)),
                Some(2) => UpdateEvent::Ready,
                // Nothing to install: the repo hasn't caught up with the release yet.
                Some(1) => UpdateEvent::Failed("this update isn't ready yet. Try again in a few minutes.".into()),
                _ => UpdateEvent::Failed(
                    info.lookup::<String>("error_message").ok().flatten().unwrap_or_else(|| "the update failed".into()),
                ),
            },
            _ => return,
        };
        let _ = tx.try_send(ev);
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        cell::RefCell,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::Command,
        rc::Rc,
        time::Duration,
    };

    use anyhow::{Context, Result, anyhow, bail};
    use gtk::glib;
    use sha2::{Digest, Sha256};

    use super::{Manifest, UpdateEvent, check, curl};

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
    const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

    /// A tool that ships with Windows, by full path: Parley's own folder is on PATH.
    pub fn system_tool(name: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        root.join("System32").join(name)
    }

    /// The folder Parley was unzipped to (`…\Parley`, holding `bin\parley.exe`).
    pub fn install_dir() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let bin = exe.parent()?;
        (bin.file_name()? == "bin" && bin.parent()?.join("lib").is_dir()).then(|| bin.parent().unwrap().to_path_buf())
    }

    /// Where the new version is unpacked: beside the install, so swapping is a rename.
    fn staging(install: &Path) -> PathBuf {
        install.with_file_name(format!("{}.new", install.file_name().unwrap_or_default().to_string_lossy()))
    }

    /// Downloads and checks the zip, and unpacks it beside the install.
    fn download(m: &Manifest, tx: &async_channel::Sender<UpdateEvent>) -> Result<()> {
        let install = install_dir().ok_or_else(|| anyhow!("can't tell where Parley is installed"))?;
        let stage = staging(&install);
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).with_context(|| format!("can't write next to {}", install.display()))?;
        let zip = stage.join("update.zip");
        let mut curl = Command::new(curl())
            .args(["-fsSL", "--max-time", "1800", "-o"])
            .arg(&zip)
            .arg(&m.windows_x64.url)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("running curl")?;
        let status = loop {
            if let Some(s) = curl.try_wait()? {
                break s;
            }
            let got = std::fs::metadata(&zip).map(|f| f.len()).unwrap_or(0);
            let _ = tx.try_send(UpdateEvent::Progress((got * 95 / m.windows_x64.size.max(1)).min(95) as u32));
            std::thread::sleep(Duration::from_millis(400));
        };
        if !status.success() {
            bail!("the download failed");
        }
        let bytes = std::fs::read(&zip)?;
        let hash: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if !hash.eq_ignore_ascii_case(&m.windows_x64.sha256) {
            bail!("the download is damaged (checksum mismatch)");
        }
        let ok = Command::new(system_tool("tar.exe"))
            .arg("-xf")
            .arg(&zip)
            .arg("-C")
            .arg(&stage)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .context("running tar")?
            .success();
        let _ = std::fs::remove_file(&zip);
        if !ok || !stage.join("Parley").join("bin").join("parley.exe").is_file() {
            bail!("the download couldn't be unpacked");
        }
        Ok(())
    }

    pub struct Win {
        tx: async_channel::Sender<UpdateEvent>,
        offered: Rc<RefCell<Option<Manifest>>>,
        /// Dropping this ends the background checks (Settings → Check for updates).
        _stop: std::sync::mpsc::Sender<()>,
    }

    impl Win {
        pub fn start(tx: async_channel::Sender<UpdateEvent>) -> Self {
            let offered: Rc<RefCell<Option<Manifest>>> = Rc::default();
            let (found_tx, found_rx) = async_channel::bounded::<Manifest>(1);
            let (stop, stopped) = std::sync::mpsc::channel::<()>();
            let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
                loop {
                    let wait = match check() {
                        Ok(Some(m)) => {
                            let _ = found_tx.send_blocking(m);
                            return;
                        }
                        Ok(None) => CHECK_EVERY,
                        Err(e) => {
                            tracing::info!("update check: {e:#}");
                            RETRY_AFTER
                        }
                    };
                    if !matches!(stopped.recv_timeout(wait), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                        return;
                    }
                }
            });
            let (o, t) = (offered.clone(), tx.clone());
            glib::spawn_future_local(async move {
                if let Ok(m) = found_rx.recv().await {
                    let version = m.version.clone();
                    *o.borrow_mut() = Some(m);
                    let _ = t.send(UpdateEvent::Available(Some(version))).await;
                }
            });
            Self { tx, offered, _stop: stop }
        }

        pub fn install(&self) {
            let Some(m) = self.offered.borrow().clone() else { return };
            let tx = self.tx.clone();
            let _ = std::thread::Builder::new().name("update-download".into()).spawn(move || {
                let ev = match download(&m, &tx) {
                    Ok(()) => UpdateEvent::Ready,
                    Err(e) => UpdateEvent::Failed(format!("{e:#}")),
                };
                let _ = tx.send_blocking(ev);
            });
        }

        /// A little script waits for this process to end, swaps the folders (putting the
        /// old one back if that fails) and starts Parley.
        pub fn relaunch(&self) -> Result<()> {
            let install = install_dir().ok_or_else(|| anyhow!("can't tell where Parley is installed"))?;
            let stage = staging(&install);
            let old = install.with_file_name(format!("{}.old", install.file_name().unwrap_or_default().to_string_lossy()));
            let (pid, i, s, o) = (std::process::id(), install.display(), stage.display(), old.display());
            let script = format!(
                "@echo off\r\nchcp 65001 >nul\r\nset PATH=%SystemRoot%\\System32;%SystemRoot%\r\n\
                 :wait\r\ntasklist /FI \"PID eq {pid}\" /NH | find \"{pid}\" >nul && (ping -n 2 127.0.0.1 >nul & goto wait)\r\n\
                 rmdir /s /q \"{o}\" 2>nul\r\n\
                 set tries=0\r\n\
                 :swap\r\nmove \"{i}\" \"{o}\" >nul 2>&1 && goto moved\r\n\
                 set /a tries+=1\r\nif %tries% geq 15 goto start\r\nping -n 2 127.0.0.1 >nul\r\ngoto swap\r\n\
                 :moved\r\nmove \"{s}\\Parley\" \"{i}\" >nul 2>&1 || (move \"{o}\" \"{i}\" >nul 2>&1 & goto start)\r\n\
                 rmdir /s /q \"{o}\" 2>nul\r\nrmdir /s /q \"{s}\" 2>nul\r\n\
                 :start\r\nstart \"\" \"{i}\\bin\\parley.exe\"\r\n(goto) 2>nul & del \"%~f0\"\r\n"
            );
            let path = crate::store::data_dir().join("finish-update.cmd");
            std::fs::write(&path, script)?;
            // (Not from Parley's working directory: that may be inside the folder being swapped.)
            Command::new(system_tool("cmd.exe"))
                .arg("/c")
                .arg(&path)
                .current_dir(crate::store::data_dir())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .context("starting the update")?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            version: "1.2.3".into(),
            windows_x64: WindowsAsset { url: "https://example.org/p.zip".into(), sha256: "ab".repeat(32), size: 42 },
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn a_signed_manifest_is_accepted() {
        let key = iroh::SecretKey::generate();
        let json = serde_json::to_vec(&manifest()).unwrap();
        let sig = hex(&key.sign(&json).to_bytes());
        assert_eq!(verified_manifest(&json, &sig, &key.public().to_string()).unwrap(), manifest());
    }

    #[test]
    fn a_tampered_or_foreign_manifest_is_refused() {
        let (key, other) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let json = serde_json::to_vec(&manifest()).unwrap();
        let sig = hex(&key.sign(&json).to_bytes());
        // Someone else's key.
        assert!(verified_manifest(&json, &sig, &other.public().to_string()).is_err());
        // A changed download link under the original signature.
        let mut evil = manifest();
        evil.windows_x64.url = "https://evil.example/p.zip".into();
        let evil = serde_json::to_vec(&evil).unwrap();
        assert!(verified_manifest(&evil, &sig, &key.public().to_string()).is_err());
        assert!(verified_manifest(&json, "not a signature", &key.public().to_string()).is_err());
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("0.12.0", "0.11.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("0.11.0", "0.11.0"));
        assert!(!is_newer("0.10.9", "0.11.0"));
        assert!(!is_newer("garbage", "0.11.0"));
    }
}
