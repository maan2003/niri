//! Entry point of the GPU process: owns Mesa (and, in drm mode, KMS) and serves the core.

use std::collections::VecDeque;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};

use anyhow::Context as _;
use calloop::channel::Sender;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmEvent;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction,
};
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::reexports::gbm::Modifier;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::utils::Size;

#[cfg(feature = "xdp-gnome-screencast")]
use super::cast::{Casting, StartParams};
use super::client::Mode;
use super::cursor::CursorThemes;
use super::drm::DrmState;
use super::exec::Executor;
use super::gl::{resources, shaders};
#[cfg(feature = "xdp-gnome-screencast")]
use super::protocol::CastEvent;
use super::protocol::{Event, GpuEvent, Request, PROTOCOL_VERSION};
use super::transport::Channel;

pub fn new_surfaceless_renderer() -> anyhow::Result<GlesRenderer> {
    let mut renderer = unsafe {
        let display =
            EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
        let context = EGLContext::new(&display).context("error creating EGL context")?;
        GlesRenderer::new(context).context("error creating renderer")?
    };
    resources::init(&mut renderer);
    shaders::init(&mut renderer);
    Ok(renderer)
}

pub(super) struct Server {
    chan: Channel,
    exec: Executor,
    drm: DrmState,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub(super) casting: Casting,
    loop_handle: LoopHandle<'static, Server>,
    signal: LoopSignal,
    /// Fds to attach to the reply of the request being handled.
    reply_fds: Vec<OwnedFd>,
    cursors: CursorThemes,
    /// Events from worker threads (PNG encoding), forwarded to the core.
    bg: Sender<GpuEvent>,
}

pub fn run(fd: OwnedFd, mode: Mode) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<'static, Server> =
        EventLoop::try_new().context("error creating event loop")?;

    let renderer = match mode {
        Mode::Headless => Some(new_surfaceless_renderer()?),
        Mode::Drm => None,
    };
    let mut exec = Executor::new(renderer);
    let caps = if exec.has_renderer() {
        Some(exec.caps()?)
    } else {
        None
    };

    let mut chan = Channel::new(fd);
    chan.send(
        &Event::Ready {
            version: PROTOCOL_VERSION,
            caps,
        },
        &[],
    )?;

    // A dup of the socket for readiness polling; the Channel keeps the original.
    let poll_fd = chan.as_fd().try_clone_to_owned()?;
    let (bg, bg_rx) = calloop::channel::channel::<GpuEvent>();
    event_loop
        .handle()
        .insert_source(bg_rx, |event, _, server: &mut Server| {
            if let calloop::channel::Event::Msg(event) = event {
                server.notify(event);
            }
        })
        .map_err(|err| anyhow::anyhow!("error registering worker channel: {err}"))?;
    let mut server = Server {
        chan,
        exec,
        drm: DrmState::default(),
        #[cfg(feature = "xdp-gnome-screencast")]
        casting: Casting::new(event_loop.handle()),
        loop_handle: event_loop.handle(),
        signal: event_loop.get_signal(),
        reply_fds: Vec::new(),
        cursors: CursorThemes::default(),
        bg,
    };

    event_loop
        .handle()
        .insert_source(
            Generic::new(poll_fd, Interest::READ, CalloopMode::Level),
            |_, _, server: &mut Server| {
                // One request per wakeup; Level mode brings us back if more are queued.
                match server.handle_one_request() {
                    Ok(true) => Ok(PostAction::Continue),
                    Ok(false) => {
                        server.signal.stop();
                        Ok(PostAction::Remove)
                    }
                    Err(err) => {
                        error!("gpu process: {err:#}");
                        server.signal.stop();
                        Ok(PostAction::Remove)
                    }
                }
            },
        )
        .map_err(|err| anyhow::anyhow!("error registering socket: {err}"))?;

    event_loop.run(None, &mut server, |_| ())?;
    Ok(())
}

impl Server {
    pub(super) fn notify(&mut self, event: GpuEvent) {
        if let Err(err) = self.chan.send(&Event::Notify(event), &[]) {
            warn!("error sending event to core: {err}");
        }
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    pub(super) fn on_cast_event(&mut self, event: CastEvent) {
        if let CastEvent::PipeWireFatal = event {
            warn!("PipeWire connection failed; dropping all casts");
            self.casting.reset();
        }
        self.notify(GpuEvent::Cast(event));
    }

    /// Returns Ok(false) when the core went away.
    fn handle_one_request(&mut self) -> anyhow::Result<bool> {
        let (req, fds): (Request, Vec<OwnedFd>) = match self.chan.recv() {
            Ok(x) => x,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        let mut fds = VecDeque::from(fds);

        if let Request::Shutdown = req {
            self.chan.send(&Event::Done, &[])?;
            return Ok(false);
        }

        let oneway = req.is_oneway();
        let reply = self.dispatch(req, &mut fds).unwrap_or_else(|err| {
            warn!("gpu request failed: {err:#}");
            Event::Error {
                message: format!("{err:#}"),
            }
        });
        if oneway {
            if let Event::Error { message } = reply {
                self.notify(GpuEvent::Error { message });
            }
        } else {
            let fds = std::mem::take(&mut self.reply_fds);
            let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(|fd| fd.as_fd()).collect();
            self.chan.send(&reply, &borrowed)?;
        }
        Ok(true)
    }

    fn dispatch(&mut self, req: Request, fds: &mut VecDeque<OwnedFd>) -> anyhow::Result<Event> {
        let exec = &mut self.exec;
        let drm = &mut self.drm;
        Ok(match req {
            Request::Execute { commands } => {
                let res = exec.execute(commands, fds);
                // Cast frames found in the batch render even if a later command failed.
                let deferred = std::mem::take(&mut exec.deferred);
                #[cfg(feature = "xdp-gnome-screencast")]
                self.casting.handle_deferred(deferred, exec);
                #[cfg(not(feature = "xdp-gnome-screencast"))]
                drop(deferred);
                res?;
                Event::Ack
            }
            Request::ImportDmabuf { id, desc } => {
                exec.import_dmabuf(id, &desc, fds)?;
                Event::Ack
            }
            Request::ReadTexture { id, region, format } => {
                Event::Image(exec.read_texture(id, region, format)?)
            }
            Request::LoadCursor {
                theme,
                names,
                size,
                fallback,
                first_id,
            } => {
                let images = self.cursors.load(&theme, &names, size, fallback)?;
                Event::Cursor {
                    frames: exec.import_cursor(&images, first_id)?,
                }
            }
            Request::EncodePng { token, id, region } => {
                let image = match exec.read_texture(id, region, Fourcc::Abgr8888 as u32) {
                    Ok(image) => image,
                    Err(err) => {
                        // The core is waiting for this token either way.
                        self.notify(GpuEvent::Png { token, data: None });
                        return Err(err);
                    }
                };
                let tx = self.bg.clone();
                // Encoding is slow; keep the loop free for frames.
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let res = write_png_rgba8(
                        std::io::Cursor::new(&mut buf),
                        image.width,
                        image.height,
                        &image.data,
                    );
                    let data = match res {
                        Ok(()) => Some(buf),
                        Err(err) => {
                            warn!("error encoding PNG: {err:?}");
                            None
                        }
                    };
                    let _ = tx.send(GpuEvent::Png { token, data });
                });
                Event::Ack
            }
            // Requests are handled in order, and Execute runs to completion (dmabuf targets
            // wait for their fence), so reaching this point is the guarantee.
            Request::Sync => Event::Ack,
            #[cfg(feature = "xdp-gnome-screencast")]
            Request::CastStart {
                stream,
                width,
                height,
                refresh,
                alpha,
                cursor_mode,
                allow_dmabuf,
                force_invalid_modifier,
            } => {
                let gbm = if allow_dmabuf {
                    drm.renderer_gbm()
                } else {
                    None
                };
                let mut formats = FormatSet::default();
                if gbm.is_some() {
                    formats = exec
                        .renderer()?
                        .egl_context()
                        .dmabuf_render_formats()
                        .clone();
                    if force_invalid_modifier {
                        formats = formats
                            .into_iter()
                            .filter(|f| f.modifier == Modifier::Invalid)
                            .collect();
                    }
                }
                let cursor_mode = self.casting.start(StartParams {
                    stream,
                    size: Size::from((width, height)),
                    refresh,
                    alpha,
                    cursor_mode,
                    formats,
                    gbm,
                })?;
                Event::CastStarted { cursor_mode }
            }
            #[cfg(feature = "xdp-gnome-screencast")]
            Request::CastConfigure {
                stream,
                width,
                height,
                refresh,
            } => {
                self.casting
                    .configure(stream, Size::from((width, height)), refresh)?;
                Event::Ack
            }
            #[cfg(feature = "xdp-gnome-screencast")]
            Request::CastClear {
                stream,
                target_time_ns,
            } => {
                self.casting.clear(stream, target_time_ns, exec)?;
                Event::Ack
            }
            #[cfg(feature = "xdp-gnome-screencast")]
            Request::CastStop { stream } => {
                self.casting.stop(stream);
                Event::Ack
            }
            #[cfg(not(feature = "xdp-gnome-screencast"))]
            Request::CastStart { .. }
            | Request::CastConfigure { .. }
            | Request::CastClear { .. }
            | Request::CastStop { .. } => {
                anyhow::bail!("built without screencast support")
            }
            Request::SetCustomShader { kind, src } => Event::ShaderSet {
                available: exec.set_custom_shader(kind, src.as_deref())?,
            },

            Request::AddDevice {
                dev,
                path,
                render_node_hint,
            } => {
                let fd = fds.pop_front().context("AddDevice needs the device fd")?;
                debug!("adding DRM device {dev} ({path}), render node hint: {render_node_hint:?}");
                let loop_handle = self.loop_handle.clone();
                let added = drm.add_device(exec, fd, dev, render_node_hint, |notifier, dev| {
                    loop_handle
                        .insert_source(
                            notifier,
                            move |event, meta, server: &mut Server| match event {
                                DrmEvent::VBlank(crtc) => {
                                    let meta = meta.expect("VBlank events must have metadata");
                                    if let Some(event) = server.drm.on_vblank(dev, crtc, meta) {
                                        if let Err(err) = server.chan.send(&event, &[]) {
                                            warn!("error sending vblank to core: {err}");
                                        }
                                    }
                                }
                                DrmEvent::Error(error) => {
                                    warn!("DRM error: {error}");
                                    server.notify(GpuEvent::DeviceError {
                                        dev,
                                        message: error.to_string(),
                                    });
                                }
                            },
                        )
                        .map_err(|err| anyhow::anyhow!("error registering DRM notifier: {err}"))
                })?;
                let caps = if added.render_node.is_some() {
                    Some(exec.caps()?)
                } else {
                    None
                };
                Event::DeviceAdded {
                    render_node: added.render_node,
                    caps,
                }
            }
            Request::RemoveDevice { dev } => {
                let mut renderer_dropped = false;
                if let Some((token, was_renderer)) = drm.remove_device(dev) {
                    self.loop_handle.remove(token);
                    if was_renderer {
                        // The renderer's EGL display belongs to this device; the next device
                        // that qualifies creates a fresh one and reports caps again.
                        exec.clear_renderer();
                        renderer_dropped = true;
                    }
                }
                Event::DeviceRemoved { renderer_dropped }
            }
            Request::PauseDevices => {
                drm.pause();
                Event::Ack
            }
            Request::ResumeDevices { force_disable } => {
                drm.resume(force_disable);
                Event::Ack
            }
            Request::RescanDevice { dev } => drm.rescan(dev)?,
            Request::CleanupDevice { dev, off } => {
                drm.cleanup(dev, &off)?;
                Event::Ack
            }
            Request::EnableOutput {
                output,
                connector,
                mode,
                vrr,
                color,
                clear,
                prefer_10bit,
            } => drm.enable_output(
                exec,
                output,
                connector,
                &mode,
                vrr,
                color,
                clear,
                prefer_10bit,
            )?,
            Request::DisableOutput { output } => {
                drm.disable_output(output)?;
                Event::Ack
            }
            Request::SetMode { output, mode } => drm.set_mode(output, &mode)?,
            Request::SetVrr { output, enable } => drm.set_vrr(output, enable)?,
            Request::SetColorState { output, state } => drm.set_color_state(output, state)?,
            Request::SetCtm { output, matrix } => {
                drm.set_ctm(output, matrix)?;
                Event::Ack
            }
            Request::SetOutputGeometry { output, geometry } => {
                drm.set_geometry(output, geometry)?;
                Event::Ack
            }
            Request::SetGamma { output, ramp } => {
                drm.set_gamma(output, ramp.as_deref())?;
                Event::Ack
            }
            Request::ClearOutputs => {
                drm.clear_outputs();
                Event::Ack
            }
            Request::SetDebugTint { enable } => {
                drm.set_debug_tint(enable);
                Event::Ack
            }
            Request::AllocateDmabuf {
                width,
                height,
                format,
                modifiers,
            } => {
                let dmabuf = drm.allocate_dmabuf(width, height, format, &modifiers)?;
                let (desc, fds) = super::exec::describe_dmabuf(&dmabuf)?;
                self.reply_fds = fds;
                Event::Dmabuf(desc)
            }
            Request::Present {
                output,
                frame,
                flags,
            } => {
                let (submitted, states) = drm.present(exec, output, frame, flags)?;
                let event = Event::Notify(GpuEvent::Presented {
                    output,
                    frame,
                    submitted,
                    states,
                });
                if let Err(err) = self.chan.send(&event, &[]) {
                    warn!("error sending event to core: {err}");
                }
                Event::Ack
            }
            Request::Shutdown => unreachable!(),
        })
    }
}

fn write_png_rgba8(
    w: impl std::io::Write,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)
}
