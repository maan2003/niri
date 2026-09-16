//! Core side of the GPU-process connection: spawning and request/reply plumbing.

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use anyhow::{anyhow, bail, Context};
use smithay::backend::allocator::dmabuf::Dmabuf;

use super::protocol::{
    self, Caps, CastCursorMode, CursorFrameDesc, DmabufDesc, Event, GpuEvent, Image, Rect, Request,
    ShaderKind, TexId, PROTOCOL_VERSION,
};
use super::transport::Channel;

pub const CHILD_SOCKET_FD: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Surfaceless EGL, no outputs. Tests and the headless backend.
    Headless,
    /// Waits for DRM devices from the core and scans out on them.
    Drm,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Headless => "headless",
            Mode::Drm => "drm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "headless" => Some(Mode::Headless),
            "drm" => Some(Mode::Drm),
            _ => None,
        }
    }
}

pub struct GpuClient {
    chan: Channel,
    child: Option<Child>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    caps: Option<Caps>,
    /// Unsolicited events that arrived while waiting for a reply.
    events: VecDeque<GpuEvent>,
    /// Called when an event is queued while a reply was awaited. The socket is no longer
    /// readable by then, so a level-triggered poll would not notice; this wakes the loop.
    waker: Option<Box<dyn Fn() + Send>>,
}

impl GpuClient {
    /// Spawns `exe gpu-process` with the socket on fd 3.
    pub fn spawn_process(exe: &Path, mode: Mode) -> anyhow::Result<Self> {
        let (ours, theirs) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .context("socketpair")?;

        let theirs_raw = theirs.as_raw_fd();
        let mut cmd = Command::new(exe);
        cmd.arg("gpu-process")
            .arg("--socket-fd")
            .arg(CHILD_SOCKET_FD.to_string())
            .arg("--mode")
            .arg(mode.as_str())
            .stdin(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(theirs_raw, CHILD_SOCKET_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("spawning gpu process")?;
        drop(theirs);

        let mut client = Self {
            chan: Channel::new(ours),
            child: Some(child),
            thread: None,
            caps: None,
            events: VecDeque::new(),
            waker: None,
        };
        client.handshake()?;
        Ok(client)
    }

    /// Runs the GPU server on a thread in this process. For tests and debugging only.
    pub fn spawn_thread(mode: Mode) -> anyhow::Result<Self> {
        let (ours, theirs) = Channel::pair()?;
        let thread = std::thread::Builder::new()
            .name("gpu-server".into())
            .spawn(move || super::server::run(theirs.into_fd(), mode))?;
        let mut client = Self {
            chan: ours,
            child: None,
            thread: Some(thread),
            caps: None,
            events: VecDeque::new(),
            waker: None,
        };
        client.handshake()?;
        Ok(client)
    }

    /// None until the GPU process has a renderer (drm mode: after the primary device).
    pub fn caps(&self) -> Option<&Caps> {
        self.caps.as_ref()
    }

    /// The socket, for registering with an event loop. Readable means an event is waiting.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.chan.as_fd()
    }

    fn handshake(&mut self) -> anyhow::Result<()> {
        let (event, _): (Event, _) = self.chan.recv().context("waiting for gpu process")?;
        match event {
            Event::Ready { version, caps } if version == PROTOCOL_VERSION => {
                self.caps = caps;
                Ok(())
            }
            Event::Ready { version, .. } => bail!("gpu process protocol version {version}"),
            other => bail!("unexpected first event from gpu process: {other:?}"),
        }
    }

    /// Reads one event; unsolicited ones are queued for [`take_events`](Self::take_events).
    fn recv_reply(&mut self) -> anyhow::Result<Event> {
        Ok(self.recv_reply_with_fds()?.0)
    }

    fn recv_reply_with_fds(&mut self) -> anyhow::Result<(Event, Vec<OwnedFd>)> {
        loop {
            let (event, fds): (Event, Vec<OwnedFd>) = self.chan.recv()?;
            match event {
                Event::Notify(ev) => {
                    self.events.push_back(ev);
                    if let Some(waker) = &self.waker {
                        waker();
                    }
                }
                other => return Ok((other, fds)),
            }
        }
    }

    pub fn set_waker(&mut self, waker: impl Fn() + Send + 'static) {
        self.waker = Some(Box::new(waker));
    }

    /// Whether a read would not block. Calloop readiness can be stale when an earlier callback
    /// in the same dispatch drained the socket through a synchronous request.
    pub fn is_readable(&self) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.chan.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is a valid, initialized pollfd array of length 1.
        let n = unsafe { libc::poll(&mut pfd, 1, 0) };
        n > 0 && pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
    }

    /// Reads one pending message without a request outstanding (the socket was readable).
    pub fn recv_event(&mut self) -> anyhow::Result<()> {
        let (event, _): (Event, _) = self.chan.recv()?;
        match event {
            Event::Notify(ev) => self.events.push_back(ev),
            other => bail!("unexpected unsolicited event from gpu process: {other:?}"),
        }
        Ok(())
    }

    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    pub fn take_events(&mut self) -> Vec<GpuEvent> {
        self.events.drain(..).collect()
    }

    pub fn request(&mut self, req: &Request, fds: &[BorrowedFd<'_>]) -> anyhow::Result<Event> {
        self.chan.send(req, fds)?;
        let event = self.recv_reply()?;
        if let Event::Error { message } = &event {
            bail!("{message}");
        }
        if let Event::DeviceAdded {
            caps: Some(caps), ..
        } = &event
        {
            self.caps = Some(caps.clone());
        }
        Ok(event)
    }

    pub fn expect_ack(event: Event) -> anyhow::Result<()> {
        match event {
            Event::Ack => Ok(()),
            other => Err(anyhow!("expected Ack, got {other:?}")),
        }
    }

    /// Sends a request that gets no reply (see `Request::is_oneway`).
    pub fn send_oneway(&mut self, req: &Request, fds: &[BorrowedFd<'_>]) -> anyhow::Result<()> {
        debug_assert!(req.is_oneway());
        self.chan.send(req, fds)?;
        Ok(())
    }

    /// Queues commands for execution. Failures come back as `GpuEvent::Error`.
    pub fn execute(
        &mut self,
        commands: Vec<protocol::Command>,
        fds: &[OwnedFd],
    ) -> anyhow::Result<()> {
        let fds: Vec<BorrowedFd<'_>> = fds.iter().map(|fd| fd.as_fd()).collect();
        self.send_oneway(&Request::Execute { commands }, &fds)
    }

    pub fn import_dmabuf(
        &mut self,
        id: TexId,
        desc: DmabufDesc,
        fds: &[OwnedFd],
    ) -> anyhow::Result<()> {
        let fds: Vec<BorrowedFd<'_>> = fds.iter().map(|fd| fd.as_fd()).collect();
        Self::expect_ack(self.request(&Request::ImportDmabuf { id, desc }, &fds)?)
    }

    /// Returns once everything sent before has executed and finished on the GPU.
    pub fn sync(&mut self) -> anyhow::Result<()> {
        Self::expect_ack(self.request(&Request::Sync, &[])?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cast_start(
        &mut self,
        stream: u64,
        size: (i32, i32),
        refresh: u32,
        alpha: bool,
        cursor_mode: CastCursorMode,
        allow_dmabuf: bool,
        force_invalid_modifier: bool,
    ) -> anyhow::Result<CastCursorMode> {
        let req = Request::CastStart {
            stream,
            width: size.0,
            height: size.1,
            refresh,
            alpha,
            cursor_mode,
            allow_dmabuf,
            force_invalid_modifier,
        };
        match self.request(&req, &[])? {
            Event::CastStarted { cursor_mode } => Ok(cursor_mode),
            other => Err(anyhow!("expected CastStarted, got {other:?}")),
        }
    }

    pub fn cast_configure(
        &mut self,
        stream: u64,
        size: (i32, i32),
        refresh: u32,
    ) -> anyhow::Result<()> {
        let req = Request::CastConfigure {
            stream,
            width: size.0,
            height: size.1,
            refresh,
        };
        Self::expect_ack(self.request(&req, &[])?)
    }

    pub fn load_cursor(
        &mut self,
        theme: &str,
        names: &[String],
        size: i32,
        fallback: bool,
        first_id: TexId,
    ) -> anyhow::Result<Vec<CursorFrameDesc>> {
        let req = Request::LoadCursor {
            theme: theme.to_owned(),
            names: names.to_vec(),
            size,
            fallback,
            first_id,
        };
        match self.request(&req, &[])? {
            Event::Cursor { frames } => Ok(frames),
            other => Err(anyhow!("expected Cursor, got {other:?}")),
        }
    }

    /// Allocates a render buffer on the GPU side and returns it as a dmabuf.
    pub fn allocate_dmabuf(
        &mut self,
        width: u32,
        height: u32,
        format: u32,
        modifiers: Vec<u64>,
    ) -> anyhow::Result<Dmabuf> {
        self.chan.send(
            &Request::AllocateDmabuf {
                width,
                height,
                format,
                modifiers,
            },
            &[],
        )?;
        let (event, fds) = self.recv_reply_with_fds()?;
        match event {
            Event::Dmabuf(desc) => {
                let mut fds: VecDeque<OwnedFd> = fds.into();
                super::exec::build_dmabuf(&desc, &mut fds)
            }
            Event::Error { message } => bail!("{message}"),
            other => Err(anyhow!("expected Dmabuf, got {other:?}")),
        }
    }

    pub fn read_texture(
        &mut self,
        id: TexId,
        region: Rect<i32>,
        format: u32,
    ) -> anyhow::Result<Image> {
        match self.request(&Request::ReadTexture { id, region, format }, &[])? {
            Event::Image(image) => Ok(image),
            other => Err(anyhow!("expected Image, got {other:?}")),
        }
    }

    /// Returns whether the shader is now available.
    pub fn set_custom_shader(
        &mut self,
        kind: ShaderKind,
        src: Option<&str>,
    ) -> anyhow::Result<bool> {
        let src = src.map(str::to_owned);
        match self.request(&Request::SetCustomShader { kind, src }, &[])? {
            Event::ShaderSet { available } => Ok(available),
            other => Err(anyhow!("expected ShaderSet, got {other:?}")),
        }
    }

    pub fn shutdown(mut self) -> anyhow::Result<()> {
        let _ = self.request(&Request::Shutdown, &[]);
        self.join()
    }

    fn join(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            let status = child.wait().context("waiting for gpu process")?;
            if !status.success() {
                bail!("gpu process exited with {status}");
            }
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("gpu server thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for GpuClient {
    fn drop(&mut self) {
        let _ = self.chan.send(&Request::Shutdown, &[]);
        let _ = self.join();
    }
}

/// Wraps the socket fd inherited from the parent (see [`CHILD_SOCKET_FD`]).
pub fn inherited_socket(fd: i32) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(fd) }
}
