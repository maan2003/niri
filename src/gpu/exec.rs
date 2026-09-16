//! GPU-process side: replays recorded commands on a real `GlesRenderer`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;

use anyhow::{anyhow, bail, ensure, Context as _};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::format::get_bpp;
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::backend::renderer::element::memory::MemoryBuffer;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::{
    Bind as _, Color32F, DebugFlags, ExportMem as _, Frame as _, FrameContext as _, ImportDma as _,
    ImportMem as _, Offscreen as _, Renderer as _, Texture as _,
};
use smithay::utils::{Buffer, Physical, Rectangle, Size};
use tracing::warn;

use super::convert;
use super::gl::blend;
use super::gl::blur::Blur;
use super::gl::capture::Capture;
use super::gl::resources::Resources;
use super::gl::shader::{self, DrawParams};
use super::gl::shaders::Shaders;
use super::protocol::{
    BlendParams, Caps, Command, CursorFrameDesc, CursorMeta, DmabufDesc, Image, OutputRef,
    PlaneDesc, Rect, ShaderKind, ShaderSupport, Target, TexId, TexProgram,
};

/// Objects the core refers to by id.
#[derive(Default)]
pub struct Tables {
    pub textures: HashMap<TexId, GlesTexture>,
    /// Dmabufs behind imported textures, for direct scanout and for binding as render targets.
    pub dmabufs: HashMap<TexId, Dmabuf>,
    /// CPU copies of small memory/shm textures (cursor images), so the DRM compositor can put
    /// them on the cursor plane without going through GL.
    pub memory: HashMap<TexId, MemoryBuffer>,
    pools: ShmPools,
    captures: HashMap<u64, Capture>,
    blurs: HashMap<u64, Blur>,
}

/// Textures up to this many pixels keep a CPU copy in `Tables::memory`.
const STAGING_MAX_PIXELS: i32 = 512 * 512;

fn keep_staging(size: Size<i32, Buffer>) -> bool {
    size.w * size.h <= STAGING_MAX_PIXELS
}

/// Copies `region` into `mem` from `src`, which points at the region's first pixel in memory
/// laid out with `src_stride` bytes per row. `region` must lie within `mem`.
///
/// # Safety
/// `src` must be readable for `(region.h - 1) * src_stride + region.w * bpp` bytes.
unsafe fn patch_memory(
    mem: &mut MemoryBuffer,
    region: Rectangle<i32, Buffer>,
    src: *const u8,
    src_stride: usize,
) {
    let bpp = get_bpp(mem.format()).unwrap_or(32) / 8;
    let Some(region) = region.intersection(Rectangle::from_size(mem.size())) else {
        return;
    };
    let row_len = region.size.w as usize * bpp;
    let stride = mem.stride() as usize;
    let bytes = mem.as_mut_slice();
    for (i, y) in (region.loc.y..region.loc.y + region.size.h).enumerate() {
        let start = y as usize * stride + region.loc.x as usize * bpp;
        std::ptr::copy_nonoverlapping(
            src.add(i * src_stride),
            bytes[start..start + row_len].as_mut_ptr(),
            row_len,
        );
    }
}

/// Mappings of client shm pools that are sealed against shrinking. A mapping of such a pool
/// can never fault, so pixels are uploaded straight from it (one mmap per pool, not per commit).
#[derive(Default)]
pub struct ShmPools {
    pools: HashMap<(u64, u64), PoolMap>,
    tick: u64,
}

struct PoolMap {
    map: memmap2::MmapRaw,
    last_used: u64,
}

const MAX_POOL_MAPS: usize = 256;

impl ShmPools {
    /// Base pointer of a read-only mapping covering at least `len` bytes of the pool behind
    /// `file`, or `None` if the pool may shrink (caller falls back to `pread`).
    fn map_sealed(&mut self, file: &File, len: usize) -> Option<*const u8> {
        // SAFETY: plain fcntl on a valid fd.
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
            return None;
        }
        let stat = rustix::fs::fstat(file).ok()?;
        let file_len = usize::try_from(stat.st_size).ok()?;
        if len > file_len {
            return None;
        }
        let key = (stat.st_dev as u64, stat.st_ino as u64);
        self.tick += 1;
        if let Some(pool) = self.pools.get_mut(&key) {
            if pool.map.len() >= len {
                pool.last_used = self.tick;
                return Some(pool.map.as_ptr());
            }
            // The pool grew (allowed by F_SEAL_SHRINK); map it again at the new size.
            self.pools.remove(&key);
        }
        let map = memmap2::MmapOptions::new()
            .len(file_len)
            .map_raw_read_only(file)
            .ok()?;
        if self.pools.len() >= MAX_POOL_MAPS {
            if let Some(oldest) = self
                .pools
                .iter()
                .min_by_key(|(_, p)| p.last_used)
                .map(|(k, _)| *k)
            {
                self.pools.remove(&oldest);
            }
        }
        let ptr = map.as_ptr();
        self.pools.insert(
            key,
            PoolMap {
                map,
                last_used: self.tick,
            },
        );
        Some(ptr)
    }
}

/// Blur pyramids are cheap to recreate; cap how many we keep for closed windows.
const MAX_BLUR_CACHE: usize = 64;

pub struct Executor {
    pub(super) renderer: Option<GlesRenderer>,
    pub tables: RefCell<Tables>,
    /// Frames recorded for outputs, waiting for `Present`.
    pub output_frames: HashMap<OutputRef, OutputFrame>,
    /// Screencast work found in the last `execute`, in order; the server hands it to the
    /// PipeWire side after the batch.
    pub deferred: Vec<Deferred>,
}

/// Commands whose effect lives outside the executor (screencast streams).
pub enum Deferred {
    CastInfo {
        stream: u64,
        scale: f64,
        target_time_ns: u64,
        cursor: Option<CursorMeta>,
    },
    CastCursor {
        stream: u64,
        size: Size<i32, Physical>,
        commands: Vec<Command>,
    },
    CastFrame {
        stream: u64,
        size: Size<i32, Physical>,
        commands: Vec<Command>,
    },
}

#[derive(Default)]
/// A recorded output frame, replayed by the DRM compositor at `Present`.
pub struct OutputFrame {
    pub blend: Option<BlendParams>,
    pub commands: Vec<Command>,
}

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

fn fourcc(format: u32) -> anyhow::Result<Fourcc> {
    Fourcc::try_from(format).map_err(|_| anyhow!("unknown fourcc {format:#x}"))
}

/// The wire description of a dmabuf plus duplicated plane fds to attach.
pub fn describe_dmabuf(dmabuf: &Dmabuf) -> anyhow::Result<(DmabufDesc, Vec<OwnedFd>)> {
    let fds = dmabuf
        .handles()
        .map(|fd| fd.try_clone_to_owned())
        .collect::<Result<Vec<OwnedFd>, _>>()
        .context("duplicating dmabuf fds")?;
    let format = dmabuf.format();
    let desc = DmabufDesc {
        width: dmabuf.width(),
        height: dmabuf.height(),
        format: format.code as u32,
        modifier: u64::from(format.modifier),
        flags: dmabuf.flags().bits(),
        planes: dmabuf
            .offsets()
            .zip(dmabuf.strides())
            .map(|(offset, stride)| PlaneDesc { offset, stride })
            .collect(),
    };
    Ok((desc, fds))
}

pub fn build_dmabuf(desc: &DmabufDesc, fds: &mut VecDeque<OwnedFd>) -> anyhow::Result<Dmabuf> {
    if desc.planes.is_empty() || fds.len() < desc.planes.len() {
        bail!("ImportDmabuf expects one fd per plane");
    }
    let mut builder = Dmabuf::builder(
        (desc.width as i32, desc.height as i32),
        fourcc(desc.format)?,
        Modifier::from(desc.modifier),
        DmabufFlags::from_bits_retain(desc.flags),
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
            deferred: Vec::new(),
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
            texture_hdr: s.texture_hdr.is_some(),
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
        let mut tables = self.tables.borrow_mut();
        tables.textures.insert(id, texture);
        tables.dmabufs.insert(id, dmabuf);
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

    /// Uploads Xcursor frames as Argb8888 textures `first_id..`, keeping CPU copies so the
    /// cursor plane can take them.
    pub fn import_cursor(
        &mut self,
        images: &[xcursor::parser::Image],
        first_id: TexId,
    ) -> anyhow::Result<Vec<CursorFrameDesc>> {
        let renderer = self.renderer.as_mut().context("no renderer yet")?;
        let mut tables = self.tables.borrow_mut();
        let mut frames = Vec::with_capacity(images.len());
        for (i, img) in images.iter().enumerate() {
            let size: Size<i32, Buffer> = Size::from((img.width as i32, img.height as i32));
            ensure!(
                img.pixels_rgba.len() == img.width as usize * img.height as usize * 4,
                "cursor frame has the wrong data size"
            );
            let id = first_id + i as u64;
            let texture = renderer
                .import_memory(&img.pixels_rgba, Fourcc::Argb8888, size, false)
                .context("import_memory")?;
            if keep_staging(size) {
                tables.memory.insert(
                    id,
                    MemoryBuffer::from_slice(&img.pixels_rgba, Fourcc::Argb8888, size),
                );
            }
            tables.textures.insert(id, texture);
            frames.push(CursorFrameDesc {
                width: img.width,
                height: img.height,
                xhot: img.xhot,
                yhot: img.yhot,
                delay: img.delay,
            });
        }
        Ok(frames)
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
                    blend,
                    ..
                } => {
                    // Kept until Present; drawn by the DRM compositor with real damage.
                    let commands = collect_frame(renderer, &self.tables, &mut iter, fds)?;
                    self.output_frames
                        .insert(output, OutputFrame { blend, commands });
                }
                Command::Begin {
                    target: Target::Cast(stream),
                    width,
                    height,
                    ..
                } => {
                    let commands = collect_frame(renderer, &self.tables, &mut iter, fds)?;
                    self.deferred.push(Deferred::CastFrame {
                        stream,
                        size: Size::from((width, height)),
                        commands,
                    });
                }
                Command::Begin {
                    target: Target::CastCursor(stream),
                    width,
                    height,
                    ..
                } => {
                    let commands = collect_frame(renderer, &self.tables, &mut iter, fds)?;
                    self.deferred.push(Deferred::CastCursor {
                        stream,
                        size: Size::from((width, height)),
                        commands,
                    });
                }
                Command::CastFrameInfo {
                    stream,
                    scale,
                    target_time_ns,
                    cursor,
                } => self.deferred.push(Deferred::CastInfo {
                    stream,
                    scale,
                    target_time_ns,
                    cursor,
                }),
                Command::Begin {
                    target: Target::Texture(target),
                    width,
                    height,
                    transform,
                    blend,
                } => {
                    let mut texture = self
                        .tables
                        .borrow()
                        .textures
                        .get(&target)
                        .context("unknown target texture")?
                        .clone();
                    blend::apply(renderer, blend);
                    let res = (|| {
                        let mut fb = renderer.bind(&mut texture).context("bind")?;
                        let mut frame = renderer
                            .render(
                                &mut fb,
                                Size::from((width, height)),
                                convert::to_transform(transform),
                            )
                            .context("render")?;
                        let res = run_frame(&mut frame, &self.tables, &mut iter, None, fds);
                        let _sync = frame.finish().context("finish")?;
                        res
                    })();
                    if blend.is_some() {
                        blend::apply(renderer, None);
                    }
                    res?;
                }
                Command::Begin {
                    target: Target::Dmabuf(target),
                    width,
                    height,
                    transform,
                    blend,
                } => {
                    // Bind the dmabuf itself rather than its imported texture: external
                    // (EGLImage) textures can't be framebuffer attachments.
                    let mut dmabuf = self
                        .tables
                        .borrow()
                        .dmabufs
                        .get(&target)
                        .context("unknown target dmabuf")?
                        .clone();
                    blend::apply(renderer, blend);
                    let res = (|| {
                        let mut fb = renderer.bind(&mut dmabuf).context("bind dmabuf")?;
                        let mut frame = renderer
                            .render(
                                &mut fb,
                                Size::from((width, height)),
                                convert::to_transform(transform),
                            )
                            .context("render")?;
                        let res = run_frame(&mut frame, &self.tables, &mut iter, None, fds);
                        let sync = frame.finish().context("finish")?;
                        res.map(|()| sync)
                    })();
                    if blend.is_some() {
                        blend::apply(renderer, None);
                    }
                    let sync = res?;
                    // The buffer leaves for another process (PipeWire consumer, image-copy
                    // client) as soon as the core's Sync returns, so finish it here.
                    if let Err(err) = sync.wait() {
                        warn!("error waiting for dmabuf render: {err:?}");
                    }
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

/// Gathers a recorded frame up to its `End` for later replay. Fd-bearing imports must consume
/// their fds from this batch now, so they run immediately.
fn collect_frame(
    renderer: &mut GlesRenderer,
    tables: &RefCell<Tables>,
    iter: &mut impl Iterator<Item = Command>,
    fds: &mut VecDeque<OwnedFd>,
) -> anyhow::Result<Vec<Command>> {
    let mut frame = Vec::new();
    loop {
        match iter.next() {
            None => bail!("unterminated frame"),
            Some(Command::End) => break,
            Some(cmd @ (Command::ImportShm { .. } | Command::ImportDmabuf { .. })) => {
                let mut tables = tables.borrow_mut();
                execute_one(renderer, &mut tables, cmd, fds)?;
            }
            Some(cmd) => frame.push(cmd),
        }
    }
    Ok(frame)
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
            let fourcc = fourcc(format)?;
            let size = Size::from((width, height));
            let texture = renderer
                .import_memory(&data, fourcc, size, flipped)
                .context("import_memory")?;
            if !flipped && keep_staging(size) && get_bpp(fourcc).is_some() {
                tables
                    .memory
                    .insert(id, MemoryBuffer::from_slice(&data, fourcc, size));
            }
            tables.textures.insert(id, texture);
        }
        Command::UpdateMemory { id, region, data } => {
            let texture = tables.textures.get(&id).context("unknown texture")?;
            let region: Rectangle<i32, Buffer> = convert::to_rect(region);
            let bpp = texture
                .format()
                .and_then(get_bpp)
                .context("texture without a memory format")?
                / 8;
            ensure!(
                region.size.w > 0 && region.size.h > 0,
                "empty update region"
            );
            let row_len = region.size.w as usize * bpp;
            ensure!(
                data.len() >= row_len * region.size.h as usize,
                "update data too short"
            );
            // SAFETY: `data` holds `region.h` packed rows of `row_len` bytes (checked above).
            unsafe {
                renderer
                    .update_memory_strided(texture, data.as_ptr(), row_len as i32, region)
                    .context("update_memory")?;
                if let Some(mem) = tables.memory.get_mut(&id) {
                    patch_memory(mem, region, data.as_ptr(), row_len);
                }
            }
        }
        Command::ImportShm {
            id,
            format,
            width,
            height,
            stride,
            offset,
            damage,
        } => {
            let fd = fds.pop_front().context("ImportShm needs the pool fd")?;
            let fourcc = fourcc(format)?;
            let bpp = get_bpp(fourcc).context("shm format without bpp")? / 8;
            ensure!(width > 0 && height > 0, "empty shm buffer");
            ensure!(
                offset >= 0 && stride >= width * bpp as i32 && stride % bpp as i32 == 0,
                "bad shm buffer layout"
            );
            let file = File::from(fd);
            let size = Size::<i32, Buffer>::from((width, height));
            let full = Rectangle::from_size(size);
            let (stride_u, offset_u) = (stride as usize, offset as usize);
            let end = offset_u + (height as usize - 1) * stride_u + width as usize * bpp;
            let pixel_at = |region: Rectangle<i32, Buffer>| -> usize {
                offset_u + region.loc.y as usize * stride_u + region.loc.x as usize * bpp
            };

            let regions: Vec<Rectangle<i32, Buffer>> = match &damage {
                None => vec![full],
                Some(damage) => damage
                    .iter()
                    .filter_map(|r| convert::to_rect::<Buffer>(*r).intersection(full))
                    .collect(),
            };

            if let Some(base) = tables.pools.map_sealed(&file, end) {
                // SAFETY: the pool is sealed against shrinking and the mapping covers `end`
                // bytes, so every read below stays inside memory that cannot fault. The
                // client may write concurrently; we only ever copy bytes out.
                unsafe {
                    if damage.is_none() {
                        let texture = renderer
                            .import_memory_strided(base.add(offset_u), stride, fourcc, size, false)
                            .context("import_memory")?;
                        if keep_staging(size) {
                            let mut mem = MemoryBuffer::new(fourcc, size);
                            patch_memory(&mut mem, full, base.add(offset_u), stride_u);
                            tables.memory.insert(id, mem);
                        }
                        tables.textures.insert(id, texture);
                    } else {
                        let texture = tables
                            .textures
                            .get(&id)
                            .context("unknown shm texture")?
                            .clone();
                        for region in regions {
                            let src = base.add(pixel_at(region));
                            renderer
                                .update_memory_strided(&texture, src, stride, region)
                                .context("update_memory")?;
                            if let Some(mem) = tables.memory.get_mut(&id) {
                                patch_memory(mem, region, src, stride_u);
                            }
                        }
                    }
                }
            } else {
                // Unsealed pool (legacy shm_open clients): the client could shrink it under a
                // mapping, so copy with pread instead, which fails cleanly. A short pool yields
                // zeros for the missing rows rather than failing the whole batch.
                let read_rows = |region: Rectangle<i32, Buffer>| -> Vec<u8> {
                    let row_len = region.size.w as usize * bpp;
                    let mut data = vec![0u8; row_len * region.size.h as usize];
                    for (i, y) in (region.loc.y..region.loc.y + region.size.h).enumerate() {
                        let pos = offset_u as u64
                            + y as u64 * stride_u as u64
                            + region.loc.x as u64 * bpp as u64;
                        if let Err(err) =
                            file.read_exact_at(&mut data[i * row_len..(i + 1) * row_len], pos)
                        {
                            warn!("error reading shm pool at row {y}: {err}");
                            break;
                        }
                    }
                    data
                };
                if damage.is_none() {
                    let data = read_rows(full);
                    let texture = renderer
                        .import_memory(&data, fourcc, size, false)
                        .context("import_memory")?;
                    if keep_staging(size) {
                        tables
                            .memory
                            .insert(id, MemoryBuffer::from_slice(&data, fourcc, size));
                    }
                    tables.textures.insert(id, texture);
                } else {
                    let texture = tables
                        .textures
                        .get(&id)
                        .context("unknown shm texture")?
                        .clone();
                    for region in regions {
                        let data = read_rows(region);
                        let row_len = region.size.w as usize * bpp;
                        // SAFETY: `data` holds `region.h` packed rows of `row_len` bytes.
                        unsafe {
                            renderer
                                .update_memory_strided(
                                    &texture,
                                    data.as_ptr(),
                                    row_len as i32,
                                    region,
                                )
                                .context("update_memory")?;
                            if let Some(mem) = tables.memory.get_mut(&id) {
                                patch_memory(mem, region, data.as_ptr(), row_len);
                            }
                        }
                    }
                }
            }
        }
        Command::ImportDmabuf { id, desc, damage } => {
            let dmabuf = build_dmabuf(&desc, fds)?;
            let damage = damage.map(|d| convert::to_rects::<Buffer>(&d));
            let texture = renderer
                .import_dmabuf(&dmabuf, damage.as_deref())
                .context("import_dmabuf")?;
            tables.textures.insert(id, texture);
            tables.dmabufs.insert(id, dmabuf);
        }
        Command::DestroyTexture { id } => {
            tables.textures.remove(&id);
            tables.dmabufs.remove(&id);
            tables.memory.remove(&id);
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
    fds: &mut VecDeque<OwnedFd>,
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
    // Overrides replaced by OverrideTexProgram / SuspendTexProgramOverride, restored by the
    // matching Clear / Restore, so the frame-wide blend override survives element overrides.
    let mut override_stack = Vec::new();

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
                override_stack.push(frame.take_tex_program_override());
                if let Some(program) = programs.get(program) {
                    frame.override_default_tex_program(
                        program.clone(),
                        convert::to_uniforms(uniforms),
                    );
                }
            }
            Command::ClearTexProgramOverride | Command::RestoreTexProgramOverride => {
                frame.set_tex_program_override(override_stack.pop().flatten());
            }
            Command::SuspendTexProgramOverride => {
                override_stack.push(frame.take_tex_program_override());
            }
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
                execute_one(guard.as_mut(), &mut tables, cmd, fds)?;
            }
        }
    }
}
