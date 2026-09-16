//! Entry point of the GPU process: owns Mesa and replays commands from the core.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use anyhow::Context as _;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;

use super::exec::Executor;
use super::protocol::{Event, Request, PROTOCOL_VERSION};
use super::transport::Channel;
use crate::render_helpers::{resources, shaders};

pub fn new_surfaceless_renderer() -> anyhow::Result<GlesRenderer> {
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
        let context = EGLContext::new(&display).context("error creating EGL context")?;
        GlesRenderer::new(context).context("error creating renderer")?
    };
    resources::init(&mut renderer);
    shaders::init(&mut renderer);
    Ok(renderer)
}

pub fn run(fd: OwnedFd) -> anyhow::Result<()> {
    let mut chan = Channel::new(fd);
    let renderer = new_surfaceless_renderer()?;
    let mut exec = Executor::new(renderer);
    chan.send(
        &Event::Ready {
            version: PROTOCOL_VERSION,
            caps: exec.caps(),
        },
        &[],
    )?;

    loop {
        let (req, fds): (Request, Vec<OwnedFd>) = match chan.recv() {
            Ok(x) => x,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let mut fds = VecDeque::from(fds);
        let reply = match req {
            Request::Execute { commands } => exec.execute(commands, &mut fds).map(|()| Event::Ack),
            Request::ReadTexture { id, region, format } => {
                exec.read_texture(id, region, format).map(Event::Image)
            }
            Request::SetCustomShader { kind, src } => {
                exec.set_custom_shader(kind, src.as_deref()).map(|()| Event::Ack)
            }
            Request::Shutdown => {
                chan.send(&Event::Done, &[])?;
                return Ok(());
            }
        };
        let event = reply.unwrap_or_else(|err| {
            warn!("gpu command failed: {err:#}");
            Event::Error {
                message: format!("{err:#}"),
            }
        });
        chan.send(&event, &[])?;
    }
}
