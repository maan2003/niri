//! GPU-process side: replays recorded commands on a real `GlesRenderer`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::{
    Bind as _, Color32F, DebugFlags, ExportMem as _, Frame as _, FrameContext as _, ImportDma as _,
    ImportMem as _, Offscreen as _, Renderer as _,
};
use smithay::utils::{Buffer, Physical, Rectangle, Size};

use super::convert;
use super::gl::blur::Blur;
use super::gl::capture::Capture;
use super::gl::resources::Resources;
use super::gl::shader::{self, DrawParams};
use super::gl::shaders::Shaders;
use super::protocol::{
    Caps, Command, DmabufDesc, Image, OutputRef, Rect, ShaderKind, ShaderSupport, Target, TexId,
    TexProgram,
};

/// Objects the core refers to by id.
#[derive(Default)]
pub struct Tables {
    pub textures: HashMap<TexId, GlesTexture>,
    captures: HashMap<u64, Capture>,
    blurs: HashMap<u64, Blur>,
}

/// Blur pyramids are cheap to recreate; cap how many we keep for closed windows.
const MAX_BLUR_CACHE: usize = 64;

pub struct Executor {
    pub(super) renderer: Option<GlesRenderer>,
    pub tables: RefCell<Tables>,
    /// Frames recorded for outputs, waiting for `Present`.
    pub output_frames: HashMap<OutputRef, Vec<Command>>,
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

pub fn build_dmabuf(desc: &DmabufDesc, fds: &mut VecDeque<OwnedFd>) -> anyhow::Result<Dmabuf> {
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
    builder.build().context("empty dmabuf")
}

impl Executor {
    pub fn new(renderer: Option<GlesRenderer>) -> Self {
        Self {
            renderer,
            tables: RefCell::new(Tables::default()),
            output_frames: HashMap::new(),
        }
    }

    pub fn has_renderer(&self) -> bool {
        self.renderer.is_some()
    }

    pub fn set_renderer(&mut self, renderer: GlesRenderer) {
        self.renderer = Some(renderer);
    }

    /// Drops all GL state. Textures go first: they belong to the renderer's context.
    pub fn clear_renderer(&mut self) {
        *self.tables.borrow_mut() = Tables::default();
        self.output_frames.clear();
        self.renderer = None;
    }

    pub fn renderer(&mut self) -> anyhow::Result<&mut GlesRenderer> {
        self.renderer.as_mut().context("no renderer yet")
    }

    pub fn caps(&mut self) -> anyhow::Result<Caps> {
        let renderer = self.renderer()?;
        let s = Shaders::get(renderer);
        let shaders = ShaderSupport {
            border: s.border.is_some(),
            shadow: s.shadow.is_some(),
            resize: s.program(ShaderKind::Resize).is_some(),
            clipped_surface: s.clipped_surface.is_some(),
            postprocess_and_clip: s.postprocess_and_clip.is_some(),
            gradient_fade: s.gradient_fade.is_some(),
            blur: s.blur.is_some(),
            close: s.program(ShaderKind::Close).is_some(),
            open: s.program(ShaderKind::Open).is_some(),
        };
        Ok(Caps {
            renderer: "gles".to_owned(),
            mem_formats: renderer.mem_formats().map(|f| f as u32).collect(),
            dmabuf_formats: renderer
                .dmabuf_formats()
                .iter()
                .map(|f| (f.code as u32, u64::from(f.modifier)))
                .collect(),
            dmabuf_render_formats: renderer
                .egl_context()
                .dmabuf_render_formats()
                .iter()
                .map(|f| (f.code as u32, u64::from(f.modifier)))
                .collect(),
            shaders,
        })
    }

    pub fn set_custom_shader(
        &mut self,
        kind: ShaderKind,
        src: Option<&str>,
    ) -> anyhow::Result<bool> {
        use super::gl::shaders as gl;
        let renderer = self.renderer()?;
        Ok(match kind {
            ShaderKind::Resize => gl::set_custom_resize_program(renderer, src),
            ShaderKind::Close => gl::set_custom_close_program(renderer, src),
            ShaderKind::Open => gl::set_custom_open_program(renderer, src),
            ShaderKind::Border | ShaderKind::Shadow => bail!("{kind:?} shader is not customizable"),
        })
    }

    pub fn import_dmabuf(
        &mut self,
        id: TexId,
        desc: &DmabufDesc,
        fds: &mut VecDeque<OwnedFd>,
    ) -> anyhow::Result<()> {
        let dmabuf = build_dmabuf(desc, fds)?;
        let texture = self
            .renderer()?
            .import_dmabuf(&dmabuf, None)
            .context("import_dmabuf")?;
        self.tables.borrow_mut().textures.insert(id, texture);
        Ok(())
    }

    pub fn read_texture(
        &mut self,
        id: TexId,
        region: Rect<i32>,
        format: u32,
    ) -> anyhow::Result<Image> {
        let format = fourcc(format)?;
        let renderer = self.renderer.as_mut().context("no renderer yet")?;
        let tables = self.tables.borrow();
        let texture = tables.textures.get(&id).context("unknown texture")?;
        let mapping = renderer
            .copy_texture(texture, convert::to_rect(region), format)
            .context("copy_texture")?;
        let data = renderer
            .map_texture(&mapping)
            .context("map_texture")?
            .to_vec();
        Ok(Image {
            width: region.w as u32,
            height: region.h as u32,
            format: format as u32,
            data,
        })
    }

    pub fn execute(
        &mut self,
        commands: Vec<Command>,
        fds: &mut VecDeque<OwnedFd>,
    ) -> anyhow::Result<()> {
        let renderer = self.renderer.as_mut().context("no renderer yet")?;
        let mut iter = commands.into_iter();
        while let Some(cmd) = iter.next() {
            match cmd {
                Command::Begin {
                    target: Target::Output(output),
                    ..
                } => {
                    // Kept until Present; drawn by the DRM compositor with real damage.
                    let mut frame = Vec::new();
                    loop {
                        match iter.next() {
                            None => bail!("unterminated frame"),
                            Some(Command::End) => break,
                            Some(cmd) => frame.push(cmd),
                        }
                    }
                    self.output_frames.insert(output, frame);
                }
                Command::Begin {
                    target: Target::Texture(target),
                    width,
                    height,
                    transform,
                } => {
                    let mut texture = self
                        .tables
                        .borrow()
                        .textures
                        .get(&target)
                        .context("unknown target texture")?
                        .clone();
                    let mut fb = renderer.bind(&mut texture).context("bind")?;
                    let mut frame = renderer
                        .render(
                            &mut fb,
                            Size::from((width, height)),
                            convert::to_transform(transform),
                        )
                        .context("render")?;
                    let res = run_frame(&mut frame, &self.tables, &mut iter, None);
                    let _sync = frame.finish().context("finish")?;
                    res?;
                }
                cmd => {
                    let mut tables = self.tables.borrow_mut();
                    execute_one(renderer, &mut tables, cmd, fds)?;
                }
            }
        }
        Ok(())
    }
}

/// Commands valid outside a frame (and, via the frame's renderer guard, inside one).
fn execute_one(
    renderer: &mut GlesRenderer,
    tables: &mut Tables,
    cmd: Command,
    fds: &mut VecDeque<OwnedFd>,
) -> anyhow::Result<()> {
    match cmd {
        Command::CreateTexture {
            id,
            format,
            width,
            height,
        } => {
            let texture = renderer
                .create_buffer(fourcc(format)?, Size::from((width, height)))
                .context("create_buffer")?;
            tables.textures.insert(id, texture);
        }
        Command::ImportMemory {
            id,
            format,
            width,
            height,
            flipped,
            data,
        } => {
            let texture = renderer
                .import_memory(&data, fourcc(format)?, Size::from((width, height)), flipped)
                .context("import_memory")?;
            tables.textures.insert(id, texture);
        }
        Command::UpdateMemory { id, region, data } => {
            let texture = tables.textures.get(&id).context("unknown texture")?;
            renderer
                .update_memory(texture, &data, convert::to_rect(region))
                .context("update_memory")?;
        }
        Command::ImportDmabuf { id, desc, damage } => {
            let dmabuf = build_dmabuf(&desc, fds)?;
            let damage = damage.map(|d| convert::to_rects::<Buffer>(&d));
            let texture = renderer
                .import_dmabuf(&dmabuf, damage.as_deref())
                .context("import_dmabuf")?;
            tables.textures.insert(id, texture);
        }
        Command::DestroyTexture { id } => {
            tables.textures.remove(&id);
        }
        Command::DestroyCapture { key } => {
            tables.captures.remove(&key);
        }
        Command::Blur {
            key,
            src,
            dst,
            params,
        } => {
            let src = tables
                .textures
                .get(&src)
                .context("unknown texture")?
                .clone();
            if tables.blurs.len() > MAX_BLUR_CACHE && !tables.blurs.contains_key(&key) {
                tables.blurs.clear();
            }
            let blur = match tables.blurs.entry(key) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(Blur::new(renderer).context("blur shader unavailable")?)
                }
            };
            let options = params.into();
            blur.prepare_textures(|f, s| renderer.create_buffer(f, s), &src, options)
                .context("prepare blur textures")?;
            let out = blur.render(renderer, &src, options).context("blur")?;
            tables.textures.insert(dst, out);
        }
        Command::SetDebugFlags { flags } => {
            renderer.set_debug_flags(DebugFlags::from_bits_truncate(flags));
        }
        other => bail!("{other:?} is only valid inside a frame"),
    }
    Ok(())
}

fn intersect(
    damage: &[Rect<i32>],
    clip: Option<&[Rectangle<i32, Physical>]>,
) -> Vec<Rectangle<i32, Physical>> {
    let damage = convert::to_rects::<Physical>(damage);
    let Some(clip) = clip else {
        return damage;
    };
    damage
        .iter()
        .flat_map(|d| clip.iter().filter_map(|c| d.intersection(*c)))
        .collect()
}

/// Replays frame commands until `End`. `clip`, if given, restricts every draw to those
/// (frame-relative) rectangles; the DRM compositor uses it to redraw only real damage.
pub fn run_frame(
    frame: &mut GlesFrame<'_, '_>,
    tables: &RefCell<Tables>,
    iter: &mut impl Iterator<Item = Command>,
    clip: Option<&[Rectangle<i32, Physical>]>,
) -> anyhow::Result<()> {
    let programs = {
        let shaders = Shaders::get_from_frame(frame);
        TexPrograms {
            clipped_surface: shaders.clipped_surface.clone(),
            postprocess_and_clip: shaders.postprocess_and_clip.clone(),
            gradient_fade: shaders.gradient_fade.clone(),
        }
    };
    let resources = Resources::get(frame);

    loop {
        let Some(cmd) = iter.next() else {
            bail!("unterminated frame");
        };
        match cmd {
            Command::End => return Ok(()),
            Command::Clear { color, at } => {
                let at = intersect(&at, clip);
                if at.is_empty() {
                    continue;
                }
                let [r, g, b, a] = color;
                frame
                    .clear(Color32F::new(r, g, b, a), &at)
                    .context("clear")?;
            }
            Command::DrawSolid { dst, damage, color } => {
                let damage = intersect(&damage, clip);
                if damage.is_empty() {
                    continue;
                }
                let [r, g, b, a] = color;
                frame
                    .draw_solid(convert::to_rect(dst), &damage, Color32F::new(r, g, b, a))
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
                let damage = intersect(&damage, clip);
                if damage.is_empty() {
                    continue;
                }
                let tables = tables.borrow();
                let texture = tables.textures.get(&texture).context("unknown texture")?;
                let uniforms = convert::to_uniforms(uniforms);
                frame
                    .render_texture_from_to(
                        texture,
                        convert::to_rect_f64(src),
                        convert::to_rect(dst),
                        &damage,
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
                    frame.override_default_tex_program(
                        program.clone(),
                        convert::to_uniforms(uniforms),
                    );
                }
            }
            Command::ClearTexProgramOverride => frame.clear_tex_program_override(),
            Command::DrawShader {
                program,
                src,
                dst,
                damage,
                scale,
                alpha,
                uniforms,
                textures,
            } => {
                let damage = intersect(&damage, clip);
                if damage.is_empty() {
                    continue;
                }
                let Some(shader) = Shaders::get_from_frame(frame).program(program) else {
                    continue;
                };
                let tables = tables.borrow();
                let textures = textures
                    .into_iter()
                    .map(|(name, id)| {
                        tables
                            .textures
                            .get(&id)
                            .cloned()
                            .map(|t| (name, t))
                            .context("unknown texture")
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let uniforms = convert::to_uniforms(uniforms);
                shader::draw(
                    frame,
                    &shader,
                    resources.as_ref().context("GL resources missing")?,
                    &DrawParams {
                        src: convert::to_rect_f64(src),
                        dest: convert::to_rect(dst),
                        damage: &damage,
                        scale,
                        alpha,
                        uniforms: &uniforms,
                        textures: &textures,
                    },
                )
                .context("draw shader")?;
            }
            Command::CaptureFramebuffer {
                key,
                src,
                dst,
                scale,
                blur,
            } => {
                let mut tables = tables.borrow_mut();
                let capture = match tables.captures.entry(key) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let mut guard = frame.renderer();
                        e.insert(Capture::new(guard.as_mut()))
                    }
                };
                capture
                    .capture(
                        frame,
                        convert::to_rect_f64(src),
                        convert::to_rect(dst),
                        scale,
                        blur.map(Into::into),
                    )
                    .context("capture framebuffer")?;
            }
            Command::DrawCaptured {
                key,
                dst,
                damage,
                uniforms,
            } => {
                let damage = intersect(&damage, clip);
                if damage.is_empty() {
                    continue;
                }
                let tables = tables.borrow();
                let Some(capture) = tables.captures.get(&key) else {
                    continue;
                };
                let uniforms = convert::to_uniforms(uniforms);
                capture
                    .draw(
                        frame,
                        convert::to_rect(dst),
                        &damage,
                        programs.postprocess_and_clip.as_ref(),
                        &uniforms,
                    )
                    .context("draw captured")?;
            }
            Command::Begin { .. } => bail!("nested Begin"),
            Command::BeginElement(_) | Command::BeginElementDraw | Command::EndElement => {
                bail!("element marker outside an output frame")
            }
            cmd => {
                // Texture management interleaved with drawing (e.g. a blur output created
                // mid-frame): run it through the frame's renderer guard.
                let mut guard = frame.renderer();
                let mut tables = tables.borrow_mut();
                let mut no_fds = VecDeque::new();
                execute_one(guard.as_mut(), &mut tables, cmd, &mut no_fds)?;
            }
        }
    }
}
