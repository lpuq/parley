//! Screen-share audio routing on PipeWire.
//!
//! While sharing audio we create a private virtual sink and link the chosen
//! apps' output ports into it (they keep playing to the speakers too). The
//! screen-audio encoder records that sink's monitor. Streams owned by our own
//! process are never linked, so friends never hear themselves echoed back.

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    rc::Rc,
    thread,
};

use pipewire as pw;
use pw::{properties::properties, types::ObjectType};

#[derive(Clone, Debug, PartialEq)]
pub enum AudioShare {
    Off,
    /// Everything playing on the desktop except Parley itself.
    Desktop,
    /// Only these applications (by `application.name`).
    Apps(Vec<String>),
}

pub enum PwEvent {
    /// Applications currently playing audio (excluding us).
    Apps(Vec<String>),
    /// The virtual sink exists and can be recorded from.
    SinkReady,
}

enum PwCmd {
    Set(AudioShare),
}

pub struct AppAudio {
    tx: pw::channel::Sender<PwCmd>,
    pub sink_name: String,
    /// node.name of the system's default microphone (from PipeWire's "default" metadata).
    pub default_source: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl AppAudio {
    pub fn start(events: async_channel::Sender<PwEvent>) -> Option<Self> {
        let (tx, rx) = pw::channel::channel();
        let sink_name = format!("parley-share-{}", std::process::id());
        let name = sink_name.clone();
        let (ok_tx, ok_rx) = std::sync::mpsc::channel();
        let default_source: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let ds = default_source.clone();
        thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                if let Err(e) = run(rx, events, name, ok_tx.clone(), ds) {
                    tracing::warn!("PipeWire app-audio routing unavailable: {e}");
                    let _ = ok_tx.send(false);
                }
            })
            .ok()?;
        if ok_rx.recv().unwrap_or(false) { Some(Self { tx, sink_name, default_source }) } else { None }
    }

    pub fn set(&self, mode: AudioShare) {
        let _ = self.tx.send(PwCmd::Set(mode));
    }
}

#[derive(Debug, Default, Clone)]
struct NodeInfo {
    name: String,
    class: String,
    /// `application.name`, else the binary; empty for system nodes.
    app: String,
    pid: Option<u32>,
    client: Option<u32>,
    /// Part of a filter-chain / loopback (e.g. Asahi speaker DSP): never capture,
    /// it carries everyone's audio including ours.
    internal: bool,
}

#[derive(Debug, Default, Clone)]
struct ClientInfo {
    pid: Option<u32>,
    binary: String,
    app: String,
}

#[derive(Debug, Clone)]
struct PortInfo {
    node: u32,
    output: bool,
    channel: String,
    monitor: bool,
}

#[derive(Default)]
struct State {
    nodes: HashMap<u32, NodeInfo>,
    clients: HashMap<u32, ClientInfo>,
    ports: HashMap<u32, PortInfo>,
    mode: Option<AudioShare>,
    sink: Option<pw::node::Node>,
    sink_id: Option<u32>,
    links: HashMap<(u32, u32), pw::link::Link>,
    last_apps: Vec<String>,
    last_linked: String,
}

fn run(
    rx: pw::channel::Receiver<PwCmd>,
    events: async_channel::Sender<PwEvent>,
    sink_name: String,
    ok: std::sync::mpsc::Sender<bool>,
    default_source: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let _ = ok.send(true);

    let my_pid = std::process::id();
    let state = Rc::new(RefCell::new(State::default()));

    let reconcile = {
        let state = state.clone();
        let core = core.clone();
        let events = events.clone();
        let sink_name = sink_name.clone();
        Rc::new(move || reconcile(&state, &core, &events, &sink_name, my_pid))
    };

    // Registry announcements only carry a subset of properties; bind nodes and
    // clients to receive their full property sets (pid, link-group, ...).
    let bound: Rc<RefCell<HashMap<u32, (Box<dyn std::any::Any>, Box<dyn std::any::Any>)>>> = Rc::default();
    let registry_weak = registry.downgrade();
    let _listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let reconcile = reconcile.clone();
            let sink_name = sink_name.clone();
            let bound = bound.clone();
            move |g| {
                let Some(props) = g.props else { return };
                let Some(registry) = registry_weak.upgrade() else { return };
                match g.type_ {
                    ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                        let Ok(meta) = registry.bind::<pw::metadata::Metadata, _>(g) else { return };
                        let ds = default_source.clone();
                        let l = meta
                            .add_listener_local()
                            .property(move |_, key, _, value| {
                                if key == Some("default.audio.source") {
                                    let name = value
                                        .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                                        .and_then(|j| j.get("name").and_then(|n| n.as_str()).map(str::to_string));
                                    tracing::debug!("default microphone: {name:?}");
                                    *ds.lock().unwrap() = name;
                                }
                                0
                            })
                            .register();
                        bound.borrow_mut().insert(g.id, (Box::new(meta), Box::new(l)));
                    }
                    ObjectType::Client => {
                        let Ok(client) = registry.bind::<pw::client::Client, _>(g) else { return };
                        let (st, rec, id) = (state.clone(), reconcile.clone(), g.id);
                        let l = client
                            .add_listener_local()
                            .info(move |info| {
                                let p = info.props();
                                let get = |k: &str| p.and_then(|p| p.get(k)).unwrap_or_default().to_string();
                                // Later info events only carry changed fields; keep what we have.
                                if get("application.name").is_empty() && get("application.process.id").is_empty() {
                                    return;
                                }
                                let ci = ClientInfo {
                                    pid: get("application.process.id").parse().ok(),
                                    binary: get("application.process.binary"),
                                    app: get("application.name"),
                                };
                                st.borrow_mut().clients.insert(id, ci);
                                rec();
                            })
                            .register();
                        bound.borrow_mut().insert(g.id, (Box::new(client), Box::new(l)));
                    }
                    ObjectType::Node => {
                        let name = props.get("node.name").unwrap_or_default().to_string();
                        if name == sink_name {
                            state.borrow_mut().sink_id = Some(g.id);
                            let _ = events.try_send(PwEvent::SinkReady);
                        }
                        let class = props.get("media.class").unwrap_or_default().to_string();
                        state.borrow_mut().nodes.insert(g.id, NodeInfo { name, class: class.clone(), ..Default::default() });
                        if class != "Stream/Output/Audio" {
                            return;
                        }
                        let Ok(node) = registry.bind::<pw::node::Node, _>(g) else { return };
                        let (st, rec, id) = (state.clone(), reconcile.clone(), g.id);
                        let l = node
                            .add_listener_local()
                            .info(move |info| {
                                let Some(p) = info.props() else { return };
                                let get = |k: &str| p.get(k).map(str::to_string);
                                // Later info events (e.g. state changes) only carry changed fields.
                                if get("media.class").is_none() {
                                    return;
                                }
                                let ni = NodeInfo {
                                    name: get("node.name").unwrap_or_default(),
                                    class: get("media.class").unwrap_or_default(),
                                    app: get("application.name")
                                        .or_else(|| get("application.process.binary"))
                                        .unwrap_or_default(),
                                    pid: get("application.process.id").and_then(|v| v.parse().ok()),
                                    client: get("client.id").and_then(|v| v.parse().ok()),
                                    internal: get("node.link-group").is_some() || get("filter.smart").is_some(),
                                };
                                st.borrow_mut().nodes.insert(id, ni);
                                rec();
                            })
                            .register();
                        bound.borrow_mut().insert(g.id, (Box::new(node), Box::new(l)));
                        return;
                    }
                    ObjectType::Port => {
                        let Some(node) = props.get("node.id").and_then(|n| n.parse().ok()) else { return };
                        let info = PortInfo {
                            node,
                            output: props.get("port.direction") == Some("out"),
                            channel: props.get("audio.channel").unwrap_or_default().to_string(),
                            monitor: props.get("port.monitor") == Some("true"),
                        };
                        state.borrow_mut().ports.insert(g.id, info);
                    }
                    _ => return,
                }
                reconcile();
            }
        })
        .global_remove({
            let state = state.clone();
            let reconcile = reconcile.clone();
            let bound = bound.clone();
            move |id| {
                bound.borrow_mut().remove(&id);
                {
                    let mut st = state.borrow_mut();
                    st.nodes.remove(&id);
                    st.clients.remove(&id);
                    st.ports.remove(&id);
                    if st.sink_id == Some(id) {
                        st.sink_id = None;
                    }
                    st.links.retain(|(o, i), _| *o != id && *i != id);
                }
                reconcile();
            }
        })
        .register();

    let _rx = rx.attach(mainloop.loop_(), {
        let state = state.clone();
        let reconcile = reconcile.clone();
        move |cmd| match cmd {
            PwCmd::Set(mode) => {
                state.borrow_mut().mode = Some(mode);
                reconcile();
            }
        }
    });

    mainloop.run();
    Ok(())
}

/// Maps an app channel onto our stereo sink's input channels.
fn targets(channel: &str) -> &'static [&'static str] {
    match channel {
        "FL" | "RL" | "SL" | "FLC" => &["FL"],
        "FR" | "RR" | "SR" | "FRC" => &["FR"],
        "MONO" | "FC" | "" => &["FL", "FR"],
        _ => &[],
    }
}

fn reconcile(
    state: &Rc<RefCell<State>>,
    core: &pw::core::CoreRc,
    events: &async_channel::Sender<PwEvent>,
    sink_name: &str,
    my_pid: u32,
) {
    let mut st = state.borrow_mut();

    // Report which apps are playing audio.
    let clients = st.clients.clone();
    let owner = |n: &NodeInfo| -> (Option<u32>, String, String) {
        let c = n.client.and_then(|id| clients.get(&id));
        let pid = n.pid.or(c.and_then(|c| c.pid));
        let binary = c.map(|c| c.binary.clone()).unwrap_or_default();
        let app = if !n.app.is_empty() { n.app.clone() } else { c.map(|c| c.app.clone()).unwrap_or_default() };
        (pid, binary, app)
    };
    let is_candidate = |n: &NodeInfo| {
        if n.class != "Stream/Output/Audio" || n.internal || n.name.starts_with("parley") {
            return false;
        }
        let (pid, binary, app) = owner(n);
        // Inside Flatpak our PID is namespaced, so also exclude by name:
        // anything Parley plays is call audio and must never be re-shared.
        pid != Some(my_pid)
            && !app.is_empty()
            && app != "Parley"
            && binary != "parley"
            && !matches!(binary.as_str(), "pipewire" | "wireplumber")
    };
    let app_of = |n: &NodeInfo| owner(n).2;
    let apps: Vec<String> = st
        .nodes
        .values()
        .filter(|n| is_candidate(n))
        .map(app_of)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if apps != st.last_apps {
        st.last_apps = apps.clone();
        let _ = events.try_send(PwEvent::Apps(apps));
    }

    let mode = st.mode.clone().unwrap_or(AudioShare::Off);
    if mode == AudioShare::Off {
        st.links.clear();
        if st.sink.take().is_some() {
            tracing::info!("screen audio sink removed");
        }
        return;
    }

    if st.sink.is_none() {
        let props = properties! {
            "factory.name" => "support.null-audio-sink",
            "node.name" => sink_name,
            "node.description" => "Parley screen share audio",
            "media.class" => "Audio/Sink",
            "audio.channels" => "2",
            "audio.position" => "FL,FR",
            "node.virtual" => "true",
            "priority.session" => "0",
            "priority.driver" => "0",
            "object.linger" => "false",
        };
        match core.create_object::<pw::node::Node>("adapter", &props) {
            Ok(n) => st.sink = Some(n),
            Err(e) => {
                tracing::error!("creating screen audio sink failed: {e}");
                return;
            }
        }
    }
    let Some(sink_id) = st.sink_id else { return };

    let selected = |n: &NodeInfo| {
        is_candidate(n)
            && match &mode {
                AudioShare::Desktop => true,
                AudioShare::Apps(list) => list.iter().any(|a| *a == app_of(n)),
                AudioShare::Off => false,
            }
    };
    let sink_inputs: HashMap<String, u32> = st
        .ports
        .iter()
        .filter(|(_, p)| p.node == sink_id && !p.output)
        .map(|(id, p)| (p.channel.clone(), *id))
        .collect();

    let mut wanted: HashMap<(u32, u32), (u32, u32)> = HashMap::new();
    for (pid, p) in &st.ports {
        if !p.output || p.monitor {
            continue;
        }
        let Some(node) = st.nodes.get(&p.node) else { continue };
        if !selected(node) {
            continue;
        }
        for ch in targets(&p.channel) {
            if let Some(inp) = sink_inputs.get(*ch) {
                wanted.insert((*pid, *inp), (p.node, sink_id));
            }
        }
    }

    let linked_apps: BTreeSet<String> = wanted
        .values()
        .filter_map(|(out_node, _)| st.nodes.get(out_node).map(app_of))
        .collect();
    let summary = format!("{linked_apps:?}");
    if summary != st.last_linked {
        tracing::info!("screen audio: sharing sound from {summary}");
        st.last_linked = summary;
    }
    st.links.retain(|k, _| wanted.contains_key(k));
    for ((out_port, in_port), (out_node, in_node)) in wanted {
        if st.links.contains_key(&(out_port, in_port)) {
            continue;
        }
        let props = properties! {
            "link.output.node" => out_node.to_string(),
            "link.output.port" => out_port.to_string(),
            "link.input.node" => in_node.to_string(),
            "link.input.port" => in_port.to_string(),
            "object.linger" => "false",
        };
        match core.create_object::<pw::link::Link>("link-factory", &props) {
            Ok(l) => {
                st.links.insert((out_port, in_port), l);
            }
            Err(e) => tracing::warn!("link failed: {e}"),
        }
    }
}
