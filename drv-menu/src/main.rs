//! The app menu, a supervisor service. Its Wayland connection (fd `wayland`), its launch
//! channel to drv-appd (fd `appd`) and a poke line from the compositor (fd `compositor`, a
//! byte per `show-launcher` bind) all come from the supervisor. It draws the launchable names
//! itself; the pick goes down the channel as a name, and drv-appd decides what that name
//! runs. Sealed with seccomp once the fonts are warm.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::process;

use drv_os::fds::Kind;
use drv_policy::PolicyClient;
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

const WIDTH: u32 = 520;
const HEIGHT: u32 = 420;
const ROW: f64 = 30.;
const PAD: f64 = 16.;
const MAX_FILTER: usize = 64;

struct App {
    ui: Ui,
    layer_shell: LayerShell,
    /// The menu while it is up; dropping it takes the surface down.
    layer: Option<LayerSurface>,
    size: (u32, u32),
    appd: PolicyClient,
    apps: Vec<String>,
    filter: String,
    selected: usize,
}

impl App {
    /// The compositor poked us: fresh names, fresh surface.
    fn show(&mut self, qh: &QueueHandle<Self>) {
        if self.layer.is_some() {
            return;
        }
        self.apps = match self.appd.apps() {
            Ok(apps) => apps,
            Err(err) => {
                eprintln!("drv-menu: asking drv-appd for the apps: {err}");
                return;
            }
        };
        self.filter.clear();
        self.selected = 0;
        self.size = (0, 0);
        let surface = self.ui.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("drv-menu"),
            None,
        );
        layer.set_size(WIDTH, HEIGHT);
        layer.set_anchor(Anchor::empty());
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        self.layer = Some(layer);
    }

    fn hide(&mut self) {
        self.layer = None;
    }

    fn matches(&self) -> Vec<String> {
        let filter = self.filter.to_lowercase();
        self.apps
            .iter()
            .filter(|a| a.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    }

    fn choose(&mut self) {
        let name = self.matches().get(self.selected).cloned();
        self.hide();
        let Some(name) = name else {
            return;
        };
        match self.appd.launch(name.clone()) {
            Ok(uid) => eprintln!("drv-menu: launched {name:?} as uid {uid}"),
            Err(err) => eprintln!("drv-menu: launching {name:?}: {err}"),
        }
    }

    fn draw(&mut self) {
        let Some(layer) = &self.layer else {
            return;
        };
        let (width, height) = self.size;
        if width == 0 || height == 0 {
            return;
        }
        let matches = self.matches();
        let filter = self.filter.clone();
        let selected = self.selected;
        let surface = layer.wl_surface().clone();
        if let Err(err) = self.ui.draw(&surface, width, height, |p| {
            paint(p, &filter, &matches, selected)
        }) {
            eprintln!("drv-menu: {err}");
        }
    }
}

fn paint(p: &Painter, filter: &str, matches: &[String], selected: usize) {
    let fg = (0.93, 0.93, 0.95, 1.);
    p.fill(0.08, 0.09, 0.12);
    p.text(PAD, PAD, 20., &format!("> {filter}"), Align::Left, fg);
    let top = PAD + 44.;
    let rows = ((p.height - top - PAD) / ROW).max(0.) as usize;
    if matches.is_empty() {
        p.text(PAD, top, 18., "nothing matches", Align::Left, (0.6, 0.6, 0.65, 1.));
        return;
    }
    // Scroll so the selection stays on screen.
    let first = selected.saturating_sub(rows.saturating_sub(1));
    for (i, name) in matches.iter().enumerate().skip(first).take(rows) {
        let y = top + (i - first) as f64 * ROW;
        if i == selected {
            p.rect(PAD / 2., y - 4., p.width - PAD, ROW, (0.25, 0.35, 0.55, 1.));
        }
        p.text(PAD, y, 18., name, Align::Left, fg);
    }
}

impl Client for App {
    fn ui(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn key(&mut self, _qh: &QueueHandle<Self>, event: KeyEvent) {
        if self.layer.is_none() {
            return;
        }
        match event.keysym {
            Keysym::Escape => self.hide(),
            Keysym::Return | Keysym::KP_Enter => self.choose(),
            Keysym::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.draw();
            }
            Keysym::Down => {
                if self.selected + 1 < self.matches().len() {
                    self.selected += 1;
                }
                self.draw();
            }
            Keysym::BackSpace => {
                self.filter.pop();
                self.selected = 0;
                self.draw();
            }
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control());
                    if printable && self.filter.len() + s.len() <= MAX_FILTER {
                        self.filter.push_str(&s);
                        self.selected = 0;
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
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let wayland = fds.socket("wayland", Kind::Stream).map_err(|e| e.to_string())?;
    let appd = fds.socket("appd", Kind::Stream).map_err(|e| e.to_string())?;
    let compositor = fds.socket("compositor", Kind::Stream).map_err(|e| e.to_string())?;
    let appd = PolicyClient::from_stream(UnixStream::from(appd))
        .map_err(|e| format!("the launch channel: {e}"))?;

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
        layer: None,
        size: (0, 0),
        appd,
        apps: Vec::new(),
        filter: String::new(),
        selected: 0,
    };

    // Each byte from the compositor is one `show-launcher`; a hangup is the compositor gone.
    let poke_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(
            Generic::new(UnixStream::from(compositor), Interest::READ, Mode::Level),
            move |_, sock, app: &mut App| {
                let mut buf = [0u8; 64];
                let mut sock: &UnixStream = sock;
                match sock.read(&mut buf) {
                    Ok(0) => Err(io::Error::other("the compositor hung up")),
                    Ok(_) => {
                        app.show(&poke_qh);
                        Ok(PostAction::Continue)
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(PostAction::Continue),
                    Err(err) => Err(err),
                }
            },
        )
        .map_err(|e| format!("event loop: {e}"))?;

    drv_ui::warm_fonts();
    drv_ui::seal("drv-menu")?;

    loop {
        if let Err(err) = event_loop.dispatch(None, &mut app) {
            return Err(format!("event loop: {err}"));
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("drv-menu: {err}");
        process::exit(1);
    }
}
