//! The host workspace as an overlay: the person's own terminal (the client drv-appd names
//! `host`) is never in the layout. Its windows are kept here, configured to the output's size,
//! and drawn over everything but the lock when toggled, like a better VT switch: one key shows
//! it with the keyboard, the same key puts it away. Hidden windows get no frame callbacks.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Transform;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::utils::output_size;
use crate::utils::send_scale_transform;

#[derive(Default)]
pub struct HostOverlay {
    /// Mapped host toplevels, the most recent last; that one is shown.
    windows: Vec<Window>,
    shown: bool,
}

impl HostOverlay {
    /// The window on screen, if the overlay is up and has one.
    pub fn shown(&self) -> Option<&Window> {
        if self.shown {
            self.windows.last()
        } else {
            None
        }
    }

    pub fn is_shown(&self) -> bool {
        self.shown().is_some()
    }

    /// Shows or hides. Nothing to show: stays hidden.
    pub fn toggle(&mut self) {
        self.shown = !self.shown && !self.windows.is_empty();
    }

    pub fn hide(&mut self) {
        self.shown = false;
    }

    pub fn add(&mut self, window: Window) {
        self.windows.push(window);
    }

    pub fn find(&self, surface: &WlSurface) -> Option<&Window> {
        self.windows
            .iter()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
    }

    /// Takes the window of `surface` out; the overlay hides when the last one goes.
    pub fn remove(&mut self, surface: &WlSurface) -> Option<Window> {
        let i = self
            .windows
            .iter()
            .position(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))?;
        let window = self.windows.remove(i);
        if self.windows.is_empty() {
            self.shown = false;
        }
        Some(window)
    }

    /// The shown window's surface, for the keyboard.
    pub fn focus(&self) -> Option<WlSurface> {
        self.shown().and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()))
    }

    /// Every window sized to `output` (its windows follow the one output they are drawn on).
    pub fn configure_all(&self, output: &Output) {
        for window in &self.windows {
            if let Some(toplevel) = window.toplevel() {
                configure_host_toplevel(toplevel, output);
                toplevel.send_pending_configure();
            }
        }
    }
}

/// The pending state of a host toplevel: the output's size, fullscreen and activated, so the
/// terminal draws no decorations and fills the screen.
pub fn configure_host_toplevel(toplevel: &ToplevelSurface, output: &Output) {
    toplevel.with_pending_state(|state| {
        state.size = Some(output_size(output).to_i32_round());
        state.states.set(xdg_toplevel::State::Fullscreen);
        state.states.set(xdg_toplevel::State::Activated);
    });
    let scale = output.current_scale();
    let transform: Transform = output.current_transform();
    let wl_surface = toplevel.wl_surface();
    with_states(wl_surface, |data| {
        send_scale_transform(wl_surface, data, scale, transform);
    });
}
