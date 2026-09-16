//! GPU-process side: replays recorded commands on a real `GlesRenderer`.

use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::{
    Bind as _, Color32F, DebugFlags, ExportMem as _, Frame as _, ImportDma as _, ImportMem as _,
    Offscreen as _, Renderer as _,
};
use smithay::utils::{Buffer, Physical, Size};

use super::convert;
use super::protocol::{Caps, Command, Image, Rect, ShaderKind, ShaderSupport, Target, TexId, TexProgram};
use crate::render_helpers::shaders::{self, Shaders};

pub struct Executor {
    renderer: GlesRenderer,
    textures: HashMap<TexId, GlesTexture>,
}

#[derive(Default)]
struct TexPrograms {
    clipped_surface: Option<GlesTexProgram>,
    postprocess_and_clip: Option<GlesTexProgram>,
    gradient_fade: Option<GlesTexProgram>,
}

impl TexPrograms {
    fn get(&self, program: TexProgram) -> Option<&GlesTexProgram> {
        match program {
            TexProgram::ClippedSurface => self.clipped_surface.as_ref(),
            TexProgram::PostprocessAndClip => self.postprocess_and_clip.as_ref(),
            TexProgram::GradientFade => self.gradient_fade.as_ref(),
        }
    }
}

fn fourcc(format: u32) -> anyhow::Result<Fourcc> {
    Fourcc::try_from(format).map_err(|_| anyhow!("unknown fourcc {format:#x}"))
}

impl Executor {
    pub fn new(renderer: GlesRenderer) -> Self {
        Self {
            renderer,
            textures: HashMap::new(),
        }
    }

    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    pub fn caps(&mut self) -> Caps {
        let s = Shaders::get(&mut self.renderer);
        let shaders = ShaderSupport {
            border: s.border.is_some(),
            shadow: s.shadow.is_some(),
            resize: s.resize.is_some(),
            clipped_surface: s.clipped_surface.is_some(),
            postprocess_and_clip: s.postprocess_and_clip.is_some(),
            gradient_fade: s.gradient_fade.is_some(),
            blur: s.blur.is_some(),
        };
        Caps {
            renderer: "gles".to_owned(),
            mem_formats: self.renderer.mem_formats().map(|f| f as u32).collect(),
            dmabuf_formats: self
                .renderer
                .dmabuf_formats()
                .iter()
                .map(|f| (f.code as u32, u64::from(f.modifier)))
                .collect(),
            shaders,
        }
    }

    pub fn set_custom_shader(&mut self, kind: ShaderKind, src: Option<&str>) -> anyhow::Result<()> {
        match kind {
            ShaderKind::Resize => shaders::set_custom_resize_program(&mut self.renderer, src),
            ShaderKind::Close => shaders::set_custom_close_program(&mut self.renderer, src),
            ShaderKind::Open => shaders::set_custom_open_program(&mut self.renderer, src),
            ShaderKind::Border | ShaderKind::Shadow => bail!("{kind:?} shader is not customizable"),
        }
        Ok(())
    }

    pub fn read_texture(&mut self, id: TexId, region: Rect<i32>, format: u32) -> anyhow::Result<Image> {
        let format = fourcc(format)?;
        let texture = self.textures.get(&id).context("unknown texture")?;
        let mapping = self
            .renderer
            .copy_texture(texture, convert::to_rect(region), format)
            .context("copy_texture")?;
        let data = self.renderer.map_texture(&mapping).context("map_texture")?.to_vec();
        Ok(Image {
            width: region.w as u32,
            height: region.h as u32,
            format: format as u32,
            data,
        })
    }

    pub fn execute(&mut self, commands: Vec<Command>, fds: &mut VecDeque<OwnedFd>) -> anyhow::Result<()> {
        let mut iter = commands.into_iter();
        while let Some(cmd) = iter.next() {
            match cmd {
                Command::CreateTexture { id, format, width, height } => {
                    let texture = self
                        .renderer
                        .create_buffer(fourcc(format)?, Size::from((width, height)))
                        .context("create_buffer")?;
                    self.textures.insert(id, texture);
                }
                Command::ImportMemory { id, format, width, height, flipped, data } => {
                    let texture = self
                        .renderer
                        .import_memory(&data, fourcc(format)?, Size::from((width, height)), flipped)
                        .context("import_memory")?;
                    self.textures.insert(id, texture);
                }
                Command::UpdateMemory { id, region, data } => {
                    let texture = self.textures.get(&id).context("unknown texture")?;
                    self.renderer
                        .update_memory(texture, &data, convert::to_rect(region))
                        .context("update_memory")?;
                }
                Command::ImportDmabuf { id, desc, damage } => {
                    if desc.planes.is_empty() || fds.len() < desc.planes.len() {
                        bail!("ImportDmabuf expects one fd per plane");
                    }
                    let mut builder = Dmabuf::builder(
                        (desc.width as i32, desc.height as i32),
                        fourcc(desc.format)?,
                        Modifier::from(desc.modifier),
                        DmabufFlags::empty(),
                    );
                    for plane in &desc.planes {
                        let fd = fds.pop_front().unwrap();
                        if !builder.add_plane(Arc::new(fd), plane.offset, plane.stride) {
                            bail!("too many dmabuf planes");
                        }
                    }
                    let dmabuf = builder.build().context("empty dmabuf")?;
                    let damage = damage.map(|d| convert::to_rects::<Buffer>(&d));
                    let texture = self
                        .renderer
                        .import_dmabuf(&dmabuf, damage.as_deref())
                        .context("import_dmabuf")?;
                    self.textures.insert(id, texture);
                }
                Command::DestroyTexture { id } => {
                    self.textures.remove(&id);
                }
                Command::SetDebugFlags { flags } => {
                    self.renderer
                        .set_debug_flags(DebugFlags::from_bits_truncate(flags));
                }
                Command::Begin { target, width, height, transform } => {
                    let Target::Texture(target) = target else {
                        bail!("output targets are not supported yet");
                    };
                    let mut texture = self
                        .textures
                        .get(&target)
                        .context("unknown target texture")?
                        .clone();
                    let mut fb = self.renderer.bind(&mut texture).context("bind")?;
                    let mut frame = self
                        .renderer
                        .render(&mut fb, Size::from((width, height)), convert::to_transform(transform))
                        .context("render")?;
                    let res = run_frame(&mut frame, &self.textures, &mut iter);
                    let _sync = frame.finish().context("finish")?;
                    res?;
                }
                other => bail!("{other:?} is only valid inside a frame"),
            }
        }
        Ok(())
    }
}

fn run_frame(
    frame: &mut GlesFrame<'_, '_>,
    textures: &HashMap<TexId, GlesTexture>,
    iter: &mut impl Iterator<Item = Command>,
) -> anyhow::Result<()> {
    let programs = {
        let shaders = Shaders::get_from_frame(frame);
        TexPrograms {
            clipped_surface: shaders.clipped_surface.clone(),
            postprocess_and_clip: shaders.postprocess_and_clip.clone(),
            gradient_fade: shaders.gradient_fade.clone(),
        }
    };

    loop {
        let Some(cmd) = iter.next() else {
            bail!("unterminated frame");
        };
        match cmd {
            Command::End => return Ok(()),
            Command::Clear { color, at } => {
                let [r, g, b, a] = color;
                frame
                    .clear(Color32F::new(r, g, b, a), &convert::to_rects::<Physical>(&at))
                    .context("clear")?;
            }
            Command::DrawSolid { dst, damage, color } => {
                let [r, g, b, a] = color;
                frame
                    .draw_solid(
                        convert::to_rect(dst),
                        &convert::to_rects::<Physical>(&damage),
                        Color32F::new(r, g, b, a),
                    )
                    .context("draw_solid")?;
            }
            Command::DrawTexture {
                texture,
                src,
                dst,
                damage,
                opaque,
                transform,
                alpha,
                program,
                uniforms,
            } => {
                let texture = textures.get(&texture).context("unknown texture")?;
                let uniforms = convert::to_uniforms(uniforms);
                frame
                    .render_texture_from_to(
                        texture,
                        convert::to_rect_f64(src),
                        convert::to_rect(dst),
                        &convert::to_rects::<Physical>(&damage),
                        &convert::to_rects::<Physical>(&opaque),
                        convert::to_transform(transform),
                        alpha,
                        program.and_then(|p| programs.get(p)),
                        &uniforms,
                    )
                    .context("render_texture_from_to")?;
            }
            Command::OverrideTexProgram { program, uniforms } => {
                if let Some(program) = programs.get(program) {
                    frame.override_default_tex_program(program.clone(), convert::to_uniforms(uniforms));
                }
            }
            Command::ClearTexProgramOverride => frame.clear_tex_program_override(),
            other => bail!("{other:?} is not valid inside a frame"),
        }
    }
}
