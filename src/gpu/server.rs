//! GPU process side: owns the renderer, imports client buffers, renders scenes.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Color32F, ImportDma, ImportMem};
use smithay::utils::{
    Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

use super::protocol::{DmabufDesc, Event, Image, Request, ShmDesc, PROTOCOL_VERSION};
use super::scene::{self, BufferId, Node, NodeId, Scene};
use super::transport::Channel;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_vec, resources, shaders};

crate::niri_render_elements! {
    GpuRenderElement => {
        SolidColor = SolidColorRenderElement,
        Texture = PrimaryGpuTextureRenderElement,
    }
}

/// Entry point for `niri gpu-process`. Runs until the core closes the socket.
pub fn run(fd: OwnedFd) -> anyhow::Result<()> {
    let mut chan = Channel::new(fd);
    let mut server = GpuServer::new_surfaceless().context("creating renderer")?;
    chan.send(
        &Event::Ready {
            version: PROTOCOL_VERSION,
            renderer: server.renderer_name(),
        },
        &[],
    )?;

    loop {
        let (req, fds): (Request, Vec<OwnedFd>) = match chan.recv() {
            Ok(x) => x,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        if matches!(req, Request::Shutdown) {
            chan.send(&Event::Done, &[])?;
            return Ok(());
        }
        let event = match server.handle(req, fds) {
            Ok(event) => event,
            Err(err) => Event::Error {
                message: format!("{err:#}"),
            },
        };
        chan.send(&event, &[])?;
    }
}

struct ShmBuffer {
    map: memmap2::Mmap,
    desc: ShmDesc,
    fourcc: Fourcc,
    texture: GlesTexture,
}

impl ShmBuffer {
    fn bytes_per_pixel(fourcc: Fourcc) -> anyhow::Result<usize> {
        smithay::backend::allocator::format::get_bpp(fourcc)
            .map(|bpp| bpp / 8)
            .ok_or_else(|| anyhow!("unsupported shm format {fourcc:?}"))
    }

    /// `import_memory` wants tightly packed rows; clients may use any stride.
    fn packed(&self) -> anyhow::Result<Vec<u8>> {
        let bpp = Self::bytes_per_pixel(self.fourcc)?;
        let row = self.desc.width as usize * bpp;
        let stride = self.desc.stride as usize;
        let offset = self.desc.offset as usize;
        let mut out = Vec::with_capacity(row * self.desc.height as usize);
        for y in 0..self.desc.height as usize {
            let start = offset + y * stride;
            out.extend_from_slice(&self.map[start..start + row]);
        }
        Ok(out)
    }
}

struct DmaBuffer {
    _dmabuf: Dmabuf,
    texture: GlesTexture,
}

enum BufferEntry {
    Shm(ShmBuffer),
    Dma(DmaBuffer),
}

impl BufferEntry {
    fn texture(&self) -> &GlesTexture {
        match self {
            BufferEntry::Shm(b) => &b.texture,
            BufferEntry::Dma(b) => &b.texture,
        }
    }
}

pub struct GpuServer {
    renderer: GlesRenderer,
    buffers: HashMap<BufferId, BufferEntry>,
    node_ids: HashMap<NodeId, Id>,
}

impl GpuServer {
    /// Surfaceless EGL: works on llvmpipe with no device node at all.
    pub fn new_surfaceless() -> anyhow::Result<Self> {
        let mut renderer = unsafe {
            let display =
                EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
            let context = EGLContext::new(&display).context("error creating EGL context")?;
            GlesRenderer::new(context).context("error creating renderer")?
        };
        resources::init(&mut renderer);
        shaders::init(&mut renderer);
        Ok(Self {
            renderer,
            buffers: HashMap::new(),
            node_ids: HashMap::new(),
        })
    }

    pub fn renderer_name(&self) -> String {
        "gles/surfaceless".to_owned()
    }

    pub fn handle(&mut self, req: Request, fds: Vec<OwnedFd>) -> anyhow::Result<Event> {
        match req {
            Request::RegisterShm { id, desc } => {
                let [fd] = <[OwnedFd; 1]>::try_from(fds)
                    .map_err(|fds| anyhow!("RegisterShm expects 1 fd, got {}", fds.len()))?;
                self.register_shm(id, fd, desc)?;
                Ok(Event::Ack)
            }
            Request::RegisterDmabuf { id, desc } => {
                self.register_dmabuf(id, fds, desc)?;
                Ok(Event::Ack)
            }
            Request::UpdateShm { id, damage: _ } => {
                self.update_shm(id)?;
                Ok(Event::Ack)
            }
            Request::DestroyBuffer { id } => {
                self.buffers.remove(&id);
                Ok(Event::Ack)
            }
            Request::RenderToImage { scene, format } => {
                let fourcc = Fourcc::try_from(format)
                    .map_err(|_| anyhow!("unknown fourcc {format:#x}"))?;
                Ok(Event::Image(self.render_to_image(&scene, fourcc)?))
            }
            Request::Shutdown => Ok(Event::Done),
        }
    }

    fn register_shm(&mut self, id: BufferId, fd: OwnedFd, desc: ShmDesc) -> anyhow::Result<()> {
        if self.buffers.contains_key(&id) {
            bail!("buffer {id:?} already registered");
        }
        // The core seals pools before forwarding. Refuse anything else: an
        // unsealed pool can be truncated under us and SIGBUS this process.
        let seals = rustix::fs::fcntl_get_seals(fd.as_fd()).context("reading seals")?;
        if !seals.contains(rustix::fs::SealFlags::SHRINK) {
            bail!("shm pool is not sealed against shrinking");
        }
        let fourcc = Fourcc::try_from(desc.format)
            .map_err(|_| anyhow!("unknown fourcc {:#x}", desc.format))?;
        let bpp = ShmBuffer::bytes_per_pixel(fourcc)?;
        if desc.width <= 0 || desc.height <= 0 || desc.offset < 0 || desc.stride <= 0 {
            bail!("bad shm geometry {desc:?}");
        }
        let (w, h) = (desc.width as usize, desc.height as usize);
        if (desc.stride as usize) < w * bpp {
            bail!("shm stride too small for width");
        }
        let end = (desc.offset as usize)
            .checked_add((desc.stride as usize).checked_mul(h).context("overflow")?)
            .context("overflow")?;
        if end > desc.size {
            bail!("shm buffer exceeds pool size");
        }
        let file = File::from(fd);
        let real = file.metadata().context("fstat")?.len() as usize;
        if real < desc.size {
            bail!("shm pool file is smaller than declared");
        }
        let map = unsafe { memmap2::MmapOptions::new().len(desc.size).map(&file) }
            .context("mmap shm pool")?;

        let mut buffer = ShmBuffer {
            map,
            desc,
            fourcc,
            // Placeholder, replaced below once we have the packed pixels.
            texture: self
                .renderer
                .import_memory(&vec![0u8; w * h * bpp], fourcc, (w as i32, h as i32).into(), false)
                .context("import placeholder")?,
        };
        let packed = buffer.packed()?;
        buffer.texture = self
            .renderer
            .import_memory(&packed, fourcc, (w as i32, h as i32).into(), false)
            .context("import shm")?;
        self.buffers.insert(id, BufferEntry::Shm(buffer));
        Ok(())
    }

    fn update_shm(&mut self, id: BufferId) -> anyhow::Result<()> {
        let Some(BufferEntry::Shm(buffer)) = self.buffers.get(&id) else {
            bail!("UpdateShm on unknown or non-shm buffer {id:?}");
        };
        // TODO: honour the damage list instead of re-uploading everything.
        let packed = buffer.packed()?;
        let region = Rectangle::<i32, BufferCoord>::from_size(
            (buffer.desc.width, buffer.desc.height).into(),
        );
        self.renderer
            .update_memory(&buffer.texture, &packed, region)
            .context("update shm")?;
        Ok(())
    }

    fn register_dmabuf(
        &mut self,
        id: BufferId,
        fds: Vec<OwnedFd>,
        desc: DmabufDesc,
    ) -> anyhow::Result<()> {
        if self.buffers.contains_key(&id) {
            bail!("buffer {id:?} already registered");
        }
        if fds.len() != desc.planes.len() || fds.is_empty() {
            bail!("RegisterDmabuf expects one fd per plane");
        }
        let fourcc = Fourcc::try_from(desc.format)
            .map_err(|_| anyhow!("unknown fourcc {:#x}", desc.format))?;
        let mut builder = Dmabuf::builder(
            (desc.width, desc.height),
            fourcc,
            Modifier::from(desc.modifier),
            DmabufFlags::empty(),
        );
        for (fd, plane) in fds.into_iter().zip(&desc.planes) {
            if !builder.add_plane(Arc::new(fd), plane.offset, plane.stride) {
                bail!("too many dmabuf planes");
            }
        }
        let dmabuf = builder.build().context("empty dmabuf")?;
        let texture = self
            .renderer
            .import_dmabuf(&dmabuf, None)
            .context("import dmabuf")?;
        self.buffers.insert(
            id,
            BufferEntry::Dma(DmaBuffer {
                _dmabuf: dmabuf,
                texture,
            }),
        );
        Ok(())
    }

    fn node_id(&mut self, id: NodeId) -> Id {
        self.node_ids.entry(id).or_insert_with(Id::new).clone()
    }

    fn build_elements(&mut self, scene: &Scene) -> anyhow::Result<Vec<GpuRenderElement>> {
        let mut out = Vec::with_capacity(scene.nodes.len());
        for node in &scene.nodes {
            out.push(match node {
                Node::SolidColor {
                    id,
                    commit,
                    geometry,
                    color,
                } => {
                    let id = self.node_id(*id);
                    SolidColorRenderElement::new(
                        id,
                        rect_i32::<Physical>(geometry),
                        CommitCounter::from(*commit as usize),
                        Color32F::new(color[0], color[1], color[2], color[3]),
                        Kind::Unspecified,
                    )
                    .into()
                }
                Node::Surface {
                    // TODO: keep a stable smithay Id per node once frames are damage-tracked.
                    id: _,
                    commit: _,
                    buffer,
                    location,
                    buffer_scale,
                    transform,
                    alpha,
                    src,
                    size,
                    opaque,
                    kind,
                } => {
                    let texture = self
                        .buffers
                        .get(buffer)
                        .with_context(|| format!("scene references unknown buffer {buffer:?}"))?
                        .texture()
                        .clone();
                    let texture_buffer = TextureBuffer::from_texture(
                        &self.renderer,
                        texture,
                        *buffer_scale as f64,
                        transform_to_smithay(*transform),
                        opaque.iter().map(rect_i32::<BufferCoord>).collect(),
                    );
                    // niri's texture element takes a logical location.
                    let location = Point::<f64, Logical>::from((
                        location.0 / scene.scale,
                        location.1 / scene.scale,
                    ));
                    PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
                        texture_buffer,
                        location,
                        *alpha,
                        src.map(|r| rect_f64::<Logical>(&r)),
                        size.map(|s| Size::<f64, Logical>::from((s.0 as f64, s.1 as f64))),
                        kind_to_smithay(*kind),
                    ))
                    .into()
                }
            });
        }
        Ok(out)
    }

    fn render_to_image(&mut self, scene: &Scene, fourcc: Fourcc) -> anyhow::Result<Image> {
        let elements = self.build_elements(scene)?;
        let size = Size::<i32, Physical>::from(scene.size);
        // Scene is front-to-back; the painter's loop wants back-to-front.
        let pixels = render_to_vec(
            &mut self.renderer,
            size,
            Scale::from(scene.scale),
            transform_to_smithay(scene.transform),
            fourcc,
            elements.iter().rev(),
        )?;
        Ok(Image {
            width: size.w,
            height: size.h,
            format: fourcc as u32,
            pixels,
        })
    }
}

fn rect_i32<K>(r: &scene::Rect<i32>) -> Rectangle<i32, K> {
    Rectangle::new((r.x, r.y).into(), (r.w, r.h).into())
}

fn rect_f64<K>(r: &scene::Rect<f64>) -> Rectangle<f64, K> {
    Rectangle::new((r.x, r.y).into(), (r.w, r.h).into())
}

fn transform_to_smithay(t: scene::Transform) -> Transform {
    match t {
        scene::Transform::Normal => Transform::Normal,
        scene::Transform::Rotate90 => Transform::_90,
        scene::Transform::Rotate180 => Transform::_180,
        scene::Transform::Rotate270 => Transform::_270,
        scene::Transform::Flipped => Transform::Flipped,
        scene::Transform::Flipped90 => Transform::Flipped90,
        scene::Transform::Flipped180 => Transform::Flipped180,
        scene::Transform::Flipped270 => Transform::Flipped270,
    }
}

fn kind_to_smithay(k: scene::Kind) -> Kind {
    match k {
        scene::Kind::Unspecified => Kind::Unspecified,
        scene::Kind::Cursor => Kind::Cursor,
    }
}
