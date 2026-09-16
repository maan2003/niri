//! Core-side view of the shader programs living in the GPU process.

use glam::Mat3;
use smithay::backend::renderer::gles::{Uniform, UniformValue};

use super::renderer::NiriRenderer;
use super::shader_element::ShaderProgram;
use crate::gpu::protocol::{ShaderKind, ShaderSupport, TexProgram};
use crate::gpu::remote::{RemoteRenderer, RemoteTexProgram};

pub type ProgramType = ShaderKind;

#[derive(Debug, Clone, Copy)]
pub struct Shaders {
    support: ShaderSupport,
    pub clipped_surface: Option<RemoteTexProgram>,
    pub postprocess_and_clip: Option<RemoteTexProgram>,
    pub gradient_fade: Option<RemoteTexProgram>,
    pub blur: bool,
}

impl Shaders {
    pub fn get(renderer: &mut impl NiriRenderer) -> Self {
        Self::from_renderer(renderer.as_remote_renderer())
    }

    pub fn from_renderer(renderer: &RemoteRenderer) -> Self {
        let support = renderer.shaders();
        Self {
            support,
            clipped_surface: renderer.tex_program(TexProgram::ClippedSurface),
            postprocess_and_clip: renderer.tex_program(TexProgram::PostprocessAndClip),
            gradient_fade: renderer.tex_program(TexProgram::GradientFade),
            blur: support.blur,
        }
    }

    pub fn program(&self, program: ProgramType) -> Option<ShaderProgram> {
        let available = match program {
            ProgramType::Border => self.support.border,
            ProgramType::Shadow => self.support.shadow,
            ProgramType::Resize => self.support.resize,
            ProgramType::Close => self.support.close,
            ProgramType::Open => self.support.open,
        };
        available.then_some(ShaderProgram(program))
    }
}

fn set_custom(renderer: &mut RemoteRenderer, kind: ShaderKind, src: Option<&str>) {
    if let Err(err) = renderer.set_custom_shader(kind, src) {
        warn!("error setting custom {kind:?} shader: {err}");
    }
}

pub fn set_custom_resize_program(renderer: &mut RemoteRenderer, src: Option<&str>) {
    set_custom(renderer, ShaderKind::Resize, src);
}

pub fn set_custom_close_program(renderer: &mut RemoteRenderer, src: Option<&str>) {
    set_custom(renderer, ShaderKind::Close, src);
}

pub fn set_custom_open_program(renderer: &mut RemoteRenderer, src: Option<&str>) {
    set_custom(renderer, ShaderKind::Open, src);
}

pub fn mat3_uniform(name: &str, mat: Mat3) -> Uniform<'_> {
    Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![mat.to_cols_array()],
            transpose: false,
        },
    )
}
