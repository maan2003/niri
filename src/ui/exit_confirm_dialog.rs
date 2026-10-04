use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use niri_config::Config;
use ordered_float::NotNan;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::Point;

use super::paint::{Paint, Text, TextAlign, TextOptions};
use crate::animation::{Animation, Clock};
use crate::gpu::remote::RemoteTexture;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::output_size;

const KEY_NAME: &str = "Enter";
const PADDING: i32 = 16;
const FONT: f32 = 14.;
const BORDER: i32 = 8;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];

pub struct ExitConfirmDialog {
    state: State,
    scene: Option<Paint>,
    buffers: RefCell<HashMap<NotNan<f64>, Option<TextureBuffer<RemoteTexture>>>>,

    clock: Clock,
    config: Rc<RefCell<Config>>,
}

niri_render_elements! {
    ExitConfirmDialogRenderElement => {
        Texture = RescaleRenderElement<PrimaryGpuTextureRenderElement>,
        SolidColor = SolidColorRenderElement,
    }
}

struct OutputData {
    backdrop: SolidColorBuffer,
}

enum State {
    Hidden,
    Showing(Animation),
    Visible,
    Hiding(Animation),
}

impl ExitConfirmDialog {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        let scene = match render() {
            Ok(x) => Some(x),
            Err(err) => {
                warn!("error creating the exit confirm dialog: {err:?}");
                None
            }
        };

        Self {
            state: State::Hidden,
            scene,
            buffers: RefCell::new(HashMap::new()),
            clock,
            config,
        }
    }

    pub fn can_show(&self) -> bool {
        self.scene.is_some()
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.exit_confirmation_open_close.0,
        )
    }

    fn value(&self) -> f64 {
        match &self.state {
            State::Hidden => 0.,
            State::Showing(anim) | State::Hiding(anim) => anim.value(),
            State::Visible => 1.,
        }
    }

    /// Returns true if the dialog will be shown (even if it is already shown).
    pub fn show(&mut self) -> bool {
        if !self.can_show() {
            return false;
        }

        if self.is_open() {
            return true;
        }

        self.state = State::Showing(self.animation(self.value(), 1.));
        true
    }

    /// Returns true if started the hide animation.
    pub fn hide(&mut self) -> bool {
        if !self.is_open() {
            return false;
        }

        self.state = State::Hiding(self.animation(self.value(), 0.));
        true
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Visible)
    }

    pub fn advance_animations(&mut self) {
        match &mut self.state {
            State::Hidden => (),
            State::Showing(anim) => {
                if anim.is_done() {
                    self.state = State::Visible;
                }
            }
            State::Visible => (),
            State::Hiding(anim) => {
                if anim.is_clamped_done() {
                    self.state = State::Hidden;
                }
            }
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_))
    }

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
        push: &mut dyn FnMut(ExitConfirmDialogRenderElement),
    ) {
        let (value, clamped_value) = match &self.state {
            State::Hidden => return,
            State::Showing(anim) | State::Hiding(anim) => (anim.value(), anim.clamped_value()),
            State::Visible => (1., 1.),
        };
        let _span = tracy_client::span!("ExitConfirmDialog::render");

        // Can be out of range when starting from past 0. or 1. from a spring bounce.
        let clamped_value = clamped_value.clamp(0., 1.);

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);

        let Some(scene) = &self.scene else { return };
        let mut buffers = self.buffers.borrow_mut();
        let remote = renderer.as_remote_renderer();
        let fallback = buffers
            .entry(NotNan::new(1.).unwrap())
            .or_insert_with(|| {
                let mut scene = scene.clone();
                scene.border(BORDER as f32, [1., 0.3, 0.3, 1.]);
                scene.render(remote, 1.).ok()
            })
            .clone();
        let Some(fallback) = fallback else {
            return;
        };
        let buffer = buffers
            .entry(NotNan::new(scale).unwrap())
            .or_insert_with(|| {
                // Keep the border an even number of physical pixels at each scale.
                let mut scene = scene.clone();
                scene.border(
                    ((BORDER as f64 / 2. * scale).round() * 2. / scale) as f32,
                    [1., 0.3, 0.3, 1.],
                );
                scene.render(remote, scale).ok()
            })
            .as_ref()
            .unwrap_or(&fallback)
            .clone();
        let size = buffer.logical_size();

        let location = (output_size.to_point() - size.to_point()).downscale(2.);
        let mut location = location.to_physical_precise_round(scale).to_logical(scale);
        location.x = f64::max(0., location.x);
        location.y = f64::max(0., location.y);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            clamped_value as f32,
            None,
            None,
            Kind::Unspecified,
        );
        let elem = PrimaryGpuTextureRenderElement(elem);
        let elem = RescaleRenderElement::from_element(
            elem,
            (location + size.downscale(2.)).to_physical_precise_round(scale),
            value.max(0.) * 0.2 + 0.8,
        );
        push(ExitConfirmDialogRenderElement::Texture(elem));

        // Backdrop.
        let data = output.user_data().get_or_insert(|| {
            Mutex::new(OutputData {
                backdrop: SolidColorBuffer::new(output_size, BACKDROP_COLOR),
            })
        });
        let mut data = data.lock().unwrap();
        data.backdrop.resize(output_size);

        let elem = SolidColorRenderElement::from_buffer(
            &data.backdrop,
            Point::new(0., 0.),
            clamped_value as f32,
            Kind::Unspecified,
        );
        push(ExitConfirmDialogRenderElement::SolidColor(elem));
    }
}

fn render() -> anyhow::Result<Paint> {
    let _span = tracy_client::span!("exit_confirm_dialog::render");
    let text = Text::with_options(
        &text(true),
        TextOptions {
            font_size: FONT,
            align: TextAlign::Center,
            ..Default::default()
        },
        true,
    )?;
    let (width, height) = text.size();
    let mut paint = Paint::new(width + PADDING * 2, height + PADDING * 2);
    paint.fill([0.1, 0.1, 0.1, 1.]);
    paint.text(&text, PADDING as f32, PADDING as f32);
    Ok(paint)
}

fn text(markup: bool) -> String {
    let key = if markup {
        format!("<span face='mono' bgcolor='#2C2C2C'> {KEY_NAME} </span>")
    } else {
        String::from(KEY_NAME)
    };

    format!(
        "Are you sure you want to exit niri?\n\n\
         Press {key} to confirm."
    )
}

#[cfg(feature = "dbus")]
pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label("Exit niri");
    node.set_description(text(false));
    node.set_modal();
    node
}

#[cfg(test)]
pub(super) fn test_paint() -> anyhow::Result<Paint> {
    let mut paint = render()?;
    paint.border(BORDER as f32, [1., 0.3, 0.3, 1.]);
    Ok(paint)
}
