use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::texture::TextureRenderElement;
use crate::gpu::protocol::{TextureEffect, TextureOptions};
use crate::gpu::remote::{RemoteError, RemoteFrame, RemoteRenderer, RemoteTexture};

#[derive(Debug, Clone)]
pub struct GradientFadeTextureRenderElement {
    inner: TextureRenderElement<RemoteTexture>,
    cutoff: (f32, f32),
}

impl GradientFadeTextureRenderElement {
    pub fn new(texture: TextureRenderElement<RemoteTexture>) -> Self {
        let logical_w = texture.buffer().logical_size().w;
        let logical_src_w = texture.logical_src().size.w;
        let cutoff = if logical_src_w < logical_w {
            // Texture is clipped, add a fade.
            let cutoff = 1. - f64::min(18. / logical_src_w, 1.);
            let full = logical_src_w / logical_w;
            ((cutoff * full) as f32, full as f32)
        } else {
            // Texture is displayed full-size, no cutoff necessary.
            (1., 1.)
        };
        Self {
            inner: texture,
            cutoff,
        }
    }
}

impl Element for GradientFadeTextureRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<RemoteRenderer> for GradientFadeTextureRenderElement {
    fn draw(
        &self,
        frame: &mut RemoteFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), RemoteError> {
        let options = TextureOptions {
            effect: Some(TextureEffect::Fade {
                cutoff: [self.cutoff.0, self.cutoff.1],
            }),
            ..TextureOptions::default()
        };
        frame.with_texture_options(options, |frame| {
            RenderElement::<RemoteRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            )
        })
    }

    fn underlying_storage(&self, _renderer: &mut RemoteRenderer) -> Option<UnderlyingStorage<'_>> {
        // If scanout for things other than Wayland buffers is implemented, this will need to take
        // the target GPU into account.
        None
    }
}
