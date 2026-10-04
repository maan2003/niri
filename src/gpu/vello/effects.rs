//! Vello scene effects and backend-private color/resize/postprocess passes.
use smithay::backend::renderer::{Color32F, Texture as _};
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};
use vello_gpu::color::{AlphaColor, ColorSpaceTag, HueDirection, Srgb};
use vello_gpu::kurbo::{Affine, BezPath, Rect, RoundedRect, Shape};
use vello_gpu::peniko::{Fill, Gradient};
use vello_gpu::{ClearSettings, Scene, TargetInit, TextureBindings};
use wgpu::util::DeviceExt;

use super::{VelloError, VelloFrame, VelloRenderer, VelloTexture};
use crate::gpu::protocol::{
    BlendParams, BlurParams, GradientHue, GradientSpace, Paint, TextureEffect, TextureOptions,
};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    input: [[f32; 4]; 4],
    previous: [[f32; 4]; 4],
    next: [[f32; 4]; 4],
    output: [f32; 4],
    effect: [f32; 4],
    background: [f32; 4],
    flags: [f32; 4],
}

/// Backend-private fixed passes, not a user shader compiler.
pub struct Effects {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
}
impl Effects {
    pub fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("niri Vello effects"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("niri Vello fixed effects"),
            source: wgpu::ShaderSource::Wgsl(include_str!("effects.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("niri Vello effects"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba16Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { layout, pipeline }
    }

    // Inputs and output must be renderer-owned resident textures. Foreign images are
    // copied through the renderer's import synchronization bracket before this call.
    fn process(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        src: &VelloTexture,
        next: &VelloTexture,
        output: &VelloTexture,
        params: &Params,
    ) {
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vello effect parameters"),
            contents: bytemuck::bytes_of(params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(next.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buffer.as_entire_binding(),
                },
            ],
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Vello fixed effect"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output.view(),
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &binding, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}
fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn mat4(m: glam::Mat3) -> [[f32; 4]; 4] {
    let [a, b, _c, d, e, _f, g, h, i] = m.to_cols_array();
    [
        [a, b, 0., 0.],
        [d, e, 0., 0.],
        [g, h, i, 0.],
        [0., 0., 0., 1.],
    ]
}
fn affine(m: glam::Mat3) -> Affine {
    let [a, b, _, c, d, _, e, f, _] = m.to_cols_array();
    Affine::new([a as f64, b as f64, c as f64, d as f64, e as f64, f as f64])
}
fn color(c: [f32; 4]) -> AlphaColor<Srgb> {
    AlphaColor::new(c)
}
fn premul_color(mut c: [f32; 4]) -> AlphaColor<Srgb> {
    if c[3] > 0. {
        for i in 0..3 {
            c[i] /= c[3];
        }
    }
    color(c)
}
fn rounded(size: [f32; 2], r: [f32; 4]) -> BezPath {
    RoundedRect::new(
        0.,
        0.,
        size[0] as f64,
        size[1] as f64,
        (r[0] as f64, r[1] as f64, r[2] as f64, r[3] as f64),
    )
    .to_path(0.1)
}
fn image(
    scene: &mut Scene,
    bindings: &mut TextureBindings,
    texture: &VelloTexture,
    transform: Affine,
) {
    use vello_common::paint::{Image, ImageSource};
    use vello_common::TextureId;
    let id = TextureId(1);
    bindings.insert(id, texture.view().clone());
    let size = texture.size();
    let source = ImageSource::external_texture(
        id,
        vello_common::geometry::RectU16::new(0, 0, size.w as u16, size.h as u16),
        !texture.force_opaque(),
    );
    // This helper paints exactly the image extent, never out-of-bounds coordinates.
    // Reflect is identity there and avoids the pinned backend's Pad pre-clamp to
    // size-1 before its bilinear half-texel adjustment (which blurs the last pixel).
    scene.set_paint(Image {
        image: source,
        sampler: vello_gpu::peniko::ImageSampler::default()
            .with_extend(vello_gpu::peniko::Extend::Reflect),
    });
    let transform = if texture.flipped() {
        transform * Affine::translate((0., size.h as f64)) * Affine::scale_non_uniform(1., -1.)
    } else {
        transform
    };
    scene.set_paint_transform(transform);
    scene.fill_rect(&Rect::new(
        0.,
        0.,
        scene.width() as f64,
        scene.height() as f64,
    ));
    scene.reset_paint_transform();
}

pub(crate) fn blend(color: Color32F, params: Option<BlendParams>) -> Color32F {
    match params {
        Some(BlendParams::HdrPq { ref_lum_scale }) => srgb_to_pq(color, ref_lum_scale),
        Some(BlendParams::DisplayP3) => srgb_to_p3(color),
        None => color,
    }
}
fn base_params(size: Size<i32, Buffer>, blend: Option<BlendParams>) -> Params {
    let (mode, reference_luminance) = match blend {
        None => (0., 0.),
        Some(BlendParams::HdrPq { ref_lum_scale }) => (1., ref_lum_scale),
        Some(BlendParams::DisplayP3) => (2., 0.),
    };
    Params {
        input: mat4(glam::Mat3::IDENTITY),
        previous: mat4(glam::Mat3::IDENTITY),
        next: mat4(glam::Mat3::IDENTITY),
        output: [size.w as f32, size.h as f32, mode, reference_luminance],
        effect: [0., 1., 0., 0.],
        background: [0.; 4],
        flags: [0.; 4],
    }
}
fn process(
    renderer: &mut VelloRenderer,
    src: &VelloTexture,
    next: &VelloTexture,
    params: &Params,
) -> Result<VelloTexture, VelloError> {
    let src = renderer.resident_texture(src)?;
    let next = renderer.resident_texture(next)?;
    let output = renderer
        .create_effect_texture((params.output[0] as i32, params.output[1] as i32).into())?;
    renderer.submit(
        &[src.clone(), next.clone(), output.clone()],
        |renderer, encoder| {
            renderer
                .effects
                .process(renderer.device(), encoder, &src, &next, &output, params);
            Ok(())
        },
    )?;
    Ok(output)
}
/// Clamp-to-edge resampling for the out-of-bounds source case that native image
/// Reflect cannot represent. This is a fixed Vello-side pass, not another backend.
pub(crate) fn resample_texture(
    renderer: &mut VelloRenderer,
    texture: &VelloTexture,
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
    transform: Transform,
    nearest: bool,
) -> Result<VelloTexture, VelloError> {
    let mut params = base_params((dst.size.w, dst.size.h).into(), None);
    params.input = mat4(super::texture_matrix(texture, src, dst, transform));
    if nearest {
        params.flags[3] = -1.;
    }
    process(renderer, texture, texture, &params)
}

fn render_native(
    renderer: &mut VelloRenderer,
    scene: &Scene,
    bindings: &TextureBindings,
) -> Result<VelloTexture, VelloError> {
    let output =
        renderer.create_effect_texture((scene.width() as i32, scene.height() as i32).into())?;
    renderer.render_scene(
        scene,
        bindings,
        &output,
        TargetInit::Clear(ClearSettings::default()),
    )?;
    Ok(output)
}
fn clip_image(
    renderer: &mut VelloRenderer,
    texture: &VelloTexture,
    path: Option<BezPath>,
    fade: Option<(Affine, [f32; 4])>,
) -> Result<VelloTexture, VelloError> {
    let texture = renderer.resident_texture(texture)?;
    let size = texture.size();
    let mut scene = Scene::new(size.w as u16, size.h as u16);
    let mut bindings = TextureBindings::new();
    scene.push_layer(path.as_ref(), None, None, None, None);
    image(&mut scene, &mut bindings, &texture, Affine::IDENTITY);
    if let Some((transform, cutoff)) = fade {
        use vello_gpu::peniko::{BlendMode, Compose, Mix};
        scene.push_layer(
            None,
            Some(BlendMode::new(Mix::Normal, Compose::DestIn)),
            None,
            None,
            None,
        );
        scene.set_paint(
            Gradient::new_linear((cutoff[0] as f64, 0.), (cutoff[1] as f64, 0.))
                .with_stops([color([1., 1., 1., 1.]), color([1., 1., 1., 0.])]),
        );
        scene.set_paint_transform(transform);
        scene.fill_rect(&Rect::new(0., 0., size.w as f64, size.h as f64));
        scene.pop_layer();
    }
    scene.pop_layer();
    render_native(renderer, &scene, &bindings)
}

/// Draw an image with independent source encoding and an optional fixed effect.
#[allow(clippy::too_many_arguments)]
pub fn draw_texture(
    frame: &mut VelloFrame<'_, '_>,
    texture: &VelloTexture,
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
    damage: &[Rectangle<i32, Physical>],
    opaque: &[Rectangle<i32, Physical>],
    transform: Transform,
    alpha: f32,
    options: TextureOptions,
) -> Result<(), VelloError> {
    let blend = options.color.conversion_to(frame.blend());
    if options.effect.is_none() && blend.is_none() {
        return frame.draw_texture(texture, src, dst, damage, opaque, transform, alpha);
    }
    let uv = super::texture_matrix(texture, src, dst, transform);
    let mut params = base_params((dst.size.w, dst.size.h).into(), None);
    params.input = mat4(uv);
    if frame.image_nearest(src, dst, transform) {
        params.flags[3] = -1.;
    }
    if let Some(TextureEffect::Postprocess(postprocess)) = options.effect {
        params.effect = [0., postprocess.saturation, postprocess.noise, 2.];
        params.background = postprocess.background;
        params.flags[0] = dst.loc.x as f32;
        params.flags[1] = dst.loc.y as f32;
    }
    let renderer = frame.renderer_mut()?;
    let mut output = process(renderer, texture, texture, &params)?;
    let clip = match options.effect {
        Some(TextureEffect::Clip(clip)) => Some(clip),
        Some(TextureEffect::Postprocess(postprocess)) => Some(postprocess.clip),
        _ => None,
    };
    let path = clip.map(|clip| {
        let geo_to_dst = glam::Mat3::from_scale(glam::vec2(dst.size.w as f32, dst.size.h as f32))
            * (glam::Mat3::from_cols_array(&clip.input_to_geo) * uv).inverse()
            * glam::Mat3::from_scale(glam::vec2(1. / clip.size[0], 1. / clip.size[1]));
        affine(geo_to_dst) * rounded(clip.size, clip.radii)
    });
    let fade = match options.effect {
        Some(TextureEffect::Fade { cutoff }) if cutoff[0] < cutoff[1] => {
            let tex_to_dst =
                glam::Mat3::from_scale(glam::vec2(dst.size.w as f32, dst.size.h as f32))
                    * uv.inverse();
            Some((affine(tex_to_dst), [cutoff[0], cutoff[1], 0., 0.]))
        }
        _ => None,
    };
    if path.is_some() || fade.is_some() {
        output = clip_image(renderer, &output, path, fade)?;
    }
    if blend.is_some() {
        params = base_params(output.size(), blend);
        output = process(renderer, &output, &output, &params)?;
    }
    frame.draw_texture(
        &output,
        Rectangle::from_size(output.size().to_f64()),
        dst,
        damage,
        &[],
        Transform::Normal,
        alpha,
    )
}

/// Native Vello Gaussian blur. The former pyramid controls now select Gaussian deviation.
pub fn blur(
    renderer: &mut VelloRenderer,
    src: &VelloTexture,
    params: BlurParams,
) -> Result<VelloTexture, VelloError> {
    let src = renderer.resident_texture(src)?;
    let size = src.size();
    let mut scene = Scene::new(size.w as u16, size.h as u16);
    let mut bindings = TextureBindings::new();
    // Approximate the old pyramid's increasing footprint, bounded by the image extent.
    let deviation = (params.offset as f32 * 2_f32.powi(params.passes.clamp(1, 31) as i32 - 1))
        .min(size.w.max(size.h) as f32);
    scene.push_filter_layer(vello_common::filter_effects::Filter::from_function(
        vello_common::filter_effects::FilterFunction::Blur { radius: deviation },
    ));
    image(&mut scene, &mut bindings, &src, Affine::IDENTITY);
    scene.pop_layer();
    render_native(renderer, &scene, &bindings)
}

/// Snapshot the already-painted backdrop, with the same destination clamping as drawing.
pub fn capture(
    frame: &mut VelloFrame<'_, '_>,
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
    scale: f32,
    blur_params: Option<BlurParams>,
) -> Result<Option<VelloTexture>, VelloError> {
    frame.flush()?;
    if dst
        .intersection(Rectangle::from_size(frame.size()))
        .is_none()
    {
        return Ok(None);
    }
    let transform = frame.transform();
    let physical = frame.root_transform().transform_rect_bbox(Rect::new(
        dst.loc.x as f64,
        dst.loc.y as f64,
        (dst.loc.x + dst.size.w) as f64,
        (dst.loc.y + dst.size.h) as f64,
    ));
    let size = Size::<i32, Buffer>::from((
        (src.size.w * scale as f64).round() as i32,
        (src.size.h * scale as f64).round() as i32,
    ));
    let size = transform.transform_size(size);
    if size.w <= 0 || size.h <= 0 {
        return Ok(None);
    }
    let target = frame.target_texture().clone();
    let source_size = target.size();
    let mut params = base_params(size, None);
    // Keep the full destination's coordinate system: cropped-off pixels are transparent
    // rather than shrinking the snapshot and stretching visible content on redraw.
    params.flags[2] = -1.;
    params.input = mat4(
        glam::Mat3::from_translation(glam::vec2(
            physical.x0 as f32 / source_size.w as f32,
            physical.y0 as f32 / source_size.h as f32,
        )) * glam::Mat3::from_scale(glam::vec2(
            physical.width() as f32 / source_size.w as f32,
            physical.height() as f32 / source_size.h as f32,
        )),
    );
    let renderer = frame.renderer_mut()?;
    let output = process(renderer, &target, &target, &params)?;
    Ok(Some(if let Some(params) = blur_params {
        blur(renderer, &output, params)?
    } else {
        output
    }))
}

/// Rasterize one typed compositor paint, preserving premultiplied resize mixing.
pub fn draw_paint(
    frame: &mut VelloFrame<'_, '_>,
    paint: &Paint<VelloTexture>,
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
    damage: &[Rectangle<i32, Physical>],
    alpha: f32,
) -> Result<(), VelloError> {
    let blend = frame.blend();
    let input = glam::Mat3::from_translation(glam::vec2(src.loc.x as f32, src.loc.y as f32))
        * glam::Mat3::from_scale(glam::vec2(src.size.w as f32, src.size.h as f32));
    let renderer = frame.renderer_mut()?;
    let mut output = match paint {
        Paint::Resize {
            params,
            previous,
            next,
        } => {
            let geo = glam::Mat3::from_cols_array(&params.input_to_geometry) * input;
            let mut pass = base_params((dst.size.w, dst.size.h).into(), None);
            pass.input = mat4(geo);
            pass.previous = mat4(glam::Mat3::from_cols_array(&params.previous_from_geometry));
            pass.next = mat4(glam::Mat3::from_cols_array(&params.next_from_geometry));
            pass.effect = [params.progress, 1., 0., 1.];
            let mut output = process(renderer, previous, next, &pass)?;
            if params.clip_to_geometry {
                let size = params.geometry_size;
                let geo_to_dst =
                    glam::Mat3::from_scale(glam::vec2(dst.size.w as f32, dst.size.h as f32))
                        * geo.inverse()
                        * glam::Mat3::from_scale(glam::vec2(1. / size[0], 1. / size[1]));
                output = clip_image(
                    renderer,
                    &output,
                    Some(affine(geo_to_dst) * rounded(size, params.radii)),
                    None,
                )?;
            }
            output
        }
        Paint::Border(params) => {
            let mut scene = Scene::new(dst.size.w as u16, dst.size.h as u16);
            let geometry = params.geometry;
            let size = geometry.size;
            let geo = glam::Mat3::from_cols_array(&geometry.input_to_geo) * input;
            let geo_to_dst =
                glam::Mat3::from_scale(glam::vec2(dst.size.w as f32, dst.size.h as f32))
                    * geo.inverse();
            let mut path = rounded(size, geometry.radii);
            if params.width > 0. && size[0] > 2. * params.width && size[1] > 2. * params.width {
                let inner = Affine::translate((params.width as f64, params.width as f64))
                    * rounded(
                        [size[0] - 2. * params.width, size[1] - 2. * params.width],
                        geometry.radii.map(|r| (r - params.width).max(0.)),
                    );
                path.extend(inner);
                scene.set_fill_rule(Fill::EvenOdd);
            }
            let [vx, vy] = params.gradient_vector;
            let [ox, oy] = params.gradient_offset;
            let mut origin = glam::vec2(-ox, -oy);
            if (vx < 0. && vy >= 0.) || (vx >= 0. && vy < 0.) {
                origin.x += params.gradient_width;
            }
            if vy < 0. {
                origin -= glam::vec2(vx, vy);
            }
            let end = origin + glam::vec2(vx, vy);
            let cs = match params.gradient_space {
                GradientSpace::Srgb => ColorSpaceTag::Srgb,
                GradientSpace::LinearSrgb => ColorSpaceTag::LinearSrgb,
                GradientSpace::Oklab => ColorSpaceTag::Oklab,
                GradientSpace::Oklch => ColorSpaceTag::Oklch,
            };
            let hue = match params.gradient_hue {
                GradientHue::Shorter => HueDirection::Shorter,
                GradientHue::Longer => HueDirection::Longer,
                GradientHue::Increasing => HueDirection::Increasing,
                GradientHue::Decreasing => HueDirection::Decreasing,
            };
            scene.set_paint(
                Gradient::new_linear(
                    (origin.x as f64, origin.y as f64),
                    (end.x as f64, end.y as f64),
                )
                .with_interpolation_cs(cs)
                .with_hue_direction(hue)
                .with_stops([color(params.color_from), color(params.color_to)]),
            );
            scene.set_transform(affine(geo_to_dst));
            scene.fill_path(&path);
            render_native(renderer, &scene, &TextureBindings::new())?
        }
        Paint::Shadow(params) => {
            let mut scene = Scene::new(dst.size.w as u16, dst.size.h as u16);
            let geometry = params.geometry;
            let geo = glam::Mat3::from_cols_array(&geometry.input_to_geo) * input;
            let geo_to_dst =
                glam::Mat3::from_scale(glam::vec2(dst.size.w as f32, dst.size.h as f32))
                    * geo.inverse();
            if let Some(window) = params.window {
                let mut clip = Rect::new(0., 0., dst.size.w as f64, dst.size.h as f64).to_path(0.1);
                let window_to_dst =
                    glam::Mat3::from_scale(glam::vec2(dst.size.w as f32, dst.size.h as f32))
                        * (glam::Mat3::from_cols_array(&window.input_to_geo) * input).inverse();
                clip.extend(affine(window_to_dst) * rounded(window.size, window.radii));
                scene.set_fill_rule(Fill::EvenOdd);
                scene.push_layer(Some(&clip), None, None, None, None);
            }
            scene.set_paint(premul_color(params.color));
            scene.set_transform(affine(geo_to_dst));
            if params.sigma < 0.1 {
                scene.fill_path(&rounded(geometry.size, geometry.radii));
            } else {
                scene.fill_blurred_rounded_rect(
                    &Rect::new(0., 0., geometry.size[0] as f64, geometry.size[1] as f64),
                    geometry.radii[0],
                    params.sigma,
                    false,
                );
            }
            if params.window.is_some() {
                scene.pop_layer();
            }
            render_native(renderer, &scene, &TextureBindings::new())?
        }
    };
    if blend.is_some() {
        let params = base_params(output.size(), blend);
        output = process(renderer, &output, &output, &params)?;
    }
    frame.draw_texture(
        &output,
        Rectangle::from_size(output.size().to_f64()),
        dst,
        damage,
        &[],
        Transform::Normal,
        alpha,
    )
}

/// CPU counterpart of the fixed WGSL conversion: encodes an electrical sRGB premultiplied
/// color into PQ/BT.2020 for the given SDR reference luminance scale (reference / 10000).
#[allow(clippy::excessive_precision)] // the ST 2084 constants, as specified
pub fn srgb_to_pq(color: Color32F, ref_lum_scale: f32) -> Color32F {
    let a = color.a();
    let unpremul = |c: f32| if a > 0. { c / a } else { c };

    let pq = |lin: f32| {
        const M1: f32 = 0.1593017578125;
        const M2: f32 = 78.84375;
        const C1: f32 = 0.8359375;
        const C2: f32 = 18.8515625;
        const C3: f32 = 18.6875;
        let y = lin.clamp(0., 1.).powf(M1);
        ((C1 + C2 * y) / (1. + C3 * y)).powf(M2)
    };

    let r = unpremul(color.r()).max(0.).powf(2.2);
    let g = unpremul(color.g()).max(0.).powf(2.2);
    let b = unpremul(color.b()).max(0.).powf(2.2);

    // BT.709 -> BT.2020, linear light, D65.
    let r2020 = 0.627404 * r + 0.329283 * g + 0.043313 * b;
    let g2020 = 0.069097 * r + 0.919540 * g + 0.011362 * b;
    let b2020 = 0.016391 * r + 0.088013 * g + 0.895595 * b;

    Color32F::new(
        pq(r2020 * ref_lum_scale) * a,
        pq(g2020 * ref_lum_scale) * a,
        pq(b2020 * ref_lum_scale) * a,
        a,
    )
}

/// CPU counterpart of the fixed WGSL P3 conversion: gamut-maps an electrical sRGB
/// premultiplied color into Display P3 with the same 2.2 decode/encode.
pub fn srgb_to_p3(color: Color32F) -> Color32F {
    let a = color.a();
    let unpremul = |c: f32| if a > 0. { c / a } else { c };

    let r = unpremul(color.r()).max(0.).powf(2.2);
    let g = unpremul(color.g()).max(0.).powf(2.2);
    let b = unpremul(color.b()).max(0.).powf(2.2);

    // BT.709 -> Display P3, linear light, D65.
    let rp3 = 0.822462 * r + 0.177538 * g;
    let gp3 = 0.033194 * r + 0.966806 * g;
    let bp3 = 0.017083 * r + 0.072397 * g + 0.910520 * b;

    let enc = |lin: f32| lin.max(0.).powf(1. / 2.2);
    Color32F::new(enc(rp3) * a, enc(gp3) * a, enc(bp3) * a, a)
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{ExportMem as _, ImportMem as _};

    use super::*;
    use crate::gpu::protocol::{
        BorderParams, ClipParams, ResizeParams, RoundedGeometry, ShadowParams,
    };
    use crate::gpu::server::new_headless_renderer;

    fn pixel(renderer: &mut VelloRenderer, texture: &VelloTexture, x: i32, y: i32) -> [u8; 4] {
        let mapping = renderer
            .copy_texture(
                texture,
                Rectangle::new((x, y).into(), (1, 1).into()),
                Fourcc::Abgr8888,
            )
            .unwrap();
        renderer.map_texture(&mapping).unwrap()[..4]
            .try_into()
            .unwrap()
    }

    /// Representative native decoration/clip/blur/fade states for visual review.
    #[test]
    #[ignore = "requires Vulkan; set NIRI_EFFECTS_GRID_PNG to capture"]
    fn native_effects_visual_grid() {
        use smithay::backend::renderer::{Bind as _, Frame as _, Offscreen as _, Renderer as _};
        let mut renderer = new_headless_renderer().expect("Vulkan visual renderer");
        let mut target = renderer
            .create_buffer(Fourcc::Abgr8888, (640, 360).into())
            .unwrap();
        let mut bytes = Vec::new();
        for y in 0..32 {
            for x in 0..32 {
                bytes.extend_from_slice(&match (x < 16, y < 16) {
                    (true, true) => [255, 100, 20, 255],
                    (false, true) => [30, 180, 255, 255],
                    (true, false) => [100, 210, 80, 255],
                    _ => [180, 60, 230, 255],
                });
            }
        }
        let texture = renderer
            .import_memory(&bytes, Fourcc::Abgr8888, (32, 32).into(), false)
            .unwrap();
        let blurred = blur(
            &mut renderer,
            &texture,
            BlurParams {
                passes: 2,
                offset: 1.5,
            },
        )
        .unwrap();
        let src = Rectangle::from_size((1., 1.).into());
        let rect = |x, y, w, h| Rectangle::<i32, Physical>::new((x, y).into(), (w, h).into());
        {
            let mut fb = renderer.bind(&mut target).unwrap();
            let mut frame = renderer
                .render(&mut fb, (640, 360).into(), Transform::Normal)
                .unwrap();
            frame
                .clear(Color32F::new(0.12, 0.14, 0.18, 1.), &[rect(0, 0, 640, 360)])
                .unwrap();
            let dst = rect(30, 30, 250, 140);
            let paint = Paint::Border(BorderParams {
                geometry: RoundedGeometry {
                    size: [250., 140.],
                    radii: [22., 6., 35., 12.],
                    input_to_geo: glam::Mat3::from_scale(glam::vec2(250., 140.)).to_cols_array(),
                },
                width: 7.,
                color_from: [1., 0.3, 0., 1.],
                color_to: [0., 0.5, 1., 1.],
                gradient_offset: [0., 0.],
                gradient_width: 0.,
                gradient_vector: [250., 140.],
                gradient_space: GradientSpace::Oklch,
                gradient_hue: GradientHue::Shorter,
            });
            draw_paint(
                &mut frame,
                &paint,
                src,
                dst,
                &[Rectangle::from_size(dst.size)],
                1.,
            )
            .unwrap();
            let dst = rect(320, 15, 280, 190);
            let geometry = glam::Mat3::from_translation(glam::vec2(-35., -45.))
                * glam::Mat3::from_scale(glam::vec2(280., 190.));
            let geometry = RoundedGeometry {
                size: [200., 100.],
                radii: [20.; 4],
                input_to_geo: geometry.to_cols_array(),
            };
            let paint = Paint::Shadow(ShadowParams {
                geometry,
                window: Some(geometry),
                color: [0., 0., 0., 0.9],
                sigma: 15.,
            });
            draw_paint(
                &mut frame,
                &paint,
                src,
                dst,
                &[Rectangle::from_size(dst.size)],
                1.,
            )
            .unwrap();
            let dst = rect(355, 60, 200, 100);
            draw_texture(
                &mut frame,
                &texture,
                Rectangle::from_size(texture.size().to_f64()),
                dst,
                &[Rectangle::from_size(dst.size)],
                &[],
                Transform::Normal,
                1.,
                TextureOptions {
                    color: crate::gpu::protocol::SourceColor::Srgb,
                    effect: Some(TextureEffect::Clip(ClipParams {
                        size: [200., 100.],
                        radii: [20., 5., 32., 12.],
                        input_to_geo: glam::Mat3::IDENTITY.to_cols_array(),
                    })),
                },
            )
            .unwrap();
            for (dst, image) in [
                (rect(30, 225, 150, 95), &texture),
                (rect(215, 225, 150, 95), &blurred),
            ] {
                frame
                    .draw_texture(
                        image,
                        Rectangle::from_size(image.size().to_f64()),
                        dst,
                        &[Rectangle::from_size(dst.size)],
                        &[],
                        Transform::Normal,
                        1.,
                    )
                    .unwrap();
            }
            let dst = rect(400, 225, 150, 95);
            draw_texture(
                &mut frame,
                &blurred,
                Rectangle::from_size(blurred.size().to_f64()),
                dst,
                &[Rectangle::from_size(dst.size)],
                &[],
                Transform::Normal,
                1.,
                TextureOptions {
                    color: crate::gpu::protocol::SourceColor::Srgb,
                    effect: Some(TextureEffect::Fade {
                        cutoff: [0.2, 0.85],
                    }),
                },
            )
            .unwrap();
            frame.finish().unwrap().wait().unwrap();
        }
        let mapping = renderer
            .copy_texture(
                &target,
                Rectangle::from_size((640, 360).into()),
                Fourcc::Abgr8888,
            )
            .unwrap();
        let pixels = renderer.map_texture(&mapping).unwrap();
        let pixel = |x: usize, y: usize| &pixels[(y * 640 + x) * 4..(y * 640 + x) * 4 + 4];
        assert_eq!(
            pixel(120, 80),
            pixel(0, 0),
            "border interior must remain transparent"
        );
        assert!(pixel(100, 31)[0] > 100, "gradient border visible");
        for (a, b) in pixel(400, 275).iter().zip(pixel(215, 275)) {
            assert!(
                a.abs_diff(*b) <= 1,
                "opaque fade differs only by RGBA8 layer rounding"
            );
        }
        assert_eq!(pixel(549, 275), pixel(0, 0), "fade ends transparent");
        if let Some(path) = std::env::var_os("NIRI_EFFECTS_GRID_PNG") {
            let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), 640, 360);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(pixels)
                .unwrap();
        }
    }

    /// A source-over crossfade would produce the wrong alpha and blue component.
    #[test]
    fn fixed_resize_mix_and_native_gaussian_render() {
        let Ok(mut renderer) = new_headless_renderer() else {
            eprintln!("no Vulkan renderer available, skipping");
            return;
        };
        use smithay::backend::renderer::{Bind as _, Frame as _, Offscreen as _, Renderer as _};
        let previous = renderer
            .import_memory(
                &[
                    0, 0, 64, 128, 0, 128, 0, 128, 102, 0, 0, 128, 32, 32, 32, 128,
                ],
                Fourcc::Abgr8888,
                (2, 2).into(),
                false,
            )
            .unwrap();
        let next = renderer
            .import_memory(
                &[
                    128, 0, 0, 192, 0, 64, 128, 192, 64, 0, 64, 192, 0, 0, 0, 192,
                ],
                Fourcc::Abgr8888,
                (2, 2).into(),
                false,
            )
            .unwrap();
        let paint = Paint::Resize {
            params: ResizeParams {
                input_to_geometry: glam::Mat3::from_scale(glam::vec2(0.5, 1.)).to_cols_array(),
                geometry_size: [1., 1.],
                previous_from_geometry: glam::Mat3::from_translation(glam::vec2(0., 0.25))
                    .to_cols_array(),
                next_from_geometry: glam::Mat3::from_translation(glam::vec2(0.5, -0.25))
                    .to_cols_array(),
                progress: 0.25,
                radii: [0.; 4],
                clip_to_geometry: false,
            },
            previous,
            next,
        };
        let mut output = renderer
            .create_buffer(Fourcc::Abgr8888, (1, 1).into())
            .unwrap();
        {
            let mut fb = renderer.bind(&mut output).unwrap();
            let mut frame = renderer
                .render(&mut fb, (1, 1).into(), Transform::Normal)
                .unwrap();
            let dst = Rectangle::from_size((1, 1).into());
            frame.clear(Color32F::TRANSPARENT, &[dst]).unwrap();
            draw_paint(
                &mut frame,
                &paint,
                Rectangle::from_size((1., 1.).into()),
                dst,
                &[dst],
                1.,
            )
            .unwrap();
            frame.finish().unwrap().wait().unwrap();
        }
        let p = pixel(&mut renderer, &output, 0, 0);
        for (got, expected) in p.into_iter().zip([77, 16, 32, 144]) {
            assert!(got.abs_diff(expected) <= 1, "mixed pixel {p:?}");
        }

        // The native blur must distribute energy, not merely return the original image.
        let mut bytes = vec![0u8; 9 * 9 * 4];
        bytes[(4 * 9 + 4) * 4..(4 * 9 + 4) * 4 + 4].fill(255);
        let impulse = renderer
            .import_memory(&bytes, Fourcc::Abgr8888, (9, 9).into(), false)
            .unwrap();
        let blurred = blur(
            &mut renderer,
            &impulse,
            BlurParams {
                passes: 1,
                offset: 1.,
            },
        )
        .unwrap();
        let center = pixel(&mut renderer, &blurred, 4, 4);
        let neighbor = pixel(&mut renderer, &blurred, 5, 4);
        assert!(center[3] > 0 && center[3] < 255, "center {center:?}");
        assert!(
            neighbor[3] > 0 && neighbor[3] < center[3],
            "neighbor {neighbor:?}"
        );
        assert_eq!(
            pixel(&mut renderer, &blurred, 3, 4),
            neighbor,
            "Gaussian symmetry"
        );
    }

    #[test]
    fn typed_matrix_affine_and_wgsl_packing_agree() {
        let matrix = [2., 3., 0., 5., 7., 0., 11., 13., 1.];
        let m = glam::Mat3::from_cols_array(&matrix);
        let p = m * glam::vec3(17., 19., 1.);
        assert_eq!([p.x, p.y], [140., 197.]);
        let p = affine(m) * vello_gpu::kurbo::Point::new(17., 19.);
        assert_eq!([p.x, p.y], [140., 197.]);
        let p = glam::Mat4::from_cols_array_2d(&mat4(m)) * glam::vec4(17., 19., 1., 1.);
        assert_eq!([p.x, p.y], [140., 197.], "WGSL packing");
    }

    #[test]
    fn srgb_to_pq_reference_values() {
        let scale = (203. / 10000.) as f32;

        // Opaque white at reference luminance 203 cd/m²: PQ(0.0203) ≈ 0.5806.
        let white = srgb_to_pq(Color32F::new(1., 1., 1., 1.), scale);
        assert!((white.r() - 0.5806).abs() < 0.002, "got {}", white.r());
        // BT.709 white maps to BT.2020 white (rows sum to 1) => neutral stays neutral.
        assert!((white.r() - white.g()).abs() < 0.0005);
        assert!((white.g() - white.b()).abs() < 0.0005);

        // Black stays (essentially) black (PQ(0) is ~4e-7) and alpha is preserved.
        let black = srgb_to_pq(Color32F::new(0., 0., 0., 0.5), scale);
        assert!(black.r() < 1e-6, "got {}", black.r());
        assert_eq!(black.a(), 0.5);

        // Premultiplied 50% white: unpremultiplied value is 1.0, so the encoded result is
        // the white point rescaled by alpha.
        let half = srgb_to_pq(Color32F::new(0.5, 0.5, 0.5, 0.5), scale);
        assert!((half.r() - white.r() * 0.5).abs() < 0.0005);
    }

    #[test]
    fn srgb_to_p3_reference_values() {
        // Neutrals are untouched: the matrix rows sum to 1 and decode/encode cancel out.
        for v in [0., 0.25, 0.5, 1.] {
            let c = srgb_to_p3(Color32F::new(v, v, v, 1.));
            assert!((c.r() - v).abs() < 0.001, "got {} for {}", c.r(), v);
            assert!((c.r() - c.g()).abs() < 0.001);
            assert!((c.g() - c.b()).abs() < 0.001);
        }

        // Pure sRGB red is desaturated into P3: linear 0.822462 -> 0.9151 encoded, with a
        // little green and blue mixed in.
        let red = srgb_to_p3(Color32F::new(1., 0., 0., 1.));
        assert!((red.r() - 0.9151).abs() < 0.001, "got {}", red.r());
        assert!(red.g() > 0.1 && red.g() < 0.3, "got {}", red.g());
        assert!(red.b() > 0.1 && red.b() < 0.3, "got {}", red.b());

        // Alpha is preserved and premultiplication round-trips.
        let half = srgb_to_p3(Color32F::new(0.5, 0.5, 0.5, 0.5));
        assert!((half.r() - 0.5).abs() < 0.001, "got {}", half.r());
        assert_eq!(half.a(), 0.5);
    }
}
