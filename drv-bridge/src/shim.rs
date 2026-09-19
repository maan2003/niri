//! The app's side: a D-Bus service on the app's private bus, running as the app. It owns
//! the desktop names, answers what it can itself (settings, versions), and turns the rest
//! into `wire` for the server. Whatever an app does to this process, it gains only the
//! ability to speak `wire` directly, which it could anyway.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use drv_bridge::wire::{self, Chooser, Cursor, Source, ToServer, ToShim};
use drv_bridge::{sender_component, NOTIFICATIONS_NAME, NOTIFICATIONS_PATH, PORTAL_NAME, PORTAL_PATH};
use drv_policy::seq;
use zbus::blocking::Connection;
use zbus::message::{Header, Message, Type as MessageType};
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue, Signature, Structure, Value};

use crate::TRACE;

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

pub fn run(socket: PathBuf, command: Vec<String>) -> anyhow::Result<()> {
    let sock = connect(&socket).with_context(|| socket.display().to_string())?;
    seq::send(&sock, &ToServer::Hello { version: wire::VERSION }, &[]).context("hello to the bridge")?;
    match seq::recv::<ToShim>(&sock).context("hello from the bridge")?.0 {
        ToShim::Hello { version } if version == wire::VERSION => {}
        other => bail!("the bridge answered {other:?}, not version {}", wire::VERSION),
    }
    let bus_iter = zbus::blocking::connection::Builder::session()?
        .build_message_iterator()
        .context("the app's private bus")?;
    let bus = Connection::from(zbus::Connection::from(bus_iter.inner()));
    bus.request_name(NOTIFICATIONS_NAME)?;
    bus.request_name(PORTAL_NAME)?;

    let shim = Arc::new(Shim {
        bus,
        sock,
        sending: Mutex::new(()),
        next: AtomicU64::new(1),
        waiting: Mutex::new(HashMap::new()),
        state: Mutex::new(State::default()),
    });
    {
        let shim = shim.clone();
        std::thread::spawn(move || {
            for msg in bus_iter {
                let Ok(msg) = msg else { break };
                if let Err(err) = shim.on_app_message(&msg) {
                    drv_os::say!("drv-bridge: from app: {err:#}");
                }
            }
        });
    }
    {
        let shim = shim.clone();
        std::thread::spawn(move || loop {
            match seq::recv::<ToShim>(&shim.sock) {
                Ok((msg, fds)) => shim.on_server_message(msg, fds),
                Err(err) => {
                    drv_os::say!("drv-bridge: lost the bridge server: {err}");
                    break;
                }
            }
        });
    }

    let status = Command::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| command[0].clone())?;
    std::process::exit(status.code().unwrap_or(1));
}

fn connect(path: &Path) -> anyhow::Result<OwnedFd> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let sock = rustix::net::socket_with(AddressFamily::UNIX, SocketType::SEQPACKET, SocketFlags::CLOEXEC, None)?;
    rustix::net::connect(&sock, &SocketAddrUnix::new(path)?)?;
    Ok(sock)
}

// ---------------------------------------------------------------- state

type Answer = Box<dyn FnOnce(ToShim, Vec<OwnedFd>) + Send>;

/// A screencast session, from `CreateSession` to `Close` or the cast's end.
struct Session {
    /// Our number for it at the server.
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
    /// server: `Request.Close` withdraws it there.
    handles: HashMap<String, u64>,
    /// Sessions by their path.
    sessions: HashMap<String, Session>,
    /// The camera was allowed this run.
    camera: bool,
}

struct Shim {
    bus: Connection,
    sock: OwnedFd,
    sending: Mutex<()>,
    next: AtomicU64,
    /// Answers from the server find their asker here.
    waiting: Mutex<HashMap<u64, Answer>>,
    state: Mutex<State>,
}

/// What a call got.
enum Ours {
    Reply(Message),
    /// Answered already, or will be when the server answers.
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

    fn tell(&self, msg: &ToServer) -> anyhow::Result<()> {
        let _one = self.sending.lock().unwrap();
        seq::send(&self.sock, msg, &[]).context("to the bridge")
    }

    /// Sends `msg`; `on` gets the answer, on the server's thread.
    fn ask(&self, req: u64, msg: &ToServer, on: impl FnOnce(ToShim, Vec<OwnedFd>) + Send + 'static) -> anyhow::Result<()> {
        self.waiting.lock().unwrap().insert(req, Box::new(on));
        if let Err(err) = self.tell(msg) {
            self.waiting.lock().unwrap().remove(&req);
            return Err(err);
        }
        Ok(())
    }

    /// The portal `Response` signal on a request handle, to the app-side caller.
    fn respond(&self, handle: &str, caller: &str, code: u32, results: HashMap<&str, Value<'_>>) -> anyhow::Result<()> {
        let signal = Message::signal(handle, REQUEST_IFACE, "Response")?
            .destination(caller)?
            .build(&(code, results))?;
        self.bus.send(&signal).context("response signal")
    }

    fn on_server_message(&self, msg: ToShim, fds: Vec<OwnedFd>) {
        match msg.req() {
            Some(req) => {
                let waiter = self.waiting.lock().unwrap().remove(&req);
                match waiter {
                    Some(on) => on(msg, fds),
                    None => drv_os::say!("drv-bridge: the bridge answered {req}, which nobody asked"),
                }
            }
            None => match msg {
                ToShim::CastClosed { session } => {
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
                            drv_os::say!("drv-bridge: session closed signal: {err}");
                        }
                    }
                }
                ToShim::Hello { .. } => {}
                _ => {}
            },
        }
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
            drv_os::say!("drv-bridge: {}", what());
        }
        let reply = match self.call(msg, &hdr) {
            Ok(Ours::Reply(reply)) => reply,
            Ok(Ours::Done) => return Ok(()),
            Err(err) => {
                drv_os::say!("drv-bridge: {}: {err:#}", what());
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
                self.tell(&ToServer::CastClose { session: s.id })?;
            }
            return Ok(Ours::Reply(Message::method_return(hdr)?.build(&())?));
        }
        if interface == REQUEST_IFACE && member == "Close" && path.starts_with(PORTAL_PATH) {
            let req = self.state.lock().unwrap().handles.remove(&path);
            if let Some(req) = req {
                self.waiting.lock().unwrap().remove(&req);
                self.tell(&ToServer::Cancel { req })?;
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
        // The camera's `IsCameraPresent` is the server's to say.
        let shim = self.clone();
        let msg = msg.clone();
        let req = self.next();
        self.ask(req, &ToServer::CameraPresent { req }, move |answer, _| {
            let hdr = msg.header();
            let present = matches!(answer, ToShim::Present { present: true, .. });
            let props = vec![("version", Value::U32(CAMERA_VERSION)), ("IsCameraPresent", Value::Bool(present))];
            if let Err(err) = reply(props, &hdr).and_then(|r| shim.bus.send(&r).map_err(Into::into)) {
                drv_os::say!("drv-bridge: camera properties: {err:#}");
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
    /// person has picked.
    fn file_chooser(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        let caller = caller_of(hdr)?;
        let (_parent, title, options): (String, String, HashMap<String, OwnedValue>) =
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
        let handle = handle_for(msg, &caller, &options, "handle_token");
        // The reply first, then the signal: the spec's order.
        self.bus.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
        let req = self.next();
        self.state.lock().unwrap().handles.insert(handle.clone(), req);
        let shim = self.clone();
        self.ask(req, &ToServer::Choose { req, title, kind }, move |answer, _| {
            shim.state.lock().unwrap().handles.remove(&handle);
            let (code, uris): (u32, Vec<String>) = match answer {
                ToShim::Files { paths, .. } => (0, paths.iter().map(|p| file_uri(p)).collect()),
                ToShim::Cancelled { .. } => (1, Vec::new()),
                other => {
                    drv_os::say!("drv-bridge: file chooser: {other:?}");
                    (2, Vec::new())
                }
            };
            let mut results: HashMap<&str, Value<'_>> = HashMap::new();
            if code == 0 {
                results.insert("uris", Value::from(uris));
            }
            if let Err(err) = shim.respond(&handle, &caller, code, results) {
                drv_os::say!("drv-bridge: file chooser response: {err}");
            }
        })?;
        Ok(Ours::Done)
    }

    /// `org.freedesktop.portal.ScreenCast`: the session is ours, the person consents at
    /// drv-portal on `Start`, and `OpenPipeWireRemote` hands out a connection that sees the
    /// one node.
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
                let (ask, persist) = {
                    let mut state = self.state.lock().unwrap();
                    let s = state.sessions.get_mut(session.as_str()).context("no such session")?;
                    anyhow::ensure!(!s.started, "the session was started already");
                    s.started = true;
                    (
                        ToServer::Cast {
                            req: 0,
                            session: s.id,
                            cursor: s.cursor,
                            screens: s.screens,
                            windows: s.windows,
                            again: s.again.clone(),
                        },
                        s.persist,
                    )
                };
                let req = self.next();
                let ask = match ask {
                    ToServer::Cast { session, cursor, screens, windows, again, .. } => {
                        ToServer::Cast { req, session, cursor, screens, windows, again }
                    }
                    other => other,
                };
                let handle = handle_for(msg, &caller, &options, "handle_token");
                self.bus.send(&reply_handle(&handle)?)?;
                self.state.lock().unwrap().handles.insert(handle.clone(), req);
                let shim = self.clone();
                self.ask(req, &ask, move |answer, _| {
                    shim.state.lock().unwrap().handles.remove(&handle);
                    let res = match answer {
                        ToShim::Cast { node_id, source, width, height, token, .. } => {
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
                                    drv_os::say!("drv-bridge: screencast streams: {err:#}");
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
                        ToShim::Cancelled { .. } => shim.respond(&handle, &caller, 1, HashMap::new()),
                        other => {
                            drv_os::say!("drv-bridge: screencast: {other:?}");
                            shim.respond(&handle, &caller, 2, HashMap::new())
                        }
                    };
                    if let Err(err) = res {
                        drv_os::say!("drv-bridge: screencast: {err:#}");
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
                self.remote(msg, ToServer::CastRemote { req: 0, session: id })
            }
            other => bail!("no {other} on {SCREEN_CAST}"),
        }
    }

    /// Asks the server for a PipeWire connection and answers `msg` with the fd it sends.
    fn remote(self: &Arc<Self>, msg: &Message, ask: ToServer) -> anyhow::Result<Ours> {
        let req = self.next();
        let ask = match ask {
            ToServer::CastRemote { session, .. } => ToServer::CastRemote { req, session },
            ToServer::CameraRemote { .. } => ToServer::CameraRemote { req },
            other => other,
        };
        let shim = self.clone();
        let msg = msg.clone();
        self.ask(req, &ask, move |answer, mut fds| {
            let hdr = msg.header();
            let reply = match (answer, fds.pop()) {
                (ToShim::Remote { .. }, Some(fd)) => Message::method_return(&hdr).and_then(|b| b.build(&zbus::zvariant::Fd::from(fd))).map_err(Into::into),
                (ToShim::Failed { reason, .. }, _) => failed(&hdr, reason),
                (other, _) => failed(&hdr, format!("no remote: {other:?}")),
            };
            if let Err(err) = reply.and_then(|r| shim.bus.send(&r).map_err(Into::into)) {
                drv_os::say!("drv-bridge: remote: {err:#}");
            }
        })?;
        Ok(Ours::Done)
    }

    /// `org.freedesktop.portal.Camera`: the person is asked once per run of the app, and
    /// the remote sees every camera, for as long as the consent stands.
    fn camera(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        match member {
            "AccessCamera" => {
                let caller = caller_of(hdr)?;
                let (options,): (HashMap<String, OwnedValue>,) = msg.body().deserialize()?;
                let handle = handle_for(msg, &caller, &options, "handle_token");
                // The reply first, then the signal: the spec's order.
                self.bus.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
                if self.state.lock().unwrap().camera {
                    self.respond(&handle, &caller, 0, HashMap::new())?;
                    return Ok(Ours::Done);
                }
                let req = self.next();
                let shim = self.clone();
                self.ask(req, &ToServer::Camera { req }, move |answer, _| {
                    let code = match answer {
                        ToShim::Granted { .. } => {
                            shim.state.lock().unwrap().camera = true;
                            0
                        }
                        ToShim::Cancelled { .. } => 1,
                        other => {
                            drv_os::say!("drv-bridge: camera: {other:?}");
                            2
                        }
                    };
                    if let Err(err) = shim.respond(&handle, &caller, code, HashMap::new()) {
                        drv_os::say!("drv-bridge: camera response: {err}");
                    }
                })?;
                Ok(Ours::Done)
            }
            "OpenPipeWireRemote" => {
                anyhow::ensure!(self.state.lock().unwrap().camera, "the camera was not allowed");
                self.remote(msg, ToServer::CameraRemote { req: 0 })
            }
            other => bail!("no {other} on {CAMERA}"),
        }
    }

    /// `OpenURI`: the server has the manifest handler started; no prompt. `writable`, `ask`
    /// and the parent window are ignored.
    fn open_uri(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        if member != "OpenURI" {
            bail!("no {member} on {OPEN_URI}");
        }
        let caller = caller_of(hdr)?;
        let (_parent, uri, options): (String, String, HashMap<String, OwnedValue>) =
            msg.body().deserialize().context("OpenURI arguments")?;
        let handle = handle_for(msg, &caller, &options, "handle_token");
        self.bus.send(&Message::method_return(hdr)?.build(&ObjectPath::try_from(handle.as_str())?)?)?;
        let req = self.next();
        let shim = self.clone();
        self.ask(req, &ToServer::Open { req, uri }, move |answer, _| {
            let code = match answer {
                ToShim::Done { .. } => 0,
                other => {
                    drv_os::say!("drv-bridge: OpenURI: {other:?}");
                    2
                }
            };
            if let Err(err) = shim.respond(&handle, &caller, code, HashMap::new()) {
                drv_os::say!("drv-bridge: OpenURI response: {err}");
            }
        })?;
        Ok(Ours::Done)
    }

    fn notification(self: &Arc<Self>, msg: &Message, hdr: &Header<'_>, member: &str) -> anyhow::Result<Ours> {
        Ok(Ours::Reply(match member {
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
                let req = self.next();
                let shim = self.clone();
                let msg = msg.clone();
                self.ask(req, &ToServer::Notify { req, replaces, summary, body }, move |answer, _| {
                    let hdr = msg.header();
                    let reply = match answer {
                        ToShim::Notified { id, .. } => Message::method_return(&hdr).and_then(|b| b.build(&id)).map_err(Into::into),
                        ToShim::Failed { reason, .. } => failed(&hdr, reason),
                        other => failed(&hdr, format!("{other:?}")),
                    };
                    if let Err(err) = reply.and_then(|r| shim.bus.send(&r).map_err(Into::into)) {
                        drv_os::say!("drv-bridge: notification reply: {err:#}");
                    }
                })?;
                return Ok(Ours::Done);
            }
            other => error(hdr, UNKNOWN_METHOD, format!("no {other} on {NOTIFICATIONS_NAME}"))?,
        }))
    }
}

/// There is no other portal behind the bridge.
fn unknown(hdr: &Header<'_>, interface: &str, member: &str) -> anyhow::Result<Ours> {
    error(hdr, UNKNOWN_METHOD, format!("the bridge does not carry {interface}.{member}")).map(Ours::Reply)
}
