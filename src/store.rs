//! Persistent state: identity key, profile, contacts, groups and settings.
//! Also the share-code format used to exchange contacts and groups.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{fs, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, Mac};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CODE_PREFIX: &str = "parley:";

/// Where per-user folders live on Windows: %APPDATA% (settings, contacts) and
/// %LOCALAPPDATA% (identity, chats, log).
fn windows_dir(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_default()).join(app_dir_name())
}

fn config_dir() -> PathBuf {
    if cfg!(windows) {
        return windows_dir("APPDATA");
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join(app_dir_name())
}

pub(crate) fn data_dir() -> PathBuf {
    if cfg!(windows) {
        return windows_dir("LOCALAPPDATA");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join(app_dir_name())
}

/// Diagnostics log (Flatpak: ~/.var/app/io.github.parley.Parley/.local/state/parley/parley.log).
pub fn log_path() -> PathBuf {
    if cfg!(windows) {
        return windows_dir("LOCALAPPDATA").join("parley.log");
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join(app_dir_name()).join("parley.log")
}

/// `PARLEY_PROFILE=name` lets two instances run side by side (handy for testing).
fn app_dir_name() -> String {
    match std::env::var("PARLEY_PROFILE") {
        Ok(p) if !p.is_empty() => format!("parley-{p}"),
        _ => "parley".into(),
    }
}

/// Windows: the pipe a running Parley listens on. GApplication has no D-Bus there to
/// keep Parley to one copy, and a second copy would be the same identity on the network;
/// instead a second copy finds this pipe taken, asks the first to show its window, and
/// exits.
#[cfg(windows)]
pub fn instance_pipe() -> String {
    format!(r"\\.\pipe\{}", app_dir_name())
}

/// Loads the long-term identity key, creating it on first run.
/// The public half is this user's address; the file never leaves the machine.
pub fn load_or_create_identity() -> Result<SecretKey> {
    let path = data_dir().join("identity.key");
    if let Ok(bytes) = fs::read(&path) {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("identity.key is corrupt")?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    fs::create_dir_all(data_dir())?;
    let key = SecretKey::generate();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .private()
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(&key.to_bytes())?;
    Ok(key)
}

/// Replacing the identity, step 1: the new key is written beside the current one.
/// It only takes over once [`commit_identity`] runs (after the move is recorded),
/// so a crash in between leaves the old identity working.
pub fn stage_identity(key: &SecretKey) -> Result<()> {
    let path = data_dir().join("identity.next.key");
    let _ = fs::remove_file(&path);
    let mut f = fs::OpenOptions::new().write(true).create_new(true).private().open(&path)?;
    f.write_all(&key.to_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Step 2: the new key replaces the old one, which is gone for good.
pub fn commit_identity() -> Result<()> {
    fs::rename(data_dir().join("identity.next.key"), data_dir().join("identity.key"))?;
    Ok(())
}

/// The identity to start with, finishing a replacement a crash interrupted (if the
/// move was recorded) or abandoning it (if not).
pub fn load_identity() -> Result<SecretKey> {
    let next = data_dir().join("identity.next.key");
    if let Ok(bytes) = fs::read(&next) {
        let recorded = <[u8; 32]>::try_from(bytes.as_slice())
            .ok()
            .map(|b| SecretKey::from_bytes(&b).public())
            .is_some_and(|id| Store::load().moves.last().is_some_and(|m| m.new == id));
        if recorded {
            commit_identity()?;
        } else {
            let _ = fs::remove_file(&next);
        }
    }
    load_or_create_identity()
}

/// Files only we may read (identity, contacts): owner-only on Unix. On Windows the
/// per-user AppData folders already are.
trait Private {
    fn private(&mut self) -> &mut Self;
}

impl Private for fs::OpenOptions {
    fn private(&mut self) -> &mut Self {
        #[cfg(unix)]
        self.mode(0o600);
        self
    }
}

pub fn random_bytes32() -> [u8; 32] {
    // SecretKey::generate draws from the OS CSPRNG.
    SecretKey::generate().to_bytes()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub id: EndpointId,
    pub name: String,
    /// When they were added (Unix seconds); unknown for contacts saved before 0.4.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,
}

impl Contact {
    /// A contact added now.
    pub fn new(id: EndpointId, name: impl Into<String>) -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).ok();
        Self { id, name: name.into(), added: now }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Member {
    pub id: EndpointId,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Group {
    pub name: String,
    /// Shared secret; anyone holding it may join. The room id is derived from it.
    #[serde(with = "hex32")]
    pub secret: [u8; 32],
    #[serde(default)]
    pub members: Vec<Member>,
    /// Who made the group, and may remove people from it. Groups from before 0.8.0
    /// take their first member (the creator, in every copy of the group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<EndpointId>,
    /// People the owner took out of the group (each signed by the owner).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<Removal>,
}

/// The group's owner taking someone out of it, signed so every member can check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Removal {
    pub id: EndpointId,
    pub by: EndpointId,
    pub ts: u64,
    pub sig: iroh::Signature,
}

impl Removal {
    fn signed_bytes(room: &RoomId, id: &EndpointId, by: &EndpointId, ts: u64) -> Vec<u8> {
        let mut m = b"parley-group-remove/1:".to_vec();
        m.extend_from_slice(room);
        m.extend_from_slice(id.as_bytes());
        m.extend_from_slice(by.as_bytes());
        m.extend_from_slice(&ts.to_be_bytes());
        m
    }

    pub fn sign(owner: &SecretKey, room: &RoomId, id: EndpointId, ts: u64) -> Self {
        let by = owner.public();
        Self { id, by, ts, sig: owner.sign(&Self::signed_bytes(room, &id, &by, ts)) }
    }

    fn verify(&self, room: &RoomId) -> bool {
        self.by.verify(&Self::signed_bytes(room, &self.id, &self.by, self.ts), &self.sig).is_ok()
    }
}

impl Group {
    /// A new group, owned by its first member (whoever creates it).
    pub fn new(name: String, members: Vec<Member>) -> Self {
        let owner = members.first().map(|m| m.id);
        Self { name, secret: random_bytes32(), members, owner, removed: Vec::new() }
    }

    /// A group as learned from a code or an invite.
    pub fn learned(name: String, secret: [u8; 32], members: Vec<Member>, owner: Option<EndpointId>, removed: &[Removal]) -> Self {
        let owner = owner.or_else(|| members.first().map(|m| m.id));
        let mut g = Self { name, secret, members, owner, removed: Vec::new() };
        g.merge_removals(removed);
        g
    }

    /// The owner, for groups saved before owners were recorded: their first member.
    pub fn settle_owner(&mut self) {
        if self.owner.is_none() {
            self.owner = self.members.first().map(|m| m.id);
        }
    }

    pub fn is_removed(&self, id: &EndpointId) -> bool {
        self.removed.iter().any(|r| &r.id == id)
    }

    /// Takes in removals signed by this group's owner (others are ignored, as is any
    /// removal of the owner). Returns whether anything changed.
    pub fn merge_removals(&mut self, incoming: &[Removal]) -> bool {
        let (room, Some(owner)) = (self.room_id(), self.owner) else { return false };
        let mut changed = false;
        for r in incoming.iter().filter(|r| r.by == owner && r.id != owner && r.verify(&room)) {
            match self.removed.iter_mut().find(|x| x.id == r.id) {
                // Re-signed by the owner's current identity (after they replaced it).
                Some(x) if x.by != owner => {
                    *x = r.clone();
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.removed.push(r.clone());
                    changed = true;
                }
            }
        }
        let before = self.members.len();
        let removed = std::mem::take(&mut self.removed);
        self.members.retain(|m| !removed.iter().any(|r| r.id == m.id));
        self.removed = removed;
        changed || self.members.len() != before
    }
    pub fn room_id(&self) -> RoomId {
        room_id_for(&self.secret)
    }
    pub fn add_member(&mut self, id: EndpointId, name: &str) -> bool {
        if self.is_removed(&id) {
            return false;
        }
        if let Some(m) = self.members.iter_mut().find(|m| m.id == id) {
            if !name.is_empty() && m.name != name {
                m.name = name.to_string();
                return true;
            }
            return false;
        }
        self.members.push(Member { id, name: name.to_string() });
        true
    }
}

pub type RoomId = [u8; 32];

pub fn room_id_for(secret: &[u8; 32]) -> RoomId {
    let mut h = Sha256::new();
    h.update(b"parley-room-v1");
    h.update(secret);
    h.finalize().into()
}

/// Proof that the sender of a join request holds the group secret, bound to
/// both endpoint ids so it can't be replayed to someone else.
pub fn join_proof(secret: &[u8; 32], from: &EndpointId, to: &EndpointId) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac key");
    mac.update(b"parley-join-v1");
    mac.update(&room_id_for(secret));
    mac.update(from.as_bytes());
    mac.update(to.as_bytes());
    mac.finalize().into_bytes().into()
}

pub fn verify_proof(secret: &[u8; 32], from: &EndpointId, to: &EndpointId, proof: &[u8; 32]) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac key");
    mac.update(b"parley-join-v1");
    mac.update(&room_id_for(secret));
    mac.update(from.as_bytes());
    mac.update(to.as_bytes());
    mac.verify_slice(proof).is_ok()
}

/// Screen share resolutions (max height) and frame rates offered in the share dialog.
pub const SCREEN_HEIGHTS: [u32; 4] = [480, 720, 1080, 1440];
/// Preset frame rates; the share dialog also offers a custom value.
pub const SCREEN_FPS: [u32; 3] = [24, 30, 60];

/// Bitrate for a screen share: grows with the pixel count, and less than linearly
/// with the frame rate (consecutive frames are similar). 1080p30 = 4.5 Mbps,
/// 1080p60 ≈ 7 Mbps. Adaptive streaming may go lower when the link is congested.
pub fn screen_kbps(height: u32, fps: u32) -> u32 {
    let pixels = (height * height * 16 / 9) as f64;
    let base = 4500.0 * (pixels / (1080.0 * 1920.0)).powf(0.75);
    (base * (fps as f64 / 30.0).powf(0.6)).round() as u32 / 100 * 100
}

/// Microphone processing presets, as in Discord.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum VoiceMode {
    /// Neural noise suppression, automatic sensitivity and automatic gain.
    #[default]
    Isolation,
    /// The microphone as it is: no suppression, gate or gain.
    Studio,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum Suppression {
    /// RNNoise: removes keyboards, fans, chewing and other non-speech sounds.
    #[default]
    Enhanced,
    /// WebRTC's classic suppressor: steady background noise only.
    Standard,
    Off,
}

/// What friends see: online, busy, or nothing at all.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum Status {
    #[default]
    Online,
    /// Calls arrive silently: no ringtone or notifications.
    DoNotDisturb,
    /// Look offline to everyone.
    Invisible,
}

/// The processing the microphone pipeline should apply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceConfig {
    /// WebRTC noise suppression (inside webrtcdsp).
    pub webrtc_ns: bool,
    /// RNNoise.
    pub denoise: bool,
    /// None = gate off; Some(None) = automatic; Some(Some(db)) = threshold.
    pub gate: Option<Option<f32>>,
    pub auto_gain: bool,
}

impl Settings {
    /// (max height, fps, kbps) for screen sharing.
    pub fn screen_params(&self) -> (u32, u32, u32) {
        (self.screen_height, self.screen_fps, screen_kbps(self.screen_height, self.screen_fps))
    }

    pub fn voice_config(&self) -> VoiceConfig {
        match self.voice_mode {
            VoiceMode::Isolation => VoiceConfig { webrtc_ns: false, denoise: true, gate: Some(None), auto_gain: true },
            VoiceMode::Studio => VoiceConfig { webrtc_ns: false, denoise: false, gate: None, auto_gain: false },
            VoiceMode::Custom => VoiceConfig {
                webrtc_ns: self.suppression == Suppression::Standard,
                denoise: self.suppression == Suppression::Enhanced,
                gate: Some(if self.sensitivity_auto { None } else { Some(self.sensitivity_db) }),
                auto_gain: self.auto_gain,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Device display names (empty = system default).
    pub mic: String,
    pub speaker: String,
    pub camera: String,
    pub echo_cancel: bool,
    /// Voice processing preset; the fields below apply in [`VoiceMode::Custom`].
    pub voice_mode: VoiceMode,
    pub suppression: Suppression,
    /// Input sensitivity: automatic (voice detection) or a fixed threshold.
    pub sensitivity_auto: bool,
    pub sensitivity_db: f32,
    pub auto_gain: bool,
    /// Online, Do Not Disturb or Invisible.
    pub status: Status,
    /// Screen share: max height (720/1080/1440) and frame rate, chosen independently.
    pub screen_height: u32,
    pub screen_fps: u32,
    pub camera_kbps: u32,
    pub hw_encode: bool,
    pub run_in_background: bool,
    /// Show a button in the top bar when a new version is out.
    pub check_updates: bool,
    pub allow_unknown_callers: bool,
    /// Remembered for the Flatpak case, where the portal owns the real setting.
    pub autostart: bool,
    /// We told the user once that closing the window keeps Parley running.
    pub background_hint_shown: bool,
    /// Allow "system default" to pick a Bluetooth headset mic (switches earbuds to headset mode).
    pub allow_bluetooth_mic: bool,
    /// List desktop audio among the microphones (Settings → Advanced).
    pub allow_desktop_mic: bool,
    /// The user hid the sidebar (it stays docked and shown otherwise).
    pub sidebar_hidden: bool,
    pub appearance: Appearance,
    /// Show the #tag after names (the end of each person's key).
    pub show_tags: bool,
    /// What people look like without a picture you chose.
    pub picture_style: PictureStyle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PictureStyle {
    /// A pattern drawn from the person's key (no two identities alike).
    #[default]
    Pattern,
    /// Initials on a colour, GNOME style.
    Initials,
}

/// A picture chosen for someone (or ourselves). It stays on this computer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Picture {
    pub id: EndpointId,
    pub kind: PictureKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PictureKind {
    /// An image, kept as [`picture_path`].
    Image,
    /// Letters, or a symbolic icon, on a colour or a gradient between two (#rrggbb).
    Made { letters: String, icon: Option<String>, from: String, to: Option<String> },
}

/// Where an image picture is kept (a 256 px square PNG).
pub fn picture_path(id: &EndpointId) -> PathBuf {
    data_dir().join("pictures").join(format!("{id}.png"))
}

/// Light or dark, or whatever the system prefers (dark when it has no preference).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic: String::new(),
            speaker: String::new(),
            camera: String::new(),
            echo_cancel: true,
            voice_mode: VoiceMode::default(),
            suppression: Suppression::default(),
            sensitivity_auto: true,
            sensitivity_db: -50.0,
            auto_gain: true,
            status: Status::default(),
            screen_height: 1080,
            screen_fps: 60,
            camera_kbps: 1500,
            hw_encode: true,
            run_in_background: true,
            check_updates: true,
            allow_unknown_callers: true,
            autostart: false,
            background_hint_shown: false,
            allow_bluetooth_mic: false,
            allow_desktop_mic: false,
            sidebar_hidden: false,
            appearance: Appearance::System,
            show_tags: true,
            picture_style: PictureStyle::Pattern,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Store {
    pub my_name: String,
    pub contacts: Vec<Contact>,
    pub groups: Vec<Group>,
    pub settings: Settings,
    /// Our own identity replacements, oldest first: what friends who knew an older
    /// identity need to follow us to the current one.
    pub moves: Vec<crate::proto::MoveNotice>,
    /// Friends and group members still to be told about our latest identity.
    pub move_pending: Vec<EndpointId>,
    /// Group members still to hear about removals from a group.
    pub group_news_pending: Vec<PendingNews>,
    /// Pictures chosen for people (only on this computer).
    pub pictures: Vec<Picture>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingNews {
    #[serde(with = "hex32")]
    pub room: RoomId,
    pub to: EndpointId,
}

impl Store {
    fn path() -> PathBuf {
        config_dir().join("state.json")
    }

    pub fn load() -> Self {
        let mut st: Self = match fs::read(Self::path()) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("state.json unreadable ({e}); starting fresh");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        for g in &mut st.groups {
            g.settle_owner();
        }
        st
    }

    pub fn save(&self) {
        let dir = config_dir();
        let res = (|| -> Result<()> {
            fs::create_dir_all(&dir)?;
            let tmp = dir.join("state.json.tmp");
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .private()
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self)?)?;
            f.sync_all()?;
            fs::rename(tmp, Self::path())?;
            Ok(())
        })();
        if let Err(e) = res {
            tracing::error!("saving state failed: {e:#}");
        }
    }

    /// Someone (or we) now goes by a new identity: rewrite them in contacts and groups,
    /// keeping names and when they were added. Returns whether anything changed.
    pub fn replace_id(&mut self, old: &EndpointId, new: &EndpointId) -> bool {
        let mut changed = false;
        // A picture chosen for them follows (unless the new identity has one already).
        if !self.pictures.iter().any(|p| &p.id == new) {
            if let Some(p) = self.pictures.iter_mut().find(|p| &p.id == old) {
                p.id = *new;
                if p.kind == PictureKind::Image {
                    let _ = fs::rename(picture_path(old), picture_path(new));
                }
                changed = true;
            }
        }
        if self.contacts.iter().any(|c| &c.id == new) {
            let before = self.contacts.len();
            self.contacts.retain(|c| &c.id != old);
            changed |= self.contacts.len() != before;
        } else if let Some(c) = self.contacts.iter_mut().find(|c| &c.id == old) {
            c.id = *new;
            changed = true;
        }
        for g in &mut self.groups {
            if g.owner.as_ref() == Some(old) {
                g.owner = Some(*new);
                changed = true;
            }
            if g.members.iter().any(|m| &m.id == new) {
                let before = g.members.len();
                g.members.retain(|m| &m.id != old);
                changed |= g.members.len() != before;
            } else if let Some(m) = g.members.iter_mut().find(|m| &m.id == old) {
                m.id = *new;
                changed = true;
            }
        }
        changed
    }

    /// Everyone we know: contacts and the members of our groups (not us).
    pub fn known_peers(&self, me: &EndpointId) -> Vec<EndpointId> {
        let mut v: Vec<EndpointId> = self.contacts.iter().map(|c| c.id).collect();
        for m in self.groups.iter().flat_map(|g| g.members.iter()) {
            if !v.contains(&m.id) {
                v.push(m.id);
            }
        }
        v.retain(|id| id != me);
        v
    }

    pub fn picture(&self, id: &EndpointId) -> Option<&Picture> {
        self.pictures.iter().find(|p| &p.id == id)
    }

    pub fn contact(&self, id: &EndpointId) -> Option<&Contact> {
        self.contacts.iter().find(|c| &c.id == id)
    }

    pub fn group_by_room(&self, room: &RoomId) -> Option<&Group> {
        self.groups.iter().find(|g| &g.room_id() == room)
    }

    pub fn group_by_room_mut(&mut self, room: &RoomId) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| &g.room_id() == room)
    }

    /// Best display name for a peer: our contact name, then any group member name.
    pub fn name_for(&self, id: &EndpointId) -> Option<String> {
        if let Some(c) = self.contact(id) {
            return Some(c.name.clone());
        }
        self.groups
            .iter()
            .flat_map(|g| g.members.iter())
            .find(|m| &m.id == id && !m.name.is_empty())
            .map(|m| m.name.clone())
    }
}

/// What a share code decodes to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum ShareCode {
    #[serde(rename = "c")]
    Contact { n: String, k: EndpointId },
    #[serde(rename = "g")]
    Group {
        n: String,
        #[serde(with = "hex32")]
        s: [u8; 32],
        #[serde(default)]
        m: Vec<Member>,
        /// The owner (0.8.0+; older codes: the first member).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        o: Option<EndpointId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        r: Vec<Removal>,
    },
}

impl ShareCode {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("serialize code");
        format!("{CODE_PREFIX}{}", B64.encode(json))
    }

    /// Accepts the code with surrounding whitespace/line breaks, as chat apps
    /// tend to wrap long strings.
    pub fn decode(text: &str) -> Result<Self> {
        let cleaned: String = text.split_whitespace().collect();
        let Some(start) = cleaned.find(CODE_PREFIX) else {
            bail!("That doesn't look like a Parley code (it should start with \"{CODE_PREFIX}\").");
        };
        let body = &cleaned[start + CODE_PREFIX.len()..];
        let body: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let json = B64.decode(body.as_bytes()).context("The code is damaged or incomplete.")?;
        serde_json::from_slice(&json).context("The code is damaged or from a newer version.")
    }

    pub fn for_group(g: &Group) -> Self {
        ShareCode::Group { n: g.name.clone(), s: g.secret, m: g.members.clone(), o: g.owner, r: g.removed.clone() }
    }
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let v = hex::decode(s).map_err(serde::de::Error::custom)?;
        v.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_removes_people_and_they_stay_out() {
        let (owner, bob, mallory) = (SecretKey::generate(), SecretKey::generate(), SecretKey::generate());
        let m = |k: &SecretKey, n: &str| Member { id: k.public(), name: n.into() };
        let mut g = Group::new("trip".into(), vec![m(&owner, "o"), m(&bob, "b"), m(&mallory, "m")]);
        let room = g.room_id();
        assert_eq!(g.owner, Some(owner.public()));

        // A member can't remove anyone, nor can a removal of the owner count.
        assert!(!g.merge_removals(&[Removal::sign(&mallory, &room, bob.public(), 1)]));
        assert!(!g.merge_removals(&[Removal::sign(&owner, &room, owner.public(), 1)]));
        // Signed for another group, or tampered with: ignored.
        assert!(!g.merge_removals(&[Removal::sign(&owner, &[0; 32], mallory.public(), 1)]));
        let mut bent = Removal::sign(&owner, &room, bob.public(), 1);
        bent.id = mallory.public();
        assert!(!g.merge_removals(&[bent]));
        assert_eq!(g.members.len(), 3);

        // The owner can, and the removed person can't come back in through a roster.
        let r = Removal::sign(&owner, &room, mallory.public(), 2);
        assert!(g.merge_removals(std::slice::from_ref(&r)));
        assert!(!g.members.iter().any(|x| x.id == mallory.public()));
        assert!(!g.add_member(mallory.public(), "m"));
        assert!(!g.merge_removals(&[r.clone()]), "already known");

        // The code carries the owner and removals to people who join later.
        let ShareCode::Group { n, s, m: mem, o, r: rem } = ShareCode::decode(&ShareCode::for_group(&g).encode()).unwrap() else { panic!() };
        let joined = Group::learned(n, s, mem, o, &rem);
        assert_eq!((joined.owner, joined.removed.len()), (Some(owner.public()), 1));
        assert!(joined.is_removed(&mallory.public()));

        // A group saved before owners were recorded: its first member (the creator).
        let mut old = Group { owner: None, removed: Vec::new(), ..Group::new("old".into(), vec![m(&bob, "b"), m(&owner, "o")]) };
        old.settle_owner();
        assert_eq!(old.owner, Some(bob.public()));
    }

    #[test]
    fn code_roundtrip_survives_wrapping() {
        let k = SecretKey::generate().public();
        let c = ShareCode::Contact { n: "Alice".into(), k };
        let enc = c.encode();
        let wrapped = format!("hey add me:\n {} \n", enc.replace("parley:", "parley:\n"));
        assert_eq!(ShareCode::decode(&wrapped).unwrap(), c);
    }

    #[test]
    fn proof_binds_both_ids() {
        let s = random_bytes32();
        let a = SecretKey::generate().public();
        let b = SecretKey::generate().public();
        let p = join_proof(&s, &a, &b);
        assert!(verify_proof(&s, &a, &b, &p));
        assert!(!verify_proof(&s, &b, &a, &p));
        assert!(!verify_proof(&random_bytes32(), &a, &b, &p));
    }
}

#[cfg(test)]
mod screen_tests {
    #[test]
    fn bitrates() {
        assert_eq!(super::screen_kbps(1080, 30), 4500);
        assert!((6500..=7500).contains(&super::screen_kbps(1080, 60)));
        assert!(super::screen_kbps(720, 24) < super::screen_kbps(720, 30));
        assert!(super::screen_kbps(480, 30) < super::screen_kbps(720, 30));
        assert!(super::screen_kbps(1440, 60) > super::screen_kbps(1080, 60));
    }
}
