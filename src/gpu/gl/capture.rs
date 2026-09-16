//! GPU-process side of the framebuffer effect: snapshot what is under an element and blur it.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::backend::renderer::{Frame as _, FrameContext as _, Offscreen as _, Texture as _};
use smithay::utils::{Buffer, Physical, Rectangle, Transform};

use super::blur::{Blur, BlurOptions};

#[derive(Debug)]
pub struct Capture {
    framebuffer: Option<GlesTexture>,
    blur: Option<Blur>,
    intermediate: Option<GlesTexture>,
}

impl Capture {
    pub fn new(renderer: &mut GlesRenderer) -> Self {
        Self {
            framebuffer: None,
            blur: Blur::new(renderer),
            intermediate: None,
        }
    }

    pub fn capture(
        &mut self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        scale: f32,
        blur_options: Option<BlurOptions>,
    ) -> Result<(), GlesError> {
        let _span = tracy_client::span!("Capture::capture");
        let output_rect = Rectangle::from_size(frame.output_size());
        let transform = frame.transformation();

        self.intermediate = None;

        let clamped_dst = match dst.intersection(output_rect) {
            Some(clamped) => clamped,
            None => return Ok(()),
        };
        let clamp_scale = clamped_dst.size.to_f64() / dst.size.to_f64();
        let dst = transform.transform_rect_in(clamped_dst, &output_rect.size);

        let size = src
            .size
            .to_logical(1., Transform::Normal)
            .upscale(clamp_scale)
            .to_physical_precise_round(scale);
        let size = transform.transform_size(size);
        let size = size.to_logical(1).to_buffer(1, Transform::Normal);

        if self
            .framebuffer
            .as_ref()
            .is_some_and(|fb| fb.size() != size)
        {
            self.framebuffer = None;
        }

        let mut guard = frame.renderer();
        let framebuffer = if let Some(fb) = &self.framebuffer {
            fb
        } else {
            trace!("creating framebuffer texture sized {} × {}", size.w, size.h);
            let renderer = guard.as_mut();
            let texture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
            self.framebuffer.insert(texture)
        };

        let mut blur = Option::zip(self.blur.as_mut(), blur_options);
        if let Some((b, options)) = &mut blur {
            let renderer = guard.as_mut();
            if let Err(err) = b.prepare_textures(
                |fourcc, size| renderer.create_buffer(fourcc, size),
                framebuffer,
                *options,
            ) {
                warn!("error preparing blur textures: {err:?}");
                blur = None;
            }
        }
        drop(guard);

        frame.with_context(|gl| unsafe {
            while gl.GetError() != ffi::NO_ERROR {}
            let mut current_fbo = 0i32;
            gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut current_fbo as *mut _);
            gl.Disable(ffi::SCISSOR_TEST);
            let mut fbo = 0;
            gl.GenFramebuffers(1, &mut fbo as *mut _);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::DRAW_FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                framebuffer.tex_id(),
                0,
            );
            gl.BlitFramebuffer(
                dst.loc.x,
                dst.loc.y,
                dst.loc.x + dst.size.w,
                dst.loc.y + dst.size.h,
                0,
                0,
                size.w,
                size.h,
                ffi::COLOR_BUFFER_BIT,
                ffi::LINEAR,
            );
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, current_fbo as u32);
            gl.Enable(ffi::SCISSOR_TEST);
            gl.DeleteFramebuffers(1, &mut fbo as *mut _);
            if gl.GetError() != ffi::NO_ERROR {
                Err(GlesError::BlitError)
            } else {
                Ok(())
            }
        })??;

        if blur_options.is_none() {
            self.intermediate = Some(framebuffer.clone());
            return Ok(());
        }

        if let Some((blur, options)) = blur {
            let mut guard = frame.renderer();
            let renderer = guard.as_mut();
            match blur.render(renderer, framebuffer, options) {
                Ok(blurred) => self.intermediate = Some(blurred),
                Err(err) => warn!("error rendering blur: {err:?}"),
            }
        }
        Ok(())
    }

    pub fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        program: Option<&GlesTexProgram>,
        uniforms: &[Uniform<'_>],
    ) -> Result<(), GlesError> {
        let Some(texture) = &self.intermediate else {
            return Ok(());
        };
        let transform = frame.transformation().invert();
        frame.render_texture_from_to(
            texture,
            Rectangle::from_size(texture.size().to_f64()),
            dst,
            damage,
            &[],
            transform,
            1.,
            program,
            uniforms,
        )
    }
}
