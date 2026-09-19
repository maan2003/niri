//! The trusted side: one thread per connected app, keyed on its UID. Reads `wire`, asks
//! drv-portal, hands out PipeWire remotes, launches URI handlers, relays notifications.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use drv_bridge::wire::{self, clip, ToServer, ToShim};
use drv_os::fds::Kind;
use drv_policy::{rpc, seq, AppPolicy, PolicyClient};
use drv_portal::protocol::{self, Device};
use zbus::blocking::Connection;
use zbus::zvariant::Value;

use crate::access::{self, Access};
use crate::TRACE;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub fn serve(identity: PathBuf) -> anyhow::Result<()> {
    let mut fds = drv_os::fds::take().context("fds from the supervisor")?;
    // Bound by the supervisor, any UID may connect; who they are is decided per connection.
    let listener = fds.listener_of("listener", Kind::SeqPacket)?;
    let portal = Portal::start(fds.socket("portal", Kind::SeqPacket)?)?;
    // drv-appd starts a URI's handler for us; the app names nothing but the URI.
    let launcher = Arc::new(Mutex::new(
        PolicyClient::from_stream(UnixStream::from(fds.socket("appd", Kind::Stream)?)).context("launch channel")?,
    ));
    // For the remotes, and the microphone and camera consents.
    pipewire::init();
    // Fail at startup, not on the first app, if there is no session bus.
    let bus = Connection::session().context("the services' bus")?;
    let policy = Arc::new(Mutex::new(
        PolicyClient::connect(identity).context("identity daemon")?,
    ));
    let access = {
        let policy = policy.clone();
        Access::start(portal.clone(), move |uid| {
            let app = policy.lock().unwrap().lookup(uid).ok()?;
            (*app != AppPolicy::unknown()).then(|| app.name.clone())
        })?
    };

    for stream in listener.incoming() {
        let sock = OwnedFd::from(stream?);
        let policy = policy.clone();
        let portal = portal.clone();
        let access = access.clone();
        let launcher = launcher.clone();
        let bus = bus.clone();
        thread::spawn(move || {
            let uid = match rustix::net::sockopt::socket_peercred(&sock) {
                Ok(cred) => cred.uid.as_raw(),
                Err(err) => {
                    eprintln!("bridge: no peer credentials: {err}");
                    return;
                }
            };
            let app = match policy.lock().unwrap().lookup(uid) {
                Ok(app) if *app != AppPolicy::unknown() => app,
                Ok(_) => {
                    eprintln!("bridge: refusing uid {uid}: not an app");
                    return;
                }
                Err(err) => {
                    eprintln!("bridge: identity lookup for uid {uid}: {err}");
                    return;
                }
            };
            eprintln!("bridge: {} (uid {uid}) connected", app.name);
            let link = Arc::new(AppLink {
                app,
                uid,
                portal,
                access,
                launcher,
                bus,
                sock,
                sending: Mutex::new(()),
                state: Mutex::new(LinkState::default()),
            });
            if let Err(err) = link.run() {
                eprintln!("bridge: {}: {err:#}", link.app.name);
            }
            link.gone();
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- PipeWire remotes

/// A PipeWire connection an app may have: it sees the core, these nodes and the factory
/// for its own stream node, nothing else. The permissions live in the daemon, so they hold
/// whatever the app does with the fd. With it, the daemon's id for the connection, to end
/// it later.
fn pipewire_remote(nodes: &[u32]) -> anyhow::Result<(OwnedFd, u32)> {
    use pipewire::context::ContextRc;
    use pipewire::core::PW_ID_CORE;
    use pipewire::loop_::Timeout;
    use pipewire::main_loop::MainLoopRc;
    use pipewire::permissions::{Permission, PermissionFlags};
    use pipewire::types::ObjectType;

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
                    eprintln!("bridge: PipeWire error on {id} ({seq}): {res} {message}");
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
    // What this connection is shown: after the permissions, what the app will see.
    let seen = Rc::new(RefCell::new(std::collections::BTreeSet::new()));
    let _globals = {
        let factory = factory.clone();
        let (seen_add, seen_remove) = (seen.clone(), seen.clone());
        registry
            .add_listener_local()
            .global(move |g| {
                seen_add.borrow_mut().insert(g.id);
                let client_node = g.props.is_some_and(|p| {
                    p.get("factory.type.name") == Some("PipeWire:Interface:ClientNode")
                });
                if g.type_ == ObjectType::Factory && client_node {
                    factory.set(g.id);
                }
            })
            .global_remove(move |id| {
                seen_remove.borrow_mut().remove(&id);
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
    perms.extend(nodes.iter().map(|node| Permission::new(*node, rwx)));
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
    if *TRACE {
        eprintln!("bridge: remote client {client_id} for nodes {nodes:?} sees {:?}", seen.borrow());
    }
    drop(_globals);
    drop(registry);
    // SAFETY: after steal_fd the core no longer owns the fd; nothing else here uses it.
    let fd = unsafe { pipewire::sys::pw_core_steal_fd(core.as_raw_ptr()) };
    anyhow::ensure!(fd >= 0, "PipeWire kept its fd");
    Ok((unsafe { OwnedFd::from_raw_fd(fd) }, client_id))
}

/// Seen once in many tries: a round trip that never came back. A fresh connection is
/// cheap, and the app would otherwise drop the whole share.
fn remote_twice(nodes: &[u32], who: &str) -> anyhow::Result<(OwnedFd, u32)> {
    pipewire_remote(nodes).or_else(|err| {
        eprintln!("bridge: {who}: {err:#}; once more");
        pipewire_remote(nodes)
    })
}

// ---------------------------------------------------------------- drv-portal

/// Our line to drv-portal. Requests carry an id; answers come back on one reader thread
/// and find their asker here. Losing the portal ends us: the set restarts as one.
pub struct Portal {
    sock: OwnedFd,
    next: AtomicU64,
    /// One answer each: the choosers.
    waiting: Mutex<HashMap<u64, Box<dyn FnOnce(protocol::Response) + Send>>>,
    /// Casts and grants hear more than once: `Cast`/`Granted`, then `Closed` at the end.
    casts: Mutex<HashMap<u64, Arc<dyn Fn(protocol::Response) + Send + Sync>>>,
}

impl Portal {
    fn start(sock: OwnedFd) -> anyhow::Result<Arc<Self>> {
        seq::send(&sock, &protocol::Request::Hello { version: protocol::VERSION }, &[])
            .context("hello to drv-portal")?;
        let (hello, _) = seq::recv::<protocol::Response>(&sock).context("hello from drv-portal")?;
        match hello {
            protocol::Response::Hello { version } if version == protocol::VERSION => {}
            other => bail!("drv-portal answered {other:?}, not version {}", protocol::VERSION),
        }
        let portal = Arc::new(Self {
            sock,
            next: AtomicU64::new(1),
            waiting: Mutex::new(HashMap::new()),
            casts: Mutex::new(HashMap::new()),
        });
        let reader = portal.clone();
        thread::spawn(move || loop {
            match seq::recv::<protocol::Response>(&reader.sock) {
                Ok((resp, _)) => reader.dispatch(resp),
                Err(err) => {
                    eprintln!("bridge: drv-portal: {err}");
                    process::exit(1);
                }
            }
        });
        Ok(portal)
    }

    fn dispatch(&self, resp: protocol::Response) {
        let id = match &resp {
            protocol::Response::Chosen { id, .. }
            | protocol::Response::Cast { id, .. }
            | protocol::Response::Granted { id }
            | protocol::Response::Closed { id }
            | protocol::Response::Cancelled { id }
            | protocol::Response::Failed { id, .. } => *id,
            protocol::Response::Hello { .. } => return,
        };
        let waiter = self.waiting.lock().unwrap().remove(&id);
        if let Some(done) = waiter {
            return done(resp);
        }
        let last = !matches!(resp, protocol::Response::Cast { .. } | protocol::Response::Granted { .. });
        let cast = {
            let mut casts = self.casts.lock().unwrap();
            if last { casts.remove(&id) } else { casts.get(&id).cloned() }
        };
        match cast {
            Some(on) => on(resp),
            None => eprintln!("bridge: drv-portal answered {id}, which nobody asked"),
        }
    }

    fn send(&self, id: u64, req: protocol::Request, once: bool) -> anyhow::Result<u64> {
        if let Err(err) = seq::send(&self.sock, &req, &[]) {
            if once {
                self.waiting.lock().unwrap().remove(&id);
            } else {
                self.casts.lock().unwrap().remove(&id);
            }
            return Err(err).context("asking drv-portal");
        }
        Ok(id)
    }

    fn ask(
        &self,
        app: &str,
        uid: u32,
        title: String,
        kind: protocol::Kind,
        done: impl FnOnce(protocol::Response) + Send + 'static,
    ) -> anyhow::Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.waiting.lock().unwrap().insert(id, Box::new(done));
        self.send(id, protocol::Request::Choose { id, app: app.to_owned(), uid, title, kind }, true)
    }

    /// Asks for a screen or a window for `app`. `on` hears `Cast` once the node streams,
    /// then `Closed` when it ends; or `Cancelled`/`Failed` instead.
    #[allow(clippy::too_many_arguments)]
    fn cast(
        &self,
        app: &str,
        uid: u32,
        cursor: protocol::Cursor,
        screens: bool,
        windows: bool,
        again: Option<String>,
        on: impl Fn(protocol::Response) + Send + Sync + 'static,
    ) -> anyhow::Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.casts.lock().unwrap().insert(id, Arc::new(on));
        let req = protocol::Request::Cast { id, app: app.to_owned(), uid, cursor, screens, windows, again };
        self.send(id, req, false)
    }

    /// Asks whether `app` may use `device`. `on` hears `Granted` then, one day, `Closed`;
    /// or `Cancelled`/`Failed` instead.
    fn grant(
        &self,
        app: &str,
        uid: u32,
        device: Device,
        on: impl Fn(protocol::Response) + Send + Sync + 'static,
    ) -> anyhow::Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.casts.lock().unwrap().insert(id, Arc::new(on));
        self.send(id, protocol::Request::Grant { id, app: app.to_owned(), uid, device }, false)
    }

    /// The app's connection ended: its screen consents end with it.
    fn forget(&self, app: &str, uid: u32) {
        let req = protocol::Request::Forget { app: app.to_owned(), uid };
        if let Err(err) = seq::send(&self.sock, &req, &[]) {
            eprintln!("bridge: forgetting {app} at drv-portal: {err}");
        }
    }

    /// Withdraws a chooser, or a cast: a live one stops.
    fn cancel(&self, id: u64) {
        self.waiting.lock().unwrap().remove(&id);
        self.casts.lock().unwrap().remove(&id);
        if let Err(err) = seq::send(&self.sock, &protocol::Request::Cancel { id }, &[]) {
            eprintln!("bridge: cancelling {id} at drv-portal: {err}");
        }
    }
}

impl access::Prompter for Portal {
    fn grant(&self, app: &str, uid: u32, device: Device, on: Box<dyn Fn(access::Answer) + Send + Sync>) -> anyhow::Result<u64> {
        let app_name = app.to_owned();
        Portal::grant(self, app, uid, device, move |resp| match resp {
            protocol::Response::Granted { .. } => on(access::Answer::Granted),
            protocol::Response::Closed { .. } => on(access::Answer::Revoked),
            protocol::Response::Cancelled { .. } => on(access::Answer::Refused),
            protocol::Response::Failed { reason, .. } => {
                eprintln!("bridge: {app_name}: device: {reason}");
                on(access::Answer::Refused)
            }
            _ => {}
        })
    }

    fn cancel(&self, id: u64) {
        Portal::cancel(self, id)
    }
}

// ---------------------------------------------------------------- one app

/// A screencast of one app, by the shim's session id.
struct Cast {
    /// Set once drv-portal was asked.
    portal_id: Option<u64>,
    /// The node, once the person consented and it streams.
    node: Option<u32>,
    /// The remotes handed out for it: PipeWire client ids, disconnected when it ends, so
    /// that their marks do not outlive the node.
    remotes: Vec<u32>,
}

#[derive(Default)]
struct LinkState {
    /// Requests waiting on the person, by the shim's `req`, with the id at drv-portal:
    /// `Cancel` withdraws them there.
    pending: HashMap<u64, u64>,
    casts: HashMap<u64, Cast>,
}

/// One connected app.
struct AppLink {
    app: Arc<AppPolicy>,
    uid: u32,
    portal: Arc<Portal>,
    access: Arc<Access>,
    /// drv-appd's launch channel, shared by every app: `Open` only.
    launcher: Arc<Mutex<PolicyClient>>,
    /// The services' bus, for the notification daemon.
    bus: Connection,
    sock: OwnedFd,
    /// Answers go out from several threads; one at a time.
    sending: Mutex<()>,
    state: Mutex<LinkState>,
}

impl AppLink {
    fn send(&self, msg: &ToShim, fds: &[BorrowedFd<'_>]) {
        let _one = self.sending.lock().unwrap();
        if let Err(err) = seq::send(&self.sock, msg, fds) {
            eprintln!("bridge: {}: to its shim: {err}", self.app.name);
        }
    }

    fn fail(&self, req: u64, err: impl std::fmt::Display) {
        let reason = format!("{err:#}");
        eprintln!("bridge: {}: request {req}: {reason}", self.app.name);
        self.send(&ToShim::Failed { req, reason }, &[]);
    }

    fn run(self: &Arc<Self>) -> anyhow::Result<()> {
        let (hello, _) = seq::recv::<ToServer>(&self.sock).context("hello from the shim")?;
        match hello {
            ToServer::Hello { version } if version == wire::VERSION => {}
            other => bail!("the shim opened with {other:?}, not version {}", wire::VERSION),
        }
        self.send(&ToShim::Hello { version: wire::VERSION }, &[]);
        loop {
            let (msg, fds) = match seq::recv::<ToServer>(&self.sock) {
                Ok(got) => got,
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err).context("from the shim"),
            };
            drop(fds);
            if *TRACE {
                eprintln!("bridge: {}: {msg:?}", self.app.name);
            }
            self.on(msg);
        }
    }

    fn on(self: &Arc<Self>, msg: ToServer) {
        match msg {
            ToServer::Hello { .. } => {}
            ToServer::Choose { req, title, kind } => self.choose(req, title, kind),
            ToServer::Cast { req, session, cursor, screens, windows, again } => {
                self.cast(req, session, cursor, screens, windows, again)
            }
            ToServer::CastRemote { req, session } => {
                let node = self.state.lock().unwrap().casts.get(&session).and_then(|c| c.node);
                let Some(node) = node else {
                    return self.fail(req, "the session is not streaming");
                };
                match remote_twice(&[node], &self.app.name).and_then(|(fd, client)| {
                    self.access.mark(client, &format!("node:{node}"))?;
                    Ok((fd, client))
                }) {
                    Ok((fd, client)) => {
                        if let Some(c) = self.state.lock().unwrap().casts.get_mut(&session) {
                            c.remotes.push(client);
                        }
                        self.send(&ToShim::Remote { req }, &[fd.as_fd()]);
                    }
                    Err(err) => self.fail(req, err),
                }
            }
            ToServer::CastClose { session } => {
                let cast = self.state.lock().unwrap().casts.remove(&session);
                if let Some(cast) = cast {
                    self.end_cast(cast);
                }
            }
            ToServer::Camera { req } => {
                if self.access.has(self.uid, Device::Camera) {
                    return self.send(&ToShim::Granted { req }, &[]);
                }
                let link = self.clone();
                self.access.camera(&self.app.name, self.uid, move |allowed| {
                    link.send(&if allowed { ToShim::Granted { req } } else { ToShim::Cancelled { req } }, &[]);
                });
            }
            ToServer::CameraRemote { req } => {
                if !self.access.has(self.uid, Device::Camera) {
                    return self.fail(req, "the camera was not allowed");
                }
                let cameras = self.access.cameras();
                match remote_twice(&cameras, &self.app.name).and_then(|(fd, client)| {
                    self.access.mark(client, "camera")?;
                    Ok((fd, client))
                }) {
                    Ok((fd, client)) => {
                        self.access.remote(self.uid, client);
                        self.send(&ToShim::Remote { req }, &[fd.as_fd()]);
                    }
                    Err(err) => self.fail(req, err),
                }
            }
            ToServer::CameraPresent { req } => {
                let present = !self.access.cameras().is_empty();
                self.send(&ToShim::Present { req, present }, &[]);
            }
            ToServer::Open { req, uri } => {
                // drv-appd checks the same and the manifest; this keeps garbage out of its log.
                if rpc::uri_scheme(&uri).is_none() {
                    return self.fail(req, format!("not a URI ({} bytes)", uri.len()));
                }
                match self.launcher.lock().unwrap().open(uri.clone()) {
                    Ok(uid) => {
                        eprintln!("bridge: {}: {uri:?} opens as uid {uid}", self.app.name);
                        self.send(&ToShim::Done { req }, &[]);
                    }
                    Err(err) => self.fail(req, format!("OpenURI {uri:?}: {err}")),
                }
            }
            ToServer::Notify { req, replaces, summary, body } => {
                // The name comes from the manifest, never from the app. The body is where
                // daemons render markup, so the app's body goes in as plain text.
                let sent = NotificationsProxyBlocking::new(&self.bus).and_then(|proxy| {
                    proxy.notify(
                        &self.app.name,
                        replaces,
                        self.app.icon.as_deref().unwrap_or(""),
                        &format!("{}: {}", self.app.name, clip(&summary)),
                        &plain(clip(&body)),
                        &[],
                        HashMap::new(),
                        -1,
                    )
                });
                match sent {
                    Ok(id) => self.send(&ToShim::Notified { req, id }, &[]),
                    Err(err) => self.fail(req, format!("notification: {err}")),
                }
            }
            ToServer::Cancel { req } => {
                let id = self.state.lock().unwrap().pending.remove(&req);
                if let Some(id) = id {
                    self.portal.cancel(id);
                }
            }
        }
    }

    fn choose(self: &Arc<Self>, req: u64, title: String, kind: protocol::Kind) {
        let link = self.clone();
        let asked = self.portal.ask(&self.app.name, self.uid, clip(&title).to_owned(), kind, move |resp| {
            link.state.lock().unwrap().pending.remove(&req);
            let answer = match resp {
                protocol::Response::Chosen { paths, .. } => ToShim::Files { req, paths },
                protocol::Response::Cancelled { .. } => ToShim::Cancelled { req },
                protocol::Response::Failed { reason, .. } => ToShim::Failed { req, reason },
                _ => return,
            };
            link.send(&answer, &[]);
        });
        match asked {
            Ok(id) => {
                self.state.lock().unwrap().pending.insert(req, id);
            }
            Err(err) => self.fail(req, err),
        }
    }

    fn cast(
        self: &Arc<Self>,
        req: u64,
        session: u64,
        cursor: protocol::Cursor,
        screens: bool,
        windows: bool,
        again: Option<String>,
    ) {
        {
            let state = self.state.lock().unwrap();
            if state.casts.contains_key(&session) {
                return self.fail(req, "the session was started already");
            }
        }
        let link = self.clone();
        let asked = self.portal.cast(&self.app.name, self.uid, cursor, screens, windows, again, move |resp| {
            match resp {
                protocol::Response::Cast { node_id, source, width, height, token, .. } => {
                    let mut state = link.state.lock().unwrap();
                    state.pending.remove(&req);
                    match state.casts.get_mut(&session) {
                        Some(c) => c.node = Some(node_id),
                        None => return, // closed meanwhile; drv-portal has the Cancel
                    }
                    drop(state);
                    link.send(&ToShim::Cast { req, node_id, source, width, height, token }, &[]);
                }
                protocol::Response::Cancelled { .. } => {
                    link.state.lock().unwrap().pending.remove(&req);
                    link.send(&ToShim::Cancelled { req }, &[]);
                }
                protocol::Response::Failed { reason, .. } => {
                    link.state.lock().unwrap().pending.remove(&req);
                    link.send(&ToShim::Failed { req, reason }, &[]);
                }
                protocol::Response::Closed { .. } => {
                    let cast = link.state.lock().unwrap().casts.remove(&session);
                    if let Some(cast) = cast {
                        link.end_cast(cast);
                        link.send(&ToShim::CastClosed { session }, &[]);
                    }
                }
                protocol::Response::Hello { .. }
                | protocol::Response::Chosen { .. }
                | protocol::Response::Granted { .. } => {}
            }
        });
        match asked {
            Ok(id) => {
                let mut state = self.state.lock().unwrap();
                state.pending.insert(req, id);
                state.casts.insert(session, Cast { portal_id: Some(id), node: None, remotes: Vec::new() });
            }
            Err(err) => self.fail(req, err),
        }
    }

    /// A cast is over on our side: drv-portal stops it, the remotes it handed out go.
    fn end_cast(&self, cast: Cast) {
        if let Some(id) = cast.portal_id {
            self.portal.cancel(id);
        }
        for client in cast.remotes {
            self.access.drop_client(client);
        }
    }

    /// The app disconnected: what drv-portal holds for it goes, dialogs down, casts stopped.
    fn gone(&self) {
        eprintln!("bridge: {} disconnected", self.app.name);
        let state = std::mem::take(&mut *self.state.lock().unwrap());
        for id in state.pending.into_values() {
            self.portal.cancel(id);
        }
        for cast in state.casts.into_values() {
            self.end_cast(cast);
        }
        self.portal.forget(&self.app.name, self.uid);
        self.access.forget(self.uid);
    }
}

/// Markup-safe for notification daemons that render Pango markup.
fn plain(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
