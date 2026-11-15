//! Persistent state: identity key, profile, contacts, groups and settings.
//! Also the share-code format used to exchange contacts and groups.

use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, Mac};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CODE_PREFIX: &str = "parley:";

fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join(app_dir_name())
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join(app_dir_name())
}

/// Diagnostics log (Flatpak: ~/.var/app/io.github.parley.Parley/.local/state/parley/parley.log).
pub fn log_path() -> PathBuf {
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
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(&key.to_bytes())?;
    Ok(key)
}

pub fn random_bytes32() -> [u8; 32] {
    // SecretKey::generate draws from the OS CSPRNG.
    SecretKey::generate().to_bytes()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub id: EndpointId,
    pub name: String,
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
}

impl Group {
    pub fn new(name: String, members: Vec<Member>) -> Self {
        Self { name, secret: random_bytes32(), members }
    }
    pub fn room_id(&self) -> RoomId {
        room_id_for(&self.secret)
    }
    pub fn add_member(&mut self, id: EndpointId, name: &str) -> bool {
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
    pub allow_unknown_callers: bool,
    /// Remembered for the Flatpak case, where the portal owns the real setting.
    pub autostart: bool,
    /// We told the user once that closing the window keeps Parley running.
    pub background_hint_shown: bool,
    /// Allow "system default" to pick a Bluetooth headset mic (switches earbuds to headset mode).
    pub allow_bluetooth_mic: bool,
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
            allow_unknown_callers: true,
            autostart: false,
            background_hint_shown: false,
            allow_bluetooth_mic: false,
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
}

impl Store {
    fn path() -> PathBuf {
        config_dir().join("state.json")
    }

    pub fn load() -> Self {
        match fs::read(Self::path()) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("state.json unreadable ({e}); starting fresh");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
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
                .mode(0o600)
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
        ShareCode::Group { n: g.name.clone(), s: g.secret, m: g.members.clone() }
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
