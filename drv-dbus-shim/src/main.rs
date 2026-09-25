//! The D-Bus shim, run as the app on its private session bus (`dbus-run-session`). It owns
//! the desktop names apps expect (`org.freedesktop.portal.Desktop`,
//! `org.freedesktop.Notifications`), answers what it can itself (settings, versions) and
//! turns the rest into the set's own wires: files to drv-files, screens and cameras to
//! drv-cast, notifications to drv-shell, URIs to drv-appd. Each of those keys the
//! connection on this uid; the shim is compatibility, never a boundary. Whatever an app
//! does to this process, it gains only the ability to speak those wires directly, which it
//! could anyway. D-Bus ends here.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use anyhow::{Context as _, bail};
use clap::Parser;
use drv_cast::wire::{Cursor, FromCast, Source, ToCast};
use drv_dbus_shim::{sender_component, NOTIFICATIONS_NAME, NOTIFICATIONS_PATH, PORTAL_NAME, PORTAL_PATH};
use drv_files::wire::{FromFiles, Kind as Chooser, ToFiles};
use drv_policy::PolicyClient;
use drv_policy::seq;
use drv_shell::notify::{FromShell, ToShell};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zbus::blocking::Connection;
use zbus::message::{Header, Message, Type as MessageType};
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue, Signature, Structure, Value};

const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
const FILE_CHOOSER_VERSION: u32 = 4;
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const SCREEN_CAST_VERSION: u32 = 4;
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const SETTINGS_VERSION: u32 = 2;
const CAMERA: &str = "org.freedesktop.portal.Camera";
const CAMERA_VERSION: u32 = 1;
/// `OpenURI` only: version 1 has neither `OpenFile` nor `OpenDirectory`.
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const OPEN_URI_VERSION: u32 = 1;
/// `AvailableSourceTypes`: monitors and windows.
const SOURCE_TYPES: u32 = 1 | 2;
/// `AvailableCursorModes`: hidden, embedded, metadata.
const CURSOR_MODES: u32 = 1 | 2 | 4;

const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const FAILED: &str = "org.freedesktop.DBus.Error.Failed";

static TRACE: LazyLock<bool> = LazyLock::new(|| std::env::var_os("DRV_SHIM_TRACE").is_some());

#[derive(Parser)]
#[command(name = "drv-dbus-shim", about = "The desktop's D-Bus names on an app's private bus")]
struct Args {
    /// drv-files' socket.
    #[arg(long, default_value = drv_files::wire::SOCKET)]
    files: PathBuf,
    /// drv-cast's socket.
    #[arg(long, default_value = drv_cast::wire::SOCKET)]
    cast: PathBuf,
    /// drv-shell's notification socket.
    #[arg(long, default_value = drv_shell::notify::SOCKET)]
    notify: PathBuf,
    /// drv-appd's public socket, for OpenURI.
    #[arg(long, env = "DRV_APPD_SOCKET", default_value = "/run/drv/appd.sock")]
    appd: PathBuf,
    /// The app, run once the names are owned.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(args) {
        drv_os::say!("drv-dbus-shim: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let bus_iter = zbus::blocking::connection::Builder::session()?
        .build_message_iterator()
        .context("the app's private bus")?;
    let bus = Connection::from(zbus::Connection::from(bus_iter.inner()));
    bus.request_name(NOTIFICATIONS_NAME)?;
    bus.request_name(PORTAL_NAME)?;

    let shim = Arc::new(Shim {
        bus,
        paths: Paths { files: args.files, cast: args.cast, notify: args.notify, appd: args.appd },
        files: OnceLock::new(),
        cast: OnceLock::new(),
        notify: OnceLock::new(),
        next: AtomicU64::new(1),
        state: Mutex::new(State::default()),
    });
    {
        let shim = shim.clone();
        std::thread::spawn(move || {
            for msg in bus_iter {
                let Ok(msg) = msg else { break };
                if let Err(err) = shim.on_app_message(&msg) {
                    drv_os::say!("drv-dbus-shim: from app: {err:#}");
                }
            }
        });
    }

    let status = Command::new(&args.command[0])
        .args(&args.command[1..])
        .status()
        .with_context(|| args.command[0].clone())?;
    std::process::exit(status.code().unwrap_or(1));
}

fn connect(path: &Path) -> anyhow::Result<OwnedFd> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let sock = rustix::net::socket_with(AddressFamily::UNIX, SocketType::SEQPACKET, SocketFlags::CLOEXEC, None)?;
    rustix::net::connect(&sock, &SocketAddrUnix::new(path)?).with_context(|| path.display().to_string())?;
    Ok(sock)
}

// ---------------------------------------------------------------- a service's line

/// An answer from a service names the request it answers, or nothing (an event).
trait Answer: DeserializeOwned + std::fmt::Debug + Send + 'static {
    fn req(&self) -> Option<u64>;
    const VERSION: u32;
    fn is_hello(&self, version: u32) -> bool;
}

impl Answer for FromFiles {
    fn req(&self) -> Option<u64> {
        match self {
            FromFiles::Chosen { req, .. } | FromFiles::Cancelled { req } | FromFiles::Failed { req, .. } => Some(*req),
            FromFiles::Hello { .. } => None,
        }
    }
    const VERSION: u32 = drv_files::wire::VERSION;
    fn is_hello(&self, version: u32) -> bool {
        matches!(self, FromFiles::Hello { version: v } if *v == version)
    }
}

impl Answer for FromCast {
    fn req(&self) -> Option<u64> {
        FromCast::req(self)
    }
    const VERSION: u32 = drv_cast::wire::VERSION;
    fn is_hello(&self, version: u32) -> bool {
        matches!(self, FromCast::Hello { version: v } if *v == version)
    }
}

impl Answer for FromShell {
    fn req(&self) -> Option<u64> {
        match self {
            FromShell::Notified { req, .. } | FromShell::Failed { req, .. } => Some(*req),
            FromShell::Hello { .. } => None,
        }
    }
    const VERSION: u32 = drv_shell::notify::VERSION;
    fn is_hello(&self, version: u32) -> bool {
        matches!(self, FromShell::Hello { version: v } if *v == version)
    }
}

type On<From> = Box<dyn FnOnce(From, Vec<OwnedFd>) + Send>;

/// One connection to one service, made on first use. Requests go out under our numbers;
/// answers come back on a reader thread and find their asker here. Events go to `on_event`.
struct Link<From: Answer> {
    sock: OwnedFd,
    sending: Mutex<()>,
    waiting: Mutex<HashMap<u64, On<From>>>,
}

impl<From: Answer> Link<From> {
    fn open<To: Serialize>(
        path: &Path,
        hello: To,
        on_event: impl Fn(From) + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let sock = connect(path)?;
        seq::send(&sock, &hello, &[]).context("hello")?;
        let (answer, _) = seq::recv::<From>(&sock).context("hello back")?;
        if !answer.is_hello(From::VERSION) {
            bail!("{} answered {answer:?}, not version {}", path.display(), From::VERSION);
        }
        let link = Arc::new(Self { sock, sending: Mutex::new(()), waiting: Mutex::new(HashMap::new()) });
        let reader = link.clone();
        let who = path.display().to_string();
        std::thread::spawn(move || {
            loop {
                match seq::recv::<From>(&reader.sock) {
                    Ok((msg, fds)) => match msg.req() {
                        Some(req) => {
                            let waiter = reader.waiting.lock().unwrap().remove(&req);
                            match waiter {
                                Some(on) => on(msg, fds),
                                None => drv_os::say!("drv-dbus-shim: {who} answered {req}, which nobody asked"),
                            }
                        }
                        None => on_event(msg),
                    },
                    Err(err) => {
                        drv_os::say!("drv-dbus-shim: lost {who}: {err}");
                        break;
                    }
                }
            }
        });
        Ok(link)
    }

    fn tell<To: Serialize>(&self, msg: &To) -> anyhow::Result<()> {
        let _one = self.sending.lock().unwrap();
        seq::send(&self.sock, msg, &[]).context("to the service")
    }

    /// Sends `msg`; `on` gets the answer, on the reader thread.
    fn ask<To: Serialize>(&self, req: u64, msg: &To, on: impl FnOnce(From, Vec<OwnedFd>) + Send + 'static) -> anyhow::Result<()> {
        self.waiting.lock().unwrap().insert(req, Box::new(on));
        if let Err(err) = self.tell(msg) {
            self.waiting.lock().unwrap().remove(&req);
            return Err(err);
        }
        Ok(())
    }

    fn forget(&self, req: u64) {
        self.waiting.lock().unwrap().remove(&req);
    }
}

// ---------------------------------------------------------------- state

/// Which service a request handle went to.
#[derive(Clone, Copy)]
enum Where {
    Files,
    Cast,
}

/// A screencast session, from `CreateSession` to `Close` or the cast's end.
struct Session {
    /// Our number for it at drv-cast.
    id: u64,
    /// The unique name of the app-side connection that made it: `Closed` goes there.
    caller: String,
    cursor: Cursor,
    /// `SelectSources.types`: whole screens (bit 1), windows (bit 2).
    screens: bool,
    windows: bool,
    /// The app asked for a restore token (any `persist_mode`); it gets one that lasts while
    /// it runs.
    persist: bool,
    /// The token the app gave back, for the same source with no dialog.
    again: Option<String>,
    started: bool,
}

#[derive(Default)]
struct State {
    /// Request handles still waiting on the person, by path, with the request at the
    /// service: `Request.Close` withdraws it there.
    handles: HashMap<String, (Where, u64)>,
    /// Sessions by their path.
    sessions: HashMap<String, Session>,
}

struct Paths {
    files: PathBuf,
    cast: PathBuf,
    notify: PathBuf,
    appd: PathBuf,
}

struct Shim {
    bus: Connection,
    paths: Paths,
    files: OnceLock<Arc<Link<FromFiles>>>,
    cast: OnceLock<Arc<Link<FromCast>>>,
    notify: OnceLock<Arc<Link<FromShell>>>,
    next: AtomicU64,
    state: Mutex<State>,
}

/// What a call got.
enum Ours {
    Reply(Message),
    /// Answered already, or will be when the service answers.
    Done,
}

fn error(hdr: &Header<'_>, name: &str, text: String) -> anyhow::Result<Message> {
    Ok(Message::error(hdr, name)?.build(&text)?)
}

fn failed(hdr: &Header<'_>, text: String) -> anyhow::Result<Message> {
    error(hdr, FAILED, text)
}

/// The unique name of the app-side connection calling.
fn caller_of(hdr: &Header<'_>) -> anyhow::Result<String> {
    Ok(hdr.sender().context("no sender")?.as_str().to_owned())
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

/// The app's request handle for a call: its token (one path element) under its own caller
/// component, as the portal spec has it.
fn handle_for(msg: &Message, caller: &str, options: &HashMap<String, OwnedValue>, key: &str) -> String {
    let token: String = options
        .get(key)
        .cloned()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_else(|| format!("drv{}", msg.primary_header().serial_num()))
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let kind = if key == "session_handle_token" { "session" } else { "request" };
    format!("{PORTAL_PATH}/{kind}/{}/{token}", sender_component(caller))
}

impl Shim {
    fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// drv-files' line, opened on the first file request.
    fn files(&self) -> anyhow::Result<Arc<Link<FromFiles>>> {
        if let Some(l) = self.files.get() {
            return Ok(l.clone());
        }
        let hello = ToFiles::Hello { version: drv_files::wire::VERSION };
        let link = Link::open(&self.paths.files, hello, |_| {}).context("drv-files")?;
        Ok(self.files.get_or_init(|| link).clone())
    }

    /// drv-cast's line, opened on the first cast or camera request. `CastClosed` events
    /// end sessions.
    fn cast(self: &Arc<Self>) -> anyhow::Result<Arc<Link<FromCast>>> {
        if let Some(l) = self.cast.get() {
            return Ok(l.clone());
        }
        let hello = ToCast::Hello { version: drv_cast::wire::VERSION };
        let shim = self.clone();
        let link = Link::open(&self.paths.cast, hello, move |ev| {
            if let FromCast::CastClosed { session } = ev {
                shim.session_closed(session);
            }
        })
        .context("drv-cast")?;
        Ok(self.cast.get_or_init(|| link).clone())
    }

    /// drv-shell's notification line, opened on the first notification.
    fn notify(&self) -> anyhow::Result<Arc<Link<FromShell>>> {
        if let Some(l) = self.notify.get() {
            return Ok(l.clone());
        }
        let hello = ToShell::Hello { version: drv_shell::notify::VERSION };
        let link = Link::open(&self.paths.notify, hello, |_| {}).context("drv-shell")?;
        Ok(self.notify.get_or_init(|| link).clone())
    }

    /// drv-cast ended a session (the person, or the compositor): the app hears `Closed`.
    fn session_closed(&self, session: u64) {
        let closed = {
            let mut state = self.state.lock().unwrap();
            let path = state.sessions.iter().find(|(_, s)| s.id == session).map(|(p, _)| p.clone());
            path.and_then(|p| state.sessions.remove(&p).map(|s| (p, s)))
        };
        if let Some((path, s)) = closed {
            let sent = Message::signal(path.as_str(), SESSION_IFACE, "Closed")
                .and_then(|b| b.destination(s.caller.as_str()))
                .and_then(|b| b.build(&HashMap::<&str, Value<'_>>::new()))
                .and_then(|signal| self.bus.send(&signal));
            if let Err(err) = sent {
                drv_os::say!("drv-dbus-shim: session closed signal: {err}");
            }
        }
    }

    /// The portal `Response` signal on a request handle, to the app-side caller.
    fn respond(&self, handle: &str, caller: &str, code: u32, results: HashMap<&str, Value<'_>>) -> anyhow::Result<()> {
        let signal = Message::signal(handle, REQUEST_IFACE, "Response")?
            .destination(caller)?
            .build(&(code, results))?;
        self.bus.send(&signal).context("response signal")
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
            drv_os::say!("drv-dbus-shim: {}", what());
        }
        let reply = match self.call(msg, &hdr) {
            Ok(Ours::Reply(reply)) => reply,
            Ok(Ours::Done) => return Ok(()),
            Err(err) => {
                drv_os::say!("drv-dbus-shim: {}: {err:#}", what());
                failed(&hdr, format!("{err:#}"))?
            }
        };
        self.bus.send(&reply)?;
        Ok(())
    }

    fn call(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>) -> anyhow::Result<Ours> {
        let path = hdr.path().context("no path")?.as_str().to_owned();
        let interface = hdr.interface().map(|i| i.as_str().to_owned()).unwrap_or_default();
        let member = hdr.member().map(|m| m.as_str().to_owned()).unwrap_or_default();
        if path == NOTIFICATIONS_PATH && interface == NOTIFICATIONS_NAME {
            return self.notification(msg, hdr, &member);
        }
        if path == PORTAL_PATH {
            return match interface.as_str() {
                FILE_CHOOSER => self.file_chooser(msg, hdr, &member),
                SCREEN_CAST => self.screen_cast(msg, hdr, &member),
                CAMERA => self.camera(msg, hdr, &member),
                OPEN_URI => self.open_uri(msg, hdr, &member),
                SETTINGS => self.settings(msg, hdr, &member).map(Ours::Reply),
                PROPERTIES => self.properties(msg, hdr, &member),
                _ => unknown(hdr, &interface, &member),
            };
        }
        if interface == SESSION_IFACE && path.starts_with(PORTAL_PATH) {
            let session = {
                let mut state = self.state.lock().unwrap();
                if !state.sessions.contains_key(&path) {
                    return error(hdr, "org.freedesktop.DBus.Error.UnknownObject", "no such session".to_owned())
                        .map(Ours::Reply);
                }
                match member.as_str() {
                    "Close" => state.sessions.remove(&path),
                    other => bail!("no {other} on {SESSION_IFACE}"),
                }
            };
            if let Some(s) = session {
                self.cast()?.tell(&ToCast::CastClose { session: s.id })?;
            }
            return Ok(Ours::Reply(Message::method_return(hdr)?.build(&())?));
        }
        if interface == REQUEST_IFACE && member == "Close" && path.starts_with(PORTAL_PATH) {
            let handle = self.state.lock().unwrap().handles.remove(&path);
            if let Some((at, req)) = handle {
                match at {
                    Where::Files => {
                        let files = self.files()?;
                        files.forget(req);
                        files.tell(&ToFiles::Cancel { req })?;
                    }
                    Where::Cast => {
                        let cast = self.cast()?;
                        cast.forget(req);
                        cast.tell(&ToCast::Cancel { req })?;
                    }
                }
            }
            return Ok(Ours::Reply(Message::method_return(hdr)?.build(&())?));
        }
        if !path.starts_with(PORTAL_PATH) {
            return error(hdr, "org.freedesktop.DBus.Error.UnknownObject", "not here".to_owned()).map(Ours::Reply);
        }
        unknown(hdr, &interface, &member)
    }

    /// `org.freedesktop.DBus.Properties` on the portal object: versions, and what we offer.
    fn properties(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        let (iface, prop): (String, Option<String>) = match member {
            "Get" => {
                let (iface, prop): (String, String) = msg.body().deserialize()?;
                (iface, Some(prop))
            }
            "GetAll" => {
                let (iface,): (String,) = msg.body().deserialize()?;
                (iface, None)
            }
            other => return unknown(hdr, PROPERTIES, other),
        };
        let fixed: Option<Vec<(&str, Value<'static>)>> = match iface.as_str() {
            FILE_CHOOSER => Some(vec![("version", Value::U32(FILE_CHOOSER_VERSION))]),
            SETTINGS => Some(vec![("version", Value::U32(SETTINGS_VERSION))]),
            OPEN_URI => Some(vec![("version", Value::U32(OPEN_URI_VERSION))]),
            SCREEN_CAST => Some(vec![
                ("version", Value::U32(SCREEN_CAST_VERSION)),
                ("AvailableSourceTypes", Value::U32(SOURCE_TYPES)),
                ("AvailableCursorModes", Value::U32(CURSOR_MODES)),
            ]),
            CAMERA => None,
            _ => return unknown(hdr, PROPERTIES, member),
        };
        let reply = move |props: Vec<(&str, Value<'static>)>, hdr: &Header<'_>| -> anyhow::Result<Message> {
            Ok(match &prop {
                Some(prop) => match props.into_iter().find(|(name, _)| name == prop) {
                    Some((_, value)) => Message::method_return(hdr)?.build(&value)?,
                    None => error(hdr, "org.freedesktop.DBus.Error.InvalidArgs", format!("no property {prop} on {iface}"))?,
                },
                None => {
                    let all: HashMap<&str, Value<'_>> = props.into_iter().collect();
                    Message::method_return(hdr)?.build(&all)?
                }
            })
        };
        if let Some(props) = fixed {
            return reply(props, hdr).map(Ours::Reply);
        }
        // The camera's `IsCameraPresent` is drv-cast's to say.
        let shim = self.clone();
        let msg = msg.clone();
        let req = self.next();
        self.cast()?.ask(req, &ToCast::CameraPresent { req }, move |answer, _| {
            let hdr = msg.header();
            let present = matches!(answer, FromCast::Present { present: true, .. });
            let props = vec![("version", Value::U32(CAMERA_VERSION)), ("IsCameraPresent", Value::Bool(present))];
            if let Err(err) = reply(props, &hdr).and_then(|r| shim.bus.send(&r).map_err(Into::into)) {
                drv_os::say!("drv-dbus-shim: camera properties: {err:#}");
            }
        })?;
        Ok(Ours::Done)
    }

    /// `org.freedesktop.portal.Settings`: how the desktop looks, the same for every app.
    fn settings(&self, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Message> {
        // 1 is "prefer dark", which the desktop is.
        let all: [(&str, &str, Value<'static>); 2] = [
            ("org.freedesktop.appearance", "color-scheme", Value::U32(1)),
            ("org.freedesktop.appearance", "contrast", Value::U32(0)),
        ];
        Ok(match member {
            // `Read` wraps the value in a second variant, a mistake `ReadOne` corrected.
            "Read" | "ReadOne" => {
                let (namespace, key): (String, String) = msg.body().deserialize()?;
                match all.into_iter().find(|(ns, k, _)| *ns == namespace && *k == key) {
                    Some((_, _, value)) if member == "Read" => {
                        Message::method_return(hdr)?.build(&Value::Value(Box::new(value)))?
                    }
                    Some((_, _, value)) => Message::method_return(hdr)?.build(&value)?,
                    None => error(hdr, "org.freedesktop.portal.Error.NotFound", format!("no setting {namespace} {key}"))?,
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
            other => error(hdr, UNKNOWN_METHOD, format!("no {other} on {SETTINGS}"))?,
        })
    }

    /// `OpenFile`/`SaveFile`: the handle goes back now, the `Response` signal when the
    /// person has picked at drv-files.
    fn file_chooser(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        let caller = caller_of(hdr)?;
        let (_parent, _title, options): (String, String, HashMap<String, OwnedValue>) =
            msg.body().deserialize().context("FileChooser arguments")?;
        let string = |key: &str| options.get(key).cloned().and_then(|v| String::try_from(v).ok());
        let flag = |key: &str| options.get(key).cloned().and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        let kind = match member {
            "OpenFile" if flag("directory") => bail!("directories are not handed out yet"),
            "OpenFile" => Chooser::Open,
            "SaveFile" => Chooser::Save {
                name: string("current_name").unwrap_or_default(),
            },
            other => bail!("no {other} on {FILE_CHOOSER}"),
        };
        let files = self.files()?;
        let handle = handle_for(msg, &caller, &options, "handle_token");
        // The reply first, then the signal: the spec's order.
        self.bus.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
        let req = self.next();
        self.state.lock().unwrap().handles.insert(handle.clone(), (Where::Files, req));
        let shim = self.clone();
        files.ask(req, &ToFiles::Choose { req, kind }, move |answer, _| {
            shim.state.lock().unwrap().handles.remove(&handle);
            let (code, uris): (u32, Vec<String>) = match answer {
                FromFiles::Chosen { paths, .. } => (0, paths.iter().map(|p| file_uri(p)).collect()),
                FromFiles::Cancelled { .. } => (1, Vec::new()),
                other => {
                    drv_os::say!("drv-dbus-shim: file chooser: {other:?}");
                    (2, Vec::new())
                }
            };
            let mut results: HashMap<&str, Value<'_>> = HashMap::new();
            if code == 0 {
                results.insert("uris", Value::from(uris));
            }
            if let Err(err) = shim.respond(&handle, &caller, code, results) {
                drv_os::say!("drv-dbus-shim: file chooser response: {err}");
            }
        })?;
        Ok(Ours::Done)
    }

    /// `org.freedesktop.portal.ScreenCast`: the session is ours, the person consents at the
    /// shell on `Start`, and `OpenPipeWireRemote` hands out a connection that sees the one
    /// node.
    fn screen_cast(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        let caller = caller_of(hdr)?;
        let reply_handle = |handle: &str| -> anyhow::Result<Message> {
            Ok(Message::method_return(hdr)?.build(&ObjectPath::try_from(handle)?)?)
        };
        match member {
            "CreateSession" => {
                let (options,): (HashMap<String, OwnedValue>,) = msg.body().deserialize()?;
                let handle = handle_for(msg, &caller, &options, "handle_token");
                let session = handle_for(msg, &caller, &options, "session_handle_token");
                self.state.lock().unwrap().sessions.insert(
                    session.clone(),
                    Session {
                        id: self.next(),
                        caller: caller.clone(),
                        cursor: Cursor::Embedded,
                        screens: true,
                        windows: false,
                        persist: false,
                        again: None,
                        started: false,
                    },
                );
                // The reply first, then the signal: the spec's order.
                self.bus.send(&reply_handle(&handle)?)?;
                let mut results: HashMap<&str, Value<'_>> = HashMap::new();
                results.insert("session_handle", Value::from(session.as_str()));
                self.respond(&handle, &caller, 0, results)?;
                Ok(Ours::Done)
            }
            "SelectSources" => {
                let (session, options): (OwnedObjectPath, HashMap<String, OwnedValue>) =
                    msg.body().deserialize()?;
                let cursor = match options.get("cursor_mode").cloned().and_then(|v| u32::try_from(v).ok()) {
                    Some(1) => Cursor::Hidden,
                    Some(4) => Cursor::Metadata,
                    _ => Cursor::Embedded,
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
                let handle = handle_for(msg, &caller, &options, "handle_token");
                self.bus.send(&reply_handle(&handle)?)?;
                self.respond(&handle, &caller, 0, HashMap::new())?;
                Ok(Ours::Done)
            }
            "Start" => {
                let (session, _parent, options): (OwnedObjectPath, String, HashMap<String, OwnedValue>) =
                    msg.body().deserialize()?;
                let req = self.next();
                let (ask, persist) = {
                    let mut state = self.state.lock().unwrap();
                    let s = state.sessions.get_mut(session.as_str()).context("no such session")?;
                    anyhow::ensure!(!s.started, "the session was started already");
                    s.started = true;
                    (
                        ToCast::Cast {
                            req,
                            session: s.id,
                            cursor: s.cursor,
                            screens: s.screens,
                            windows: s.windows,
                            again: s.again.clone(),
                        },
                        s.persist,
                    )
                };
                let cast = self.cast()?;
                let handle = handle_for(msg, &caller, &options, "handle_token");
                self.bus.send(&reply_handle(&handle)?)?;
                self.state.lock().unwrap().handles.insert(handle.clone(), (Where::Cast, req));
                let shim = self.clone();
                cast.ask(req, &ask, move |answer, _| {
                    shim.state.lock().unwrap().handles.remove(&handle);
                    let res = match answer {
                        FromCast::Cast { node_id, source, width, height, token, .. } => {
                            let mut stream: HashMap<&str, Value<'_>> = HashMap::new();
                            let (source_type, source_id) = match source {
                                Source::Screen(name) => (1, name),
                                Source::Window(id) => (2, id.to_string()),
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
                                    drv_os::say!("drv-dbus-shim: screencast streams: {err:#}");
                                    return;
                                }
                            };
                            if persist {
                                // 1: for as long as the app runs, whatever it asked for.
                                results.insert("persist_mode", Value::U32(1));
                                results.insert("restore_token", Value::from(token));
                            }
                            shim.respond(&handle, &caller, 0, results)
                        }
                        FromCast::Cancelled { .. } => shim.respond(&handle, &caller, 1, HashMap::new()),
                        other => {
                            drv_os::say!("drv-dbus-shim: screencast: {other:?}");
                            shim.respond(&handle, &caller, 2, HashMap::new())
                        }
                    };
                    if let Err(err) = res {
                        drv_os::say!("drv-dbus-shim: screencast: {err:#}");
                    }
                })?;
                Ok(Ours::Done)
            }
            "OpenPipeWireRemote" => {
                let (session, _options): (OwnedObjectPath, HashMap<String, OwnedValue>) =
                    msg.body().deserialize()?;
                let id = self
                    .state
                    .lock()
                    .unwrap()
                    .sessions
                    .get(session.as_str())
                    .context("no such session")?
                    .id;
                let req = self.next();
                self.remote(msg, req, ToCast::CastRemote { req, session: id })
            }
            other => bail!("no {other} on {SCREEN_CAST}"),
        }
    }

    /// Asks drv-cast for a PipeWire connection and answers `msg` with the fd it sends.
    fn remote(self: &Arc<Self>, msg: &Message, req: u64, ask: ToCast) -> anyhow::Result<Ours> {
        let shim = self.clone();
        let msg = msg.clone();
        self.cast()?.ask(req, &ask, move |answer, mut fds| {
            let hdr = msg.header();
            let reply = match (answer, fds.pop()) {
                (FromCast::Remote { .. }, Some(fd)) => Message::method_return(&hdr).and_then(|b| b.build(&zbus::zvariant::Fd::from(fd))).map_err(Into::into),
                (FromCast::Failed { reason, .. }, _) => failed(&hdr, reason),
                (other, _) => failed(&hdr, format!("no remote: {other:?}")),
            };
            if let Err(err) = reply.and_then(|r| shim.bus.send(&r).map_err(Into::into)) {
                drv_os::say!("drv-dbus-shim: remote: {err:#}");
            }
        })?;
        Ok(Ours::Done)
    }

    /// `org.freedesktop.portal.Camera`: no question here. Chromium asks for access to list
    /// the cameras at the first page that enumerates devices, so access is granted and the
    /// remote sees every camera; the person is asked when a stream is to be linked to one
    /// (WirePlumber's gate, as for the microphone), which is when a page captures.
    fn camera(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        match member {
            "AccessCamera" => {
                let caller = caller_of(hdr)?;
                let (options,): (HashMap<String, OwnedValue>,) = msg.body().deserialize()?;
                let handle = handle_for(msg, &caller, &options, "handle_token");
                // The reply first, then the signal: the spec's order.
                self.bus.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
                self.respond(&handle, &caller, 0, HashMap::new())?;
                Ok(Ours::Done)
            }
            "OpenPipeWireRemote" => {
                let req = self.next();
                self.remote(msg, req, ToCast::CameraRemote { req })
            }
            other => bail!("no {other} on {CAMERA}"),
        }
    }

    /// `OpenURI`: drv-appd starts the manifest handler; no prompt. `writable`, `ask` and
    /// the parent window are ignored. Its own connection, on its own thread: the answer
    /// waits on a launch.
    fn open_uri(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        if member != "OpenURI" {
            bail!("no {member} on {OPEN_URI}");
        }
        let caller = caller_of(hdr)?;
        let (_parent, uri, options): (String, String, HashMap<String, OwnedValue>) =
            msg.body().deserialize().context("OpenURI arguments")?;
        let handle = handle_for(msg, &caller, &options, "handle_token");
        self.bus.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
        let shim = self.clone();
        std::thread::spawn(move || {
            let opened = PolicyClient::connect(shim.paths.appd.clone())
                .and_then(|mut appd| appd.open(uri.clone()));
            let code = match opened {
                Ok(uid) => {
                    drv_os::say!("drv-dbus-shim: {uri:?} opens as uid {uid}");
                    0
                }
                Err(err) => {
                    drv_os::say!("drv-dbus-shim: OpenURI: {err}");
                    2
                }
            };
            if let Err(err) = shim.respond(&handle, &caller, code, HashMap::new()) {
                drv_os::say!("drv-dbus-shim: OpenURI response: {err}");
            }
        });
        Ok(Ours::Done)
    }

    fn notification(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        Ok(Ours::Reply(match member {
            // "actions" is advertised though the shell shows none: Chromium will not use a
            // server without it and draws its own notifications instead.
            "GetCapabilities" => Message::method_return(hdr)?.build(&vec!["body", "actions"])?,
            "GetServerInformation" => Message::method_return(hdr)?.build(&(
                "drv-dbus-shim",
                "drv",
                env!("CARGO_PKG_VERSION"),
                "1.2",
            ))?,
            "CloseNotification" => {
                let (id,): (u32,) = msg.body().deserialize()?;
                self.notify()?.tell(&ToShell::Close { id })?;
                Message::method_return(hdr)?.build(&())?
            }
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
                let notify = self.notify()?;
                let req = self.next();
                let shim = self.clone();
                let msg = msg.clone();
                notify.ask(req, &ToShell::Notify { req, replaces, summary, body }, move |answer, _| {
                    let hdr = msg.header();
                    let reply = match answer {
                        FromShell::Notified { id, .. } => Message::method_return(&hdr).and_then(|b| b.build(&id)).map_err(Into::into),
                        FromShell::Failed { reason, .. } => failed(&hdr, reason),
                        other => failed(&hdr, format!("{other:?}")),
                    };
                    if let Err(err) = reply.and_then(|r| shim.bus.send(&r).map_err(Into::into)) {
                        drv_os::say!("drv-dbus-shim: notification reply: {err:#}");
                    }
                })?;
                return Ok(Ours::Done);
            }
            other => error(hdr, UNKNOWN_METHOD, format!("no {other} on {NOTIFICATIONS_NAME}"))?,
        }))
    }
}

/// There is no other portal behind the shim.
fn unknown(hdr: &Header<'_>, interface: &str, member: &str) -> anyhow::Result<Ours> {
    error(hdr, UNKNOWN_METHOD, format!("the shim does not carry {interface}.{member}")).map(Ours::Reply)
}
