//! What the set's little windows share (the locker, the menu): a Wayland connection on the
//! fd the supervisor handed over, the client-toolkit boilerplate, text drawn into shm buffers
//! with Pango, and sealing with seccomp once the fonts are warm. A window here is one process
//! with one connection: nothing to find, nothing to fork.

pub use pangocairo;
pub use smithay_client_toolkit as sctk;
pub use wayland_client;

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use pangocairo::cairo::{Context, Format, ImageSurface};
use pangocairo::pango::{Alignment, FontDescription, SCALE};
use sctk::compositor::CompositorState;
use sctk::output::OutputState;
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::registry::RegistryState;
use sctk::seat::keyboard::KeyEvent;
use sctk::seat::SeatState;
use sctk::shm::slot::SlotPool;
use sctk::shm::Shm;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};

/// The toolkit state every client of ours carries; [`ui!`] builds it.
pub struct Ui {
    pub registry_state: RegistryState,
    pub output_state: OutputState,
    pub seat_state: SeatState,
    pub compositor_state: CompositorState,
    pub shm: Shm,
    pub pool: SlotPool,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The surface of ours the keyboard is on, if any: a client with several surfaces
    /// (drv-shell) routes keys by it.
    pub focus: Option<wl_surface::WlSurface>,
}

impl Ui {
    /// Paints a fresh buffer of `width` x `height` with `draw`, attaches it and commits.
    pub fn draw(
        &mut self,
        surface: &wl_surface::WlSurface,
        width: u32,
        height: u32,
        draw: impl FnOnce(&Painter),
    ) -> Result<(), String> {
        let stride = width as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
            .map_err(|e| format!("buffer: {e}"))?;
        paint(canvas, width, height, stride, draw)?;
        surface.damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(surface).map_err(|e| format!("attach: {e}"))?;
        surface.commit();
        Ok(())
    }
}

/// What the app adds on top of [`Ui`]: the toolkit's other handlers come from [`client!`].
pub trait Client: Sized + 'static {
    fn ui(&mut self) -> &mut Ui;
    /// A key press or repeat on our keyboard.
    fn key(&mut self, _qh: &QueueHandle<Self>, _event: KeyEvent) {}
    fn new_output(&mut self, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn output_gone(&mut self, _output: wl_output::WlOutput) {}
}

/// The connection, its globals, and an event loop already dispatching it.
pub struct Session<A: 'static> {
    pub conn: Connection,
    pub globals: GlobalList,
    pub qh: QueueHandle<A>,
    pub event_loop: EventLoop<'static, A>,
}

/// Connects on `fd`, the socket the supervisor handed us.
pub fn session<A>(fd: OwnedFd) -> Result<Session<A>, String>
where
    A: Dispatch<wl_registry::WlRegistry, GlobalListContents> + 'static,
{
    let conn = Connection::from_socket(UnixStream::from(fd))
        .map_err(|e| format!("wayland connection: {e}"))?;
    let (globals, event_queue) =
        registry_queue_init::<A>(&conn).map_err(|e| format!("wayland registry: {e}"))?;
    let qh = event_queue.handle();
    let event_loop = EventLoop::try_new().map_err(|e| format!("event loop: {e}"))?;
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|e| format!("event loop: {e}"))?;
    Ok(Session {
        conn,
        globals,
        qh,
        event_loop,
    })
}

/// Builds a [`Ui`] from the globals: `ui!(&globals, &qh)`. A macro because the toolkit's
/// constructors are bound on the app type's dispatch impls, which [`client!`] provides.
#[macro_export]
macro_rules! ui {
    ($globals:expr, $qh:expr) => {{
        let globals = $globals;
        let qh = $qh;
        (|| -> Result<$crate::Ui, String> {
            let shm = $crate::sctk::shm::Shm::bind(globals, qh).map_err(|e| format!("wl_shm: {e}"))?;
            let pool = $crate::sctk::shm::slot::SlotPool::new(4096, &shm)
                .map_err(|e| format!("shm pool: {e}"))?;
            Ok($crate::Ui {
                registry_state: $crate::sctk::registry::RegistryState::new(globals),
                output_state: $crate::sctk::output::OutputState::new(globals, qh),
                seat_state: $crate::sctk::seat::SeatState::new(globals, qh),
                compositor_state: $crate::sctk::compositor::CompositorState::bind(globals, qh)
                    .map_err(|e| format!("wl_compositor: {e}"))?,
                shm,
                pool,
                keyboard: None,
                focus: None,
            })
        })()
    }};
}

/// The toolkit handlers for an app type that implements [`Client`]: seat and keyboard
/// (forwarded to `key`), outputs (forwarded), compositor, shm, registry and the dispatch
/// plumbing. The app adds its shell handler (session lock, layer shell) itself.
#[macro_export]
macro_rules! client {
    ($app:ty) => {
        impl $crate::sctk::seat::SeatHandler for $app {
            fn seat_state(&mut self) -> &mut $crate::sctk::seat::SeatState {
                &mut <Self as $crate::Client>::ui(self).seat_state
            }

            fn new_seat(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: $crate::wayland_client::protocol::wl_seat::WlSeat,
            ) {
            }

            fn new_capability(
                &mut self,
                _: &$crate::wayland_client::Connection,
                qh: &$crate::wayland_client::QueueHandle<Self>,
                seat: $crate::wayland_client::protocol::wl_seat::WlSeat,
                capability: $crate::sctk::seat::Capability,
            ) {
                let ui = <Self as $crate::Client>::ui(self);
                if capability == $crate::sctk::seat::Capability::Keyboard && ui.keyboard.is_none() {
                    match ui.seat_state.get_keyboard(qh, &seat, None) {
                        Ok(kb) => ui.keyboard = Some(kb),
                        Err(err) => drv_os::say!("keyboard: {err}"),
                    }
                }
            }

            fn remove_capability(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: $crate::wayland_client::protocol::wl_seat::WlSeat,
                capability: $crate::sctk::seat::Capability,
            ) {
                if capability == $crate::sctk::seat::Capability::Keyboard {
                    if let Some(kb) = <Self as $crate::Client>::ui(self).keyboard.take() {
                        kb.release();
                    }
                }
            }

            fn remove_seat(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: $crate::wayland_client::protocol::wl_seat::WlSeat,
            ) {
            }
        }

        impl $crate::sctk::seat::keyboard::KeyboardHandler for $app {
            fn enter(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_keyboard::WlKeyboard,
                surface: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: u32,
                _: &[u32],
                _: &[$crate::sctk::seat::keyboard::Keysym],
            ) {
                <Self as $crate::Client>::ui(self).focus = Some(surface.clone());
            }

            fn leave(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_keyboard::WlKeyboard,
                surface: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: u32,
            ) {
                let ui = <Self as $crate::Client>::ui(self);
                if ui.focus.as_ref() == Some(surface) {
                    ui.focus = None;
                }
            }

            fn press_key(
                &mut self,
                _: &$crate::wayland_client::Connection,
                qh: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_keyboard::WlKeyboard,
                _: u32,
                event: $crate::sctk::seat::keyboard::KeyEvent,
            ) {
                <Self as $crate::Client>::key(self, qh, event);
            }

            fn repeat_key(
                &mut self,
                _: &$crate::wayland_client::Connection,
                qh: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_keyboard::WlKeyboard,
                _: u32,
                event: $crate::sctk::seat::keyboard::KeyEvent,
            ) {
                <Self as $crate::Client>::key(self, qh, event);
            }

            fn release_key(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_keyboard::WlKeyboard,
                _: u32,
                _: $crate::sctk::seat::keyboard::KeyEvent,
            ) {
            }

            fn update_modifiers(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_keyboard::WlKeyboard,
                _: u32,
                _: $crate::sctk::seat::keyboard::Modifiers,
                _: $crate::sctk::seat::keyboard::RawModifiers,
                _: u32,
            ) {
            }
        }

        impl $crate::sctk::compositor::CompositorHandler for $app {
            fn scale_factor_changed(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: i32,
            ) {
            }
            fn transform_changed(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: $crate::wayland_client::protocol::wl_output::Transform,
            ) {
            }
            fn frame(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: u32,
            ) {
            }
            fn surface_enter(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: &$crate::wayland_client::protocol::wl_output::WlOutput,
            ) {
            }
            fn surface_leave(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: &$crate::wayland_client::protocol::wl_surface::WlSurface,
                _: &$crate::wayland_client::protocol::wl_output::WlOutput,
            ) {
            }
        }

        impl $crate::sctk::output::OutputHandler for $app {
            fn output_state(&mut self) -> &mut $crate::sctk::output::OutputState {
                &mut <Self as $crate::Client>::ui(self).output_state
            }

            fn new_output(
                &mut self,
                _: &$crate::wayland_client::Connection,
                qh: &$crate::wayland_client::QueueHandle<Self>,
                output: $crate::wayland_client::protocol::wl_output::WlOutput,
            ) {
                <Self as $crate::Client>::new_output(self, qh, output);
            }

            fn update_output(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                _: $crate::wayland_client::protocol::wl_output::WlOutput,
            ) {
            }

            fn output_destroyed(
                &mut self,
                _: &$crate::wayland_client::Connection,
                _: &$crate::wayland_client::QueueHandle<Self>,
                output: $crate::wayland_client::protocol::wl_output::WlOutput,
            ) {
                <Self as $crate::Client>::output_gone(self, output);
            }
        }

        impl $crate::sctk::shm::ShmHandler for $app {
            fn shm_state(&mut self) -> &mut $crate::sctk::shm::Shm {
                &mut <Self as $crate::Client>::ui(self).shm
            }
        }

        impl $crate::sctk::registry::ProvidesRegistryState for $app {
            fn registry(&mut self) -> &mut $crate::sctk::registry::RegistryState {
                &mut <Self as $crate::Client>::ui(self).registry_state
            }
            $crate::sctk::registry_handlers![
                $crate::sctk::output::OutputState,
                $crate::sctk::seat::SeatState
            ];
        }

        $crate::sctk::delegate_registry!($app);
        $crate::wayland_client::delegate_noop!($app: ignore $crate::wayland_client::protocol::wl_buffer::WlBuffer);
        $crate::sctk::delegate_dispatch2!($app);
    };
}

/// Where a line of text starts from.
#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Center,
}

/// A cairo context sized to the buffer, with the few strokes we draw.
pub struct Painter<'a> {
    cr: &'a Context,
    pub width: f64,
    pub height: f64,
}

impl Painter<'_> {
    pub fn fill(&self, r: f64, g: f64, b: f64) {
        self.cr.set_source_rgb(r, g, b);
        let _ = self.cr.paint();
    }

    pub fn rect(&self, x: f64, y: f64, w: f64, h: f64, (r, g, b, a): (f64, f64, f64, f64)) {
        self.cr.set_source_rgba(r, g, b, a);
        self.cr.rectangle(x, y, w, h);
        let _ = self.cr.fill();
    }

    /// One line of sans text at `size` px, laid out from `x` to the right edge.
    pub fn text(&self, x: f64, y: f64, size: f64, s: &str, align: Align, (r, g, b, a): (f64, f64, f64, f64)) {
        let layout = pangocairo::functions::create_layout(self.cr);
        layout.set_alignment(match align {
            Align::Left => Alignment::Left,
            Align::Center => Alignment::Center,
        });
        layout.set_width(((self.width - x) * SCALE as f64) as i32);
        let mut font = FontDescription::new();
        font.set_family("sans");
        font.set_absolute_size(size * SCALE as f64);
        layout.set_font_description(Some(&font));
        layout.set_text(s);
        self.cr.set_source_rgba(r, g, b, a);
        self.cr.move_to(x, y);
        pangocairo::functions::show_layout(self.cr, &layout);
    }
}

/// Paints `canvas` (ARGB, `stride` bytes per row) through `draw`.
pub fn paint(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    stride: i32,
    draw: impl FnOnce(&Painter),
) -> Result<(), String> {
    let mut surface = ImageSurface::create(Format::ARgb32, width as i32, height as i32)
        .map_err(|e| format!("cairo surface: {e}"))?;
    {
        let cr = Context::new(&surface).map_err(|e| format!("cairo context: {e}"))?;
        draw(&Painter {
            cr: &cr,
            width: width as f64,
            height: height as f64,
        });
    }
    surface.flush();
    let src_stride = surface.stride() as usize;
    let data = surface.data().map_err(|e| format!("cairo data: {e}"))?;
    let row_bytes = width as usize * 4;
    for y in 0..height as usize {
        let row = &data[y * src_stride..y * src_stride + row_bytes];
        canvas[y * stride as usize..y * stride as usize + row_bytes].copy_from_slice(row);
    }
    Ok(())
}

/// Fontconfig and Pango load their configuration and caches on the first render: do that on
/// a scrap buffer, so that after [`seal`] they only need to read font files.
pub fn warm_fonts() {
    let mut scrap = [0u8; 16];
    let _ = paint(&mut scrap, 2, 2, 8, |p| {
        p.text(0., 0., 10., "a", Align::Left, (1., 1., 1., 1.))
    });
}

/// The syscall allowlist for a sealed window: the baseline plus reading files (fonts).
pub fn seal(name: &'static str) -> Result<(), String> {
    seal_with(name, |_| Ok(()))
}

/// [`seal`] with `extend` adding what this window needs on top.
pub fn seal_with(
    name: &'static str,
    extend: impl FnOnce(&mut drv_os::seccomp::Allowlist) -> std::io::Result<()>,
) -> Result<(), String> {
    if !drv_os::seccomp::enabled() {
        return Ok(());
    }
    let mut allow = drv_os::seccomp::Allowlist::base().map_err(|e| e.to_string())?;
    allow.read_files().map_err(|e| e.to_string())?;
    extend(&mut allow).map_err(|e| e.to_string())?;
    allow.apply(name).map_err(|e| e.to_string())?;
    drv_os::say!("{name}: seccomp: syscall allowlist applied");
    Ok(())
}
