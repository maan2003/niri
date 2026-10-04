use anyhow::Context as _;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::RenderElement;
use smithay::utils::{Logical, Point, Scale, Size};

use crate::animation::Animation;
use crate::gpu::remote::RemoteRenderer;
use crate::niri_render_elements;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenData, OffscreenRenderElement};

#[derive(Debug)]
pub struct OpenAnimation {
    anim: Animation,
    buffer: OffscreenBuffer,
}

niri_render_elements! {
    OpeningWindowRenderElement => {
        Offscreen = RelocateRenderElement<RescaleRenderElement<OffscreenRenderElement>>,
    }
}

impl OpenAnimation {
    pub fn new(anim: Animation) -> Self {
        Self {
            anim,
            buffer: OffscreenBuffer::default(),
        }
    }

    pub fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    // We can't depend on view_rect here, because the result of window opening can be snapshot and
    // then rendered elsewhere.
    pub fn render(
        &self,
        renderer: &mut RemoteRenderer,
        elements: &[impl RenderElement<RemoteRenderer>],
        geo_size: Size<f64, Logical>,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> anyhow::Result<(OpeningWindowRenderElement, OffscreenData)> {
        let progress = self.anim.value();
        let clamped_progress = self.anim.clamped_value().clamp(0., 1.);

        let (elem, _sync_point, data) = self
            .buffer
            .render(renderer, scale, elements)
            .context("error rendering to offscreen buffer")?;

        let elem = elem.with_alpha(clamped_progress as f32 * alpha);

        let center = geo_size.to_point().downscale(2.);
        let elem = RescaleRenderElement::from_element(
            elem,
            center.to_physical_precise_round(scale),
            (progress / 2. + 0.5).max(0.),
        );

        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        Ok((elem.into(), data))
    }
}
