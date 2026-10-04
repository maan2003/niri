//! Runs scene ops on a Vello frame.
//!
//! The one place that turns frame-space ops into smithay draw calls. Every op is clipped by
//! the same rule: the node's damage (frame coordinates) intersected with the op's `dst`, then
//! made `dst`-relative because that is what smithay's draw calls want.

use std::cell::RefCell;

use anyhow::Context as _;
use smithay::backend::renderer::{Color32F, Frame as _, Texture as _};
use smithay::utils::{Physical, Rectangle};

use super::convert;
use super::exec::Tables;
use super::protocol::{Op, Paint, Rect, SourceColor, TextureEffect, TextureOptions};
use super::vello::{effects, VelloFrame};

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

/// Draws flat, explicitly encoded scene ops in painter order.
pub(crate) fn draw_ops(
    frame: &mut VelloFrame<'_, '_>,
    tables: &RefCell<Tables>,
    ops: &[Op],
    clip: Option<&[Rectangle<i32, Physical>]>,
) -> anyhow::Result<()> {
    for op in ops {
        match op {
            Op::Solid { dst, color } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if !damage.is_empty() {
                    let [r, g, b, a] = *color;
                    frame.draw_solid(dst, &damage, Color32F::new(r, g, b, a))?;
                }
            }
            Op::Texture {
                texture,
                src,
                dst,
                opaque,
                transform,
                alpha,
                options,
            } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let texture = tables
                    .borrow()
                    .textures
                    .get(texture)
                    .cloned()
                    .context("unknown texture")?;
                effects::draw_texture(
                    frame,
                    &texture,
                    convert::to_rect_f64(*src),
                    dst,
                    &damage,
                    &relative_to(opaque, dst),
                    convert::to_transform(*transform),
                    *alpha,
                    *options,
                )?;
            }
            Op::Paint {
                paint,
                src,
                dst,
                alpha,
            } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let lookup = |id| {
                    tables
                        .borrow()
                        .textures
                        .get(&id)
                        .cloned()
                        .context("unknown texture")
                };
                let paint = match paint {
                    Paint::Border(params) => Paint::Border(*params),
                    Paint::Shadow(params) => Paint::Shadow(*params),
                    Paint::Resize {
                        params,
                        previous,
                        next,
                    } => Paint::Resize {
                        params: *params,
                        previous: lookup(*previous)?,
                        next: lookup(*next)?,
                    },
                };
                effects::draw_paint(
                    frame,
                    &paint,
                    convert::to_rect_f64(*src),
                    dst,
                    &damage,
                    *alpha,
                )?;
            }
            Op::Capture {
                key,
                src,
                dst,
                scale,
                blur,
            } => {
                let texture = effects::capture(
                    frame,
                    convert::to_rect_f64(*src),
                    convert::to_rect(*dst),
                    *scale,
                    *blur,
                )?;
                let mut tables = tables.borrow_mut();
                if let Some(texture) = texture {
                    tables.captures.insert(*key, texture);
                } else {
                    tables.captures.remove(key);
                }
            }
            Op::Captured {
                key,
                dst,
                postprocess,
            } => {
                let dst = convert::to_rect(*dst);
                let damage = op_damage(dst, clip);
                if damage.is_empty() {
                    continue;
                }
                let Some(texture) = tables.borrow().captures.get(key).cloned() else {
                    continue;
                };
                let transform = frame.transformation().invert();
                effects::draw_texture(
                    frame,
                    &texture,
                    Rectangle::from_size(texture.size().to_f64()),
                    dst,
                    &damage,
                    &[],
                    transform,
                    1.,
                    TextureOptions {
                        color: SourceColor::Target,
                        effect: Some(TextureEffect::Postprocess(*postprocess)),
                    },
                )?;
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
    use crate::gpu::protocol::{BlendParams, ClipParams, PostprocessParams};
    fn postprocess(size: [f32; 2]) -> PostprocessParams {
        PostprocessParams {
            clip: ClipParams {
                size,
                radii: [0.; 4],
                input_to_geo: glam::Mat3::IDENTITY.to_cols_array(),
            },
            saturation: 1.,
            noise: 0.,
            background: [0.; 4],
        }
    }

    #[test]
    fn ordered_capture_and_explicit_source_colors_render() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{
            Bind as _, ExportMem as _, ImportMem as _, Offscreen as _, Renderer as _,
        };
        use smithay::utils::{Buffer, Transform};

        use crate::gpu::server::new_headless_renderer;
        let Ok(mut renderer) = new_headless_renderer() else {
            eprintln!("no Vulkan renderer available, skipping");
            return;
        };
        let texture = renderer
            .import_memory(&[80, 120, 200, 255], Fourcc::Abgr8888, (1, 1).into(), false)
            .unwrap();
        let tables = RefCell::new(Tables::default());
        tables.borrow_mut().textures.insert(1, texture);
        let mut target = renderer
            .create_buffer(Fourcc::Abgr8888, (10, 2).into())
            .unwrap();
        // Golden channels independently apply the specified BT.709->target matrices and
        // 2.2 transfer; PQ additionally applies ST2084 at reference white 203 cd/m².
        for (blend, converted, matching) in [
            (
                Some(BlendParams::HdrPq {
                    ref_lum_scale: 0.0203,
                }),
                [98u8, 106, 132, 255],
                SourceColor::Hdr,
            ),
            (
                Some(BlendParams::DisplayP3),
                [89, 119, 194, 255],
                SourceColor::DisplayP3,
            ),
            (None, [80, 120, 200, 255], SourceColor::Srgb),
        ] {
            renderer.set_blend(blend);
            {
                let mut fb = renderer.bind(&mut target).unwrap();
                let mut frame = renderer
                    .render(&mut fb, (10, 2).into(), Transform::Normal)
                    .unwrap();
                frame
                    .clear(Color32F::TRANSPARENT, &[rect(0, 0, 10, 2)])
                    .unwrap();
                let mut ops = Vec::new();
                for (i, color) in [
                    SourceColor::Target,
                    SourceColor::Srgb,
                    SourceColor::Hdr,
                    SourceColor::DisplayP3,
                    SourceColor::Srgb,
                ]
                .into_iter()
                .enumerate()
                {
                    ops.push(Op::Texture {
                        texture: 1,
                        src: Rect {
                            x: 0.,
                            y: 0.,
                            w: 1.,
                            h: 1.,
                        },
                        dst: Rect {
                            x: i as i32 * 2,
                            y: 0,
                            w: 2,
                            h: 1,
                        },
                        opaque: vec![],
                        transform: super::super::protocol::Transform::Normal,
                        alpha: 1.,
                        options: TextureOptions {
                            color,
                            effect: None,
                        },
                    });
                }
                ops.extend([
                    Op::Solid {
                        dst: Rect {
                            x: 0,
                            y: 1,
                            w: 2,
                            h: 1,
                        },
                        color: [1., 0., 0., 1.],
                    },
                    Op::Capture {
                        key: 7,
                        src: Rect {
                            x: 0.,
                            y: 0.,
                            w: 2.,
                            h: 1.,
                        },
                        dst: Rect {
                            x: 0,
                            y: 1,
                            w: 2,
                            h: 1,
                        },
                        scale: 1.,
                        blur: None,
                    },
                    // The snapshot must exclude this later overwrite.
                    Op::Solid {
                        dst: Rect {
                            x: 0,
                            y: 1,
                            w: 2,
                            h: 1,
                        },
                        color: [0., 0., 1., 1.],
                    },
                    Op::Captured {
                        key: 7,
                        dst: Rect {
                            x: 2,
                            y: 1,
                            w: 2,
                            h: 1,
                        },
                        postprocess: postprocess([2., 1.]),
                    },
                ]);
                draw_ops(&mut frame, &tables, &ops, None).unwrap();
                frame.finish().unwrap().wait().unwrap();
            }
            let mapping = renderer
                .copy_texture(
                    &target,
                    Rectangle::<i32, Buffer>::from_size((10, 2).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            let pixels = renderer.map_texture(&mapping).unwrap();
            for (i, color) in [
                SourceColor::Target,
                SourceColor::Srgb,
                SourceColor::Hdr,
                SourceColor::DisplayP3,
                SourceColor::Srgb,
            ]
            .into_iter()
            .enumerate()
            {
                let expected = if color == SourceColor::Target || color == matching {
                    [80, 120, 200, 255]
                } else {
                    converted
                };
                for (got, expected) in pixels[i * 8..i * 8 + 4].iter().zip(expected) {
                    assert!(
                        got.abs_diff(expected) <= 1,
                        "source {color:?} target {blend:?}: {:?}",
                        &pixels[i * 8..i * 8 + 4]
                    );
                }
            }
            if matches!(blend, Some(BlendParams::HdrPq { .. })) {
                // sRGB red becomes PQ(0.627404,0.069097,0.016391)*203 nits; nonzero G/B
                // also catches omitted gamut conversion and a second capture re-encode.
                let captured = &pixels[(10 + 2) * 4..(10 + 3) * 4];
                for (got, expected) in captured.iter().zip([136u8, 83, 56, 255]) {
                    assert!(
                        got.abs_diff(expected) <= 1,
                        "snapshot before overwrite {captured:?}"
                    );
                }
                let later = &pixels[10 * 4..11 * 4];
                assert!(later[2] > later[0], "later blue overwrite {later:?}");
            }
        }
    }

    #[test]
    fn rotated_backdrop_capture_retains_logical_orientation() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{
            Bind as _, ExportMem as _, Offscreen as _, Renderer as _,
        };
        use smithay::utils::{Buffer, Transform};

        use crate::gpu::server::new_headless_renderer;
        let Ok(mut renderer) = new_headless_renderer() else {
            eprintln!("no Vulkan renderer available, skipping");
            return;
        };
        for transform in [
            Transform::_90,
            Transform::_270,
            Transform::Flipped90,
            Transform::Flipped270,
        ] {
            let tables = RefCell::new(Tables::default());
            let mut target = renderer
                .create_buffer(Fourcc::Abgr8888, (4, 6).into())
                .unwrap();
            {
                let mut fb = renderer.bind(&mut target).unwrap();
                let mut frame = renderer.render(&mut fb, (4, 6).into(), transform).unwrap();
                frame
                    .clear(Color32F::TRANSPARENT, &[rect(0, 0, 6, 4)])
                    .unwrap();
                let ops = vec![
                    Op::Solid {
                        dst: Rect {
                            x: 0,
                            y: 0,
                            w: 1,
                            h: 1,
                        },
                        color: [1., 0., 0., 1.],
                    },
                    Op::Solid {
                        dst: Rect {
                            x: 1,
                            y: 0,
                            w: 1,
                            h: 1,
                        },
                        color: [0., 1., 0., 1.],
                    },
                    Op::Solid {
                        dst: Rect {
                            x: 0,
                            y: 1,
                            w: 1,
                            h: 1,
                        },
                        color: [0., 0., 1., 1.],
                    },
                    Op::Solid {
                        dst: Rect {
                            x: 1,
                            y: 1,
                            w: 1,
                            h: 1,
                        },
                        color: [1., 1., 0., 1.],
                    },
                    Op::Capture {
                        key: 1,
                        src: Rect {
                            x: 0.,
                            y: 0.,
                            w: 2.,
                            h: 2.,
                        },
                        dst: Rect {
                            x: 0,
                            y: 0,
                            w: 2,
                            h: 2,
                        },
                        scale: 1.,
                        blur: None,
                    },
                    Op::Captured {
                        key: 1,
                        dst: Rect {
                            x: 2,
                            y: 0,
                            w: 2,
                            h: 2,
                        },
                        postprocess: postprocess([2., 2.]),
                    },
                ];
                draw_ops(&mut frame, &tables, &ops, None).unwrap();
                frame.finish().unwrap().wait().unwrap();
            }
            let expected = [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 0, 255],
            ];
            for (index, (x, y)) in [(2, 0), (3, 0), (2, 1), (3, 1)].into_iter().enumerate() {
                let physical = match transform {
                    Transform::_90 => (3 - y, x),
                    Transform::_270 => (y, 5 - x),
                    Transform::Flipped90 => (3 - y, 5 - x),
                    Transform::Flipped270 => (y, x),
                    _ => unreachable!(),
                };
                let mapping = renderer
                    .copy_texture(
                        &target,
                        Rectangle::<i32, Buffer>::new(physical.into(), (1, 1).into()),
                        Fourcc::Abgr8888,
                    )
                    .unwrap();
                assert_eq!(
                    &renderer.map_texture(&mapping).unwrap()[..4],
                    &expected[index],
                    "capture {transform:?} logicalpixel {x},{y}"
                );
            }
        }
    }
    #[test]
    fn partially_offscreen_capture_keeps_unclipped_coordinate_alignment() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{
            Bind as _, ExportMem as _, Offscreen as _, Renderer as _,
        };
        use smithay::utils::{Buffer, Transform};

        use crate::gpu::server::new_headless_renderer;
        let Ok(mut renderer) = new_headless_renderer() else {
            eprintln!("no Vulkan renderer available, skipping");
            return;
        };
        let tables = RefCell::new(Tables::default());
        let mut target = renderer
            .create_buffer(Fourcc::Abgr8888, (6, 2).into())
            .unwrap();
        {
            let mut fb = renderer.bind(&mut target).unwrap();
            let mut frame = renderer
                .render(&mut fb, (6, 2).into(), Transform::Normal)
                .unwrap();
            frame
                .clear(Color32F::TRANSPARENT, &[rect(0, 0, 6, 2)])
                .unwrap();
            let ops = vec![
                Op::Solid {
                    dst: Rect {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                    color: [1., 0., 0., 1.],
                },
                Op::Solid {
                    dst: Rect {
                        x: 1,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                    color: [0., 1., 0., 1.],
                },
                Op::Solid {
                    dst: Rect {
                        x: 2,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                    color: [0., 0., 1., 1.],
                },
                Op::Capture {
                    key: 1,
                    src: Rect {
                        x: 0.,
                        y: 0.,
                        w: 4.,
                        h: 1.,
                    },
                    dst: Rect {
                        x: -1,
                        y: 0,
                        w: 4,
                        h: 1,
                    },
                    scale: 1.,
                    blur: None,
                },
                Op::Captured {
                    key: 1,
                    dst: Rect {
                        x: 2,
                        y: 1,
                        w: 4,
                        h: 1,
                    },
                    postprocess: postprocess([4., 1.]),
                },
            ];
            draw_ops(&mut frame, &tables, &ops, None).unwrap();
            frame.finish().unwrap().wait().unwrap();
        }
        let mapping = renderer
            .copy_texture(
                &target,
                Rectangle::<i32, Buffer>::new((2, 1).into(), (4, 1).into()),
                Fourcc::Abgr8888,
            )
            .unwrap();
        assert_eq!(
            renderer.map_texture(&mapping).unwrap(),
            &[0, 0, 0, 0, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255,],
            "clipped source must leave its missing pixel transparent, not stretch visible pixels"
        );
    }
}
