use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context as _;
macro_rules! ensure { ($condition:expr, $($args:tt)*) => { if !$condition { return Err(anyhow::anyhow!($($args)*).into()); } }; }
use glam::{Mat3, Vec3};
use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::format::{has_alpha, FormatSet};
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::drm::DrmNode;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, DebugFlags, ExportMem, Frame, ImportDma, ImportMem, Offscreen,
    Renderer, RendererSuper, Texture, TextureFilter, TextureMapping,
};
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};
use vello_common::geometry::RectU16;
use vello_common::paint::ImageSource;
use vello_common::TextureId;
use vello_gpu::color::{AlphaColor, Srgb};
use vello_gpu::kurbo::{Affine, Circle, Rect, Shape};
use vello_gpu::peniko::{Blob, Extend, FontData, ImageBrush, ImageQuality};
use vello_gpu::{RenderSize, RenderTargetConfig, Resources, Scene, TargetInit, TextureBindings};

use super::{dmabuf, effects, validate};
use crate::gpu::protocol::{BlendParams, UiOp, UiScene};

#[derive(Debug)]
pub(crate) struct VelloError(anyhow::Error);
impl fmt::Display for VelloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for VelloError {}
impl From<anyhow::Error> for VelloError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}

#[derive(Debug)]
pub(super) struct Imported {
    pub dmabuf: Dmabuf,
    pub render: bool,
    pub identity: (u64, u64),
}
#[derive(Debug)]
pub(super) struct TextureInner {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    id: TextureId,
    context: ContextId<VelloTexture>,
    format: Option<Fourcc>,
    flipped: bool,
    pub imported: Option<Imported>,
}
/// Cloneable ownership, not just a raw image name: frames retain images until submission retires.
#[derive(Debug, Clone)]
pub(crate) struct VelloTexture(pub(super) Arc<TextureInner>);
impl VelloTexture {
    pub fn texture(&self) -> &wgpu::Texture {
        &self.0.texture
    }
    pub fn view(&self) -> &wgpu::TextureView {
        &self.0.view
    }
    pub fn flipped(&self) -> bool {
        self.0.flipped
    }
    pub fn force_opaque(&self) -> bool {
        self.0.format.is_some_and(|f| !has_alpha(f))
    }
    pub fn is_imported(&self) -> bool {
        self.0.imported.is_some()
    }
}
impl Texture for VelloTexture {
    fn width(&self) -> u32 {
        self.texture().width()
    }
    fn height(&self) -> u32 {
        self.texture().height()
    }
    fn format(&self) -> Option<Fourcc> {
        self.0.format
    }
}
#[derive(Debug)]
pub(crate) struct VelloTarget<'buffer> {
    texture: VelloTexture,
    _borrow: PhantomData<&'buffer mut ()>,
}
impl Texture for VelloTarget<'_> {
    fn width(&self) -> u32 {
        self.texture.width()
    }
    fn height(&self) -> u32 {
        self.texture.height()
    }
    fn format(&self) -> Option<Fourcc> {
        self.texture.format()
    }
}
#[derive(Debug)]
pub(crate) struct VelloMapping {
    bytes: Vec<u8>,
    size: Size<i32, Buffer>,
    format: Fourcc,
}
impl Texture for VelloMapping {
    fn width(&self) -> u32 {
        self.size.w as u32
    }
    fn height(&self) -> u32 {
        self.size.h as u32
    }
    fn format(&self) -> Option<Fourcc> {
        Some(self.format)
    }
}
impl TextureMapping for VelloMapping {
    fn flipped(&self) -> bool {
        false
    }
}

/// Wall-clock submission latency includes DMA-buf waits, encoding and fence waits,
/// not just GPU execution. Reset each reporting interval; never call this FPS.
#[derive(Default)]
struct SubmissionMetrics {
    count: u64,
    errors: u64,
    total: Duration,
    max: Duration,
    acquire: PhaseMetrics,
    encode: PhaseMetrics,
    fence: PhaseMetrics,
}
#[derive(Default)]
struct PhaseMetrics {
    total: Duration,
    max: Duration,
}
impl PhaseMetrics {
    fn record(&mut self, elapsed: Duration) {
        self.total += elapsed;
        self.max = self.max.max(elapsed);
    }
}
impl SubmissionMetrics {
    fn record(&mut self, elapsed: Duration, failed: bool) {
        self.count += 1;
        self.errors += u64::from(failed);
        self.total += elapsed;
        self.max = self.max.max(elapsed);
    }
}

struct Rasterizer {
    renderer: vello_gpu::Renderer,
    resources: Resources,
}
pub(crate) struct VelloRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    context: ContextId<VelloTexture>,
    node: Option<DrmNode>,
    sample_formats: FormatSet,
    render_formats: FormatSet,
    imports: HashMap<(WeakDmabuf, bool), Weak<TextureInner>>,
    rasterizers: HashMap<wgpu::TextureFormat, Rasterizer>,
    next_texture: u64,
    metrics: SubmissionMetrics,
    last_metrics: Instant,
    downscale: TextureFilter,
    upscale: TextureFilter,
    debug: DebugFlags,
    blend: Option<BlendParams>,
    pub(crate) effects: effects::Effects,
    opaque_pipeline: wgpu::RenderPipeline,
}
impl fmt::Debug for VelloRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VelloRenderer")
            .field("node", &self.node)
            .field("context", &self.context)
            .finish()
    }
}
fn scopes(device: &wgpu::Device) -> [wgpu::ErrorScopeGuard; 3] {
    [
        device.push_error_scope(wgpu::ErrorFilter::OutOfMemory),
        device.push_error_scope(wgpu::ErrorFilter::Internal),
        device.push_error_scope(wgpu::ErrorFilter::Validation),
    ]
}
fn check_scopes(scopes: [wgpu::ErrorScopeGuard; 3]) -> Result<(), VelloError> {
    let mut error = None;
    for scope in scopes.into_iter().rev() {
        if let Some(e) = pollster::block_on(scope.pop()) {
            error = Some(e);
        }
    }
    if let Some(error) = error {
        return Err(anyhow::anyhow!("Vello wgpu: {error}").into());
    }
    Ok(())
}
impl VelloRenderer {
    pub fn new_headless() -> Result<Self, VelloError> {
        Self::new(None)
    }
    pub fn new_for_node(node: DrmNode) -> Result<Self, VelloError> {
        Self::new(Some(node))
    }
    fn new(node: Option<DrmNode>) -> Result<Self, VelloError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = if let Some(node) = node {
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
                .into_iter()
                .find(|adapter| dmabuf::matches_node(adapter, node))
                .context("no Vulkan adapter matches DRM node")?
        } else {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .context("requesting Vello Vulkan adapter")?
        };
        let features = if node.is_some() {
            wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
        } else {
            wgpu::Features::empty()
        };
        ensure!(
            adapter.features().contains(features),
            "Vulkan adapter lacks DMA-buf external-memory support"
        );
        let desc = wgpu::DeviceDescriptor {
            label: Some("niri Vello compositor"),
            required_features: features,
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d.min(16_384),
                ..Default::default()
            },
            ..Default::default()
        };
        let (device, queue) = if node.is_some() {
            let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
                .context("not a Vulkan adapter")?;
            let extensions = unsafe {
                hal.shared_instance()
                    .raw_instance()
                    .enumerate_device_extension_properties(hal.raw_physical_device())
            }
            .context("querying Vulkan device extensions")?;
            ensure!(
                extensions.iter().any(|extension| unsafe {
                    std::ffi::CStr::from_ptr(extension.extension_name.as_ptr())
                } == ash::ext::queue_family_foreign::NAME),
                "Vulkan adapter lacks FOREIGN queue-family ownership"
            );
            let open = unsafe {
                hal.open_with_callback(
                    features,
                    &desc.required_limits,
                    &desc.memory_hints,
                    Some(Box::new(|args| {
                        args.extensions.push(ash::ext::queue_family_foreign::NAME);
                    })),
                )
            }
            .context("opening native Vello Vulkan device")?;
            unsafe { adapter.create_device_from_hal::<wgpu::hal::api::Vulkan>(open, &desc) }
                .context("wrapping Vello Vulkan device")?
        } else {
            pollster::block_on(adapter.request_device(&desc))
                .context("creating Vello Vulkan device")?
        };
        let sample_formats = if node.is_some() {
            dmabuf::formats(&adapter, false)
        } else {
            FormatSet::default()
        };
        let render_formats = if node.is_some() {
            dmabuf::formats(&adapter, true)
        } else {
            FormatSet::default()
        };
        let errors = scopes(&device);
        let effects = effects::Effects::new(&device);
        let opaque_pipeline = opaque_pipeline(&device);
        let mut renderer = Self {
            device,
            queue,
            context: ContextId::new(),
            node,
            sample_formats,
            render_formats,
            imports: HashMap::new(),
            rasterizers: HashMap::new(),
            next_texture: 1,
            metrics: SubmissionMetrics::default(),
            last_metrics: Instant::now(),
            downscale: TextureFilter::Linear,
            upscale: TextureFilter::Linear,
            debug: DebugFlags::empty(),
            effects,
            opaque_pipeline,
            blend: None,
        };
        // Initialize all target pipelines before sealing the GPU process.
        for &(_, format) in dmabuf::FORMATS {
            renderer.rasterizer(format);
        }
        renderer.rasterizer(wgpu::TextureFormat::Rgba16Float);
        check_scopes(errors)?;
        let warm = renderer.render_ui(UiScene {
            width: 1,
            height: 1,
            fonts: vec![],
            ops: vec![UiOp::Rect {
                rect: [0., 0., 1., 1.],
                color: [1., 1., 1., 1.],
            }],
        })?;
        renderer.copy_texture(&warm, Rectangle::from_size((1, 1).into()), Fourcc::Abgr8888)?;
        // Warm each native target pipeline and the opaque import-normalization pass.
        for format in [Fourcc::Argb8888, Fourcc::Abgr2101010] {
            let target = renderer.create_texture(format, (1, 1).into())?;
            renderer.render_scene(
                &Scene::new(1, 1),
                &TextureBindings::new(),
                &target,
                TargetInit::Clear(Default::default()),
            )?;
        }
        let opaque =
            renderer.import_memory(&[1, 2, 3, 0], Fourcc::Xbgr8888, (1, 1).into(), false)?;
        let normalized = renderer.resident_texture(&opaque)?;
        renderer.render_scene(
            &Scene::new(1, 1),
            &TextureBindings::new(),
            &normalized,
            TargetInit::Clear(Default::default()),
        )?;
        // Startup pipeline warm-up is not steady-state compositor latency.
        renderer.metrics = SubmissionMetrics::default();
        renderer.last_metrics = Instant::now();
        Ok(renderer)
    }
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }
    pub fn set_blend(&mut self, blend: Option<BlendParams>) {
        self.blend = blend;
    }
    pub fn render_node(&self) -> Option<DrmNode> {
        self.node
    }
    pub fn dmabuf_render_formats(&self) -> FormatSet {
        self.render_formats.clone()
    }
    fn rasterizer(&mut self, format: wgpu::TextureFormat) -> &mut Rasterizer {
        self.rasterizers.entry(format).or_insert_with(|| {
            let (renderer, resources) = vello_gpu::Renderer::new(
                &self.device,
                &RenderTargetConfig {
                    format,
                    width: 1,
                    height: 1,
                },
            );
            Rasterizer {
                renderer,
                resources,
            }
        })
    }
    fn wrap(
        &mut self,
        texture: wgpu::Texture,
        format: Option<Fourcc>,
        flipped: bool,
        imported: Option<Imported>,
    ) -> VelloTexture {
        let view = texture.create_view(&Default::default());
        let id = TextureId(self.next_texture);
        self.next_texture += 1;
        VelloTexture(Arc::new(TextureInner {
            texture,
            view,
            id,
            context: self.context.clone(),
            format,
            flipped,
            imported,
        }))
    }
    pub fn create_texture(
        &mut self,
        format: Fourcc,
        size: Size<i32, Buffer>,
    ) -> Result<VelloTexture, VelloError> {
        self.check_size(size)?;
        let gpu_format = dmabuf::texture_format(format)?;
        let errors = scopes(&self.device);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("niri resident Vello texture"),
            size: wgpu::Extent3d {
                width: size.w as u32,
                height: size.h as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        check_scopes(errors)?;
        Ok(self.wrap(texture, Some(format), false, None))
    }
    pub fn create_effect_texture(
        &mut self,
        size: Size<i32, Buffer>,
    ) -> Result<VelloTexture, VelloError> {
        self.check_size(size)?;
        let errors = scopes(&self.device);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("niri HDR intermediate"),
            size: wgpu::Extent3d {
                width: size.w as u32,
                height: size.h as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let id = TextureId(self.next_texture);
        self.next_texture += 1;
        check_scopes(errors)?;
        Ok(VelloTexture(Arc::new(TextureInner {
            texture,
            view,
            id,
            context: self.context.clone(),
            format: None,
            flipped: false,
            imported: None,
        })))
    }
    fn check_size(&self, size: Size<i32, Buffer>) -> Result<(), VelloError> {
        ensure!(
            size.w > 0
                && size.h > 0
                && size.w as u32 <= self.device.limits().max_texture_dimension_2d
                && size.h as u32 <= self.device.limits().max_texture_dimension_2d,
            "invalid Vello texture dimensions"
        );
        Ok(())
    }
    fn check_texture(&self, texture: &VelloTexture) -> Result<(), VelloError> {
        ensure!(
            texture.0.context == self.context,
            "texture belongs to another Vulkan device"
        );
        Ok(())
    }
    fn imported(&mut self, dmabuf: &Dmabuf, render: bool) -> Result<VelloTexture, VelloError> {
        ensure!(
            (if render {
                &self.render_formats
            } else {
                &self.sample_formats
            })
            .contains(&dmabuf.format()),
            "unsupported DMA-buf format/modifier/usage"
        );
        self.check_size(dmabuf.size())?;
        let key = (dmabuf.weak(), render);
        if let Some(texture) = self.imports.get(&key).and_then(Weak::upgrade) {
            return Ok(VelloTexture(texture));
        }
        let errors = scopes(&self.device);
        let result = dmabuf::import(&self.device, dmabuf, render);
        check_scopes(errors)?;
        let texture = self.wrap(
            result?,
            Some(dmabuf.format().code),
            dmabuf.y_inverted(),
            Some(Imported {
                dmabuf: dmabuf.clone(),
                render,
                identity: dmabuf::identity(dmabuf)?,
            }),
        );
        self.imports.insert(key, Arc::downgrade(&texture.0));
        Ok(texture)
    }
    /// All imported image use, including copies and private shader passes, goes through this API.
    /// `encode` may submit atlas/resident uploads, but must not itself submit work using imported
    /// images.
    pub(crate) fn submit(
        &mut self,
        textures: &[VelloTexture],
        encode: impl FnOnce(&mut Self, &mut wgpu::CommandEncoder) -> Result<(), VelloError>,
    ) -> Result<(), VelloError> {
        let start = Instant::now();
        let result = self.submit_inner(textures, encode);
        self.metrics.record(start.elapsed(), result.is_err());
        if let Err(error) = &result {
            tracing::warn!(%error, "Vello submission failed");
        }
        // At most one allocator walk per ten seconds, only while rendering.
        // This needs no /proc access or new permissions in the sealed worker.
        if self.last_metrics.elapsed() >= Duration::from_secs(10) {
            self.log_metrics();
        }
        result
    }

    fn log_metrics(&mut self) {
        let metrics = std::mem::take(&mut self.metrics);
        let interval = self.last_metrics.elapsed();
        self.last_metrics = Instant::now();
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        // External DMA-buf imports bypass wgpu's allocator. Do not describe these
        // totals as total GPU/process memory, or count imported bytes twice.
        let report = self.device.generate_allocator_report();
        let live_imports = self.imports.values().filter(|t| t.strong_count() > 0).count();
        tracing::debug!(
            interval_ms = interval.as_millis() as u64,
            submissions = metrics.count,
            submission_errors = metrics.errors,
            submission_wall_ms = metrics.total.as_secs_f64() * 1000.,
            max_submission_wall_ms = metrics.max.as_secs_f64() * 1000.,
            acquire_ms = metrics.acquire.total.as_secs_f64() * 1000.,
            max_acquire_ms = metrics.acquire.max.as_secs_f64() * 1000.,
            encode_submit_ms = metrics.encode.total.as_secs_f64() * 1000.,
            max_encode_submit_ms = metrics.encode.max.as_secs_f64() * 1000.,
            fence_wait_ms = metrics.fence.total.as_secs_f64() * 1000.,
            max_fence_wait_ms = metrics.fence.max.as_secs_f64() * 1000.,
            allocated_bytes = report.as_ref().map(|r| r.total_allocated_bytes),
            reserved_bytes = report.as_ref().map(|r| r.total_reserved_bytes),
            allocations = report.as_ref().map(|r| r.allocations.len()),
            live_imports,
            rasterizers = self.rasterizers.len(),
            "Vello metrics (allocator excludes external DMA-bufs)"
        );
    }

    fn submit_inner(
        &mut self,
        textures: &[VelloTexture],
        encode: impl FnOnce(&mut Self, &mut wgpu::CommandEncoder) -> Result<(), VelloError>,
    ) -> Result<(), VelloError> {
        let mut textures = textures.to_vec();
        textures.sort_by_key(|t| t.0.id.0);
        textures.dedup_by_key(|t| t.0.id.0);
        for (index, texture) in textures.iter().enumerate() {
            self.check_texture(texture)?;
            if let Some(imported) = &texture.0.imported {
                ensure!(
                    !textures[..index].iter().any(|other| other
                        .0
                        .imported
                        .as_ref()
                        .is_some_and(|other| other.identity == imported.identity)),
                    "aliased DMA-buf images cannot be acquired twice in one submission"
                );
            }
        }
        let start = Instant::now();
        let bracket = dmabuf::bracket(&self.device, &textures);
        self.metrics.acquire.record(start.elapsed());
        let bracket = bracket?;
        let errors = scopes(&self.device);
        let result = (|| {
            let encode_start = Instant::now();
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("niri Vello frame"),
                });
            let encoded = encode(self, &mut encoder);
            if let Err(error) = encoded {
                self.metrics.encode.record(encode_start.elapsed());
                return Err(error);
            }
            dmabuf::restore(&mut encoder, &textures);
            let main = encoder.finish();
            let submission = if let Some((acquire, release)) = bracket {
                self.queue.submit([acquire, main, release])
            } else {
                self.queue.submit([main])
            };
            self.metrics.encode.record(encode_start.elapsed());
            let wait_start = Instant::now();
            let waited = self.device.poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(30)),
            });
            self.metrics.fence.record(wait_start.elapsed());
            waited.context("waiting for Vello submission")?;
            Ok(())
        })();
        check_scopes(errors)?;
        self.imports.retain(|_, texture| texture.strong_count() > 0);
        result
    }
    pub fn render_scene(
        &mut self,
        scene: &Scene,
        bindings: &TextureBindings,
        target: &VelloTexture,
        init: TargetInit<'_>,
    ) -> Result<(), VelloError> {
        self.render_scene_with_textures(scene, bindings, target, init, &[])
    }
    fn render_scene_with_textures(
        &mut self,
        scene: &Scene,
        bindings: &TextureBindings,
        target: &VelloTexture,
        init: TargetInit<'_>,
        inputs: &[VelloTexture],
    ) -> Result<(), VelloError> {
        ensure!(
            !inputs.iter().any(|input| input.0.id == target.0.id
                || input
                    .0
                    .imported
                    .as_ref()
                    .zip(target.0.imported.as_ref())
                    .is_some_and(|(input, target)| input.identity == target.identity)),
            "cannot sample the active render target"
        );
        let mut textures = inputs.to_vec();
        textures.push(target.clone());
        self.submit(&textures, |renderer, encoder| {
            let render_size = RenderSize {
                width: target.width() as u16,
                height: target.height() as u16,
            };
            let depth =
                vello_gpu::Renderer::create_depth_texture_view(&renderer.device, &render_size);
            let format = target.texture().format();
            renderer.rasterizer(format);
            let rasterizer = renderer.rasterizers.get_mut(&format).unwrap();
            rasterizer
                .renderer
                .render(
                    scene,
                    &mut rasterizer.resources,
                    &renderer.device,
                    &renderer.queue,
                    encoder,
                    &render_size,
                    target.view(),
                    Some(&depth),
                    bindings,
                    init,
                )
                .context("Vello scene rendering")?;
            Ok(())
        })
    }
    pub fn render_ui(&mut self, input: UiScene) -> Result<VelloTexture, VelloError> {
        validate(&input, self.device.limits().max_texture_dimension_2d)?;
        let target = self.create_texture(
            Fourcc::Abgr8888,
            (input.width as i32, input.height as i32).into(),
        )?;
        let mut scene = Scene::new(input.width as u16, input.height as u16);
        let fonts: Vec<_> = input
            .fonts
            .into_iter()
            .map(|font| FontData::new(Blob::new(Arc::new(font.data)), font.index))
            .collect();
        let result = (|| {
            let resources = &mut self.rasterizer(wgpu::TextureFormat::Rgba8Unorm).resources;
            for op in input.ops {
                match op {
                    UiOp::Rect {
                        rect: [x, y, w, h],
                        color,
                    } => {
                        scene.set_paint(AlphaColor::<Srgb>::new(color));
                        scene.fill_rect(&Rect::new(
                            x as f64,
                            y as f64,
                            (x + w) as f64,
                            (y + h) as f64,
                        ));
                    }
                    UiOp::Circle {
                        center: [x, y],
                        radius,
                        color,
                    } => {
                        scene.set_paint(AlphaColor::<Srgb>::new(color));
                        scene.fill_path(
                            &Circle::new((x as f64, y as f64), radius as f64).to_path(0.1),
                        );
                    }
                    UiOp::GlyphRun {
                        font,
                        font_size,
                        coords,
                        glyphs,
                        color,
                    } => {
                        scene.set_paint(AlphaColor::<Srgb>::new(color));
                        scene
                            .glyph_run(resources, &fonts[font as usize])
                            .font_size(font_size)
                            .normalized_coords(&coords)
                            .fill_glyphs(glyphs.iter().map(|g| glifo::Glyph {
                                id: g.id,
                                x: g.x,
                                y: g.y,
                            }))
                            .map_err(|e| anyhow::anyhow!("Vello glyph rendering: {e}"))?;
                    }
                }
            }
            self.render_scene(
                &scene,
                &TextureBindings::new(),
                &target,
                TargetInit::Clear(Default::default()),
            )?;
            Ok(target)
        })();
        if result.is_err() {
            // Errors before upstream's successful-render cache maintenance must not retain
            // unbounded fonts.
            self.rasterizers.remove(&wgpu::TextureFormat::Rgba8Unorm);
        }
        result
    }
    /// Make a resident GPU image for private effects. Never reads pixels into CPU memory.
    pub fn resident_texture(&mut self, source: &VelloTexture) -> Result<VelloTexture, VelloError> {
        self.check_texture(source)?;
        if !source.is_imported() && !source.force_opaque() {
            return Ok(source.clone());
        }
        let target = if source.force_opaque() {
            self.create_effect_texture(source.size())?
        } else {
            self.create_texture(
                source
                    .0
                    .format
                    .context("resident source format unavailable")?,
                source.size(),
            )?
        };
        self.submit(&[source.clone()], |renderer, encoder| {
            if source.force_opaque() {
                let bind = renderer
                    .device
                    .create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("opaque DMA-buf view"),
                        layout: &renderer.opaque_pipeline.get_bind_group_layout(0),
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(source.view()),
                        }],
                    });
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("normalize XRGB alpha"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view(),
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&renderer.opaque_pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            } else {
                encoder.copy_texture_to_texture(
                    source.texture().as_image_copy(),
                    target.texture().as_image_copy(),
                    wgpu::Extent3d {
                        width: source.width(),
                        height: source.height(),
                        depth_or_array_layers: 1,
                    },
                );
            }
            Ok(())
        })?;
        // A resident copy retains the source's orientation metadata.
        let texture = self.wrap(
            target.texture().clone(),
            target.format(),
            source.flipped(),
            None,
        );
        Ok(texture)
    }
    /// # Safety
    /// Rows must be readable for width*4 bytes, and stride must describe the caller's sealed pool.
    pub unsafe fn import_memory_strided(
        &mut self,
        src: *const u8,
        stride: i32,
        format: Fourcc,
        size: Size<i32, Buffer>,
        flipped: bool,
    ) -> Result<VelloTexture, VelloError> {
        self.check_size(size)?;
        ensure!(
            stride >= size.w.checked_mul(4).context("upload row overflow")?,
            "invalid upload stride"
        );
        // The sealed pool may still be written by its client. Copy raw rows into owned
        // storage without creating immutable Rust references to that shared mapping.
        let data = copy_rows(src, stride as usize, size.w as usize * 4, size.h as usize);
        self.import_memory(&data, format, size, flipped)
    }
    /// # Safety
    /// Each row in region must be readable from the checked sealed pool pointer.
    pub unsafe fn update_memory_strided(
        &mut self,
        texture: &VelloTexture,
        src: *const u8,
        stride: i32,
        region: Rectangle<i32, Buffer>,
    ) -> Result<(), VelloError> {
        ensure!(
            region.size.w > 0 && region.size.h > 0 && region.loc.x >= 0 && region.loc.y >= 0,
            "invalid upload region"
        );
        ensure!(
            i64::from(region.loc.x) + i64::from(region.size.w) <= i64::from(texture.width())
                && i64::from(region.loc.y) + i64::from(region.size.h)
                    <= i64::from(texture.height()),
            "upload region out of bounds"
        );
        ensure!(
            texture.0.format.is_some() && !texture.is_imported(),
            "texture is not a writable memory import"
        );
        ensure!(
            stride
                >= region
                    .size
                    .w
                    .checked_mul(4)
                    .context("upload row overflow")?,
            "invalid upload stride"
        );
        let data = copy_rows(
            src,
            stride as usize,
            region.size.w as usize * 4,
            region.size.h as usize,
        );
        self.update_memory(texture, &data, region)
    }
}
/// Caller validates each row's readable range; no source references are formed.
unsafe fn copy_rows(src: *const u8, stride: usize, row_bytes: usize, rows: usize) -> Vec<u8> {
    let mut bytes = vec![0; row_bytes * rows];
    for y in 0..rows {
        std::ptr::copy_nonoverlapping(
            src.add(y * stride),
            bytes.as_mut_ptr().add(y * row_bytes),
            row_bytes,
        );
    }
    bytes
}

fn opaque_pipeline(device: &wgpu::Device) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("XRGB normalization"),
        source: wgpu::ShaderSource::Wgsl(
            r#"
@group(0) @binding(0) var image: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> {
    let p=array<vec2<f32>,3>(vec2(-1.,-1.),vec2(3.,-1.),vec2(-1.,3.));
    return vec4(p[i],0.,1.);
}
@fragment fn fs(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    return vec4(textureLoad(image,vec2<i32>(p.xy),0).rgb,1.);
}
"#
            .into(),
        ),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("XRGB normalization"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba16Float,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}
impl RendererSuper for VelloRenderer {
    type Error = VelloError;
    type TextureId = VelloTexture;
    type Framebuffer<'buffer> = VelloTarget<'buffer>;
    type Frame<'frame, 'buffer>
        = VelloFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame;
}
impl Renderer for VelloRenderer {
    fn context_id(&self) -> ContextId<VelloTexture> {
        self.context.clone()
    }
    fn downscale_filter(&mut self, filter: TextureFilter) -> Result<(), VelloError> {
        self.downscale = filter;
        Ok(())
    }
    fn upscale_filter(&mut self, filter: TextureFilter) -> Result<(), VelloError> {
        self.upscale = filter;
        Ok(())
    }
    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.debug = flags;
    }
    fn debug_flags(&self) -> DebugFlags {
        self.debug
    }
    fn render<'frame, 'buffer>(
        &'frame mut self,
        target: &'frame mut VelloTarget<'buffer>,
        size: Size<i32, Physical>,
        transform: Transform,
    ) -> Result<VelloFrame<'frame, 'buffer>, VelloError>
    where
        'buffer: 'frame,
    {
        ensure!(
            size.w > 0
                && size.h > 0
                && size == Size::from((target.width() as i32, target.height() as i32)),
            "frame dimensions do not match target"
        );
        // Smithay passes framebuffer dimensions; Frame coordinates are post-transform.
        let size = transform.transform_size(size);
        let blend = self.blend;
        let mut scene = Scene::new(target.width() as u16, target.height() as u16);
        scene.set_transform(output_affine(transform, size));
        Ok(VelloFrame {
            renderer: self,
            target: target.texture.clone(),
            scene,
            bindings: TextureBindings::new(),
            inputs: Vec::new(),
            size,
            transform,
            blend,
            _borrow: PhantomData,
        })
    }
    fn wait(&mut self, sync: &SyncPoint) -> Result<(), VelloError> {
        sync.wait().context("waiting for renderer fence")?;
        Ok(())
    }
    fn cleanup_texture_cache(&mut self) -> Result<(), VelloError> {
        self.imports.retain(|_, v| v.strong_count() > 0);
        Ok(())
    }
    fn invalidate_caches(&mut self) -> Result<(), VelloError> {
        self.imports.clear();
        Ok(())
    }
}
impl Bind<VelloTexture> for VelloRenderer {
    fn bind<'a>(&mut self, target: &'a mut VelloTexture) -> Result<VelloTarget<'a>, VelloError> {
        self.check_texture(target)?;
        ensure!(
            !target.is_imported() || target.0.imported.as_ref().unwrap().render,
            "sample-only DMA-buf cannot be a target"
        );
        Ok(VelloTarget {
            texture: target.clone(),
            _borrow: PhantomData,
        })
    }
}
impl Bind<Dmabuf> for VelloRenderer {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<VelloTarget<'a>, VelloError> {
        Ok(VelloTarget {
            texture: self.imported(target, true)?,
            _borrow: PhantomData,
        })
    }
    fn supported_formats(&self) -> Option<FormatSet> {
        Some(self.render_formats.clone())
    }
}
impl Offscreen<VelloTexture> for VelloRenderer {
    fn create_buffer(
        &mut self,
        format: Fourcc,
        size: Size<i32, Buffer>,
    ) -> Result<VelloTexture, VelloError> {
        self.create_texture(format, size)
    }
}
impl ImportDma for VelloRenderer {
    fn dmabuf_formats(&self) -> FormatSet {
        self.sample_formats.clone()
    }
    fn import_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        _damage: Option<&[Rectangle<i32, Buffer>]>,
    ) -> Result<VelloTexture, VelloError> {
        self.imported(dmabuf, false)
    }
}
impl ImportMem for VelloRenderer {
    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        Box::new(dmabuf::FORMATS.iter().map(|(format, _)| *format))
    }
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, Buffer>,
        flipped: bool,
    ) -> Result<VelloTexture, VelloError> {
        let target = self.create_texture(format, size)?;
        let texture = self.wrap(target.texture().clone(), Some(format), flipped, None);
        self.update_memory(&texture, data, Rectangle::from_size(size))?;
        Ok(texture)
    }
    fn update_memory(
        &mut self,
        texture: &VelloTexture,
        data: &[u8],
        region: Rectangle<i32, Buffer>,
    ) -> Result<(), VelloError> {
        self.check_texture(texture)?;
        ensure!(
            texture.0.format.is_some(),
            "texture has no memory pixel format"
        );
        ensure!(
            !texture.is_imported(),
            "cannot write imported DMA-buf with queue.write_texture"
        );
        ensure!(
            region.loc.x >= 0
                && region.loc.y >= 0
                && region.size.w > 0
                && region.size.h > 0
                && i64::from(region.loc.x) + i64::from(region.size.w) <= i64::from(texture.width())
                && i64::from(region.loc.y) + i64::from(region.size.h)
                    <= i64::from(texture.height()),
            "upload region out of bounds"
        );
        let len = region.size.w as usize * region.size.h as usize * 4;
        ensure!(data.len() >= len, "upload pixels too short");
        let mut normalized;
        let data = if texture.force_opaque() {
            normalized = data[..len].to_vec();
            if matches!(texture.0.format, Some(Fourcc::Xbgr2101010)) {
                for p in normalized.chunks_exact_mut(4) {
                    p[3] |= 0xc0;
                }
            } else {
                for p in normalized.chunks_exact_mut(4) {
                    p[3] = 255;
                }
            }
            &normalized[..]
        } else {
            &data[..len]
        };
        let errors = scopes(&self.device);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: region.loc.x as u32,
                    y: region.loc.y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(region.size.w as u32 * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: region.size.w as u32,
                height: region.size.h as u32,
                depth_or_array_layers: 1,
            },
        );
        check_scopes(errors)
    }
}
impl ExportMem for VelloRenderer {
    type TextureMapping = VelloMapping;
    fn copy_framebuffer(
        &mut self,
        target: &VelloTarget<'_>,
        region: Rectangle<i32, Buffer>,
        format: Fourcc,
    ) -> Result<VelloMapping, VelloError> {
        self.copy_texture(&target.texture, region, format)
    }
    fn copy_texture(
        &mut self,
        texture: &VelloTexture,
        region: Rectangle<i32, Buffer>,
        format: Fourcc,
    ) -> Result<VelloMapping, VelloError> {
        ensure!(
            region.loc.x >= 0
                && region.loc.y >= 0
                && region.size.w > 0
                && region.size.h > 0
                && i64::from(region.loc.x) + i64::from(region.size.w) <= i64::from(texture.width())
                && i64::from(region.loc.y) + i64::from(region.size.h)
                    <= i64::from(texture.height()),
            "readback region out of bounds"
        );
        // Smithay callers may request the equivalent channel ordering for screenshot/shm capture.
        let source_format = texture.texture().format();
        let target_format = dmabuf::texture_format(format)?;
        if source_format != target_format
            && !matches!(
                (source_format, target_format),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureFormat::Bgra8Unorm
                ) | (
                    wgpu::TextureFormat::Bgra8Unorm,
                    wgpu::TextureFormat::Rgba8Unorm
                )
            )
        {
            let converted = self.create_texture(format, texture.size())?;
            let mut bindings = TextureBindings::new();
            bindings.insert(texture.0.id, texture.view().clone());
            let mut scene = Scene::new(texture.width() as u16, texture.height() as u16);
            scene.set_paint(ImageBrush {
                image: ImageSource::external_texture(
                    texture.0.id,
                    RectU16::new(0, 0, texture.width() as u16, texture.height() as u16),
                    !texture.force_opaque(),
                ),
                sampler: vello_gpu::peniko::ImageSampler::default().with_extend(Extend::Reflect),
            });
            scene.fill_rect(&Rect::new(
                0.,
                0.,
                texture.width() as f64,
                texture.height() as f64,
            ));
            self.render_scene_with_textures(
                &scene,
                &bindings,
                &converted,
                TargetInit::Clear(Default::default()),
                std::slice::from_ref(texture),
            )?;
            return self.copy_texture(&converted, region, format);
        }
        let row_len = region.size.w as u32 * 4;
        let stride = row_len.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("explicit Vello readback"),
            size: u64::from(stride) * region.size.h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.submit(&[texture.clone()], |_, encoder| {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: texture.texture(),
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: region.loc.x as u32,
                        y: region.loc.y as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride),
                        rows_per_image: None,
                    },
                },
                wgpu::Extent3d {
                    width: region.size.w as u32,
                    height: region.size.h as u32,
                    depth_or_array_layers: 1,
                },
            );
            Ok(())
        })?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(30)),
            })
            .context("waiting for explicit readback")?;
        rx.recv()
            .context("readback callback")?
            .context("mapping readback")?;
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .context("accessing readback")?;
        let mut bytes = Vec::with_capacity(row_len as usize * region.size.h as usize);
        for row in mapped.chunks_exact(stride as usize) {
            bytes.extend_from_slice(&row[..row_len as usize]);
        }
        drop(mapped);
        buffer.unmap();
        if source_format != target_format {
            for pixel in bytes.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        if !has_alpha(format) {
            for pixel in bytes.chunks_exact_mut(4) {
                if target_format == wgpu::TextureFormat::Rgb10a2Unorm {
                    pixel[3] |= 0xc0;
                } else {
                    pixel[3] = 255;
                }
            }
        }
        Ok(VelloMapping {
            bytes,
            size: region.size,
            format,
        })
    }
    fn can_read_texture(&mut self, texture: &VelloTexture) -> Result<bool, VelloError> {
        self.check_texture(texture)?;
        Ok(true)
    }
    fn map_texture<'a>(&mut self, mapping: &'a VelloMapping) -> Result<&'a [u8], VelloError> {
        Ok(&mapping.bytes)
    }
}

pub(crate) struct VelloFrame<'frame, 'buffer> {
    renderer: &'frame mut VelloRenderer,
    target: VelloTexture,
    scene: Scene,
    bindings: TextureBindings,
    inputs: Vec<VelloTexture>,
    size: Size<i32, Physical>,
    transform: Transform,
    blend: Option<BlendParams>,
    _borrow: PhantomData<&'buffer mut ()>,
}
impl VelloFrame<'_, '_> {
    pub fn target_texture(&self) -> &VelloTexture {
        &self.target
    }
    pub fn size(&self) -> Size<i32, Physical> {
        self.size
    }
    pub fn transform(&self) -> Transform {
        self.transform
    }
    pub fn root_transform(&self) -> Affine {
        output_affine(self.transform, self.size)
    }
    pub fn image_nearest(
        &self,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        transform: Transform,
    ) -> bool {
        let oriented = transform.transform_size(src.size);
        let downscaling = oriented.w > dst.size.w as f64 || oriented.h > dst.size.h as f64;
        (if downscaling {
            self.renderer.downscale
        } else {
            self.renderer.upscale
        }) == TextureFilter::Nearest
    }
    pub fn blend(&self) -> Option<BlendParams> {
        self.blend
    }
    pub fn use_texture(&mut self, texture: &VelloTexture) -> TextureId {
        self.bindings.insert(texture.0.id, texture.view().clone());
        self.inputs.push(texture.clone());
        texture.0.id
    }
    pub fn flush(&mut self) -> Result<(), VelloError> {
        self.renderer.render_scene_with_textures(
            &self.scene,
            &self.bindings,
            &self.target,
            TargetInit::SrcOver,
            &self.inputs,
        )?;
        self.scene = Scene::new(self.target.width() as u16, self.target.height() as u16);
        self.scene.set_transform(self.root_transform());
        self.bindings = TextureBindings::new();
        self.inputs.clear();
        Ok(())
    }
    pub fn renderer_mut(&mut self) -> Result<&mut VelloRenderer, VelloError> {
        self.flush()?;
        Ok(self.renderer)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn draw_texture(
        &mut self,
        texture: &VelloTexture,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque: &[Rectangle<i32, Physical>],
        transform: Transform,
        alpha: f32,
    ) -> Result<(), VelloError> {
        if src.size.is_empty() || dst.size.is_empty() || damage.is_empty() || alpha == 0. {
            return Ok(());
        }
        self.renderer.check_texture(texture)?;
        ensure!(
            src.loc.x.is_finite()
                && src.loc.y.is_finite()
                && src.size.w.is_finite()
                && src.size.h.is_finite()
                && alpha.is_finite()
                && (0.0..=1.0).contains(&alpha),
            "invalid texture geometry/alpha"
        );
        let nearest = self.image_nearest(src, dst, transform);
        if src.loc.x < 0.
            || src.loc.y < 0.
            || src.loc.x + src.size.w > texture.width() as f64
            || src.loc.y + src.size.h > texture.height() as f64
        {
            // Vello's Reflect workaround below is only valid inside the image. Preserve
            // Smithay's unrestricted source rectangle / clamp-to-edge semantics using
            // the existing fixed GPU resampler for crops crossing either boundary.
            self.flush()?;
            let sampled =
                effects::resample_texture(self.renderer, texture, src, dst, transform, nearest)?;
            return self.draw_texture(
                &sampled,
                Rectangle::from_size(sampled.size().to_f64()),
                dst,
                damage,
                &[],
                Transform::Normal,
                alpha,
            );
        }
        let normalized;
        let texture = if texture.is_imported() && texture.force_opaque() {
            self.flush()?;
            normalized = self.renderer.resident_texture(texture)?;
            &normalized
        } else {
            texture
        };
        let id = self.use_texture(texture);
        let gpu_source = ImageSource::external_texture(
            id,
            RectU16::new(0, 0, texture.width() as u16, texture.height() as u16),
            !texture.force_opaque(),
        );
        let quality = if nearest {
            ImageQuality::Low
        } else {
            ImageQuality::Medium
        };
        let image = ImageBrush {
            image: gpu_source,
            sampler: Default::default(),
        }
        .with_alpha(alpha)
        .with_quality(quality)
        // Pinned Vello Pad clamps to size-1 before the bilinear half-texel adjustment,
        // blending the last two pixels. Reflect is identity throughout valid image
        // coordinates [0,size]; the external sampler then clamps individual taps.
        .with_extend(Extend::Reflect);
        self.scene.set_transform(self.root_transform());
        self.scene.set_paint(image);
        // texture_matrix maps normalized destination UV to normalized texture UV.
        // Vello instead needs image texels -> destination pixels (paint transform).
        let m = texture_matrix(texture, src, dst, transform);
        let tex_to_dest =
            Mat3::from_translation(glam::Vec2::new(dst.loc.x as f32, dst.loc.y as f32))
                * Mat3::from_scale(glam::Vec2::new(dst.size.w as f32, dst.size.h as f32))
                * m.inverse()
                * Mat3::from_scale(glam::Vec2::new(
                    1. / texture.width() as f32,
                    1. / texture.height() as f32,
                ));
        self.scene.set_paint_transform(mat_affine(tex_to_dest));
        for rect in damage {
            if let Some(rect) = rect.intersection(Rectangle::from_size(dst.size)) {
                self.scene
                    .push_clip_rect(&rect_f64(Rectangle::new(dst.loc + rect.loc, rect.size)));
                self.scene.fill_rect(&rect_f64(dst));
                self.scene.pop_clip();
            }
        }
        self.scene.reset_paint_transform();
        Ok(())
    }
}
impl Frame for VelloFrame<'_, '_> {
    type Error = VelloError;
    type TextureId = VelloTexture;
    fn context_id(&self) -> ContextId<VelloTexture> {
        self.renderer.context.clone()
    }
    fn transformation(&self) -> Transform {
        self.transform
    }
    fn output_size(&self) -> Size<i32, Physical> {
        self.size
    }
    fn wait(&mut self, sync: &SyncPoint) -> Result<(), VelloError> {
        sync.wait().context("waiting for frame fence")?;
        Ok(())
    }
    fn finish(mut self) -> Result<SyncPoint, VelloError> {
        self.flush()?;
        Ok(SyncPoint::signaled())
    }
    fn clear(
        &mut self,
        color: Color32F,
        at: &[Rectangle<i32, Physical>],
    ) -> Result<(), VelloError> {
        if at.is_empty() {
            return Ok(());
        }
        self.flush()?;
        let root = self.root_transform();
        let bounds = Rect::new(
            0.,
            0.,
            self.target.width() as f64,
            self.target.height() as f64,
        );
        let rects: Vec<_> = at
            .iter()
            .filter_map(|r| {
                let r = root.transform_rect_bbox(rect_f64(*r)).intersect(bounds);
                (r.width() > 0. && r.height() > 0.)
                    .then(|| RectU16::new(r.x0 as u16, r.y0 as u16, r.x1 as u16, r.y1 as u16))
            })
            .collect();
        let scene = Scene::new(self.target.width() as u16, self.target.height() as u16);
        self.renderer.render_scene(
            &scene,
            &TextureBindings::new(),
            &self.target,
            TargetInit::Clear(vello_gpu::ClearSettings::Rects {
                color: straight(effects::blend(color, self.blend)),
                rects: &rects,
            }),
        )
    }
    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), VelloError> {
        let color = effects::blend(color, self.blend);
        self.scene.set_transform(self.root_transform());
        self.scene.reset_paint_transform();
        self.scene.set_paint(straight(color));
        for rect in damage {
            if let Some(rect) = rect.intersection(Rectangle::from_size(dst.size)) {
                self.scene
                    .push_clip_rect(&rect_f64(Rectangle::new(dst.loc + rect.loc, rect.size)));
                self.scene.fill_rect(&rect_f64(dst));
                self.scene.pop_clip();
            }
        }
        Ok(())
    }
    fn render_texture_from_to(
        &mut self,
        texture: &VelloTexture,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        transform: Transform,
        alpha: f32,
    ) -> Result<(), VelloError> {
        effects::draw_texture(
            self,
            texture,
            src,
            dst,
            damage,
            opaque,
            transform,
            alpha,
            super::super::protocol::TextureOptions::default(),
        )
    }
}
fn straight(color: Color32F) -> AlphaColor<Srgb> {
    let [r, g, b, a] = color.components();
    AlphaColor::new(if a > 0. {
        [r / a, g / a, b / a, a]
    } else {
        [0., 0., 0., 0.]
    })
}
fn rect_f64(rect: Rectangle<i32, Physical>) -> Rect {
    Rect::new(
        rect.loc.x as f64,
        rect.loc.y as f64,
        (rect.loc.x + rect.size.w) as f64,
        (rect.loc.y + rect.size.h) as f64,
    )
}
// Matches Smithay DRM plane/damage `Transform::transform_rect_in` (and the GLES
// framebuffer projection). Its flipped 90/270 convention differs from transform_point_in.
fn output_affine(transform: Transform, size: Size<i32, Physical>) -> Affine {
    let (w, h) = (size.w as f64, size.h as f64);
    match transform {
        Transform::Normal => Affine::IDENTITY,
        Transform::_90 => Affine::new([0., 1., -1., 0., h, 0.]),
        Transform::_180 => Affine::new([-1., 0., 0., -1., w, h]),
        Transform::_270 => Affine::new([0., -1., 1., 0., 0., w]),
        Transform::Flipped => Affine::new([-1., 0., 0., 1., w, 0.]),
        Transform::Flipped90 => Affine::new([0., -1., -1., 0., h, w]),
        Transform::Flipped180 => Affine::new([1., 0., 0., -1., 0., h]),
        Transform::Flipped270 => Affine::new([0., 1., 1., 0., 0., 0.]),
    }
}
fn mat_affine(m: Mat3) -> Affine {
    Affine::new([
        m.x_axis.x as f64,
        m.x_axis.y as f64,
        m.y_axis.x as f64,
        m.y_axis.y as f64,
        m.z_axis.x as f64,
        m.z_axis.y as f64,
    ])
}
pub(crate) fn texture_matrix(
    texture: &VelloTexture,
    src: Rectangle<f64, Buffer>,
    _dst: Rectangle<i32, Physical>,
    transform: Transform,
) -> Mat3 {
    let sample = |x: f64, y: f64| {
        let (u, v) = match transform {
            Transform::Normal => (x, y),
            Transform::_90 => (y, 1. - x),
            Transform::_180 => (1. - x, 1. - y),
            Transform::_270 => (1. - y, x),
            Transform::Flipped => (1. - x, y),
            Transform::Flipped90 => (y, x),
            Transform::Flipped180 => (x, 1. - y),
            Transform::Flipped270 => (1. - y, 1. - x),
        };
        let x = (u * src.size.w + src.loc.x) / texture.width() as f64;
        let mut y = (v * src.size.h + src.loc.y) / texture.height() as f64;
        if texture.flipped() {
            y = 1. - y;
        }
        glam::Vec2::new(x as f32, y as f32)
    };
    let a = sample(0., 0.);
    let b = sample(1., 0.);
    let c = sample(0., 1.);
    Mat3::from_cols(
        Vec3::new(b.x - a.x, b.y - a.y, 0.),
        Vec3::new(c.x - a.x, c.y - a.y, 0.),
        Vec3::new(a.x, a.y, 1.),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_metrics_count_failures_and_reset() {
        let mut metrics = SubmissionMetrics::default();
        metrics.record(Duration::from_millis(3), false);
        metrics.record(Duration::from_millis(19), true);
        metrics.record(Duration::from_millis(5), false);
        metrics.acquire.record(Duration::from_millis(2));
        metrics.acquire.record(Duration::from_millis(7));
        metrics.encode.record(Duration::from_millis(11));
        metrics.fence.record(Duration::from_millis(4));
        let interval = std::mem::take(&mut metrics);
        assert_eq!(interval.acquire.total, Duration::from_millis(9));
        assert_eq!(interval.acquire.max, Duration::from_millis(7));
        assert_eq!(interval.encode.total, Duration::from_millis(11));
        assert_eq!(interval.fence.total, Duration::from_millis(4));
        assert_eq!(metrics.acquire.total, Duration::ZERO);
        assert_eq!(metrics.encode.max, Duration::ZERO);
        assert_eq!(metrics.fence.total, Duration::ZERO);
        assert_eq!(interval.count, 3);
        assert_eq!(interval.errors, 1);
        assert_eq!(interval.total, Duration::from_millis(27));
        assert_eq!(interval.max, Duration::from_millis(19));
        metrics.record(Duration::from_millis(2), false);
        assert_eq!(metrics.count, 1);
        assert_eq!(metrics.errors, 0);
        assert_eq!(metrics.total, Duration::from_millis(2));
        assert_eq!(metrics.max, Duration::from_millis(2));
    }

    #[test]
    fn submission_reporting_preserves_errors_and_resets_interval() {
        let _subscriber = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_test_writer()
                .finish(),
        );
        let mut renderer = VelloRenderer::new_headless().unwrap();
        renderer.metrics = SubmissionMetrics::default();
        let error = renderer
            .submit(&[], |_, _| Err(anyhow::anyhow!("diagnostic test error").into()))
            .unwrap_err();
        assert!(error.to_string().contains("diagnostic test error"));
        assert_eq!(renderer.metrics.count, 1);
        assert_eq!(renderer.metrics.errors, 1);

        // Exercise the real backend report rather than substitute CPU RSS.
        let report = renderer.device.generate_allocator_report().unwrap();
        assert!(report.total_allocated_bytes > 0);
        assert!(report.total_reserved_bytes >= report.total_allocated_bytes);
        renderer.log_metrics();
        assert_eq!(renderer.metrics.count, 0);
        assert_eq!(renderer.metrics.errors, 0);

        // The periodic path also reports and resets after a successful submission.
        renderer.last_metrics = Instant::now() - Duration::from_secs(10);
        renderer.submit(&[], |_, _| Ok(())).unwrap();
        assert_eq!(renderer.metrics.count, 0);
        assert!(renderer.last_metrics.elapsed() < Duration::from_secs(10));
    }

    fn pixels(renderer: &mut VelloRenderer, texture: &VelloTexture) -> Vec<[u8; 4]> {
        let mapping = renderer
            .copy_texture(
                texture,
                Rectangle::from_size(texture.size()),
                Fourcc::Abgr8888,
            )
            .unwrap();
        renderer
            .map_texture(&mapping)
            .unwrap()
            .chunks_exact(4)
            .map(|p| p.try_into().unwrap())
            .collect()
    }
    fn draw(
        renderer: &mut VelloRenderer,
        target: &mut VelloTexture,
        source: &VelloTexture,
        src: Rectangle<f64, Buffer>,
        size: Size<i32, Physical>,
        source_transform: Transform,
        output_transform: Transform,
    ) {
        let actual_size = Size::from((target.width() as i32, target.height() as i32));
        let mut framebuffer = renderer.bind(target).unwrap();
        let mut frame = renderer
            .render(&mut framebuffer, actual_size, output_transform)
            .unwrap();
        frame
            .clear(Color32F::BLACK, &[Rectangle::from_size(size)])
            .unwrap();
        frame
            .draw_texture(
                source,
                src,
                Rectangle::from_size(size),
                &[Rectangle::from_size(size)],
                &[],
                source_transform,
                1.,
            )
            .unwrap();
        frame.finish().unwrap().wait().unwrap();
    }

    #[test]
    fn native_images_transform_damage_and_premultiplied_alpha() {
        let mut renderer = VelloRenderer::new_headless().unwrap();
        renderer.downscale_filter(TextureFilter::Nearest).unwrap();
        renderer.upscale_filter(TextureFilter::Nearest).unwrap();
        let colors = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 0, 255],
            [0, 255, 255, 255],
            [255, 0, 255, 255],
            [255, 128, 0, 255],
            [255, 255, 255, 255],
            [128, 128, 128, 255],
            [0, 0, 128, 255],
            [128, 0, 128, 255],
            [128, 255, 0, 255],
        ];
        // Stride includes padding that must never become pixels.
        let mut rows = Vec::new();
        for row in colors.chunks_exact(4) {
            for pixel in row {
                rows.extend_from_slice(pixel);
            }
            rows.extend_from_slice(&[99; 8]);
        }
        let source = unsafe {
            renderer.import_memory_strided(
                rows.as_ptr(),
                24,
                Fourcc::Abgr8888,
                (4, 3).into(),
                false,
            )
        }
        .unwrap();
        let crop = Rectangle::new((1., 0.).into(), (3., 2.).into());
        // Explicit expected source labels catch opposite rotation and wrong crop order.
        let cases = [
            (Transform::Normal, (3, 2), vec![1, 2, 3, 5, 6, 7]),
            (Transform::_90, (2, 3), vec![5, 1, 6, 2, 7, 3]),
            (Transform::_180, (3, 2), vec![7, 6, 5, 3, 2, 1]),
            (Transform::_270, (2, 3), vec![3, 7, 2, 6, 1, 5]),
            (Transform::Flipped, (3, 2), vec![3, 2, 1, 7, 6, 5]),
            (Transform::Flipped90, (2, 3), vec![1, 5, 2, 6, 3, 7]),
            (Transform::Flipped180, (3, 2), vec![5, 6, 7, 1, 2, 3]),
            (Transform::Flipped270, (2, 3), vec![7, 3, 6, 2, 5, 1]),
        ];
        for (transform, size, labels) in cases {
            let mut target = renderer
                .create_texture(Fourcc::Abgr8888, size.into())
                .unwrap();
            draw(
                &mut renderer,
                &mut target,
                &source,
                crop,
                size.into(),
                transform,
                Transform::Normal,
            );
            let expected: Vec<_> = labels.into_iter().map(|label| colors[label]).collect();
            assert_eq!(
                pixels(&mut renderer, &target),
                expected,
                "source transform {transform:?}"
            );
        }
        // Output-space transforms must agree with Smithay's physical rectangle contract,
        // not accidentally cancel source rotation or assume a square target.
        let output_cases = [
            (Transform::Normal, (3, 2), vec![1, 2, 3, 5, 6, 7]),
            (Transform::_90, (2, 3), vec![5, 1, 6, 2, 7, 3]),
            (Transform::_180, (3, 2), vec![7, 6, 5, 3, 2, 1]),
            (Transform::_270, (2, 3), vec![3, 7, 2, 6, 1, 5]),
            (Transform::Flipped, (3, 2), vec![3, 2, 1, 7, 6, 5]),
            (Transform::Flipped90, (2, 3), vec![7, 3, 6, 2, 5, 1]),
            (Transform::Flipped180, (3, 2), vec![5, 6, 7, 1, 2, 3]),
            (Transform::Flipped270, (2, 3), vec![1, 5, 2, 6, 3, 7]),
        ];
        for (transform, actual, labels) in output_cases {
            let mut target = renderer
                .create_texture(Fourcc::Abgr8888, actual.into())
                .unwrap();
            draw(
                &mut renderer,
                &mut target,
                &source,
                crop,
                (3, 2).into(),
                Transform::Normal,
                transform,
            );
            let expected: Vec<_> = labels.iter().map(|&i| colors[i]).collect();
            assert_eq!(
                pixels(&mut renderer, &target),
                expected,
                "output transform {transform:?}"
            );
            let mut framebuffer = renderer.bind(&mut target).unwrap();
            let mut frame = renderer
                .render(&mut framebuffer, actual.into(), transform)
                .unwrap();
            assert_eq!(
                frame.output_size(),
                (3, 2).into(),
                "actual framebuffer vs logical frame size"
            );
            frame
                .clear(
                    Color32F::TRANSPARENT,
                    &[Rectangle::new((1, 0).into(), (1, 1).into())],
                )
                .unwrap();
            frame.finish().unwrap().wait().unwrap();
            let mut expected = expected;
            expected[labels.iter().position(|&label| label == 2).unwrap()] = [0, 0, 0, 0];
            assert_eq!(
                pixels(&mut renderer, &target),
                expected,
                "rotated partial clear {transform:?}"
            );
        }

        // Nonzero dst + relative damage: clear one pixel and draw another; preserve every other
        // pixel.
        let mut target = renderer
            .create_texture(Fourcc::Abgr8888, (6, 4).into())
            .unwrap();
        {
            let mut framebuffer = renderer.bind(&mut target).unwrap();
            let mut frame = renderer
                .render(&mut framebuffer, (6, 4).into(), Transform::Normal)
                .unwrap();
            frame
                .clear(
                    Color32F::new(0., 0., 1., 1.),
                    &[Rectangle::from_size((6, 4).into())],
                )
                .unwrap();
            frame.finish().unwrap().wait().unwrap();
        }
        {
            let mut framebuffer = renderer.bind(&mut target).unwrap();
            let mut frame = renderer
                .render(&mut framebuffer, (6, 4).into(), Transform::Normal)
                .unwrap();
            frame
                .clear(
                    Color32F::TRANSPARENT,
                    &[Rectangle::new((4, 3).into(), (1, 1).into())],
                )
                .unwrap();
            frame
                .draw_solid(
                    Rectangle::new((2, 1).into(), (3, 2).into()),
                    &[Rectangle::new((1, 0).into(), (1, 1).into())],
                    Color32F::new(0.5, 0., 0., 0.5),
                )
                .unwrap();
            frame.finish().unwrap().wait().unwrap();
        }
        let mut expected = vec![[0, 0, 255, 255]; 24];
        expected[9] = [128, 0, 127, 255];
        expected[22] = [0, 0, 0, 0];
        assert_eq!(
            pixels(&mut renderer, &target),
            expected,
            "damage must not erase unchanged target pixels"
        );

        let translucent = renderer
            .import_memory(&[128, 0, 0, 128], Fourcc::Abgr8888, (1, 1).into(), false)
            .unwrap();
        let mut framebuffer = renderer.bind(&mut target).unwrap();
        let mut frame = renderer
            .render(&mut framebuffer, (6, 4).into(), Transform::Normal)
            .unwrap();
        frame
            .draw_texture(
                &translucent,
                Rectangle::from_size((1., 1.).into()),
                Rectangle::new((0, 0).into(), (1, 1).into()),
                &[Rectangle::from_size((1, 1).into())],
                &[],
                Transform::Normal,
                0.5,
            )
            .unwrap();
        frame.finish().unwrap().wait().unwrap();
        assert_eq!(
            pixels(&mut renderer, &target)[0],
            [64, 0, 191, 255],
            "premultiplied alpha applied exactly once"
        );

        let flipped = renderer
            .import_memory(
                &colors.into_iter().flatten().collect::<Vec<_>>(),
                Fourcc::Abgr8888,
                (4, 3).into(),
                true,
            )
            .unwrap();
        let mut target = renderer
            .create_texture(Fourcc::Abgr8888, (4, 3).into())
            .unwrap();
        draw(
            &mut renderer,
            &mut target,
            &flipped,
            Rectangle::from_size((4., 3.).into()),
            (4, 3).into(),
            Transform::Normal,
            Transform::Normal,
        );
        let expected: Vec<_> = [8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3]
            .into_iter()
            .map(|i| colors[i])
            .collect();
        assert_eq!(
            pixels(&mut renderer, &target),
            expected,
            "buffer y inversion"
        );
    }

    #[test]
    fn native_linear_sampling_retains_edges_and_outside_clamp() {
        let mut renderer = VelloRenderer::new_headless().unwrap();
        let colors = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 0, 255],
        ];
        let source = renderer
            .import_memory(
                &colors.into_iter().flatten().collect::<Vec<_>>(),
                Fourcc::Abgr8888,
                (2, 2).into(),
                false,
            )
            .unwrap();
        // Default Linear must be identity at every 1:1 texel center, including last row/column.
        for transform in [Transform::Normal, Transform::_90] {
            let mut target = renderer
                .create_texture(Fourcc::Abgr8888, (2, 2).into())
                .unwrap();
            draw(
                &mut renderer,
                &mut target,
                &source,
                Rectangle::from_size((2., 2.).into()),
                (2, 2).into(),
                Transform::Normal,
                transform,
            );
            let expected = if transform == Transform::Normal {
                colors.to_vec()
            } else {
                vec![colors[2], colors[0], colors[3], colors[1]]
            };
            assert_eq!(
                pixels(&mut renderer, &target),
                expected,
                "Linear 1:1 edge pixels {transform:?}"
            );
        }
        let source = renderer
            .import_memory(
                &[255, 0, 0, 255, 0, 0, 255, 255],
                Fourcc::Abgr8888,
                (2, 1).into(),
                false,
            )
            .unwrap();
        let mut target = renderer
            .create_texture(Fourcc::Abgr8888, (3, 1).into())
            .unwrap();
        draw(
            &mut renderer,
            &mut target,
            &source,
            Rectangle::new((0.25, 0.).into(), (1.5, 1.).into()),
            (3, 1).into(),
            Transform::Normal,
            Transform::Normal,
        );
        assert_eq!(
            pixels(&mut renderer, &target),
            vec![[255, 0, 0, 255], [128, 0, 128, 255], [0, 0, 255, 255]],
            "fractional in-bounds crop samples half-texel boundaries"
        );

        // Samples extend more than one whole image outside BOTH sides. Reflect alone would
        // wrap blue onto the left and red onto the right; clamp must keep edge colors.
        let crop = Rectangle::new((-2.25, 0.).into(), (6.5, 1.).into());
        let mut target = renderer
            .create_texture(Fourcc::Abgr8888, (13, 1).into())
            .unwrap();
        draw(
            &mut renderer,
            &mut target,
            &source,
            crop,
            (13, 1).into(),
            Transform::Normal,
            Transform::Normal,
        );
        let mut expected = vec![[255, 0, 0, 255]; 6];
        expected.push([128, 0, 128, 255]);
        expected.extend(vec![[0, 0, 255, 255]; 6]);
        assert_eq!(
            pixels(&mut renderer, &target),
            expected,
            "outside Linear edge clamp"
        );
        renderer.upscale_filter(TextureFilter::Nearest).unwrap();
        draw(
            &mut renderer,
            &mut target,
            &source,
            crop,
            (13, 1).into(),
            Transform::Normal,
            Transform::Normal,
        );
        let mut expected = vec![[255, 0, 0, 255]; 6];
        expected.extend(vec![[0, 0, 255, 255]; 7]);
        assert_eq!(
            pixels(&mut renderer, &target),
            expected,
            "outside Nearest differs at fractional texel boundary"
        );
    }

    #[test]
    fn default_blend_applies_to_trait_draw_but_not_raw_effect_output() {
        let mut renderer = VelloRenderer::new_headless().unwrap();
        renderer.set_blend(Some(BlendParams::DisplayP3));
        let source = renderer
            .import_memory(&[255, 0, 0, 255], Fourcc::Abgr8888, (1, 1).into(), false)
            .unwrap();
        let mut target = renderer
            .create_texture(Fourcc::Abgr8888, (2, 1).into())
            .unwrap();
        let mut framebuffer = renderer.bind(&mut target).unwrap();
        let mut frame = renderer
            .render(&mut framebuffer, (2, 1).into(), Transform::Normal)
            .unwrap();
        frame
            .clear(Color32F::BLACK, &[Rectangle::from_size((2, 1).into())])
            .unwrap();
        Frame::render_texture_from_to(
            &mut frame,
            &source,
            Rectangle::from_size((1., 1.).into()),
            Rectangle::new((0, 0).into(), (1, 1).into()),
            &[Rectangle::from_size((1, 1).into())],
            &[],
            Transform::Normal,
            0.5,
        )
        .unwrap();
        frame
            .draw_texture(
                &source,
                Rectangle::from_size((1., 1.).into()),
                Rectangle::new((1, 0).into(), (1, 1).into()),
                &[Rectangle::from_size((1, 1).into())],
                &[],
                Transform::Normal,
                0.5,
            )
            .unwrap();
        frame.finish().unwrap().wait().unwrap();
        let result = pixels(&mut renderer, &target);
        // sRGB red -> DisplayP3 linear primaries (D65); encoded with gamma 2.2 then alpha.
        let linear = [0.8225f32, 0.0332, 0.0171];
        let expected = linear.map(|c| (c.powf(1. / 2.2) * 0.5 * 255.).round() as u8);
        for (actual, expected) in result[0][..3].iter().zip(expected) {
            assert!(
                (*actual as i16 - expected as i16).abs() <= 2,
                "trait default blend {result:?}, expected{expected}"
            );
        }
        assert_eq!(
            result[1],
            [128, 0, 0, 255],
            "raw already-converted draw must not apply default blend"
        );

        // ST 2084 reference: 100 cd/m² is encoded as 0.508078. A 50%-alpha
        // premultiplied reference-white clear is therefore 65/255, not raw 128/255.
        renderer.set_blend(Some(BlendParams::HdrPq {
            ref_lum_scale: 0.01,
        }));
        let mut framebuffer = renderer.bind(&mut target).unwrap();
        let mut frame = renderer
            .render(&mut framebuffer, (2, 1).into(), Transform::Normal)
            .unwrap();
        frame
            .clear(
                Color32F::new(0.5, 0.5, 0.5, 0.5),
                &[Rectangle::new((0, 0).into(), (1, 1).into())],
            )
            .unwrap();
        frame.finish().unwrap().wait().unwrap();
        let result = pixels(&mut renderer, &target);
        assert_eq!(
            result[0],
            [65, 65, 65, 128],
            "clear must encode PQ reference white before premultiplication"
        );
        assert_eq!(
            result[1],
            [128, 0, 0, 255],
            "partial HDR clear preserves unaffected target pixels"
        );
    }
}
