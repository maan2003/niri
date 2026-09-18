//! The lock screen, a supervisor service in the compositor's group. It only draws and
//! collects the PIN: the compositor decides when the session is locked, and `drv-authd` tells
//! the compositor to unlock on a correct PIN. Both connections come down the supervisor's
//! wire. After every `finished` we ask to lock again; the compositor holds that request until
//! the session locks.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process;
use std::time::Duration;


use pangocairo::cairo::{Format, ImageSurface};
use pangocairo::pango::{Alignment, FontDescription};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};
use zeroize::Zeroize;

const MAX_PIN: usize = 32;

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor_state: CompositorState,
    shm: Shm,
    pool: SlotPool,
    session_lock_state: SessionLockState,
    session_lock: Option<SessionLock>,
    surfaces: HashMap<wl_output::WlOutput, Surface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pin: Vec<u8>,
    message: String,
    /// Waiting for the compositor to finish us after a correct PIN.
    granted: bool,
    granted_ticks: u32,
    /// Our connection to drv-authd, from the supervisor's wire; a restarted daemon arrives
    /// there as a new one.
    auth: Option<OwnedFd>,
}

const NO_AUTH: &str = "Auth daemon unavailable";

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
        let surface = self.compositor_state.create_surface(qh);
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
        let stride = width as i32 * 4;
        let text = self.text();
        let (buffer, canvas) = match self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("drv-lock: buffer: {err}");
                return;
            }
        };
        render(canvas, width, height, stride, &text);
        let wl_surface = surface.lock_surface.wl_surface();
        wl_surface.damage_buffer(0, 0, width as i32, height as i32);
        if let Err(err) = buffer.attach_to(wl_surface) {
            eprintln!("drv-lock: attach: {err}");
        }
        wl_surface.commit();
    }

    fn text(&self) -> Text {
        let dots = "●".repeat(self.pin.len());
        let prompt = if self.granted {
            "Unlocking…".to_owned()
        } else if self.pin.is_empty() {
            "Enter PIN".to_owned()
        } else {
            dots
        };
        Text {
            prompt,
            message: self.message.clone(),
        }
    }

    /// Asks to lock; the compositor answers `locked` when the session is (or gets) locked.
    fn relock(&mut self, qh: &QueueHandle<Self>) {
        match self.session_lock_state.lock(qh) {
            Ok(lock) => self.session_lock = Some(lock),
            Err(err) => {
                eprintln!("drv-lock: no session-lock global: {err}");
                process::exit(1);
            }
        }
    }

    fn submit(&mut self) {
        if self.pin.is_empty() || self.granted {
            return;
        }
        let Some(auth) = &self.auth else {
            self.pin.zeroize();
            self.pin.clear();
            self.message = NO_AUTH.to_owned();
            self.draw_all();
            return;
        };
        let reply = drv_auth::verify(auth, &self.pin);
        self.pin.zeroize();
        self.pin.clear();
        match reply {
            Ok(drv_auth::Response::Granted) => {
                self.granted = true;
                self.message.clear();
            }
            Ok(drv_auth::Response::Denied { retry_after_ms }) => {
                self.message = if retry_after_ms == 0 {
                    "Wrong PIN".to_owned()
                } else {
                    format!(
                        "Wrong PIN, try again in {} s",
                        retry_after_ms.div_ceil(1000)
                    )
                };
            }
            Ok(other) => self.message = format!("Auth failed: {other:?}"),
            Err(err) => {
                // drv-authd is gone; so is the set, us included, in a moment.
                eprintln!("drv-lock: auth daemon unreachable: {err}; exiting");
                process::exit(1);
            }
        }
        self.draw_all();
    }
}

struct Text {
    prompt: String,
    message: String,
}

fn render(canvas: &mut [u8], width: u32, height: u32, stride: i32, text: &Text) {
    let mut surface =
        ImageSurface::create(Format::ARgb32, width as i32, height as i32).expect("cairo surface");
    {
        let cr = pangocairo::cairo::Context::new(&surface).expect("cairo context");
        cr.set_source_rgb(0.08, 0.09, 0.12);
        cr.paint().unwrap();

        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_alignment(Alignment::Center);
        layout.set_width(width as i32 * pangocairo::pango::SCALE);

        let draw_line = |y: f64, size: f64, s: &str, alpha: f64| {
            let mut font = FontDescription::new();
            font.set_family("sans");
            font.set_absolute_size(size * pangocairo::pango::SCALE as f64);
            layout.set_font_description(Some(&font));
            layout.set_text(s);
            cr.set_source_rgba(0.93, 0.93, 0.95, alpha);
            cr.move_to(0., y);
            pangocairo::functions::show_layout(&cr, &layout);
        };

        let mid = height as f64 / 2.;
        draw_line(mid - 120., 36., "Locked", 0.7);
        draw_line(mid - 30., 44., &text.prompt, 1.);
        draw_line(mid + 50., 22., &text.message, 0.9);
    }
    surface.flush();
    let src_stride = surface.stride() as usize;
    let data = surface.data().expect("surface data");
    for y in 0..height as usize {
        let row = &data[y * src_stride..y * src_stride + width as usize * 4];
        canvas[y * stride as usize..y * stride as usize + width as usize * 4].copy_from_slice(row);
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, _lock: SessionLock) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
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
        self.pin.zeroize();
        self.pin.clear();
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

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
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
                self.pin.zeroize();
                self.pin.clear();
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

    fn repeat_key(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.press_key(conn, qh, keyboard, serial, event);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(kb) => self.keyboard = Some(kb),
                Err(err) => eprintln!("drv-lock: keyboard: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if !self.surfaces.contains_key(&output) {
            self.create_surface(qh, output);
        }
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.remove(&output);
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_registry!(App);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
smithay_client_toolkit::delegate_dispatch2!(App);

fn main() {
    // Our peers, from the supervisor: the compositor (our Wayland connection) and drv-authd.
    let (compositor, auth) = match drv_os::fds::take().and_then(|mut fds| {
        let compositor = fds.socket("compositor", drv_os::fds::Kind::Stream)?;
        let auth = fds.socket("auth", drv_os::fds::Kind::SeqPacket)?;
        Ok((compositor, auth))
    }) {
        Ok(peers) => peers,
        Err(err) => {
            eprintln!("drv-lock: fds from the supervisor: {err} (the locker runs under drv-supervisor)");
            process::exit(1);
        }
    };
    let auth = Some(auth);
    let conn = Connection::from_socket(UnixStream::from(compositor)).expect("wayland connection");
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry");
    let qh: QueueHandle<App> = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("event loop");
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .expect("wayland source");

    let shm = Shm::bind(&globals, &qh).expect("wl_shm");
    let pool = SlotPool::new(4096, &shm).expect("shm pool");
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor_state: CompositorState::bind(&globals, &qh).expect("wl_compositor"),
        shm,
        pool,
        session_lock_state: SessionLockState::new(&globals, &qh),
        session_lock: None,
        surfaces: HashMap::new(),
        keyboard: None,
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
        .insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            |_, _, app: &mut App| {
                if app.granted {
                    app.granted_ticks += 1;
                    if app.granted_ticks > 10 {
                        eprintln!("drv-lock: no finish after grant");
                        app.granted = false;
                        app.granted_ticks = 0;
                        app.message = "Unlock did not go through, try again".to_owned();
                        app.draw_all();
                    }
                }
                TimeoutAction::ToDuration(Duration::from_secs(1))
            },
        )
        .expect("timer");

    // Fontconfig and Pango load their configuration and caches on the first render: do that
    // now, on a scrap buffer, so from here on they only need to read font files.
    let mut scrap = [0u8; 16];
    render(&mut scrap, 2, 2, 8, &Text { prompt: String::new(), message: String::new() });
    if drv_os::seccomp::enabled() {
        let mut allow = drv_os::seccomp::Allowlist::base().expect("seccomp baseline");
        allow.read_files().expect("seccomp rules");
        allow.apply("drv-lock").expect("seccomp filter");
        eprintln!("drv-lock: seccomp: syscall allowlist applied");
    }

    loop {
        if let Err(err) = event_loop.dispatch(None, &mut app) {
            // The compositor went away; the supervisor restarts the whole group, us included.
            eprintln!("drv-lock: connection lost: {err}; exiting");
            process::exit(0);
        }
    }
}
