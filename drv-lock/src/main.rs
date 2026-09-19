//! The lock screen, a supervisor service. It only draws and collects the PIN: the compositor
//! decides when the session is locked, and `drv-authd` tells the compositor to unlock on a
//! correct PIN. Both connections are supervisor fds. After every `finished` we ask to lock
//! again; the compositor holds that request until the session locks.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::process;
use std::time::Duration;

use drv_ui::sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use drv_ui::sctk::seat::keyboard::{KeyEvent, Keysym};
use drv_ui::sctk::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use drv_ui::wayland_client::protocol::wl_output;
use drv_ui::wayland_client::{Connection, QueueHandle};
use drv_ui::{Align, Client, Painter, Ui};
use zeroize::Zeroize;

const MAX_PIN: usize = 32;

struct App {
    ui: Ui,
    session_lock_state: SessionLockState,
    session_lock: Option<SessionLock>,
    surfaces: HashMap<wl_output::WlOutput, Surface>,
    pin: Vec<u8>,
    message: String,
    /// Waiting for the compositor to finish us after a correct PIN.
    granted: bool,
    granted_ticks: u32,
    /// Our connection to drv-authd.
    auth: OwnedFd,
}

struct Surface {
    lock_surface: SessionLockSurface,
    size: (u32, u32),
}

impl App {
    fn create_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(lock) = &self.session_lock else {
            return;
        };
        if self.surfaces.contains_key(&output) {
            return;
        }
        let surface = self.ui.compositor_state.create_surface(qh);
        let lock_surface = lock.create_lock_surface(surface, &output, qh);
        self.surfaces.insert(
            output,
            Surface {
                lock_surface,
                size: (0, 0),
            },
        );
    }

    fn draw_all(&mut self) {
        let outputs: Vec<_> = self.surfaces.keys().cloned().collect();
        for output in outputs {
            self.draw(&output);
        }
    }

    fn draw(&mut self, output: &wl_output::WlOutput) {
        let Some(surface) = self.surfaces.get(output) else {
            return;
        };
        let (width, height) = surface.size;
        if width == 0 || height == 0 {
            return;
        }
        let prompt = if self.granted {
            "Unlocking…".to_owned()
        } else if self.pin.is_empty() {
            "Enter PIN".to_owned()
        } else {
            "●".repeat(self.pin.len())
        };
        let message = self.message.clone();
        let wl_surface = surface.lock_surface.wl_surface().clone();
        if let Err(err) = self.ui.draw(&wl_surface, width, height, |p| paint(p, &prompt, &message)) {
            drv_os::say!("drv-lock: {err}");
        }
    }

    /// Asks to lock; the compositor answers `locked` when the session is (or gets) locked.
    fn relock(&mut self, qh: &QueueHandle<Self>) {
        match self.session_lock_state.lock(qh) {
            Ok(lock) => self.session_lock = Some(lock),
            Err(err) => {
                drv_os::say!("drv-lock: no session-lock global: {err}");
                process::exit(1);
            }
        }
    }

    fn forget_pin(&mut self) {
        self.pin.zeroize();
        self.pin.clear();
    }

    fn submit(&mut self) {
        if self.pin.is_empty() || self.granted {
            return;
        }
        let reply = drv_auth::verify(&self.auth, &self.pin);
        self.forget_pin();
        match reply {
            Ok(drv_auth::Response::Granted) => {
                self.granted = true;
                self.message.clear();
            }
            Ok(drv_auth::Response::Denied { retry_after_ms }) => {
                self.message = if retry_after_ms == 0 {
                    "Wrong PIN".to_owned()
                } else {
                    format!("Wrong PIN, try again in {} s", retry_after_ms.div_ceil(1000))
                };
            }
            Ok(other) => self.message = format!("Auth failed: {other:?}"),
            Err(err) => {
                // drv-authd is gone; so is the set, us included, in a moment.
                drv_os::say!("drv-lock: auth daemon unreachable: {err}; exiting");
                process::exit(1);
            }
        }
        self.draw_all();
    }
}

fn paint(p: &Painter, prompt: &str, message: &str) {
    p.fill(0.08, 0.09, 0.12);
    let mid = p.height / 2.;
    p.text(0., mid - 120., 36., "Locked", Align::Center, (0.93, 0.93, 0.95, 0.7));
    p.text(0., mid - 30., 44., prompt, Align::Center, (0.93, 0.93, 0.95, 1.));
    p.text(0., mid + 50., 22., message, Align::Center, (0.93, 0.93, 0.95, 0.9));
}

impl Client for App {
    fn ui(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn key(&mut self, _qh: &QueueHandle<Self>, event: KeyEvent) {
        if self.granted {
            return;
        }
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::BackSpace => {
                self.pin.pop();
                self.draw_all();
            }
            Keysym::Escape => {
                self.forget_pin();
                self.message.clear();
                self.draw_all();
            }
            _ => {
                if let Some(s) = event.utf8 {
                    let printable = s.chars().all(|c| !c.is_control());
                    if printable && self.pin.len() + s.len() <= MAX_PIN {
                        self.pin.extend_from_slice(s.as_bytes());
                        self.message.clear();
                        self.draw_all();
                    }
                }
            }
        }
    }

    fn new_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.create_surface(qh, output);
    }

    fn output_gone(&mut self, output: wl_output::WlOutput) {
        self.surfaces.remove(&output);
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, _lock: SessionLock) {
        let outputs: Vec<_> = self.ui.output_state.outputs().collect();
        for output in outputs {
            self.create_surface(qh, output);
        }
    }

    fn finished(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, lock: SessionLock) {
        // The compositor unlocked (or refused): let this lock go, forget the PIN, and ask
        // again so the next locking finds our request waiting.
        self.surfaces.clear();
        if lock.is_locked() {
            lock.unlock();
        }
        self.session_lock = None;
        self.forget_pin();
        self.message.clear();
        self.granted = false;
        self.granted_ticks = 0;
        self.relock(qh);
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
            .surfaces
            .iter()
            .find(|(_, s)| s.lock_surface.wl_surface() == lock_surface.wl_surface())
            .map(|(o, _)| o.clone());
        if let Some(output) = output {
            self.surfaces.get_mut(&output).unwrap().size = configure.new_size;
            self.draw(&output);
        }
    }
}

drv_ui::client!(App);

fn run() -> Result<(), String> {
    // Our peers, from the supervisor: the compositor (our Wayland connection) and drv-authd.
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let compositor = fds.socket("compositor", drv_os::fds::Kind::Stream).map_err(|e| e.to_string())?;
    let auth = fds.socket("auth", drv_os::fds::Kind::SeqPacket).map_err(|e| e.to_string())?;
    let drv_ui::Session {
        globals,
        qh,
        mut event_loop,
        ..
    } = drv_ui::session::<App>(compositor)?;
    let ui = drv_ui::ui!(&globals, &qh)?;
    let mut app = App {
        ui,
        session_lock_state: SessionLockState::new(&globals, &qh),
        session_lock: None,
        surfaces: HashMap::new(),
        pin: Vec::new(),
        message: String::new(),
        granted: false,
        granted_ticks: 0,
        auth,
    };
    app.relock(&qh);

    // If the compositor never finishes us after a grant, take the PIN again.
    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app: &mut App| {
            if app.granted {
                app.granted_ticks += 1;
                if app.granted_ticks > 10 {
                    drv_os::say!("drv-lock: no finish after grant");
                    app.granted = false;
                    app.granted_ticks = 0;
                    app.message = "Unlock did not go through, try again".to_owned();
                    app.draw_all();
                }
            }
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| format!("timer: {e}"))?;

    drv_ui::warm_fonts();
    drv_ui::seal("drv-lock")?;

    loop {
        if let Err(err) = event_loop.dispatch(None, &mut app) {
            // The compositor went away; the supervisor restarts the whole set, us included.
            return Err(format!("connection lost: {err}"));
        }
    }
}

fn main() {
    if let Err(err) = run() {
        drv_os::say!("drv-lock: {err}");
        process::exit(1);
    }
}
