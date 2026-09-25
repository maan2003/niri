//! Screen sharing, cameras and microphones, a supervisor service. Apps connect to fd
//! `listener` (`drv_cast::wire`), each keyed on its uid and drv-appd's word for it. The
//! person is asked at the shell (fd `shell`, `drv_shell::ask`): which screen, whether the
//! camera or the microphone. A consented cast is started at the compositor (fd
//! `compositor`, `drv_cast::compositor`), which names the PipeWire node; the app gets a
//! PipeWire connection of ours that sees that node and nothing else (`pw`). Microphone and
//! camera grants are written where WirePlumber enforces them, keyed on the uid.
//!
//! One thread owns every bit of state and hears everything as an [`Event`]: the apps'
//! connections, the compositor, the shell and PipeWire each have a reader that only
//! forwards. Nothing here parses D-Bus; the shim in the app does that.

mod pw;

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use drv_cast::compositor::{self, FromCompositor, Output, ToCompositor, Window};
use drv_cast::wire::{self, Cursor, FromCast, Source, ToCast};
use drv_os::fds::Kind;
use drv_policy::door::Door;
use drv_policy::seq;
use drv_shell::ask::{self, Choice, Request, Response};

use pw::Pw;

/// A device an app may be let use: the microphone stands for any audio capture (sink
/// monitors included), the camera for any video source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    Microphone,
    Camera,
}

fn device_name(device: Device) -> &'static str {
    match device {
        Device::Microphone => "microphone",
        Device::Camera => "camera",
    }
}

/// Everything that happens, to the one thread that holds the state.
pub enum Event {
    AppNew { conn: u64, out: OwnedFd, app: String, uid: u32 },
    App { conn: u64, msg: ToCast },
    AppGone { conn: u64 },
    Compositor(FromCompositor),
    Shell(Response),
    /// WirePlumber has a stream of `uid` waiting on this.
    PwAsked { uid: u32, device: Device },
    /// The last PipeWire connection of `uid` is gone.
    PwClientsGone { uid: u32 },
    /// A remote made for an app on its own thread.
    Remote { conn: u64, req: u64, for_what: RemoteFor, result: anyhow::Result<(OwnedFd, u32)> },
}

pub enum RemoteFor {
    Session(u64),
    Camera,
}

struct Conn {
    out: OwnedFd,
    app: String,
    uid: u32,
    /// The app's sessions, by its numbers for them.
    sessions: HashMap<u64, Session>,
}

struct Session {
    /// The `Cast` request, answered when the stream is up.
    req: u64,
    cursor: Cursor,
    /// The shell's question, while the person has not picked.
    ask: Option<u64>,
    /// Our id for the cast at the compositor, once consented.
    cast: Option<u64>,
    node: Option<u32>,
    /// PipeWire clients of remotes handed out for it.
    remotes: Vec<u32>,
}

/// A cast the person consented to, from `Start` until the compositor says `Stopped`.
struct Live {
    conn: u64,
    session: u64,
    app: String,
    uid: u32,
    source: Source,
    /// For the log: the screen's name, or the window's app and title.
    label: String,
    token: String,
}

/// A source the person let an app share, good for that app until it is gone.
struct Consent {
    app: String,
    uid: u32,
    source: Source,
}

/// A question up at the shell.
enum Asking {
    Pick { conn: u64, session: u64, screens: bool, windows: bool },
    Device { uid: u32, device: Device, waiting: Vec<(u64, u64)> },
}

struct Cast {
    compositor: OwnedFd,
    shell: OwnedFd,
    pw: Pw,
    door: Arc<Door>,
    events: mpsc::Sender<Event>,
    conns: HashMap<u64, Conn>,
    /// What the compositor last said it has; asked again with every cast request.
    outputs: Vec<Output>,
    windows: Vec<Window>,
    /// Ids for casts at the compositor and questions at the shell.
    next_id: u64,
    asks: HashMap<u64, Asking>,
    casts: HashMap<u64, Live>,
    consents: HashMap<String, Consent>,
    /// Devices the person allowed, by uid: the app's name and the devices.
    devices: HashMap<u32, (String, Vec<Device>)>,
    /// Camera remotes handed out, by uid: their PipeWire clients.
    camera_remotes: HashMap<u32, Vec<u32>>,
    next_token: u64,
}

impl Cast {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn tell(&self, msg: ToCompositor) {
        if let Err(err) = seq::send(&self.compositor, &msg, &[]) {
            drv_os::say!("drv-cast: to the compositor: {err}");
        }
    }

    fn ask(&self, req: Request) {
        if let Err(err) = seq::send(&self.shell, &req, &[]) {
            drv_os::say!("drv-cast: to the shell: {err}");
        }
    }

    fn send(&self, conn: u64, msg: FromCast, fds: &[std::os::fd::BorrowedFd<'_>]) {
        let Some(c) = self.conns.get(&conn) else { return };
        if let Err(err) = seq::send(&c.out, &msg, fds) {
            drv_os::say!("drv-cast: to {}: {err}", c.app);
        }
    }

    fn fail(&self, conn: u64, req: u64, reason: impl std::fmt::Display) {
        let reason = format!("{reason:#}");
        if let Some(c) = self.conns.get(&conn) {
            drv_os::say!("drv-cast: {}: request {req}: {reason}", c.app);
        }
        self.send(conn, FromCast::Failed { req, reason }, &[]);
    }

    fn on(&mut self, ev: Event) {
        match ev {
            Event::AppNew { conn, out, app, uid } => {
                self.conns.insert(conn, Conn { out, app, uid, sessions: HashMap::new() });
            }
            Event::App { conn, msg } => self.on_app(conn, msg),
            Event::AppGone { conn } => self.gone(conn),
            Event::Compositor(msg) => self.on_compositor(msg),
            Event::Shell(resp) => self.on_shell(resp),
            Event::PwAsked { uid, device } => match self.door.who(uid) {
                Ok(policy) if device == Device::Camera && policy.camera => self.grant_device(uid, device, Vec::new()),
                Ok(policy) => self.ask_device(policy.name.clone(), uid, device, None),
                Err(err) => {
                    drv_os::say!("drv-cast: uid {uid} asks for the {}: {err}", device_name(device));
                    self.pw.answered(uid, device);
                }
            },
            Event::PwClientsGone { uid } => self.forget_devices(uid),
            Event::Remote { conn, req, for_what, result } => match result {
                Ok((fd, client)) => {
                    let Some(c) = self.conns.get_mut(&conn) else {
                        // Gone meanwhile: the connection is cut with the fd.
                        return self.pw.drop_client(client);
                    };
                    match for_what {
                        RemoteFor::Session(session) => match c.sessions.get_mut(&session) {
                            Some(s) => s.remotes.push(client),
                            None => return self.pw.drop_client(client),
                        },
                        RemoteFor::Camera => self.camera_remotes.entry(c.uid).or_default().push(client),
                    }
                    self.send(conn, FromCast::Remote { req }, &[fd.as_fd()]);
                }
                Err(err) => self.fail(conn, req, err),
            },
        }
    }

    // ------------------------------------------------------------ apps

    fn on_app(&mut self, conn: u64, msg: ToCast) {
        let Some(c) = self.conns.get(&conn) else { return };
        let (app, uid) = (c.app.clone(), c.uid);
        match msg {
            ToCast::Hello { .. } => {}
            ToCast::Cast { req, session, cursor, screens, windows, again } => {
                if c.sessions.contains_key(&session) {
                    return self.fail(conn, req, "the session was started already");
                }
                self.tell(ToCompositor::Outputs);
                self.tell(ToCompositor::Windows);
                let mut s = Session { req, cursor, ask: None, cast: None, node: None, remotes: Vec::new() };
                if let Some(token) = again {
                    let standing = self.consents.get(&token).filter(|k| k.app == app && k.uid == uid);
                    if let Some((source, label)) = standing.and_then(|k| self.label(&k.source).map(|l| (k.source.clone(), l))) {
                        drv_os::say!("drv-cast: {app} (uid {uid}) shares {label} again");
                        let cast = self.id();
                        s.cast = Some(cast);
                        self.casts.insert(cast, Live { conn, session, app, uid, source: source.clone(), label, token });
                        self.tell(ToCompositor::Start { cast, source, cursor });
                        self.conns.get_mut(&conn).unwrap().sessions.insert(session, s);
                        return;
                    }
                }
                let id = self.id();
                s.ask = Some(id);
                self.conns.get_mut(&conn).unwrap().sessions.insert(session, s);
                self.asks.insert(id, Asking::Pick { conn, session, screens, windows });
                self.ask(self.pick(id, &app, uid, screens, windows));
            }
            ToCast::CastRemote { req, session } => {
                let node = c.sessions.get(&session).and_then(|s| s.node);
                let Some(node) = node else {
                    return self.fail(conn, req, "the session is not streaming");
                };
                self.remote(conn, req, RemoteFor::Session(session), vec![node], format!("node:{node}"), app);
            }
            ToCast::CastClose { session } => self.close_session(conn, session, false),
            ToCast::Camera { req } => {
                if self.has(uid, Device::Camera) {
                    return self.send(conn, FromCast::Granted { req }, &[]);
                }
                if self.standing(uid, Device::Camera) {
                    self.grant_device(uid, Device::Camera, vec![(conn, req)]);
                    return;
                }
                self.ask_device(app, uid, Device::Camera, Some((conn, req)));
            }
            ToCast::CameraRemote { req } => {
                if !self.has(uid, Device::Camera) {
                    return self.fail(conn, req, "the camera was not allowed");
                }
                let cameras = self.pw.cameras();
                self.remote(conn, req, RemoteFor::Camera, cameras, "camera".to_owned(), app);
            }
            ToCast::CameraPresent { req } => {
                let present = !self.pw.cameras().is_empty();
                self.send(conn, FromCast::Present { req, present }, &[]);
            }
            ToCast::Cancel { req } => {
                // A cast waiting on the person, or a camera question.
                let session = c.sessions.iter().find(|(_, s)| s.req == req && s.ask.is_some()).map(|(k, _)| *k);
                if let Some(session) = session {
                    return self.close_session(conn, session, false);
                }
                for asking in self.asks.values_mut() {
                    if let Asking::Device { waiting, .. } = asking {
                        waiting.retain(|w| *w != (conn, req));
                    }
                }
            }
        }
    }

    /// A remote for the app, made on its own thread (PipeWire round trips), back as an event.
    fn remote(&self, conn: u64, req: u64, for_what: RemoteFor, nodes: Vec<u32>, mark: String, app: String) {
        let (pw, events) = (self.pw.clone(), self.events.clone());
        thread::spawn(move || {
            let result = pw::remote_twice(&nodes, &app).and_then(|(fd, client)| {
                pw.mark(client, &mark)?;
                Ok((fd, client))
            });
            let _ = events.send(Event::Remote { conn, req, for_what, result });
        });
    }

    /// The session ends on our side: its question comes down, its cast stops, its remotes
    /// are cut. `tell_app`: it hears `CastClosed` (the compositor or the person ended it).
    fn close_session(&mut self, conn: u64, session: u64, tell_app: bool) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let Some(s) = c.sessions.remove(&session) else { return };
        if let Some(id) = s.ask {
            self.asks.remove(&id);
            self.ask(Request::Cancel { id });
        }
        if let Some(cast) = s.cast {
            if let Some(live) = self.casts.remove(&cast) {
                drv_os::say!("drv-cast: {} (uid {}) closed its cast of {}", live.app, live.uid, live.label);
                self.tell(ToCompositor::Stop { cast });
            }
        }
        for client in s.remotes {
            self.pw.drop_client(client);
        }
        if tell_app {
            self.send(conn, FromCast::CastClosed { session }, &[]);
        }
    }

    /// The app disconnected: its sessions end, its questions come down, and if it was the
    /// last connection of its uid, what the person allowed it ends too.
    fn gone(&mut self, conn: u64) {
        let Some(c) = self.conns.get(&conn) else { return };
        let (app, uid) = (c.app.clone(), c.uid);
        drv_os::say!("drv-cast: {app} disconnected");
        let sessions: Vec<u64> = c.sessions.keys().copied().collect();
        for session in sessions {
            self.close_session(conn, session, false);
        }
        self.conns.remove(&conn);
        for asking in self.asks.values_mut() {
            if let Asking::Device { waiting, .. } = asking {
                waiting.retain(|(k, _)| *k != conn);
            }
        }
        if !self.conns.values().any(|c| c.uid == uid) {
            self.consents.retain(|_, k| k.uid != uid);
            self.forget_devices(uid);
        }
    }

    // ------------------------------------------------------------ casts

    /// How a source reads in the log, if the compositor still has it.
    fn label(&self, source: &Source) -> Option<String> {
        match source {
            Source::Screen(name) => self
                .outputs
                .iter()
                .find(|o| o.name == *name && o.width != 0 && o.height != 0)
                .map(|o| o.name.clone()),
            Source::Window(id) => self
                .windows
                .iter()
                .find(|w| w.id == *id)
                .map(|w| format!("{}'s window {:?}", w.app, w.title)),
        }
    }

    /// The question for a cast: the screens and windows there are.
    fn pick(&self, id: u64, app: &str, uid: u32, screens: bool, windows: bool) -> Request {
        let mut choices = Vec::new();
        if screens {
            for o in self.outputs.iter().filter(|o| o.width != 0 && o.height != 0) {
                choices.push(Choice {
                    key: format!("screen:{}", o.name),
                    name: o.name.clone(),
                    detail: format!("{} {}  {}x{}", o.make, o.model, o.width, o.height),
                });
            }
        }
        if windows {
            for w in &self.windows {
                // The app's name is ours; the title is the window's own word.
                choices.push(Choice { key: format!("window:{}", w.id), name: w.app.clone(), detail: w.title.clone() });
            }
        }
        Request::Pick {
            id,
            app: app.to_owned(),
            uid,
            what: "see your screen".to_owned(),
            note: "what it may see, until you stop it (Mod+Shift+Esc stops them all)".to_owned(),
            choices,
        }
    }

    fn parse_key(key: &str) -> Option<Source> {
        if let Some(name) = key.strip_prefix("screen:") {
            return Some(Source::Screen(name.to_owned()));
        }
        key.strip_prefix("window:")?.parse().ok().map(Source::Window)
    }

    /// The person picked a source: the compositor starts the cast; the app hears when the
    /// node exists.
    fn start_cast(&mut self, conn: u64, session: u64, source: Source) {
        let label = self.label(&source);
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let (app, uid) = (c.app.clone(), c.uid);
        let Some(s) = c.sessions.get_mut(&session) else { return };
        let req = s.req;
        s.ask = None;
        let Some(label) = label else {
            c.sessions.remove(&session);
            return self.fail(conn, req, "that source is gone");
        };
        drv_os::say!("drv-cast: {app} (uid {uid}) may share {label}");
        // Tokens only mean something with the app they were given to, so plain counting does.
        self.next_token += 1;
        let token = format!("drv{}", self.next_token);
        let cast = self.id();
        let c = self.conns.get_mut(&conn).unwrap();
        let s = c.sessions.get_mut(&session).unwrap();
        s.cast = Some(cast);
        let cursor = s.cursor;
        self.consents.insert(token.clone(), Consent { app: app.clone(), uid, source: source.clone() });
        self.casts.insert(cast, Live { conn, session, app, uid, source: source.clone(), label, token });
        self.tell(ToCompositor::Start { cast, source, cursor });
    }

    fn on_compositor(&mut self, ev: FromCompositor) {
        match ev {
            FromCompositor::Hello { version } => {
                if version != compositor::VERSION {
                    drv_os::say!("drv-cast: the compositor speaks version {version}, we speak {}", compositor::VERSION);
                }
            }
            FromCompositor::Outputs(outputs) => {
                self.outputs = outputs;
                self.repick();
            }
            FromCompositor::Windows(windows) => {
                self.windows = windows;
                self.repick();
            }
            FromCompositor::Started { cast, node_id, width, height } => match self.casts.get(&cast) {
                Some(live) => {
                    drv_os::say!("drv-cast: {} (uid {}) shares {} on PipeWire node {node_id}", live.app, live.uid, live.label);
                    let (conn, session, source, token) = (live.conn, live.session, live.source.clone(), live.token.clone());
                    let req = match self.conns.get_mut(&conn).and_then(|c| c.sessions.get_mut(&session)) {
                        Some(s) => {
                            s.node = Some(node_id);
                            s.req
                        }
                        None => return self.tell(ToCompositor::Stop { cast }),
                    };
                    self.send(conn, FromCast::Cast { req, node_id, source, width, height, token }, &[]);
                }
                // Cancelled in between: the compositor started it for nobody.
                None => self.tell(ToCompositor::Stop { cast }),
            },
            FromCompositor::Stopped { cast } => {
                if let Some(live) = self.casts.remove(&cast) {
                    drv_os::say!("drv-cast: the cast of {} for {} ended", live.label, live.app);
                    self.close_session(live.conn, live.session, true);
                }
            }
            FromCompositor::Revoke => {
                let uids: Vec<u32> = self.devices.keys().copied().collect();
                for uid in uids {
                    if let Some((app, devices)) = self.devices.remove(&uid) {
                        for device in devices {
                            drv_os::say!("drv-cast: {app} (uid {uid}) loses the {}", device_name(device));
                        }
                    }
                    self.pw.write(uid, std::iter::empty());
                    for client in self.camera_remotes.remove(&uid).unwrap_or_default() {
                        self.pw.drop_client(client);
                    }
                }
                self.show_devices();
            }
        }
    }

    /// The compositor's lists changed: every cast question shows the new ones.
    fn repick(&mut self) {
        let picks: Vec<(u64, u64, u64, bool, bool)> = self
            .asks
            .iter()
            .filter_map(|(id, a)| match a {
                Asking::Pick { conn, session, screens, windows } => Some((*id, *conn, *session, *screens, *windows)),
                _ => None,
            })
            .collect();
        for (id, conn, _, screens, windows) in picks {
            if let Some(c) = self.conns.get(&conn) {
                self.ask(self.pick(id, &c.app, c.uid, screens, windows));
            }
        }
    }

    fn on_shell(&mut self, resp: Response) {
        let id = match &resp {
            Response::Hello { version } => {
                if *version != ask::VERSION {
                    drv_os::say!("drv-cast: the shell speaks version {version}, we speak {}", ask::VERSION);
                }
                return;
            }
            Response::Yes { id } | Response::Secret { id, .. } | Response::Picked { id, .. } | Response::Cancelled { id } => *id,
        };
        let Some(asking) = self.asks.remove(&id) else {
            drv_os::say!("drv-cast: the shell answered {id}, which nobody asked");
            return;
        };
        match (asking, resp) {
            (Asking::Pick { conn, session, .. }, Response::Picked { key, .. }) => match Self::parse_key(&key) {
                Some(source) => self.start_cast(conn, session, source),
                None => {
                    let req = self.conns.get_mut(&conn).and_then(|c| c.sessions.remove(&session)).map(|s| s.req);
                    if let Some(req) = req {
                        self.fail(conn, req, format!("the shell picked {key:?}"));
                    }
                }
            },
            (Asking::Pick { conn, session, .. }, _) => {
                let req = self.conns.get_mut(&conn).and_then(|c| c.sessions.remove(&session)).map(|s| s.req);
                if let Some(req) = req {
                    self.send(conn, FromCast::Cancelled { req }, &[]);
                }
            }
            (Asking::Device { uid, device, waiting }, Response::Yes { .. }) => self.grant_device(uid, device, waiting),
            (Asking::Device { uid, device, waiting }, _) => {
                drv_os::say!("drv-cast: uid {uid} may not use the {}", device_name(device));
                self.pw.answered(uid, device);
                for (conn, req) in waiting {
                    self.send(conn, FromCast::Cancelled { req }, &[]);
                }
            }
        }
    }

    // ------------------------------------------------------------ devices

    fn has(&self, uid: u32, device: Device) -> bool {
        self.devices.get(&uid).is_some_and(|(_, d)| d.contains(&device))
    }

    /// The manifest grants the device for good (`camera`): no question, the grant is
    /// written on the first request of the run, like an answer of yes.
    fn standing(&self, uid: u32, device: Device) -> bool {
        device == Device::Camera && self.door.who(uid).is_ok_and(|p| p.camera)
    }

    /// The person said yes (or the manifest did): the grant is written where WirePlumber
    /// reads it, shown, and everyone waiting hears `Granted`.
    fn grant_device(&mut self, uid: u32, device: Device, waiting: Vec<(u64, u64)>) {
        let app = self.conns.values().find(|c| c.uid == uid).map(|c| c.app.clone());
        let app = app.or_else(|| self.door.who(uid).ok().map(|p| p.name.clone())).unwrap_or_else(|| format!("uid {uid}"));
        drv_os::say!("drv-cast: {app} (uid {uid}) may use the {}", device_name(device));
        let entry = self.devices.entry(uid).or_insert_with(|| (app, Vec::new()));
        if !entry.1.contains(&device) {
            entry.1.push(device);
        }
        self.pw.write(uid, entry.1.iter().copied());
        self.pw.answered(uid, device);
        self.show_devices();
        for (conn, req) in waiting {
            self.send(conn, FromCast::Granted { req }, &[]);
        }
    }

    /// One question per uid and device at a time; `waiter` hears the answer.
    fn ask_device(&mut self, app: String, uid: u32, device: Device, waiter: Option<(u64, u64)>) {
        if self.has(uid, device) {
            // Told again, for a request that came before the grant was written.
            self.pw.answered(uid, device);
            if let Some((conn, req)) = waiter {
                self.send(conn, FromCast::Granted { req }, &[]);
            }
            return;
        }
        let up = self.asks.iter_mut().find_map(|(_, a)| match a {
            Asking::Device { uid: u, device: d, waiting } if *u == uid && *d == device => Some(waiting),
            _ => None,
        });
        if let Some(waiting) = up {
            waiting.extend(waiter);
            return;
        }
        let id = self.id();
        self.asks.insert(id, Asking::Device { uid, device, waiting: waiter.into_iter().collect() });
        self.ask(Request::Confirm {
            id,
            app,
            uid,
            what: format!("use your {}", device_name(device)),
            note: "until it exits or you revoke it (Mod+Shift+Esc revokes everything)".to_owned(),
        });
    }

    /// The uid is gone (its last connection, its last PipeWire client): its grants end, its
    /// questions come down, its camera remotes are cut.
    fn forget_devices(&mut self, uid: u32) {
        if let Some((app, devices)) = self.devices.remove(&uid) {
            for device in devices {
                drv_os::say!("drv-cast: {app} (uid {uid}) is done with the {}", device_name(device));
            }
            self.show_devices();
        }
        let asks: Vec<u64> = self
            .asks
            .iter()
            .filter(|(_, a)| matches!(a, Asking::Device { uid: u, .. } if *u == uid))
            .map(|(id, _)| *id)
            .collect();
        for id in asks {
            self.asks.remove(&id);
            self.ask(Request::Cancel { id });
        }
        for client in self.camera_remotes.remove(&uid).unwrap_or_default() {
            self.pw.drop_client(client);
        }
        self.pw.forget(uid);
    }

    /// The compositor's indicator follows `devices`: who holds the microphone, who the camera.
    fn show_devices(&self) {
        let holders = |device: Device| {
            let mut names: Vec<String> = self
                .devices
                .values()
                .filter(|(_, d)| d.contains(&device))
                .map(|(app, _)| app.clone())
                .collect();
            names.sort();
            names.dedup();
            names
        };
        self.tell(ToCompositor::Devices { mic: holders(Device::Microphone), camera: holders(Device::Camera) });
    }
}

// ---------------------------------------------------------------- readers

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// One app's connection, on its own thread: the hello, then its messages to the state.
fn conn(tx: mpsc::Sender<Event>, sock: OwnedFd, uid: u32, app: String) {
    match seq::recv::<ToCast>(&sock) {
        Ok((ToCast::Hello { version }, _)) if version == wire::VERSION => {}
        Ok((other, _)) => {
            drv_os::say!("drv-cast: {app} opened with {other:?}, not version {}", wire::VERSION);
            return;
        }
        Err(err) => {
            drv_os::say!("drv-cast: {app}: {err}");
            return;
        }
    }
    if let Err(err) = seq::send(&sock, &FromCast::Hello { version: wire::VERSION }, &[]) {
        drv_os::say!("drv-cast: to {app}: {err}");
        return;
    }
    let out = match sock.try_clone() {
        Ok(out) => out,
        Err(err) => {
            drv_os::say!("drv-cast: dup: {err}");
            return;
        }
    };
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    if tx.send(Event::AppNew { conn, out, app: app.clone(), uid }).is_err() {
        return;
    }
    loop {
        match seq::recv::<ToCast>(&sock) {
            Ok((msg, _)) => {
                if tx.send(Event::App { conn, msg }).is_err() {
                    break;
                }
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    drv_os::say!("drv-cast: {app}: {err}");
                }
                break;
            }
        }
    }
    let _ = tx.send(Event::AppGone { conn });
}

/// A supervisor link: everything on it becomes an event; losing it ends us.
fn reader<T: serde::de::DeserializeOwned + Send + 'static>(
    sock: OwnedFd,
    who: &'static str,
    tx: mpsc::Sender<Event>,
    wrap: impl Fn(T) -> Event + Send + 'static,
) {
    thread::spawn(move || {
        loop {
            match seq::recv::<T>(&sock) {
                Ok((msg, _)) => {
                    if tx.send(wrap(msg)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    drv_os::say!("drv-cast: {who}: {err}");
                    break;
                }
            }
        }
        process::exit(1);
    });
}

fn run() -> Result<(), String> {
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let compositor = fds.socket("compositor", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let shell = fds.socket("shell", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let listener = fds.listener_of("listener", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let door = Arc::new(Door::open().map_err(|e| format!("drv-appd: {e}"))?);
    let (tx, rx) = mpsc::channel::<Event>();
    let pw = Pw::start(tx.clone()).map_err(|e| format!("{e:#}"))?;

    let compositor_out = compositor.try_clone().map_err(|e| format!("dup: {e}"))?;
    let shell_out = shell.try_clone().map_err(|e| format!("dup: {e}"))?;
    seq::send(&compositor_out, &ToCompositor::Hello { version: compositor::VERSION }, &[])
        .map_err(|e| format!("hello to the compositor: {e}"))?;
    seq::send(&shell_out, &Request::Hello { version: ask::VERSION }, &[])
        .map_err(|e| format!("hello to the shell: {e}"))?;
    reader(compositor, "the compositor", tx.clone(), Event::Compositor);
    reader(shell, "the shell", tx.clone(), Event::Shell);
    {
        let (door, tx) = (door.clone(), tx.clone());
        thread::spawn(move || {
            let err = door.serve(listener, "drv-cast", move |sock, uid, policy| {
                conn(tx.clone(), sock, uid, policy.name.clone())
            });
            drv_os::say!("drv-cast: the socket: {err:?}");
            process::exit(1);
        });
    }

    let mut cast = Cast {
        compositor: compositor_out,
        shell: shell_out,
        pw,
        door,
        events: tx,
        conns: HashMap::new(),
        outputs: Vec::new(),
        windows: Vec::new(),
        next_id: 0,
        asks: HashMap::new(),
        casts: HashMap::new(),
        consents: HashMap::new(),
        devices: HashMap::new(),
        camera_remotes: HashMap::new(),
        next_token: 0,
    };
    drv_os::say!("drv-cast: serving");
    for ev in rx {
        cast.on(ev);
    }
    Err("every sender is gone".to_owned())
}

fn main() {
    if let Err(err) = run() {
        drv_os::say!("drv-cast: {err}");
        process::exit(1);
    }
}
