//! Wire protocol. Every peer pair shares one QUIC connection (TLS 1.3,
//! authenticated by the endpoints' ed25519 keys) carrying:
//!
//! * one bidirectional control stream with length-prefixed JSON [`Ctrl`] messages,
//! * unreliable datagrams `[track u8][RTP packet]` for Opus audio,
//! * one unidirectional stream per video GOP: `[track u8]` then frames of
//!   `[len u32][pts_us u64][flags u8][H.264 access unit]`. A new GOP opens a
//!   new stream, so a receiver can drop a stalled GOP without corrupting the
//!   decoder and a congested sender can reset it.

use anyhow::{Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::store::{Member, RoomId};

pub const ALPN: &[u8] = b"parley/1";
pub const PROTO_VERSION: u32 = 1;

/// Media tracks. Audio tracks travel as datagrams, video as streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Track {
    Mic = 0,
    ScreenAudio = 1,
    Camera = 2,
    Screen = 3,
}

impl Track {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Mic,
            1 => Self::ScreenAudio,
            2 => Self::Camera,
            3 => Self::Screen,
            _ => return None,
        })
    }
    pub fn is_video(self) -> bool {
        matches!(self, Self::Camera | Self::Screen)
    }
    /// Index into per-video-track arrays.
    pub fn video_index(self) -> usize {
        match self {
            Self::Camera => 0,
            Self::Screen => 1,
            _ => panic!("not a video track"),
        }
    }
}

pub const VIDEO_TRACKS: [Track; 2] = [Track::Camera, Track::Screen];

pub const FRAME_FLAG_KEY: u8 = 1;
pub const FRAME_HEADER_LEN: usize = 13;
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// Close codes for connections.
pub const CLOSE_LEAVE: u32 = 0;
pub const CLOSE_DUPLICATE: u32 = 1;
pub const CLOSE_DECLINE: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomKey {
    /// One-to-one call. Both sides use the *other* peer as the key locally,
    /// so on the wire this carries no data.
    Direct(EndpointId),
    Group(#[serde(with = "hex_room")] RoomId),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaState {
    pub mic_muted: bool,
    pub deafened: bool,
    pub camera: bool,
    pub screen: bool,
    pub screen_audio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclineReason {
    Declined,
    Busy,
    Duplicate,
    NotAllowed,
    Unknown,
}

/// Group invitation: lets a contact who has never seen the group's code
/// accept a ring for it. Only ever sent to an authenticated peer we chose to dial.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    pub name: String,
    #[serde(with = "hex_room")]
    pub secret: [u8; 32],
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Ctrl {
    Hello {
        version: u32,
        name: String,
        /// None = direct call.
        #[serde(default, with = "opt_hex_room")]
        room: Option<RoomId>,
        #[serde(default, with = "opt_hex_room")]
        proof: Option<[u8; 32]>,
        #[serde(default)]
        invite: Option<Invite>,
        state: MediaState,
    },
    Ringing,
    Accept {
        name: String,
        #[serde(default, with = "opt_hex_room")]
        proof: Option<[u8; 32]>,
        state: MediaState,
    },
    Decline(DeclineReason),
    State(MediaState),
    Keyframe(Track),
    /// The receiver started/stopped watching this track; the sender pauses it for them.
    Watch { track: Track, on: bool },
    /// The sender's current call participants, so newcomers can complete the mesh.
    Roster(Vec<Member>),
    Leave,
}

pub async fn write_ctrl(s: &mut iroh::endpoint::SendStream, msg: &Ctrl) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut buf = Vec::with_capacity(body.len() + 4);
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    s.write_all(&buf).await?;
    Ok(())
}

pub async fn read_ctrl(r: &mut iroh::endpoint::RecvStream) -> Result<Ctrl> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 256 * 1024 {
        bail!("control message too large");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    let mut msg: Ctrl = serde_json::from_slice(&body)?;
    msg.clean_names();
    Ok(msg)
}

/// Longest display name we accept from someone else.
pub const MAX_NAME_CHARS: usize = 40;

/// Makes a name received from another person safe to show: no control characters
/// or line breaks, whitespace collapsed, and at most [`MAX_NAME_CHARS`] characters.
pub fn clean_name(s: &str) -> String {
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect();
    let joined = words.join(" ");
    let mut out: String = joined.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    if joined.chars().count() > MAX_NAME_CHARS {
        out.push('…');
    }
    out
}

impl Ctrl {
    /// Applies [`clean_name`] to every name carried by the message.
    pub fn clean_names(&mut self) {
        let fix_members = |m: &mut Vec<Member>| {
            m.truncate(16);
            for x in m.iter_mut() {
                x.name = clean_name(&x.name);
            }
        };
        match self {
            Ctrl::Hello { name, invite, .. } => {
                *name = clean_name(name);
                if let Some(i) = invite {
                    i.name = clean_name(&i.name);
                    fix_members(&mut i.members);
                }
            }
            Ctrl::Accept { name, .. } => *name = clean_name(name),
            Ctrl::Roster(m) => fix_members(m),
            _ => {}
        }
    }
}

pub fn encode_frame_header(len: usize, pts_us: u64, key: bool) -> [u8; FRAME_HEADER_LEN] {
    let mut h = [0u8; FRAME_HEADER_LEN];
    h[0..4].copy_from_slice(&(len as u32).to_be_bytes());
    h[4..12].copy_from_slice(&pts_us.to_be_bytes());
    h[12] = if key { FRAME_FLAG_KEY } else { 0 };
    h
}

pub fn decode_frame_header(h: &[u8; FRAME_HEADER_LEN]) -> (usize, u64, bool) {
    let len = u32::from_be_bytes(h[0..4].try_into().unwrap()) as usize;
    let pts = u64::from_be_bytes(h[4..12].try_into().unwrap());
    (len, pts, h[12] & FRAME_FLAG_KEY != 0)
}

mod hex_room {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(s)
            .map_err(serde::de::Error::custom)?
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod opt_hex_room {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_some(&hex::encode(v)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let Some(s) = Option::<String>::deserialize(d)? else { return Ok(None) };
        let v = hex::decode(s).map_err(serde::de::Error::custom)?;
        Ok(Some(v.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))?))
    }
}

#[cfg(test)]
mod name_tests {
    use super::clean_name;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Alice\n\tSmith\u{7}  "), "Alice Smith");
        assert_eq!(clean_name(&"x".repeat(100)).chars().count(), super::MAX_NAME_CHARS + 1);
        assert_eq!(clean_name("<b>Bob</b>"), "<b>Bob</b>"); // escaping is the UI's job
    }
}
