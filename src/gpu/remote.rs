//! A smithay renderer that records commands for the GPU process instead of touching GL.
//!
//! Every renderer call becomes a [`Command`]; they are batched and flushed on
//! [`RemoteRenderer::flush`], before any synchronous read, or when the batch grows large.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::format::{get_bpp, FormatSet};
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::renderer::gles::Uniform as GlesUniform;
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
    Caps, Command, DmabufDesc, PlaneDesc, ShaderKind, ShaderSupport, Target, TexId, TexProgram,
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
}

struct Shared {
    client: Mutex<GpuClient>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    context_id: ContextId<RemoteTexture>,
    caps: Caps,
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
            pending.commands.push(cmd);
            pending.fds.extend(fds);
            pending.bytes > MAX_PENDING_BYTES
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
        let caps = client.caps().clone();
        let shaders = caps.shaders;
        Self {
            shared: Arc::new(Shared {
                client: Mutex::new(client),
                pending: Mutex::new(Pending::default()),
                next_id: AtomicU64::new(1),
                context_id: ContextId::new(),
                caps,
                shaders: Mutex::new(shaders),
                dmabuf_cache: Mutex::new(HashMap::new()),
            }),
            debug_flags: DebugFlags::empty(),
        }
    }

    pub fn caps(&self) -> &Caps {
        &self.shared.caps
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
        let mut client = self.shared.client.lock().unwrap();
        let ok = client
            .set_custom_shader(kind, src)
            .map_err(|err| RemoteError::Gpu(format!("{err:#}")))?;
        let mut shaders = self.shared.shaders.lock().unwrap();
        match kind {
            ShaderKind::Resize => shaders.resize = ok || self.shared.caps.shaders.resize,
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
}

impl Frame for RemoteFrame<'_, '_> {
    type Error = RemoteError;
    type TextureId = RemoteTexture;

    fn context_id(&self) -> ContextId<RemoteTexture> {
        self.renderer.shared.context_id.clone()
    }

    fn clear(&mut self, color: Color32F, at: &[Rectangle<i32, Physical>]) -> Result<(), RemoteError> {
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

impl Bind<Dmabuf> for RemoteRenderer {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<RemoteTarget<'a>, RemoteError> {
        let texture = self.import_dmabuf(target, None)?;
        Ok(RemoteTarget {
            target: Target::Texture(texture.id()),
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
        self.shared.push(Command::UpdateMemory {
            id: texture.id(),
            region: convert::rect(region),
            data: data.to_vec(),
        });
        Ok(())
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        let formats: Vec<Fourcc> = self
            .shared
            .caps
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

        let result = shm::with_buffer_contents(buffer, |ptr, len, data| {
            let fourcc = shm_format_to_fourcc(data.format)
                .ok_or(RemoteError::Unsupported("shm format"))?;
            if !self.shared.caps.mem_formats.contains(&(fourcc as u32)) {
                return Err(RemoteError::Unsupported("shm format"));
            }
            let bpp = get_bpp(fourcc).ok_or(RemoteError::Unsupported("shm format"))? / 8;

            let width = data.width;
            let height = data.height;
            let stride = usize::try_from(data.stride).map_err(|_| RemoteError::Shm("stride".into()))?;
            let offset = usize::try_from(data.offset).map_err(|_| RemoteError::Shm("offset".into()))?;
            if width <= 0 || height <= 0 {
                return Err(RemoteError::Shm("empty buffer".into()));
            }
            let row_len = width as usize * bpp;
            let end = offset + (height as usize - 1) * stride + row_len;
            if stride < row_len || end > len {
                return Err(RemoteError::Shm("buffer does not fit in its pool".into()));
            }

            let size = Size::<i32, Buffer>::from((width, height));
            let existing = cache
                .as_ref()
                .and_then(|cache| cache.lock().unwrap().get(&context_id).cloned())
                .filter(|tex| tex.size() == size && tex.format() == Some(fourcc));

            // SAFETY: bounds were checked against the pool length above; the pool is mapped
            // for the duration of the closure.
            let row = |y: usize, x: usize, w: usize| unsafe {
                std::slice::from_raw_parts(ptr.add(offset + y * stride + x * bpp), w * bpp)
            };

            if let Some(texture) = existing {
                let full = Rectangle::from_size(size);
                let regions: Vec<Rectangle<i32, Buffer>> = if damage.is_empty() {
                    vec![full]
                } else {
                    damage.iter().filter_map(|r| r.intersection(full)).collect()
                };
                for region in regions {
                    let mut bytes =
                        Vec::with_capacity(region.size.w as usize * region.size.h as usize * bpp);
                    for y in region.loc.y..region.loc.y + region.size.h {
                        bytes.extend_from_slice(row(
                            y as usize,
                            region.loc.x as usize,
                            region.size.w as usize,
                        ));
                    }
                    self.shared.push(Command::UpdateMemory {
                        id: texture.id(),
                        region: convert::rect(region),
                        data: bytes,
                    });
                }
                Ok(texture)
            } else {
                let mut bytes = Vec::with_capacity(row_len * height as usize);
                for y in 0..height as usize {
                    bytes.extend_from_slice(row(y, 0, width as usize));
                }
                let id = self.shared.alloc_id();
                self.shared.push(Command::ImportMemory {
                    id,
                    format: fourcc as u32,
                    width,
                    height,
                    flipped: false,
                    data: bytes,
                });
                let texture = self.texture(id, size, Some(fourcc));
                if let Some(cache) = &cache {
                    cache
                        .lock()
                        .unwrap()
                        .insert(context_id.clone(), texture.clone());
                }
                Ok(texture)
            }
        });
        result.map_err(|err| RemoteError::Shm(format!("{err:?}")))?
    }
}

impl ImportDma for RemoteRenderer {
    fn dmabuf_formats(&self) -> FormatSet {
        self.shared
            .caps
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

        let fds = dmabuf
            .handles()
            .map(|fd| fd.try_clone_to_owned())
            .collect::<Result<Vec<OwnedFd>, _>>()
            .map_err(|err| RemoteError::Transport(err.to_string()))?;
        let format = dmabuf.format();
        let desc = DmabufDesc {
            width: dmabuf.width(),
            height: dmabuf.height(),
            format: format.code as u32,
            modifier: u64::from(format.modifier),
            planes: dmabuf
                .offsets()
                .zip(dmabuf.strides())
                .map(|(offset, stride)| PlaneDesc { offset, stride })
                .collect(),
        };
        let id = self.shared.alloc_id();
        self.shared.push_with_fds(
            Command::ImportDmabuf {
                id,
                desc,
                damage: damage.map(convert::rects),
            },
            fds,
        );
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
            Target::Texture(id) => self.read(id, region, format),
            Target::Output(_) => Err(RemoteError::InvalidTarget),
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
