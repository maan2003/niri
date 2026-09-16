//! A smithay renderer that records commands for the GPU process instead of touching GL.
//!
//! Every renderer call becomes a [`Command`]; they are batched and flushed on
//! [`RemoteRenderer::flush`], before any synchronous read, or when the batch grows large.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::{fmt, mem};

use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::format::{get_bpp, FormatSet};
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::egl::display::EGLBufferReader;
use smithay::backend::egl::Error as EglError;
use smithay::backend::renderer::gles::Uniform as GlesUniform;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, DebugFlags, ErasedContextId, ExportMem, Frame, ImportDma,
    ImportDmaWl, ImportEgl, ImportMem, ImportMemWl, Offscreen, Renderer, RendererSuper, Texture,
    TextureFilter, TextureMapping,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::shm::{self, shm_format_to_fourcc};

use super::client::GpuClient;
use super::convert;
use super::protocol::{
    BlurParams, Caps, Command, CursorFrameDesc, CursorMeta, ElementMeta, OutputRef, Rect, Request,
    ShaderKind, ShaderSupport, Target, TexId, TexProgram, MAX_CURSOR_FRAMES,
};

const MAX_PENDING_FDS: usize = 32;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum RemoteError {
    /// The GPU process reported an error while executing a batch.
    Gpu(String),
    /// The GPU process is gone or the socket failed.
    Transport(String),
    Unsupported(&'static str),
    /// Tried to read back from a target that is not a texture.
    InvalidTarget,
    Shm(String),
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Gpu(msg) => write!(f, "gpu process error: {msg}"),
            RemoteError::Transport(msg) => write!(f, "gpu process transport error: {msg}"),
            RemoteError::Unsupported(what) => write!(f, "unsupported: {what}"),
            RemoteError::InvalidTarget => write!(f, "cannot read back from this target"),
            RemoteError::Shm(msg) => write!(f, "shm buffer error: {msg}"),
        }
    }
}

impl std::error::Error for RemoteError {}

#[derive(Default)]
struct Pending {
    commands: Vec<Command>,
    fds: Vec<OwnedFd>,
    bytes: usize,
    /// `Begin`s without their `End` yet. A batch must not be cut inside a frame.
    open_frames: usize,
}

struct Shared {
    client: Mutex<GpuClient>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    context_id: ContextId<RemoteTexture>,
    caps: RwLock<Caps>,
    shaders: Mutex<ShaderSupport>,
    dmabuf_cache: Mutex<HashMap<WeakDmabuf, RemoteTexture>>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("context_id", &self.context_id)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn push(&self, cmd: Command) {
        self.push_with_fds(cmd, Vec::new());
    }

    fn push_with_fds(&self, cmd: Command, fds: Vec<OwnedFd>) {
        let bytes = match &cmd {
            Command::ImportMemory { data, .. } | Command::UpdateMemory { data, .. } => data.len(),
            _ => 0,
        } + 64;

        let needs_flush = {
            let pending = self.pending.lock().unwrap();
            pending.open_frames == 0
                && !pending.fds.is_empty()
                && pending.fds.len() + fds.len() > MAX_PENDING_FDS
        };
        if needs_flush {
            if let Err(err) = self.flush() {
                warn!("error flushing gpu commands: {err}");
            }
        }

        let over_budget = {
            let mut pending = self.pending.lock().unwrap();
            match &cmd {
                Command::Begin { .. } => pending.open_frames += 1,
                Command::End => pending.open_frames = pending.open_frames.saturating_sub(1),
                _ => (),
            }
            pending.bytes += bytes;
            pending.commands.push(cmd);
            pending.fds.extend(fds);
            pending.open_frames == 0
                && (pending.bytes > MAX_PENDING_BYTES || pending.fds.len() > MAX_PENDING_FDS)
        };
        if over_budget {
            if let Err(err) = self.flush() {
                warn!("error flushing gpu commands: {err}");
            }
        }
    }

    fn flush(&self) -> Result<(), RemoteError> {
        let pending = mem::take(&mut *self.pending.lock().unwrap());
        if pending.commands.is_empty() {
            return Ok(());
        }
        let mut client = self.client.lock().unwrap();
        client
            .execute(pending.commands, &pending.fds)
            .map_err(|err| RemoteError::Gpu(format!("{err:#}")))
    }

    fn alloc_id(&self) -> TexId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Handle to the GPU process for code that doesn't render itself.
#[derive(Clone)]
pub struct GpuHandle {
    shared: Arc<Shared>,
}

impl GpuHandle {
    /// Whether the GPU process has a renderer up (it comes with the primary DRM device).
    pub fn is_ready(&self) -> bool {
        !self.shared.caps.read().unwrap().renderer.is_empty()
    }

    pub fn context_id(&self) -> ContextId<RemoteTexture> {
        self.shared.context_id.clone()
    }

    /// Loads an Xcursor icon in the GPU process; returns each frame with its texture.
    pub fn load_cursor(
        &self,
        theme: &str,
        names: &[String],
        size: i32,
        fallback: bool,
    ) -> anyhow::Result<Vec<(CursorFrameDesc, RemoteTexture)>> {
        self.shared.flush()?;
        let first_id = self
            .shared
            .next_id
            .fetch_add(MAX_CURSOR_FRAMES, Ordering::Relaxed);
        let frames = self
            .shared
            .client
            .lock()
            .unwrap()
            .load_cursor(theme, names, size, fallback, first_id)?;
        Ok(frames
            .into_iter()
            .enumerate()
            .map(|(i, frame)| {
                let texture = RemoteTexture(Arc::new(TexInner {
                    id: first_id + i as u64,
                    size: Size::from((frame.width as i32, frame.height as i32)),
                    format: Some(Fourcc::Argb8888),
                    shared: Arc::downgrade(&self.shared),
                }));
                (frame, texture)
            })
            .collect())
    }
}

/// Handle to a texture living in the GPU process.
///
/// Dropping the last clone destroys the remote texture.
#[derive(Clone)]
pub struct RemoteTexture(Arc<TexInner>);

struct TexInner {
    id: TexId,
    size: Size<i32, Buffer>,
    format: Option<Fourcc>,
    shared: Weak<Shared>,
}

impl Drop for TexInner {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.push(Command::DestroyTexture { id: self.id });
        }
    }
}

impl RemoteTexture {
    pub fn id(&self) -> TexId {
        self.0.id
    }

    pub fn is_unique_reference(&mut self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}

impl fmt::Debug for RemoteTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteTexture")
            .field("id", &self.0.id)
            .field("size", &self.0.size)
            .field("format", &self.0.format)
            .finish()
    }
}

impl Texture for RemoteTexture {
    fn width(&self) -> u32 {
        self.0.size.w as u32
    }

    fn height(&self) -> u32 {
        self.0.size.h as u32
    }

    fn size(&self) -> Size<i32, Buffer> {
        self.0.size
    }

    fn format(&self) -> Option<Fourcc> {
        self.0.format
    }
}

/// Handle to a texture shader program in the GPU process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTexProgram(pub TexProgram);

/// A bound render target.
#[derive(Debug)]
pub struct RemoteTarget<'a> {
    target: Target,
    size: Size<i32, Buffer>,
    format: Option<Fourcc>,
    _keep: Option<RemoteTexture>,
    _marker: PhantomData<&'a mut ()>,
}

impl RemoteTarget<'_> {
    pub fn target(&self) -> Target {
        self.target
    }
}

impl Texture for RemoteTarget<'_> {
    fn width(&self) -> u32 {
        self.size.w as u32
    }

    fn height(&self) -> u32 {
        self.size.h as u32
    }

    fn size(&self) -> Size<i32, Buffer> {
        self.size
    }

    fn format(&self) -> Option<Fourcc> {
        self.format
    }
}

#[derive(Debug)]
pub struct RemoteMapping {
    data: Vec<u8>,
    size: Size<i32, Buffer>,
    format: Fourcc,
}

impl Texture for RemoteMapping {
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

impl TextureMapping for RemoteMapping {
    fn flipped(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct RemoteRenderer {
    shared: Arc<Shared>,
    debug_flags: DebugFlags,
}

impl RemoteRenderer {
    pub fn new(client: GpuClient) -> Self {
        let caps = client.caps().cloned().unwrap_or_default();
        let shaders = caps.shaders;
        Self {
            shared: Arc::new(Shared {
                client: Mutex::new(client),
                pending: Mutex::new(Pending::default()),
                next_id: AtomicU64::new(1),
                context_id: ContextId::new(),
                caps: RwLock::new(caps),
                shaders: Mutex::new(shaders),
                dmabuf_cache: Mutex::new(HashMap::new()),
            }),
            debug_flags: DebugFlags::empty(),
        }
    }

    pub fn caps(&self) -> Caps {
        self.shared.caps.read().unwrap().clone()
    }

    /// Formats the GPU process can render into, for allocating screencast / capture buffers.
    pub fn dmabuf_render_formats(&self) -> FormatSet {
        self.shared
            .caps
            .read()
            .unwrap()
            .dmabuf_render_formats
            .iter()
            .filter_map(|&(code, modifier)| {
                Some(Format {
                    code: Fourcc::try_from(code).ok()?,
                    modifier: Modifier::from(modifier),
                })
            })
            .collect()
    }

    /// Called once the GPU process has brought up its renderer (after the primary DRM device
    /// was added).
    pub fn set_caps(&self, caps: Caps) {
        *self.shared.shaders.lock().unwrap() = caps.shaders;
        *self.shared.caps.write().unwrap() = caps;
    }

    /// The GPU-process connection, for requests that are not renderer calls (DRM/KMS).
    pub fn client(&self) -> MutexGuard<'_, GpuClient> {
        self.shared.client.lock().unwrap()
    }

    /// A handle for allocating GPU-side render buffers, usable without the renderer.
    pub fn dmabuf_allocator(&self) -> DmabufAllocator {
        DmabufAllocator {
            shared: self.shared.clone(),
        }
    }

    /// A render target that scans out on `output`. The recorded frame is drawn when the
    /// core sends `Request::Present`.
    pub fn output_target(
        &self,
        output: OutputRef,
        size: Size<i32, Physical>,
    ) -> RemoteTarget<'static> {
        RemoteTarget {
            target: Target::Output(output),
            size: Size::from((size.w, size.h)),
            format: None,
            _keep: None,
            _marker: PhantomData,
        }
    }

    /// Handle for code that talks to the GPU process without rendering (cursor loading).
    pub fn gpu_handle(&self) -> GpuHandle {
        GpuHandle {
            shared: self.shared.clone(),
        }
    }

    /// Encodes `region` of `texture` as PNG in the GPU process. The bytes arrive later as
    /// `GpuEvent::Png { token, .. }`.
    pub fn encode_png(
        &self,
        texture: &RemoteTexture,
        region: Rectangle<i32, Buffer>,
        token: u64,
    ) -> anyhow::Result<()> {
        // Queued commands (incl. the render into `texture`) must run first.
        self.shared.flush()?;
        self.shared.client.lock().unwrap().send_oneway(
            &Request::EncodePng {
                token,
                id: texture.id(),
                region: convert::rect(region),
            },
            &[],
        )
    }

    /// Parameters for the next `cast_target` frame of `stream`.
    pub fn cast_frame_info(
        &self,
        stream: u64,
        scale: f64,
        target_time_ns: u64,
        cursor: Option<CursorMeta>,
    ) {
        self.shared.push(Command::CastFrameInfo {
            stream,
            scale,
            target_time_ns,
            cursor,
        });
    }

    /// Target for a screencast stream's next buffer; the GPU renders it when the frame ends.
    pub fn cast_target(&self, stream: u64, size: Size<i32, Physical>) -> RemoteTarget<'static> {
        RemoteTarget {
            target: Target::Cast(stream),
            size: Size::from((size.w, size.h)),
            format: None,
            _keep: None,
            _marker: PhantomData,
        }
    }

    /// Target for a screencast stream's cursor bitmap (metadata cursor mode).
    pub fn cast_cursor_target(
        &self,
        stream: u64,
        size: Size<i32, Physical>,
    ) -> RemoteTarget<'static> {
        RemoteTarget {
            target: Target::CastCursor(stream),
            size: Size::from((size.w, size.h)),
            format: None,
            _keep: None,
            _marker: PhantomData,
        }
    }

    /// Allocates a framebuffer-capture slot in the GPU process.
    pub fn new_capture(&self) -> CaptureHandle {
        CaptureHandle(Arc::new(CaptureInner {
            key: self.shared.alloc_id(),
            shared: Arc::downgrade(&self.shared),
        }))
    }

    /// Blurs `src` into a new texture. The GPU process caches its pyramid textures per `key`.
    pub fn blur_texture(
        &mut self,
        key: u64,
        src: &RemoteTexture,
        params: BlurParams,
    ) -> RemoteTexture {
        let id = self.shared.alloc_id();
        self.shared.push(Command::Blur {
            key,
            src: src.id(),
            dst: id,
            params,
        });
        self.texture(id, src.size(), Some(Fourcc::Abgr8888))
    }

    /// Which shader programs the GPU process managed to compile.
    pub fn shaders(&self) -> ShaderSupport {
        *self.shared.shaders.lock().unwrap()
    }

    pub fn tex_program(&self, program: TexProgram) -> Option<RemoteTexProgram> {
        let shaders = self.shaders();
        let available = match program {
            TexProgram::ClippedSurface => shaders.clipped_surface,
            TexProgram::PostprocessAndClip => shaders.postprocess_and_clip,
            TexProgram::GradientFade => shaders.gradient_fade,
        };
        available.then_some(RemoteTexProgram(program))
    }

    /// Sends all recorded commands to the GPU process and waits for them to be accepted.
    pub fn flush(&self) -> Result<(), RemoteError> {
        self.shared.flush()
    }

    /// Replaces a customizable shader (resize/close/open) with `src`, or restores the default.
    pub fn set_custom_shader(
        &self,
        kind: ShaderKind,
        src: Option<&str>,
    ) -> Result<(), RemoteError> {
        self.flush()?;
        let ok = self
            .client()
            .set_custom_shader(kind, src)
            .map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        let mut shaders = self.shared.shaders.lock().unwrap();
        match kind {
            ShaderKind::Resize => shaders.resize = ok,
            ShaderKind::Close => shaders.close = ok,
            ShaderKind::Open => shaders.open = ok,
            _ => (),
        }
        Ok(())
    }

    fn texture(&self, id: TexId, size: Size<i32, Buffer>, format: Option<Fourcc>) -> RemoteTexture {
        RemoteTexture(Arc::new(TexInner {
            id,
            size,
            format,
            shared: Arc::downgrade(&self.shared),
        }))
    }

    fn read(
        &self,
        id: TexId,
        region: Rectangle<i32, Buffer>,
        format: Fourcc,
    ) -> Result<RemoteMapping, RemoteError> {
        self.flush()?;
        let mut client = self.shared.client.lock().unwrap();
        let image = client
            .read_texture(id, convert::rect(region), format as u32)
            .map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        Ok(RemoteMapping {
            data: image.data,
            size: Size::from((image.width as i32, image.height as i32)),
            format,
        })
    }
}

pub struct RemoteFrame<'frame, 'buffer> {
    renderer: &'frame mut RemoteRenderer,
    // Keeping the target borrowed ties 'buffer to 'frame like GlesFrame does.
    target: &'frame mut RemoteTarget<'buffer>,
    size: Size<i32, Physical>,
    transform: Transform,
}

impl fmt::Debug for RemoteFrame<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteFrame")
            .field("size", &self.size)
            .field("transform", &self.transform)
            .finish()
    }
}

impl RemoteFrame<'_, '_> {
    /// GPU spans are recorded in the GPU process; here this is a plain passthrough so render
    /// elements can keep their `with_gpu_span` calls.
    pub fn with_gpu_span<L, F, R>(&mut self, _location: L, func: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        func(self)
    }

    pub fn renderer(&mut self) -> &mut RemoteRenderer {
        self.renderer
    }

    pub fn target(&self) -> Target {
        self.target.target
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_texture_from_to(
        &mut self,
        texture: &RemoteTexture,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        transform: Transform,
        alpha: f32,
        program: Option<&RemoteTexProgram>,
        uniforms: &[GlesUniform<'_>],
    ) -> Result<(), RemoteError> {
        self.renderer.shared.push(Command::DrawTexture {
            texture: texture.id(),
            src: convert::rect_f64(src),
            dst: convert::rect(dst),
            damage: convert::rects(damage),
            opaque: convert::rects(opaque_regions),
            transform: convert::transform(transform),
            alpha,
            program: program.map(|p| p.0),
            uniforms: convert::uniforms(uniforms),
        });
        Ok(())
    }

    pub fn override_default_tex_program(
        &mut self,
        program: RemoteTexProgram,
        uniforms: Vec<GlesUniform<'static>>,
    ) {
        self.renderer.shared.push(Command::OverrideTexProgram {
            program: program.0,
            uniforms: convert::uniforms(&uniforms),
        });
    }

    pub fn clear_tex_program_override(&mut self) {
        self.renderer.shared.push(Command::ClearTexProgramOverride);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_element(&mut self, meta: ElementMeta) {
        self.renderer.shared.push(Command::BeginElement(meta));
    }

    pub fn begin_element_draw(&mut self) {
        self.renderer.shared.push(Command::BeginElementDraw);
    }

    pub fn end_element(&mut self) {
        self.renderer.shared.push(Command::EndElement);
    }

    pub fn draw_shader(
        &mut self,
        program: ShaderKind,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        scale: f32,
        alpha: f32,
        uniforms: &[GlesUniform<'_>],
        textures: &[(String, RemoteTexture)],
    ) {
        self.renderer.shared.push(Command::DrawShader {
            program,
            src: convert::rect_f64(src),
            dst: convert::rect(dst),
            damage: convert::rects(damage),
            scale,
            alpha,
            uniforms: convert::uniforms(uniforms),
            textures: textures.iter().map(|(n, t)| (n.clone(), t.id())).collect(),
        });
    }

    pub fn capture_framebuffer(
        &mut self,
        capture: &CaptureHandle,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        scale: f32,
        blur: Option<BlurParams>,
    ) {
        self.renderer.shared.push(Command::CaptureFramebuffer {
            key: capture.key(),
            src: convert::rect_f64(src),
            dst: convert::rect(dst),
            scale,
            blur,
        });
    }

    pub fn draw_captured(
        &mut self,
        capture: &CaptureHandle,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        uniforms: &[GlesUniform<'_>],
    ) {
        self.renderer.shared.push(Command::DrawCaptured {
            key: capture.key(),
            dst: convert::rect(dst),
            damage: convert::rects(damage),
            uniforms: convert::uniforms(uniforms),
        });
    }
}

/// A framebuffer-capture slot in the GPU process; freed when dropped.
#[derive(Debug, Clone)]
pub struct CaptureHandle(Arc<CaptureInner>);

#[derive(Debug)]
struct CaptureInner {
    key: u64,
    shared: Weak<Shared>,
}

impl CaptureHandle {
    pub fn key(&self) -> u64 {
        self.0.key
    }
}

impl Drop for CaptureInner {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.push(Command::DestroyCapture { key: self.key });
        }
    }
}

impl Frame for RemoteFrame<'_, '_> {
    type Error = RemoteError;
    type TextureId = RemoteTexture;

    fn context_id(&self) -> ContextId<RemoteTexture> {
        self.renderer.shared.context_id.clone()
    }

    fn clear(
        &mut self,
        color: Color32F,
        at: &[Rectangle<i32, Physical>],
    ) -> Result<(), RemoteError> {
        self.renderer.shared.push(Command::Clear {
            color: color.components(),
            at: convert::rects(at),
        });
        Ok(())
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), RemoteError> {
        self.renderer.shared.push(Command::DrawSolid {
            dst: convert::rect(dst),
            damage: convert::rects(damage),
            color: color.components(),
        });
        Ok(())
    }

    fn render_texture_from_to(
        &mut self,
        texture: &RemoteTexture,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
    ) -> Result<(), RemoteError> {
        RemoteFrame::render_texture_from_to(
            self,
            texture,
            src,
            dst,
            damage,
            opaque_regions,
            src_transform,
            alpha,
            None,
            &[],
        )
    }

    fn transformation(&self) -> Transform {
        self.transform
    }

    fn output_size(&self) -> Size<i32, Physical> {
        self.size
    }

    fn wait(&mut self, _sync: &SyncPoint) -> Result<(), RemoteError> {
        Ok(())
    }

    fn finish(self) -> Result<SyncPoint, RemoteError> {
        self.renderer.shared.push(Command::End);
        if let Target::Dmabuf(_) = self.target.target {
            // The caller hands this buffer to another process right away (PipeWire, an
            // image-copy client), so make "signaled" true: run and finish it on the GPU now.
            self.renderer.shared.flush()?;
            self.renderer
                .client()
                .sync()
                .map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        }
        Ok(SyncPoint::signaled())
    }
}

impl RendererSuper for RemoteRenderer {
    type Error = RemoteError;
    type TextureId = RemoteTexture;
    type Framebuffer<'buffer> = RemoteTarget<'buffer>;
    type Frame<'frame, 'buffer>
        = RemoteFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl Renderer for RemoteRenderer {
    fn context_id(&self) -> ContextId<RemoteTexture> {
        self.shared.context_id.clone()
    }

    fn downscale_filter(&mut self, _filter: TextureFilter) -> Result<(), RemoteError> {
        Ok(())
    }

    fn upscale_filter(&mut self, _filter: TextureFilter) -> Result<(), RemoteError> {
        Ok(())
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.debug_flags = flags;
        self.shared.push(Command::SetDebugFlags {
            flags: flags.bits(),
        });
    }

    fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
    }

    fn render<'frame, 'buffer>(
        &'frame mut self,
        framebuffer: &'frame mut RemoteTarget<'buffer>,
        output_size: Size<i32, Physical>,
        dst_transform: Transform,
    ) -> Result<RemoteFrame<'frame, 'buffer>, RemoteError>
    where
        'buffer: 'frame,
    {
        self.shared.push(Command::Begin {
            target: framebuffer.target,
            width: output_size.w,
            height: output_size.h,
            transform: convert::transform(dst_transform),
        });
        Ok(RemoteFrame {
            renderer: self,
            target: framebuffer,
            size: output_size,
            transform: dst_transform,
        })
    }

    fn wait(&mut self, _sync: &SyncPoint) -> Result<(), RemoteError> {
        Ok(())
    }
}

impl Bind<RemoteTexture> for RemoteRenderer {
    fn bind<'a>(&mut self, target: &'a mut RemoteTexture) -> Result<RemoteTarget<'a>, RemoteError> {
        Ok(RemoteTarget {
            target: Target::Texture(target.id()),
            size: target.size(),
            format: target.format(),
            _keep: Some(target.clone()),
            _marker: PhantomData,
        })
    }
}

impl Offscreen<RemoteTexture> for RemoteRenderer {
    fn create_buffer(
        &mut self,
        format: Fourcc,
        size: Size<i32, Buffer>,
    ) -> Result<RemoteTexture, RemoteError> {
        let id = self.shared.alloc_id();
        self.shared.push(Command::CreateTexture {
            id,
            format: format as u32,
            width: size.w,
            height: size.h,
        });
        Ok(self.texture(id, size, Some(format)))
    }
}

/// Allocates dmabufs in the GPU process (screencast and capture targets). The buffers are
/// rendered into GPU-side via `Bind<Dmabuf>`; the core only passes fds around.
#[derive(Clone)]
pub struct DmabufAllocator {
    shared: Arc<Shared>,
}

impl fmt::Debug for DmabufAllocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmabufAllocator").finish_non_exhaustive()
    }
}

impl DmabufAllocator {
    pub fn allocate(
        &self,
        size: Size<u32, Buffer>,
        fourcc: Fourcc,
        modifiers: &[Modifier],
    ) -> anyhow::Result<Dmabuf> {
        // Ordering with queued commands doesn't matter for a fresh buffer, but keep the
        // channel simple: everything pending goes first.
        self.shared.flush()?;
        let modifiers = modifiers.iter().map(|m| u64::from(*m)).collect();
        self.shared
            .client
            .lock()
            .unwrap()
            .allocate_dmabuf(size.w, size.h, fourcc as u32, modifiers)
    }
}

impl Bind<Dmabuf> for RemoteRenderer {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<RemoteTarget<'a>, RemoteError> {
        let texture = self.import_dmabuf(target, None)?;
        Ok(RemoteTarget {
            target: Target::Dmabuf(texture.id()),
            size: texture.size(),
            format: texture.format(),
            _keep: Some(texture),
            _marker: PhantomData,
        })
    }
}

impl ImportMem for RemoteRenderer {
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, Buffer>,
        flipped: bool,
    ) -> Result<RemoteTexture, RemoteError> {
        let id = self.shared.alloc_id();
        self.shared.push(Command::ImportMemory {
            id,
            format: format as u32,
            width: size.w,
            height: size.h,
            flipped,
            data: data.to_vec(),
        });
        Ok(self.texture(id, size, Some(format)))
    }

    fn update_memory(
        &mut self,
        texture: &RemoteTexture,
        data: &[u8],
        region: Rectangle<i32, Buffer>,
    ) -> Result<(), RemoteError> {
        // `data` is the whole buffer (smithay semantics); ship only the rows of `region`.
        let bpp = texture
            .format()
            .and_then(get_bpp)
            .ok_or(RemoteError::Unsupported("memory format"))?
            / 8;
        let size = texture.size();
        let region =
            region
                .intersection(Rectangle::from_size(size))
                .ok_or(RemoteError::Unsupported(
                    "update region outside the texture",
                ))?;
        let stride = size.w as usize * bpp;
        let row_len = region.size.w as usize * bpp;
        let mut rows = Vec::with_capacity(row_len * region.size.h as usize);
        for y in region.loc.y..region.loc.y + region.size.h {
            let start = y as usize * stride + region.loc.x as usize * bpp;
            let row = data
                .get(start..start + row_len)
                .ok_or(RemoteError::Unsupported("update data too short"))?;
            rows.extend_from_slice(row);
        }
        self.shared.push(Command::UpdateMemory {
            id: texture.id(),
            region: convert::rect(region),
            data: rows,
        });
        Ok(())
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        let formats: Vec<Fourcc> = self
            .shared
            .caps
            .read()
            .unwrap()
            .mem_formats
            .iter()
            .filter_map(|f| Fourcc::try_from(*f).ok())
            .collect();
        Box::new(formats.into_iter())
    }
}

type ShmCache = HashMap<ErasedContextId, RemoteTexture>;

impl ImportMemWl for RemoteRenderer {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, Buffer>],
    ) -> Result<RemoteTexture, RemoteError> {
        let cache = surface.map(|surface| {
            surface
                .data_map
                .get_or_insert_threadsafe(|| Arc::new(Mutex::new(ShmCache::new())))
                .clone()
        });
        let context_id = self.shared.context_id.erased();

        let result = shm::with_buffer_fd(buffer, |fd, data| {
            let fourcc =
                shm_format_to_fourcc(data.format).ok_or(RemoteError::Unsupported("shm format"))?;
            if !self
                .shared
                .caps
                .read()
                .unwrap()
                .mem_formats
                .contains(&(fourcc as u32))
            {
                return Err(RemoteError::Unsupported("shm format"));
            }
            if data.width <= 0 || data.height <= 0 {
                return Err(RemoteError::Shm("empty buffer".into()));
            }
            let size = Size::<i32, Buffer>::from((data.width, data.height));
            let existing = cache
                .as_ref()
                .and_then(|cache| cache.lock().unwrap().get(&context_id).cloned())
                .filter(|tex| tex.size() == size && tex.format() == Some(fourcc));

            // The GPU process reads the pool itself; we only pass the fd along. Nothing here
            // maps client memory, so a truncated pool can't SIGBUS the compositor.
            let fd = fd
                .try_clone_to_owned()
                .map_err(|err| RemoteError::Transport(err.to_string()))?;
            let (id, damage) = match &existing {
                Some(texture) => {
                    let full = Rectangle::from_size(size);
                    let regions: Vec<Rect<i32>> = if damage.is_empty() {
                        vec![convert::rect(full)]
                    } else {
                        damage
                            .iter()
                            .filter_map(|r| r.intersection(full))
                            .map(convert::rect)
                            .collect()
                    };
                    (texture.id(), Some(regions))
                }
                None => (self.shared.alloc_id(), None),
            };
            self.shared.push_with_fds(
                Command::ImportShm {
                    id,
                    format: fourcc as u32,
                    width: data.width,
                    height: data.height,
                    stride: data.stride,
                    offset: data.offset,
                    damage,
                },
                vec![fd],
            );
            if let Some(texture) = existing {
                return Ok(texture);
            }
            let texture = self.texture(id, size, Some(fourcc));
            if let Some(cache) = &cache {
                cache
                    .lock()
                    .unwrap()
                    .insert(context_id.clone(), texture.clone());
            }
            Ok(texture)
        });
        result.map_err(|err| RemoteError::Shm(format!("{err:?}")))?
    }
}

impl ImportDma for RemoteRenderer {
    fn dmabuf_formats(&self) -> FormatSet {
        self.shared
            .caps
            .read()
            .unwrap()
            .dmabuf_formats
            .iter()
            .filter_map(|(code, modifier)| {
                Some(Format {
                    code: Fourcc::try_from(*code).ok()?,
                    modifier: Modifier::from(*modifier),
                })
            })
            .collect()
    }

    fn import_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        damage: Option<&[Rectangle<i32, Buffer>]>,
    ) -> Result<RemoteTexture, RemoteError> {
        let weak = dmabuf.weak();
        {
            let mut cache = self.shared.dmabuf_cache.lock().unwrap();
            if let Some(texture) = cache.get(&weak).cloned() {
                return Ok(texture);
            }
            cache.retain(|weak, _| weak.upgrade().is_some());
        }

        let format = dmabuf.format();
        let (desc, fds) = super::exec::describe_dmabuf(dmabuf)
            .map_err(|err| RemoteError::Transport(err.to_string()))?;
        // Synchronous: a rejected buffer must not take a whole batch down with it, and the
        // caller (dmabuf global) wants a yes/no answer.
        let _ = damage;
        self.flush()?;
        let id = self.shared.alloc_id();
        self.client()
            .import_dmabuf(id, desc, &fds)
            .map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        let texture = self.texture(id, dmabuf.size(), Some(format.code));
        self.shared
            .dmabuf_cache
            .lock()
            .unwrap()
            .insert(weak, texture.clone());
        Ok(texture)
    }
}

impl ImportDmaWl for RemoteRenderer {}

impl ImportEgl for RemoteRenderer {
    fn bind_wl_display(&mut self, _display: &DisplayHandle) -> Result<(), EglError> {
        // Legacy wl_drm buffers aren't supported; clients use linux-dmabuf.
        Err(EglError::NoEGLDisplayBound)
    }

    fn unbind_wl_display(&mut self) {}

    fn egl_reader(&self) -> Option<&EGLBufferReader> {
        None
    }

    fn import_egl_buffer(
        &mut self,
        _buffer: &WlBuffer,
        _surface: Option<&SurfaceData>,
        _damage: &[Rectangle<i32, Buffer>],
    ) -> Result<RemoteTexture, RemoteError> {
        Err(RemoteError::Unsupported("EGL buffers"))
    }
}

impl ExportMem for RemoteRenderer {
    type TextureMapping = RemoteMapping;

    fn copy_framebuffer(
        &mut self,
        target: &RemoteTarget<'_>,
        region: Rectangle<i32, Buffer>,
        format: Fourcc,
    ) -> Result<RemoteMapping, RemoteError> {
        match target.target {
            // A dmabuf target is read back through the texture imported from the same buffer.
            Target::Texture(id) | Target::Dmabuf(id) => self.read(id, region, format),
            Target::Output(_) | Target::Cast(_) | Target::CastCursor(_) => {
                Err(RemoteError::InvalidTarget)
            }
        }
    }

    fn copy_texture(
        &mut self,
        texture: &RemoteTexture,
        region: Rectangle<i32, Buffer>,
        format: Fourcc,
    ) -> Result<RemoteMapping, RemoteError> {
        self.read(texture.id(), region, format)
    }

    fn can_read_texture(&mut self, _texture: &RemoteTexture) -> Result<bool, RemoteError> {
        Ok(true)
    }

    fn map_texture<'a>(&mut self, mapping: &'a RemoteMapping) -> Result<&'a [u8], RemoteError> {
        Ok(&mapping.data)
    }
}
