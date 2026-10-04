//! "Screen is being shared" / "Microphone: app" / "Camera: app" indicator.
//!
//! The compositor draws it above everything on every output while a screencast session is
//! live or an app holds the microphone or the camera (drv-cast says which), and never into
//! the cast itself. Apps cannot draw or cover it, so the human at the screen always knows,
//! and sees the key that ends all of it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use niri_config::{Action, Config, ModKey};
use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::Point;

use super::hotkey_overlay::key_name;
use super::paint::{Paint, Text};
use crate::gpu::remote::{RemoteRenderer, RemoteTexture};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::output_size;

const PADDING: i32 = 8;
const MARGIN: i32 = 12;
const DOT: i32 = 10;
const FONT: f32 = 13.;
const BORDER: i32 = 3;

pub struct CastIndicator {
    /// Live screencast sessions; shown while there are any.
    sessions: usize,
    /// Apps holding the microphone, by name; drv-cast's word.
    mic: Vec<String>,
    /// Apps holding the camera, by name.
    camera: Vec<String>,
    config: Rc<RefCell<Config>>,
    mod_key: ModKey,
    buffers: RefCell<HashMap<NotNan<f64>, Option<TextureBuffer<RemoteTexture>>>>,
}

impl CastIndicator {
    pub fn new(config: Rc<RefCell<Config>>, mod_key: ModKey) -> Self {
        Self {
            sessions: 0,
            mic: Vec::new(),
            camera: Vec::new(),
            config,
            mod_key,
            buffers: RefCell::new(HashMap::new()),
        }
    }

    /// Returns true when the indicator changed and outputs need a redraw.
    pub fn set_sessions(&mut self, sessions: usize) -> bool {
        if self.sessions == sessions {
            return false;
        }
        self.sessions = sessions;
        self.buffers.borrow_mut().clear();
        true
    }

    /// Returns true when the indicator changed and outputs need a redraw.
    pub fn set_devices(&mut self, mic: Vec<String>, camera: Vec<String>) -> bool {
        if self.mic == mic && self.camera == camera {
            return false;
        }
        self.mic = mic;
        self.camera = camera;
        self.buffers.borrow_mut().clear();
        true
    }

    fn shown(&self) -> bool {
        self.sessions > 0 || !self.mic.is_empty() || !self.camera.is_empty()
    }

    pub fn on_hotkey_config_updated(&mut self, mod_key: ModKey) {
        self.mod_key = mod_key;
        self.buffers.borrow_mut().clear();
    }

    fn text(&self) -> String {
        let mut lines = Vec::new();
        if self.sessions > 0 {
            let mut line = String::from("Screen is being shared");
            if self.sessions > 1 {
                line.push_str(&format!(" ({} sessions)", self.sessions));
            }
            lines.push(line);
        }
        if !self.mic.is_empty() {
            lines.push(format!("Microphone: {}", self.mic.join(", ")));
        }
        if !self.camera.is_empty() {
            lines.push(format!("Camera: {}", self.camera.join(", ")));
        }

        let config = self.config.borrow();
        let stop = config
            .binds
            .0
            .iter()
            .find(|bind| bind.action == Action::StopAllCasts);
        if let Some(bind) = stop {
            let key = key_name(false, self.mod_key, &bind.key);
            if lines.len() == 1 {
                lines[0].push_str(&format!("    {key} to stop"));
            } else {
                lines.push(format!("{key} to stop all"));
            }
        }
        lines.join("\n")
    }

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        if !self.shown() {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);

        let mut buffers = self.buffers.borrow_mut();
        let buffer = buffers
            .entry(NotNan::new(scale).unwrap())
            .or_insert_with(|| render(renderer.as_remote_renderer(), scale, &self.text()).ok());
        let buffer = buffer.clone()?;

        let size = buffer.logical_size();
        let x = (output_size.w - size.w - f64::from(MARGIN)).max(0.);
        let y = f64::from(MARGIN);
        let location = Point::from((x, y))
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            1.,
            None,
            None,
            Kind::Unspecified,
        );
        Some(PrimaryGpuTextureRenderElement(elem))
    }
}

fn render(
    renderer: &mut RemoteRenderer,
    scale: f64,
    text: &str,
) -> anyhow::Result<TextureBuffer<RemoteTexture>> {
    paint(scale, text)?.render(renderer, scale)
}

fn paint(scale: f64, text: &str) -> anyhow::Result<Paint> {
    let _span = tracy_client::span!("cast_indicator::render");
    let text = Text::new(text, FONT)?;
    let (text_width, text_height) = text.size();
    let width = text_width + PADDING * 3 + DOT;
    let height = text_height + PADDING * 2;
    let mut paint = Paint::new(width, height);
    paint.fill([0.1, 0.1, 0.1, 1.]);
    let r = DOT as f32 / 2.;
    paint.circle(
        PADDING as f32 + r,
        height as f32 / 2.,
        r,
        [1., 0.25, 0.25, 1.],
    );
    paint.text(&text, (PADDING * 2 + DOT) as f32, PADDING as f32);
    paint.border(
        ((BORDER as f64 / 2. * scale).round() * 2. / scale) as f32,
        [1., 0.25, 0.25, 1.],
    );
    Ok(paint)
}

#[cfg(test)]
pub(super) fn test_paint(scale: f64, text: &str) -> anyhow::Result<Paint> {
    paint(scale, text)
}
