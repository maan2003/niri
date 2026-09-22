//! Headless outputs for agent desktops and compositor tests.
//! Composition is on-demand through the desktop capture endpoint; application
//! frame callbacks continue independently of screenshot consumers.

use std::mem;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use niri_config::OutputName;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::Size;
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::niri::{Niri, RedrawState};
use crate::render_helpers::{resources, shaders, RenderTarget};
use crate::utils::{get_monotonic_time, logical_output};

pub struct Headless {
    pub video: Option<crate::desktop::video::Video>,
    renderer: Option<GlesRenderer>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
}

impl Headless {
    pub fn new() -> Self {
        Self {
            video: None,
            renderer: None,
            ipc_outputs: Default::default(),
        }
    }

    pub fn init(&mut self, _niri: &mut Niri) {}

    pub fn add_renderer(&mut self) -> anyhow::Result<()> {
        if self.renderer.is_some() {
            error!("add_renderer: renderer must not already exist");
            return Ok(());
        }

        let mut renderer = unsafe {
            let display =
                EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
            let context = EGLContext::new(&display).context("error creating EGL context")?;
            GlesRenderer::new(context).context("error creating renderer")?
        };

        resources::init(&mut renderer);
        shaders::init(&mut renderer);

        self.renderer = Some(renderer);
        Ok(())
    }

    pub fn add_output(&mut self, niri: &mut Niri, n: u8, size: (u16, u16)) {
        let connector = format!("headless-{n}");
        let make = "niri".to_string();
        let model = "headless".to_string();
        let serial = n.to_string();

        let output = Output::new(
            connector.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial.clone(),
            },
        );

        let mode = Mode {
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector,
            make: Some(make),
            model: Some(model),
            serial: Some(serial),
        });

        niri.add_output(output.clone(), None, false);

        let physical_properties = output.physical_properties();
        self.ipc_outputs.lock().unwrap().insert(
            OutputId::next(),
            niri_ipc::Output {
                name: output.name(),
                make: physical_properties.make,
                model: physical_properties.model,
                serial: None,
                physical_size: None,
                modes: vec![niri_ipc::Mode {
                    width: size.0,
                    height: size.1,
                    refresh_rate: 60_000,
                    is_preferred: true,
                }],
                current_mode: Some(0),
                vrr_supported: false,
                vrr_enabled: false,
                logical: Some(logical_output(&output)),
            },
        );
    }

    pub fn seat_name(&self) -> String {
        "headless".to_owned()
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        self.renderer.as_mut().map(f)
    }

    pub fn render(&mut self, niri: &mut Niri, output: &Output) -> RenderResult {
        if let (Some(video), Some(renderer)) = (&mut self.video, &mut self.renderer) {
            if let Err(error) = video.render(niri, renderer, output) {
                warn!("desktop composition: {error:#}");
            }
        }
        // Frame callbacks use primary-scanout visibility even without a physical
        // scanout. Empty states leave every client on the one-second fallback.
        // Compute visibility without drawing or reading pixels when nobody watches.
        let states = if let Some(renderer) = self.renderer.as_mut() {
            let elements = niri.render::<GlesRenderer>(renderer, output, true, RenderTarget::Output);
            let (_, states) = OutputDamageTracker::from_output(output)
                .damage_output(0, &elements)
                .expect("headless output has a mode");
            states
        } else {
            Default::default()
        };
        niri.update_primary_scanout_output(output, &states);
        let mut presentation_feedbacks = niri.take_presentation_feedbacks(output, &states);
        presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
            get_monotonic_time(),
            Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );

        let output_state = niri.output_state.get_mut(output).unwrap();
        assert!(matches!(output_state.redraw_state, RedrawState::Queued));
        output_state.redraw_state = RedrawState::WaitingForVBlank { redraw_needed: false };

        // A virtual vblank coalesces commits and prevents callback-only clients
        // from spinning. No recurring timer is armed for an idle output.
        let output = output.clone();
        let interval = std::time::Duration::from_nanos(
            1_000_000_000_000 / output.current_mode().unwrap().refresh as u64,
        );
        niri.event_loop
            .insert_source(
                calloop::timer::Timer::from_duration(interval),
                move |_, _, state| {
                    let Some(output_state) = state.niri.output_state.get_mut(&output) else {
                        return calloop::timer::TimeoutAction::Drop;
                    };
                    let RedrawState::WaitingForVBlank { redraw_needed } =
                        mem::replace(&mut output_state.redraw_state, RedrawState::Idle)
                    else {
                        unreachable!()
                    };
                    output_state.frame_callback_sequence =
                        output_state.frame_callback_sequence.wrapping_add(1);
                    if redraw_needed || output_state.unfinished_animations_remain {
                        state.niri.queue_redraw(&output);
                    } else {
                        state.niri.send_frame_callbacks(&output);
                    }
                    calloop::timer::TimeoutAction::Drop
                },
            )
            .expect("insert headless frame timer");

        RenderResult::Submitted
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.renderer
            .as_mut()
            .is_some_and(|renderer| renderer.import_dmabuf(dmabuf, None).is_ok())
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}
