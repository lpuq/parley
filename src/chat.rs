//! Text chat storage: one JSON-lines history file per conversation, plus an outbox of
//! messages not yet delivered to every recipient (there's no server to hold them, so
//! they wait here until the recipient is reachable).

use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::PathBuf,
};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::{
    proto::WireMsg,
    store::{RoomId, data_dir},
};

/// History kept in memory per conversation (older lines stay on disk).
const KEEP: usize = 500;

/// A conversation: with one contact, or a group's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Conv {
    Direct(EndpointId),
    Group(RoomId),
}

impl Conv {
    /// Stable text key (file names, notification actions): `d-<key>` / `g-<room>`.
    pub fn key(&self) -> String {
        match self {
            Conv::Direct(id) => format!("d-{id}"),
            Conv::Group(r) => format!("g-{}", hex::encode(r)),
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        let (kind, rest) = s.split_once('-')?;
        match kind {
            "d" => rest.parse().ok().map(Conv::Direct),
            "g" => hex::decode(rest).ok()?.try_into().ok().map(Conv::Group),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: EndpointId,
    /// Sender's clock, Unix milliseconds.
    pub ts: u64,
    pub text: String,
}

/// One of our messages still owed to some recipients.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub conv_key: String,
    pub msg: WireMsg,
    pub to: Vec<EndpointId>,
    /// Someone refused it (they don't have us as a contact).
    #[serde(default)]
    pub refused: Vec<EndpointId>,
}

pub struct ChatStore {
    dir: PathBuf,
    history: HashMap<Conv, Vec<Message>>,
    ids: HashMap<Conv, HashSet<String>>,
    pub outbox: Vec<Pending>,
    pub unread: HashMap<Conv, u32>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn new_id() -> String {
    hex::encode(&crate::store::random_bytes32()[..12])
}

impl ChatStore {
    pub fn open() -> Self {
        let dir = data_dir().join("chats");
        let _ = std::fs::create_dir_all(&dir);
        let read = |name: &str| std::fs::read(dir.join(name)).ok();
        let outbox = read("outbox.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let unread: HashMap<String, u32> = read("unread.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let unread = unread.into_iter().filter_map(|(k, n)| Some((Conv::from_key(&k)?, n))).collect();
        Self { dir, history: HashMap::new(), ids: HashMap::new(), outbox, unread }
    }

    fn file(&self, conv: &Conv) -> PathBuf {
        self.dir.join(format!("{}.jsonl", conv.key()))
    }

    /// The conversation's recent messages, oldest first (loaded on first use).
    pub fn messages(&mut self, conv: &Conv) -> &Vec<Message> {
        if !self.history.contains_key(conv) {
            let mut v: Vec<Message> = std::fs::read_to_string(self.file(conv))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if v.len() > KEEP {
                v.drain(..v.len() - KEEP);
            }
            self.ids.insert(*conv, v.iter().map(|m| m.id.clone()).collect());
            self.history.insert(*conv, v);
        }
        &self.history[conv]
    }

    /// Adds a message unless we already have it. Returns whether it was new.
    pub fn add(&mut self, conv: &Conv, msg: Message) -> bool {
        self.messages(conv);
        if !self.ids.get_mut(conv).unwrap().insert(msg.id.clone()) {
            return false;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.file(conv)) {
            if let Ok(line) = serde_json::to_string(&msg) {
                let _ = writeln!(f, "{line}");
            }
        }
        let v = self.history.get_mut(conv).unwrap();
        v.push(msg);
        if v.len() > KEEP {
            v.remove(0);
        }
        true
    }

    /// Our messages in `conv` that haven't reached everyone yet, and whether any
    /// recipient refused them.
    pub fn pending_in(&self, conv: &Conv) -> HashMap<String, bool> {
        let key = conv.key();
        self.outbox.iter().filter(|p| p.conv_key == key).map(|p| (p.msg.id.clone(), !p.refused.is_empty())).collect()
    }

    /// When the oldest message in `conv` still waiting for delivery (not refused) was
    /// sent, if any.
    pub fn oldest_waiting(&self, conv: &Conv) -> Option<u64> {
        let key = conv.key();
        self.outbox.iter().filter(|p| p.conv_key == key && p.refused.is_empty()).map(|p| p.msg.ts).min()
    }

    /// What we still owe `peer`: (group, messages), grouped per conversation.
    pub fn owed_to(&self, peer: &EndpointId) -> Vec<(Option<RoomId>, Vec<WireMsg>)> {
        let mut by_conv: Vec<(Option<RoomId>, Vec<WireMsg>)> = Vec::new();
        for p in self.outbox.iter().filter(|p| p.to.contains(peer)) {
            let group = match Conv::from_key(&p.conv_key) {
                Some(Conv::Group(r)) => Some(r),
                _ => None,
            };
            match by_conv.iter_mut().find(|(g, _)| *g == group) {
                Some((_, v)) => v.push(p.msg.clone()),
                None => by_conv.push((group, vec![p.msg.clone()])),
            }
        }
        by_conv
    }

    /// `peer` confirmed these; drops messages everyone now has.
    pub fn delivered(&mut self, peer: &EndpointId, ids: &[String]) {
        for p in self.outbox.iter_mut().filter(|p| ids.contains(&p.msg.id)) {
            p.to.retain(|x| x != peer);
            p.refused.retain(|x| x != peer);
        }
        self.outbox.retain(|p| !p.to.is_empty());
        self.save_outbox();
    }

    /// `peer` won't take our messages (we're not their contact).
    pub fn refused(&mut self, peer: &EndpointId) {
        for p in self.outbox.iter_mut().filter(|p| p.to.contains(peer)) {
            if !p.refused.contains(peer) {
                p.refused.push(*peer);
            }
        }
        self.save_outbox();
    }

    pub fn queue(&mut self, conv: &Conv, msg: WireMsg, to: Vec<EndpointId>) {
        if to.is_empty() {
            return;
        }
        self.outbox.push(Pending { conv_key: conv.key(), msg, to, refused: Vec::new() });
        self.save_outbox();
    }

    fn save_outbox(&self) {
        if let Ok(b) = serde_json::to_vec(&self.outbox) {
            let _ = std::fs::write(self.dir.join("outbox.json"), b);
        }
    }

    pub fn set_unread(&mut self, conv: &Conv, n: u32) {
        if n == 0 {
            self.unread.remove(conv);
        } else {
            self.unread.insert(*conv, n);
        }
        let m: HashMap<String, u32> = self.unread.iter().map(|(c, n)| (c.key(), *n)).collect();
        if let Ok(b) = serde_json::to_vec(&m) {
            let _ = std::fs::write(self.dir.join("unread.json"), b);
        }
    }

    pub fn unread(&self, conv: &Conv) -> u32 {
        self.unread.get(conv).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conv_keys_round_trip() {
        let id = iroh::SecretKey::generate().public();
        for c in [Conv::Direct(id), Conv::Group([9; 32])] {
            assert_eq!(Conv::from_key(&c.key()), Some(c));
        }
        assert_eq!(Conv::from_key("x-1"), None);
    }
}
