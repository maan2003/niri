//! A smithay renderer that describes frames for the GPU process instead of rendering locally.
//!
//! Resource calls become [`Command`]s right away. Draw calls are collected into a
//! [`SceneFrame`] (nodes of ops) that is sent as one command when the frame finishes. Commands
//! are batched and flushed on [`RemoteRenderer::flush`], before any synchronous read, or when
//! the batch grows large.

use std::collections::HashMap;
use std::fs::File;
use std::marker::PhantomData;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::{fmt, mem};

use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::format::{get_bpp, FormatSet};
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, DebugFlags, ErasedContextId, ExportMem, Frame, ImportDma,
    ImportDmaWl, ImportMem, ImportMemWl, Offscreen, Renderer, RendererSuper, Texture,
    TextureFilter, TextureMapping,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::shm::{self, shm_format_to_fourcc};

use super::client::GpuClient;
use super::convert;
use super::protocol::{
    BlendParams, BlurParams, Caps, CastInfo, Command, CursorFrameDesc, Node, Op, OutputRef, Paint,
    PostprocessParams, Rect, Request, SceneFrame, Target, TexId, TextureOptions, UiScene,
    ANONYMOUS_NODE, MAX_CURSOR_FRAMES,
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
    /// Frames being described right now. While one is open, destroys are held back so a
    /// texture dropped mid-frame still exists when the frame runs.
    open_frames: usize,
    held: Vec<Command>,
}

struct Shared {
    client: Mutex<GpuClient>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    context_id: ContextId<RemoteTexture>,
    caps: RwLock<Caps>,
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
            !pending.fds.is_empty() && pending.fds.len() + fds.len() > MAX_PENDING_FDS
        };
        if needs_flush {
            if let Err(err) = self.flush() {
                warn!("error flushing gpu commands: {err}");
            }
        }

        let over_budget = {
            let mut pending = self.pending.lock().unwrap();
            pending.bytes += bytes;
            match cmd {
                Command::DestroyTexture { .. } | Command::DestroyCapture { .. }
                    if pending.open_frames > 0 =>
                {
                    pending.held.push(cmd);
                }
                cmd => pending.commands.push(cmd),
            }
            pending.fds.extend(fds);
            pending.bytes > MAX_PENDING_BYTES || pending.fds.len() > MAX_PENDING_FDS
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

    fn open_frame(&self) {
        self.pending.lock().unwrap().open_frames += 1;
    }

    /// Sends the finished frame (if any) and lets the destroys held during it through.
    fn close_frame(&self, frame: Option<SceneFrame>) {
        if let Some(frame) = frame {
            self.push(Command::Frame(Box::new(frame)));
        }
        let held = {
            let mut pending = self.pending.lock().unwrap();
            pending.open_frames = pending.open_frames.saturating_sub(1);
            if pending.open_frames == 0 {
                mem::take(&mut pending.held)
            } else {
                Vec::new()
            }
        };
        for cmd in held {
            self.push(cmd);
        }
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

    /// Uploads an Xcursor icon file to the GPU process; returns each frame with its texture.
    pub fn load_cursor(
        &self,
        icon: Option<&File>,
        size: i32,
        fallback: bool,
    ) -> anyhow::Result<Vec<(CursorFrameDesc, RemoteTexture)>> {
        self.shared.flush()?;
        let first_id = self
            .shared
            .next_id
            .fetch_add(MAX_CURSOR_FRAMES, Ordering::Relaxed);
        let frames = self.shared.client.lock().unwrap().load_cursor(
            icon.map(AsFd::as_fd),
            size,
            fallback,
            first_id,
        )?;
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

/// A bound render target.
#[derive(Debug)]
pub struct RemoteTarget<'a> {
    target: Target,
    size: Size<i32, Buffer>,
    format: Option<Fourcc>,
    cast: Option<CastInfo>,
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
    /// Blend space for frames begun from now on (`None` = SDR); see `SceneFrame::blend`.
    frame_blend: Option<BlendParams>,
}

impl RemoteRenderer {
    pub fn new(client: GpuClient) -> Self {
        let caps = client.caps().cloned().unwrap_or_default();
        Self {
            shared: Arc::new(Shared {
                client: Mutex::new(client),
                pending: Mutex::new(Pending::default()),
                next_id: AtomicU64::new(1),
                context_id: ContextId::new(),
                caps: RwLock::new(caps),
                dmabuf_cache: Mutex::new(HashMap::new()),
            }),
            debug_flags: DebugFlags::empty(),
            frame_blend: None,
        }
    }

    /// Sets the blend space of frames begun from now on. Set it around rendering an HDR /
    /// wide-gamut output and reset to `None` after, so casts and screenshots stay SDR.
    pub fn set_frame_blend(&mut self, blend: Option<BlendParams>) {
        self.frame_blend = blend;
    }

    pub fn frame_blend(&self) -> Option<BlendParams> {
        self.frame_blend
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
        *self.shared.caps.write().unwrap() = caps;
    }

    /// Swaps in a fresh GPU process. Only valid while nothing lives on the GPU side (no
    /// renderer was ready), so all the per-process state to reset is the queue and the dmabuf
    /// import cache. The old client is shut down here.
    pub fn replace_client(&self, client: GpuClient) {
        let caps = client.caps().cloned().unwrap_or_default();
        *self.shared.pending.lock().unwrap() = Pending::default();
        self.shared.dmabuf_cache.lock().unwrap().clear();
        *self.shared.client.lock().unwrap() = client;
        self.set_caps(caps);
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

    /// A render target that scans out on `output`. The frame is drawn when the
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
            cast: None,
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

    /// Target for a screencast stream's next buffer; the GPU renders it when the frame ends.
    pub fn cast_target(
        &self,
        stream: u64,
        size: Size<i32, Physical>,
        info: CastInfo,
    ) -> RemoteTarget<'static> {
        RemoteTarget {
            target: Target::Cast(stream),
            size: Size::from((size.w, size.h)),
            format: None,
            cast: Some(info),
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
            cast: None,
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

    pub fn flush(&self) -> Result<(), RemoteError> {
        self.shared.flush()
    }

    fn texture(&self, id: TexId, size: Size<i32, Buffer>, format: Option<Fourcc>) -> RemoteTexture {
        RemoteTexture(Arc::new(TexInner {
            id,
            size,
            format,
            shared: Arc::downgrade(&self.shared),
        }))
    }

    /// Paints a cached UI surface entirely in the GPU process, acknowledging completion
    /// before returning its remote texture handle. No UI pixels enter the core.
    pub fn render_ui(&mut self, scene: UiScene) -> Result<RemoteTexture, RemoteError> {
        let size = Size::from((scene.width as i32, scene.height as i32));
        let id = self.shared.alloc_id();
        self.flush()?;
        let event = self
            .client()
            .request(&Request::RenderUi { id, scene }, &[])
            .map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        GpuClient::expect_ack(event).map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        Ok(self.texture(id, size, Some(Fourcc::Abgr8888)))
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
    // Keeping the target borrowed ties its lifetime to the frame.
    target: &'frame mut RemoteTarget<'buffer>,
    size: Size<i32, Physical>,
    transform: Transform,
    scene: Option<SceneFrame>,
    /// The node being described, if any.
    node: Option<OpenNode>,
    /// Core-only draw context; every recorded texture carries its resolved intent.
    texture_options: TextureOptions,
    /// Ops outside any node; become an anonymous node.
    loose: Vec<Op>,
    anonymous: u64,
}

struct OpenNode {
    node: Node,
    /// Whether ops go to `draw` (after `begin_node_draw`) or `capture`.
    in_draw: bool,
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
    pub fn renderer(&mut self) -> &mut RemoteRenderer {
        self.renderer
    }

    pub fn target(&self) -> Target {
        self.target.target
    }

    fn push_op(&mut self, op: Op) {
        if let Some(open) = &mut self.node {
            if open.in_draw {
                open.node.draw.push(op);
            } else {
                open.node.capture.push(op);
            }
        } else {
            self.loose.push(op);
        }
    }

    /// Turns ops drawn outside any node into a one-off node covering them.
    fn flush_loose(&mut self) {
        if self.loose.is_empty() {
            return;
        }
        let ops = mem::take(&mut self.loose);
        let geometry = ops
            .iter()
            .filter_map(op_bounds)
            .reduce(|a, b| a.merge(b))
            .unwrap_or_default();
        self.anonymous += 1;
        self.scene.as_mut().unwrap().nodes.push(Node {
            id: ANONYMOUS_NODE | self.anonymous,
            src: convert::rect_f64(Rectangle::<f64, Buffer>::from_size(Size::from((
                geometry.size.w as f64,
                geometry.size.h as f64,
            )))),
            geometry: convert::rect(geometry),
            damage: None,
            opaque: Vec::new(),
            kind: super::protocol::ElementKind::Unspecified,
            transform: convert::transform(Transform::Normal),
            capture: Vec::new(),
            draw: ops,
        });
    }

    /// Starts a scene node; ops until `begin_node_draw` are its framebuffer capture, the rest
    /// until `end_node` its draw.
    pub fn begin_node(&mut self, node: Node) {
        if self.node.is_some() {
            warn!("begin_node inside a node");
            self.end_node();
        }
        self.flush_loose();
        self.node = Some(OpenNode {
            node,
            in_draw: false,
        });
    }

    pub fn begin_node_draw(&mut self) {
        match &mut self.node {
            Some(open) => open.in_draw = true,
            None => warn!("begin_node_draw outside a node"),
        }
    }

    pub fn end_node(&mut self) {
        match self.node.take() {
            Some(open) => self.scene.as_mut().unwrap().nodes.push(open.node),
            None => warn!("end_node outside a node"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_texture_from_to(
        &mut self,
        texture: &RemoteTexture,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        transform: Transform,
        alpha: f32,
        options: TextureOptions,
    ) -> Result<(), RemoteError> {
        // Opaque regions arrive dst-relative; the scene is in frame coordinates.
        let opaque: Vec<_> = opaque_regions
            .iter()
            .map(|r| {
                let mut r = *r;
                r.loc += dst.loc;
                r
            })
            .collect();
        self.push_op(Op::Texture {
            texture: texture.id(),
            src: convert::rect_f64(src),
            dst: convert::rect(dst),
            opaque: convert::rects(&opaque),
            transform: convert::transform(transform),
            alpha,
            options,
        });
        Ok(())
    }

    /// Supply explicit texture intent while a Smithay element records its draws.
    /// No stateful scope crosses the wire. Restoration also happens on a returned error.
    pub fn with_texture_options<T>(
        &mut self,
        options: TextureOptions,
        draw: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous = mem::replace(&mut self.texture_options, options);
        let result = draw(self);
        self.texture_options = previous;
        result
    }

    pub fn texture_options(&self) -> TextureOptions {
        self.texture_options
    }

    /// Target encoded blend space (`None` = SDR).
    pub fn blend(&self) -> Option<BlendParams> {
        self.renderer.frame_blend
    }

    pub fn draw_paint(
        &mut self,
        paint: Paint<RemoteTexture>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        alpha: f32,
    ) {
        self.push_op(Op::Paint {
            paint: paint.map_textures(|texture| texture.id()),
            src: convert::rect_f64(src),
            dst: convert::rect(dst),
            alpha,
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
        self.push_op(Op::Capture {
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
        _damage: &[Rectangle<i32, Physical>],
        postprocess: PostprocessParams,
    ) {
        self.push_op(Op::Captured {
            key: capture.key(),
            dst: convert::rect(dst),
            postprocess,
        });
    }

    /// Marks which history baseline the node damage in this frame refers to.
    pub fn set_generation(&mut self, generation: u64) {
        self.scene.as_mut().unwrap().generation = generation;
    }

    /// Closes everything still open and takes the finished scene.
    fn take_scene(&mut self) -> SceneFrame {
        if self.node.is_some() {
            warn!("frame finished inside a node");
            self.end_node();
        }
        self.flush_loose();
        let mut scene = self.scene.take().unwrap();
        scene.cast = self.target.cast;
        scene
    }
}

impl Drop for RemoteFrame<'_, '_> {
    fn drop(&mut self) {
        // Dropped without `finish` (an element failed): the frame never reaches the GPU.
        if self.scene.is_some() {
            self.renderer.shared.close_frame(None);
        }
    }
}

/// Frame-space bounds of an op's output, for anonymous nodes.
fn op_bounds(op: &Op) -> Option<Rectangle<i32, Physical>> {
    match op {
        Op::Solid { dst, .. }
        | Op::Texture { dst, .. }
        | Op::Paint { dst, .. }
        | Op::Captured { dst, .. } => Some(convert::to_rect(*dst)),
        Op::Capture { .. } => None,
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

    /// Offscreen frames are cleared whole before drawing; outputs and casts clear
    /// themselves. `at` is ignored: the GPU owns damage.
    fn clear(
        &mut self,
        color: Color32F,
        _at: &[Rectangle<i32, Physical>],
    ) -> Result<(), RemoteError> {
        self.scene.as_mut().unwrap().clear = Some(color.components());
        Ok(())
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), RemoteError> {
        self.push_op(Op::Solid {
            dst: convert::rect(dst),
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
            self.texture_options,
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

    fn finish(mut self) -> Result<SyncPoint, RemoteError> {
        let scene = self.take_scene();
        self.renderer.shared.close_frame(Some(scene));
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
        self.shared.open_frame();
        let scene = SceneFrame {
            target: framebuffer.target,
            width: output_size.w,
            height: output_size.h,
            transform: convert::transform(dst_transform),
            blend: self.frame_blend,
            clear: None,
            generation: 0,
            cast: None,
            nodes: Vec::new(),
        };
        Ok(RemoteFrame {
            renderer: self,
            target: framebuffer,
            size: output_size,
            transform: dst_transform,
            scene: Some(scene),
            node: None,
            texture_options: TextureOptions::default(),
            loose: Vec::new(),
            anonymous: 0,
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
            cast: None,
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
            cast: None,
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
