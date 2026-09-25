//! PipeWire, on its own thread. Two things: the grants WirePlumber enforces (drv-access.lua,
//! keyed on the uid its clients came in with), written into its "drv-access" metadata as
//! `grant:<uid>`, with its questions (`request:<uid>:<kind>`) coming back as events; and
//! the remotes handed to apps, connections of our own cut down to a few nodes, marked in the
//! same metadata so WirePlumber leaves them alone.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{process, thread};

use anyhow::Context as _;
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::{Metadata, MetadataListener};
use pipewire::properties::properties;
use pipewire::types::ObjectType;

use crate::{Device, Event};

/// WirePlumber's metadata object for this, and the value a client came in with through
/// the apps' socket (set by the daemon from the socket, not by the client).
pub const METADATA: &str = "drv-access";
pub const ACCESS: &str = "drv-app";

pub fn kind(device: Device) -> &'static str {
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
    /// Disconnect a remote.
    Drop { client: u32 },
    Cameras(mpsc::Sender<Vec<u32>>),
    /// A remote handed out: what its streams may reach, under its client id; answered
    /// once PipeWire has it, so it is in before the app can act on the fd.
    Mark { client: u32, what: String, done: mpsc::Sender<()> },
}

#[derive(Clone)]
pub struct Pw {
    tx: pipewire::channel::Sender<Cmd>,
}

impl Pw {
    /// Connects to PipeWire (the manager socket: WirePlumber leaves that connection alone)
    /// on its own thread. Losing it ends us: the set restarts as one.
    pub fn start(events: mpsc::Sender<Event>) -> anyhow::Result<Self> {
        let (tx, rx) = pipewire::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || {
            if let Err(err) = run(rx, ready_tx, events) {
                drv_os::say!("drv-cast: PipeWire: {err:#}");
            }
            process::exit(1);
        });
        ready_rx.recv().context("the PipeWire thread")??;
        Ok(Self { tx })
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            drv_os::say!("drv-cast: the PipeWire thread is gone");
            process::exit(1);
        }
    }

    /// What `uid` may capture now.
    pub fn write(&self, uid: u32, devices: impl Iterator<Item = Device>) {
        let kinds = devices.map(kind).collect::<Vec<_>>().join(" ");
        self.send(Cmd::Write { uid, kinds });
    }

    pub fn answered(&self, uid: u32, device: Device) {
        self.send(Cmd::Answered { uid, device });
    }

    pub fn forget(&self, uid: u32) {
        self.send(Cmd::Forget { uid });
    }

    pub fn drop_client(&self, client: u32) {
        self.send(Cmd::Drop { client });
    }

    /// The camera nodes, for a remote.
    pub fn cameras(&self) -> Vec<u32> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Cameras(tx));
        rx.recv().unwrap_or_default()
    }

    /// A remote went out as PipeWire client `client`: its streams may reach `what` only
    /// ("camera:<uid>", the cameras under the app's grant, or "node:<id>"), which
    /// WirePlumber enforces. Back once that is in.
    pub fn mark(&self, client: u32, what: &str) -> anyhow::Result<()> {
        let (done, back) = mpsc::channel();
        self.send(Cmd::Mark { client, what: what.to_owned(), done });
        back.recv().context("marking the remote")
    }
}

fn run(
    rx: pipewire::channel::Receiver<Cmd>,
    ready: mpsc::Sender<anyhow::Result<()>>,
    events: mpsc::Sender<Event>,
) -> anyhow::Result<()> {
    let main_loop = MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = ContextRc::new(&main_loop, None).context("PipeWire context")?;
    let props = properties! {
        *pipewire::keys::REMOTE_NAME => "pipewire-0-manager",
        *pipewire::keys::APP_NAME => "drv-cast",
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
        let (events, events2) = (events.clone(), events.clone());
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
                                drv_os::say!("drv-cast: binding the {METADATA} metadata: {err}");
                                return;
                            }
                        };
                        let events = events.clone();
                        let listener = bound
                            .add_listener_local()
                            .property(move |subject, key, _type, value| {
                                if subject == 0 && value.is_some() {
                                    if let Some((uid, device)) = key.and_then(parse_request) {
                                        let _ = events.send(Event::PwAsked { uid, device });
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
                    // closed its socket.
                    if !clients2.borrow().values().any(|u| *u == uid) {
                        let _ = events2.send(Event::PwClientsGone { uid });
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
                None => return drv_os::say!("drv-cast: no {METADATA} metadata to mark a remote in"),
            }
            marks.borrow_mut().insert(client);
            match core2.sync(0) {
                Ok(seq) => {
                    pending.borrow_mut().push((seq, done));
                }
                Err(err) => drv_os::say!("drv-cast: PipeWire sync: {err}"),
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

fn parse_request(key: &str) -> Option<(u32, Device)> {
    let rest = key.strip_prefix("request:")?;
    let (uid, kind) = rest.split_once(':')?;
    Some((uid.parse().ok()?, device(kind)?))
}

// ---------------------------------------------------------------- remotes

/// A PipeWire connection an app may have: it sees the core, these nodes and the factory
/// for its own stream node, nothing else. The permissions live in the daemon, so they hold
/// whatever the app does with the fd. With it, the daemon's id for the connection, to end
/// it later.
fn remote(nodes: &[u32]) -> anyhow::Result<(OwnedFd, u32)> {
    use pipewire::core::PW_ID_CORE;
    use pipewire::loop_::Timeout;
    use pipewire::permissions::{Permission, PermissionFlags};

    let main_loop = MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = ContextRc::new(&main_loop, None).context("PipeWire context")?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    // A round trip: what was sent before it is in.
    let roundtrip = |what: &str| -> anyhow::Result<()> {
        let done = Rc::new(Cell::new(false));
        let pending = core.sync(0).context("PipeWire sync")?;
        let _listener = {
            let done = done.clone();
            core.add_listener_local()
                .done(move |id, seq| {
                    if id == PW_ID_CORE && seq == pending {
                        done.set(true);
                    }
                })
                .error(|id, seq, res, message| {
                    drv_os::say!("drv-cast: PipeWire error on {id} ({seq}): {res} {message}");
                })
                .register()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done.get() {
            anyhow::ensure!(Instant::now() < deadline, "PipeWire did not answer ({what})");
            main_loop.loop_().iterate(Timeout::Finite(Duration::from_millis(200)));
        }
        Ok(())
    };
    // The app makes its own stream node through the client-node factory, which it must
    // be able to name: that one factory stays visible.
    let registry = core.get_registry_rc().context("PipeWire registry")?;
    let factory = Rc::new(Cell::new(0u32));
    let _globals = {
        let factory = factory.clone();
        registry
            .add_listener_local()
            .global(move |g| {
                let client_node = g.props.is_some_and(|p| {
                    p.get("factory.type.name") == Some("PipeWire:Interface:ClientNode")
                });
                if g.type_ == ObjectType::Factory && client_node {
                    factory.set(g.id);
                }
            })
            .register()
    };
    roundtrip("registry")?;
    anyhow::ensure!(factory.get() != 0, "PipeWire has no client-node factory");
    // SAFETY: the core is connected; the client proxy it returns lives as long as the core.
    let client = unsafe { pipewire::sys::pw_core_get_client(core.as_raw_ptr()) };
    anyhow::ensure!(!client.is_null(), "PipeWire gave no client");
    let rwx = PermissionFlags::R | PermissionFlags::W | PermissionFlags::X;
    let mut perms = vec![Permission::new(PW_ID_CORE, rwx), Permission::new(factory.get(), PermissionFlags::R)];
    // The nodes: seen and read (their formats), not changed; linking is WirePlumber's.
    perms.extend(nodes.iter().map(|node| Permission::new(*node, PermissionFlags::R | PermissionFlags::X)));
    perms.push(Permission::new(pipewire::sys::PW_ID_ANY, PermissionFlags::empty()));
    // SAFETY: a live client proxy, and the array is `pw_permission` in memory.
    unsafe {
        pipewire::spa::spa_interface_call_method!(
            client,
            pipewire::sys::pw_client_methods,
            update_permissions,
            perms.len() as u32,
            perms.as_ptr().cast()
        );
    }
    // So the permissions are in before the fd changes hands.
    roundtrip("permissions")?;
    // SAFETY: the client proxy is live; bound after the round trip.
    let client_id = unsafe { pipewire::sys::pw_proxy_get_bound_id(client.cast()) };
    drop(_globals);
    drop(registry);
    // SAFETY: after steal_fd the core no longer owns the fd; nothing else here uses it.
    let fd = unsafe { pipewire::sys::pw_core_steal_fd(core.as_raw_ptr()) };
    anyhow::ensure!(fd >= 0, "PipeWire kept its fd");
    // SAFETY: a valid fd just stolen from the core, owned by nobody else.
    Ok((unsafe { OwnedFd::from_raw_fd(fd) }, client_id))
}

/// Seen once in many tries: a round trip that never came back. A fresh connection is
/// cheap, and the app would otherwise drop the whole share.
pub fn remote_twice(nodes: &[u32], who: &str) -> anyhow::Result<(OwnedFd, u32)> {
    remote(nodes).or_else(|err| {
        drv_os::say!("drv-cast: {who}: {err:#}; once more");
        remote(nodes)
    })
}
