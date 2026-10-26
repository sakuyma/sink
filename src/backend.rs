// PipeWire backend: runs on its own thread and communicates with the GTK
// thread through an async_channel (state snapshots) and a pw::channel
// (user actions).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::thread;

use async_channel::Sender;
use pipewire as pw;
use pw::device::{Device, DeviceListener};
use pw::link::{Link, LinkInfoRef, LinkListener};
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node, NodeInfoRef, NodeListener};
use pw::registry::RegistryRc;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray};
use pw::types::ObjectType;

use crate::model::{
    gain_from_percent, json_name, parse_default_name, percent_from_gain, Action, AppInfo,
    DeviceInfo, Snapshot,
};

pub type ActionSender = pw::channel::Sender<Action>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sink,
    Source,
    Stream,
}

impl Kind {
    fn from_class(class: &str) -> Option<Self> {
        match class {
            "Audio/Sink" | "Audio/Sink/Virtual" => Some(Self::Sink),
            "Audio/Source" | "Audio/Source/Virtual" => Some(Self::Source),
            "Stream/Output/Audio" => Some(Self::Stream),
            _ => None,
        }
    }
}

struct NodeEntry {
    proxy: Node,
    _listener: NodeListener,
    kind: Option<Kind>,
    node_name: String,
    node_nick: String,
    description: String,
    device_icon: String,
    app_name: String,
    app_icon: String,
    device_id: Option<u32>,
    card_profile_device: Option<i32>,
    channels: u32,
    volumes: Vec<f32>,
    muted: bool,
    params_requested: bool,
}

impl NodeEntry {
    fn new(proxy: Node, listener: NodeListener) -> Self {
        Self {
            proxy,
            _listener: listener,
            kind: None,
            node_name: String::new(),
            node_nick: String::new(),
            description: String::new(),
            device_icon: String::new(),
            app_name: String::new(),
            app_icon: String::new(),
            device_id: None,
            card_profile_device: None,
            channels: 2,
            volumes: Vec::new(),
            muted: false,
            params_requested: false,
        }
    }

    fn title(&self) -> String {
        first_non_empty(&[&self.node_nick, &self.description, &self.node_name]).to_string()
    }

    fn description_rest(&self) -> String {
        let title = self.title();
        let desc = self.description.trim();
        if desc.is_empty() || desc == title {
            return String::new();
        }
        if let Some(rest) = desc.strip_prefix(&title) {
            rest.trim_start_matches([' ', '-', ':', ',', '('])
                .to_string()
        } else {
            desc.to_string()
        }
    }

    fn volume_percent(&self) -> f32 {
        let gain = self.volumes.iter().copied().fold(0.0_f32, f32::max);
        percent_from_gain(gain)
    }

    fn device_info(&self, id: u32, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id,
            title: self.title(),
            subtitle: self.description_rest(),
            icon: self.device_icon.clone(),
            volume: self.volume_percent(),
            muted: self.muted,
            is_default,
        }
    }

    fn app_info(&self, id: u32, sink_id: Option<u32>, follow_default: bool) -> AppInfo {
        AppInfo {
            id,
            title: first_non_empty(&[
                &self.app_name,
                &self.description,
                &self.node_nick,
                &self.node_name,
            ])
            .to_string(),
            icon: if self.app_icon.is_empty() {
                "sink-app-symbolic".to_string()
            } else {
                self.app_icon.clone()
            },
            volume: self.volume_percent(),
            muted: self.muted,
            sink_id,
            follow_default,
        }
    }
}

struct LinkEntry {
    _proxy: Link,
    _listener: LinkListener,
    output_node: Option<u32>,
    input_node: Option<u32>,
}

#[derive(Clone)]
struct RouteEntry {
    index: i32,
    device: i32,
    direction: Option<u32>,
    volumes: Vec<f32>,
    muted: Option<bool>,
}

struct DeviceEntry {
    proxy: Device,
    _listener: DeviceListener,
    routes: HashMap<i32, RouteEntry>,
}

struct State {
    tx: Sender<Snapshot>,
    nodes: HashMap<u32, NodeEntry>,
    links: HashMap<u32, LinkEntry>,
    devices: HashMap<u32, DeviceEntry>,
    metadata: Option<Metadata>,
    metadata_listener: Option<MetadataListener>,
    metadata_id: Option<u32>,
    default_sink_name: Option<String>,
    default_source_name: Option<String>,
    targets: HashMap<u32, i32>,
    ready: bool,
}

impl State {
    fn new(tx: Sender<Snapshot>) -> Self {
        Self {
            tx,
            nodes: HashMap::new(),
            links: HashMap::new(),
            devices: HashMap::new(),
            metadata: None,
            metadata_listener: None,
            metadata_id: None,
            default_sink_name: None,
            default_source_name: None,
            targets: HashMap::new(),
            ready: false,
        }
    }

    fn notify(&self) {
        if !self.ready {
            return;
        }
        let snapshot = self.snapshot();
        if std::env::var_os("SINK_DEBUG").is_some() {
            eprintln!(
                "[{}] sink: {} sinks, {} sources, {} apps, default {:?}",
                stamp(),
                snapshot.sinks.len(),
                snapshot.sources.len(),
                snapshot.apps.len(),
                self.default_sink_name
            );
            for device in snapshot.sinks.iter().chain(snapshot.sources.iter()) {
                eprintln!(
                    "  device {} {:?} vol={:.0}%{}",
                    device.id,
                    device.title,
                    device.volume,
                    if device.is_default { " (default)" } else { "" }
                );
            }
            for app in &snapshot.apps {
                eprintln!(
                    "  app {} {:?} vol={:.0}% mute={} sink={:?} follow={}",
                    app.id, app.title, app.volume, app.muted, app.sink_id, app.follow_default
                );
            }
        }
        let _ = self.tx.try_send(snapshot);
    }

    fn snapshot(&self) -> Snapshot {
        let default_sink_id = self.default_id(true);
        let default_source_id = self.default_id(false);

        let mut sinks = Vec::new();
        let mut sources = Vec::new();
        for (id, node) in &self.nodes {
            match node.kind {
                Some(Kind::Sink) => sinks.push(node.device_info(*id, Some(*id) == default_sink_id)),
                Some(Kind::Source) => {
                    sources.push(node.device_info(*id, Some(*id) == default_source_id))
                }
                _ => {}
            }
        }

        // Direct stream -> sink links. A stream has one link per channel, all
        // pointing at the same sink; the last one wins.
        let mut direct_sink: HashMap<u32, u32> = HashMap::new();
        for link in self.links.values() {
            let (Some(out), Some(input)) = (link.output_node, link.input_node) else {
                continue;
            };
            if let Some(node) = self.nodes.get(&input) {
                if node.kind == Some(Kind::Sink) {
                    direct_sink.insert(out, input);
                }
            }
        }

        let mut apps = Vec::new();
        for (id, node) in &self.nodes {
            if node.kind != Some(Kind::Stream) {
                continue;
            }
            let target = self.targets.get(id).copied();
            let follow_default = target.map_or(true, |t| t < 0);
            let target_sink = target.and_then(|t| {
                if t < 0 {
                    return None;
                }
                let id = t as u32;
                self.nodes
                    .get(&id)
                    .filter(|n| n.kind == Some(Kind::Sink))
                    .map(|_| id)
            });
            let sink_id = direct_sink
                .get(id)
                .copied()
                .or(target_sink)
                .or(if follow_default {
                    default_sink_id
                } else {
                    None
                });
            apps.push(node.app_info(*id, sink_id, follow_default));
        }

        Snapshot {
            sinks,
            sources,
            apps,
            ready: true,
            error: None,
        }
    }

    fn default_id(&self, sink: bool) -> Option<u32> {
        let name = if sink {
            self.default_sink_name.as_deref()
        } else {
            self.default_source_name.as_deref()
        }?;
        let kind = if sink { Kind::Sink } else { Kind::Source };
        self.nodes
            .iter()
            .find(|(_, node)| node.kind == Some(kind) && node.node_name == name)
            .map(|(id, _)| *id)
    }

    fn on_node_info(&mut self, id: u32, info: &NodeInfoRef) {
        let Some(props) = info.props() else {
            return;
        };
        let kind_update = props
            .get("media.class")
            .filter(|class| !class.is_empty())
            .map(Kind::from_class);
        let node_name = props.get("node.name").unwrap_or_default().to_string();
        let node_nick = props.get("node.nick").unwrap_or_default().to_string();
        let description = props
            .get("node.description")
            .unwrap_or_default()
            .to_string();
        let device_icon = props
            .get("device.icon-name")
            .unwrap_or_default()
            .to_string();
        let app_name = props
            .get("application.name")
            .unwrap_or_default()
            .to_string();
        let app_icon = props
            .get("application.icon-name")
            .unwrap_or_default()
            .to_string();
        let device_id = props
            .get("device.id")
            .and_then(|value| value.parse::<u32>().ok());
        let card_profile_device = props
            .get("card.profile.device")
            .and_then(|value| value.parse::<i32>().ok());
        let channels = props
            .get("audio.channels")
            .and_then(|v| v.parse::<u32>().ok())
            .map(|channels| channels.max(1));
        let node_name_changed;
        let request_params;
        let is_audio_node;
        {
            let Some(entry) = self.nodes.get_mut(&id) else {
                return;
            };
            let kind = kind_update.unwrap_or(entry.kind);
            let effective_node_name = if node_name.is_empty() {
                entry.node_name.clone()
            } else {
                node_name.clone()
            };
            node_name_changed = entry.kind != kind || entry.node_name != effective_node_name;
            if entry.kind != kind {
                entry.params_requested = false;
            }
            entry.kind = kind;
            if !node_name.is_empty() {
                entry.node_name = node_name;
            }
            if !node_nick.is_empty() {
                entry.node_nick = node_nick;
            }
            if !description.is_empty() {
                entry.description = description;
            }
            if !device_icon.is_empty() {
                entry.device_icon = device_icon;
            }
            if !app_name.is_empty() {
                entry.app_name = app_name;
            }
            if !app_icon.is_empty() {
                entry.app_icon = app_icon;
            }
            if device_id.is_some() {
                entry.device_id = device_id;
            }
            if card_profile_device.is_some() {
                entry.card_profile_device = card_profile_device;
            }
            if let Some(channels) = channels {
                entry.channels = channels;
            }
            request_params = kind.is_some() && !entry.params_requested;
            if request_params {
                entry.params_requested = true;
            }
            is_audio_node = kind.is_some();
        }
        if request_params {
            if let Some(entry) = self.nodes.get(&id) {
                entry.proxy.subscribe_params(&[ParamType::Props]);
                entry
                    .proxy
                    .enum_params(0, Some(ParamType::Props), 0, u32::MAX);
            }
        }
        if is_audio_node && node_name_changed {
            self.notify();
        }
    }

    fn on_node_param(&mut self, id: u32, param_type: ParamType, pod: Option<&Pod>) {
        if param_type != ParamType::Props {
            return;
        }
        let Some(pod) = pod else {
            return;
        };
        let Some((volumes, muted)) = parse_props(pod) else {
            return;
        };
        let mut changed = false;
        if let Some(entry) = self.nodes.get_mut(&id) {
            if let Some(volumes) = volumes {
                if !volumes.is_empty() && entry.volumes != volumes {
                    entry.channels = volumes.len() as u32;
                    entry.volumes = volumes;
                    changed = true;
                }
            }
            if let Some(muted) = muted {
                if entry.muted != muted {
                    entry.muted = muted;
                    changed = true;
                }
            }
            changed &= entry.kind.is_some();
        }
        if changed {
            self.notify();
        }
    }

    fn on_device_param(&mut self, device_id: u32, param_type: ParamType, pod: Option<&Pod>) {
        if param_type != ParamType::Route {
            return;
        }
        let Some(pod) = pod else {
            return;
        };
        let Some(route) = parse_route(pod) else {
            return;
        };

        let update = {
            let Some(device) = self.devices.get_mut(&device_id) else {
                return;
            };
            let entry = device
                .routes
                .entry(route.index)
                .or_insert_with(|| route.clone());
            entry.device = route.device;
            if route.direction.is_some() {
                entry.direction = route.direction;
            }
            if !route.volumes.is_empty() {
                entry.volumes = route.volumes;
            }
            if route.muted.is_some() {
                entry.muted = route.muted;
            }
            entry.clone()
        };

        let mut changed = false;
        for node in self.nodes.values_mut() {
            if node.device_id != Some(device_id) || node.card_profile_device != Some(update.device)
            {
                continue;
            }
            let expected_direction = match node.kind {
                Some(Kind::Sink) => spa::sys::SPA_DIRECTION_OUTPUT,
                Some(Kind::Source) => spa::sys::SPA_DIRECTION_INPUT,
                _ => continue,
            };
            if update
                .direction
                .is_some_and(|direction| direction != expected_direction)
            {
                continue;
            }
            if !update.volumes.is_empty() && node.volumes != update.volumes {
                node.channels = update.volumes.len() as u32;
                node.volumes = update.volumes.clone();
                changed = true;
            }
            if let Some(muted) = update.muted {
                if node.muted != muted {
                    node.muted = muted;
                    changed = true;
                }
            }
        }
        if changed {
            self.notify();
        }
    }

    fn on_metadata(&mut self, subject: u32, key: Option<&str>, value: Option<&str>) {
        let mut changed = false;
        match key {
            Some("default.audio.sink") => {
                let name = value.and_then(parse_default_name);
                if self.default_sink_name != name {
                    self.default_sink_name = name;
                    changed = true;
                }
            }
            Some("default.audio.source") => {
                let name = value.and_then(parse_default_name);
                if self.default_source_name != name {
                    self.default_source_name = name;
                    changed = true;
                }
            }
            Some("target.node") if subject != 0 => {
                let target = value.and_then(|v| v.parse::<i32>().ok());
                match target {
                    Some(target) => {
                        if self.targets.insert(subject, target) != Some(target) {
                            changed = true;
                        }
                    }
                    None => {
                        if self.targets.remove(&subject).is_some() {
                            changed = true;
                        }
                    }
                }
            }
            _ => {}
        }
        if changed {
            self.notify();
        }
    }

    fn handle(&mut self, action: Action) {
        match action {
            Action::SetDefaultSink(id) => self.set_default(id, true),
            Action::SetDefaultSource(id) => self.set_default(id, false),
            Action::SetVolume { id, percent } => self.set_volume(id, percent),
            Action::SetMute { id, muted } => self.set_mute(id, muted),
            Action::MoveStream { id, sink_id } => self.move_stream(id, sink_id),
        }
    }

    fn set_default(&mut self, id: u32, sink: bool) {
        let Some(entry) = self.nodes.get(&id) else {
            return;
        };
        if entry.kind != Some(if sink { Kind::Sink } else { Kind::Source }) {
            return;
        }
        let name = entry.node_name.clone();
        let Some(metadata) = self.metadata.as_ref() else {
            return;
        };
        let value = json_name(&name);
        let key = if sink {
            "default.audio.sink"
        } else {
            "default.audio.source"
        };
        let configured = if sink {
            "default.configured.audio.sink"
        } else {
            "default.configured.audio.source"
        };
        metadata.set_property(0, key, Some("Spa:String:JSON"), Some(&value));
        metadata.set_property(0, configured, Some("Spa:String:JSON"), Some(&value));
        if sink {
            self.default_sink_name = Some(name);
        } else {
            self.default_source_name = Some(name);
        }
        self.notify();
    }

    fn set_volume(&mut self, id: u32, percent: f32) {
        let gain = gain_from_percent(percent);
        let (volumes, muted, kind, device_id, card_profile_device) = {
            let Some(entry) = self.nodes.get_mut(&id) else {
                return;
            };
            if entry.kind.is_none() {
                return;
            }
            if entry.volumes.is_empty() {
                entry.volumes = vec![1.0; entry.channels.max(1) as usize];
            }
            for volume in &mut entry.volumes {
                *volume = gain;
            }
            (
                entry.volumes.clone(),
                entry.muted,
                entry.kind.unwrap(),
                entry.device_id,
                entry.card_profile_device,
            )
        };
        self.apply_level(id, kind, device_id, card_profile_device, &volumes, muted);
        self.notify();
    }

    fn set_mute(&mut self, id: u32, muted: bool) {
        let (volumes, kind, device_id, card_profile_device) = {
            let Some(entry) = self.nodes.get_mut(&id) else {
                return;
            };
            if entry.kind.is_none() {
                return;
            }
            if entry.volumes.is_empty() {
                entry.volumes = vec![1.0; entry.channels.max(1) as usize];
            }
            entry.muted = muted;
            let values = (
                entry.volumes.clone(),
                entry.kind.unwrap(),
                entry.device_id,
                entry.card_profile_device,
            );
            values
        };
        self.apply_level(id, kind, device_id, card_profile_device, &volumes, muted);
        self.notify();
    }

    fn apply_level(
        &self,
        id: u32,
        kind: Kind,
        device_id: Option<u32>,
        card_profile_device: Option<i32>,
        volumes: &[f32],
        muted: bool,
    ) {
        let direction = match kind {
            Kind::Sink => Some(spa::sys::SPA_DIRECTION_OUTPUT),
            Kind::Source => Some(spa::sys::SPA_DIRECTION_INPUT),
            Kind::Stream => None,
        };
        if let (Some(device_id), Some(direction)) = (device_id, direction) {
            if let Some(device) = self.devices.get(&device_id) {
                let route = card_profile_device
                    .and_then(|card_device| {
                        device.routes.values().find(|route| {
                            route.device == card_device
                                && route
                                    .direction
                                    .map_or(true, |candidate| candidate == direction)
                        })
                    })
                    .or_else(|| {
                        device.routes.values().find(|route| {
                            route
                                .direction
                                .map_or(true, |candidate| candidate == direction)
                        })
                    });
                if let Some(route) = route {
                    if let Some(bytes) = route_pod(route.index, route.device, volumes, muted) {
                        if let Some(pod) = Pod::from_bytes(&bytes) {
                            device.proxy.set_param(ParamType::Route, 0, pod);
                        }
                    }
                }
            }
        }

        if let Some(entry) = self.nodes.get(&id) {
            if let Some(bytes) = props_pod(volumes, muted) {
                if let Some(pod) = Pod::from_bytes(&bytes) {
                    entry.proxy.set_param(ParamType::Props, 0, pod);
                }
            }
        }
    }

    fn move_stream(&mut self, id: u32, sink_id: Option<u32>) {
        let Some(entry) = self.nodes.get(&id) else {
            return;
        };
        if entry.kind != Some(Kind::Stream) {
            return;
        }
        if let Some(sink_id) = sink_id {
            if !self
                .nodes
                .get(&sink_id)
                .is_some_and(|n| n.kind == Some(Kind::Sink))
            {
                return;
            }
        }
        let Some(metadata) = self.metadata.as_ref() else {
            return;
        };
        let value = sink_id.map_or_else(|| "-1".to_string(), |id| id.to_string());
        metadata.set_property(id, "target.node", Some("Spa:Id"), Some(&value));
        self.targets.insert(id, sink_id.map_or(-1, |id| id as i32));
        self.notify();
    }

    fn remove_global(&mut self, id: u32) {
        let removed = self.nodes.remove(&id).is_some()
            || self.links.remove(&id).is_some()
            || self.devices.remove(&id).is_some();
        self.targets.remove(&id);
        if self.metadata_id == Some(id) {
            self.metadata = None;
            self.metadata_listener = None;
            self.metadata_id = None;
        }
        if removed {
            self.notify();
        }
    }
}

pub fn start() -> (
    pw::channel::Sender<Action>,
    async_channel::Receiver<Snapshot>,
) {
    let (tx, rx) = async_channel::unbounded();
    let (action_tx, action_rx) = pw::channel::channel();
    thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(error) = run(tx.clone(), action_rx) {
                let _ = tx.try_send(Snapshot {
                    error: Some(format!("Cannot connect to PipeWire: {error}")),
                    ..Default::default()
                });
            }
        })
        .expect("failed to spawn PipeWire thread");
    (action_tx, rx)
}

fn run(tx: Sender<Snapshot>, action_rx: pw::channel::Receiver<Action>) -> Result<(), pw::Error> {
    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let state = Rc::new(RefCell::new(State::new(tx.clone())));

    let error_tx = tx.clone();
    let weak = Rc::downgrade(&state);
    let main_loop_weak = main_loop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |_id, _seq| {
            if let Some(state) = weak.upgrade() {
                let mut state = state.borrow_mut();
                if !state.ready {
                    state.ready = true;
                    state.notify();
                }
            }
        })
        .error(move |id, _seq, _res, message| {
            if id == 0 {
                let _ = error_tx.try_send(Snapshot {
                    error: Some(format!("PipeWire: {message}")),
                    ..Default::default()
                });
                if let Some(main_loop) = main_loop_weak.upgrade() {
                    main_loop.quit();
                }
            } else if std::env::var_os("SINK_DEBUG").is_some() {
                eprintln!("sink: proxy {id} error: {message}");
            }
        })
        .register();

    let registry_for_bind = registry.clone();
    let weak = Rc::downgrade(&state);
    let weak_remove = Rc::downgrade(&state);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(state) = weak.upgrade() {
                handle_global(&registry_for_bind, global, &state);
            }
        })
        .global_remove(move |id| {
            if let Some(state) = weak_remove.upgrade() {
                state.borrow_mut().remove_global(id);
            }
        })
        .register();

    let weak = Rc::downgrade(&state);
    let _action_receiver = action_rx.attach(&main_loop.loop_(), move |action| {
        if let Some(state) = weak.upgrade() {
            state.borrow_mut().handle(action);
        }
    });

    core.sync(0)?;
    main_loop.run();
    Ok(())
}

fn handle_global(
    registry: &RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    state: &Rc<RefCell<State>>,
) {
    match global.type_ {
        ObjectType::Device => {
            let Ok(device) = registry.bind::<Device, _>(global) else {
                return;
            };
            let id = global.id;
            let weak = Rc::downgrade(state);
            let listener = device
                .add_listener_local()
                .param(move |_seq, param_type, _index, _next, pod| {
                    if let Some(state) = weak.upgrade() {
                        state.borrow_mut().on_device_param(id, param_type, pod);
                    }
                })
                .register();
            state.borrow_mut().devices.insert(
                id,
                DeviceEntry {
                    proxy: device,
                    _listener: listener,
                    routes: HashMap::new(),
                },
            );
            if let Some(entry) = state.borrow().devices.get(&id) {
                entry.proxy.subscribe_params(&[ParamType::Route]);
                entry
                    .proxy
                    .enum_params(0, Some(ParamType::Route), 0, u32::MAX);
            }
        }
        ObjectType::Node => {
            let Ok(node) = registry.bind::<Node, _>(global) else {
                return;
            };
            let id = global.id;
            let weak_info = Rc::downgrade(state);
            let weak_param = Rc::downgrade(state);
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    if let Some(state) = weak_info.upgrade() {
                        state.borrow_mut().on_node_info(id, info);
                    }
                })
                .param(move |_seq, param_type, _index, _next, pod| {
                    if let Some(state) = weak_param.upgrade() {
                        state.borrow_mut().on_node_param(id, param_type, pod);
                    }
                })
                .register();
            state
                .borrow_mut()
                .nodes
                .insert(id, NodeEntry::new(node, listener));
        }
        ObjectType::Link => {
            let Ok(link) = registry.bind::<Link, _>(global) else {
                return;
            };
            let id = global.id;
            let weak = Rc::downgrade(state);
            let listener = link
                .add_listener_local()
                .info(move |info| {
                    if let Some(state) = weak.upgrade() {
                        state.borrow_mut().on_link_info(id, info);
                    }
                })
                .register();
            state.borrow_mut().links.insert(
                id,
                LinkEntry {
                    _proxy: link,
                    _listener: listener,
                    output_node: None,
                    input_node: None,
                },
            );
        }
        ObjectType::Metadata => {
            let is_default =
                global.props.and_then(|props| props.get("metadata.name")) == Some("default");
            if !is_default || state.borrow().metadata.is_some() {
                return;
            }
            let Ok(metadata) = registry.bind::<Metadata, _>(global) else {
                return;
            };
            let weak = Rc::downgrade(state);
            let listener = metadata
                .add_listener_local()
                .property(move |subject, key, _type, value| {
                    if let Some(state) = weak.upgrade() {
                        state.borrow_mut().on_metadata(subject, key, value);
                    }
                    0
                })
                .register();
            let mut state = state.borrow_mut();
            state.metadata_id = Some(global.id);
            state.metadata = Some(metadata);
            state.metadata_listener = Some(listener);
        }
        _ => {}
    }
}

impl State {
    fn on_link_info(&mut self, id: u32, info: &LinkInfoRef) {
        let changed = {
            let Some(entry) = self.links.get_mut(&id) else {
                return;
            };
            let output = info.output_node_id();
            let input = info.input_node_id();
            if entry.output_node == Some(output) && entry.input_node == Some(input) {
                false
            } else {
                entry.output_node = Some(output);
                entry.input_node = Some(input);
                true
            }
        };
        if changed {
            self.notify();
        }
    }
}

fn parse_props(pod: &Pod) -> Option<(Option<Vec<f32>>, Option<bool>)> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };
    Some(parse_props_object(object))
}

fn parse_props_object(object: Object) -> (Option<Vec<f32>>, Option<bool>) {
    let mut volumes = None;
    let mut muted = None;
    for property in object.properties {
        match property.key {
            spa::sys::SPA_PROP_channelVolumes => {
                if let Value::ValueArray(ValueArray::Float(values)) = property.value {
                    volumes = Some(values);
                }
            }
            spa::sys::SPA_PROP_mute => {
                if let Value::Bool(value) = property.value {
                    muted = Some(value);
                }
            }
            _ => {}
        }
    }
    (volumes, muted)
}

fn parse_route(pod: &Pod) -> Option<RouteEntry> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };
    if object.type_ != spa::sys::SPA_TYPE_OBJECT_ParamRoute {
        return None;
    }

    let mut index = None;
    let mut device = None;
    let mut direction = None;
    let mut volumes = Vec::new();
    let mut muted = None;
    for property in object.properties {
        match property.key {
            spa::sys::SPA_PARAM_ROUTE_index => {
                if let Value::Int(value) = property.value {
                    index = Some(value);
                }
            }
            spa::sys::SPA_PARAM_ROUTE_device => {
                if let Value::Int(value) = property.value {
                    device = Some(value);
                }
            }
            spa::sys::SPA_PARAM_ROUTE_direction => {
                if let Value::Id(value) = property.value {
                    direction = Some(value.0);
                }
            }
            spa::sys::SPA_PARAM_ROUTE_props => {
                if let Value::Object(props) = property.value {
                    let (route_volumes, route_muted) = parse_props_object(props);
                    if let Some(route_volumes) = route_volumes {
                        volumes = route_volumes;
                    }
                    muted = route_muted;
                }
            }
            _ => {}
        }
    }

    Some(RouteEntry {
        index: index?,
        device: device?,
        direction,
        volumes,
        muted,
    })
}

fn props_pod(volumes: &[f32], muted: bool) -> Option<Vec<u8>> {
    let value = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Props,
        id: ParamType::Props.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            },
            Property {
                key: spa::sys::SPA_PROP_softVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            },
            Property {
                key: spa::sys::SPA_PROP_mute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(muted),
            },
            Property {
                key: spa::sys::SPA_PROP_softMute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(muted),
            },
        ],
    });
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &value).ok()?;
    Some(cursor.into_inner())
}

fn route_pod(index: i32, device: i32, volumes: &[f32], muted: bool) -> Option<Vec<u8>> {
    let props = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Props,
        id: ParamType::Props.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            },
            Property {
                key: spa::sys::SPA_PROP_mute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(muted),
            },
        ],
    });
    let value = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamRoute,
        id: ParamType::Route.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_ROUTE_index,
                flags: PropertyFlags::empty(),
                value: Value::Int(index),
            },
            Property {
                key: spa::sys::SPA_PARAM_ROUTE_device,
                flags: PropertyFlags::empty(),
                value: Value::Int(device),
            },
            Property {
                key: spa::sys::SPA_PARAM_ROUTE_props,
                flags: PropertyFlags::empty(),
                value: props,
            },
            Property {
                key: spa::sys::SPA_PARAM_ROUTE_save,
                flags: PropertyFlags::empty(),
                value: Value::Bool(true),
            },
        ],
    });
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &value).ok()?;
    Some(cursor.into_inner())
}

fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_pod_roundtrips() {
        let bytes = props_pod(&[0.125, 0.125], true).unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let (volumes, muted) = parse_props(pod).unwrap();
        assert_eq!(volumes.unwrap(), vec![0.125, 0.125]);
        assert_eq!(muted, Some(true));
    }

    #[test]
    fn route_pod_roundtrips() {
        let bytes = route_pod(2, 4, &[0.25, 0.25], true).unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let route = parse_route(pod).unwrap();
        assert_eq!(route.index, 2);
        assert_eq!(route.device, 4);
        assert_eq!(route.volumes, vec![0.25, 0.25]);
        assert_eq!(route.muted, Some(true));
    }

    #[test]
    fn recognizes_virtual_audio_nodes() {
        assert_eq!(Kind::from_class("Audio/Sink"), Some(Kind::Sink));
        assert_eq!(Kind::from_class("Audio/Sink/Virtual"), Some(Kind::Sink));
        assert_eq!(Kind::from_class("Audio/Source"), Some(Kind::Source));
        assert_eq!(Kind::from_class("Audio/Source/Virtual"), Some(Kind::Source));
        assert_eq!(Kind::from_class("Stream/Output/Audio"), Some(Kind::Stream));
        assert_eq!(Kind::from_class("Video/Source"), None);
    }

    #[test]
    #[ignore = "requires a live PipeWire session and wpctl"]
    fn live_actions_roundtrip() {
        use crate::model::Action;
        use std::process::Command;
        use std::time::Duration;

        fn wait_for(
            rx: &async_channel::Receiver<Snapshot>,
            predicate: impl Fn(&Snapshot) -> bool,
        ) -> Snapshot {
            for _ in 0..500 {
                let snapshot = rx.recv_blocking().expect("backend closed");
                if let Some(error) = snapshot.error {
                    panic!("backend error: {error}");
                }
                if snapshot.ready && predicate(&snapshot) {
                    return snapshot;
                }
            }
            panic!("timed out waiting for a matching snapshot");
        }

        fn wpctl_volume(id: u32) -> f32 {
            let output = Command::new("wpctl")
                .args(["get-volume", &id.to_string()])
                .output()
                .expect("wpctl get-volume");
            let text = String::from_utf8_lossy(&output.stdout);
            let value: f32 = text
                .split_whitespace()
                .nth(1)
                .expect("wpctl volume field")
                .parse()
                .expect("wpctl volume number");
            value * 100.0
        }

        fn wpctl_muted(id: u32) -> bool {
            let output = Command::new("wpctl")
                .args(["get-volume", &id.to_string()])
                .output()
                .expect("wpctl get-volume");
            String::from_utf8_lossy(&output.stdout).contains("[MUTED]")
        }

        let (action_tx, rx) = start();
        let first = wait_for(&rx, |snapshot| {
            snapshot.sinks.iter().any(|sink| sink.is_default)
                && snapshot.sinks.iter().any(|sink| !sink.is_default)
        });
        let device = first
            .sinks
            .iter()
            .find(|device| !device.is_default)
            .expect("a non-default sink")
            .clone();

        let original_volume = wpctl_volume(device.id);
        let target = if (original_volume - 37.0).abs() < 2.0 {
            43.0
        } else {
            37.0
        };
        action_tx
            .send(Action::SetVolume {
                id: device.id,
                percent: target,
            })
            .expect("send volume");
        wait_for(&rx, |snapshot| {
            snapshot
                .sinks
                .iter()
                .any(|sink| sink.id == device.id && (sink.volume - target).abs() < 0.6)
        });
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            (wpctl_volume(device.id) - target).abs() < 0.7,
            "volume write did not reach PipeWire"
        );

        let original_muted = wpctl_muted(device.id);
        action_tx
            .send(Action::SetMute {
                id: device.id,
                muted: !original_muted,
            })
            .expect("send mute");
        wait_for(&rx, |snapshot| {
            snapshot
                .sinks
                .iter()
                .any(|sink| sink.id == device.id && sink.muted == !original_muted)
        });
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(wpctl_muted(device.id), !original_muted);

        // Put everything back the way we found it.
        action_tx
            .send(Action::SetMute {
                id: device.id,
                muted: original_muted,
            })
            .expect("restore mute");
        wait_for(&rx, |snapshot| {
            snapshot
                .sinks
                .iter()
                .any(|sink| sink.id == device.id && sink.muted == original_muted)
        });
        action_tx
            .send(Action::SetVolume {
                id: device.id,
                percent: original_volume,
            })
            .expect("restore volume");
        let latest = wait_for(&rx, |snapshot| {
            snapshot
                .sinks
                .iter()
                .any(|sink| sink.id == device.id && (sink.volume - original_volume).abs() < 1.0)
        });
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            (wpctl_volume(device.id) - original_volume).abs() < 0.7,
            "volume was not restored"
        );
        assert_eq!(
            wpctl_muted(device.id),
            original_muted,
            "mute was not restored"
        );

        if let Some(default) = latest.sinks.iter().find(|sink| sink.is_default) {
            action_tx
                .send(Action::SetDefaultSink(default.id))
                .expect("send default");
        }

        if let Some(app) = latest.apps.iter().find(|app| app.sink_id.is_some()) {
            let sink_id = app.sink_id.unwrap();
            action_tx
                .send(Action::MoveStream {
                    id: app.id,
                    sink_id: Some(sink_id),
                })
                .expect("pin stream");
            wait_for(&rx, |snapshot| {
                snapshot
                    .apps
                    .iter()
                    .any(|candidate| candidate.id == app.id && !candidate.follow_default)
            });
            action_tx
                .send(Action::MoveStream {
                    id: app.id,
                    sink_id: None,
                })
                .expect("unpin stream");
            wait_for(&rx, |snapshot| {
                snapshot
                    .apps
                    .iter()
                    .any(|candidate| candidate.id == app.id && candidate.follow_default)
            });
        }
    }
}
