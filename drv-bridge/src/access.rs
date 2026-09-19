//! Microphones and cameras. PipeWire's own permissions do the gating, in WirePlumber's
//! drv-access.lua, keyed on the uid its clients came in with. This is the bridge's half:
//! through WirePlumber's "drv-access" metadata it hears it ask (`request:<uid>:<kind>`),
//! asks the person at drv-portal, and writes what they allow (`grant:<uid>`). Cameras
//! also come the portal way: `AccessCamera` asks the same question, and the remote that
//! follows sees the camera nodes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::{process, thread};

use anyhow::Context as _;
use drv_portal::protocol::Device;
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::{Metadata, MetadataListener};
use pipewire::properties::properties;
use pipewire::types::ObjectType;

/// WirePlumber's metadata object for this, and the value a client came in with through
/// the apps' socket (set by the daemon from the socket, not by the client).
pub const METADATA: &str = "drv-access";
pub const ACCESS: &str = "drv-app";

/// What the person said, or did later.
pub enum Answer {
    Granted,
    Refused,
    Revoked,
}

/// Whoever puts the question to the person: drv-portal, through the bridge's line.
pub trait Prompter: Send + Sync {
    /// `on` hears `Granted` or `Refused` once, and `Revoked` later if the person takes it
    /// back.
    fn grant(&self, app: &str, uid: u32, device: Device, on: Box<dyn Fn(Answer) + Send + Sync>) -> anyhow::Result<u64>;
    fn cancel(&self, id: u64);
}

fn kind(device: Device) -> &'static str {
    match device {
        Device::Microphone => "mic",
        Device::Camera => "camera",
    }
}

fn device(kind: &str) -> Option<Device> {
    match kind {
        "mic" => Some(Device::Microphone),
        "camera" => Some(Device::Camera),
        _ => None,
    }
}

/// To the PipeWire thread.
enum Cmd {
    /// `grant:<uid>` becomes these kinds (none clears it).
    Write { uid: u32, kinds: String },
    /// `request:<uid>:<kind>` is answered.
    Answered { uid: u32, device: Device },
    /// Everything of this uid goes.
    Forget { uid: u32 },
    /// Disconnect a camera remote.
    Drop { client: u32 },
    Cameras(mpsc::Sender<Vec<u32>>),
    /// A remote handed out: what its streams may reach, under its client id; answered
    /// once PipeWire has it, so it is in before the app can act on the fd.
    Mark { client: u32, what: String, done: mpsc::Sender<()> },
}

#[derive(Default)]
struct State {
    /// By uid: each allowed device and the id of its consent at drv-portal.
    granted: HashMap<u32, HashMap<Device, u64>>,
    /// Questions up at drv-portal.
    asking: HashMap<(u32, Device), u64>,
    /// `AccessCamera` callers waiting on the question.
    waiting: HashMap<(u32, Device), Vec<Box<dyn FnOnce(bool) + Send>>>,
    /// Camera remotes handed out, by uid: their PipeWire client ids.
    remotes: HashMap<u32, Vec<u32>>,
}

pub struct Access {
    tx: pipewire::channel::Sender<Cmd>,
    prompter: Arc<dyn Prompter>,
    name_of: Box<dyn Fn(u32) -> Option<String> + Send + Sync>,
    state: Mutex<State>,
}

impl Access {
    /// Connects to PipeWire (the manager socket: WirePlumber leaves that connection alone)
    /// on its own thread. Losing it ends us: the set restarts as one.
    pub fn start(
        prompter: Arc<dyn Prompter>,
        name_of: impl Fn(u32) -> Option<String> + Send + Sync + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let (tx, rx) = pipewire::channel::channel();
        let access = Arc::new(Self {
            tx,
            prompter,
            name_of: Box::new(name_of),
            state: Mutex::new(State::default()),
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let me = access.clone();
        thread::spawn(move || {
            if let Err(err) = me.run(rx, ready_tx) {
                drv_os::say!("bridge: PipeWire: {err:#}");
            }
            process::exit(1);
        });
        ready_rx.recv().context("the PipeWire thread")??;
        Ok(access)
    }

    fn run(self: &Arc<Self>, rx: pipewire::channel::Receiver<Cmd>, ready: mpsc::Sender<anyhow::Result<()>>) -> anyhow::Result<()> {
        let main_loop = MainLoopRc::new(None).context("PipeWire main loop")?;
        let context = ContextRc::new(&main_loop, None).context("PipeWire context")?;
        let props = properties! {
            *pipewire::keys::REMOTE_NAME => "pipewire-0-manager",
            *pipewire::keys::APP_NAME => "drv-bridge",
        };
        let core = context.connect_rc(Some(props)).context("connecting to PipeWire")?;
        let registry = core.get_registry_rc().context("PipeWire registry")?;
        // WirePlumber's metadata, while it is there, and the grants to write into it: all of
        // them again whenever it comes back.
        let metadata: Rc<RefCell<Option<(Metadata, MetadataListener)>>> = Rc::default();
        let metadata_id: Rc<Cell<Option<u32>>> = Rc::default();
        let grants: Rc<RefCell<HashMap<u32, String>>> = Rc::default();
        // The apps' clients (id to uid) and the cameras.
        let clients: Rc<RefCell<HashMap<u32, u32>>> = Rc::default();
        let cameras: Rc<RefCell<HashSet<u32>>> = Rc::default();
        // Remotes marked in the metadata: the mark goes with the client, since PipeWire
        // reuses ids.
        let marks: Rc<RefCell<HashSet<u32>>> = Rc::default();
        let (marks2, metadata3) = (marks.clone(), metadata.clone());
        let _globals = {
            let (clients, cameras) = (clients.clone(), cameras.clone());
            let (clients2, cameras2) = (clients.clone(), cameras.clone());
            let (metadata, metadata2) = (metadata.clone(), metadata.clone());
            let (metadata_id, metadata_id2) = (metadata_id.clone(), metadata_id.clone());
            let grants = grants.clone();
            let registry2 = registry.clone();
            let me = self.clone();
            let me2 = self.clone();
            registry
                .add_listener_local()
                .global(move |g| {
                    let Some(props) = g.props else { return };
                    match g.type_ {
                        ObjectType::Client if props.get("pipewire.access") == Some(ACCESS) => {
                            if let Some(uid) = props.get("pipewire.sec.uid").and_then(|s| s.parse().ok()) {
                                clients.borrow_mut().insert(g.id, uid);
                            }
                        }
                        ObjectType::Node if props.get("media.class").is_some_and(|c| c.starts_with("Video/Source")) => {
                            cameras.borrow_mut().insert(g.id);
                        }
                        ObjectType::Metadata if props.get("metadata.name") == Some(METADATA) => {
                            let bound: Metadata = match registry2.bind(g) {
                                Ok(m) => m,
                                Err(err) => {
                                    drv_os::say!("bridge: binding the {METADATA} metadata: {err}");
                                    return;
                                }
                            };
                            let me = me.clone();
                            let listener = bound
                                .add_listener_local()
                                .property(move |subject, key, _type, value| {
                                    if subject == 0 && value.is_some() {
                                        if let Some((uid, device)) = key.and_then(parse_request) {
                                            me.asked(uid, device);
                                        }
                                    }
                                    0
                                })
                                .register();
                            for (uid, kinds) in grants.borrow().iter() {
                                bound.set_property(0, &format!("grant:{uid}"), None, Some(kinds));
                            }
                            *metadata.borrow_mut() = Some((bound, listener));
                            metadata_id.set(Some(g.id));
                        }
                        _ => {}
                    }
                })
                .global_remove(move |id| {
                    cameras2.borrow_mut().remove(&id);
                    if marks2.borrow_mut().remove(&id) {
                        if let Some((m, _)) = metadata2.borrow().as_ref() {
                            m.set_property(id, "drv.remote", None, None);
                        }
                    }
                    if metadata_id2.get() == Some(id) {
                        metadata_id2.set(None);
                        *metadata2.borrow_mut() = None;
                    }
                    let gone = clients2.borrow_mut().remove(&id);
                    if let Some(uid) = gone {
                        // Its last connection: what it was allowed ends, as with an app that
                        // closed its bus.
                        if !clients2.borrow().values().any(|u| *u == uid) {
                            me2.forget(uid);
                        }
                    }
                })
                .register()
        };
        let set = move |key: &str, value: Option<&str>| {
            if let Some((m, _)) = metadata.borrow().as_ref() {
                m.set_property(0, key, None, value);
            }
        };
        // Marks waiting for their round trip, by sequence number.
        let pending: Rc<RefCell<Vec<(pipewire::spa::utils::result::AsyncSeq, mpsc::Sender<()>)>>> = Rc::default();
        let _done = {
            let pending = pending.clone();
            core.add_listener_local()
                .done(move |id, seq| {
                    if id == pipewire::core::PW_ID_CORE {
                        let mut pending = pending.borrow_mut();
                        if let Some(i) = pending.iter().position(|(s, _)| *s == seq) {
                            let _ = pending.remove(i).1.send(());
                        }
                    }
                })
                .register()
        };
        let core2 = core.clone();
        let _cmds = rx.attach(main_loop.loop_(), move |cmd| match cmd {
            Cmd::Mark { client, what, done } => {
                match metadata3.borrow().as_ref() {
                    Some((m, _)) => m.set_property(client, "drv.remote", None, Some(&what)),
                    // Dropped `done` answers the caller with an error.
                    None => return drv_os::say!("bridge: no {METADATA} metadata to mark a remote in"),
                }
                marks.borrow_mut().insert(client);
                match core2.sync(0) {
                    Ok(seq) => {
                        pending.borrow_mut().push((seq, done));
                    }
                    Err(err) => drv_os::say!("bridge: PipeWire sync: {err}"),
                }
            }
            Cmd::Write { uid, kinds } => {
                if kinds.is_empty() {
                    grants.borrow_mut().remove(&uid);
                    set(&format!("grant:{uid}"), None);
                } else {
                    set(&format!("grant:{uid}"), Some(&kinds));
                    grants.borrow_mut().insert(uid, kinds);
                }
            }
            Cmd::Answered { uid, device } => {
                set(&format!("request:{uid}:{}", kind(device)), None);
            }
            Cmd::Forget { uid } => {
                grants.borrow_mut().remove(&uid);
                set(&format!("grant:{uid}"), None);
                for device in [Device::Microphone, Device::Camera] {
                    set(&format!("request:{uid}:{}", kind(device)), None);
                }
            }
            Cmd::Drop { client } => {
                registry.destroy_global(client);
            }
            Cmd::Cameras(reply) => {
                let _ = reply.send(cameras.borrow().iter().copied().collect());
            }
        });
        let _ = ready.send(Ok(()));
        main_loop.run();
        anyhow::bail!("the loop ended")
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            drv_os::say!("bridge: the PipeWire thread is gone");
            process::exit(1);
        }
    }

    fn write(&self, st: &State, uid: u32) {
        let kinds = st
            .granted
            .get(&uid)
            .map(|g| g.keys().map(|d| kind(*d)).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        self.send(Cmd::Write { uid, kinds });
    }

    /// Does the person allow it now?
    pub fn has(&self, uid: u32, device: Device) -> bool {
        self.state.lock().unwrap().granted.get(&uid).is_some_and(|g| g.contains_key(&device))
    }

    /// WirePlumber has a stream waiting on this.
    fn asked(self: &Arc<Self>, uid: u32, device: Device) {
        let Some(app) = (self.name_of)(uid) else {
            drv_os::say!("bridge: uid {uid} asks for the {}: not an app", kind(device));
            self.send(Cmd::Answered { uid, device });
            return;
        };
        self.ask(&app, uid, device, None);
    }

    /// One question per app and device at a time; `done`, if given, hears the answer.
    fn ask(self: &Arc<Self>, app: &str, uid: u32, device: Device, done: Option<Box<dyn FnOnce(bool) + Send>>) {
        let mut st = self.state.lock().unwrap();
        if st.granted.get(&uid).is_some_and(|g| g.contains_key(&device)) {
            drop(st);
            // Told again, for a request that came before the grant was written.
            self.send(Cmd::Answered { uid, device });
            if let Some(done) = done {
                done(true);
            }
            return;
        }
        if let Some(done) = done {
            st.waiting.entry((uid, device)).or_default().push(done);
        }
        if st.asking.contains_key(&(uid, device)) {
            return;
        }
        let me = self.clone();
        let on = Box::new(move |answer| me.answered(uid, device, answer));
        match self.prompter.grant(app, uid, device, on) {
            Ok(id) => {
                st.asking.insert((uid, device), id);
            }
            Err(err) => {
                drv_os::say!("bridge: {app}: asking for the {}: {err:#}", kind(device));
                let waiting = st.waiting.remove(&(uid, device)).unwrap_or_default();
                drop(st);
                self.send(Cmd::Answered { uid, device });
                for done in waiting {
                    done(false);
                }
            }
        }
    }

    fn answered(&self, uid: u32, device: Device, answer: Answer) {
        let mut st = self.state.lock().unwrap();
        match answer {
            Answer::Granted => {
                let id = st.asking.remove(&(uid, device)).unwrap_or(0);
                st.granted.entry(uid).or_default().insert(device, id);
                self.write(&st, uid);
                self.send(Cmd::Answered { uid, device });
                let waiting = st.waiting.remove(&(uid, device)).unwrap_or_default();
                drop(st);
                for done in waiting {
                    done(true);
                }
            }
            Answer::Refused => {
                st.asking.remove(&(uid, device));
                self.send(Cmd::Answered { uid, device });
                let waiting = st.waiting.remove(&(uid, device)).unwrap_or_default();
                drop(st);
                for done in waiting {
                    done(false);
                }
            }
            Answer::Revoked => {
                if let Some(g) = st.granted.get_mut(&uid) {
                    g.remove(&device);
                }
                self.write(&st, uid);
                if device == Device::Camera {
                    for client in st.remotes.remove(&uid).unwrap_or_default() {
                        self.send(Cmd::Drop { client });
                    }
                }
            }
        }
    }

    /// `AccessCamera`: the answer comes to `done`, at once if it stands already.
    pub fn camera(self: &Arc<Self>, app: &str, uid: u32, done: impl FnOnce(bool) + Send + 'static) {
        self.ask(app, uid, Device::Camera, Some(Box::new(done)));
    }

    /// The camera nodes, for a remote.
    pub fn cameras(&self) -> Vec<u32> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Cameras(tx));
        rx.recv().unwrap_or_default()
    }

    /// A remote went out as PipeWire client `client`: its streams may reach `what` only
    /// ("camera", or "node:<id>"), which WirePlumber enforces. Back once that is in.
    pub fn mark(&self, client: u32, what: &str) -> anyhow::Result<()> {
        let (done, back) = mpsc::channel();
        self.send(Cmd::Mark { client, what: what.to_owned(), done });
        back.recv().context("marking the remote")
    }

    /// Disconnect a remote.
    pub fn drop_client(&self, client: u32) {
        self.send(Cmd::Drop { client });
    }

    /// A camera remote went to `uid`: revoking the camera disconnects it.
    pub fn remote(&self, uid: u32, client: u32) {
        self.state.lock().unwrap().remotes.entry(uid).or_default().push(client);
    }

    /// The app is gone: its consents end, its questions come down.
    pub fn forget(&self, uid: u32) {
        let mut st = self.state.lock().unwrap();
        let ids: Vec<u64> = st
            .granted
            .remove(&uid)
            .map(|g| g.into_values().collect())
            .unwrap_or_default();
        let asking: Vec<((u32, Device), u64)> = st.asking.iter().filter(|((u, _), _)| *u == uid).map(|(k, v)| (*k, *v)).collect();
        for (key, _) in &asking {
            st.asking.remove(key);
            st.waiting.remove(key);
        }
        let remotes = st.remotes.remove(&uid).unwrap_or_default();
        drop(st);
        for id in ids.into_iter().chain(asking.into_iter().map(|(_, id)| id)) {
            self.prompter.cancel(id);
        }
        for client in remotes {
            self.send(Cmd::Drop { client });
        }
        self.send(Cmd::Forget { uid });
    }
}

fn parse_request(key: &str) -> Option<(u32, Device)> {
    let rest = key.strip_prefix("request:")?;
    let (uid, kind) = rest.split_once(':')?;
    Some((uid.parse().ok()?, device(kind)?))
}
