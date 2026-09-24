//! The person's files, a supervisor service. Apps connect to fd `listener`
//! (`drv_files::wire`), each keyed on its uid and drv-appd's word for it, and ask for a
//! file; we ask the person on a layer-shell surface (fd `wayland`), one dialog at a time,
//! showing the tree we own (`--files`). The pick is answered as a path under the documents
//! mount, which we serve on fd `fuse` (see `docs`): the app never sees the tree, only the
//! file it was given, and only as the uid it was given to. Sealed with seccomp once the
//! fonts are warm.

mod docs;

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use clap::Parser;
use drv_files::wire::{self, FromFiles, Kind as Ask, ToFiles};
use drv_os::fds::Kind;
use drv_policy::door::Door;
use drv_policy::seq;
use drv_ui::sctk::reexports::calloop::channel::{self, Sender};
use drv_ui::sctk::seat::keyboard::{KeyEvent, Keysym};
use drv_ui::sctk::shell::WaylandSurface;
use drv_ui::sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use drv_ui::wayland_client::protocol::wl_surface;
use drv_ui::wayland_client::{Connection, QueueHandle};
use drv_ui::{Align, Client, Painter, Ui};

use docs::{Docs, Grant};

#[derive(Parser)]
#[command(name = "drv-files", about = "The file chooser and the documents mount")]
struct Args {
    /// The person's files: the tree the chooser shows. Ours alone.
    #[arg(long)]
    files: PathBuf,
    /// Where apps see the documents mount (the supervisor mounted our fd `fuse` there).
    #[arg(long, default_value = "/run/drv/doc")]
    docs: PathBuf,
}

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const ROW: f64 = 28.;
const PAD: f64 = 16.;
const MAX_TYPED: usize = 200;

/// From the threads that read the apps' connections.
enum Event {
    New { conn: u64, out: OwnedFd, app: String, uid: u32 },
    Choose { conn: u64, req: u64, kind: Ask },
    Cancel { conn: u64, req: u64 },
    Gone { conn: u64 },
}

struct Conn {
    out: OwnedFd,
    app: String,
    uid: u32,
}

struct Pending {
    conn: u64,
    req: u64,
    app: String,
    uid: u32,
    kind: Ask,
}

struct Entry {
    name: String,
    dir: bool,
}

/// The dialog that is up.
struct Dialog {
    req: Pending,
    layer: LayerSurface,
    size: (u32, u32),
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
        matches!(self.req.kind, Ask::Save { .. })
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
            .filter(|(_, e)| e.name.to_lowercase().contains(&filter))
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
    files: PathBuf,
    docs: PathBuf,
    grants: docs::Shared,
    conns: HashMap<u64, Conn>,
    queue: VecDeque<Pending>,
    dialog: Option<Dialog>,
}

impl App {
    fn on_event(&mut self, qh: &QueueHandle<Self>, ev: Event) {
        match ev {
            Event::New { conn, out, app, uid } => {
                self.conns.insert(conn, Conn { out, app, uid });
            }
            Event::Gone { conn } => {
                // Its questions go with it; a dialog up for it comes down.
                self.conns.remove(&conn);
                self.queue.retain(|p| p.conn != conn);
                if self.dialog.as_ref().is_some_and(|d| d.req.conn == conn) {
                    self.dialog = None;
                    self.next(qh);
                }
            }
            Event::Choose { conn, req, kind } => {
                let Some(c) = self.conns.get(&conn) else { return };
                let pending = Pending { conn, req, app: c.app.clone(), uid: c.uid, kind };
                self.queue.push_back(pending);
                self.next(qh);
            }
            Event::Cancel { conn, req } => {
                self.queue.retain(|p| !(p.conn == conn && p.req == req));
                if self.dialog.as_ref().is_some_and(|d| d.req.conn == conn && d.req.req == req) {
                    self.dialog = None;
                    self.next(qh);
                }
            }
        }
    }

    fn reply(&self, conn: u64, resp: FromFiles) {
        let Some(c) = self.conns.get(&conn) else { return };
        if let Err(err) = seq::send(&c.out, &resp, &[]) {
            drv_os::say!("drv-files: to {}: {err}", c.app);
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
        let typed = match &req.kind {
            Ask::Save { name } => {
                name.chars().filter(|c| !c.is_control() && *c != '/').take(MAX_TYPED).collect()
            }
            Ask::Open => String::new(),
        };
        let surface = self.ui.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("drv-files"),
            None,
        );
        layer.set_size(WIDTH, HEIGHT);
        layer.set_anchor(Anchor::empty());
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        let mut dialog = Dialog {
            req,
            layer,
            size: (0, 0),
            dir: PathBuf::new(),
            entries: Vec::new(),
            selected: None,
            typed,
            note: None,
        };
        self.list(&mut dialog);
        self.dialog = Some(dialog);
    }

    /// Fills `d.entries` with its directory: files and directories, no dotfiles, and no
    /// symlinks, which could lead out of the tree.
    fn list(&self, d: &mut Dialog) {
        d.entries.clear();
        d.note = None;
        match fs::read_dir(self.files.join(&d.dir)) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let Ok(name) = e.file_name().into_string() else { continue };
                    if name.starts_with('.') || !(ft.is_dir() || ft.is_file()) {
                        continue;
                    }
                    d.entries.push(Entry { name, dir: ft.is_dir() });
                }
                d.entries.sort_by(|a, b| {
                    b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
            }
            Err(err) => d.note = Some(format!("cannot list /{}: {err}", d.dir.display())),
        }
        d.reset_selection();
    }

    fn relist(&mut self) {
        if let Some(mut d) = self.dialog.take() {
            self.list(&mut d);
            self.dialog = Some(d);
            self.draw();
        }
    }

    fn finish(&mut self, qh: &QueueHandle<Self>, resp: FromFiles) {
        if let Some(d) = self.dialog.take() {
            self.reply(d.req.conn, resp);
        }
        self.next(qh);
    }

    fn enter(&mut self, qh: &QueueHandle<Self>) {
        let Some(d) = self.dialog.as_mut() else { return };
        let shown = d.shown();
        let picked = d.selected.and_then(|i| shown.get(i)).map(|&i| (d.entries[i].name.clone(), d.entries[i].dir));
        match picked {
            Some((name, true)) => {
                d.dir.push(name);
                if !d.saving() {
                    d.typed.clear();
                }
                self.relist();
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
        let (req, uid, app) = (d.req.req, d.req.uid, d.req.app.clone());
        let id = self.grants.lock().unwrap().add(Grant { uid, name: name.clone(), file, write });
        let doc = self.docs.join(id.to_string()).join(&name);
        drv_os::say!(
            "drv-files: {app} (uid {uid}) gets {} as {}{}",
            path.display(),
            doc.display(),
            if write { ", writable" } else { "" }
        );
        self.finish(qh, FromFiles::Chosen { req, paths: vec![doc.to_string_lossy().into_owned()] });
    }

    fn up(&mut self) {
        let Some(d) = self.dialog.as_mut() else { return };
        if d.dir.pop() {
            self.relist();
        }
    }

    fn draw(&mut self) {
        let Some(d) = &self.dialog else { return };
        let (width, height) = d.size;
        if width == 0 || height == 0 {
            return;
        }
        let shown = d.shown();
        let surface = d.layer.wl_surface().clone();
        if let Err(err) = self.ui.draw(&surface, width, height, |p| paint(p, d, &shown)) {
            drv_os::say!("drv-files: {err}");
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
    let verb = if d.saving() { "save" } else { "open" };
    p.text(PAD, PAD, 20., &format!("{} wants to {verb} a file", d.req.app), Align::Left, fg);
    p.text(PAD, PAD + 40., 15., &format!("/{}", d.dir.display()), Align::Left, blue);

    let top = PAD + 72.;
    let bottom = p.height - PAD - 2. * ROW;
    let rows = ((bottom - top) / ROW).max(0.) as usize;
    if shown.is_empty() {
        p.text(PAD, top, 16., "nothing here", Align::Left, dim);
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
    }

    let (label, cursor) = if d.saving() {
        ("name", if d.selected.is_none() { "_" } else { "" })
    } else {
        ("filter", "")
    };
    p.text(PAD, bottom + 6., 16., &format!("{label}: {}{cursor}", d.typed), Align::Left, fg);
    let (hint, color) = match &d.note {
        Some(note) => (note.as_str(), (1., 0.6, 0.5, 1.)),
        None => ("Enter choose   Backspace up   Esc cancel", dim),
    };
    p.text(PAD, p.height - PAD - ROW + 8., 13., hint, Align::Left, color);
}

impl Client for App {
    fn ui(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn scale_changed(&mut self, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface) {
        self.draw();
    }

    fn key(&mut self, qh: &QueueHandle<Self>, event: KeyEvent) {
        let Some(d) = self.dialog.as_mut() else { return };
        match event.keysym {
            Keysym::Escape => {
                let req = d.req.req;
                drv_os::say!("drv-files: {} (uid {}): cancelled", d.req.app, d.req.uid);
                self.finish(qh, FromFiles::Cancelled { req });
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
                } else {
                    self.up();
                }
            }
            Keysym::Left => self.up(),
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control() && c != '/');
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
    fn closed(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface) {
        // The compositor took it down: the app hears a cancel.
        if let Some(d) = self.dialog.take() {
            self.reply(d.req.conn, FromFiles::Cancelled { req: d.req.req });
        }
        self.next(qh);
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
        if let Some(d) = self.dialog.as_mut() {
            d.size = (if w == 0 { WIDTH } else { w }, if h == 0 { HEIGHT } else { h });
        }
        self.draw();
    }
}

drv_ui::client!(App);

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// One app's connection, on its own thread: the hello, then its requests to the event loop,
/// which answers on a dup.
fn conn(tx: Sender<Event>, sock: OwnedFd, uid: u32, app: String) {
    match seq::recv::<ToFiles>(&sock) {
        Ok((ToFiles::Hello { version }, _)) if version == wire::VERSION => {}
        Ok((other, _)) => {
            drv_os::say!("drv-files: {app} opened with {other:?}, not version {}", wire::VERSION);
            return;
        }
        Err(err) => {
            drv_os::say!("drv-files: {app}: {err}");
            return;
        }
    }
    if let Err(err) = seq::send(&sock, &FromFiles::Hello { version: wire::VERSION }, &[]) {
        drv_os::say!("drv-files: to {app}: {err}");
        return;
    }
    let out = match sock.try_clone() {
        Ok(out) => out,
        Err(err) => {
            drv_os::say!("drv-files: dup: {err}");
            return;
        }
    };
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    if tx.send(Event::New { conn, out, app: app.clone(), uid }).is_err() {
        return;
    }
    loop {
        let ev = match seq::recv::<ToFiles>(&sock) {
            Ok((ToFiles::Choose { req, kind }, _)) => Event::Choose { conn, req, kind },
            Ok((ToFiles::Cancel { req }, _)) => Event::Cancel { conn, req },
            Ok((ToFiles::Hello { .. }, _)) => continue,
            Err(err) => {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    drv_os::say!("drv-files: {app}: {err}");
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
    let args = Args::parse();
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let wayland = fds.socket("wayland", Kind::Stream).map_err(|e| e.to_string())?;
    let fuse = fds.file("fuse").map_err(|e| e.to_string())?;
    let listener = fds.listener_of("listener", Kind::SeqPacket).map_err(|e| e.to_string())?;

    // The mount is up before we are; serving it is its own thread, and losing it ends us.
    // Served first: anything touching the mount (the forker cloning the doors for an app)
    // blocks until we answer, and drv-appd, which we wait for next, needs the forker.
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
            drv_os::say!("drv-files: the documents mount: {err}");
        }
        process::exit(1);
    });
    let door = Arc::new(Door::open().map_err(|e| format!("drv-appd: {e}"))?);

    let drv_ui::Session {
        globals,
        qh,
        mut event_loop,
        ..
    } = drv_ui::session::<App>(wayland)?;
    let ui = drv_ui::ui!(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| format!("layer shell: {e}"))?;
    let mut app = App {
        ui,
        layer_shell,
        files: args.files,
        docs: args.docs,
        grants,
        conns: HashMap::new(),
        queue: VecDeque::new(),
        dialog: None,
    };

    let (tx, rx) = channel::channel::<Event>();
    let ev_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(rx, move |ev, _, app: &mut App| {
            if let channel::Event::Msg(ev) = ev {
                app.on_event(&ev_qh, ev);
            }
        })
        .map_err(|e| format!("event loop: {e}"))?;
    thread::spawn(move || {
        let err = door.serve(listener, "drv-files", move |sock, uid, policy| {
            conn(tx.clone(), sock, uid, policy.name.clone())
        });
        drv_os::say!("drv-files: the socket: {err:?}");
        process::exit(1);
    });

    drv_ui::warm_fonts();
    drv_ui::seal_with("drv-files", |allow| {
        allow.write_files()?;
        // The apps' connections come in, and drv-appd is asked who they are.
        allow.accept();
        allow.socket(rustix::net::AddressFamily::UNIX.as_raw() as _)?;
        allow.connect_unix().map(|_| ())
    })?;

    loop {
        if let Err(err) = event_loop.dispatch(None, &mut app) {
            return Err(format!("event loop: {err}"));
        }
    }
}

fn main() {
    if let Err(err) = run() {
        drv_os::say!("drv-files: {err}");
        process::exit(1);
    }
}
