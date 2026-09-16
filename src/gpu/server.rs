//! Entry point of the GPU process: owns Mesa (and, in drm mode, KMS) and serves the core.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use anyhow::Context as _;
use smithay::backend::drm::DrmEvent;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction,
};

use super::client::Mode;
use super::drm::DrmState;
use super::exec::Executor;
use super::gl::{resources, shaders};
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

struct Server {
    chan: Channel,
    exec: Executor,
    drm: DrmState,
    loop_handle: LoopHandle<'static, Server>,
    signal: LoopSignal,
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
    let mut server = Server {
        chan,
        exec,
        drm: DrmState::default(),
        loop_handle: event_loop.handle(),
        signal: event_loop.get_signal(),
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
    fn notify(&mut self, event: GpuEvent) {
        if let Err(err) = self.chan.send(&Event::Notify(event), &[]) {
            warn!("error sending event to core: {err}");
        }
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
            self.chan.send(&reply, &[])?;
        }
        Ok(true)
    }

    fn dispatch(&mut self, req: Request, fds: &mut VecDeque<OwnedFd>) -> anyhow::Result<Event> {
        let exec = &mut self.exec;
        let drm = &mut self.drm;
        Ok(match req {
            Request::Execute { commands } => {
                exec.execute(commands, fds)?;
                Event::Ack
            }
            Request::ImportDmabuf { id, desc } => {
                exec.import_dmabuf(id, &desc, fds)?;
                Event::Ack
            }
            Request::ReadTexture { id, region, format } => {
                Event::Image(exec.read_texture(id, region, format)?)
            }
            Request::SetCustomShader { kind, src } => Event::ShaderSet {
                available: exec.set_custom_shader(kind, src.as_deref())?,
            },

            Request::AddDevice { dev, path, primary } => {
                let fd = fds.pop_front().context("AddDevice needs the device fd")?;
                debug!("adding DRM device {dev} ({path}), primary: {primary}");
                let loop_handle = self.loop_handle.clone();
                let added = drm.add_device(exec, fd, dev, primary, |notifier, dev| {
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
                let caps = if added.renderer_created {
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
                if let Some((token, was_primary)) = drm.remove_device(dev) {
                    self.loop_handle.remove(token);
                    if was_primary {
                        // The renderer's EGL display belongs to this device; a re-added
                        // primary creates a fresh one and reports caps again.
                        exec.clear_renderer();
                    }
                }
                Event::Ack
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
                max_bpc,
                clear,
                allow_10bit,
            } => drm.enable_output(
                exec,
                output,
                connector,
                &mode,
                vrr,
                max_bpc,
                clear,
                allow_10bit,
            )?,
            Request::DisableOutput { output } => {
                drm.disable_output(output)?;
                Event::Ack
            }
            Request::SetMode { output, mode } => drm.set_mode(output, &mode)?,
            Request::SetVrr { output, enable } => drm.set_vrr(output, enable)?,
            Request::SetMaxBpc { output, max_bpc } => drm.set_max_bpc(output, max_bpc)?,
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
            Request::Present { output, frame } => {
                let (submitted, states) = drm.present(exec, output, frame)?;
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
