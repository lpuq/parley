//! Network engine. One actor task owns all call state; per-connection tasks
//! feed it events. Calls are a full mesh (fine for the 2–3 people we target):
//! every participant holds one encrypted QUIC connection to every other.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};

use crate::{
    hub::{Hub, PeerOut, VIDEO_QUEUE, VideoFrame},
    proto::*,
    store::{Contact, Group, Member, RoomId, Status, join_proof, room_id_for, verify_proof},
};

const RING_TIMEOUT: Duration = Duration::from_secs(45);
const DIAL_TIMEOUT: Duration = Duration::from_secs(25);
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);
const STALE_FRAME: Duration = Duration::from_millis(500);
/// How often contacts' online status is checked.
const PRESENCE_INTERVAL: Duration = Duration::from_secs(30);
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum NetCmd {
    Directory {
        my_name: String,
        contacts: Vec<Contact>,
        groups: Vec<Group>,
        allow_unknown: bool,
    },
    CallDirect(EndpointId),
    JoinGroup(RoomId),
    Accept(RoomKey),
    Decline(RoomKey),
    Hangup,
    MediaState(MediaState),
    /// We just started decoding this peer's track: ask them for a fresh keyframe.
    RequestKeyframe(EndpointId, Track),
    /// Start/stop watching a peer's screen share (video and its audio).
    WatchScreen(EndpointId, bool),
    /// Our status as shown to contacts.
    SetStatus(Status),
    /// Deliver chat messages to `to` (a direct message when `group` is None).
    SendChat { to: EndpointId, group: Option<RoomId>, name: String, msgs: Vec<WireMsg> },
    /// Our identity moves (oldest first), and who still has to be told.
    Moves { chain: Vec<MoveNotice>, pending: Vec<EndpointId> },
    /// Tell `to` who the group's owner removed.
    GroupNews { to: EndpointId, news: GroupNews },
    /// Test hook: drop every connection abruptly, as a crash or network loss would.
    #[cfg(test)]
    SimulateDropout,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Ready {
        #[allow(dead_code)]
        id: EndpointId,
    },
    Incoming { room: RoomKey, from: EndpointId, name: String, group_name: Option<String> },
    IncomingGone { room: RoomKey },
    OutgoingRinging,
    CallActive { room: RoomKey },
    CallEnded { reason: String },
    PeerJoined { id: EndpointId, name: String, state: MediaState, app: String },
    PeerLeft { id: EndpointId },
    PeerReconnecting { id: EndpointId },
    PeerState { id: EndpointId, state: MediaState },
    PeerLink { id: EndpointId, direct: bool, rtt_ms: u32 },
    /// A group was learned from an invite or gained members; the UI merges and saves it.
    GroupUpdate { group: Group },
    MissedCall { from: EndpointId, name: String },
    /// Chat messages arrived (already acknowledged to the sender).
    ChatReceived { from: EndpointId, name: String, group: Option<RoomId>, msgs: Vec<WireMsg> },
    /// A delivery finished: `ids` reached `to` (empty if it failed or was refused).
    ChatDelivered { to: EndpointId, ids: Vec<String>, refused: bool },
    /// A contact's online status (and group call, if any) changed.
    Presence { id: EndpointId, presence: Presence, call: Option<CallPresence>, app: Option<String> },
    /// A friend or group member replaced their identity (proven by the old key):
    /// `old` is every identity they went by since the one we knew, oldest first.
    IdentityMoved { old: Vec<EndpointId>, new: EndpointId },
    /// `to` was told about our new identity (`known`: and followed it).
    MoveDelivered { to: EndpointId, known: bool },
    /// A group member passed on the owner's removals (already checked).
    GroupNews { from: EndpointId, news: GroupNews },
    /// Our group news reached `to` (or not).
    GroupNewsDelivered { to: EndpointId, room: RoomId, ok: bool },
    Notice(String),
}

/// Commands for the network engine. Clones share it, so [`NetHandle::replace`] (a
/// new identity means a new endpoint) switches everyone over at once.
#[derive(Clone)]
pub struct NetHandle {
    tx: Arc<std::sync::Mutex<mpsc::UnboundedSender<NetCmd>>>,
}

impl NetHandle {
    pub fn send(&self, cmd: NetCmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }

    /// Hands over to another engine; the current one hangs up and shuts down.
    pub fn replace(&self, other: NetHandle) {
        let tx = other.tx.lock().unwrap().clone();
        *self.tx.lock().unwrap() = tx;
    }
}

pub async fn start(
    secret: SecretKey,
    hub: Arc<Hub>,
    ui: async_channel::Sender<UiEvent>,
) -> Result<NetHandle> {
    let transport = iroh::endpoint::QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(2))
        .max_idle_timeout(Some(Duration::from_secs(12).try_into()?))
        .max_concurrent_uni_streams(128u32.into())
        .stream_receive_window((8u32 * 1024 * 1024).into())
        .send_window(32 * 1024 * 1024)
        // Audio datagrams wait here when the link is full. The default (1 MB) is most of
        // a minute of sound: keep well under a second, so the oldest go, not the voice.
        .datagram_send_buffer_size(12 * 1024)
        .build();
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec(), PRESENCE_ALPN.to_vec(), CHAT_ALPN.to_vec(), MOVE_ALPN.to_vec(), GROUP_ALPN.to_vec()])
        .transport_config(transport)
        .bind()
        .await
        .context("starting network endpoint")?;
    let me = ep.id();
    info!("endpoint id {me}");
    let _ = ui.try_send(UiEvent::Ready { id: me });

    let (tx, cmd_rx) = mpsc::unbounded_channel();
    let (itx, irx) = mpsc::unbounded_channel();

    // Accept loop.
    {
        let ep = ep.clone();
        let itx = itx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let itx = itx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_incoming(incoming, itx).await {
                        debug!("incoming connection failed: {e:#}");
                    }
                });
            }
        });
    }

    let engine = Engine {
        ep,
        me,
        hub,
        ui,
        itx,
        my_name: String::new(),
        contacts: HashMap::new(),
        groups: HashMap::new(),
        allow_unknown: true,
        sessions: HashMap::new(),
        next_sid: 1,
        call: None,
        pending: HashMap::new(),
        my_state: MediaState::default(),
        status: Status::Online,
        presence: HashMap::new(),
        announced: None,
        probing: std::collections::HashSet::new(),
        reprobe: std::collections::HashSet::new(),
        last_probe: None,
        probed_by: HashMap::new(),
        moves: Vec::new(),
        move_pending: std::collections::HashSet::new(),
        move_tried: HashMap::new(),
    };
    tokio::spawn(engine.run(cmd_rx, irx));
    Ok(NetHandle { tx: Arc::new(std::sync::Mutex::new(tx)) })
}

async fn handle_incoming(
    incoming: iroh::endpoint::Incoming,
    itx: mpsc::UnboundedSender<Internal>,
) -> Result<()> {
    let conn = incoming.accept()?.await?;
    if conn.alpn() == PRESENCE_ALPN {
        let (send, mut recv) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi())
            .await
            .context("presence timeout")??;
        let theirs: PresenceMsg = tokio::time::timeout(Duration::from_secs(5), read_small(&mut recv))
            .await
            .context("presence timeout")??;
        let _ = itx.send(Internal::PresenceProbe { conn, send, theirs });
        return Ok(());
    }
    if conn.alpn() == CHAT_ALPN {
        let (send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
            .await
            .context("chat timeout")??;
        let mut env: ChatEnvelope = tokio::time::timeout(Duration::from_secs(10), read_limited(&mut recv, 512 * 1024))
            .await
            .context("chat timeout")??;
        env.clean();
        let _ = itx.send(Internal::ChatIn { conn, send, env });
        return Ok(());
    }
    if conn.alpn() == MOVE_ALPN {
        let (send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
            .await
            .context("move timeout")??;
        let msg: MoveMsg = tokio::time::timeout(Duration::from_secs(10), read_limited(&mut recv, 64 * 1024))
            .await
            .context("move timeout")??;
        let _ = itx.send(Internal::MoveIn { conn, send, msg });
        return Ok(());
    }
    if conn.alpn() == GROUP_ALPN {
        let (send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
            .await
            .context("group news timeout")??;
        let news: GroupNews = tokio::time::timeout(Duration::from_secs(10), read_limited(&mut recv, 256 * 1024))
            .await
            .context("group news timeout")??;
        let _ = itx.send(Internal::GroupIn { conn, send, news });
        return Ok(());
    }
    if conn.alpn() != ALPN {
        conn.close(CLOSE_DECLINE.into(), b"alpn");
        return Ok(());
    }
    let (send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
        .await
        .context("hello timeout")??;
    let hello = tokio::time::timeout(Duration::from_secs(10), read_ctrl(&mut recv))
        .await
        .context("hello timeout")??;
    if !matches!(hello, Ctrl::Hello { .. }) {
        return Err(anyhow!("expected hello"));
    }
    let _ = itx.send(Internal::Incoming { conn, send, recv, hello });
    Ok(())
}

type Sid = u64;

enum Internal {
    Incoming { conn: Connection, send: SendStream, recv: RecvStream, hello: Ctrl },
    DialDone { sid: Sid, result: Result<(Connection, SendStream, RecvStream)> },
    Ctrl { sid: Sid, msg: Ctrl },
    Closed { sid: Sid, code: Option<u32> },
    RingTimeout { room: RoomKey },
    Tick,
    /// A contact checked our status (and told us theirs).
    PresenceProbe { conn: Connection, send: SendStream, theirs: PresenceMsg },
    /// Chat messages arrived from someone.
    ChatIn { conn: Connection, send: SendStream, env: ChatEnvelope },
    /// Our check of a contact finished; None = unreachable.
    PresenceResult { peer: EndpointId, presence: Option<PresenceMsg> },
    /// Someone says they replaced their identity.
    MoveIn { conn: Connection, send: SendStream, msg: MoveMsg },
    /// Telling `peer` about our new identity finished; None = unreachable (retry).
    MoveResult { peer: EndpointId, known: Option<bool> },
    /// A member passed on group news.
    GroupIn { conn: Connection, send: SendStream, news: GroupNews },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dialing,
    AwaitingAnswer,
    IncomingPending,
    Active,
}

struct Session {
    peer: EndpointId,
    room: RoomKey,
    phase: Phase,
    conn: Option<Connection>,
    ctrl: Option<mpsc::UnboundedSender<Ctrl>>,
    peer_name: String,
    /// Their Parley version, from Hello/Accept (empty before 0.4.3).
    peer_app: String,
    peer_state: MediaState,
    invite: Option<Invite>,
    media: Vec<JoinHandle<()>>,
    /// Ended on purpose (hangup/duplicate/decline); don't treat closure as a dropout.
    retired: bool,
}

impl Session {
    fn send(&self, msg: Ctrl) {
        if let Some(c) = &self.ctrl {
            let _ = c.send(msg);
        }
    }
    fn close(&mut self, code: u32) {
        self.retired = true;
        if let Some(c) = &self.conn {
            let conn = c.clone();
            // Let queued control messages (e.g. Leave/Decline) flush first.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                conn.close(code.into(), b"");
            });
        }
        for t in self.media.drain(..) {
            t.abort();
        }
    }
}

struct Call {
    room: RoomKey,
    /// Direct call we placed that hasn't been answered yet.
    outgoing: bool,
    peers: HashMap<EndpointId, Sid>,
    /// Peers that dropped unexpectedly and when; we keep redialing for a while.
    lost: HashMap<EndpointId, Instant>,
    /// Peers that hung up recently; stale rosters must not pull them back in.
    left: HashMap<EndpointId, Instant>,
    started: Instant,
}

struct Engine {
    ep: Endpoint,
    me: EndpointId,
    hub: Arc<Hub>,
    ui: async_channel::Sender<UiEvent>,
    itx: mpsc::UnboundedSender<Internal>,
    my_name: String,
    contacts: HashMap<EndpointId, String>,
    groups: HashMap<RoomId, Group>,
    allow_unknown: bool,
    sessions: HashMap<Sid, Session>,
    next_sid: Sid,
    call: Option<Call>,
    /// Unanswered incoming rings by room.
    pending: HashMap<RoomKey, Vec<Sid>>,
    my_state: MediaState,
    /// Our status, and what we last learned about each contact's.
    status: Status,
    presence: HashMap<EndpointId, PresenceMsg>,
    /// What we last told everyone (status, group call, media state): re-announce on change.
    announced: Option<(Status, Option<RoomKey>, MediaState)>,
    probing: std::collections::HashSet<EndpointId>,
    /// Checks to redo once the one in flight finishes (our status changed meanwhile).
    reprobe: std::collections::HashSet<EndpointId>,
    last_probe: Option<Instant>,
    /// When each peer last checked our status (rate limit).
    probed_by: HashMap<EndpointId, Instant>,
    /// Our identity moves, the people still to tell, and when we last tried each.
    moves: Vec<MoveNotice>,
    move_pending: std::collections::HashSet<EndpointId>,
    move_tried: HashMap<EndpointId, Instant>,
}

impl Engine {
    async fn run(
        mut self,
        mut cmds: mpsc::UnboundedReceiver<NetCmd>,
        mut internal: mpsc::UnboundedReceiver<Internal>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(c) => self.on_cmd(c),
                    None => break,
                },
                Some(ev) = internal.recv() => self.on_internal(ev),
                _ = tick.tick() => self.on_internal(Internal::Tick),
            }
        }
        self.hangup();
        self.ep.close().await;
    }

    fn emit(&self, ev: UiEvent) {
        let _ = self.ui.try_send(ev);
    }

    fn name_of(&self, id: &EndpointId) -> String {
        if let Some(n) = self.contacts.get(id) {
            return n.clone();
        }
        for g in self.groups.values() {
            if let Some(m) = g.members.iter().find(|m| &m.id == id) {
                if !m.name.is_empty() {
                    return m.name.clone();
                }
            }
        }
        id.fmt_short().to_string()
    }

    fn secret_for(&self, room: &RoomId) -> Option<[u8; 32]> {
        self.groups.get(room).map(|g| g.secret)
    }

    // ---------- commands ----------

    fn on_cmd(&mut self, cmd: NetCmd) {
        match cmd {
            NetCmd::Directory { my_name, contacts, groups, allow_unknown } => {
                self.my_name = my_name;
                self.contacts = contacts.into_iter().map(|c| (c.id, c.name)).collect();
                self.groups = groups.into_iter().map(|g| (g.room_id(), g)).collect();
                self.allow_unknown = allow_unknown;
                let known = self.presence_peers();
                self.presence.retain(|id, _| known.contains(id));
                let new: Vec<EndpointId> = known.into_iter().filter(|id| !self.presence.contains_key(id)).collect();
                for id in new {
                    self.probe(id);
                }
            }
            NetCmd::CallDirect(peer) => {
                self.hangup();
                let room = RoomKey::Direct(peer);
                self.call = Some(Call::new(room, true));
                self.dial(peer, room);
            }
            NetCmd::JoinGroup(room_id) => {
                self.hangup();
                let room = RoomKey::Group(room_id);
                self.call = Some(Call::new(room, false));
                self.emit(UiEvent::CallActive { room });
                self.dial_group_members(room_id);
            }
            NetCmd::Accept(room) => self.accept_pending(room),
            NetCmd::Decline(room) => {
                for sid in self.pending.remove(&room).unwrap_or_default() {
                    if let Some(s) = self.sessions.get_mut(&sid) {
                        s.send(Ctrl::Decline(DeclineReason::Declined));
                        s.close(CLOSE_DECLINE);
                    }
                }
            }
            NetCmd::Hangup => {
                self.hangup();
                self.emit(UiEvent::CallEnded { reason: String::new() });
            }
            #[cfg(test)]
            NetCmd::SimulateDropout => {
                for s in self.sessions.values() {
                    if let Some(c) = &s.conn {
                        c.close(99u32.into(), b"");
                    }
                }
            }
            NetCmd::RequestKeyframe(peer, track) => {
                if let Some(sid) = self.call.as_ref().and_then(|c| c.peers.get(&peer)) {
                    if let Some(s) = self.sessions.get(sid) {
                        s.send(Ctrl::Keyframe(track));
                    }
                }
            }
            NetCmd::SendChat { to, group, name, msgs } => {
                let (ep, ui) = (self.ep.clone(), self.ui.clone());
                tokio::spawn(async move {
                    let ids: Vec<String> = msgs.iter().map(|m| m.id.clone()).collect();
                    let env = ChatEnvelope { group, name, msgs };
                    let res = tokio::time::timeout(Duration::from_secs(20), async {
                        let conn = ep.connect(EndpointAddr::from(to), CHAT_ALPN).await?;
                        let (mut send, mut recv) = conn.open_bi().await?;
                        write_small(&mut send, &env).await?;
                        let ack: ChatAck = read_limited(&mut recv, 64 * 1024).await?;
                        conn.close(0u32.into(), b"");
                        anyhow::Ok(ack)
                    })
                    .await;
                    let ev = match res {
                        Ok(Ok(ack)) => {
                            let ok: Vec<String> = ack.ids.into_iter().filter(|i| ids.contains(i)).collect();
                            UiEvent::ChatDelivered { to, ids: ok, refused: ack.refused }
                        }
                        _ => UiEvent::ChatDelivered { to, ids: Vec::new(), refused: false },
                    };
                    let _ = ui.send(ev).await;
                });
            }
            NetCmd::Moves { chain, pending } => {
                self.moves = chain;
                self.move_pending = pending.into_iter().collect();
                self.move_tried.clear();
                self.deliver_moves();
            }
            NetCmd::GroupNews { to, news } => {
                let (ep, ui, room) = (self.ep.clone(), self.ui.clone(), news.room);
                tokio::spawn(async move {
                    let res = tokio::time::timeout(Duration::from_secs(20), async {
                        let conn = ep.connect(EndpointAddr::from(to), GROUP_ALPN).await?;
                        let (mut send, mut recv) = conn.open_bi().await?;
                        write_small(&mut send, &news).await?;
                        let _: bool = read_small(&mut recv).await?;
                        conn.close(0u32.into(), b"");
                        anyhow::Ok(())
                    })
                    .await;
                    let _ = ui.send(UiEvent::GroupNewsDelivered { to, room, ok: matches!(res, Ok(Ok(()))) }).await;
                });
            }
            NetCmd::SetStatus(st) => {
                if st != self.status {
                    info!("status now {st:?}");
                    self.status = st;
                    self.on_tick(); // announces the change
                }
            }
            NetCmd::WatchScreen(peer, on) => {
                if let Some(sid) = self.call.as_ref().and_then(|c| c.peers.get(&peer)) {
                    if let Some(s) = self.sessions.get(sid) {
                        s.send(Ctrl::Watch { track: Track::Screen, on });
                        s.send(Ctrl::Watch { track: Track::ScreenAudio, on });
                    }
                }
            }
            NetCmd::MediaState(st) => {
                self.my_state = st;
                for s in self.sessions.values() {
                    if s.phase == Phase::Active {
                        s.send(Ctrl::State(st));
                    }
                }
            }
        }
    }

    fn hangup(&mut self) {
        if self.call.take().is_none() {
            return;
        }
        let in_call: Vec<Sid> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.phase != Phase::IncomingPending)
            .map(|(k, _)| *k)
            .collect();
        for sid in in_call {
            if let Some(mut s) = self.sessions.remove(&sid) {
                s.send(Ctrl::Leave);
                s.close(CLOSE_LEAVE);
            }
        }
        self.hub.clear_peers();
    }

    fn dial_group_members(&mut self, room_id: RoomId) {
        let Some(g) = self.groups.get(&room_id) else { return };
        let ids: Vec<EndpointId> =
            g.members.iter().map(|m| m.id).filter(|id| *id != self.me).collect();
        for id in ids {
            if !self.has_session_with(&id) {
                self.dial(id, RoomKey::Group(room_id));
            }
        }
    }

    fn has_session_with(&self, peer: &EndpointId) -> bool {
        self.sessions
            .values()
            .any(|s| &s.peer == peer && !s.retired && s.phase != Phase::IncomingPending)
    }

    fn accept_pending(&mut self, room: RoomKey) {
        let sids = self.pending.remove(&room).unwrap_or_default();
        let live: Vec<Sid> = sids.into_iter().filter(|s| self.sessions.contains_key(s)).collect();
        if live.is_empty() {
            self.emit(UiEvent::CallEnded { reason: "The call was no longer available.".into() });
            return;
        }
        self.hangup();
        self.call = Some(Call::new(room, false));
        // Remember a group we were invited into.
        if let RoomKey::Group(room_id) = room {
            if !self.groups.contains_key(&room_id) {
                if let Some(inv) = live.iter().find_map(|s| self.sessions[s].invite.clone()) {
                    let g = inv.group();
                    self.groups.insert(room_id, g.clone());
                    self.emit(UiEvent::GroupUpdate { group: g });
                }
            }
        }
        self.emit(UiEvent::CallActive { room });
        for sid in live {
            let proof = self.acceptor_proof(sid);
            if let Some(s) = self.sessions.get(&sid) {
                s.send(Ctrl::Accept { name: self.my_name.clone(), proof, state: self.my_state, app: APP_VERSION.into() });
            }
            self.activate(sid);
        }
        if let RoomKey::Group(room_id) = room {
            self.dial_group_members(room_id);
        }
    }

    fn acceptor_proof(&self, sid: Sid) -> Option<[u8; 32]> {
        let s = self.sessions.get(&sid)?;
        match s.room {
            RoomKey::Group(r) => self.secret_for(&r).map(|sec| join_proof(&sec, &self.me, &s.peer)),
            RoomKey::Direct(_) => None,
        }
    }

    // ---------- dialing ----------

    fn new_session(&mut self, peer: EndpointId, room: RoomKey, phase: Phase) -> Sid {
        let sid = self.next_sid;
        self.next_sid += 1;
        self.sessions.insert(
            sid,
            Session {
                peer,
                room,
                phase,
                conn: None,
                ctrl: None,
                peer_name: self.name_of(&peer),
                peer_app: String::new(),
                peer_state: MediaState::default(),
                invite: None,
                media: Vec::new(),
                retired: false,
            },
        );
        sid
    }

    fn dial(&mut self, peer: EndpointId, room: RoomKey) {
        if peer == self.me {
            return;
        }
        info!("calling {} ({})", self.name_of(&peer), peer.fmt_short());
        let sid = self.new_session(peer, room, Phase::Dialing);
        let (room_id, proof, invite) = match room {
            RoomKey::Direct(_) => (None, None, None),
            RoomKey::Group(r) => {
                let Some(g) = self.groups.get(&r) else { return };
                let inv = Invite { name: g.name.clone(), secret: g.secret, members: g.members.clone(), owner: g.owner, removed: g.removed.clone() };
                (Some(r), Some(join_proof(&g.secret, &self.me, &peer)), Some(inv))
            }
        };
        let hello = Ctrl::Hello {
            version: PROTO_VERSION,
            name: self.my_name.clone(),
            room: room_id,
            proof,
            invite,
            state: self.my_state,
            app: APP_VERSION.into(),
        };
        let ep = self.ep.clone();
        let itx = self.itx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(DIAL_TIMEOUT, async {
                let conn = ep.connect(EndpointAddr::from(peer), ALPN).await?;
                let (mut send, recv) = conn.open_bi().await?;
                write_ctrl(&mut send, &hello).await?;
                anyhow::Ok((conn, send, recv))
            })
            .await
            .map_err(|_| anyhow!("timed out"))
            .and_then(|r| r);
            let _ = itx.send(Internal::DialDone { sid, result });
        });
    }

    fn attach_conn(&mut self, sid: Sid, conn: Connection, mut send: SendStream, mut recv: RecvStream) {
        let (ctx, mut crx) = mpsc::unbounded_channel::<Ctrl>();
        tokio::spawn(async move {
            while let Some(msg) = crx.recv().await {
                if write_ctrl(&mut send, &msg).await.is_err() {
                    break;
                }
            }
        });
        let itx = self.itx.clone();
        let c2 = conn.clone();
        tokio::spawn(async move {
            loop {
                match read_ctrl(&mut recv).await {
                    Ok(msg) => {
                        let _ = itx.send(Internal::Ctrl { sid, msg });
                    }
                    Err(_) => break,
                }
            }
            // Wait for the connection to actually close to learn the reason.
            let _ = tokio::time::timeout(Duration::from_secs(15), c2.closed()).await;
            let code = close_code(&c2);
            let _ = itx.send(Internal::Closed { sid, code });
        });
        if let Some(s) = self.sessions.get_mut(&sid) {
            s.conn = Some(conn);
            s.ctrl = Some(ctx);
        }
    }

    // ---------- internal events ----------

    fn on_internal(&mut self, ev: Internal) {
        match ev {
            Internal::Incoming { conn, send, recv, hello } => self.on_incoming(conn, send, recv, hello),
            Internal::DialDone { sid, result } => self.on_dial_done(sid, result),
            Internal::Ctrl { sid, msg } => self.on_ctrl(sid, msg),
            Internal::Closed { sid, code } => self.on_closed(sid, code),
            Internal::RingTimeout { room } => {
                if let Some(sids) = self.pending.remove(&room) {
                    let mut who = None;
                    for sid in sids {
                        if let Some(mut s) = self.sessions.remove(&sid) {
                            // Keep a missed group's invite (from a contact) so it can be joined later.
                            if let (RoomKey::Group(r), Some(inv)) = (room, s.invite.take()) {
                                if !self.groups.contains_key(&r) && self.contacts.contains_key(&s.peer) {
                                    let g = inv.group();
                                    self.groups.insert(r, g.clone());
                                    self.emit(UiEvent::GroupUpdate { group: g });
                                }
                            }
                            who.get_or_insert((s.peer, s.peer_name.clone()));
                            s.send(Ctrl::Decline(DeclineReason::Declined));
                            s.close(CLOSE_DECLINE);
                        }
                    }
                    self.emit(UiEvent::IncomingGone { room });
                    if let Some((from, name)) = who {
                        self.emit(UiEvent::MissedCall { from, name });
                    }
                }
            }
            Internal::Tick => self.on_tick(),
            Internal::PresenceProbe { conn, send, theirs } => self.on_presence_probe(conn, send, theirs),
            Internal::ChatIn { conn, send, env } => self.on_chat_in(conn, send, env),
            Internal::MoveIn { conn, send, msg } => self.on_move_in(conn, send, msg),
            Internal::GroupIn { conn, mut send, news } => {
                // Only from people in the group (the removed included: they may pass on
                // their own removal); the signatures are what count.
                let peer = conn.remote_id();
                let mut learned = None;
                if let Some(g) = self.groups.get_mut(&news.room) {
                    let member = g.members.iter().any(|m| m.id == peer) || g.is_removed(&peer) || g.owner == Some(peer);
                    if member && g.merge_removals(&news.removed) {
                        learned = Some(g.removed.clone());
                    }
                }
                if let Some(removed) = learned {
                    info!("group news from {}: {} removed", peer.fmt_short(), removed.len());
                    self.emit(UiEvent::GroupNews { from: peer, news: GroupNews { room: news.room, removed } });
                }
                // Answered either way, so the sender stops retrying.
                tokio::spawn(async move {
                    let _ = write_small(&mut send, &true).await;
                    let _ = send.finish();
                    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
                });
            }
            Internal::MoveResult { peer, known } => match known {
                Some(known) => {
                    self.move_pending.remove(&peer);
                    self.move_tried.remove(&peer);
                    self.emit(UiEvent::MoveDelivered { to: peer, known });
                    if known {
                        // They know who we are again: catch up on each other's status.
                        self.probe(peer);
                    }
                }
                None => {
                    self.move_tried.insert(peer, Instant::now());
                }
            },
            Internal::PresenceResult { peer, presence } => {
                self.probing.remove(&peer);
                if self.knows(&peer) {
                    self.set_presence(peer, presence.unwrap_or(PresenceMsg::offline()));
                    if self.reprobe.remove(&peer) {
                        self.probe(peer);
                    }
                }
            }
        }
    }

    fn on_incoming(&mut self, conn: Connection, send: SendStream, recv: RecvStream, hello: Ctrl) {
        let Ctrl::Hello { version, name, room, proof, invite, state, app } = hello else { return };
        let peer = conn.remote_id();
        info!("incoming call from {name} ({})", peer.fmt_short());
        if version != PROTO_VERSION {
            spawn_decline(send, conn, DeclineReason::Unknown);
            self.emit(UiEvent::Notice(format!(
                "{} tried to call with a different Parley version.",
                if name.is_empty() { peer.fmt_short().to_string() } else { name }
            )));
            return;
        }

        let room_key = match room {
            None => RoomKey::Direct(peer),
            Some(r) => RoomKey::Group(r),
        };

        // Authorization.
        let mut accepted_invite = None;
        match room_key {
            RoomKey::Group(r) => {
                let secret = self.secret_for(&r).or_else(|| {
                    invite
                        .as_ref()
                        .filter(|i| room_id_for(&i.secret) == r)
                        .map(|i| i.secret)
                });
                let ok = match (secret, proof) {
                    (Some(sec), Some(p)) => verify_proof(&sec, &peer, &self.me, &p),
                    _ => false,
                };
                // The owner took them out of the group: the secret alone isn't enough.
                let removed = self.groups.get(&r).is_some_and(|g| g.is_removed(&peer));
                if !ok || removed {
                    spawn_decline(send, conn, DeclineReason::NotAllowed);
                    return;
                }
                if !self.groups.contains_key(&r) {
                    // Only someone we know may pull us into a brand-new group.
                    if !self.contacts.contains_key(&peer) && !self.allow_unknown {
                        spawn_decline(send, conn, DeclineReason::NotAllowed);
                        return;
                    }
                    accepted_invite = invite;
                }
            }
            RoomKey::Direct(_) => {
                if !self.contacts.contains_key(&peer) && !self.allow_unknown {
                    spawn_decline(send, conn, DeclineReason::NotAllowed);
                    return;
                }
            }
        }

        let in_this_call = self.call.as_ref().is_some_and(|c| c.room == room_key);
        if self.call.is_some() && !in_this_call {
            spawn_decline(send, conn, DeclineReason::Busy);
            let n = if name.is_empty() { self.name_of(&peer) } else { name };
            self.emit(UiEvent::MissedCall { from: peer, name: n });
            return;
        }

        if in_this_call {
            // Resolve a simultaneous dial: the connection dialed by the lower id wins.
            let existing: Vec<Sid> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.peer == peer && !s.retired && s.room == room_key)
                .map(|(k, _)| *k)
                .collect();
            for sid in existing {
                let s = self.sessions.get_mut(&sid).unwrap();
                match s.phase {
                    Phase::Dialing | Phase::AwaitingAnswer if self.me < peer => {
                        // Ours wins; refuse theirs.
                        spawn_decline(send, conn, DeclineReason::Duplicate);
                        return;
                    }
                    _ => {
                        // Theirs wins (or it's a reconnect replacing a stale session).
                        s.close(CLOSE_DUPLICATE);
                    }
                }
            }
        }

        // Don't let anyone flood us with ringing calls: one at a time per caller, and
        // only a few callers at once.
        if !in_this_call {
            let ringing: Vec<&Session> =
                self.sessions.values().filter(|s| s.phase == Phase::IncomingPending && !s.retired).collect();
            if ringing.iter().any(|s| s.peer == peer) || ringing.len() >= 4 {
                spawn_decline(send, conn, DeclineReason::Busy);
                return;
            }
        }

        let sid = self.new_session(peer, room_key, Phase::IncomingPending);
        {
            let s = self.sessions.get_mut(&sid).unwrap();
            if !name.is_empty() {
                s.peer_name = name.clone();
            }
            s.peer_state = state;
            s.peer_app = app;
            s.invite = accepted_invite.clone();
        }
        self.attach_conn(sid, conn, send, recv);

        if in_this_call {
            let proof = self.acceptor_proof(sid);
            self.sessions[&sid].send(Ctrl::Accept { name: self.my_name.clone(), proof, state: self.my_state, app: APP_VERSION.into() });
            self.activate(sid);
            return;
        }

        // Ring.
        self.sessions[&sid].send(Ctrl::Ringing);
        let first = !self.pending.contains_key(&room_key);
        self.pending.entry(room_key).or_default().push(sid);
        if first {
            let group_name = match room_key {
                RoomKey::Group(r) => self
                    .groups
                    .get(&r)
                    .map(|g| g.name.clone())
                    .or(accepted_invite.map(|i| i.name)),
                RoomKey::Direct(_) => None,
            };
            let display = self.sessions[&sid].peer_name.clone();
            self.emit(UiEvent::Incoming { room: room_key, from: peer, name: display, group_name });
            let itx = self.itx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(RING_TIMEOUT).await;
                let _ = itx.send(Internal::RingTimeout { room: room_key });
            });
        }
    }

    fn on_dial_done(&mut self, sid: Sid, result: Result<(Connection, SendStream, RecvStream)>) {
        let Some(s) = self.sessions.get(&sid) else {
            if let Ok((conn, ..)) = result {
                conn.close(CLOSE_LEAVE.into(), b"");
            }
            return;
        };
        let (peer, room, retired) = (s.peer, s.room, s.retired);
        match result {
            Ok((conn, send, recv)) => {
                let still_wanted = !retired && self.call.as_ref().is_some_and(|c| c.room == room);
                // An incoming connection may have already connected us.
                let already = self.call.as_ref().is_some_and(|c| {
                    c.peers.get(&peer).is_some_and(|other| *other != sid)
                });
                if !still_wanted || already {
                    self.sessions.remove(&sid);
                    conn.close(CLOSE_DUPLICATE.into(), b"");
                    return;
                }
                self.sessions.get_mut(&sid).unwrap().phase = Phase::AwaitingAnswer;
                self.attach_conn(sid, conn, send, recv);
            }
            Err(e) => {
                info!("couldn't reach {}: {e:#}", peer.fmt_short());
                self.sessions.remove(&sid);
                if let Some(call) = &self.call {
                    if call.outgoing && call.room == room && !call.lost.contains_key(&peer) {
                        let name = self.name_of(&peer);
                        self.call = None;
                        self.emit(UiEvent::CallEnded {
                            reason: format!("Couldn't reach {name}. They may be offline or not running Parley."),
                        });
                    }
                }
            }
        }
    }

    fn on_ctrl(&mut self, sid: Sid, msg: Ctrl) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        if s.retired {
            return;
        }
        let peer = s.peer;
        match msg {
            Ctrl::Hello { .. } => {}
            Ctrl::Ringing => {
                if self.call.as_ref().is_some_and(|c| c.outgoing) {
                    self.emit(UiEvent::OutgoingRinging);
                }
            }
            Ctrl::Accept { name, proof, state, app } => {
                if s.phase != Phase::AwaitingAnswer {
                    return;
                }
                if let RoomKey::Group(r) = s.room {
                    let ok = match (self.groups.get(&r), proof) {
                        (Some(g), Some(p)) => verify_proof(&g.secret, &peer, &self.me, &p) && !g.is_removed(&peer),
                        _ => false,
                    };
                    if !ok {
                        warn!("peer {} failed group proof", peer.fmt_short());
                        s.close(CLOSE_DECLINE);
                        self.sessions.remove(&sid);
                        return;
                    }
                }
                if !name.is_empty() {
                    s.peer_name = name;
                }
                s.peer_state = state;
                s.peer_app = app;
                let mut became_active = None;
                if let Some(call) = &mut self.call {
                    if call.outgoing {
                        call.outgoing = false;
                        call.started = Instant::now();
                        became_active = Some(call.room);
                    }
                }
                if let Some(room) = became_active {
                    self.emit(UiEvent::CallActive { room });
                }
                self.activate(sid);
            }
            Ctrl::Decline(reason) => {
                let name = s.peer_name.clone();
                let room = s.room;
                s.close(CLOSE_DECLINE);
                self.sessions.remove(&sid);
                if reason == DeclineReason::Duplicate {
                    return;
                }
                if let Some(call) = &mut self.call {
                    call.lost.remove(&peer);
                    if matches!(room, RoomKey::Direct(_)) && call.room == room {
                        if call.peers.is_empty() {
                            self.call = None;
                            self.hub.clear_peers();
                            let reason = match reason {
                                DeclineReason::Busy => format!("{name} is in another call."),
                                DeclineReason::NotAllowed => {
                                    format!("{name} only accepts calls from their contacts.")
                                }
                                DeclineReason::Unknown => {
                                    format!("{name} is running an incompatible version of Parley.")
                                }
                                _ => format!("{name} declined the call."),
                            };
                            self.emit(UiEvent::CallEnded { reason });
                        }
                    } else {
                        self.emit(UiEvent::PeerLeft { id: peer });
                    }
                }
            }
            Ctrl::State(st) => {
                s.peer_state = st;
                if s.phase == Phase::Active {
                    self.emit(UiEvent::PeerState { id: peer, state: st });
                }
            }
            Ctrl::Keyframe(t) => {
                if t.is_video() {
                    self.hub.request_keyframe(t);
                }
            }
            Ctrl::Watch { track, on } => {
                if s.phase == Phase::Active {
                    self.hub.set_paused(&peer, track, !on);
                }
            }
            Ctrl::Roster(members) => {
                let room = s.room;
                if s.phase != Phase::Active {
                    return;
                }
                let mut changed = false;
                if let RoomKey::Group(r) = room {
                    if let Some(g) = self.groups.get_mut(&r) {
                        for m in &members {
                            if m.id != self.me {
                                changed |= g.add_member(m.id, &m.name);
                            }
                        }
                        if changed {
                            let g = g.clone();
                            self.emit(UiEvent::GroupUpdate { group: g });
                        }
                    }
                    let recently_left = |id: &EndpointId| {
                        self.call.as_ref().is_some_and(|c| {
                            c.left.get(id).is_some_and(|t| t.elapsed() < Duration::from_secs(15))
                        })
                    };
                    let removed = |id: &EndpointId| self.groups.get(&r).is_some_and(|g| g.is_removed(id));
                    let to_dial: Vec<EndpointId> = members
                        .iter()
                        .map(|m| m.id)
                        .filter(|id| *id != self.me && !self.has_session_with(id) && !recently_left(id) && !removed(id))
                        .collect();
                    for id in to_dial {
                        self.dial(id, room);
                    }
                }
            }
            Ctrl::Leave => {
                s.retired = true;
                self.peer_gone(sid, true);
            }
        }
    }

    /// Promotes a session to a live call participant and starts media.
    fn activate(&mut self, sid: Sid) {
        let Some(call) = &mut self.call else { return };
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        let Some(conn) = s.conn.clone() else { return };
        s.phase = Phase::Active;
        let peer = s.peer;
        if let Some(old) = call.peers.insert(peer, sid) {
            if old != sid {
                if let Some(o) = self.sessions.get_mut(&old) {
                    o.close(CLOSE_DUPLICATE);
                }
            }
        }
        call.lost.remove(&peer);
        call.left.remove(&peer);

        // Media plumbing.
        let mut senders = Vec::new();
        let mut flags = Vec::new();
        let s = self.sessions.get_mut(&sid).unwrap();
        for t in VIDEO_TRACKS {
            let (tx, rx) = mpsc::channel::<VideoFrame>(VIDEO_QUEUE);
            let need_key = Arc::new(AtomicBool::new(true));
            s.media.push(tokio::spawn(video_writer(
                conn.clone(),
                t,
                rx,
                need_key.clone(),
                self.hub.clone(),
            )));
            senders.push(tx);
            flags.push(need_key);
        }
        s.media.push(tokio::spawn(datagram_reader(conn.clone(), peer, self.hub.clone())));
        s.media.push(tokio::spawn(uni_acceptor(conn.clone(), peer, self.hub.clone())));
        let out = PeerOut {
            conn: conn.clone(),
            video: [senders.remove(0), senders.remove(0)],
            need_key: [flags.remove(0), flags.remove(0)],
            paused: Default::default(),
        };
        self.hub.add_peer(peer, Arc::new(out));

        let (name, state, room, app) = (s.peer_name.clone(), s.peer_state, s.room, s.peer_app.clone());
        s.send(Ctrl::State(self.my_state));

        // Record membership.
        if let RoomKey::Group(r) = room {
            if let Some(g) = self.groups.get_mut(&r) {
                if g.add_member(peer, &name) {
                    let g = g.clone();
                    self.emit(UiEvent::GroupUpdate { group: g });
                }
            }
            // Share who's here so the newcomer can connect to everyone.
            let roster = self.roster();
            for s in self.sessions.values() {
                if s.phase == Phase::Active {
                    s.send(Ctrl::Roster(roster.clone()));
                }
            }
        }
        self.emit(UiEvent::PeerJoined { id: peer, name, state, app });
    }

    fn roster(&self) -> Vec<Member> {
        let mut v = vec![Member { id: self.me, name: self.my_name.clone() }];
        if let Some(call) = &self.call {
            for (id, sid) in &call.peers {
                let name = self.sessions.get(sid).map(|s| s.peer_name.clone()).unwrap_or_default();
                v.push(Member { id: *id, name });
            }
        }
        v
    }

    fn on_closed(&mut self, sid: Sid, code: Option<u32>) {
        let Some(s) = self.sessions.get(&sid) else { return };
        if s.retired || matches!(code, Some(CLOSE_DUPLICATE)) {
            // Superseded or intentionally ended; just forget it.
            let is_current = self
                .call
                .as_ref()
                .is_some_and(|c| c.peers.get(&s.peer) == Some(&sid));
            if is_current {
                self.peer_gone(sid, true);
            } else {
                self.sessions.remove(&sid);
            }
            return;
        }
        match s.phase {
            Phase::IncomingPending => {
                let room = s.room;
                self.sessions.remove(&sid);
                if let Some(v) = self.pending.get_mut(&room) {
                    v.retain(|x| *x != sid);
                    if v.is_empty() {
                        self.pending.remove(&room);
                        self.emit(UiEvent::IncomingGone { room });
                    }
                }
            }
            Phase::Active => {
                let left = matches!(code, Some(CLOSE_LEAVE) | Some(CLOSE_DECLINE));
                self.peer_gone(sid, left);
            }
            Phase::Dialing | Phase::AwaitingAnswer => {
                let (peer, room) = (s.peer, s.room);
                self.sessions.remove(&sid);
                if let Some(call) = &self.call {
                    if call.outgoing && call.room == room {
                        let name = self.name_of(&peer);
                        self.call = None;
                        self.emit(UiEvent::CallEnded { reason: format!("The call to {name} failed.") });
                    }
                }
            }
        }
    }

    /// A participant's session ended. `left` = on purpose; otherwise we try to reconnect.
    fn peer_gone(&mut self, sid: Sid, left: bool) {
        let Some(mut s) = self.sessions.remove(&sid) else { return };
        for t in s.media.drain(..) {
            t.abort();
        }
        let peer = s.peer;
        if let Some(conn) = &s.conn {
            self.hub.remove_peer(&peer, conn);
        }
        let Some(call) = &mut self.call else { return };
        if call.peers.get(&peer) != Some(&sid) {
            return;
        }
        call.peers.remove(&peer);
        let direct = matches!(call.room, RoomKey::Direct(_));
        if !left {
            info!("lost {}, will try to reconnect", peer.fmt_short());
            call.lost.insert(peer, Instant::now());
            self.emit(UiEvent::PeerReconnecting { id: peer });
            return;
        }
        call.left.insert(peer, Instant::now());
        self.hub.unregister_peer_sinks(&peer);
        self.emit(UiEvent::PeerLeft { id: peer });
        if direct {
            let name = s.peer_name.clone();
            self.hangup();
            self.emit(UiEvent::CallEnded { reason: format!("{name} left the call.") });
        }
    }

    /// Everyone whose status we follow and who may see ours: contacts and the members
    /// of our groups.
    fn presence_peers(&self) -> std::collections::HashSet<EndpointId> {
        let mut s: std::collections::HashSet<EndpointId> = self.contacts.keys().copied().collect();
        for g in self.groups.values() {
            s.extend(g.members.iter().map(|m| m.id));
        }
        s.remove(&self.me);
        s
    }

    fn knows(&self, peer: &EndpointId) -> bool {
        self.contacts.contains_key(peer) || self.groups.values().any(|g| g.members.iter().any(|m| &m.id == peer))
    }

    /// What we tell `peer`: our status, and our group call if they're in that group.
    fn my_presence_for(&self, peer: &EndpointId) -> PresenceMsg {
        let status = match self.status {
            Status::Online => Presence::Online,
            Status::DoNotDisturb => Presence::DoNotDisturb,
            Status::Invisible => return PresenceMsg::offline(),
        };
        let call = match self.call.as_ref().map(|c| c.room) {
            Some(RoomKey::Group(r)) if self.groups.get(&r).is_some_and(|g| g.members.iter().any(|m| &m.id == peer)) => {
                Some(CallPresence { room: r, state: self.my_state })
            }
            _ => None,
        };
        PresenceMsg { status, call, app: Some(APP_VERSION.into()) }
    }

    fn set_presence(&mut self, peer: EndpointId, p: PresenceMsg) {
        if self.presence.get(&peer) != Some(&p) {
            debug!("{} is {:?}{}", peer.fmt_short(), p.status, if p.call.is_some() { " (in a group call)" } else { "" });
            self.emit(UiEvent::Presence { id: peer, presence: p.status, call: p.call, app: p.app.clone() });
            self.presence.insert(peer, p);
        }
    }

    /// Checks someone's status, telling them ours on the way.
    fn probe(&mut self, peer: EndpointId) {
        if !self.probing.insert(peer) {
            return;
        }
        let (ep, itx, mine) = (self.ep.clone(), self.itx.clone(), self.my_presence_for(&peer));
        tokio::spawn(async move {
            let res = tokio::time::timeout(PRESENCE_TIMEOUT, async {
                let conn = ep.connect(EndpointAddr::from(peer), PRESENCE_ALPN).await?;
                let (mut send, mut recv) = conn.open_bi().await?;
                write_small(&mut send, &mine).await?;
                let theirs: PresenceMsg = read_small(&mut recv).await?;
                conn.close(0u32.into(), b"");
                anyhow::Ok(theirs)
            })
            .await;
            let presence = match res {
                Ok(Ok(p)) => Some(p),
                _ => None,
            };
            let _ = itx.send(Internal::PresenceResult { peer, presence });
        });
    }

    /// Tells everyone our current status right away (checks under way carry the old one,
    /// so they're redone afterwards).
    fn announce(&mut self) {
        self.reprobe.extend(self.probing.iter().copied());
        self.last_probe = Some(Instant::now());
        for id in self.presence_peers() {
            self.probe(id);
        }
    }

    /// Chat messages from someone: group messages from that group's members, direct
    /// ones from contacts (or anyone with our code, if we allow unknown callers).
    fn on_chat_in(&mut self, conn: Connection, mut send: SendStream, env: ChatEnvelope) {
        let peer = conn.remote_id();
        let allowed = match env.group {
            Some(r) => self.groups.get(&r).is_some_and(|g| g.members.iter().any(|m| m.id == peer)),
            None => self.contacts.contains_key(&peer) || self.allow_unknown,
        };
        let ack = ChatAck { ids: if allowed { env.msgs.iter().map(|m| m.id.clone()).collect() } else { Vec::new() }, refused: !allowed };
        if allowed && !env.msgs.is_empty() {
            self.emit(UiEvent::ChatReceived { from: peer, name: env.name, group: env.group, msgs: env.msgs });
        }
        tokio::spawn(async move {
            let _ = write_small(&mut send, &ack).await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        });
    }

    /// Someone checked on us. Only contacts and group members get an answer, and
    /// nobody does while we're invisible (we look offline).
    fn on_presence_probe(&mut self, conn: Connection, mut send: SendStream, theirs: PresenceMsg) {
        let peer = conn.remote_id();
        if !self.knows(&peer) {
            conn.close(CLOSE_DECLINE.into(), b"");
            return;
        }
        let now = Instant::now();
        let flood = self.probed_by.get(&peer).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(1));
        self.probed_by.insert(peer, now);
        // Their check tells us how they are (offline if they're invisible).
        self.set_presence(peer, theirs);
        if flood || self.status == Status::Invisible {
            conn.close(CLOSE_DECLINE.into(), b"");
            return;
        }
        let mine = self.my_presence_for(&peer);
        tokio::spawn(async move {
            let _ = write_small(&mut send, &mine).await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        });
    }

    /// Tells friends and group members who haven't heard yet about our new identity
    /// (at most every 30 s each, like status checks, until they're reached).
    fn deliver_moves(&mut self) {
        if self.moves.is_empty() {
            return;
        }
        let due: Vec<EndpointId> = self
            .move_pending
            .iter()
            .filter(|id| self.knows(id) && self.move_tried.get(*id).is_none_or(|t| t.elapsed() >= PRESENCE_INTERVAL))
            .copied()
            .collect();
        for peer in due {
            // Marks it in flight too (a result clears or refreshes it).
            self.move_tried.insert(peer, Instant::now());
            let (ep, itx, msg) = (self.ep.clone(), self.itx.clone(), MoveMsg { chain: self.moves.clone() });
            tokio::spawn(async move {
                let res = tokio::time::timeout(Duration::from_secs(20), async {
                    let conn = ep.connect(EndpointAddr::from(peer), MOVE_ALPN).await?;
                    let (mut send, mut recv) = conn.open_bi().await?;
                    write_small(&mut send, &msg).await?;
                    let ack: MoveAck = read_small(&mut recv).await?;
                    conn.close(0u32.into(), b"");
                    anyhow::Ok(ack)
                })
                .await;
                let known = match res {
                    Ok(Ok(ack)) => Some(ack.known),
                    _ => None,
                };
                let _ = itx.send(Internal::MoveResult { peer, known });
            });
        }
    }

    /// Someone says they replaced their identity. Only a chain signed by an identity we
    /// know (a contact or group member), ending at whoever is connected, is followed;
    /// anyone else just hears "not known".
    fn on_move_in(&mut self, conn: Connection, mut send: SendStream, msg: MoveMsg) {
        let sender = conn.remote_id();
        let olds = follow_moves(&msg.chain, &sender, |id| self.knows(id));
        if let Some(olds) = &olds {
            info!("{} is now {}", olds[0].fmt_short(), sender.fmt_short());
            for old in olds {
                if let Some(name) = self.contacts.remove(old) {
                    self.contacts.entry(sender).or_insert(name);
                }
                for g in self.groups.values_mut() {
                    if g.members.iter().any(|m| m.id == sender) {
                        g.members.retain(|m| m.id != *old);
                    } else if let Some(m) = g.members.iter_mut().find(|m| m.id == *old) {
                        m.id = sender;
                    }
                }
                self.presence.remove(old);
            }
            self.emit(UiEvent::IdentityMoved { old: olds.clone(), new: sender });
            self.probe(sender);
        }
        let ack = MoveAck { known: olds.is_some() };
        tokio::spawn(async move {
            let _ = write_small(&mut send, &ack).await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        });
    }

    fn on_tick(&mut self) {
        self.deliver_moves();
        // Re-announce at once when our status, group call or mute/deafen/sharing changes,
        // so group members see who's in a call nearly live; otherwise check every 30 s.
        let now_state = (self.status, self.call.as_ref().map(|c| c.room), self.my_state);
        if self.announced != Some(now_state) {
            self.announced = Some(now_state);
            self.announce();
        } else if self.last_probe.is_none_or(|t| t.elapsed() >= PRESENCE_INTERVAL) {
            self.last_probe = Some(Instant::now());
            for id in self.presence_peers() {
                self.probe(id);
            }
        }
        // Reconnect attempts.
        let mut redial = Vec::new();
        let mut give_up = Vec::new();
        if let Some(call) = &self.call {
            for (peer, since) in &call.lost {
                if since.elapsed() > RECONNECT_WINDOW {
                    give_up.push(*peer);
                } else if !self.has_session_with(peer) {
                    redial.push((*peer, call.room));
                }
            }
        }
        for (peer, room) in redial {
            self.dial(peer, room);
        }
        for peer in give_up {
            self.hub.unregister_peer_sinks(&peer);
            self.emit(UiEvent::PeerLeft { id: peer });
            let mut end = None;
            if let Some(call) = &mut self.call {
                call.lost.remove(&peer);
                if matches!(call.room, RoomKey::Direct(_)) && call.peers.is_empty() {
                    end = Some(self.name_of(&peer));
                }
            }
            if let Some(name) = end {
                self.hangup();
                self.emit(UiEvent::CallEnded { reason: format!("Lost connection to {name}.") });
            }
        }

        // Link stats for the UI.
        if let Some(call) = &self.call {
            for (peer, sid) in &call.peers {
                if let Some(conn) = self.sessions.get(sid).and_then(|s| s.conn.as_ref()) {
                    let (direct, rtt) = link_info(conn);
                    self.emit(UiEvent::PeerLink { id: *peer, direct, rtt_ms: rtt });
                }
            }
        }
    }
}

impl Call {
    fn new(room: RoomKey, outgoing: bool) -> Self {
        Self { room, outgoing, peers: HashMap::new(), lost: HashMap::new(), left: HashMap::new(), started: Instant::now() }
    }
}

fn spawn_decline(mut send: SendStream, conn: Connection, why: DeclineReason) {
    tokio::spawn(async move {
        let _ = write_ctrl(&mut send, &Ctrl::Decline(why)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        conn.close(CLOSE_DECLINE.into(), b"");
    });
}

fn close_code(conn: &Connection) -> Option<u32> {
    use iroh::endpoint::ConnectionError;
    match conn.close_reason()? {
        ConnectionError::ApplicationClosed(c) => u32::try_from(c.error_code.into_inner()).ok(),
        ConnectionError::LocallyClosed => Some(CLOSE_LEAVE),
        _ => None,
    }
}

fn link_info(conn: &Connection) -> (bool, u32) {
    let mut direct = false;
    let mut rtt = None;
    for p in conn.paths().iter() {
        if p.is_ip() {
            direct = true;
        }
        let ms = p.rtt().as_millis() as u32;
        rtt = Some(rtt.map_or(ms, |x: u32| x.min(ms)));
    }
    (direct, rtt.unwrap_or(0))
}

// ---------- media tasks ----------

async fn datagram_reader(conn: Connection, peer: EndpointId, hub: Arc<Hub>) {
    // Test hooks: simulate a bad network (random extra delay and loss per packet).
    let env_num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let (jitter_ms, loss_pct) = (env_num("PARLEY_TEST_JITTER_MS"), env_num("PARLEY_TEST_LOSS_PCT"));
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut n = 0u64;
    let (mut per_track, mut since) = ([0u64; 2], std::time::Instant::now());
    while let Ok(d) = conn.read_datagram().await {
        n += 1;
        if n % 500 == 1 {
            debug!("audio from {}: {n} packets", peer.fmt_short());
        }
        if let Some(t @ (Track::Mic | Track::ScreenAudio)) = d.first().and_then(|b| Track::from_u8(*b)) {
            per_track[t as usize] += 1;
        }
        if since.elapsed() >= Duration::from_secs(10) {
            if per_track[1] > 0 || per_track[0] > 0 {
                info!("audio rx from {} in 10 s: voice {} packets, screen audio {} packets", peer.fmt_short(), per_track[0], per_track[1]);
            }
            per_track = [0; 2];
            since = std::time::Instant::now();
        }
        if d.len() < 2 {
            continue;
        }
        let Some(track) = Track::from_u8(d[0]) else { continue };
        if track.is_video() {
            continue;
        }
        if loss_pct > 0 && next() % 100 < loss_pct {
            continue;
        }
        if jitter_ms > 0 {
            let (hub, data, delay) = (hub.clone(), d[1..].to_vec(), next() % (jitter_ms + 1));
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                hub.push(&peer, track, data);
            });
            continue;
        }
        hub.push(&peer, track, d[1..].to_vec());
    }
}

async fn uni_acceptor(conn: Connection, peer: EndpointId, hub: Arc<Hub>) {
    while let Ok(mut recv) = conn.accept_uni().await {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut t = [0u8; 1];
            if recv.read_exact(&mut t).await.is_err() {
                return;
            }
            let Some(track) = Track::from_u8(t[0]).filter(|t| t.is_video()) else {
                let _ = recv.stop(0u32.into());
                return;
            };
            let generation = hub.new_generation(peer, track);
            let mut hdr = [0u8; FRAME_HEADER_LEN];
            loop {
                if recv.read_exact(&mut hdr).await.is_err() {
                    return;
                }
                let (len, _pts, _key) = decode_frame_header(&hdr);
                if len > MAX_FRAME_LEN {
                    let _ = recv.stop(0u32.into());
                    return;
                }
                let mut data = vec![0u8; len];
                if recv.read_exact(&mut data).await.is_err() {
                    return;
                }
                if _key {
                    debug!("{track:?} keyframe from {} ({len} bytes, gop {generation})", peer.fmt_short());
                }
                if !hub.is_current_generation(&peer, track, generation) {
                    // A newer GOP has started; abandon this one.
                    let _ = recv.stop(0u32.into());
                    return;
                }
                hub.push(&peer, track, data);
            }
        });
    }
}

async fn video_writer(
    conn: Connection,
    track: Track,
    mut rx: mpsc::Receiver<VideoFrame>,
    need_key: Arc<AtomicBool>,
    hub: Arc<Hub>,
) {
    use std::sync::atomic::Ordering;
    let mut cur: Option<SendStream> = None;
    let priority = if track == Track::Camera { 1 } else { 0 };
    // Test hook: pretend the link only carries this many kbit/s of video.
    let cap_kbps: u64 = std::env::var("PARLEY_TEST_VIDEO_CAP_KBPS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    while let Some(f) = rx.recv().await {
        if need_key.load(Ordering::Relaxed) {
            if let Some(mut s) = cur.take() {
                let _ = s.reset(0u32.into());
            }
            if !f.key {
                continue;
            }
            need_key.store(false, Ordering::Relaxed);
        }
        if f.captured.elapsed() > STALE_FRAME {
            // We're behind: drop the rest of this GOP and ask for a fresh start.
            hub.report_video_drop(track);
            need_key.store(true, Ordering::Relaxed);
            hub.request_keyframe(track);
            continue;
        }
        if f.key {
            if let Some(mut s) = cur.take() {
                let _ = s.finish();
            }
            debug!("{track:?}: starting GOP stream ({} byte keyframe)", f.data.len());
            match conn.open_uni().await {
                Ok(mut s) => {
                    let _ = s.set_priority(priority);
                    if s.write_all(&[track as u8]).await.is_err() {
                        need_key.store(true, Ordering::Relaxed);
                        continue;
                    }
                    cur = Some(s);
                }
                Err(_) => return,
            }
        }
        let Some(s) = cur.as_mut() else { continue };
        let hdr = encode_frame_header(f.data.len(), f.pts_us, f.key);
        let ok = s.write_all(&hdr).await.is_ok() && s.write_all(&f.data).await.is_ok();
        if cap_kbps > 0 {
            tokio::time::sleep(Duration::from_micros(f.data.len() as u64 * 8 * 1000 / cap_kbps)).await;
        }
        hub.report_video_sent(track, f.captured.elapsed());
        if !ok {
            cur = None;
            need_key.store(true, Ordering::Relaxed);
            hub.request_keyframe(track);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    struct Node {
        id: EndpointId,
        hub: Arc<Hub>,
        net: NetHandle,
        rx: async_channel::Receiver<UiEvent>,
        name: String,
    }

    async fn node(name: &str) -> Node {
        node_with(name, SecretKey::generate()).await
    }

    async fn node_with(name: &str, key: SecretKey) -> Node {
        let (tx, rx) = async_channel::unbounded();
        let hub = Hub::new();
        let net = start(key, hub.clone(), tx).await.unwrap();
        let id = match rx.recv().await.unwrap() {
            UiEvent::Ready { id } => id,
            e => panic!("unexpected {e:?}"),
        };
        Node { id, hub, net, rx, name: name.into() }
    }

    impl Node {
        fn directory(&self, contacts: &[&Node], groups: Vec<Group>) {
            self.net.send(NetCmd::Directory {
                my_name: self.name.clone(),
                contacts: contacts.iter().map(|n| Contact::new(n.id, n.name.clone())).collect(),
                groups,
                allow_unknown: true,
            });
        }
        async fn expect<T>(&self, what: &str, mut f: impl FnMut(&UiEvent) -> Option<T>) -> T {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
            loop {
                let ev = tokio::time::timeout_at(deadline, self.rx.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{}: timed out waiting for {what}", self.name))
                    .unwrap();
                eprintln!("[{}] {ev:?}", self.name);
                if let Some(v) = f(&ev) {
                    return v;
                }
            }
        }
    }

    use std::collections::HashSet;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn presence_status() {
        let _ = tracing_subscriber::fmt().with_env_filter("parley=debug").try_init();
        let (a, b, c) = tokio::join!(node("alice"), node("bob"), node("carol"));
        // Carol isn't Bob's contact: Bob must not tell her anything.
        a.directory(&[&b], vec![]);
        b.directory(&[&a], vec![]);
        c.directory(&[&b], vec![]);
        let want = |id: EndpointId, p: Presence| move |e: &UiEvent| matches!(e, UiEvent::Presence { id: i, presence, .. } if *i == id && *presence == p).then_some(());
        a.expect("bob online", want(b.id, Presence::Online)).await;
        b.expect("alice online", want(a.id, Presence::Online)).await;

        b.net.send(NetCmd::SetStatus(Status::DoNotDisturb));
        a.expect("bob dnd", want(b.id, Presence::DoNotDisturb)).await;

        b.net.send(NetCmd::SetStatus(Status::Invisible));
        a.expect("bob offline", want(b.id, Presence::Offline)).await;

        // Carol has Bob as a contact but Bob doesn't have her: she never sees him online.
        b.net.send(NetCmd::SetStatus(Status::Online));
        a.expect("bob back online", want(b.id, Presence::Online)).await;
        let seen = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(UiEvent::Presence { id, presence, .. }) = c.rx.recv().await {
                    if id == b.id && presence != Presence::Offline {
                        return presence;
                    }
                }
            }
        })
        .await;
        assert!(seen.is_err(), "a non-contact learned Bob's status: {seen:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn removed_members_stay_out() {
        let _ = tracing_subscriber::fmt().with_env_filter("parley=debug").try_init();
        let ka = SecretKey::generate();
        let (a, b, c) = tokio::join!(node_with("alice", ka.clone()), node("bob"), node("carol"));
        let m = |n: &Node| Member { id: n.id, name: n.name.clone() };
        let g = Group::new("trip".into(), vec![m(&a), m(&b), m(&c)]);
        let room = g.room_id();
        for n in [&a, &b, &c] {
            n.directory(&[], vec![g.clone()]);
        }
        // Before: Carol can join Bob in the group's call.
        b.net.send(NetCmd::JoinGroup(room));
        c.net.send(NetCmd::JoinGroup(room));
        b.expect("carol joins", |e| matches!(e, UiEvent::PeerJoined { id, .. } if *id == c.id).then_some(())).await;
        b.net.send(NetCmd::Hangup);
        c.net.send(NetCmd::Hangup);
        tokio::time::sleep(Duration::from_secs(16)).await; // past the "just left" grace

        // Alice (the owner) removes Carol and tells Bob.
        let removal = crate::store::Removal::sign(&ka, &room, c.id, 1);
        let mut ga = g.clone();
        ga.merge_removals(std::slice::from_ref(&removal));
        a.directory(&[], vec![ga]);
        a.net.send(NetCmd::GroupNews { to: b.id, news: GroupNews { room, removed: vec![removal.clone()] } });
        let got = b
            .expect("bob hears of the removal", |e| match e {
                UiEvent::GroupNews { news, .. } => Some(news.removed.clone()),
                _ => None,
            })
            .await;
        assert_eq!(got, [removal]);
        a.expect("delivered", |e| matches!(e, UiEvent::GroupNewsDelivered { ok: true, .. }).then_some(())).await;

        // Carol still has the secret, but nobody lets her into the group's call.
        b.net.send(NetCmd::JoinGroup(room));
        c.net.send(NetCmd::JoinGroup(room));
        let res = tokio::time::timeout(Duration::from_secs(10), b.expect("carol", |e| matches!(e, UiEvent::PeerJoined { id, .. } if *id == c.id).then_some(()))).await;
        assert!(res.is_err(), "a removed member got into the call");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn identity_move() {
        let _ = tracing_subscriber::fmt().with_env_filter("parley=debug").try_init();
        let (k1, k2) = (SecretKey::generate(), SecretKey::generate());
        let (a, b, d) = tokio::join!(node_with("alice", k1.clone()), node("bob"), node("dave"));
        // Bob is Alice's friend; Dave is on Alice's list but never had her old code.
        a.directory(&[&b, &d], vec![]);
        b.directory(&[&a], vec![]);
        d.directory(&[], vec![]);
        b.expect("alice online", |e| matches!(e, UiEvent::Presence { id, presence: Presence::Online, .. } if *id == a.id).then_some(())).await;

        // Alice replaces her identity: the old engine goes away, a new one tells friends.
        let notice = MoveNotice::sign(&k1, k2.public(), 1);
        drop(a);
        let a2 = node_with("alice2", k2).await;
        a2.directory(&[&b, &d], vec![]);
        a2.net.send(NetCmd::Moves { chain: vec![notice], pending: vec![b.id, d.id] });

        let (old, new) = b
            .expect("bob follows the move", |e| match e {
                UiEvent::IdentityMoved { old, new } => Some((old[0], *new)),
                _ => None,
            })
            .await;
        assert_eq!((old, new), (k1.public(), a2.id));
        let mut told = HashMap::new();
        while told.len() < 2 {
            let (to, known) = a2
                .expect("deliveries", |e| match e {
                    UiEvent::MoveDelivered { to, known } => Some((*to, *known)),
                    _ => None,
                })
                .await;
            told.insert(to, known);
        }
        assert_eq!(told[&b.id], true);
        assert_eq!(told[&d.id], false, "dave never knew alice, so he learns nothing");
        // Bob's engine knows Alice's new identity at once: her status reaches him again.
        b.expect("alice online again", |e| matches!(e, UiEvent::Presence { id, presence: Presence::Online, .. } if *id == a2.id).then_some(())).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn chat_delivery() {
        let _ = tracing_subscriber::fmt().with_env_filter("parley=debug").try_init();
        let (a, b, c) = tokio::join!(node("alice"), node("bob"), node("carol"));
        let g = Group::new("trip".into(), vec![Member { id: a.id, name: "alice".into() }, Member { id: b.id, name: "bob".into() }]);
        a.directory(&[&b], vec![g.clone()]);
        b.directory(&[&a], vec![g.clone()]);
        // Carol knows Bob's code, but Bob doesn't take messages from strangers.
        c.net.send(NetCmd::Directory { my_name: "carol".into(), contacts: vec![Contact::new(b.id, "bob")], groups: vec![], allow_unknown: false });
        b.net.send(NetCmd::Directory { my_name: "bob".into(), contacts: vec![Contact::new(a.id, "alice")], groups: vec![g.clone()], allow_unknown: false });

        let m = |id: &str, t: &str| WireMsg { id: id.into(), ts: 1, text: t.into() };
        a.net.send(NetCmd::SendChat { to: b.id, group: None, name: "alice".into(), msgs: vec![m("m1", "hi bob"), m("m2", "you there?")] });
        let got = b
            .expect("direct message", |e| match e {
                UiEvent::ChatReceived { from, group: None, msgs, .. } if *from == a.id => Some(msgs.clone()),
                _ => None,
            })
            .await;
        assert_eq!(got.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["hi bob", "you there?"]);
        let acked = a
            .expect("ack", |e| match e {
                UiEvent::ChatDelivered { to, ids, .. } if *to == b.id => Some(ids.clone()),
                _ => None,
            })
            .await;
        assert_eq!(acked, ["m1", "m2"]);

        a.net.send(NetCmd::SendChat { to: b.id, group: Some(g.room_id()), name: "alice".into(), msgs: vec![m("g1", "group hello")] });
        b.expect("group message", |e| matches!(e, UiEvent::ChatReceived { group: Some(r), .. } if *r == g.room_id()).then_some(())).await;

        c.net.send(NetCmd::SendChat { to: b.id, group: None, name: "carol".into(), msgs: vec![m("c1", "spam")] });
        let refused = c
            .expect("refusal", |e| match e {
                UiEvent::ChatDelivered { to, ids, refused } if *to == b.id => Some((ids.is_empty(), *refused)),
                _ => None,
            })
            .await;
        assert_eq!(refused, (true, true));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn group_call_presence() {
        let _ = tracing_subscriber::fmt().with_env_filter("parley=debug").try_init();
        let (a, b, c) = tokio::join!(node("alice"), node("bob"), node("carol"));
        let g = Group::new(
            "trip".into(),
            vec![Member { id: a.id, name: "alice".into() }, Member { id: b.id, name: "bob".into() }],
        );
        let room = g.room_id();
        // Carol and Bob are contacts, but Carol isn't in the group.
        a.directory(&[], vec![g.clone()]);
        b.directory(&[&c], vec![g.clone()]);
        c.directory(&[&b], vec![]);

        // Bob starts the group call alone ("waiting"), muted and sharing his screen.
        b.net.send(NetCmd::MediaState(MediaState { mic_muted: true, screen: true, ..Default::default() }));
        b.net.send(NetCmd::JoinGroup(room));
        let st = a
            .expect("bob in the group call", |e| match e {
                UiEvent::Presence { id, call: Some(cp), .. } if *id == b.id && cp.room == room => Some(cp.state),
                _ => None,
            })
            .await;
        assert!(st.mic_muted && st.screen);

        // Carol sees Bob online but never which call he's in.
        let leaked = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(UiEvent::Presence { id, call: Some(_), .. }) = c.rx.recv().await {
                    if id == b.id {
                        return;
                    }
                }
            }
        })
        .await;
        assert!(leaked.is_err(), "a non-member learned about the group call");

        // Hanging up clears it for Alice.
        b.net.send(NetCmd::Hangup);
        a.expect("bob left the call", |e| matches!(e, UiEvent::Presence { id, call: None, .. } if *id == b.id).then_some(())).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn direct_and_group_calls() {
        let _ = tracing_subscriber::fmt().with_env_filter("parley=debug").try_init();
        let (a, b) = tokio::join!(node("alice"), node("bob"));
        // Bob only knows Alice from her code; Alice doesn't have Bob (one-way share).
        a.directory(&[], vec![]);
        b.directory(&[&a], vec![]);
        tokio::time::sleep(Duration::from_secs(2)).await; // let discovery publish

        b.net.send(NetCmd::CallDirect(a.id));
        let room = a.expect("incoming", |e| match e {
            UiEvent::Incoming { room, from, name, .. } => {
                assert_eq!(*from, b.id);
                assert_eq!(name, "bob");
                Some(*room)
            }
            _ => None,
        }).await;
        b.expect("ringing", |e| matches!(e, UiEvent::OutgoingRinging).then_some(())).await;
        a.net.send(NetCmd::Accept(room));
        a.expect("peer joined", |e| matches!(e, UiEvent::PeerJoined { id, .. } if *id == b.id).then_some(())).await;
        b.expect("peer joined", |e| matches!(e, UiEvent::PeerJoined { id, .. } if *id == a.id).then_some(())).await;
        b.net.send(NetCmd::Hangup);
        a.expect("call ended", |e| matches!(e, UiEvent::CallEnded { .. }).then_some(())).await;

        // ---- group of three; only the creator knows everyone ----
        let (c, d, e) = tokio::join!(node("carol"), node("dave"), node("erin"));
        let g = Group::new(
            "trio".into(),
            vec![
                Member { id: c.id, name: "carol".into() },
                Member { id: d.id, name: "dave".into() },
                Member { id: e.id, name: "erin".into() },
            ],
        );
        c.directory(&[&d, &e], vec![g.clone()]);
        d.directory(&[&c], vec![]); // dave never saw the group code
        e.directory(&[&c], vec![]);
        tokio::time::sleep(Duration::from_secs(2)).await;

        c.net.send(NetCmd::JoinGroup(g.room_id()));
        for n in [&d, &e] {
            let room = n.expect("group ring", |ev| match ev {
                UiEvent::Incoming { room, group_name, .. } => {
                    assert_eq!(group_name.as_deref(), Some("trio"));
                    Some(*room)
                }
                _ => None,
            }).await;
            n.net.send(NetCmd::Accept(room));
        }
        // Everyone ends up connected to both others.
        for (n, others) in [(&c, [d.id, e.id]), (&d, [c.id, e.id]), (&e, [c.id, d.id])] {
            let mut seen = HashSet::new();
            n.expect("full mesh", |ev| {
                if let UiEvent::PeerJoined { id, .. } = ev {
                    seen.insert(*id);
                }
                others.iter().all(|o| seen.contains(o)).then_some(())
            }).await;
        }

        // Dave leaves; Carol and Erin stay connected.
        d.net.send(NetCmd::Hangup);
        c.expect("dave left", |ev| matches!(ev, UiEvent::PeerLeft { id } if *id == d.id).then_some(())).await;
        e.expect("dave left", |ev| matches!(ev, UiEvent::PeerLeft { id } if *id == d.id).then_some(())).await;

        // Dave rejoins from the group he learned via the invite; nobody gets rung.
        d.directory(&[&c], vec![g.clone()]);
        d.net.send(NetCmd::JoinGroup(g.room_id()));
        for (n, who) in [(&c, d.id), (&e, d.id)] {
            n.expect("dave back", |ev| matches!(ev, UiEvent::PeerJoined { id, .. } if *id == who).then_some(())).await;
        }
        let mut seen = HashSet::new();
        d.expect("dave meshed", |ev| {
            assert!(!matches!(ev, UiEvent::Incoming { .. }), "rejoin must not ring");
            if let UiEvent::PeerJoined { id, .. } = ev {
                seen.insert(*id);
            }
            (seen.contains(&c.id) && seen.contains(&e.id)).then_some(())
        }).await;

        // Erin's network drops without a goodbye: the others see her as reconnecting,
        // then everyone redials and the call heals by itself.
        e.net.send(NetCmd::SimulateDropout);
        c.expect("erin reconnecting", |ev| matches!(ev, UiEvent::PeerReconnecting { id } if *id == e.id).then_some(())).await;
        c.expect("erin back", |ev| matches!(ev, UiEvent::PeerJoined { id, .. } if *id == e.id).then_some(())).await;
        d.expect("erin back", |ev| matches!(ev, UiEvent::PeerJoined { id, .. } if *id == e.id).then_some(())).await;
    }

    /// Collects buffers pushed into a fake receive pipeline.
    fn capture_sink(hub: &Hub, peer: EndpointId, track: Track) -> (gst::Pipeline, std::sync::mpsc::Receiver<Vec<u8>>) {
        use gst::prelude::*;
        let pipeline = gst::Pipeline::new();
        let src = gst_app::AppSrc::builder().is_live(true).format(gst::Format::Bytes).build();
        let (tx, rx) = std::sync::mpsc::channel();
        let sink = gst_app::AppSink::builder().sync(false).build();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let smp = s.pull_sample().unwrap();
                    let _ = tx.send(smp.buffer().unwrap().map_readable().unwrap().to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add_many([src.upcast_ref::<gst::Element>(), sink.upcast_ref::<gst::Element>()]).unwrap();
        src.link(&sink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        hub.register_sink(peer, track, src);
        (pipeline, rx)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs internet (n0 relays + discovery)"]
    async fn media_transport() {
        gst::init().unwrap();
        let (a, b) = tokio::join!(node("alice"), node("bob"));
        a.directory(&[&b], vec![]);
        b.directory(&[&a], vec![]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        a.net.send(NetCmd::CallDirect(b.id));
        let room = b.expect("incoming", |e| match e { UiEvent::Incoming { room, .. } => Some(*room), _ => None }).await;
        b.net.send(NetCmd::Accept(room));
        a.expect("joined", |e| matches!(e, UiEvent::PeerJoined { .. }).then_some(())).await;

        let (_p1, video) = capture_sink(&b.hub, a.id, Track::Screen);
        let (_p2, audio) = capture_sink(&b.hub, a.id, Track::Mic);
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Delta frames before the first keyframe must be skipped (decoder can't use them).
        let frame = |n: u8, key: bool, len: usize| VideoFrame {
            data: bytes::Bytes::from(vec![n; len]),
            pts_us: n as u64,
            key,
            captured: std::time::Instant::now(),
        };
        a.hub.send_video(Track::Screen, frame(0, false, 100));
        for n in 1..=60u8 {
            // Two GOPs of 30, with frames large enough to span many packets.
            a.hub.send_video(Track::Screen, frame(n, n == 1 || n == 31, 20_000));
            a.hub.send_audio(Track::Mic, &[n; 160]);
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        let mut got = Vec::new();
        while let Ok(v) = video.recv_timeout(Duration::from_secs(5)) {
            assert!(v.iter().all(|x| *x == v[0]), "frame corrupted");
            got.push(v[0]);
            if got.len() == 60 { break; }
        }
        assert_eq!(got, (1..=60).collect::<Vec<u8>>(), "frames lost or reordered");
        let mut audio_n = 0;
        while audio.recv_timeout(Duration::from_millis(500)).is_ok() { audio_n += 1; }
        assert!(audio_n >= 55, "only {audio_n}/60 audio datagrams arrived");
        eprintln!("video 60/60 in order, audio {audio_n}/60");
    }
}
