//! Runs scene ops on a `GlesFrame`.
//!
//! The one place that turns frame-space ops into smithay draw calls. Every op is clipped by
//! the same rule: the node's damage (frame coordinates) intersected with the op's `dst`, then
//! made `dst`-relative because that is what smithay's draw calls want.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Context as _;
use smithay::backend::renderer::gles::{GlesFrame, GlesTexProgram};
use smithay::backend::renderer::{Color32F, FrameContext as _};
use smithay::utils::{Physical, Rectangle};

use super::convert;
use super::exec::Tables;
use super::gl::capture::Capture;
use super::gl::resources::Resources;
use super::gl::shader::{self, DrawParams};
use super::gl::shaders::Shaders;
use super::protocol::{Op, Rect, TexProgram};

struct TexPrograms {
    clipped_surface: Option<GlesTexProgram>,
    postprocess_and_clip: Option<GlesTexProgram>,
    gradient_fade: Option<GlesTexProgram>,
    texture_hdr: Option<GlesTexProgram>,
}

impl TexPrograms {
    fn get(&self, program: TexProgram) -> Option<&GlesTexProgram> {
        match program {
            TexProgram::ClippedSurface => self.clipped_surface.as_ref(),
            TexProgram::PostprocessAndClip => self.postprocess_and_clip.as_ref(),
            TexProgram::GradientFade => self.gradient_fade.as_ref(),
            TexProgram::TextureHdr => self.texture_hdr.as_ref(),
        }
    }
}

/// Damage for an op drawn at `dst`, relative to `dst`: the frame-space `clip` (or all of
/// `dst` when there is none) intersected with `dst`. Empty means the op can be skipped.
pub fn op_damage(
    dst: Rectangle<i32, Physical>,
    clip: Option<&[Rectangle<i32, Physical>]>,
) -> Vec<Rectangle<i32, Physical>> {
    let Some(clip) = clip else {
        return vec![Rectangle::from_size(dst.size)];
    };
    clip.iter()
        .filter_map(|c| c.intersection(dst))
        .map(|mut r| {
            r.loc -= dst.loc;
            r
        })
        .collect()
}

/// Frame-space rectangles made relative to `dst`.
fn relative_to(
    rects: &[Rect<i32>],
    dst: Rectangle<i32, Physical>,
) -> Vec<Rectangle<i32, Physical>> {
    rects
        .iter()
        .map(|r| {
            let mut r = convert::to_rect::<Physical>(*r);
            r.loc -= dst.loc;
            r
        })
        .collect()
}

/// Draws `ops` in order, clipped to `clip` (frame coordinates; `None` = unclipped).
pub fn draw_ops(
    frame: &mut GlesFrame<'_, '_>,
    tables: &RefCell<Tables>,
    ops: &[Op],
    clip: Option<&[Rectangle<i32, Physical>]>,
) -> anyhow::Result<()> {
    let programs = {
        let shaders = Shaders::get_from_frame(frame);
        TexPrograms {
            clipped_surface: shaders.clipped_surface.clone(),
            postprocess_and_clip: shaders.postprocess_and_clip.clone(),
            gradient_fade: shaders.gradient_fade.clone(),
            texture_hdr: shaders.texture_hdr.clone(),
        }
    };
    let resources = Resources::get(frame);
    draw_ops_inner(frame, tables, &programs, resources.as_ref(), ops, clip)
}

fn draw_ops_inner(
    frame: &mut GlesFrame<'_, '_>,
    tables: &RefCell<Tables>,
    programs: &TexPrograms,
    resources: Option<&Rc<RefCell<Resources>>>,
    ops: &[Op],
    clip: Option<&[Rectangle<i32, Physical>]>,
) -> anyhow::Result<()> {
    for op in ops {
        match op {
            Op::Solid { dst, color } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let [r, g, b, a] = *color;
                frame
                    .draw_solid(dst, &damage, Color32F::new(r, g, b, a))
                    .context("draw_solid")?;
            }
            Op::Texture {
                texture,
                src,
                dst,
                opaque,
                transform,
                alpha,
                program,
                uniforms,
            } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let tables = tables.borrow();
                let texture = tables.textures.get(texture).context("unknown texture")?;
                let uniforms = convert::to_uniforms(uniforms.clone());
                frame
                    .render_texture_from_to(
                        texture,
                        convert::to_rect_f64(*src),
                        dst,
                        &damage,
                        &relative_to(opaque, dst),
                        convert::to_transform(*transform),
                        *alpha,
                        program.and_then(|p| programs.get(p)),
                        &uniforms,
                    )
                    .context("render_texture_from_to")?;
            }
            Op::Shader {
                program,
                src,
                dst,
                scale,
                alpha,
                uniforms,
                textures,
            } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let Some(shader) = Shaders::get_from_frame(frame).program(*program) else {
                    continue;
                };
                let tables = tables.borrow();
                let textures = textures
                    .iter()
                    .map(|(name, id)| {
                        tables
                            .textures
                            .get(id)
                            .cloned()
                            .map(|t| (name.clone(), t))
                            .context("unknown texture")
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let uniforms = convert::to_uniforms(uniforms.clone());
                shader::draw(
                    frame,
                    &shader,
                    resources.context("GL resources missing")?,
                    &DrawParams {
                        src: convert::to_rect_f64(*src),
                        dest: dst,
                        damage: &damage,
                        scale: *scale,
                        alpha: *alpha,
                        uniforms: &uniforms,
                        textures: &textures,
                    },
                )
                .context("draw shader")?;
            }
            Op::Capture {
                key,
                src,
                dst,
                scale,
                blur,
            } => {
                let mut tables = tables.borrow_mut();
                let capture = match tables.captures.entry(*key) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let mut guard = frame.renderer();
                        e.insert(Capture::new(guard.as_mut()))
                    }
                };
                capture
                    .capture(
                        frame,
                        convert::to_rect_f64(*src),
                        convert::to_rect(*dst),
                        *scale,
                        blur.map(Into::into),
                    )
                    .context("capture framebuffer")?;
            }
            Op::Captured { key, dst, uniforms } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let tables = tables.borrow();
                let Some(capture) = tables.captures.get(key) else {
                    continue;
                };
                let uniforms = convert::to_uniforms(uniforms.clone());
                capture
                    .draw(
                        frame,
                        dst,
                        &damage,
                        programs.postprocess_and_clip.as_ref(),
                        &uniforms,
                    )
                    .context("draw captured")?;
            }
            Op::WithTexProgram {
                program,
                uniforms,
                ops,
            } => {
                // Replaces the frame-wide blend override for the scope; restored after.
                let saved = frame.take_tex_program_override();
                if let Some(program) = programs.get(*program) {
                    frame.override_default_tex_program(
                        program.clone(),
                        convert::to_uniforms(uniforms.clone()),
                    );
                }
                let res = draw_ops_inner(frame, tables, programs, resources, ops, clip);
                frame.set_tex_program_override(saved);
                res?;
            }
            Op::Raw { ops } => {
                let saved = frame.take_tex_program_override();
                let res = draw_ops_inner(frame, tables, programs, resources, ops, clip);
                frame.set_tex_program_override(saved);
                res?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn op_damage_is_clipped_in_frame_space_and_made_relative() {
        // A 100x100 op at (200, 300); clip covers its bottom-right quarter.
        let dst = rect(200, 300, 100, 100);
        let clip = [rect(250, 350, 100, 100)];
        assert_eq!(op_damage(dst, Some(&clip)), vec![rect(50, 50, 50, 50)]);
        // No clip: the whole op.
        assert_eq!(op_damage(dst, None), vec![rect(0, 0, 100, 100)]);
        // A clip that misses the op yields nothing.
        assert!(op_damage(dst, Some(&[rect(0, 0, 100, 100)])).is_empty());
    }
}
