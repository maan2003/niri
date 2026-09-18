use std::collections::HashMap;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context as _};
use clap::{Parser, Subcommand};
use drv_bridge::{
    rewrite_handle, rewrite_structure, sender_component, tokens_owned_by, unique_from_component,
    APP_ID_PREFIX, NOTIFICATIONS_NAME, NOTIFICATIONS_PATH, PORTAL_NAME, PORTAL_PATH,
};
use drv_os::fds::Kind;
use drv_policy::seq;
use drv_policy::{AppPolicy, PolicyClient};
use drv_portal::protocol::{self, Kind as ChooserKind};
use zbus::blocking::Connection;
use zbus::message::{Builder, Header, Message, Type as MessageType};
use zbus::names::BusName;
use zbus::zvariant::serialized::Context;
use zbus::zvariant::{ObjectPath, OwnedValue, Signature, Structure, Value};
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
    /// `portal` the line to drv-portal. Each connection is keyed on the peer UID and
    /// forwarded to the services' bus.
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

/// The body of `msg` with portal handles owned by `from` renamed to `to`. Also reports the
/// tokens of handles owned by `from` (before renaming).
fn rewritten_body(
    msg: &Message,
    from: &str,
    to: &str,
    tokens: &mut Vec<String>,
) -> anyhow::Result<RawBody> {
    let body = msg.body();
    if body.is_empty() {
        return Ok((Vec::new(), body.signature().clone(), Vec::new()));
    }
    let structure: Structure = body.deserialize().context("body")?;
    tokens_owned_by(&Value::Structure(structure.try_clone()?), from, tokens);
    let structure = rewrite_structure(structure, from, to)?;
    let context = Context::new_dbus(body.data().context().endian(), 0);
    let data = zbus::zvariant::to_bytes(context, &structure)?;
    let fds = dup_fds(data.fds())?;
    Ok((data.bytes().to_vec(), body.signature().clone(), fds))
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
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// What we claim `org.freedesktop.portal.FileChooser` is (`current_name`, `current_folder`).
const FILE_CHOOSER_VERSION: u32 = 4;

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

fn is_portal_call(hdr: &Header<'_>) -> bool {
    let on_portal_path = hdr.path().is_some_and(|p| {
        p.as_str() == PORTAL_PATH || p.as_str().starts_with(&format!("{PORTAL_PATH}/"))
    });
    on_portal_path
        && hdr.interface().is_some_and(|i| {
            i.starts_with("org.freedesktop.portal.") || i.as_str() == PROPERTIES
                || i.as_str() == "org.freedesktop.DBus.Properties"
                || i.as_str() == "org.freedesktop.DBus.Introspectable"
        })
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
    // Fail at startup, not on the first app, if there is no session bus.
    drop(Connection::session().context("the services' bus")?);
    let policy = Arc::new(Mutex::new(
        PolicyClient::connect(identity).context("identity daemon")?,
    ));

    for stream in listener.incoming() {
        let stream = stream?;
        let policy = policy.clone();
        let portal = portal.clone();
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
            if let Err(err) = AppLink::run(app, uid, stream, portal) {
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
    waiting: Mutex<HashMap<u64, Box<dyn FnOnce(protocol::Response) + Send>>>,
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
            | protocol::Response::Cancelled { id }
            | protocol::Response::Failed { id, .. } => *id,
            protocol::Response::Hello { .. } => return,
        };
        let waiter = self.waiting.lock().unwrap().remove(&id);
        match waiter {
            Some(done) => done(resp),
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

    fn cancel(&self, id: u64) {
        self.waiting.lock().unwrap().remove(&id);
        if let Err(err) = seq::send(&self.sock, &protocol::Request::Cancel { id }, &[]) {
            eprintln!("bridge: cancelling {id} at drv-portal: {err}");
        }
    }
}

/// One connected app: its peer-to-peer link and its own connection on the human's bus.
struct AppLink {
    app: Arc<AppPolicy>,
    uid: u32,
    portal: Arc<Portal>,
    p2p: Connection,
    bus: Connection,
    /// The sender component of `bus`'s unique name.
    me: String,
    state: Mutex<LinkState>,
}

#[derive(Default)]
struct LinkState {
    /// Calls forwarded to the bus, by their serial there, with the app's call they answer.
    pending: HashMap<NonZeroU32, Message>,
    /// Portal handle token → the app-side sender component that owns it.
    owners: HashMap<String, String>,
    /// Request handles we answer ourselves (the file chooser), by path, with the id at
    /// drv-portal: `Request.Close` on one cancels there.
    ours: HashMap<String, u64>,
}

impl AppLink {
    fn run(app: Arc<AppPolicy>, uid: u32, stream: UnixStream, portal: Arc<Portal>) -> anyhow::Result<()> {
        // Who the peer is was settled by SO_PEERCRED, so no D-Bus authentication on top.
        #[allow(deprecated)] // the async-io variant needs an Async wrapper for nothing
        let p2p_iter = zbus::blocking::connection::Builder::unix_stream(stream)
            .server(zbus::Guid::generate())?
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .build_message_iterator()
            .context("peer-to-peer handshake")?;
        let p2p = Connection::from(zbus::Connection::from(p2p_iter.inner()));
        let bus_iter = zbus::blocking::connection::Builder::session()?
            .build_message_iterator()
            .context("the human's session bus")?;
        let bus = Connection::from(zbus::Connection::from(bus_iter.inner()));
        let me = sender_component(bus.unique_name().context("no unique name")?.as_str());

        // Tell the portal frontend who this connection speaks for; its dialogs and its
        // permission store then use the manifest name.
        let app_id = format!("{APP_ID_PREFIX}{}", app.name);
        if let Err(err) = bus.call_method(
            Some(PORTAL_NAME),
            PORTAL_PATH,
            Some("org.freedesktop.host.portal.Registry"),
            "Register",
            &(&app_id, HashMap::<&str, Value<'_>>::new()),
        ) {
            eprintln!("bridge: {}: portal registry: {err}", app.name);
        }

        let link = Arc::new(AppLink {
            app,
            uid,
            portal,
            p2p,
            bus,
            me,
            state: Mutex::new(LinkState::default()),
        });
        let from_bus = {
            let link = link.clone();
            std::thread::spawn(move || {
                for msg in bus_iter {
                    let Ok(msg) = msg else { break };
                    if let Err(err) = link.on_bus_message(&msg) {
                        eprintln!("bridge: {}: from bus: {err:#}", link.app.name);
                    }
                }
            })
        };
        for msg in p2p_iter {
            let Ok(msg) = msg else { break };
            if let Err(err) = link.on_app_message(&msg) {
                eprintln!("bridge: {}: from app: {err:#}", link.app.name);
            }
        }
        eprintln!("bridge: {} disconnected", link.app.name);
        // Dropping our bus connection ends the other thread; sessions the portal still holds
        // for this connection go away with it.
        zbus::block_on(link.bus.inner().clone().close())?;
        let _ = from_bus.join();
        Ok(())
    }

    fn on_app_message(self: &Arc<Self>, msg: &Message) -> anyhow::Result<()> {
        let hdr = msg.header();
        if hdr.message_type() != MessageType::MethodCall {
            return Ok(());
        }
        let reply = if let Some(reply) = self.ours(msg, &hdr)? {
            reply
        } else if is_portal_call(&hdr) {
            match self.forward_to_portal(msg, &hdr) {
                Ok(()) => return Ok(()),
                Err(err) => failed(&hdr, format!("{err:#}"))?,
            }
        } else if hdr
            .interface()
            .is_some_and(|i| i.as_str() == NOTIFICATIONS_NAME)
        {
            self.notification(msg, &hdr)?
        } else {
            Message::error(&hdr, "org.freedesktop.DBus.Error.UnknownMethod")?.build(&format!(
                "the bridge does not carry {}.{}",
                hdr.interface().map(|i| i.as_str()).unwrap_or("?"),
                hdr.member().map(|m| m.as_str()).unwrap_or("?")
            ))?
        };
        self.p2p.send(&reply)?;
        Ok(())
    }

    /// What we answer without xdg-desktop-portal: the file chooser (drv-portal's), its
    /// `version`, and `Close` on a request of ours. `None` is "not ours, forward it".
    fn ours(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<Option<Message>> {
        let path = hdr.path().context("no path")?.as_str().to_owned();
        let interface = hdr.interface().map(|i| i.as_str().to_owned()).unwrap_or_default();
        let member = hdr.member().map(|m| m.as_str().to_owned()).unwrap_or_default();
        if path == PORTAL_PATH && interface == FILE_CHOOSER {
            return self.file_chooser(msg, hdr, &member).map(Some);
        }
        if path == PORTAL_PATH && interface == PROPERTIES {
            match member.as_str() {
                "Get" => {
                    let (iface, prop): (String, String) = msg.body().deserialize()?;
                    if iface == FILE_CHOOSER {
                        return Ok(Some(if prop == "version" {
                            Message::method_return(hdr)?.build(&Value::U32(FILE_CHOOSER_VERSION))?
                        } else {
                            Message::error(hdr, "org.freedesktop.DBus.Error.InvalidArgs")?
                                .build(&format!("no property {prop} on {FILE_CHOOSER}"))?
                        }));
                    }
                }
                "GetAll" => {
                    let (iface,): (String,) = msg.body().deserialize()?;
                    if iface == FILE_CHOOSER {
                        let mut all: HashMap<&str, Value<'_>> = HashMap::new();
                        all.insert("version", Value::U32(FILE_CHOOSER_VERSION));
                        return Ok(Some(Message::method_return(hdr)?.build(&all)?));
                    }
                }
                _ => {}
            }
            return Ok(None);
        }
        if interface == REQUEST_IFACE && member == "Close" {
            let id = self.state.lock().unwrap().ours.remove(&path);
            if let Some(id) = id {
                self.portal.cancel(id);
                return Ok(Some(Message::method_return(hdr)?.build(&())?));
            }
        }
        Ok(None)
    }

    /// `OpenFile`/`SaveFile`: the handle goes back now, the `Response` signal when the
    /// person has picked. The app's `title` is shown as a hint under our own line naming it.
    fn file_chooser(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Message> {
        let caller = match hdr.destination() {
            Some(BusName::Unique(name)) => sender_component(name.as_str()),
            _ => bail!("no caller"),
        };
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
        // The token is the app's; it must be one path element.
        let token: String = string("handle_token")
            .unwrap_or_else(|| format!("drv{}", msg.primary_header().serial_num()))
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        let handle = format!("{PORTAL_PATH}/request/{caller}/{token}");
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
                protocol::Response::Hello { .. } => return,
            };
            let mut results: HashMap<&str, Value<'_>> = HashMap::new();
            if code == 0 {
                results.insert("uris", Value::from(uris));
            }
            let signal = Message::signal(signal_path.as_str(), REQUEST_IFACE, "Response")
                .and_then(|b| b.destination(unique_from_component(&caller)))
                .and_then(|b| b.build(&(code, results)));
            let sent = signal.and_then(|s| link.p2p.send(&s));
            if let Err(err) = sent {
                eprintln!("bridge: {}: file chooser response: {err}", link.app.name);
            }
        })?;
        self.state.lock().unwrap().ours.insert(handle.clone(), id);
        Ok(Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)
    }

    fn forward_to_portal(&self, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<()> {
        // The shim put the app-side caller's unique name in the destination field.
        let caller = match hdr.destination() {
            Some(BusName::Unique(name)) => sender_component(name.as_str()),
            _ => bail!("no caller"),
        };
        let path = hdr.path().context("no path")?.as_str();
        let path = rewrite_handle(path, &caller, &self.me).unwrap_or_else(|| path.to_owned());
        let mut tokens = Vec::new();
        let body = rewritten_body(msg, &caller, &self.me, &mut tokens)?;
        let mut builder = Message::method_call(path, hdr.member().context("no member")?.clone())?
            .destination(PORTAL_NAME)?;
        if let Some(interface) = hdr.interface() {
            builder = builder.interface(interface.clone())?;
        }
        let forwarded = with_body(builder, body)?;
        {
            let mut state = self.state.lock().unwrap();
            state
                .pending
                .insert(forwarded.primary_header().serial_num(), msg.clone());
            for token in tokens {
                state.owners.insert(token, caller.clone());
            }
        }
        self.bus.send(&forwarded)?;
        Ok(())
    }

    fn on_bus_message(&self, msg: &Message) -> anyhow::Result<()> {
        let hdr = msg.header();
        match hdr.message_type() {
            MessageType::MethodReturn | MessageType::Error => {
                let Some(serial) = hdr.reply_serial() else {
                    return Ok(());
                };
                let Some(call) = self.state.lock().unwrap().pending.remove(&serial) else {
                    return Ok(());
                };
                let call_hdr = call.header();
                let caller = match call_hdr.destination() {
                    Some(BusName::Unique(name)) => sender_component(name.as_str()),
                    _ => bail!("pending call without caller"),
                };
                let mut tokens = Vec::new();
                let body = rewritten_body(msg, &self.me, &caller, &mut tokens)?;
                {
                    let mut state = self.state.lock().unwrap();
                    for token in tokens {
                        state.owners.insert(token, caller.clone());
                    }
                }
                self.p2p.send(&mirrored_reply(&call_hdr, &hdr, body)?)?;
            }
            MessageType::Signal => {
                let path = hdr.path().context("no path")?.as_str();
                let Some((_, owner, token)) = drv_bridge::handle_parts(path) else {
                    return Ok(());
                };
                if owner != self.me {
                    return Ok(());
                }
                let Some(caller) = self.state.lock().unwrap().owners.get(token).cloned() else {
                    return Ok(());
                };
                let new_path = rewrite_handle(path, &self.me, &caller).context("handle")?;
                let mut tokens = Vec::new();
                let body = rewritten_body(msg, &self.me, &caller, &mut tokens)?;
                {
                    let mut state = self.state.lock().unwrap();
                    for token in tokens {
                        state.owners.insert(token, caller.clone());
                    }
                }
                let builder = Message::signal(
                    new_path,
                    hdr.interface().context("no interface")?.clone(),
                    hdr.member().context("no member")?.clone(),
                )?
                .destination(unique_from_component(&caller))?;
                self.p2p.send(&with_body(builder, body)?)?;
            }
            MessageType::MethodCall => {}
        }
        Ok(())
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
