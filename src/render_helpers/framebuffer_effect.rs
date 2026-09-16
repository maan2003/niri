use glam::{Mat3, Vec2};
use niri_config::CornerRadius;
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::gles::Uniform;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::Frame as _;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform};

use crate::gpu::remote::{CaptureHandle, RemoteError, RemoteFrame, RemoteRenderer};
use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::blend::FrameBlendState;
use crate::render_helpers::blur::BlurOptions;
use crate::render_helpers::shaders::{mat3_uniform, Shaders};
use crate::utils::region::TransformedRegion;

#[derive(Debug)]
pub struct FramebufferEffect {
    id: Id,
    commit: CommitCounter,
}

#[derive(Debug)]
pub struct FramebufferEffectElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    clip_geo: Rectangle<f64, Logical>,
    corner_radius: CornerRadius,
    subregion: Option<TransformedRegion>,
    scale: f32,
    blur_options: Option<BlurOptions>,
    noise: f32,
    saturation: f32,
}

impl FramebufferEffect {
    pub fn new() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    pub fn damage(&mut self) {
        self.commit.increment();
    }

    pub fn render(
        &self,
        ns: Option<usize>,
        params: RenderParams,
        blur_options: Option<BlurOptions>,
        noise: f32,
        saturation: f32,
    ) -> FramebufferEffectElement {
        let (clip_geo, corner_radius) = params
            .clip
            .unwrap_or((params.geometry, CornerRadius::default()));

        let mut id = self.id.clone();
        if let Some(ns) = ns {
            id = id.namespaced(ns);
        }

        FramebufferEffectElement {
            id,
            commit: self.commit,
            geometry: params.geometry,
            clip_geo,
            corner_radius,
            subregion: params.subregion,
            scale: params.scale as f32,
            blur_options,
            noise,
            saturation,
        }
    }
}

impl FramebufferEffectElement {
    fn compute_uniforms(
        &self,
        crop: Rectangle<f64, Logical>,
        transform: Transform,
    ) -> [Uniform<'static>; 7] {
        let offset = crop.loc - (self.clip_geo.loc - self.geometry.loc);
        let offset = Vec2::new(offset.x as f32, offset.y as f32);
        let crop_size = Vec2::new(crop.size.w as f32, crop.size.h as f32);
        let clip_size = Vec2::new(self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

        // Our v_coords are [0, 1] inside crop. We want them to be [0, 1] inside clip_geo.
        let input_to_clip_geo =
            Mat3::from_scale(crop_size / clip_size) * Mat3::from_translation(offset / crop_size);

        // Revert the effect of the texture transform.
        let transform_mat = Mat3::from_translation(Vec2::new(0.5, 0.5))
            * transform.matrix()
            * Mat3::from_translation(Vec2::new(-0.5, -0.5));
        let input_to_clip_geo = input_to_clip_geo * transform_mat;

        let clip_geo_size = (self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

        [
            Uniform::new("niri_scale", self.scale),
            Uniform::new("geo_size", clip_geo_size),
            Uniform::new("corner_radius", <[f32; 4]>::from(self.corner_radius)),
            mat3_uniform("input_to_geo", input_to_clip_geo),
            Uniform::new("noise", self.noise),
            Uniform::new("saturation", self.saturation),
            Uniform::new("bg_color", [0f32, 0., 0., 0.]),
        ]
    }
}

impl Element for FramebufferEffectElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        // We don't use src for drawing but we can use it to figure out how we were cropped.
        let size = self.geometry.size.to_buffer(1., Transform::Normal);
        Rectangle::from_size(size)
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }
}

impl RenderElement<RemoteRenderer> for FramebufferEffectElement {
    fn capture_framebuffer(
        &self,
        frame: &mut RemoteFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), RemoteError> {
        let _span = tracy_client::span!("FramebufferEffectElement::capture_framebuffer");

        let output_rect = Rectangle::from_size(frame.output_size());
        if dst.intersection(output_rect).is_none() {
            return Ok(());
        }

        // The GPU process does the blit, sizing and blur; we just need a stable slot for it.
        let handle = cache.get_or_insert::<CaptureHandle, _>(|| frame.renderer().new_capture());
        frame.capture_framebuffer(
            handle,
            src,
            dst,
            self.scale,
            self.blur_options.map(Into::into),
        );
        Ok(())
    }

    fn draw(
        &self,
        frame: &mut RemoteFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), RemoteError> {
        let Some(cache) = cache else {
            return Ok(());
        };
        let Some(handle) = cache.get::<CaptureHandle>() else {
            return Ok(());
        };

        // Clamp the same way as the GPU-side capture.
        let output_rect = Rectangle::from_size(frame.output_size());
        let clamped_dst = match dst.intersection(output_rect) {
            Some(clamped) => clamped,
            None => return Ok(()),
        };
        let clamp_offset = clamped_dst.loc - dst.loc;

        let mut filtered = Vec::with_capacity(damage.len());
        if let Some(subregion) = &self.subregion {
            // Convert to subregion coordinates.
            let mut crop = src.to_logical(1., Transform::Normal, &src.size);
            crop.loc += self.geometry.loc;
            subregion.filter_damage(crop, dst, damage, &mut filtered);
        } else {
            filtered.extend(damage.iter());
        };

        // Adjust for clamped dst.
        if clamped_dst != dst {
            let r = Rectangle::new(clamp_offset, clamped_dst.size);
            filtered.retain_mut(|d| {
                if let Some(mut crop) = d.intersection(r) {
                    crop.loc -= clamp_offset;
                    *d = crop;
                    true
                } else {
                    false
                }
            });
        }

        if filtered.is_empty() {
            return Ok(());
        }

        // Adjust src proportionally to the dst clamping.
        let src_loc = src.loc.to_logical(1., Transform::Normal, &src.size);
        let dst_to_src = src.size / dst.size.to_f64();
        let crop = Rectangle::new(
            src_loc + clamp_offset.to_f64().upscale(dst_to_src).to_logical(1.),
            clamped_dst.size.to_f64().upscale(dst_to_src).to_logical(1.),
        );

        let has_program = Shaders::from_renderer(frame.renderer())
            .postprocess_and_clip
            .is_some();
        let uniforms = has_program.then(|| {
            let mut uniforms = self.compute_uniforms(crop, frame.transformation()).to_vec();
            // The sampled framebuffer content is already in the output blend space.
            uniforms.extend(FrameBlendState::uniforms_for_content(frame, true));
            uniforms
        });
        let uniforms = uniforms.as_ref().map_or(&[][..], |x| &x[..]);

        frame.draw_captured(handle, clamped_dst, &filtered, uniforms);
        Ok(())
    }
}
