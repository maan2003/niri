use std::collections::HashMap;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use clap::{Parser, Subcommand};

mod access;
use access::Access;
use drv_bridge::{
    sender_component, unique_from_component, NOTIFICATIONS_NAME, NOTIFICATIONS_PATH, PORTAL_NAME,
    PORTAL_PATH,
};
use drv_os::fds::Kind;
use drv_policy::{rpc, seq};
use drv_policy::{AppPolicy, PolicyClient};
use drv_portal::protocol::{self, Device, Kind as ChooserKind};
use zbus::blocking::Connection;
use zbus::message::{Builder, Header, Message, Type as MessageType};
use zbus::names::BusName;
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue, Signature, Structure, Value};
use zbus::AuthMechanism;

#[derive(Parser)]
#[command(about = "Desktop services for sandboxed apps, keyed on the peer UID")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run as its own user under the supervisor: fd `listener` is the apps' socket, fd
    /// `portal` the line to drv-portal, fd `appd` a launch channel that only opens URIs.
    /// Each connection is keyed on the peer UID and forwarded to the services' bus.
    Serve {
        #[arg(long, env = "DRV_APPD_SOCKET")]
        appd: PathBuf,
    },
    /// Run in the app's UID on its private bus: claim the desktop names, forward to the server,
    /// then run the app.
    App {
        #[arg(long, env = drv_bridge::SOCKET_ENV)]
        socket: PathBuf,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve { appd } => serve(appd),
        Cmd::App { socket, command } => app(socket, command),
    }
}

// ---------------------------------------------------------------- message plumbing

type RawBody = (Vec<u8>, Signature, Vec<zbus::zvariant::OwnedFd>);

fn dup_fds(fds: &[zbus::zvariant::Fd<'_>]) -> anyhow::Result<Vec<zbus::zvariant::OwnedFd>> {
    fds.iter()
        .map(|fd| {
            let fd: OwnedFd = fd.as_fd().try_clone_to_owned().context("dup")?;
            Ok(fd.into())
        })
        .collect()
}

/// The body of `msg`, byte for byte.
fn raw_body(msg: &Message) -> anyhow::Result<RawBody> {
    let body = msg.body();
    let data = body.data();
    Ok((
        data.bytes().to_vec(),
        body.signature().clone(),
        dup_fds(data.fds())?,
    ))
}

fn with_body(builder: Builder<'_>, (bytes, signature, fds): RawBody) -> zbus::Result<Message> {
    // Safety: the bytes were a body of the same signature in another message.
    unsafe { builder.build_raw_body(&bytes, signature, fds) }
}

/// A reply to `call` with `body`, mirroring a return or error `reply` seen elsewhere.
fn mirrored_reply(call: &Header<'_>, reply: &Header<'_>, body: RawBody) -> zbus::Result<Message> {
    let builder = match reply.message_type() {
        MessageType::Error => Message::error(
            call,
            reply
                .error_name()
                .cloned()
                .unwrap_or_else(|| "org.freedesktop.DBus.Error.Failed".try_into().unwrap()),
        )?,
        _ => Message::method_return(call)?,
    };
    with_body(builder, body)
}

fn failed(call: &Header<'_>, text: String) -> zbus::Result<Message> {
    Message::error(call, "org.freedesktop.DBus.Error.Failed")?.build(&text)
}

const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// What we claim `org.freedesktop.portal.FileChooser` is (`current_name`, `current_folder`).
const FILE_CHOOSER_VERSION: u32 = 4;
/// What we claim `org.freedesktop.portal.ScreenCast` is. Restore tokens last the app's run.
const SCREEN_CAST_VERSION: u32 = 4;
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const SETTINGS_VERSION: u32 = 2;
const CAMERA: &str = "org.freedesktop.portal.Camera";
const CAMERA_VERSION: u32 = 1;
/// `OpenURI` only: version 1 has neither `OpenFile` nor `OpenDirectory`.
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const OPEN_URI_VERSION: u32 = 1;

/// Every call an app makes, logged: for finding out what a portal client does.
static TRACE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("DRV_BRIDGE_TRACE").is_some());
/// `AvailableSourceTypes`: monitors and windows.
const SOURCE_TYPES: u32 = 1 | 2;
/// `AvailableCursorModes`: hidden, embedded, metadata.
const CURSOR_MODES: u32 = 1 | 2 | 4;

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
    use std::cell::{Cell, RefCell};

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

/// `file://` with everything outside the unreserved set escaped.
fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The app-side caller of a call the shim relayed (it puts the unique name in the
/// destination field), as a path component.
fn caller_of(hdr: &Header<'_>) -> anyhow::Result<String> {
    match hdr.destination() {
        Some(BusName::Unique(name)) => Ok(sender_component(name.as_str())),
        _ => bail!("no caller"),
    }
}

// ---------------------------------------------------------------- the human's side

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

fn serve(identity: PathBuf) -> anyhow::Result<()> {
    let mut fds = drv_os::fds::take().context("fds from the supervisor")?;
    // Bound by the supervisor, any UID may connect; who they are is decided per connection.
    let listener = fds.listener("listener")?;
    let portal = Portal::start(fds.socket("portal", Kind::SeqPacket)?)?;
    // drv-appd starts a URI's handler for us; the app names nothing but the URI.
    let launcher = Arc::new(Mutex::new(
        PolicyClient::from_stream(UnixStream::from(fds.socket("appd", Kind::Stream)?)).context("launch channel")?,
    ));
    // For the remotes, and the microphone and camera consents.
    pipewire::init();
    // Fail at startup, not on the first app, if there is no session bus.
    drop(Connection::session().context("the services' bus")?);
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
        let stream = stream?;
        let policy = policy.clone();
        let portal = portal.clone();
        let access = access.clone();
        let launcher = launcher.clone();
        std::thread::spawn(move || {
            let uid = match rustix::net::sockopt::socket_peercred(&stream) {
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
            if let Err(err) = AppLink::run(app, uid, stream, portal, access, launcher) {
                eprintln!("bridge: {err:#}");
            }
        });
    }
    Ok(())
}

/// Our line to drv-portal. Requests carry an id; answers come back on one reader thread
/// and find their asker here. Losing the portal ends us: the set restarts as one.
struct Portal {
    sock: OwnedFd,
    next: AtomicU64,
    /// One answer each: the choosers.
    waiting: Mutex<HashMap<u64, Box<dyn FnOnce(protocol::Response) + Send>>>,
    /// Casts hear more than once: `Cast` when streaming, `Closed` at the end.
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

    fn ask(
        &self,
        app: &str,
        uid: u32,
        title: String,
        kind: ChooserKind,
        done: impl FnOnce(protocol::Response) + Send + 'static,
    ) -> anyhow::Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.waiting.lock().unwrap().insert(id, Box::new(done));
        let req = protocol::Request::Choose {
            id,
            app: app.to_owned(),
            uid,
            title,
            kind,
        };
        if let Err(err) = seq::send(&self.sock, &req, &[]) {
            self.waiting.lock().unwrap().remove(&id);
            return Err(err).context("asking drv-portal");
        }
        Ok(id)
    }

    /// Asks for a screen or a window for `app`. `on` hears `Cast` once the node streams,
    /// then `Closed` when it ends; or `Cancelled`/`Failed` instead.
    fn cast(
        &self,
        app: &str,
        uid: u32,
        session: &CastSession,
        on: impl Fn(protocol::Response) + Send + Sync + 'static,
    ) -> anyhow::Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.casts.lock().unwrap().insert(id, Arc::new(on));
        let req = protocol::Request::Cast {
            id,
            app: app.to_owned(),
            uid,
            cursor: session.cursor,
            screens: session.screens,
            windows: session.windows,
            again: session.again.clone(),
        };
        if let Err(err) = seq::send(&self.sock, &req, &[]) {
            self.casts.lock().unwrap().remove(&id);
            return Err(err).context("asking drv-portal");
        }
        Ok(id)
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
        let req = protocol::Request::Grant { id, app: app.to_owned(), uid, device };
        if let Err(err) = seq::send(&self.sock, &req, &[]) {
            self.casts.lock().unwrap().remove(&id);
            return Err(err).context("asking drv-portal");
        }
        Ok(id)
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

/// A screencast session of one app: from `CreateSession` to `Close`, or the cast's end.
struct CastSession {
    cursor: protocol::Cursor,
    /// `SelectSources.types`: whole screens (bit 1), windows (bit 2).
    screens: bool,
    windows: bool,
    /// The app asked for a restore token (any `persist_mode`); it gets one that lasts while
    /// it runs.
    persist: bool,
    /// The token the app gave back, for the same screen with no dialog.
    again: Option<String>,
    /// Set by `Start`.
    portal_id: Option<u64>,
    /// The node, once the person consented and it streams.
    node: Option<u32>,
    /// The remotes handed out for it: PipeWire client ids, disconnected when it ends, so
    /// that their marks do not outlive the node.
    remotes: Vec<u32>,
}

/// What `ours` did with a call.
enum Ours {
    Reply(Message),
    /// Answered already (the reply had to go before a signal).
    Done,
    No,
}

/// One connected app: its peer-to-peer link and its own connection on the human's bus.
struct AppLink {
    app: Arc<AppPolicy>,
    uid: u32,
    portal: Arc<Portal>,
    access: Arc<Access>,
    /// drv-appd's launch channel, shared by every app: `Open` only.
    launcher: Arc<Mutex<PolicyClient>>,
    p2p: Connection,
    /// The human's session bus, for the notification daemon.
    bus: Connection,
    state: Mutex<LinkState>,
}

#[derive(Default)]
struct LinkState {
    /// Request handles we answer ourselves (the file chooser, `Start`), by path, with the
    /// id at drv-portal: `Request.Close` on one cancels there.
    ours: HashMap<String, u64>,
    /// Screencast sessions by their path.
    sessions: HashMap<String, CastSession>,
}

impl AppLink {
    fn run(
        app: Arc<AppPolicy>,
        uid: u32,
        stream: UnixStream,
        portal: Arc<Portal>,
        access: Arc<Access>,
        launcher: Arc<Mutex<PolicyClient>>,
    ) -> anyhow::Result<()> {
        // Who the peer is was settled by SO_PEERCRED, so no D-Bus authentication on top.
        #[allow(deprecated)] // the async-io variant needs an Async wrapper for nothing
        let p2p_iter = zbus::blocking::connection::Builder::unix_stream(stream)
            .server(zbus::Guid::generate())?
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .build_message_iterator()
            .context("peer-to-peer handshake")?;
        let p2p = Connection::from(zbus::Connection::from(p2p_iter.inner()));
        let bus = Connection::session().context("the human's session bus")?;

        let link = Arc::new(AppLink {
            app,
            uid,
            portal,
            access,
            launcher,
            p2p,
            bus,
            state: Mutex::new(LinkState::default()),
        });
        for msg in p2p_iter {
            let Ok(msg) = msg else { break };
            if let Err(err) = link.on_app_message(&msg) {
                eprintln!("bridge: {}: from app: {err:#}", link.app.name);
            }
        }
        eprintln!("bridge: {} disconnected", link.app.name);
        // What drv-portal holds for this connection: dialogs down, casts stopped.
        let (ids, remotes): (Vec<u64>, Vec<u32>) = {
            let state = link.state.lock().unwrap();
            let sessions = state.sessions.values().filter_map(|s| s.portal_id);
            (
                state.ours.values().copied().chain(sessions).collect(),
                state.sessions.values().flat_map(|s| s.remotes.iter().copied()).collect(),
            )
        };
        for id in ids {
            link.portal.cancel(id);
        }
        for client in remotes {
            link.access.drop_client(client);
        }
        link.portal.forget(&link.app.name, link.uid);
        link.access.forget(link.uid);
        Ok(())
    }

    fn on_app_message(self: &Arc<Self>, msg: &Message) -> anyhow::Result<()> {
        let hdr = msg.header();
        if hdr.message_type() != MessageType::MethodCall {
            return Ok(());
        }
        let what = || {
            format!(
                "{}.{}",
                hdr.interface().map(|i| i.as_str()).unwrap_or("?"),
                hdr.member().map(|m| m.as_str()).unwrap_or("?")
            )
        };
        if *TRACE {
            eprintln!("bridge: {}: {}", self.app.name, what());
        }
        let reply = match self.ours(msg, &hdr) {
            Ok(Ours::Reply(reply)) => reply,
            Ok(Ours::Done) => return Ok(()),
            Err(err) => {
                eprintln!("bridge: {}: {}: {err:#}", self.app.name, what());
                failed(&hdr, format!("{err:#}"))?
            }
            Ok(Ours::No) => self.other(msg, &hdr)?,
        };
        self.p2p.send(&reply)?;
        Ok(())
    }

    /// Notifications, the settings apps read at startup, or nothing: there is no other
    /// portal behind the bridge.
    fn other(&self, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<Message> {
        let interface = hdr.interface().map(|i| i.as_str()).unwrap_or_default();
        Ok(if interface == NOTIFICATIONS_NAME {
            self.notification(msg, hdr)?
        } else if interface == SETTINGS && hdr.path().is_some_and(|p| p.as_str() == PORTAL_PATH) {
            self.settings(msg, hdr)?
        } else {
            Message::error(hdr, "org.freedesktop.DBus.Error.UnknownMethod")?.build(&format!(
                "the bridge does not carry {}.{}",
                hdr.interface().map(|i| i.as_str()).unwrap_or("?"),
                hdr.member().map(|m| m.as_str()).unwrap_or("?")
            ))?
        })
    }

    /// `org.freedesktop.portal.Settings`: how the desktop looks, the same for every app.
    fn settings(&self, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<Message> {
        // 1 is "prefer dark", which the desktop is.
        let all: [(&str, &str, Value<'static>); 2] = [
            ("org.freedesktop.appearance", "color-scheme", Value::U32(1)),
            ("org.freedesktop.appearance", "contrast", Value::U32(0)),
        ];
        let member = hdr.member().context("no member")?.as_str();
        Ok(match member {
            // `Read` wraps the value in a second variant, a mistake `ReadOne` corrected.
            "Read" | "ReadOne" => {
                let (namespace, key): (String, String) = msg.body().deserialize()?;
                match all.into_iter().find(|(ns, k, _)| *ns == namespace && *k == key) {
                    Some((_, _, value)) if member == "Read" => {
                        Message::method_return(hdr)?.build(&Value::Value(Box::new(value)))?
                    }
                    Some((_, _, value)) => Message::method_return(hdr)?.build(&value)?,
                    None => Message::error(hdr, "org.freedesktop.portal.Error.NotFound")?
                        .build(&format!("no setting {namespace} {key}"))?,
                }
            }
            "ReadAll" => {
                let (patterns,): (Vec<String>,) = msg.body().deserialize()?;
                let wanted = |ns: &str| {
                    patterns.iter().any(|p| {
                        p.is_empty() || p == ns || p.strip_suffix('*').is_some_and(|prefix| ns.starts_with(prefix))
                    })
                };
                let mut out: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();
                for (ns, key, value) in all {
                    if wanted(ns) {
                        out.entry(ns).or_default().insert(key, value);
                    }
                }
                Message::method_return(hdr)?.build(&out)?
            }
            other => Message::error(hdr, "org.freedesktop.DBus.Error.UnknownMethod")?
                .build(&format!("no {other} on {SETTINGS}"))?,
        })
    }

    /// What we answer ourselves: the file chooser and the screencast (drv-portal's), their
    /// properties, their sessions, and `Close` on a request of ours.
    fn ours(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<Ours> {
        let path = hdr.path().context("no path")?.as_str().to_owned();
        let interface = hdr.interface().map(|i| i.as_str().to_owned()).unwrap_or_default();
        let member = hdr.member().map(|m| m.as_str().to_owned()).unwrap_or_default();
        if path == PORTAL_PATH && interface == FILE_CHOOSER {
            return self.file_chooser(msg, hdr, &member).map(Ours::Reply);
        }
        if path == PORTAL_PATH && interface == SCREEN_CAST {
            return self.screen_cast(msg, hdr, &member);
        }
        if path == PORTAL_PATH && interface == CAMERA {
            return self.camera(msg, hdr, &member);
        }
        if path == PORTAL_PATH && interface == OPEN_URI {
            return self.open_uri(msg, hdr, &member);
        }
        if path == PORTAL_PATH && interface == PROPERTIES {
            let props = |iface: &str| -> Option<Vec<(&'static str, Value<'static>)>> {
                match iface {
                    FILE_CHOOSER => Some(vec![("version", Value::U32(FILE_CHOOSER_VERSION))]),
                    SETTINGS => Some(vec![("version", Value::U32(SETTINGS_VERSION))]),
                    OPEN_URI => Some(vec![("version", Value::U32(OPEN_URI_VERSION))]),
                    CAMERA => Some(vec![
                        ("version", Value::U32(CAMERA_VERSION)),
                        ("IsCameraPresent", Value::Bool(!self.access.cameras().is_empty())),
                    ]),
                    SCREEN_CAST => Some(vec![
                        ("version", Value::U32(SCREEN_CAST_VERSION)),
                        ("AvailableSourceTypes", Value::U32(SOURCE_TYPES)),
                        ("AvailableCursorModes", Value::U32(CURSOR_MODES)),
                    ]),
                    _ => None,
                }
            };
            match member.as_str() {
                "Get" => {
                    let (iface, prop): (String, String) = msg.body().deserialize()?;
                    if let Some(props) = props(&iface) {
                        let found = props.into_iter().find(|(name, _)| *name == prop);
                        return Ok(Ours::Reply(match found {
                            Some((_, value)) => Message::method_return(hdr)?.build(&value)?,
                            None => Message::error(hdr, "org.freedesktop.DBus.Error.InvalidArgs")?
                                .build(&format!("no property {prop} on {iface}"))?,
                        }));
                    }
                }
                "GetAll" => {
                    let (iface,): (String,) = msg.body().deserialize()?;
                    if let Some(props) = props(&iface) {
                        let all: HashMap<&str, Value<'_>> = props.into_iter().collect();
                        return Ok(Ours::Reply(Message::method_return(hdr)?.build(&all)?));
                    }
                }
                _ => {}
            }
            return Ok(Ours::No);
        }
        if interface == SESSION_IFACE {
            let session = {
                let mut state = self.state.lock().unwrap();
                if !state.sessions.contains_key(&path) {
                    return Ok(Ours::No);
                }
                match member.as_str() {
                    "Close" => state.sessions.remove(&path),
                    other => bail!("no {other} on {SESSION_IFACE}"),
                }
            };
            if let Some(s) = session {
                if let Some(id) = s.portal_id {
                    self.portal.cancel(id);
                }
                self.end_cast(&s);
            }
            return Ok(Ours::Reply(Message::method_return(hdr)?.build(&())?));
        }
        if interface == REQUEST_IFACE && member == "Close" {
            let id = self.state.lock().unwrap().ours.remove(&path);
            if let Some(id) = id {
                self.portal.cancel(id);
                return Ok(Ours::Reply(Message::method_return(hdr)?.build(&())?));
            }
        }
        Ok(Ours::No)
    }

    /// The portal `Response` signal on a request handle, to the app-side caller.
    /// A cast session is over: the remotes it handed out go with it.
    fn end_cast(&self, session: &CastSession) {
        for client in &session.remotes {
            self.access.drop_client(*client);
        }
    }

    fn respond(&self, handle: &str, caller: &str, code: u32, results: HashMap<&str, Value<'_>>) -> anyhow::Result<()> {
        let signal = Message::signal(handle, REQUEST_IFACE, "Response")?
            .destination(unique_from_component(caller))?
            .build(&(code, results))?;
        self.p2p.send(&signal).context("response signal")
    }

    /// The app's request handle for a call: its token (one path element) under its own
    /// caller component, as the portal spec has it.
    fn handle(&self, msg: &Message, caller: &str, options: &HashMap<String, OwnedValue>, key: &str) -> String {
        let token: String = options
            .get(key)
            .cloned()
            .and_then(|v| String::try_from(v).ok())
            .unwrap_or_else(|| format!("drv{}", msg.primary_header().serial_num()))
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        let kind = if key == "session_handle_token" { "session" } else { "request" };
        format!("{PORTAL_PATH}/{kind}/{caller}/{token}")
    }

    /// `org.freedesktop.portal.ScreenCast`, monitors only: the session is ours, the person
    /// consents at drv-portal on `Start`, and `OpenPipeWireRemote` hands out a connection
    /// that sees the one node.
    fn screen_cast(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        let caller = caller_of(hdr)?;
        let reply_handle = |handle: &str| -> anyhow::Result<Message> {
            Ok(Message::method_return(hdr)?.build(&ObjectPath::try_from(handle)?)?)
        };
        match member {
            "CreateSession" => {
                let (options,): (HashMap<String, OwnedValue>,) = msg.body().deserialize()?;
                let handle = self.handle(msg, &caller, &options, "handle_token");
                let session = self.handle(msg, &caller, &options, "session_handle_token");
                self.state.lock().unwrap().sessions.insert(
                    session.clone(),
                    CastSession {
                        cursor: protocol::Cursor::Embedded,
                        screens: true,
                        windows: false,
                        persist: false,
                        again: None,
                        portal_id: None,
                        node: None,
                        remotes: Vec::new(),
                    },
                );
                // The reply first, then the signal: the spec's order.
                self.p2p.send(&reply_handle(&handle)?)?;
                let mut results: HashMap<&str, Value<'_>> = HashMap::new();
                results.insert("session_handle", Value::from(session.as_str()));
                self.respond(&handle, &caller, 0, results)?;
                Ok(Ours::Done)
            }
            "SelectSources" => {
                let (session, options): (OwnedObjectPath, HashMap<String, OwnedValue>) =
                    msg.body().deserialize()?;
                let cursor = match options.get("cursor_mode").cloned().and_then(|v| u32::try_from(v).ok()) {
                    Some(1) => protocol::Cursor::Hidden,
                    Some(4) => protocol::Cursor::Metadata,
                    _ => protocol::Cursor::Embedded,
                };
                let persist = options.get("persist_mode").cloned().and_then(|v| u32::try_from(v).ok()).unwrap_or(0) != 0;
                let again = options.get("restore_token").cloned().and_then(|v| String::try_from(v).ok());
                // Bit 1 monitors, bit 2 windows (4, virtual, we do not have); nothing asked
                // means monitors.
                let types = options.get("types").cloned().and_then(|v| u32::try_from(v).ok()).unwrap_or(1);
                let windows = types & 2 != 0;
                let screens = types & 1 != 0 || !windows;
                {
                    let mut state = self.state.lock().unwrap();
                    let s = state.sessions.get_mut(session.as_str()).context("no such session")?;
                    s.cursor = cursor;
                    s.screens = screens;
                    s.windows = windows;
                    s.persist = persist;
                    s.again = again;
                }
                let handle = self.handle(msg, &caller, &options, "handle_token");
                self.p2p.send(&reply_handle(&handle)?)?;
                self.respond(&handle, &caller, 0, HashMap::new())?;
                Ok(Ours::Done)
            }
            "Start" => {
                let (session, _parent, options): (OwnedObjectPath, String, HashMap<String, OwnedValue>) =
                    msg.body().deserialize()?;
                let asked = {
                    let state = self.state.lock().unwrap();
                    let s = state.sessions.get(session.as_str()).context("no such session")?;
                    anyhow::ensure!(s.portal_id.is_none(), "the session was started already");
                    CastSession { again: s.again.clone(), remotes: Vec::new(), ..*s }
                };
                let handle = self.handle(msg, &caller, &options, "handle_token");
                let link = self.clone();
                let session_path = session.as_str().to_owned();
                let (caller2, handle2) = (caller.clone(), handle.clone());
                let id = self.portal.cast(&self.app.name, self.uid, &asked, move |resp| {
                    let res = match resp {
                        protocol::Response::Cast { node_id, source, width, height, token, .. } => {
                            let mut state = link.state.lock().unwrap();
                            state.ours.remove(&handle2);
                            let persist = match state.sessions.get_mut(&session_path) {
                                Some(s) => {
                                    s.node = Some(node_id);
                                    s.persist
                                }
                                None => return, // closed meanwhile; drv-portal has the Cancel
                            };
                            drop(state);
                            let mut stream: HashMap<&str, Value<'_>> = HashMap::new();
                            let (source_type, source_id) = match source {
                                protocol::Source::Screen(name) => (1, name),
                                protocol::Source::Window(id) => (2, id.to_string()),
                            };
                            stream.insert("source_type", Value::U32(source_type));
                            stream.insert("id", Value::from(source_id));
                            stream.insert("position", Value::from((0i32, 0i32)));
                            stream.insert("size", Value::from((width, height)));
                            // `a(ua{sv})` as the spec has it; a Vec of Values would go
                            // out as `av`, which apps' parsers read as node 0.
                            let streams = Signature::try_from("(ua{sv})").map_err(|e| anyhow::anyhow!("{e:?}")).and_then(|sig| {
                                let mut streams = Array::new(&sig);
                                streams.append(Value::from(Structure::from((node_id, stream))))?;
                                Ok(streams)
                            });
                            let mut results: HashMap<&str, Value<'_>> = HashMap::new();
                            match streams {
                                Ok(streams) => results.insert("streams", Value::from(streams)),
                                Err(err) => {
                                    eprintln!("bridge: {}: screencast streams: {err:#}", link.app.name);
                                    return;
                                }
                            };
                            if persist {
                                // 1: for as long as the app runs, whatever it asked for.
                                results.insert("persist_mode", Value::U32(1));
                                results.insert("restore_token", Value::from(token));
                            }
                            link.respond(&handle2, &caller2, 0, results)
                        }
                        protocol::Response::Cancelled { .. } => {
                            link.state.lock().unwrap().ours.remove(&handle2);
                            link.respond(&handle2, &caller2, 1, HashMap::new())
                        }
                        protocol::Response::Failed { reason, .. } => {
                            eprintln!("bridge: {}: screencast: {reason}", link.app.name);
                            link.state.lock().unwrap().ours.remove(&handle2);
                            link.respond(&handle2, &caller2, 2, HashMap::new())
                        }
                        protocol::Response::Closed { .. } => {
                            let Some(s) = link.state.lock().unwrap().sessions.remove(&session_path) else {
                                return;
                            };
                            link.end_cast(&s);
                            Message::signal(session_path.as_str(), SESSION_IFACE, "Closed")
                                .and_then(|b| b.destination(unique_from_component(&caller2)))
                                .and_then(|b| b.build(&HashMap::<&str, Value<'_>>::new()))
                                .map_err(anyhow::Error::from)
                                .and_then(|s| link.p2p.send(&s).map_err(anyhow::Error::from))
                        }
                        protocol::Response::Hello { .. }
                        | protocol::Response::Chosen { .. }
                        | protocol::Response::Granted { .. } => return,
                    };
                    if let Err(err) = res {
                        eprintln!("bridge: {}: screencast: {err:#}", link.app.name);
                    }
                })?;
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(s) = state.sessions.get_mut(session.as_str()) {
                        s.portal_id = Some(id);
                    }
                    state.ours.insert(handle.clone(), id);
                }
                Ok(Ours::Reply(reply_handle(&handle)?))
            }
            "OpenPipeWireRemote" => {
                let (session, _options): (OwnedObjectPath, HashMap<String, OwnedValue>) =
                    msg.body().deserialize()?;
                let node = {
                    let state = self.state.lock().unwrap();
                    let s = state.sessions.get(session.as_str()).context("no such session")?;
                    s.node.context("the session is not streaming")?
                };
                // Seen once in many tries: a round trip that never came back. A fresh
                // connection is cheap, and the app would otherwise drop the whole share.
                let (fd, client) = pipewire_remote(&[node]).or_else(|err| {
                    eprintln!("bridge: {}: {err:#}; once more", self.app.name);
                    pipewire_remote(&[node])
                })?;
                self.access.mark(client, &format!("node:{node}"))?;
                if let Some(s) = self.state.lock().unwrap().sessions.get_mut(session.as_str()) {
                    s.remotes.push(client);
                }
                Ok(Ours::Reply(Message::method_return(hdr)?.build(&zbus::zvariant::Fd::from(fd))?))
            }
            other => bail!("no {other} on {SCREEN_CAST}"),
        }
    }

    /// `org.freedesktop.portal.Camera`: the person is asked once per run of the app, and
    /// the remote sees every camera, for as long as the consent stands.
    fn camera(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        match member {
            "AccessCamera" => {
                let caller = caller_of(hdr)?;
                let (options,): (HashMap<String, OwnedValue>,) = msg.body().deserialize()?;
                let handle = self.handle(msg, &caller, &options, "handle_token");
                let reply = Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?;
                if self.access.has(self.uid, Device::Camera) {
                    // The reply first, then the signal: the spec's order.
                    self.p2p.send(&reply)?;
                    self.respond(&handle, &caller, 0, HashMap::new())?;
                    return Ok(Ours::Done);
                }
                let link = self.clone();
                self.access.camera(&self.app.name, self.uid, move |allowed| {
                    let code = if allowed { 0 } else { 1 };
                    if let Err(err) = link.respond(&handle, &caller, code, HashMap::new()) {
                        eprintln!("bridge: {}: camera response: {err}", link.app.name);
                    }
                });
                Ok(Ours::Reply(reply))
            }
            "OpenPipeWireRemote" => {
                anyhow::ensure!(self.access.has(self.uid, Device::Camera), "the camera was not allowed");
                let cameras = self.access.cameras();
                let (fd, client) = pipewire_remote(&cameras).or_else(|err| {
                    eprintln!("bridge: {}: {err:#}; once more", self.app.name);
                    pipewire_remote(&cameras)
                })?;
                self.access.mark(client, "camera")?;
                self.access.remote(self.uid, client);
                Ok(Ours::Reply(Message::method_return(hdr)?.build(&zbus::zvariant::Fd::from(fd))?))
            }
            other => bail!("no {other} on {CAMERA}"),
        }
    }

    /// `OpenURI`: drv-appd starts the scheme's handler from the manifest with the URI as its
    /// last argument, no prompt. The URI is the only thing the app gets to say, and only a
    /// well-formed one gets through; `writable`, `ask` and the parent window are ignored.
    fn open_uri(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        if member != "OpenURI" {
            bail!("no {member} on {OPEN_URI}");
        }
        let caller = caller_of(hdr)?;
        let (_parent, uri, options): (String, String, HashMap<String, OwnedValue>) =
            msg.body().deserialize().context("OpenURI arguments")?;
        let handle = self.handle(msg, &caller, &options, "handle_token");
        let code = match rpc::uri_scheme(&uri) {
            None => {
                eprintln!("bridge: {}: OpenURI of something that is not a URI ({} bytes)", self.app.name, uri.len());
                2
            }
            Some(_) => match self.launcher.lock().unwrap().open(uri.clone()) {
                Ok(uid) => {
                    eprintln!("bridge: {}: {uri:?} opens as uid {uid}", self.app.name);
                    0
                }
                Err(err) => {
                    eprintln!("bridge: {}: OpenURI {uri:?}: {err}", self.app.name);
                    2
                }
            },
        };
        // The reply first, then the signal: the spec's order.
        self.p2p.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
        self.respond(&handle, &caller, code, HashMap::new())?;
        Ok(Ours::Done)
    }

    /// `OpenFile`/`SaveFile`: the handle goes back now, the `Response` signal when the
    /// person has picked. The app's `title` is shown as a hint under our own line naming it.
    fn file_chooser(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Message> {
        let caller = caller_of(hdr)?;
        let (_parent, title, options): (String, String, HashMap<String, OwnedValue>) =
            msg.body().deserialize().context("FileChooser arguments")?;
        let string = |key: &str| options.get(key).cloned().and_then(|v| String::try_from(v).ok());
        let flag = |key: &str| options.get(key).cloned().and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        let kind = match member {
            "OpenFile" if flag("directory") => bail!("directories are not handed out yet"),
            "OpenFile" => ChooserKind::Open,
            "SaveFile" => ChooserKind::Save {
                name: string("current_name").unwrap_or_default(),
            },
            other => bail!("no {other} on {FILE_CHOOSER}"),
        };
        let handle = self.handle(msg, &caller, &options, "handle_token");
        let link = self.clone();
        let signal_path = handle.clone();
        let id = self.portal.ask(&self.app.name, self.uid, title, kind, move |resp| {
            link.state.lock().unwrap().ours.remove(&signal_path);
            let (code, uris): (u32, Vec<String>) = match resp {
                protocol::Response::Chosen { paths, .. } => {
                    (0, paths.iter().map(|p| file_uri(p)).collect())
                }
                protocol::Response::Cancelled { .. } => (1, Vec::new()),
                protocol::Response::Failed { reason, .. } => {
                    eprintln!("bridge: {}: file chooser: {reason}", link.app.name);
                    (2, Vec::new())
                }
                _ => return,
            };
            let mut results: HashMap<&str, Value<'_>> = HashMap::new();
            if code == 0 {
                results.insert("uris", Value::from(uris));
            }
            if let Err(err) = link.respond(&signal_path, &caller, code, results) {
                eprintln!("bridge: {}: file chooser response: {err}", link.app.name);
            }
        })?;
        self.state.lock().unwrap().ours.insert(handle.clone(), id);
        Ok(Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)
    }

    fn notification(&self, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<Message> {
        let member = hdr.member().context("no member")?.as_str();
        Ok(match member {
            "GetCapabilities" => Message::method_return(hdr)?.build(&vec!["body"])?,
            "GetServerInformation" => Message::method_return(hdr)?.build(&(
                "drv-bridge",
                "drv",
                env!("CARGO_PKG_VERSION"),
                "1.2",
            ))?,
            "CloseNotification" => Message::method_return(hdr)?.build(&())?,
            "Notify" => {
                #[allow(clippy::type_complexity)]
                let (_app_name, replaces, _icon, summary, body, _actions, _hints, _timeout): (
                    String,
                    u32,
                    String,
                    String,
                    String,
                    Vec<String>,
                    HashMap<String, OwnedValue>,
                    i32,
                ) = msg.body().deserialize()?;
                let proxy = NotificationsProxyBlocking::new(&self.bus)?;
                // The name comes from the manifest, never from the app. The body is where
                // daemons render markup, so the app's body goes in as plain text.
                let id = proxy.notify(
                    &self.app.name,
                    replaces,
                    self.app.icon.as_deref().unwrap_or(""),
                    &format!("{}: {summary}", self.app.name),
                    &plain(&body),
                    &[],
                    HashMap::new(),
                    -1,
                )?;
                Message::method_return(hdr)?.build(&id)?
            }
            other => Message::error(hdr, "org.freedesktop.DBus.Error.UnknownMethod")?
                .build(&format!("no {other} on {NOTIFICATIONS_NAME}"))?,
        })
    }
}

/// Markup-safe for notification daemons that render Pango markup.
fn plain(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------- the app's side

struct Shim {
    bus: Connection,
    server: Connection,
    /// Calls forwarded to the server, by their serial there, with the app's call they answer.
    pending: Mutex<HashMap<NonZeroU32, Message>>,
}

fn app(socket: PathBuf, command: Vec<String>) -> anyhow::Result<()> {
    #[allow(deprecated)]
    let server_iter = zbus::blocking::connection::Builder::unix_stream(
        UnixStream::connect(&socket).with_context(|| socket.display().to_string())?,
    )
    .p2p()
    .auth_mechanism(AuthMechanism::Anonymous)
    .build_message_iterator()
    .context("bridge server")?;
    let server = Connection::from(zbus::Connection::from(server_iter.inner()));
    let bus_iter = zbus::blocking::connection::Builder::session()?
        .build_message_iterator()
        .context("the app's private bus")?;
    let bus = Connection::from(zbus::Connection::from(bus_iter.inner()));
    bus.request_name(NOTIFICATIONS_NAME)?;
    bus.request_name(PORTAL_NAME)?;

    let shim = Arc::new(Shim {
        bus,
        server,
        pending: Mutex::new(HashMap::new()),
    });
    {
        let shim = shim.clone();
        std::thread::spawn(move || {
            for msg in bus_iter {
                let Ok(msg) = msg else { break };
                if let Err(err) = shim.on_app_message(&msg) {
                    eprintln!("drv-bridge: from app: {err:#}");
                }
            }
        });
    }
    {
        let shim = shim.clone();
        std::thread::spawn(move || {
            for msg in server_iter {
                let Ok(msg) = msg else { break };
                if let Err(err) = shim.on_server_message(&msg) {
                    eprintln!("drv-bridge: from server: {err:#}");
                }
            }
            eprintln!("drv-bridge: lost the bridge server");
        });
    }

    let status = Command::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| command[0].clone())?;
    std::process::exit(status.code().unwrap_or(1));
}

impl Shim {
    fn on_app_message(&self, msg: &Message) -> anyhow::Result<()> {
        let hdr = msg.header();
        if hdr.message_type() != MessageType::MethodCall {
            return Ok(());
        }
        let on_our_paths = hdr.path().is_some_and(|p| {
            p.as_str() == NOTIFICATIONS_PATH || p.as_str().starts_with(PORTAL_PATH)
        });
        if !on_our_paths {
            let reply = Message::error(&hdr, "org.freedesktop.DBus.Error.UnknownObject")?
                .build(&"not here")?;
            self.bus.send(&reply)?;
            return Ok(());
        }
        // The caller's unique name rides in the destination field; the server needs it for
        // portal handles.
        let caller = hdr.sender().context("no sender")?.to_owned();
        let mut builder = Message::method_call(
            hdr.path().context("no path")?.clone(),
            hdr.member().context("no member")?.clone(),
        )?
        .destination(BusName::Unique(caller))?;
        if let Some(interface) = hdr.interface() {
            builder = builder.interface(interface.clone())?;
        }
        let forwarded = with_body(builder, raw_body(msg)?)?;
        self.pending
            .lock()
            .unwrap()
            .insert(forwarded.primary_header().serial_num(), msg.clone());
        if let Err(err) = self.server.send(&forwarded) {
            self.pending
                .lock()
                .unwrap()
                .remove(&forwarded.primary_header().serial_num());
            self.bus.send(&failed(&hdr, format!("bridge: {err}"))?)?;
        }
        Ok(())
    }

    fn on_server_message(&self, msg: &Message) -> anyhow::Result<()> {
        let hdr = msg.header();
        match hdr.message_type() {
            MessageType::MethodReturn | MessageType::Error => {
                let Some(serial) = hdr.reply_serial() else {
                    return Ok(());
                };
                let Some(call) = self.pending.lock().unwrap().remove(&serial) else {
                    return Ok(());
                };
                self.bus
                    .send(&mirrored_reply(&call.header(), &hdr, raw_body(msg)?)?)?;
            }
            MessageType::Signal => {
                let mut builder = Message::signal(
                    hdr.path().context("no path")?.clone(),
                    hdr.interface().context("no interface")?.clone(),
                    hdr.member().context("no member")?.clone(),
                )?;
                if let Some(destination) = hdr.destination() {
                    builder = builder.destination(destination.clone())?;
                }
                self.bus.send(&with_body(builder, raw_body(msg)?)?)?;
            }
            MessageType::MethodCall => {}
        }
        Ok(())
    }
}
