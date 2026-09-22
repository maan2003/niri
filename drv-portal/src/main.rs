//! The person's side of the portals, a supervisor service. An app asks its private bus for
//! a file or a screen; the bridge turns that into a request down our link (fd `bridge`); we
//! ask the person on a layer-shell surface (fd `wayland`). The ssh agent asks the same way
//! (fd `agent`) for the authenticator's PIN and touch, so no app ever sees the PIN. A file comes from the tree we own
//! (`--files`) and is answered as a path under the documents mount, which we serve on fd
//! `fuse` (see `docs`): the app never sees the tree, only the file it was given, and only as
//! the UID it was given to. A screen or a window is started at the compositor over fd
//! `compositor` (`drv_portal::compositor`), and the app gets the PipeWire node, nothing else. Sealed with
//! seccomp once the fonts are warm.

mod docs;

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::thread;

use clap::Parser;
use drv_os::fds::Kind;
use drv_policy::seq;
use drv_portal::compositor::{self, Output, ToCompositor, ToPortal, Window};
use drv_portal::protocol::{Cursor, Device, Kind as Ask, Request, Response, Source, VERSION};
use drv_ui::sctk::reexports::calloop::generic::Generic;
use drv_ui::sctk::reexports::calloop::{Interest, Mode, PostAction};
use drv_ui::sctk::seat::keyboard::{KeyEvent, Keysym};
use drv_ui::sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use drv_ui::sctk::shell::WaylandSurface;
use drv_ui::wayland_client::{Connection, QueueHandle};
use drv_ui::{Align, Client, Painter, Ui};

use docs::{Docs, Grant};

#[derive(Parser)]
#[command(name = "drv-portal", about = "The file chooser, the documents mount, screen sharing")]
struct Args {
    /// The person's files: the tree the chooser shows. Ours alone.
    #[arg(long)]
    files: PathBuf,
    /// Where apps see the documents mount (the supervisor mounted our fd `fuse` there).
    #[arg(long, default_value = "/run/drv-doc")]
    docs: PathBuf,
}

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
/// A PIN or a touch: a line or two, no list.
const HEIGHT_SMALL: u32 = 180;
const ROW: f64 = 28.;
const PAD: f64 = 16.;
const MAX_TYPED: usize = 200;

/// Who asked, and gets the answer: ids are theirs, so they only mean something per wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Bridge,
    Agent,
}

struct Pending {
    wire: Wire,
    id: u64,
    app: String,
    uid: u32,
    /// The chooser's title; the agent's prompt.
    title: String,
    what: What,
}

enum What {
    Choose(Ask),
    Cast { cursor: Cursor, screens: bool, windows: bool },
    Grant(Device),
    /// The authenticator's PIN, typed here and sent to the agent.
    Pin,
    /// A touch on the authenticator: shown until the agent takes it down.
    Touch,
}

fn device_name(device: Device) -> &'static str {
    match device {
        Device::Microphone => "microphone",
        Device::Camera => "camera",
    }
}

/// A device the person let an app use, until they revoke it or the bridge says the app is
/// gone.
struct LiveDevice {
    app: String,
    uid: u32,
    device: Device,
}

struct Entry {
    name: String,
    dir: bool,
    /// Shown after the name: make, model and size for a screen; the title for a window.
    detail: String,
    /// What a cast entry stands for.
    source: Option<Source>,
}

/// A cast the person consented to, from `Start` until the compositor says `Stopped`.
struct Live {
    app: String,
    uid: u32,
    source: Source,
    /// For the log: the screen's name, or the window's app and title.
    label: String,
    token: String,
}

/// A source the person let an app share, good for that app until the bridge says it is gone.
struct Consent {
    app: String,
    uid: u32,
    source: Source,
}

/// The dialog that is up.
struct Dialog {
    req: Pending,
    /// Below `--files`; empty at the top.
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Index into the shown entries; `None` while saving means the name line has the cursor.
    selected: Option<usize>,
    /// What the person typed: the name to save as, or the filter while opening.
    typed: String,
    note: Option<String>,
}

impl Dialog {
    fn saving(&self) -> bool {
        matches!(self.req.what, What::Choose(Ask::Save { .. }))
    }

    fn casting(&self) -> bool {
        matches!(self.req.what, What::Cast { .. })
    }

    fn granting(&self) -> Option<Device> {
        match self.req.what {
            What::Grant(device) => Some(device),
            _ => None,
        }
    }

    fn pinning(&self) -> bool {
        matches!(self.req.what, What::Pin)
    }

    fn touching(&self) -> bool {
        matches!(self.req.what, What::Touch)
    }

    /// The small dialogs: a PIN or a touch.
    fn small(&self) -> bool {
        self.pinning() || self.touching()
    }

    /// Entries that pass the filter, as indices into `entries`.
    fn shown(&self) -> Vec<usize> {
        if self.saving() || self.typed.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let filter = self.typed.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.name.to_lowercase().contains(&filter) || e.detail.to_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn reset_selection(&mut self) {
        self.selected = if self.saving() || self.shown().is_empty() { None } else { Some(0) };
    }
}

struct App {
    ui: Ui,
    layer_shell: LayerShell,
    layer: Option<LayerSurface>,
    size: (u32, u32),
    files: PathBuf,
    docs: PathBuf,
    bridge: OwnedFd,
    agent: OwnedFd,
    compositor: OwnedFd,
    grants: docs::Shared,
    queue: VecDeque<Pending>,
    dialog: Option<Dialog>,
    /// What the compositor last said it has; asked again with every cast request.
    outputs: Vec<Output>,
    windows: Vec<Window>,
    /// Casts by the bridge's request id.
    casts: HashMap<u64, Live>,
    /// Consents by token.
    consents: HashMap<String, Consent>,
    /// Devices the person allowed, by the bridge's id for the request.
    devices: HashMap<u64, LiveDevice>,
    next_token: u64,
}

impl App {
    fn on_request(&mut self, qh: &QueueHandle<Self>, wire: Wire, req: Request) {
        match req {
            Request::Hello { version } => {
                if version != VERSION {
                    let who = match wire {
                        Wire::Bridge => "the bridge",
                        Wire::Agent => "the ssh agent",
                    };
                    drv_os::say!("drv-portal: {who} speaks version {version}, we speak {VERSION}");
                }
                self.reply(wire, Response::Hello { version: VERSION });
            }
            Request::Choose { id, app, uid, title, kind } => {
                self.queue.push_back(Pending { wire, id, app, uid, title, what: What::Choose(kind) });
                self.next(qh);
            }
            Request::Pin { id, app, uid, prompt } => {
                self.queue.push_back(Pending { wire, id, app, uid, title: prompt, what: What::Pin });
                self.next(qh);
            }
            Request::Touch { id, app, uid, prompt } => {
                self.queue.push_back(Pending { wire, id, app, uid, title: prompt, what: What::Touch });
                self.next(qh);
            }
            Request::Cast { id, app, uid, cursor, screens, windows, again } => {
                self.tell(ToCompositor::Outputs);
                self.tell(ToCompositor::Windows);
                if let Some(token) = again {
                    let standing = self.consents.get(&token).filter(|c| c.app == app && c.uid == uid);
                    if let Some((source, label)) = standing.and_then(|c| self.label(&c.source).map(|l| (c.source.clone(), l))) {
                        drv_os::say!("drv-portal: {app} (uid {uid}) shares {label} again");
                        self.casts.insert(id, Live { app, uid, source: source.clone(), label, token });
                        self.tell(ToCompositor::Start { cast: id, source, cursor });
                        return;
                    }
                }
                let what = What::Cast { cursor, screens, windows };
                self.queue.push_back(Pending { wire, id, app, uid, title: String::new(), what });
                self.next(qh);
            }
            Request::Grant { id, app, uid, device } => {
                let what = What::Grant(device);
                self.queue.push_back(Pending { wire, id, app, uid, title: String::new(), what });
                self.next(qh);
            }
            Request::Forget { app, uid } => {
                self.consents.retain(|_, c| !(c.app == app && c.uid == uid));
                self.devices.retain(|_, d| !(d.app == app && d.uid == uid));
                self.show_devices();
            }
            Request::Cancel { id } => {
                self.queue.retain(|p| !(p.id == id && p.wire == wire));
                if self.dialog.as_ref().is_some_and(|d| d.req.id == id && d.req.wire == wire) {
                    self.dialog = None;
                    self.layer = None;
                    self.next(qh);
                }
                if wire != Wire::Bridge {
                    return;
                }
                if let Some(live) = self.casts.remove(&id) {
                    drv_os::say!("drv-portal: {} (uid {}) closed its cast of {}", live.app, live.uid, live.label);
                    self.tell(ToCompositor::Stop { cast: id });
                }
                if let Some(live) = self.devices.remove(&id) {
                    drv_os::say!("drv-portal: {} (uid {}) is done with the {}", live.app, live.uid, device_name(live.device));
                    self.show_devices();
                }
            }
        }
    }

    fn on_compositor(&mut self, ev: ToPortal) {
        match ev {
            ToPortal::Hello { version } => {
                if version != compositor::VERSION {
                    drv_os::say!("drv-portal: the compositor speaks version {version}, we speak {}", compositor::VERSION);
                }
            }
            ToPortal::Outputs(outputs) => {
                self.outputs = outputs;
                self.relist();
            }
            ToPortal::Windows(windows) => {
                self.windows = windows;
                self.relist();
            }
            ToPortal::Started { cast, node_id, width, height } => match self.casts.get(&cast) {
                Some(live) => {
                    drv_os::say!("drv-portal: {} (uid {}) shares {} on PipeWire node {node_id}", live.app, live.uid, live.label);
                    self.send(Response::Cast {
                        id: cast,
                        node_id,
                        source: live.source.clone(),
                        width,
                        height,
                        token: live.token.clone(),
                    });
                }
                // Cancelled in between: the compositor started it for nobody.
                None => self.tell(ToCompositor::Stop { cast }),
            },
            ToPortal::Stopped { cast } => {
                if let Some(live) = self.casts.remove(&cast) {
                    drv_os::say!("drv-portal: the cast of {} for {} ended", live.label, live.app);
                    self.send(Response::Closed { id: cast });
                }
            }
            ToPortal::Revoke => {
                let devices: Vec<_> = self.devices.drain().collect();
                for (id, live) in devices {
                    drv_os::say!("drv-portal: {} (uid {}) loses the {}", live.app, live.uid, device_name(live.device));
                    self.send(Response::Closed { id });
                }
                self.show_devices();
            }
        }
    }

    /// To the bridge: casts and devices are its alone.
    fn send(&self, resp: Response) {
        self.reply(Wire::Bridge, resp);
    }

    fn reply(&self, wire: Wire, resp: Response) {
        let (sock, who) = match wire {
            Wire::Bridge => (&self.bridge, "the bridge"),
            Wire::Agent => (&self.agent, "the ssh agent"),
        };
        if let Err(err) = seq::send(sock, &resp, &[]) {
            drv_os::say!("drv-portal: to {who}: {err}");
        }
    }

    /// The compositor's indicator follows `devices`: who holds the microphone, who the camera.
    fn show_devices(&self) {
        let holders = |device: Device| {
            let mut names: Vec<String> =
                self.devices.values().filter(|d| d.device == device).map(|d| d.app.clone()).collect();
            names.sort();
            names.dedup();
            names
        };
        self.tell(ToCompositor::Devices { mic: holders(Device::Microphone), camera: holders(Device::Camera) });
    }

    fn tell(&self, msg: ToCompositor) {
        if let Err(err) = seq::send(&self.compositor, &msg, &[]) {
            drv_os::say!("drv-portal: to the compositor: {err}");
        }
    }

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

    /// The compositor's lists changed: a cast dialog shows the new ones.
    fn relist(&mut self) {
        if self.dialog.as_ref().is_some_and(|d| d.casting()) {
            let mut d = self.dialog.take().unwrap();
            self.list(&mut d);
            self.dialog = Some(d);
            self.draw();
        }
    }

    /// Puts up the next request, if none is up.
    fn next(&mut self, qh: &QueueHandle<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let Some(req) = self.queue.pop_front() else {
            return;
        };
        let typed = match &req.what {
            What::Choose(Ask::Save { name }) => {
                name.chars().filter(|c| !c.is_control() && *c != '/').take(MAX_TYPED).collect()
            }
            _ => String::new(),
        };
        let mut dialog = Dialog {
            req,
            dir: PathBuf::new(),
            entries: Vec::new(),
            selected: None,
            typed,
            note: None,
        };
        self.list(&mut dialog);
        self.dialog = Some(dialog);
        self.size = (0, 0);
        let surface = self.ui.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("drv-portal"),
            None,
        );
        let small = self.dialog.as_ref().is_some_and(|d| d.small());
        layer.set_size(WIDTH, if small { HEIGHT_SMALL } else { HEIGHT });
        layer.set_anchor(Anchor::empty());
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        self.layer = Some(layer);
    }

    /// Fills `d.entries`: the screens and windows for a cast; otherwise its directory,
    /// files and directories, no dotfiles, and no symlinks, which could lead out of the tree.
    fn list(&self, d: &mut Dialog) {
        d.entries.clear();
        d.note = None;
        if d.granting().is_some() || d.small() {
            return;
        }
        if let What::Cast { screens, windows, .. } = d.req.what {
            if screens {
                for o in &self.outputs {
                    if o.width == 0 || o.height == 0 {
                        continue;
                    }
                    let detail = format!("{} {}  {}x{}", o.make, o.model, o.width, o.height);
                    let source = Some(Source::Screen(o.name.clone()));
                    d.entries.push(Entry { name: o.name.clone(), dir: false, detail, source });
                }
            }
            if windows {
                for w in &self.windows {
                    // The app's name is ours; the title is the window's own word.
                    let source = Some(Source::Window(w.id));
                    d.entries.push(Entry { name: w.app.clone(), dir: false, detail: w.title.clone(), source });
                }
            }
            d.reset_selection();
            return;
        }
        match fs::read_dir(self.files.join(&d.dir)) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let Ok(name) = e.file_name().into_string() else { continue };
                    if name.starts_with('.') || !(ft.is_dir() || ft.is_file()) {
                        continue;
                    }
                    d.entries.push(Entry { name, dir: ft.is_dir(), detail: String::new(), source: None });
                }
                d.entries.sort_by(|a, b| {
                    b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
            }
            Err(err) => d.note = Some(format!("cannot list /{}: {err}", d.dir.display())),
        }
        d.reset_selection();
    }

    fn finish(&mut self, qh: &QueueHandle<Self>, resp: Response) {
        let wire = self.dialog.as_ref().map_or(Wire::Bridge, |d| d.req.wire);
        self.reply(wire, resp);
        self.dialog = None;
        self.layer = None;
        self.next(qh);
    }

    fn enter(&mut self, qh: &QueueHandle<Self>) {
        let Some(d) = self.dialog.as_mut() else { return };
        if let Some(device) = d.granting() {
            let id = d.req.id;
            drv_os::say!("drv-portal: {} (uid {}) may use the {}", d.req.app, d.req.uid, device_name(device));
            self.devices.insert(id, LiveDevice { app: d.req.app.clone(), uid: d.req.uid, device });
            self.show_devices();
            self.finish(qh, Response::Granted { id });
            return;
        }
        if d.pinning() {
            if d.typed.is_empty() {
                return;
            }
            let id = d.req.id;
            let pin = std::mem::take(&mut d.typed);
            drv_os::say!("drv-portal: {} (uid {}) gets the authenticator's PIN", d.req.app, d.req.uid);
            self.finish(qh, Response::Pin { id, pin });
            return;
        }
        if d.touching() {
            return;
        }
        let shown = d.shown();
        let picked = d.selected.and_then(|i| shown.get(i)).map(|&i| (d.entries[i].name.clone(), d.entries[i].dir));
        if d.casting() {
            let source = d.selected.and_then(|i| shown.get(i)).and_then(|&i| d.entries[i].source.clone());
            if let Some(source) = source {
                self.start_cast(qh, source);
            }
            return;
        }
        match picked {
            Some((name, true)) => {
                d.dir.push(name);
                if !d.saving() {
                    d.typed.clear();
                }
                let mut d = self.dialog.take().unwrap();
                self.list(&mut d);
                self.dialog = Some(d);
                self.draw();
            }
            Some((name, false)) if d.saving() => {
                // Picked an existing file to overwrite: its name goes on the line, Enter again
                // saves there.
                d.typed = name;
                d.selected = None;
                self.draw();
            }
            Some((name, false)) => self.grant(qh, name),
            None if d.saving() => {
                let name = d.typed.clone();
                self.grant(qh, name)
            }
            None => {}
        }
    }

    /// Opens the file for the app and answers with where the app finds it.
    fn grant(&mut self, qh: &QueueHandle<Self>, name: String) {
        let Some(d) = self.dialog.as_mut() else { return };
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            d.note = Some("that is not a file name".to_owned());
            self.draw();
            return;
        }
        let write = d.saving();
        let path = self.files.join(&d.dir).join(&name);
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        if write {
            options.write(true).create(true).mode(0o600);
        }
        let file = match options.open(&path).and_then(check_regular) {
            Ok(file) => file,
            Err(err) => {
                d.note = Some(format!("{name}: {err}"));
                self.draw();
                return;
            }
        };
        let (id_req, uid, app) = (d.req.id, d.req.uid, d.req.app.clone());
        let id = self.grants.lock().unwrap().add(Grant { uid, name: name.clone(), file, write });
        let doc = self.docs.join(id.to_string()).join(&name);
        drv_os::say!(
            "drv-portal: {app} (uid {uid}) gets {} as {}{}",
            path.display(),
            doc.display(),
            if write { ", writable" } else { "" }
        );
        self.finish(qh, Response::Chosen { id: id_req, paths: vec![doc.to_string_lossy().into_owned()] });
    }

    /// The person picked a source: the compositor starts the cast; the bridge hears when
    /// the node exists.
    fn start_cast(&mut self, qh: &QueueHandle<Self>, source: Source) {
        let label = self.label(&source);
        let Some(d) = self.dialog.as_mut() else { return };
        let What::Cast { cursor, .. } = d.req.what else { return };
        let Some(label) = label else {
            d.note = Some("that is gone".to_owned());
            self.draw();
            return;
        };
        let id = d.req.id;
        drv_os::say!("drv-portal: {} (uid {}) may share {label}", d.req.app, d.req.uid);
        // Tokens only mean something with the app they were given to, so plain counting does.
        self.next_token += 1;
        let token = format!("drv{}", self.next_token);
        self.consents.insert(token.clone(), Consent { app: d.req.app.clone(), uid: d.req.uid, source: source.clone() });
        self.casts.insert(id, Live { app: d.req.app.clone(), uid: d.req.uid, source: source.clone(), label, token });
        self.tell(ToCompositor::Start { cast: id, source, cursor });
        self.dialog = None;
        self.layer = None;
        self.next(qh);
    }

    fn up(&mut self) {
        let Some(d) = self.dialog.as_mut() else { return };
        if d.dir.pop() {
            let mut d = self.dialog.take().unwrap();
            self.list(&mut d);
            self.dialog = Some(d);
            self.draw();
        }
    }

    fn draw(&mut self) {
        let (Some(layer), Some(d)) = (&self.layer, &self.dialog) else {
            return;
        };
        let (width, height) = self.size;
        if width == 0 || height == 0 {
            return;
        }
        let shown = d.shown();
        let surface = layer.wl_surface().clone();
        if let Err(err) = self.ui.draw(&surface, width, height, |p| paint(p, d, &shown)) {
            drv_os::say!("drv-portal: {err}");
        }
    }
}

fn check_regular(file: File) -> io::Result<File> {
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::other("not a regular file"))
    }
}

fn paint(p: &Painter, d: &Dialog, shown: &[usize]) {
    let fg = (0.93, 0.93, 0.95, 1.);
    let dim = (0.6, 0.6, 0.65, 1.);
    let blue = (0.55, 0.75, 1., 1.);
    p.fill(0.08, 0.09, 0.12);
    let head = if let Some(device) = d.granting() {
        format!("{} wants to use your {}", d.req.app, device_name(device))
    } else if d.small() {
        format!("{} wants to use your security key", d.req.app)
    } else if d.casting() {
        format!("{} wants to see your screen", d.req.app)
    } else {
        let verb = if d.saving() { "save" } else { "open" };
        format!("{} wants to {verb} a file", d.req.app)
    };
    p.text(PAD, PAD, 20., &head, Align::Left, fg);
    if !d.req.title.is_empty() {
        p.text(PAD, PAD + 30., 14., &d.req.title, Align::Left, dim);
    }
    if d.small() {
        let line = if d.pinning() {
            format!("PIN: {}_", "\u{25cf}".repeat(d.typed.chars().count()))
        } else {
            "touch it now".to_owned()
        };
        p.text(PAD, PAD + 54., 18., &line, Align::Left, blue);
        let (hint, color) = match &d.note {
            Some(note) => (note.as_str(), (1., 0.6, 0.5, 1.)),
            None if d.pinning() => ("Enter send   Esc refuse", dim),
            None => ("Esc refuse", dim),
        };
        p.text(PAD, p.height - PAD - ROW + 8., 13., hint, Align::Left, color);
        return;
    }
    let line = if d.granting().is_some() {
        "until it exits or you revoke it (Mod+Shift+Esc revokes everything)".to_owned()
    } else if d.casting() {
        "what it may see, until you stop it (Mod+Shift+Esc stops them all)".to_owned()
    } else {
        format!("/{}", d.dir.display())
    };
    p.text(PAD, PAD + 54., 15., &line, Align::Left, blue);

    let top = PAD + 86.;
    let bottom = p.height - PAD - 2. * ROW;
    let rows = ((bottom - top) / ROW).max(0.) as usize;
    if shown.is_empty() && d.granting().is_none() {
        let what = if d.casting() { "nothing to share" } else { "nothing here" };
        p.text(PAD, top, 16., what, Align::Left, dim);
    }
    // Scroll so the selection stays on screen.
    let first = d.selected.map_or(0, |s| s.saturating_sub(rows.saturating_sub(1)));
    for (row, &i) in shown.iter().enumerate().skip(first).take(rows) {
        let e = &d.entries[i];
        let y = top + (row - first) as f64 * ROW;
        if d.selected == Some(row) {
            p.rect(PAD / 2., y - 4., p.width - PAD, ROW, (0.25, 0.35, 0.55, 1.));
        }
        let label = if e.dir { format!("{}/", e.name) } else { e.name.clone() };
        p.text(PAD, y, 16., &label, Align::Left, if e.dir { blue } else { fg });
        if !e.detail.is_empty() {
            p.text(PAD + 120., y, 16., &e.detail, Align::Left, dim);
        }
    }

    if !d.casting() && d.granting().is_none() {
        let (label, cursor) = if d.saving() {
            ("name", if d.selected.is_none() { "_" } else { "" })
        } else {
            ("filter", "")
        };
        p.text(PAD, bottom + 6., 16., &format!("{label}: {}{cursor}", d.typed), Align::Left, fg);
    }
    let (hint, color) = match &d.note {
        Some(note) => (note.as_str(), (1., 0.6, 0.5, 1.)),
        None if d.granting().is_some() => ("Enter allow   Esc refuse", dim),
        None if d.casting() => ("Enter share   Esc refuse", dim),
        None => ("Enter choose   Backspace up   Esc cancel", dim),
    };
    p.text(PAD, p.height - PAD - ROW + 8., 13., hint, Align::Left, color);
}

impl Client for App {
    fn ui(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn key(&mut self, qh: &QueueHandle<Self>, event: KeyEvent) {
        let Some(d) = self.dialog.as_mut() else { return };
        // A yes or no: nothing to type, nothing to pick. A touch: only a refusal.
        let yes_or_no = matches!(event.keysym, Keysym::Escape | Keysym::Return | Keysym::KP_Enter);
        if (d.granting().is_some() && !yes_or_no) || (d.touching() && event.keysym != Keysym::Escape) {
            return;
        }
        // A PIN: typed, erased, sent or refused; nothing to pick.
        if d.pinning() && !yes_or_no && !matches!(event.keysym, Keysym::BackSpace) && event.utf8.is_none() {
            return;
        }
        match event.keysym {
            Keysym::Escape => {
                let id = d.req.id;
                self.finish(qh, Response::Cancelled { id });
            }
            Keysym::Return | Keysym::KP_Enter => self.enter(qh),
            Keysym::Up => {
                let n = d.shown().len();
                d.selected = match d.selected {
                    Some(i) => Some(i.saturating_sub(1)),
                    None => n.checked_sub(1),
                };
                self.draw();
            }
            Keysym::Down => {
                let n = d.shown().len();
                d.selected = match d.selected {
                    Some(i) if i + 1 < n => Some(i + 1),
                    Some(i) => Some(i),
                    None if n > 0 => Some(0),
                    None => None,
                };
                self.draw();
            }
            Keysym::BackSpace => {
                if d.typed.pop().is_some() {
                    d.reset_selection();
                    self.draw();
                } else if !d.pinning() {
                    self.up();
                }
            }
            Keysym::Left => self.up(),
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control() && (c != '/' || d.pinning()));
                    if printable && d.typed.len() + s.len() <= MAX_TYPED {
                        d.typed.push_str(&s);
                        d.reset_selection();
                        self.draw();
                    }
                }
            }
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.layer = None;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        self.size = (
            if w == 0 { WIDTH } else { w },
            if h == 0 { HEIGHT } else { h },
        );
        self.draw();
    }
}

drv_ui::client!(App);

fn run() -> Result<(), String> {
    let args = Args::parse();
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let wayland = fds.socket("wayland", Kind::Stream).map_err(|e| e.to_string())?;
    let bridge = fds.socket("bridge", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let agent = fds.socket("agent", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let compositor = fds.socket("compositor", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let fuse = fds.file("fuse").map_err(|e| e.to_string())?;

    // The mount is up before we are; serving it is its own thread, and losing it ends us.
    let grants: docs::Shared = Arc::default();
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let session = fuser::Session::from_fd(
        Docs::new(grants.clone(), uid),
        fuse,
        fuser::SessionACL::All,
        fuser::Config::default(),
    )
    .map_err(|e| format!("the documents mount: {e}"))?;
    thread::spawn(move || {
        if let Err(err) = session.run() {
            drv_os::say!("drv-portal: the documents mount: {err}");
        }
        process::exit(1);
    });

    let drv_ui::Session {
        globals,
        qh,
        mut event_loop,
        ..
    } = drv_ui::session::<App>(wayland)?;
    let ui = drv_ui::ui!(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| format!("layer shell: {e}"))?;
    let bridge_out = bridge.try_clone().map_err(|e| format!("dup: {e}"))?;
    let agent_out = agent.try_clone().map_err(|e| format!("dup: {e}"))?;
    let compositor_out = compositor.try_clone().map_err(|e| format!("dup: {e}"))?;
    seq::send(&compositor_out, &ToCompositor::Hello { version: compositor::VERSION }, &[])
        .map_err(|e| format!("hello to the compositor: {e}"))?;
    let mut app = App {
        ui,
        layer_shell,
        layer: None,
        size: (0, 0),
        files: args.files,
        docs: args.docs,
        bridge: bridge_out,
        agent: agent_out,
        compositor: compositor_out,
        grants,
        queue: VecDeque::new(),
        dialog: None,
        outputs: Vec::new(),
        windows: Vec::new(),
        casts: HashMap::new(),
        consents: HashMap::new(),
        devices: HashMap::new(),
        next_token: 0,
    };

    let src_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(
            Generic::new(bridge, Interest::READ, Mode::Level),
            move |_, sock, app: &mut App| match seq::recv::<Request>(&*sock) {
                Ok((req, _)) => {
                    app.on_request(&src_qh, Wire::Bridge, req);
                    Ok(PostAction::Continue)
                }
                Err(err) => Err(io::Error::other(format!("the bridge: {err}"))),
            },
        )
        .map_err(|e| format!("event loop: {e}"))?;
    let agent_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(
            Generic::new(agent, Interest::READ, Mode::Level),
            move |_, sock, app: &mut App| match seq::recv::<Request>(&*sock) {
                Ok((req, _)) => {
                    app.on_request(&agent_qh, Wire::Agent, req);
                    Ok(PostAction::Continue)
                }
                Err(err) => Err(io::Error::other(format!("the ssh agent: {err}"))),
            },
        )
        .map_err(|e| format!("event loop: {e}"))?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(compositor, Interest::READ, Mode::Level),
            move |_, sock, app: &mut App| match seq::recv::<ToPortal>(&*sock) {
                Ok((ev, _)) => {
                    app.on_compositor(ev);
                    Ok(PostAction::Continue)
                }
                Err(err) => Err(io::Error::other(format!("the compositor: {err}"))),
            },
        )
        .map_err(|e| format!("event loop: {e}"))?;

    drv_ui::warm_fonts();
    drv_ui::seal_with("drv-portal", |allow| allow.write_files().map(|_| ()))?;

    loop {
        if let Err(err) = event_loop.dispatch(None, &mut app) {
            return Err(format!("event loop: {err}"));
        }
    }
}

fn main() {
    if let Err(err) = run() {
        drv_os::say!("drv-portal: {err}");
        process::exit(1);
    }
}
