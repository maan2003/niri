//! The shell, a supervisor service: everything the desktop draws that is not an app's
//! window, on one Wayland connection (fd `wayland`, a layer-shell and session-lock client).
//!
//! The lock screen: draws and takes the PIN, drv-authd (fd `auth`) verifies it and tells the
//! compositor; the compositor alone decides when the session is locked, and a dead shell
//! leaves it locked and black. The app menu: the compositor pokes fd `poke` on
//! `show-launcher`, the pick goes down the launch channel (fd `appd`) as a name. The
//! person's prompts: drv-cast (fd `cast`) and drv-agent (fd `agent`) ask over
//! `drv_shell::ask`; the shell shows what it is told, one dialog at a time, none while
//! locked. Notifications: apps connect to fd `listener` (`drv_shell::notify`), each keyed
//! on its uid and drv-appd's word for it; the text is shown under the manifest name.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use drv_os::fds::Kind;
use drv_policy::door::Door;
use drv_policy::seq;
use drv_policy::{PolicyClient, clip};
use drv_shell::ask::{self, Choice, Request, Response};
use drv_shell::notify::{self, FromShell, ToShell};
use drv_ui::sctk::reexports::calloop::channel::{self, Sender};
use drv_ui::sctk::reexports::calloop::generic::Generic;
use drv_ui::sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use drv_ui::sctk::reexports::calloop::{Interest, Mode, PostAction};
use drv_ui::sctk::seat::keyboard::{KeyEvent, Keysym};
use drv_ui::sctk::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use drv_ui::sctk::shell::WaylandSurface;
use drv_ui::sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use drv_ui::wayland_client::protocol::{wl_output, wl_surface};
use drv_ui::wayland_client::{Connection, QueueHandle};
use drv_ui::{Align, Client, Painter, Ui};
use zeroize::Zeroize;

const MAX_PIN: usize = 32;
const MAX_TYPED: usize = 200;
const MENU_WIDTH: u32 = 520;
const MENU_HEIGHT: u32 = 420;
const DIALOG_WIDTH: u32 = 640;
const DIALOG_HEIGHT: u32 = 480;
/// A yes or no, a secret, a touch: a line or two, no list.
const DIALOG_SMALL: u32 = 180;
const NOTE_WIDTH: u32 = 400;
const NOTE_HEIGHT: u32 = 84;
const NOTE_MARGIN: i32 = 12;
const NOTES_SHOWN: usize = 5;
const NOTE_TTL: Duration = Duration::from_secs(8);
const ROW: f64 = 28.;
const PAD: f64 = 16.;

const FG: (f64, f64, f64, f64) = (0.93, 0.93, 0.95, 1.);
const DIM: (f64, f64, f64, f64) = (0.6, 0.6, 0.65, 1.);
const BLUE: (f64, f64, f64, f64) = (0.55, 0.75, 1., 1.);
const RED: (f64, f64, f64, f64) = (1., 0.6, 0.5, 1.);
const SELECTED: (f64, f64, f64, f64) = (0.25, 0.35, 0.55, 1.);

// ---------------------------------------------------------------- the lock screen

struct Lock {
    state: SessionLockState,
    lock: Option<SessionLock>,
    surfaces: HashMap<wl_output::WlOutput, LockSurface>,
    pin: Vec<u8>,
    message: String,
    /// Waiting for the compositor to finish us after a correct PIN.
    granted: bool,
    granted_ticks: u32,
    /// Our connection to drv-authd.
    auth: OwnedFd,
}

struct LockSurface {
    surface: SessionLockSurface,
    size: (u32, u32),
}

// ---------------------------------------------------------------- the menu

struct Menu {
    /// Up while this is; dropping it takes the surface down.
    layer: Option<LayerSurface>,
    size: (u32, u32),
    appd: PolicyClient,
    apps: Vec<String>,
    filter: String,
    selected: usize,
}

impl Menu {
    fn matches(&self) -> Vec<String> {
        let filter = self.filter.to_lowercase();
        self.apps
            .iter()
            .filter(|a| a.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------- the prompts

/// Who asked, and gets the answer: ids are theirs, so they only mean something per wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wire {
    Cast,
    Agent,
}

impl Wire {
    fn name(self) -> &'static str {
        match self {
            Wire::Cast => "drv-cast",
            Wire::Agent => "drv-agent",
        }
    }
}

struct Pending {
    wire: Wire,
    id: u64,
    app: String,
    uid: u32,
    what: String,
    kind: What,
}

enum What {
    Confirm { note: String },
    Secret { prompt: String },
    Touch { prompt: String },
    Pick { note: String, choices: Vec<Choice> },
}

struct Dialog {
    req: Pending,
    layer: LayerSurface,
    size: (u32, u32),
    /// Into the shown choices.
    selected: Option<usize>,
    /// The secret, or the filter over the choices.
    typed: String,
}

impl Dialog {
    fn small(&self) -> bool {
        !matches!(self.req.kind, What::Pick { .. })
    }

    /// Choices that pass the filter, as indices into the list.
    fn shown(&self) -> Vec<usize> {
        let What::Pick { choices, .. } = &self.req.kind else {
            return Vec::new();
        };
        let filter = self.typed.to_lowercase();
        choices
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                filter.is_empty()
                    || c.name.to_lowercase().contains(&filter)
                    || c.detail.to_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn reset_selection(&mut self) {
        self.selected = (!self.shown().is_empty()).then_some(0);
    }
}

// ---------------------------------------------------------------- notifications

/// From the threads that read the apps' connections.
enum Event {
    New { conn: u64, out: OwnedFd, app: String, uid: u32 },
    Notify { conn: u64, req: u64, replaces: u32, summary: String, body: String },
    Close { conn: u64, id: u32 },
    Gone { conn: u64 },
}

struct Conn {
    out: OwnedFd,
    app: String,
    uid: u32,
}

struct Note {
    id: u32,
    uid: u32,
    app: String,
    summary: String,
    body: String,
    until: Instant,
}

struct Notes {
    conns: HashMap<u64, Conn>,
    list: Vec<Note>,
    next_id: u32,
    layer: Option<LayerSurface>,
    size: (u32, u32),
}

// ---------------------------------------------------------------- the shell

struct App {
    ui: Ui,
    layer_shell: LayerShell,
    lock: Lock,
    menu: Menu,
    cast: OwnedFd,
    agent: OwnedFd,
    queue: VecDeque<Pending>,
    dialog: Option<Dialog>,
    notes: Notes,
}

impl App {
    /// Locked as far as we know: our lock surfaces are up. The compositor knows better, and
    /// shows nothing else while it is locked anyway.
    fn is_locked(&self) -> bool {
        !self.lock.surfaces.is_empty()
    }

    fn wire(&self, wire: Wire) -> &OwnedFd {
        match wire {
            Wire::Cast => &self.cast,
            Wire::Agent => &self.agent,
        }
    }

    fn reply(&self, wire: Wire, resp: Response) {
        if let Err(err) = seq::send(self.wire(wire), &resp, &[]) {
            drv_os::say!("drv-shell: to {}: {err}", wire.name());
        }
    }

    // ------------------------------------------------------------ lock

    fn lock_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(lock) = &self.lock.lock else {
            return;
        };
        if self.lock.surfaces.contains_key(&output) {
            return;
        }
        let surface = self.ui.compositor_state.create_surface(qh);
        let lock_surface = lock.create_lock_surface(surface, &output, qh);
        self.lock.surfaces.insert(
            output,
            LockSurface {
                surface: lock_surface,
                size: (0, 0),
            },
        );
    }

    fn draw_lock_all(&mut self) {
        let outputs: Vec<_> = self.lock.surfaces.keys().cloned().collect();
        for output in outputs {
            self.draw_lock(&output);
        }
    }

    fn draw_lock(&mut self, output: &wl_output::WlOutput) {
        let Some(surface) = self.lock.surfaces.get(output) else {
            return;
        };
        let (width, height) = surface.size;
        if width == 0 || height == 0 {
            return;
        }
        let prompt = if self.lock.granted {
            "Unlocking…".to_owned()
        } else if self.lock.pin.is_empty() {
            "Enter PIN".to_owned()
        } else {
            "●".repeat(self.lock.pin.len())
        };
        let message = self.lock.message.clone();
        let wl_surface = surface.surface.wl_surface().clone();
        if let Err(err) = self.ui.draw(&wl_surface, width, height, |p| {
            p.fill(0.08, 0.09, 0.12);
            let mid = p.height / 2.;
            p.text(0., mid - 120., 36., "Locked", Align::Center, (0.93, 0.93, 0.95, 0.7));
            p.text(0., mid - 30., 44., &prompt, Align::Center, FG);
            p.text(0., mid + 50., 22., &message, Align::Center, RED);
        }) {
            drv_os::say!("drv-shell: lock: {err}");
        }
    }

    /// Asks to lock; the compositor answers `locked` when the session is (or gets) locked.
    fn relock(&mut self, qh: &QueueHandle<Self>) {
        match self.lock.state.lock(qh) {
            Ok(lock) => self.lock.lock = Some(lock),
            Err(err) => {
                drv_os::say!("drv-shell: no session-lock global: {err}");
                process::exit(1);
            }
        }
    }

    fn forget_pin(&mut self) {
        self.lock.pin.zeroize();
        self.lock.pin.clear();
    }

    fn submit_pin(&mut self) {
        if self.lock.pin.is_empty() || self.lock.granted {
            return;
        }
        let reply = drv_auth::verify(&self.lock.auth, &self.lock.pin);
        self.forget_pin();
        match reply {
            Ok(drv_auth::Response::Granted) => {
                self.lock.granted = true;
                self.lock.message.clear();
            }
            Ok(drv_auth::Response::Denied { retry_after_ms }) => {
                self.lock.message = if retry_after_ms == 0 {
                    "Wrong PIN".to_owned()
                } else {
                    format!("Wrong PIN, try again in {} s", retry_after_ms.div_ceil(1000))
                };
            }
            Ok(other) => self.lock.message = format!("Auth failed: {other:?}"),
            Err(err) => {
                // drv-authd is gone; so is the set, us included, in a moment.
                drv_os::say!("drv-shell: auth daemon unreachable: {err}; exiting");
                process::exit(1);
            }
        }
        self.draw_lock_all();
    }

    fn lock_key(&mut self, event: KeyEvent) {
        if self.lock.granted {
            return;
        }
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit_pin(),
            Keysym::BackSpace => {
                self.lock.pin.pop();
                self.draw_lock_all();
            }
            Keysym::Escape => {
                self.forget_pin();
                self.lock.message.clear();
                self.draw_lock_all();
            }
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control());
                    if printable && self.lock.pin.len() + s.len() <= MAX_PIN {
                        self.lock.pin.extend_from_slice(s.as_bytes());
                        self.lock.message.clear();
                        self.draw_lock_all();
                    }
                }
            }
        }
    }

    /// Locked: nothing else of ours stays up. The dialog goes back to the front of the
    /// queue; the notes stay listed and come back with the unlock.
    fn hide_all(&mut self) {
        if let Some(d) = self.dialog.take() {
            self.queue.push_front(d.req);
        }
        self.menu.layer = None;
        self.notes.layer = None;
    }

    // ------------------------------------------------------------ menu

    /// The compositor poked us: fresh names, fresh surface.
    fn show_menu(&mut self, qh: &QueueHandle<Self>) {
        if self.menu.layer.is_some() || self.is_locked() {
            return;
        }
        self.menu.apps = match self.menu.appd.apps() {
            Ok(apps) => apps,
            Err(err) => {
                drv_os::say!("drv-shell: asking drv-appd for the apps: {err}");
                return;
            }
        };
        self.menu.filter.clear();
        self.menu.selected = 0;
        self.menu.size = (0, 0);
        let surface = self.ui.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("drv-shell-menu"),
            None,
        );
        layer.set_size(MENU_WIDTH, MENU_HEIGHT);
        layer.set_anchor(Anchor::empty());
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        self.menu.layer = Some(layer);
    }

    fn choose_app(&mut self) {
        let name = self.menu.matches().get(self.menu.selected).cloned();
        self.menu.layer = None;
        let Some(name) = name else {
            return;
        };
        match self.menu.appd.launch(name.clone()) {
            Ok(uid) => drv_os::say!("drv-shell: launched {name:?} as uid {uid}"),
            Err(err) => drv_os::say!("drv-shell: launching {name:?}: {err}"),
        }
    }

    fn draw_menu(&mut self) {
        let Some(layer) = &self.menu.layer else {
            return;
        };
        let (width, height) = self.menu.size;
        if width == 0 || height == 0 {
            return;
        }
        let matches = self.menu.matches();
        let filter = self.menu.filter.clone();
        let selected = self.menu.selected;
        let surface = layer.wl_surface().clone();
        if let Err(err) = self.ui.draw(&surface, width, height, |p| {
            p.fill(0.08, 0.09, 0.12);
            p.text(PAD, PAD, 20., &format!("> {filter}"), Align::Left, FG);
            let top = PAD + 44.;
            let rows = ((p.height - top - PAD) / 30.).max(0.) as usize;
            if matches.is_empty() {
                p.text(PAD, top, 18., "nothing matches", Align::Left, DIM);
                return;
            }
            // Scroll so the selection stays on screen.
            let first = selected.saturating_sub(rows.saturating_sub(1));
            for (i, name) in matches.iter().enumerate().skip(first).take(rows) {
                let y = top + (i - first) as f64 * 30.;
                if i == selected {
                    p.rect(PAD / 2., y - 4., p.width - PAD, 30., SELECTED);
                }
                p.text(PAD, y, 18., name, Align::Left, FG);
            }
        }) {
            drv_os::say!("drv-shell: menu: {err}");
        }
    }

    fn menu_key(&mut self, event: KeyEvent) {
        match event.keysym {
            Keysym::Escape => self.menu.layer = None,
            Keysym::Return | Keysym::KP_Enter => self.choose_app(),
            Keysym::Up => {
                self.menu.selected = self.menu.selected.saturating_sub(1);
                self.draw_menu();
            }
            Keysym::Down => {
                if self.menu.selected + 1 < self.menu.matches().len() {
                    self.menu.selected += 1;
                }
                self.draw_menu();
            }
            Keysym::BackSpace => {
                self.menu.filter.pop();
                self.menu.selected = 0;
                self.draw_menu();
            }
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control());
                    if printable && self.menu.filter.len() + s.len() <= 64 {
                        self.menu.filter.push_str(&s);
                        self.menu.selected = 0;
                        self.draw_menu();
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ prompts

    fn on_ask(&mut self, qh: &QueueHandle<Self>, wire: Wire, req: Request) {
        match req {
            Request::Hello { version } => {
                if version != ask::VERSION {
                    drv_os::say!(
                        "drv-shell: {} speaks version {version}, we speak {}",
                        wire.name(),
                        ask::VERSION
                    );
                }
                self.reply(wire, Response::Hello { version: ask::VERSION });
            }
            Request::Confirm { id, app, uid, what, note } => {
                let kind = What::Confirm { note };
                self.enqueue(qh, Pending { wire, id, app, uid, what, kind });
            }
            Request::Secret { id, app, uid, what, prompt } => {
                let kind = What::Secret { prompt };
                self.enqueue(qh, Pending { wire, id, app, uid, what, kind });
            }
            Request::Touch { id, app, uid, what, prompt } => {
                let kind = What::Touch { prompt };
                self.enqueue(qh, Pending { wire, id, app, uid, what, kind });
            }
            Request::Pick { id, app, uid, what, note, choices } => {
                // The same id again: the list changed.
                if let Some(d) = self.dialog.as_mut().filter(|d| d.req.id == id && d.req.wire == wire) {
                    d.req.kind = What::Pick { note, choices };
                    d.reset_selection();
                    self.draw_dialog();
                    return;
                }
                if let Some(p) = self.queue.iter_mut().find(|p| p.id == id && p.wire == wire) {
                    p.kind = What::Pick { note, choices };
                    return;
                }
                let kind = What::Pick { note, choices };
                self.enqueue(qh, Pending { wire, id, app, uid, what, kind });
            }
            Request::Cancel { id } => {
                self.queue.retain(|p| !(p.id == id && p.wire == wire));
                if self.dialog.as_ref().is_some_and(|d| d.req.id == id && d.req.wire == wire) {
                    self.dialog = None;
                    self.next(qh);
                }
            }
        }
    }

    fn enqueue(&mut self, qh: &QueueHandle<Self>, req: Pending) {
        self.queue.push_back(req);
        self.next(qh);
    }

    /// Puts up the next request, if none is up and the session is not locked.
    fn next(&mut self, qh: &QueueHandle<Self>) {
        if self.dialog.is_some() || self.is_locked() {
            return;
        }
        let Some(req) = self.queue.pop_front() else {
            return;
        };
        let surface = self.ui.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("drv-shell-ask"),
            None,
        );
        let small = !matches!(req.kind, What::Pick { .. });
        layer.set_size(DIALOG_WIDTH, if small { DIALOG_SMALL } else { DIALOG_HEIGHT });
        layer.set_anchor(Anchor::empty());
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        let mut dialog = Dialog {
            req,
            layer,
            size: (0, 0),
            selected: None,
            typed: String::new(),
        };
        dialog.reset_selection();
        self.dialog = Some(dialog);
    }

    fn finish(&mut self, qh: &QueueHandle<Self>, resp: Response) {
        if let Some(d) = self.dialog.take() {
            self.reply(d.req.wire, resp);
        }
        self.next(qh);
    }

    fn dialog_enter(&mut self, qh: &QueueHandle<Self>) {
        let Some(d) = self.dialog.as_mut() else { return };
        let id = d.req.id;
        match &d.req.kind {
            What::Confirm { .. } => {
                drv_os::say!("drv-shell: {} (uid {}) may {}", d.req.app, d.req.uid, d.req.what);
                self.finish(qh, Response::Yes { id });
            }
            What::Secret { .. } => {
                if d.typed.is_empty() {
                    return;
                }
                let secret = std::mem::take(&mut d.typed);
                drv_os::say!("drv-shell: {} (uid {}): the secret goes to {}", d.req.app, d.req.uid, d.req.wire.name());
                self.finish(qh, Response::Secret { id, secret });
            }
            What::Touch { .. } => {}
            What::Pick { choices, .. } => {
                let shown = d.shown();
                let key = d.selected.and_then(|i| shown.get(i)).map(|&i| choices[i].key.clone());
                if let Some(key) = key {
                    drv_os::say!("drv-shell: {} (uid {}) may {}: {key}", d.req.app, d.req.uid, d.req.what);
                    self.finish(qh, Response::Picked { id, key });
                }
            }
        }
    }

    fn dialog_key(&mut self, qh: &QueueHandle<Self>, event: KeyEvent) {
        let Some(d) = self.dialog.as_mut() else { return };
        let yes_or_no = matches!(event.keysym, Keysym::Escape | Keysym::Return | Keysym::KP_Enter);
        let typing = matches!(event.keysym, Keysym::BackSpace) || event.utf8.is_some();
        match &d.req.kind {
            // A yes or no: nothing to type, nothing to pick. A touch: only a refusal.
            What::Confirm { .. } if !yes_or_no => return,
            What::Touch { .. } if event.keysym != Keysym::Escape => return,
            // A secret: typed, erased, sent or refused; nothing to pick.
            What::Secret { .. } if !yes_or_no && !typing => return,
            _ => {}
        }
        let secret = matches!(d.req.kind, What::Secret { .. });
        match event.keysym {
            Keysym::Escape => {
                let id = d.req.id;
                drv_os::say!("drv-shell: {} (uid {}): refused to {}", d.req.app, d.req.uid, d.req.what);
                self.finish(qh, Response::Cancelled { id });
            }
            Keysym::Return | Keysym::KP_Enter => self.dialog_enter(qh),
            Keysym::Up => {
                let n = d.shown().len();
                d.selected = match d.selected {
                    Some(i) => Some(i.saturating_sub(1)),
                    None => n.checked_sub(1),
                };
                self.draw_dialog();
            }
            Keysym::Down => {
                let n = d.shown().len();
                d.selected = match d.selected {
                    Some(i) if i + 1 < n => Some(i + 1),
                    Some(i) => Some(i),
                    None if n > 0 => Some(0),
                    None => None,
                };
                self.draw_dialog();
            }
            Keysym::BackSpace => {
                if d.typed.pop().is_some() {
                    if !secret {
                        d.reset_selection();
                    }
                    self.draw_dialog();
                }
            }
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control());
                    if printable && d.typed.len() + s.len() <= MAX_TYPED {
                        d.typed.push_str(&s);
                        if !secret {
                            d.reset_selection();
                        }
                        self.draw_dialog();
                    }
                }
            }
        }
    }

    fn draw_dialog(&mut self) {
        let Some(d) = &self.dialog else { return };
        let (width, height) = d.size;
        if width == 0 || height == 0 {
            return;
        }
        let surface = d.layer.wl_surface().clone();
        if let Err(err) = self.ui.draw(&surface, width, height, |p| paint_dialog(p, d)) {
            drv_os::say!("drv-shell: dialog: {err}");
        }
    }

    // ------------------------------------------------------------ notifications

    fn on_notify(&mut self, qh: &QueueHandle<Self>, ev: Event) {
        match ev {
            Event::New { conn, out, app, uid } => {
                self.notes.conns.insert(conn, Conn { out, app, uid });
            }
            Event::Gone { conn } => {
                self.notes.conns.remove(&conn);
            }
            Event::Notify { conn, req, replaces, summary, body } => {
                let Some(c) = self.notes.conns.get(&conn) else { return };
                let (app, uid) = (c.app.clone(), c.uid);
                let summary = clip(&summary, notify::MAX_TEXT).to_owned();
                let body = clip(&body, notify::MAX_TEXT).to_owned();
                drv_os::say!("drv-shell: {app} (uid {uid}) notifies: {summary:?}");
                let until = Instant::now() + NOTE_TTL;
                let existing = (replaces != 0)
                    .then(|| self.notes.list.iter_mut().find(|n| n.id == replaces && n.uid == uid))
                    .flatten();
                let id = match existing {
                    Some(n) => {
                        n.summary = summary;
                        n.body = body;
                        n.until = until;
                        n.id
                    }
                    None => {
                        let id = self.notes.next_id;
                        self.notes.next_id = self.notes.next_id.wrapping_add(1).max(1);
                        self.notes.list.push(Note { id, uid, app, summary, body, until });
                        id
                    }
                };
                if let Err(err) = seq::send(&c.out, &FromShell::Notified { req, id }, &[]) {
                    drv_os::say!("drv-shell: to {}: {err}", c.app);
                }
                self.show_notes(qh);
            }
            Event::Close { conn, id } => {
                let Some(c) = self.notes.conns.get(&conn) else { return };
                let uid = c.uid;
                self.notes.list.retain(|n| !(n.id == id && n.uid == uid));
                self.show_notes(qh);
            }
        }
    }

    /// The notes' surface follows the list: sized to it, gone when it is empty, hidden while
    /// locked.
    fn show_notes(&mut self, qh: &QueueHandle<Self>) {
        let shown = self.notes.list.len().min(NOTES_SHOWN) as u32;
        if shown == 0 || self.is_locked() {
            self.notes.layer = None;
            return;
        }
        let height = shown * NOTE_HEIGHT + (shown - 1) * 6;
        match &self.notes.layer {
            Some(layer) => {
                if self.notes.size.1 == height {
                    self.draw_notes();
                } else {
                    layer.set_size(NOTE_WIDTH, height);
                    layer.commit();
                }
            }
            None => {
                self.notes.size = (0, 0);
                let surface = self.ui.compositor_state.create_surface(qh);
                let layer = self.layer_shell.create_layer_surface(
                    qh,
                    surface,
                    Layer::Overlay,
                    Some("drv-shell-notes"),
                    None,
                );
                layer.set_size(NOTE_WIDTH, height);
                layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
                layer.set_margin(NOTE_MARGIN, NOTE_MARGIN, 0, 0);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.commit();
                self.notes.layer = Some(layer);
            }
        }
    }

    fn draw_notes(&mut self) {
        let Some(layer) = &self.notes.layer else { return };
        let (width, height) = self.notes.size;
        if width == 0 || height == 0 {
            return;
        }
        let surface = layer.wl_surface().clone();
        let notes = &self.notes.list;
        if let Err(err) = self.ui.draw(&surface, width, height, |p| {
            // Transparent between the cards.
            p.rect(0., 0., p.width, p.height, (0., 0., 0., 0.));
            for (i, n) in notes.iter().rev().take(NOTES_SHOWN).enumerate() {
                let y = i as f64 * (NOTE_HEIGHT as f64 + 6.);
                p.rect(0., y, p.width, NOTE_HEIGHT as f64, (0.08, 0.09, 0.12, 0.96));
                p.text(PAD, y + 10., 13., &n.app, Align::Left, BLUE);
                p.text(PAD, y + 30., 16., one_line(&n.summary), Align::Left, FG);
                p.text(PAD, y + 54., 13., one_line(&n.body), Align::Left, DIM);
            }
        }) {
            drv_os::say!("drv-shell: notes: {err}");
        }
    }

    /// Every second: notes past their time go; a grant the compositor never finished is
    /// taken back.
    fn tick(&mut self, qh: &QueueHandle<Self>) {
        let now = Instant::now();
        let before = self.notes.list.len();
        self.notes.list.retain(|n| n.until > now);
        if self.notes.list.len() != before {
            self.show_notes(qh);
        }
        if self.lock.granted {
            self.lock.granted_ticks += 1;
            if self.lock.granted_ticks > 10 {
                drv_os::say!("drv-shell: no finish after grant");
                self.lock.granted = false;
                self.lock.granted_ticks = 0;
                self.lock.message = "Unlock did not go through, try again".to_owned();
                self.draw_lock_all();
            }
        }
    }
}

/// The first line of `s`: a card has one line for it.
fn one_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn paint_dialog(p: &Painter, d: &Dialog) {
    p.fill(0.08, 0.09, 0.12);
    p.text(PAD, PAD, 20., &format!("{} wants to {}", d.req.app, d.req.what), Align::Left, FG);
    let hint = |text: &str| p.text(PAD, p.height - PAD - ROW + 8., 13., text, Align::Left, DIM);
    match &d.req.kind {
        What::Confirm { note } => {
            p.text(PAD, PAD + 40., 15., note, Align::Left, BLUE);
            hint("Enter allow   Esc refuse");
        }
        What::Secret { prompt } => {
            p.text(PAD, PAD + 30., 14., prompt, Align::Left, DIM);
            let line = format!("PIN: {}_", "\u{25cf}".repeat(d.typed.chars().count()));
            p.text(PAD, PAD + 54., 18., &line, Align::Left, BLUE);
            hint("Enter send   Esc refuse");
        }
        What::Touch { prompt } => {
            p.text(PAD, PAD + 30., 14., prompt, Align::Left, DIM);
            p.text(PAD, PAD + 54., 18., "touch it now", Align::Left, BLUE);
            hint("Esc refuse");
        }
        What::Pick { note, choices } => {
            p.text(PAD, PAD + 54., 15., note, Align::Left, BLUE);
            let shown = d.shown();
            let top = PAD + 86.;
            let bottom = p.height - PAD - 2. * ROW;
            let rows = ((bottom - top) / ROW).max(0.) as usize;
            if shown.is_empty() {
                p.text(PAD, top, 16., "nothing to pick", Align::Left, DIM);
            }
            // Scroll so the selection stays on screen.
            let first = d.selected.map_or(0, |s| s.saturating_sub(rows.saturating_sub(1)));
            for (row, &i) in shown.iter().enumerate().skip(first).take(rows) {
                let c = &choices[i];
                let y = top + (row - first) as f64 * ROW;
                if d.selected == Some(row) {
                    p.rect(PAD / 2., y - 4., p.width - PAD, ROW, SELECTED);
                }
                p.text(PAD, y, 16., &c.name, Align::Left, FG);
                if !c.detail.is_empty() {
                    p.text(PAD + 120., y, 16., &c.detail, Align::Left, DIM);
                }
            }
            p.text(PAD, bottom + 6., 16., &format!("filter: {}", d.typed), Align::Left, FG);
            hint("Enter share   Esc refuse");
        }
    }
}

// ---------------------------------------------------------------- toolkit handlers

impl Client for App {
    fn ui(&mut self) -> &mut Ui {
        &mut self.ui
    }

    /// Keys go to the surface the keyboard is on: a lock surface, the dialog, the menu.
    fn key(&mut self, qh: &QueueHandle<Self>, event: KeyEvent) {
        let Some(focus) = self.ui.focus.clone() else { return };
        let is = |s: &wl_surface::WlSurface| *s == focus;
        if self.lock.surfaces.values().any(|s| is(s.surface.wl_surface())) {
            self.lock_key(event);
        } else if self.dialog.as_ref().is_some_and(|d| is(d.layer.wl_surface())) {
            self.dialog_key(qh, event);
        } else if self.menu.layer.as_ref().is_some_and(|l| is(l.wl_surface())) {
            self.menu_key(event);
        }
    }

    fn new_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.is_locked() {
            self.lock_surface(qh, output);
        }
    }

    fn output_gone(&mut self, output: wl_output::WlOutput) {
        self.lock.surfaces.remove(&output);
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, _lock: SessionLock) {
        let outputs: Vec<_> = self.ui.output_state.outputs().collect();
        for output in outputs {
            self.lock_surface(qh, output);
        }
        self.hide_all();
    }

    fn finished(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, lock: SessionLock) {
        // The compositor unlocked (or refused): let this lock go, forget the PIN, and ask
        // again so the next locking finds our request waiting. Then what waited comes up.
        self.lock.surfaces.clear();
        if lock.is_locked() {
            lock.unlock();
        }
        self.lock.lock = None;
        self.forget_pin();
        self.lock.message.clear();
        self.lock.granted = false;
        self.lock.granted_ticks = 0;
        self.relock(qh);
        self.next(qh);
        self.show_notes(qh);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        lock_surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let output = self
            .lock
            .surfaces
            .iter()
            .find(|(_, s)| s.surface.wl_surface() == lock_surface.wl_surface())
            .map(|(o, _)| o.clone());
        if let Some(output) = output {
            self.lock.surfaces.get_mut(&output).unwrap().size = configure.new_size;
            self.draw_lock(&output);
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let surface = layer.wl_surface();
        if self.menu.layer.as_ref().is_some_and(|l| l.wl_surface() == surface) {
            self.menu.layer = None;
        } else if self.notes.layer.as_ref().is_some_and(|l| l.wl_surface() == surface) {
            self.notes.layer = None;
        } else if self.dialog.as_ref().is_some_and(|d| d.layer.wl_surface() == surface) {
            // The compositor took it down: the asker hears a refusal.
            let d = self.dialog.take().unwrap();
            self.reply(d.req.wire, Response::Cancelled { id: d.req.id });
            self.next(qh);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let surface = layer.wl_surface();
        let (w, h) = configure.new_size;
        let size = |dw: u32, dh: u32| (if w == 0 { dw } else { w }, if h == 0 { dh } else { h });
        if self.menu.layer.as_ref().is_some_and(|l| l.wl_surface() == surface) {
            self.menu.size = size(MENU_WIDTH, MENU_HEIGHT);
            self.draw_menu();
        } else if self.notes.layer.as_ref().is_some_and(|l| l.wl_surface() == surface) {
            self.notes.size = size(NOTE_WIDTH, NOTE_HEIGHT);
            self.draw_notes();
        } else if let Some(d) = self.dialog.as_mut().filter(|d| d.layer.wl_surface() == surface) {
            let small = d.small();
            d.size = size(DIALOG_WIDTH, if small { DIALOG_SMALL } else { DIALOG_HEIGHT });
            self.draw_dialog();
        }
    }
}

drv_ui::client!(App);

// ---------------------------------------------------------------- the apps' connections

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// One app's notification connection, on its own thread: the hello, then its messages to
/// the event loop, which answers on a dup.
fn notify_conn(tx: Sender<Event>, sock: OwnedFd, uid: u32, app: String) {
    match seq::recv::<ToShell>(&sock) {
        Ok((ToShell::Hello { version }, _)) if version == notify::VERSION => {}
        Ok((other, _)) => {
            drv_os::say!("drv-shell: {app} opened with {other:?}, not version {}", notify::VERSION);
            return;
        }
        Err(err) => {
            drv_os::say!("drv-shell: {app}: {err}");
            return;
        }
    }
    if let Err(err) = seq::send(&sock, &FromShell::Hello { version: notify::VERSION }, &[]) {
        drv_os::say!("drv-shell: to {app}: {err}");
        return;
    }
    let out = match sock.try_clone() {
        Ok(out) => out,
        Err(err) => {
            drv_os::say!("drv-shell: dup: {err}");
            return;
        }
    };
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    if tx.send(Event::New { conn, out, app: app.clone(), uid }).is_err() {
        return;
    }
    loop {
        let ev = match seq::recv::<ToShell>(&sock) {
            Ok((ToShell::Notify { req, replaces, summary, body }, _)) => {
                Event::Notify { conn, req, replaces, summary, body }
            }
            Ok((ToShell::Close { id }, _)) => Event::Close { conn, id },
            Ok((ToShell::Hello { .. }, _)) => continue,
            Err(err) => {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    drv_os::say!("drv-shell: {app}: {err}");
                }
                break;
            }
        };
        if tx.send(ev).is_err() {
            break;
        }
    }
    let _ = tx.send(Event::Gone { conn });
}

fn run() -> Result<(), String> {
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let wayland = fds.socket("wayland", Kind::Stream).map_err(|e| e.to_string())?;
    let auth = fds.socket("auth", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let appd = fds.socket("appd", Kind::Stream).map_err(|e| e.to_string())?;
    let poke = fds.socket("poke", Kind::Stream).map_err(|e| e.to_string())?;
    let cast = fds.socket("cast", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let agent = fds.socket("agent", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let listener = fds.listener_of("listener", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let appd = PolicyClient::from_stream(UnixStream::from(appd))
        .map_err(|e| format!("the launch channel: {e}"))?;
    let door = Arc::new(Door::open().map_err(|e| format!("drv-appd: {e}"))?);

    let drv_ui::Session {
        globals,
        qh,
        mut event_loop,
        ..
    } = drv_ui::session::<App>(wayland)?;
    let ui = drv_ui::ui!(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| format!("layer shell: {e}"))?;
    let cast_out = cast.try_clone().map_err(|e| format!("dup: {e}"))?;
    let agent_out = agent.try_clone().map_err(|e| format!("dup: {e}"))?;
    let mut app = App {
        lock: Lock {
            state: SessionLockState::new(&globals, &qh),
            lock: None,
            surfaces: HashMap::new(),
            pin: Vec::new(),
            message: String::new(),
            granted: false,
            granted_ticks: 0,
            auth,
        },
        ui,
        layer_shell,
        menu: Menu {
            layer: None,
            size: (0, 0),
            appd,
            apps: Vec::new(),
            filter: String::new(),
            selected: 0,
        },
        cast: cast_out,
        agent: agent_out,
        queue: VecDeque::new(),
        dialog: None,
        notes: Notes {
            conns: HashMap::new(),
            list: Vec::new(),
            next_id: 1,
            layer: None,
            size: (0, 0),
        },
    };
    app.relock(&qh);

    // Each byte from the compositor is one `show-launcher`; a hangup is the compositor gone.
    let poke_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(
            Generic::new(UnixStream::from(poke), Interest::READ, Mode::Level),
            move |_, sock, app: &mut App| {
                let mut buf = [0u8; 64];
                let mut sock: &UnixStream = sock;
                match sock.read(&mut buf) {
                    Ok(0) => Err(io::Error::other("the compositor hung up")),
                    Ok(_) => {
                        app.show_menu(&poke_qh);
                        Ok(PostAction::Continue)
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(PostAction::Continue),
                    Err(err) => Err(err),
                }
            },
        )
        .map_err(|e| format!("event loop: {e}"))?;
    for (wire, sock) in [(Wire::Cast, cast), (Wire::Agent, agent)] {
        let wire_qh = qh.clone();
        event_loop
            .handle()
            .insert_source(
                Generic::new(sock, Interest::READ, Mode::Level),
                move |_, sock, app: &mut App| match seq::recv::<Request>(&*sock) {
                    Ok((req, _)) => {
                        app.on_ask(&wire_qh, wire, req);
                        Ok(PostAction::Continue)
                    }
                    Err(err) => Err(io::Error::other(format!("{}: {err}", wire.name()))),
                },
            )
            .map_err(|e| format!("event loop: {e}"))?;
    }
    let (tx, rx) = channel::channel::<Event>();
    let notes_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(rx, move |ev, _, app: &mut App| {
            if let channel::Event::Msg(ev) = ev {
                app.on_notify(&notes_qh, ev);
            }
        })
        .map_err(|e| format!("event loop: {e}"))?;
    let tick_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::from_secs(1)), move |_, _, app: &mut App| {
            app.tick(&tick_qh);
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| format!("timer: {e}"))?;

    std::thread::spawn(move || {
        let err = door.serve(listener, "drv-shell", move |sock, uid, policy| {
            notify_conn(tx.clone(), sock, uid, policy.name.clone())
        });
        drv_os::say!("drv-shell: the notification socket: {err:?}");
        process::exit(1);
    });

    drv_ui::warm_fonts();
    drv_ui::seal_with("drv-shell", |allow| {
        // The apps' connections come in, and drv-appd is asked who they are.
        allow.accept();
        allow.socket(rustix::net::AddressFamily::UNIX.as_raw() as _)?;
        allow.connect_unix().map(|_| ())
    })?;

    loop {
        if let Err(err) = event_loop.dispatch(None, &mut app) {
            // The compositor went away; the supervisor restarts the whole set, us included.
            return Err(format!("event loop: {err}"));
        }
    }
}

fn main() {
    if let Err(err) = run() {
        drv_os::say!("drv-shell: {err}");
        process::exit(1);
    }
}
