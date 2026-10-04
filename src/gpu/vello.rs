//! Native Vello compositor renderer. Vulkan devices and client pixel access stay in the GPU worker.

mod dmabuf;
pub(crate) mod effects;
mod renderer;

use anyhow::{ensure, Context as _};
pub(crate) use renderer::{
    texture_matrix, VelloError, VelloFrame, VelloRenderer, VelloTarget, VelloTexture,
};
use skrifa::instance::{LocationRef, Size};
use skrifa::{FontRef, MetadataProvider};

use super::protocol::{UiOp, UiScene};

const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_FONT_BYTES: usize = 32 * 1024 * 1024;
const MAX_OPS: usize = 65_536;
const MAX_GLYPHS: usize = 262_144;
const MAX_COORD: f32 = 65_536.0;

fn validate(scene: &UiScene, max_dimension: u32) -> anyhow::Result<()> {
    ensure!(
        scene.width > 0
            && scene.height > 0
            && scene.width <= max_dimension.min(u16::MAX.into())
            && scene.height <= max_dimension.min(u16::MAX.into())
            && u64::from(scene.width) * u64::from(scene.height) <= MAX_PIXELS,
        "invalid UI dimensions"
    );
    ensure!(
        scene.ops.len() <= MAX_OPS && scene.fonts.len() <= 64,
        "too many UI resources"
    );
    let font_bytes: usize = scene.fonts.iter().map(|f| f.data.len()).sum();
    ensure!(font_bytes <= MAX_FONT_BYTES, "UI font data too large");
    let fonts: Vec<_> = scene
        .fonts
        .iter()
        .map(|f| FontRef::from_index(&f.data, f.index).context("invalid UI font data/face"))
        .collect::<anyhow::Result<_>>()?;
    let geometry = |values: &[f32]| values.iter().all(|v| v.is_finite() && v.abs() <= MAX_COORD);
    let mut glyph_count = 0usize;
    for op in &scene.ops {
        let color = match op {
            UiOp::Rect { rect, color } => {
                ensure!(
                    geometry(rect) && rect[2] >= 0. && rect[3] >= 0.,
                    "invalid UI rectangle"
                );
                color
            }
            UiOp::Circle {
                center,
                radius,
                color,
            } => {
                ensure!(
                    geometry(center) && geometry(&[*radius]) && *radius >= 0.,
                    "invalid UI circle"
                );
                color
            }
            UiOp::GlyphRun {
                font,
                font_size,
                coords,
                glyphs,
                color,
            } => {
                let font = fonts.get(*font as usize).context("invalid UI font index")?;
                ensure!(
                    font_size.is_finite() && *font_size > 0. && *font_size <= 4096.,
                    "invalid UI font size"
                );
                ensure!(
                    coords.len() <= font.axes().len()
                        && coords.iter().all(|c| (-16384..=16384).contains(c)),
                    "invalid UI variation coordinates"
                );
                glyph_count += glyphs.len();
                ensure!(glyph_count <= MAX_GLYPHS, "too many UI glyphs");
                let count = font
                    .glyph_metrics(Size::unscaled(), LocationRef::default())
                    .glyph_count();
                ensure!(
                    glyphs.iter().all(|g| g.id < count && geometry(&[g.x, g.y])),
                    "invalid UI glyph"
                );
                color
            }
        };
        ensure!(
            color
                .iter()
                .all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
            "invalid UI color"
        );
    }
    Ok(())
}
